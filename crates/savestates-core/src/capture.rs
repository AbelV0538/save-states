//! Capturing the live desktop: enumerate windows, classify each into the item it
//! would be saved as, and attach Chrome tabs from the extension's snapshots.
//!
//! The same classification feeds restore planning (to see what is already open), so
//! [`collect`] is separate from [`capture`], which adds the screenshot and packaging.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::chrome::matching::{self, Candidate, HwndInfo};
use crate::chrome::{BridgeHub, ChromeEnv, ExtSnapshot, InstanceMap, snapshot};
use crate::desktops;
use crate::model::{
    CaptureScope, CapturedItem, ChromeTab, ChromeWindow, DesktopRef, ExplorerWindow, Geometry, ProcessWindow,
    SaveState, StoredItem, TabGroup, Thumbnail, UwpWindow,
};
use crate::paths::AppPaths;
use crate::screenshot;
use crate::win::enumerate::{self, LiveWindow};
use crate::win::{explorer, process};

pub struct CaptureContext<'a> {
    pub paths: &'a AppPaths,
    /// Live bridges, for asking extensions for fresh snapshots. `None` uses only the
    /// snapshots the bridges cached on disk.
    pub hub: Option<&'a BridgeHub>,
}

#[derive(Debug, Clone)]
pub struct LiveItem {
    pub window: LiveWindow,
    pub item: CapturedItem,
}

#[derive(Debug, Clone, Default)]
pub struct LiveSession {
    pub items: Vec<LiveItem>,
    pub desktops: Vec<DesktopRef>,
    /// True if at least one Chrome window's tabs came from the extension.
    pub chrome_tabs_known: bool,
    /// Chrome windows whose tabs could not be read (no extension / no match).
    pub chrome_windows_without_tabs: usize,
}

/// Capture the current desktop into a new [`SaveState`]. Call from a thread with COM
/// initialized (see [`crate::win::ComApartment`]), after hiding any of our own windows.
pub fn capture(name: String, scope: CaptureScope, ctx: &CaptureContext<'_>) -> Result<SaveState> {
    let id = Uuid::new_v4();
    // Screenshot first: it should show the desktop at the moment the user asked.
    let thumb_file = format!("{id}.jpg");
    let thumbnail = match screenshot::capture_thumbnail(&ctx.paths.thumbs_dir().join(&thumb_file)) {
        Ok(()) => Thumbnail { file: Some(thumb_file), custom: false },
        Err(e) => {
            warn!("thumbnail capture failed: {e:#}");
            Thumbnail::default()
        }
    };
    let session = collect(scope, ctx);
    let now = Utc::now();
    Ok(SaveState {
        id,
        name,
        created_at: now,
        updated_at: now,
        favorite: false,
        thumbnail,
        scope,
        desktops: session.desktops,
        items: session.items.into_iter().map(|li| StoredItem::Known(li.item)).collect(),
        extra: Default::default(),
    })
}

