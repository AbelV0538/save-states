//! Executing a [`RestorePlan`]: close (opt-in), launch, then watch for the new windows
//! and put each one where it was.
//!
//! New windows are found by diffing against the windows that existed before we
//! launched anything, never by PID: launchers hand off to other processes, and
//! single-instance apps (VS Code, Chrome) open the new window in the old process.
//! A window that shows up untitled is simply not a candidate until it has a title.
//! VS Code windows are told apart by the folder or workspace name in their titles.

use std::collections::HashSet;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use tracing::{info, warn};

use super::plan::{LaunchAction, LaunchUnit, RestorePlan};
use crate::capture::CaptureContext;
use crate::chrome::profiles::find_chrome_exe;
use crate::chrome::{ChromeEnv, InstanceMap};
use crate::desktops;
use crate::model::{CaptureScope, ChromeTab, Geometry, TabGroup};
use crate::vscode;
use crate::win::enumerate::{self, LiveWindow};
use crate::win::launch::{self, LaunchError, quote_arg};
use crate::win::process;

const TICK: Duration = Duration::from_millis(150);
const CHROME_WINDOW_TIMEOUT: Duration = Duration::from_secs(12);
/// Long enough for VS Code to start cold and show the folder name in its title.
const VSCODE_WINDOW_TIMEOUT: Duration = Duration::from_secs(20);
const APP_WINDOW_TIMEOUT: Duration = Duration::from_secs(15);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an app whose last window we closed gets to finish quitting.
const QUIT_TIMEOUT: Duration = Duration::from_secs(5);
/// Stay well under the 32,767-character CreateProcess limit.
const MAX_CMDLINE: usize = 30_000;

#[derive(Debug, Clone, Default)]
pub struct RestoreReport {
    pub opened: Vec<String>,
    /// Launched fine, but no window appeared in time (tray apps, slow starters).
    pub no_window: Vec<String>,
    pub already_open: usize,
    pub failed: Vec<(String, String)>,
    pub closed: usize,
    pub declined_close: Vec<String>,
    pub notes: Vec<String>,
}

enum Expect {
    Exe(String),
    Aumid(String),
    Explorer,
}

struct Pending<'a> {
    unit: &'a LaunchUnit,
    expect: Expect,
    remaining: Vec<(String, Geometry)>,
    got_window: bool,
}

