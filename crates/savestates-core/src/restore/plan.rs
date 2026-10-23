//! Restore planning: diff a saved state against what is open right now.
//!
//! Pure - no Win32 calls - so the matching rules are unit-tested directly.
//!
//! Matching rules:
//! - ordinary apps launch once per distinct (exe, document args). Multi-window apps
//!   reopen their own windows, so three captured windows of one exe must not become
//!   three launches; a still-running exe counts as already open;
//! - VS Code windows match one to one by folder or workspace, so each reopens on its own;
//! - packaged/Store apps match by AppUserModelID;
//! - Explorer matches per folder;
//! - Chrome windows match within the same profile by tab-URL overlap (>= 60%).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::capture::{LiveItem, LiveSession};
use crate::chrome::normalize_url;
use crate::model::{CaptureScope, CapturedItem, ChromeTab, DesktopRef, Geometry, SaveState, TabGroup};
use crate::vscode;

#[derive(Debug, Clone)]
pub struct RestorePlan {
    pub state_id: Uuid,
    pub state_name: String,
    pub scope: CaptureScope,
    pub desktops: Vec<DesktopRef>,
    pub launches: Vec<LaunchUnit>,
    /// Labels of saved items that are already open and will be left alone.
    pub already_open: Vec<String>,
    /// Open windows that are not part of the state: the opt-in "close these too" list.
    pub extras: Vec<ExtraWindow>,
}

#[derive(Debug, Clone)]
pub struct LaunchUnit {
    pub label: String,
    pub action: LaunchAction,
    /// Where each window this launch should produce goes, paired with its saved title.
    pub windows: Vec<(String, Geometry)>,
}