/// Enumerate and classify every app window in `scope`.
pub fn collect(scope: CaptureScope, ctx: &CaptureContext<'_>) -> LiveSession {
    let mut windows = enumerate::app_windows(std::process::id());
    if scope == CaptureScope::CurrentDesktop {
        windows.retain(|w| w.on_current_desktop);
    }

    let desktop_list = desktops::list();
    let current_desktop = desktops::current_index();
    let desktop_of: HashMap<isize, u32> = windows
        .iter()
        .filter_map(|w| {
            let idx = desktops::index_of_window(w.hwnd).or(if w.on_current_desktop { current_desktop } else { None });
            Some((w.hwnd, idx?))
        })
        .collect();
    let geometry = |w: &LiveWindow| Geometry {
        normal_rect: Some(w.normal_rect),
        show: w.show,
        dpi: w.dpi,
        desktop: desktop_of.get(&w.hwnd).map(|&i| i as usize),
    };

    let env = ChromeEnv::load();
    let chrome = ChromeTabs::resolve(&windows, &env, ctx);

    let explorer_folders =
        if windows.iter().any(is_explorer_window) { explorer::open_folders() } else { HashMap::new() };

    let process_pids: Vec<u32> = windows
        .iter()
        .filter(|w| {
            !chrome.windows.contains_key(&w.hwnd) && !is_explorer_window(w) && w.class != "ApplicationFrameWindow"
        })
        .map(|w| w.pid)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let commands = process::command_info(&process_pids);

    let mut items = Vec::with_capacity(windows.len());
    let mut seen_aumids: HashSet<String> = HashSet::new();
    for w in windows {
        let geom = geometry(&w);
        let item = if let Some(cw) = chrome.windows.get(&w.hwnd) {
            CapturedItem::Chrome(ChromeWindow { geometry: geom, ..cw.clone() })
        } else if let Some(aumid) = packaged_aumid(&w) {
            // Store apps and packaged Win32 apps relaunch by identity, not by exe path.
            // They are single-instance in practice, so one item per app.
            if !seen_aumids.insert(aumid.to_ascii_lowercase()) {
                continue;
            }
            CapturedItem::Uwp(UwpWindow { aumid, title: w.title.clone(), geometry: geom })
        } else if w.class == "ApplicationFrameWindow" {
            debug!(title = %w.title, "skipping UWP frame without an AppUserModelID");
            continue;
        } else if is_explorer_window(&w) {
            let folders = explorer_folders.get(&w.hwnd).cloned().unwrap_or_default();
            CapturedItem::Explorer(ExplorerWindow { folders, title: w.title.clone(), geometry: geom })
        } else {
            let Some(exe) = w.exe.clone() else {
                debug!(title = %w.title, "skipping window whose process image is unreadable");
                continue;
            };
            let cmd = commands.get(&w.pid).cloned().unwrap_or_default();
            CapturedItem::Process(ProcessWindow {
                args: process::replayable_args(&cmd.argv, &exe),
                cmdline: cmd.argv,
                cwd: cmd.cwd,
                exe,
                title: w.title.clone(),
                class: w.class.clone(),
                geometry: geom,
            })
        };
        items.push(LiveItem { window: w, item });
    }

    LiveSession {
        items,
        desktops: desktop_list,
        chrome_tabs_known: chrome.tabs_known,
        chrome_windows_without_tabs: chrome.without_tabs,
    }
}

/// Refresh the learned extension-instance -> profile mapping (e.g. right after a bridge
/// connects) without doing the rest of a capture.
pub fn link_chrome_instances(ctx: &CaptureContext<'_>) {
    let windows = enumerate::app_windows(std::process::id());
    let _ = ChromeTabs::resolve(&windows, &ChromeEnv::load(), ctx);
}

fn is_explorer_window(w: &LiveWindow) -> bool {
    w.class == "CabinetWClass" && w.exe_is("explorer.exe")
}

fn packaged_aumid(w: &LiveWindow) -> Option<String> {
    if w.class == "ApplicationFrameWindow" {
        return w.aumid.clone();
    }
    let exe = w.exe.as_ref()?;
    if !process::is_packaged_path(exe) {
        return None;
    }
    w.aumid.clone().or_else(|| process::package_aumid(w.pid))
}

/// Chrome windows (by HWND) with their profile and, where the extension could tell us, tabs.
struct ChromeTabs {
    windows: HashMap<isize, ChromeWindow>,
    tabs_known: bool,
    without_tabs: usize,
}