/// Run `plan`. Call from the thread that owns COM and the virtual desktop objects.
pub fn execute(
    plan: &RestorePlan,
    close_extras: bool,
    ctx: &CaptureContext<'_>,
    on_progress: &mut dyn FnMut(usize, usize, &str),
) -> RestoreReport {
    let mut report = RestoreReport { already_open: plan.already_open.len(), ..Default::default() };
    let total = plan.launches.len() + if close_extras { plan.extras.len() } else { 0 };
    let mut done = 0;

    if close_extras && !plan.extras.is_empty() {
        on_progress(done, total, "Closing windows not in this state");
        close_windows(plan, &mut report);
        done += plan.extras.len();
    }

    let placer = Placer::new(plan);
    let baseline: HashSet<isize> = enumerate::raw_top_level_windows().iter().map(|h| h.0 as isize).collect();
    let mut assigned: HashSet<isize> = HashSet::new();
    let mut placed: Vec<(isize, Geometry)> = Vec::new();

    // Everything except Chrome launches at once; their windows are collected as they appear.
    let mut pending: Vec<Pending<'_>> = Vec::new();
    for unit in &plan.launches {
        let expect = match &unit.action {
            LaunchAction::Chrome { .. } | LaunchAction::VsCode { .. } => continue,
            LaunchAction::Process { exe, args, cwd } => {
                launch::spawn(exe, args, cwd.as_deref()).map(|_| Expect::Exe(file_name(exe)))
            }
            LaunchAction::Packaged { aumid } => launch::activate_app(aumid).map(|_| Expect::Aumid(aumid.clone())),
            LaunchAction::Explorer { folder } => open_folder(folder.as_deref()).map(|_| Expect::Explorer),
        };
        done += 1;
        on_progress(done, total, &unit.label);
        match expect {
            // Nothing to place (e.g. an Explorer tab beyond the first): fire and forget.
            Ok(_) if unit.windows.is_empty() => report.opened.push(unit.label.clone()),
            Ok(expect) => pending.push(Pending { unit, expect, remaining: unit.windows.clone(), got_window: false }),
            Err(e) => report.failed.push((unit.label.clone(), e.to_string())),
        }
    }
    let apps_deadline = Instant::now() + APP_WINDOW_TIMEOUT;

    // Chrome: one window at a time, so each new browser window is unambiguous.
    let env = ChromeEnv::load();
    let instances = InstanceMap::load(&ctx.paths.chrome_instances_file());
    for unit in &plan.launches {
        let LaunchAction::Chrome { exe, profile_dir, app_id, tabs, groups } = &unit.action else { continue };
        done += 1;
        on_progress(done, total, &unit.label);
        let Some(chrome_exe) = exe.clone().filter(|p| p.exists()).or_else(find_chrome_exe) else {
            report.failed.push((unit.label.clone(), "Chrome is not installed".into()));
            continue;
        };
        let tabs: Vec<&ChromeTab> = tabs.iter().filter(|t| !t.url.trim().is_empty()).collect();
        let urls: Vec<&str> = tabs.iter().map(|t| t.url.as_str()).collect();
        let batches = chrome_batches(profile_dir, app_id.as_deref(), &urls);
        if let Err(e) = launch::spawn(&chrome_exe, &batches[0], None) {
            report.failed.push((unit.label.clone(), e.to_string()));
            continue;
        }

        let deadline = Instant::now() + CHROME_WINDOW_TIMEOUT;
        let mut found = None;
        while found.is_none() && Instant::now() < deadline {
            thread::sleep(TICK);
            for w in new_windows(&baseline, &assigned) {
                let is_ours = w.exe_is("chrome.exe")
                    && w.aumid
                        .as_deref()
                        .and_then(|a| env.parse_window_aumid(a))
                        .is_some_and(|p| p.profile_dir == *profile_dir && p.app_id == *app_id);
                if is_ours && found.is_none() {
                    found = Some(w.hwnd);
                    assigned.insert(w.hwnd);
                } else {
                    service(&mut pending, &w, &mut assigned, &placer, &mut placed);
                }
            }
        }
        let Some(hwnd) = found else {
            report.no_window.push(unit.label.clone());
            continue;
        };
        if let Some((_, geom)) = unit.windows.first() {
            placer.place(hwnd, geom);
            placed.push((hwnd, geom.clone()));
        }
        // Any URLs that didn't fit on one command line join the window we just made,
        // which is now that profile's most recently active window.
        for extra in &batches[1..] {
            let _ = launch::spawn(&chrome_exe, extra, None);
        }
        report.opened.push(unit.label.clone());
        if let Some(note) = refine_chrome_window(&tabs, groups, profile_dir, &instances, ctx) {
            report.notes.push(note);
        }
    }

    // VS Code: also one window at a time, which lets the first launch finish starting the
    // editor so later ones are handed to it instead of racing to become its main instance.
    // Empty windows and untitled workspaces have generic titles and can only be told apart
    // by when they appear, so they go last.
    let mut code_units: Vec<&LaunchUnit> =
        plan.launches.iter().filter(|u| matches!(u.action, LaunchAction::VsCode { .. })).collect();
    code_units.sort_by_key(|u| vscode_label(u).is_empty());
    if close_extras && report.closed > 0 && !code_units.is_empty() {
        let exe_names: Vec<String> = code_units
            .iter()
            .filter_map(|u| match &u.action {
                LaunchAction::VsCode { exe, .. } => Some(file_name(exe)),
                _ => None,
            })
            .collect();
        wait_for_quitting_apps(plan, &exe_names);
    }
    // What the labeled units' windows will show, to tell them from windows VS Code opens on its own.
    let unit_labels: Vec<(String, bool)> = code_units
        .iter()
        .filter_map(|u| match &u.action {
            LaunchAction::VsCode { folder, workspace, .. } => {
                Some((vscode_label(u), folder.is_none() && workspace.is_some())).filter(|(l, _)| !l.is_empty())
            }
            _ => None,
        })
        .collect();
    let shows_a_unit =
        |w: &LiveWindow| unit_labels.iter().any(|(l, ws)| vscode::title_position(&w.title, l, *ws).is_some());
    let mut code_exes: Vec<String> = Vec::new();
    for unit in code_units {
        let LaunchAction::VsCode { exe, user_data_dir, folder, workspace } = &unit.action else { continue };
        done += 1;
        on_progress(done, total, &unit.label);
        let exe_name = file_name(exe);
        if !code_exes.contains(&exe_name) {
            code_exes.push(exe_name.clone());
        }
        let label = vscode_label(unit);
        let is_workspace = folder.is_none() && workspace.is_some();
        let shows_target =
            |w: &LiveWindow| w.exe_is(&exe_name) && vscode::title_position(&w.title, &label, is_workspace).is_some();
        // Empty windows and untitled workspaces can only be told apart from the folder windows.
        let is_ours = |w: &LiveWindow| {
            if label.is_empty() { w.exe_is(&exe_name) && !shows_a_unit(w) } else { shows_target(w) }
        };

        // The window may be open already. Starting the editor for an earlier unit makes VS Code
        // reopen its untitled workspaces and windows with unsaved files, and with
        // `window.restoreWindows: preserve`, the rest of its last session.
        let mut found = new_windows(&baseline, &assigned).into_iter().find(|w| is_ours(w)).map(|w| w.hwnd);
        if found.is_none() {
            let target = folder.as_deref().or(workspace.as_deref());
            if target.and_then(vscode::local_path).is_some_and(|p| !p.exists()) {
                report.failed.push((unit.label.clone(), LaunchError::NotFound.to_string()));
                continue;
            }
            let before: HashSet<isize> = enumerate::raw_top_level_windows().iter().map(|h| h.0 as isize).collect();
            let args = vscode::launch_args(user_data_dir.as_deref(), folder.as_deref(), workspace.as_deref());
            if let Err(e) = launch::spawn(exe, &args, None) {
                report.failed.push((unit.label.clone(), e.to_string()));
                continue;
            }
            let deadline = Instant::now() + VSCODE_WINDOW_TIMEOUT;
            while found.is_none() && Instant::now() < deadline {
                thread::sleep(TICK);
                for w in new_windows(&baseline, &assigned) {
                    let ours = is_ours(&w) && (!label.is_empty() || !before.contains(&w.hwnd));
                    if ours && found.is_none() {
                        found = Some(w.hwnd);
                    } else {
                        service(&mut pending, &w, &mut assigned, &placer, &mut placed);
                    }
                }
            }
        }
        let Some(hwnd) = found else {
            report.no_window.push(unit.label.clone());
            continue;
        };
        assigned.insert(hwnd);
        if let Some((_, geom)) = unit.windows.first() {
            placer.place(hwnd, geom);
            placed.push((hwnd, geom.clone()));
        }
        report.opened.push(unit.label.clone());
    }
    let reopened = new_windows(&baseline, &assigned).iter().filter(|w| code_exes.iter().any(|n| w.exe_is(n))).count();
    if reopened > 0 {
        report.notes.push(format!(
            "VS Code also reopened {reopened} window(s) of its own (untitled workspaces, unsaved files, or its last session)."
        ));
    }

    while pending.iter().any(|p| !p.remaining.is_empty()) && Instant::now() < apps_deadline {
        thread::sleep(TICK);
        for w in new_windows(&baseline, &assigned) {
            service(&mut pending, &w, &mut assigned, &placer, &mut placed);
        }
    }
    for p in pending {
        if p.got_window {
            report.opened.push(p.unit.label.clone());
        } else {
            report.no_window.push(p.unit.label.clone());
        }
    }

    // Some apps restore their own last position right after showing; win that race.
    if !placed.is_empty() {
        thread::sleep(Duration::from_millis(700));
        for (hwnd, geom) in &placed {
            if enumerate::is_alive(*hwnd) {
                launch::apply_geometry(*hwnd, geom);
            }
        }
    }
    info!(
        opened = report.opened.len(),
        failed = report.failed.len(),
        no_window = report.no_window.len(),
        "restore finished"
    );
    report
}