#[derive(Debug, Clone)]
pub enum LaunchAction {
    Process {
        exe: PathBuf,
        args: Vec<String>,
        cwd: Option<PathBuf>,
    },
    Packaged {
        aumid: String,
    },
    Explorer {
        folder: Option<String>,
    },
    Chrome {
        exe: Option<PathBuf>,
        profile_dir: String,
        app_id: Option<String>,
        tabs: Vec<ChromeTab>,
        groups: Vec<TabGroup>,
    },
    VsCode {
        exe: PathBuf,
        user_data_dir: Option<PathBuf>,
        folder: Option<String>,
        workspace: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct ExtraWindow {
    pub hwnd: isize,
    pub pid: u32,
    pub label: String,
}

pub fn plan(state: &SaveState, live: &LiveSession) -> RestorePlan {
    let mut used = vec![false; live.items.len()];
    let mut launches = Vec::new();
    let mut already_open = Vec::new();

    plan_vscode(state, live, &mut used, &mut launches, &mut already_open);
    plan_processes(state, live, &mut used, &mut launches, &mut already_open);

    for item in state.known_items() {
        match item {
            CapturedItem::Uwp(u) => {
                let open = mark_matching(live, &mut used, |li| match &li.item {
                    CapturedItem::Uwp(l) => l.aumid.eq_ignore_ascii_case(&u.aumid),
                    _ => false,
                });
                if open {
                    already_open.push(item.label());
                } else {
                    launches.push(LaunchUnit {
                        label: item.label(),
                        action: LaunchAction::Packaged { aumid: u.aumid.clone() },
                        windows: vec![(u.title.clone(), u.geometry.clone())],
                    });
                }
            }
            CapturedItem::Explorer(e) => {
                let folders: Vec<Option<String>> =
                    if e.folders.is_empty() { vec![None] } else { e.folders.iter().cloned().map(Some).collect() };
                for (i, folder) in folders.into_iter().enumerate() {
                    // One live window can hold several saved folders as tabs, so look at
                    // used windows too.
                    let mut open = false;
                    for (li, u) in live.items.iter().zip(used.iter_mut()) {
                        let hit = match (&li.item, &folder) {
                            (CapturedItem::Explorer(l), Some(f)) => {
                                l.folders.iter().any(|lf| lf.eq_ignore_ascii_case(f))
                            }
                            (CapturedItem::Explorer(_), None) => true,
                            _ => false,
                        };
                        if hit {
                            *u = true;
                            open = true;
                        }
                    }
                    let label = format!("Explorer - {}", folder.as_deref().unwrap_or("(default)"));
                    if open {
                        already_open.push(label);
                    } else {
                        // Only the window's first folder carries its geometry; extra tabs
                        // come back as plain windows (there is no API to open a tab).
                        let windows = if i == 0 { vec![(e.title.clone(), e.geometry.clone())] } else { vec![] };
                        launches.push(LaunchUnit { label, action: LaunchAction::Explorer { folder }, windows });
                    }
                }
            }
            CapturedItem::Chrome(c) => {
                if plan_chrome_is_open(c, live, &mut used) {
                    already_open.push(item.label());
                } else {
                    launches.push(LaunchUnit {
                        label: item.label(),
                        action: LaunchAction::Chrome {
                            exe: c.exe.clone(),
                            profile_dir: c.profile_dir.clone(),
                            app_id: c.app_id.clone(),
                            tabs: c.tabs.clone(),
                            groups: c.groups.clone(),
                        },
                        windows: vec![(c.title.clone(), c.geometry.clone())],
                    });
                }
            }
            CapturedItem::VsCode(_) => {}  // handled by plan_vscode
            CapturedItem::Process(_) => {} // handled by plan_processes
        }
    }

    let extras = live
        .items
        .iter()
        .zip(&used)
        .filter(|(li, used)| !**used && (state.scope == CaptureScope::AllDesktops || li.window.on_current_desktop))
        .map(|(li, _)| ExtraWindow { hwnd: li.window.hwnd, pid: li.window.pid, label: li.item.label() })
        .collect();

    RestorePlan {
        state_id: state.id,
        state_name: state.name.clone(),
        scope: state.scope,
        desktops: state.desktops.clone(),
        launches,
        already_open,
        extras,
    }
}

/// Each saved VS Code window is one folder or workspace (or an empty window), open if one
/// live window of the same editor shows it.
fn plan_vscode(
    state: &SaveState,
    live: &LiveSession,
    used: &mut [bool],
    launches: &mut Vec<LaunchUnit>,
    already_open: &mut Vec<String>,
) {
    for item in state.known_items() {
        let CapturedItem::VsCode(v) = item else { continue };
        let label = vscode::root_label(v.folder.as_deref(), v.workspace.as_deref());
        let is_workspace = v.folder.is_none() && v.workspace.is_some();
        let open = mark_first(live, used, |li| match &li.item {
            CapturedItem::VsCode(l) => {
                same_exe_name(&l.exe, &v.exe) && l.user_data_dir == v.user_data_dir && vscode::same_target(l, v)
            }
            // The live editor couldn't be read and fell back to a plain process: go by its titles.
            CapturedItem::Process(l) => {
                same_exe_name(&l.exe, &v.exe) && vscode::title_position(&l.title, &label, is_workspace).is_some()
            }
            _ => false,
        });
        if open {
            already_open.push(item.label());
        } else {
            launches.push(LaunchUnit {
                label: item.label(),
                action: LaunchAction::VsCode {
                    exe: v.exe.clone(),
                    user_data_dir: v.user_data_dir.clone(),
                    folder: v.folder.clone(),
                    workspace: v.workspace.clone(),
                },
                windows: vec![(v.title.clone(), v.geometry.clone())],
            });
        }
    }
}

fn plan_processes(
    state: &SaveState,
    live: &LiveSession,
    used: &mut [bool],
    launches: &mut Vec<LaunchUnit>,
    already_open: &mut Vec<String>,
) {
    // An editor with saved VS Code windows reopens through those; launching it bare as well
    // would only add an empty window.
    let vscode_exes: Vec<&Path> = state
        .known_items()
        .filter_map(|i| match i {
            CapturedItem::VsCode(v) => Some(v.exe.as_path()),
            _ => None,
        })
        .collect();
    // Group saved windows by launch identity, keeping first-seen order.
    let mut groups: Vec<(String, Vec<String>, Vec<&crate::model::ProcessWindow>)> = Vec::new();
    for item in state.known_items() {
        if let CapturedItem::Process(p) = item
            && !vscode_exes.iter().any(|e| same_exe_name(e, &p.exe))
        {
            let key = p.exe.to_string_lossy().to_ascii_lowercase();
            match groups.iter_mut().find(|(k, a, _)| *k == key && *a == p.args) {
                Some((_, _, members)) => members.push(p),
                None => groups.push((key, p.args.clone(), vec![p])),
            }
        }
    }
    for (_, args, members) in groups {
        let first = members[0];
        let doc_stem = args.first().and_then(|a| Path::new(a).file_stem()).map(|s| s.to_string_lossy().to_lowercase());
        let open = mark_matching(live, used, |li| {
            let (exe, title) = match &li.item {
                CapturedItem::Process(l) => (&l.exe, &l.title),
                // States saved before VS Code windows had their own kind hold the editor as a process.
                CapturedItem::VsCode(l) => (&l.exe, &l.title),
                _ => return false,
            };
            same_exe(exe, &first.exe)
                && doc_stem.as_ref().is_none_or(|stem| title.to_lowercase().contains(stem.as_str()))
        });
        let label = CapturedItem::Process(first.clone()).label();
        if open {
            already_open.push(label);
        } else {
            launches.push(LaunchUnit {
                label,
                action: LaunchAction::Process { exe: first.exe.clone(), args, cwd: first.cwd.clone() },
                windows: members.iter().map(|m| (m.title.clone(), m.geometry.clone())).collect(),
            });
        }
    }
}

/// A saved Chrome window is open if a live window of the same profile shows mostly the
/// same pages. Without captured tabs, any live window of that profile counts.
fn plan_chrome_is_open(c: &crate::model::ChromeWindow, live: &LiveSession, used: &mut [bool]) -> bool {
    let wanted: HashSet<&str> = c.tabs.iter().map(|t| normalize_url(&t.url)).collect();
    let mut best: Option<(usize, f64)> = None;
    for (i, li) in live.items.iter().enumerate() {
        let CapturedItem::Chrome(l) = &li.item else { continue };
        if used[i] || l.profile_dir != c.profile_dir || l.app_id != c.app_id {
            continue;
        }
        let score = if c.app_id.is_some() || !c.tabs_captured || wanted.is_empty() {
            1.0
        } else {
            let have: HashSet<&str> = l.tabs.iter().map(|t| normalize_url(&t.url)).collect();
            wanted.intersection(&have).count() as f64 / wanted.len() as f64
        };
        if score >= 0.6 && best.is_none_or(|(_, s)| score > s) {
            best = Some((i, score));
        }
    }
    match best {
        Some((i, _)) => {
            used[i] = true;
            true
        }
        None => false,
    }
}

/// Mark every unused live item matching `pred`; true if there was at least one.
fn mark_matching(live: &LiveSession, used: &mut [bool], pred: impl Fn(&LiveItem) -> bool) -> bool {
    let mut any = false;
    for (i, li) in live.items.iter().enumerate() {
        if !used[i] && pred(li) {
            used[i] = true;
            any = true;
        }
    }
    any
}

/// Mark the first unused live item matching `pred`; true if there was one.
fn mark_first(live: &LiveSession, used: &mut [bool], pred: impl Fn(&LiveItem) -> bool) -> bool {
    let Some(i) = (0..live.items.len()).find(|&i| !used[i] && pred(&live.items[i])) else { return false };
    used[i] = true;
    true
}

fn same_exe(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

/// Same executable name, wherever it's installed (a per-user and a per-machine install of
/// one editor are the same editor).
fn same_exe_name(a: &Path, b: &Path) -> bool {
    match (a.file_name(), b.file_name()) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChromeWindow, ExplorerWindow, ProcessWindow, StoredItem, UwpWindow, VsCodeWindow};
    use crate::win::enumerate::LiveWindow;
    use chrono::Utc;

    fn state(items: Vec<CapturedItem>, scope: CaptureScope) -> SaveState {
        SaveState {
            id: Uuid::new_v4(),
            name: "s".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            favorite: false,
            thumbnail: Default::default(),
            scope,
            desktops: vec![],
            items: items.into_iter().map(StoredItem::Known).collect(),
            extra: Default::default(),
        }
    }

    fn live(items: Vec<(CapturedItem, bool)>) -> LiveSession {
        LiveSession {
            items: items
                .into_iter()
                .enumerate()
                .map(|(i, (item, on_current))| LiveItem {
                    window: LiveWindow {
                        hwnd: 1000 + i as isize,
                        pid: 1,
                        title: String::new(),
                        class: String::new(),
                        exe: None,
                        aumid: None,
                        cloaked: !on_current,
                        on_current_desktop: on_current,
                        show: Default::default(),
                        normal_rect: Default::default(),
                        rect: Default::default(),
                        frame: Default::default(),
                        dpi: 96,
                    },
                    item,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn proc_(exe: &str, title: &str, args: &[&str]) -> CapturedItem {
        CapturedItem::Process(ProcessWindow {
            exe: exe.into(),
            title: title.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        })
    }

    const CODE: &str = r"C:\Users\me\AppData\Local\Programs\Microsoft VS Code\Code.exe";

    fn code(folder: Option<&str>, title: &str) -> CapturedItem {
        CapturedItem::VsCode(VsCodeWindow {
            exe: CODE.into(),
            product: "Visual Studio Code".into(),
            folder: folder.map(str::to_string),
            title: title.into(),
            ..Default::default()
        })
    }

    fn chrome(profile: &str, urls: &[&str]) -> CapturedItem {
        CapturedItem::Chrome(ChromeWindow {
            profile_dir: profile.into(),
            tabs_captured: true,
            tabs: urls.iter().map(|u| ChromeTab { url: u.to_string(), ..Default::default() }).collect(),
            ..Default::default()
        })
    }

    #[test]
    fn multi_window_app_launches_once_and_running_exe_counts_as_open() {
        let code = r"C:\Apps\Code.exe";
        let s =
            state(vec![proc_(code, "a - VS Code", &[]), proc_(code, "b - VS Code", &[])], CaptureScope::AllDesktops);

        let p = plan(&s, &live(vec![]));
        assert_eq!(p.launches.len(), 1, "two windows of one exe are one launch");
        assert_eq!(p.launches[0].windows.len(), 2, "but both geometries are kept for placement");

        let p = plan(&s, &live(vec![(proc_(&code.to_uppercase(), "x - VS Code", &[]), true)]));
        assert!(p.launches.is_empty());
        assert_eq!(p.already_open.len(), 1);
        assert!(p.extras.is_empty(), "the running instance belongs to the state");
    }

    #[test]
    fn documents_distinguish_launches() {
        let word = r"C:\Office\WINWORD.EXE";
        let s = state(
            vec![proc_(word, "a - Word", &[r"C:\d\a.docx"]), proc_(word, "b - Word", &[r"C:\d\b.docx"])],
            CaptureScope::AllDesktops,
        );
        let p = plan(&s, &live(vec![(proc_(word, "a - Word", &[]), true)]));
        assert_eq!(p.launches.len(), 1);
        assert!(matches!(&p.launches[0].action, LaunchAction::Process { args, .. } if args[0].ends_with("b.docx")));
    }

    #[test]
    fn chrome_matches_by_profile_and_url_overlap() {
        let s = state(
            vec![chrome("Profile 2", &["https://a.com/", "https://b.com", "https://c.com"])],
            CaptureScope::AllDesktops,
        );
        // Same pages, other profile: not a match.
        let p = plan(&s, &live(vec![(chrome("Default", &["https://a.com", "https://b.com", "https://c.com"]), true)]));
        assert_eq!(p.launches.len(), 1);
        assert_eq!(p.extras.len(), 1);
        // Two of three pages (67%) in the right profile: a match (trailing slash ignored).
        let p =
            plan(&s, &live(vec![(chrome("Profile 2", &["https://a.com", "https://b.com/", "https://z.com"]), true)]));
        assert!(p.launches.is_empty());
        // One of three: not enough.
        let p =
            plan(&s, &live(vec![(chrome("Profile 2", &["https://a.com", "https://y.com", "https://z.com"]), true)]));
        assert_eq!(p.launches.len(), 1);
    }

    #[test]
    fn one_live_chrome_window_cannot_satisfy_two_saved_ones() {
        let s = state(
            vec![chrome("Default", &["https://a.com"]), chrome("Default", &["https://a.com"])],
            CaptureScope::AllDesktops,
        );
        let p = plan(&s, &live(vec![(chrome("Default", &["https://a.com"]), true)]));
        assert_eq!(p.already_open.len(), 1);
        assert_eq!(p.launches.len(), 1);
    }

    #[test]
    fn extras_respect_scope() {
        let notepad = proc_(r"C:\Windows\notepad.exe", "n", &[]);
        let paint = proc_(r"C:\Windows\mspaint.exe", "p", &[]);
        let l = live(vec![(notepad, true), (paint, false)]);
        let current_only = plan(&state(vec![], CaptureScope::CurrentDesktop), &l);
        assert_eq!(current_only.extras.len(), 1, "windows on other desktops are not offered for closing");
        let all = plan(&state(vec![], CaptureScope::AllDesktops), &l);
        assert_eq!(all.extras.len(), 2);
    }

    #[test]
    fn uwp_and_explorer_matching() {
        let s = state(
            vec![
                CapturedItem::Uwp(UwpWindow { aumid: "App.Id!App".into(), ..Default::default() }),
                CapturedItem::Explorer(ExplorerWindow {
                    folders: vec![r"C:\a".into(), r"C:\b".into()],
                    ..Default::default()
                }),
            ],
            CaptureScope::AllDesktops,
        );
        let l = live(vec![
            (CapturedItem::Uwp(UwpWindow { aumid: "app.id!app".into(), ..Default::default() }), true),
            (CapturedItem::Explorer(ExplorerWindow { folders: vec![r"c:\A".into()], ..Default::default() }), true),
        ]);
        let p = plan(&s, &l);
        assert_eq!(p.already_open.len(), 2, "UWP by AUMID, Explorer folder a");
        assert_eq!(p.launches.len(), 1);
        assert!(matches!(&p.launches[0].action, LaunchAction::Explorer { folder: Some(f) } if f == r"C:\b"));
        assert!(p.extras.is_empty());
    }

    #[test]
    fn one_explorer_window_with_tabs_satisfies_several_folders() {
        let s = state(
            vec![CapturedItem::Explorer(ExplorerWindow {
                folders: vec![r"C:\a".into(), r"C:\b".into()],
                ..Default::default()
            })],
            CaptureScope::AllDesktops,
        );
        let l = live(vec![(
            CapturedItem::Explorer(ExplorerWindow {
                folders: vec![r"C:\a".into(), r"C:\b".into()],
                ..Default::default()
            }),
            true,
        )]);
        let p = plan(&s, &l);
        assert!(p.launches.is_empty(), "{:?}", p.launches);
        assert_eq!(p.already_open.len(), 2);
    }

    #[test]
    fn vscode_windows_match_one_to_one_by_folder() {
        let a = "file:///c%3A/pers/research-mentor";
        let s = state(
            vec![
                code(Some(a), "x - research-mentor - Visual Studio Code"),
                code(Some("file:///c%3A/Users/me/saved_states"), "y - saved_states - Visual Studio Code"),
            ],
            CaptureScope::AllDesktops,
        );

        let p =
            plan(&s, &live(vec![(code(Some("file:///C:/Users/me/saved_states/"), "z - Visual Studio Code"), true)]));
        assert_eq!(p.already_open.len(), 1);
        assert_eq!(p.launches.len(), 1);
        assert!(matches!(&p.launches[0].action, LaunchAction::VsCode { folder: Some(f), .. } if f == a));
        assert!(p.extras.is_empty());

        // A window on another folder is not part of the state.
        let p = plan(&s, &live(vec![(code(Some("file:///c%3A/other"), "other - Visual Studio Code"), true)]));
        assert_eq!(p.launches.len(), 2);
        assert_eq!(p.extras.len(), 1);
    }

    #[test]
    fn empty_vscode_windows_match_one_to_one() {
        let s = state(vec![code(None, ""), code(None, "")], CaptureScope::AllDesktops);
        let p = plan(&s, &live(vec![(code(None, "Welcome - Visual Studio Code"), true)]));
        assert_eq!(p.already_open.len(), 1);
        assert_eq!(p.launches.len(), 1);
    }

    #[test]
    fn states_from_before_vscode_items_still_count_a_running_editor() {
        let s = state(vec![proc_(CODE, "a - Visual Studio Code", &[])], CaptureScope::AllDesktops);
        let l = live(vec![
            (code(Some("file:///c%3A/a"), "x - a - Visual Studio Code"), true),
            (code(None, "Welcome - Visual Studio Code"), true),
        ]);
        let p = plan(&s, &l);
        assert!(p.launches.is_empty());
        assert_eq!(p.already_open.len(), 1);
        assert!(p.extras.is_empty(), "every window of the running editor belongs to the state");
    }

    #[test]
    fn saved_vscode_windows_replace_a_bare_launch_of_the_editor() {
        let s = state(
            vec![code(Some("file:///c%3A/a"), "a - Visual Studio Code"), proc_(CODE, "b - Visual Studio Code", &[])],
            CaptureScope::AllDesktops,
        );
        let p = plan(&s, &live(vec![]));
        assert_eq!(p.launches.len(), 1);
        assert!(matches!(p.launches[0].action, LaunchAction::VsCode { .. }));
    }

    #[test]
    fn a_saved_folder_is_found_in_a_live_editor_read_as_a_process() {
        let s = state(vec![code(Some("file:///c%3A/x/proj"), "")], CaptureScope::AllDesktops);
        let p = plan(&s, &live(vec![(proc_(CODE, "main.rs - proj - Visual Studio Code", &[]), true)]));
        assert!(p.launches.is_empty());
        assert!(p.extras.is_empty());
    }
}