impl ChromeTabs {
    fn resolve(windows: &[LiveWindow], env: &ChromeEnv, ctx: &CaptureContext<'_>) -> Self {
        let chrome: Vec<(&LiveWindow, String, Option<String>)> = windows
            .iter()
            .filter(|w| w.exe_is("chrome.exe") && w.class.starts_with("Chrome_WidgetWin"))
            .filter_map(|w| {
                let parsed = env.parse_window_aumid(w.aumid.as_deref()?)?;
                Some((w, parsed.profile_dir, parsed.app_id))
            })
            .collect();
        if chrome.is_empty() {
            return Self { windows: HashMap::new(), tabs_known: false, without_tabs: 0 };
        }

        let snapshots = gather_snapshots(ctx);
        let instances_path = ctx.paths.chrome_instances_file();
        let mut instances = InstanceMap::load(&instances_path);

        let hwnd_infos: Vec<HwndInfo> = chrome
            .iter()
            .map(|(w, profile, _)| HwndInfo {
                title: w.title.clone(),
                rect: w.rect,
                frame: w.frame,
                dpi: w.dpi,
                show: w.show,
                profile_dir: profile.clone(),
            })
            .collect();
        let owners: Vec<(&ExtSnapshot, usize)> =
            snapshots.iter().flat_map(|s| (0..s.windows.len()).map(move |i| (s, i))).collect();
        let candidates: Vec<Candidate<'_>> = owners
            .iter()
            .map(|(s, i)| Candidate { window: &s.windows[*i], profile: instances.profile_of(&s.instance) })
            .collect();
        let matches = matching::match_windows(&hwnd_infos, &candidates);

        let mut learned = Vec::new();
        let mut out = HashMap::new();
        let (mut tabs_known, mut without_tabs) = (false, 0);
        for ((w, profile, app_id), (info, m)) in chrome.iter().zip(hwnd_infos.iter().zip(&matches)) {
            let mut cw = ChromeWindow {
                profile_dir: profile.clone(),
                profile_label: env.label(profile),
                exe: w.exe.clone(),
                app_id: app_id.clone(),
                title: w.title.clone(),
                ..Default::default()
            };
            match m {
                Some(ci) => {
                    let (snap, wi) = owners[*ci];
                    let ext = &snap.windows[wi];
                    cw.tabs = ext
                        .tabs
                        .iter()
                        .filter(|t| !t.url.is_empty())
                        .map(|t| ChromeTab {
                            url: t.url.clone(),
                            title: t.title.clone(),
                            pinned: t.pinned,
                            active: t.active,
                            group: (t.group_id >= 0).then_some(t.group_id),
                        })
                        .collect();
                    cw.groups = snap
                        .groups
                        .iter()
                        .filter(|g| g.window_id == ext.id)
                        .map(|g| TabGroup {
                            id: g.id,
                            title: g.title.clone(),
                            color: g.color.clone(),
                            collapsed: g.collapsed,
                        })
                        .collect();
                    cw.tabs_captured = true;
                    tabs_known = true;
                    if matching::title_matches_exactly(info, ext) {
                        learned.push((snap.instance.clone(), profile.clone()));
                    }
                }
                None => without_tabs += 1,
            }
            out.insert(w.hwnd, cw);
        }

        let mut changed = false;
        for (instance, profile) in learned {
            changed |= instances.learn(&instance, &profile);
        }
        if changed && let Err(e) = instances.save(&instances_path) {
            warn!("saving Chrome instance map: {e}");
        }
        Self { windows: out, tabs_known, without_tabs }
    }
}

/// Fresh snapshots from connected bridges, plus disk-cached ones for any instance that
/// didn't answer. Stale cache entries are harmless: they only contribute tabs if they
/// still match a live window by title and bounds.
fn gather_snapshots(ctx: &CaptureContext<'_>) -> Vec<ExtSnapshot> {
    let mut snaps = ctx.hub.map(|h| h.fresh_snapshots(Duration::from_millis(1500))).unwrap_or_default();
    let fresh: HashSet<String> = snaps.iter().map(|s| s.instance.clone()).collect();
    snaps.extend(snapshot::load_cached(&ctx.paths.live_dir()).into_iter().filter(|s| !fresh.contains(&s.instance)));
    snaps
}