fn close_windows(plan: &RestorePlan, report: &mut RestoreReport) {
    for e in &plan.extras {
        launch::request_close(e.hwnd);
    }
    let deadline = Instant::now() + CLOSE_TIMEOUT;
    let mut remaining: Vec<_> = plan.extras.iter().collect();
    while !remaining.is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(200));
        remaining.retain(|e| enumerate::is_alive(e.hwnd));
    }
    report.closed = plan.extras.len() - remaining.len();
    // Still open = the app asked about unsaved work, or ignored us. Never forced.
    report.declined_close = remaining.iter().map(|e| e.label.clone()).collect();
}

/// Closing an app's last window can make it quit, and while it does it may accept a new
/// launch and drop it (VS Code does). Give processes of `exe_names` whose windows we just
/// closed, and that have none left, time to exit.
fn wait_for_quitting_apps(plan: &RestorePlan, exe_names: &[String]) {
    let with_windows: HashSet<u32> = enumerate::app_windows(std::process::id()).iter().map(|w| w.pid).collect();
    let quitting: HashSet<u32> = plan
        .extras
        .iter()
        .map(|e| e.pid)
        .filter(|pid| !with_windows.contains(pid))
        .filter(|&pid| {
            process::exe_path(pid).is_some_and(|p| exe_names.iter().any(|n| file_name(&p).eq_ignore_ascii_case(n)))
        })
        .collect();
    let deadline = Instant::now() + QUIT_TIMEOUT;
    for pid in quitting {
        if !process::wait_for_exit(pid, deadline.saturating_duration_since(Instant::now())) {
            warn!(pid, "app still running after its windows were closed");
        }
    }
}

fn vscode_label(unit: &LaunchUnit) -> String {
    match &unit.action {
        LaunchAction::VsCode { folder, workspace, .. } => vscode::root_label(folder.as_deref(), workspace.as_deref()),
        _ => String::new(),
    }
}

/// App windows that appeared since the baseline and haven't been claimed yet.
fn new_windows(baseline: &HashSet<isize>, assigned: &HashSet<isize>) -> Vec<LiveWindow> {
    enumerate::app_windows(std::process::id())
        .into_iter()
        .filter(|w| !baseline.contains(&w.hwnd) && !assigned.contains(&w.hwnd))
        .collect()
}

/// Offer a new window to the pending launches; the first that expects it claims it.
fn service(
    pending: &mut [Pending<'_>],
    w: &LiveWindow,
    assigned: &mut HashSet<isize>,
    placer: &Placer<'_>,
    placed: &mut Vec<(isize, Geometry)>,
) {
    let Some(p) = pending.iter_mut().find(|p| !p.remaining.is_empty() && expects(&p.expect, w)) else {
        return;
    };
    // With several saved windows for one launch, give this window the geometry whose
    // saved title is closest to its current one.
    let best = (0..p.remaining.len()).max_by_key(|&i| title_similarity(&p.remaining[i].0, &w.title)).unwrap_or(0);
    let (_, geom) = p.remaining.remove(best);
    placer.place(w.hwnd, &geom);
    placed.push((w.hwnd, geom));
    assigned.insert(w.hwnd);
    p.got_window = true;
}

fn expects(expect: &Expect, w: &LiveWindow) -> bool {
    match expect {
        Expect::Exe(name) => w.exe_name().eq_ignore_ascii_case(name),
        Expect::Explorer => w.class == "CabinetWClass",
        Expect::Aumid(aumid) => {
            w.aumid.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(aumid))
                || w.exe.as_deref().is_some_and(process::is_packaged_path)
                    && process::package_aumid(w.pid).is_some_and(|a| a.eq_ignore_ascii_case(aumid))
        }
    }
}

/// Shared words between two titles; enough to tell "a.rs - proj - VS Code" from "b.rs - proj - VS Code".
fn title_similarity(a: &str, b: &str) -> usize {
    let words = |s: &str| {
        s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect::<HashSet<_>>()
    };
    words(a).intersection(&words(b)).count()
}

struct Placer<'a> {
    plan: &'a RestorePlan,
    move_desktops: bool,
}

impl<'a> Placer<'a> {
    fn new(plan: &'a RestorePlan) -> Self {
        let move_desktops = plan.scope == CaptureScope::AllDesktops && desktops::available();
        if move_desktops {
            let needed = plan
                .launches
                .iter()
                .flat_map(|u| u.windows.iter())
                .filter_map(|(_, g)| g.desktop)
                .max()
                .map_or(0, |m| m as u32 + 1);
            desktops::ensure_count(needed);
        }
        Self { plan, move_desktops }
    }

    fn place(&self, hwnd: isize, geom: &Geometry) {
        if self.move_desktops
            && let Some(target) = geom.desktop.and_then(|i| self.plan.desktops.iter().find(|d| d.index as usize == i))
            && !desktops::move_window(hwnd, target)
        {
            warn!(hwnd, "could not move window to desktop {}", target.index);
        }
        launch::apply_geometry(hwnd, geom);
    }
}

fn open_folder(folder: Option<&str>) -> Result<(), LaunchError> {
    match folder {
        // Shell locations (This PC, Recycle Bin, ...) are `::{GUID}` parsing names.
        Some(f) if !f.starts_with("::") && !Path::new(f).exists() => Err(LaunchError::NotFound),
        Some(f) => launch::shell_open("explorer.exe", Some(&quote_arg(f))),
        None => launch::shell_open("explorer.exe", None),
    }
}

/// Command lines for one Chrome window. The first creates the window; any further ones
/// (only when the URLs don't fit on one command line) add tabs to it.
pub(crate) fn chrome_batches(profile_dir: &str, app_id: Option<&str>, urls: &[&str]) -> Vec<Vec<String>> {
    let profile = format!("--profile-directory={profile_dir}");
    if let Some(id) = app_id {
        return vec![vec![profile, format!("--app-id={id}")]];
    }
    let mut batches = Vec::new();
    // `--` ends switch parsing, so no URL can ever be read as a Chrome flag.
    let mut current = vec![profile.clone(), "--new-window".to_string(), "--".to_string()];
    let mut len: usize = current.iter().map(|a| a.len() + 1).sum::<usize>() + 64;
    for url in urls {
        let cost = quote_arg(url).len() + 1;
        if len + cost > MAX_CMDLINE && current.len() > 3 {
            batches.push(std::mem::take(&mut current));
            current = vec![profile.clone(), "--".to_string()];
            len = current.iter().map(|a| a.len() + 1).sum::<usize>() + 64;
        }
        current.push(url.to_string());
        len += cost;
    }
    batches.push(current);
    batches
}

/// Pinned tabs, tab groups and the active tab can't be expressed on Chrome's command
/// line; the profile's extension applies them to the window it sees appear.
fn refine_chrome_window(
    tabs: &[&ChromeTab],
    groups: &[TabGroup],
    profile_dir: &str,
    instances: &InstanceMap,
    ctx: &CaptureContext<'_>,
) -> Option<String> {
    let pinned: Vec<usize> = tabs.iter().enumerate().filter(|(_, t)| t.pinned).map(|(i, _)| i).collect();
    let active = tabs.iter().position(|t| t.active).unwrap_or(0);
    let group_specs: Vec<_> = groups
        .iter()
        .filter_map(|g| {
            let indices: Vec<usize> =
                tabs.iter().enumerate().filter(|(_, t)| t.group == Some(g.id)).map(|(i, _)| i).collect();
            (!indices.is_empty())
                .then(|| json!({"title": g.title, "color": g.color, "collapsed": g.collapsed, "indices": indices}))
        })
        .collect();
    if pinned.is_empty() && group_specs.is_empty() && active == 0 {
        return None;
    }
    let spec = json!({
        "urls": tabs.iter().map(|t| t.url.as_str()).collect::<Vec<_>>(),
        "pinned": pinned,
        "active": active,
        "groups": group_specs,
    });
    let hub = ctx.hub?;
    let request = json!({"op": "apply", "spec": spec});
    let applied = instances
        .instance_for(profile_dir)
        .into_iter()
        .any(|inst| hub.request(inst, request.clone(), Duration::from_secs(10)).is_some_and(|r| r["ok"] == true));
    (!applied).then(|| "Pinned tabs, tab groups or the active tab were not restored for a Chrome window (its extension did not respond).".to_string())
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_batches_end_switch_parsing_before_urls() {
        let b = chrome_batches("Profile 2", None, &["https://a.com", "--evil-flag"]);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0], vec!["--profile-directory=Profile 2", "--new-window", "--", "https://a.com", "--evil-flag"]);
    }

    #[test]
    fn chrome_batches_split_long_command_lines() {
        let long = format!("https://example.com/{}", "x".repeat(1000));
        let urls: Vec<&str> = std::iter::repeat_n(long.as_str(), 100).collect();
        let b = chrome_batches("Default", None, &urls);
        assert!(b.len() >= 4, "100 KB of URLs needs several command lines");
        assert!(b[0].contains(&"--new-window".to_string()));
        assert!(b[1..].iter().all(|batch| !batch.contains(&"--new-window".to_string())));
        assert_eq!(
            b.iter().map(|batch| batch.iter().filter(|a| a.starts_with("https://")).count()).sum::<usize>(),
            100
        );
        for batch in &b {
            let len: usize = batch.iter().map(|a| quote_arg(a).len() + 1).sum();
            assert!(len < MAX_CMDLINE);
        }
    }

    #[test]
    fn web_apps_launch_by_app_id() {
        assert_eq!(
            chrome_batches("Default", Some("abc"), &[]),
            vec![vec!["--profile-directory=Default", "--app-id=abc"]]
        );
    }

    #[test]
    fn title_similarity_prefers_the_right_window() {
        let a = "main.rs - saved_states - Visual Studio Code";
        let b = "notes.md - portfolio - Visual Studio Code";
        assert!(title_similarity(a, "main.rs - saved_states - Visual Studio Code") > title_similarity(b, a));
    }
}
