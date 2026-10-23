//! VS Code and the editors built from it (Insiders, VSCodium, Cursor, ...): which folder or
//! workspace each window has open, and how to open it again.
//!
//! VS Code keeps its open windows as `windowsState` in `<user data>\User\globalStorage\storage.json`
//! and rewrites it whenever one of its windows loses focus, so by the time the user clicks our
//! window or the tray it lists every open window with its folder or workspace URI and its normal
//! bounds in DIPs. The file knows folders but not HWNDs; Win32 knows HWNDs but not folders.
//! Titles join them: VS Code titles a window "<editor> - <folder name> - Visual Studio Code" by
//! default. Bounds only break ties, because DIP positions on a monitor scaled differently from
//! the primary don't convert exactly to physical pixels.
//!
//! An editor is recognized by its `product.json`, not its exe name, so forks work too.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use crate::model::{Rect, ShowState, VsCodeWindow};
use crate::win::explorer::{file_url_to_path, percent_decode};

/// An installed VS Code-family editor, as found from one of its windows' executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Product {
    /// `nameLong` from `product.json` ("Visual Studio Code").
    pub name: String,
    /// Where the editor keeps its state.
    pub data_dir: PathBuf,
    /// The `--user-data-dir` the editor was started with, if any.
    pub user_data_dir_arg: Option<PathBuf>,
}

/// The editor `exe` belongs to, if it is one. `argv` is the command line of the window's
/// process (the editor's main process), which carries `--user-data-dir` when one is used.
pub fn detect(exe: &Path, argv: &[String]) -> Option<Product> {
    let install = exe.parent()?;
    let product: Value = serde_json::from_slice(&fs::read(product_json(install)?).ok()?).ok()?;
    let short = product.get("nameShort")?.as_str().filter(|s| !s.is_empty())?;
    let name = product.get("nameLong").and_then(Value::as_str).unwrap_or(short).to_string();
    let user_data_dir_arg = user_data_dir_arg(argv);
    // Relative to a working directory the editor has since left, so there's no telling where.
    if user_data_dir_arg.as_ref().is_some_and(|d| !d.is_absolute()) {
        return None;
    }
    // VS Code's own order (`getUserDataPath`). Its environment can't be read, so ours stands
    // in for it: a VSCODE_APPDATA the user set applies to both.
    let portable = install.join("data");
    let data_dir = if product.get("target").is_none() && portable.is_dir() {
        portable.join("user-data")
    } else if let Some(dir) = std::env::var_os("VSCODE_APPDATA").filter(|d| !d.is_empty()) {
        PathBuf::from(dir).join(short)
    } else if let Some(dir) = &user_data_dir_arg {
        dir.clone()
    } else {
        PathBuf::from(std::env::var_os("APPDATA")?).join(short)
    };
    Some(Product { name, data_dir, user_data_dir_arg })
}

/// `resources\app\product.json` beside the exe, or in the versioned folder current Windows
/// builds use (`<install>\<commit>\resources\app`). An update in progress can leave two
/// versioned folders; the newer one is running.
fn product_json(install: &Path) -> Option<PathBuf> {
    let rel = Path::new("resources").join("app").join("product.json");
    let direct = install.join(&rel);
    if direct.is_file() {
        return Some(direct);
    }
    fs::read_dir(install)
        .ok()?
        .flatten()
        .map(|e| e.path().join(&rel))
        .filter_map(|p| Some((p.metadata().ok()?.modified().ok()?, p)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

fn user_data_dir_arg(argv: &[String]) -> Option<PathBuf> {
    let mut args = argv.iter().skip(1);
    while let Some(a) = args.next() {
        let value = match a.strip_prefix("--user-data-dir") {
            Some(rest) if rest.starts_with('=') => rest[1..].to_string(),
            Some("") => args.next()?.clone(),
            _ => continue,
        };
        return Some(PathBuf::from(value.trim().trim_matches('"')));
    }
    None
}

/// A window VS Code says is open.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowEntry {
    pub folder: Option<String>,
    pub workspace: Option<String>,
    pub maximized: bool,
    /// Normal (restored) bounds in DIPs: left, top, width, height.
    pub bounds: Option<(i32, i32, i32, i32)>,
}

impl WindowEntry {
    fn label(&self) -> String {
        root_label(self.folder.as_deref(), self.workspace.as_deref())
    }

    fn is_workspace(&self) -> bool {
        self.folder.is_none() && self.workspace.is_some()
    }
}

/// The windows the editor with this data directory has open, or `None` if its state can't be read.
pub fn open_windows(data_dir: &Path) -> Option<Vec<WindowEntry>> {
    let path = data_dir.join("User").join("globalStorage").join("storage.json");
    if !path.is_file() {
        return None;
    }
    let read = || serde_json::from_slice::<Value>(&fs::read(&path).ok()?).ok();
    // A parse error usually means VS Code is in the middle of rewriting the file.
    let storage = read().or_else(|| {
        thread::sleep(Duration::from_millis(100));
        read()
    })?;
    Some(parse_windows_state(storage.get("windowsState")?))
}

/// `openedWindows` lists every window when there are several. With one window it is empty
/// and `lastActiveWindow` is that window. (Joining the two would bring back the state of a
/// window closed earlier, which VS Code keeps in `lastActiveWindow` for a while.)
fn parse_windows_state(state: &Value) -> Vec<WindowEntry> {
    let opened: Vec<WindowEntry> =
        state.get("openedWindows").and_then(Value::as_array).map(|a| a.iter().map(entry).collect()).unwrap_or_default();
    if !opened.is_empty() {
        return opened;
    }
    state.get("lastActiveWindow").map(entry).into_iter().collect()
}

fn entry(v: &Value) -> WindowEntry {
    let text = |p: &str| v.pointer(p).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let num = |k: &str| v.pointer(&format!("/uiState/{k}")).and_then(Value::as_f64).map(|n| n.round() as i32);
    let bounds = match (num("x"), num("y"), num("width"), num("height")) {
        (Some(x), Some(y), Some(w), Some(h)) if w > 0 && h > 0 => Some((x, y, w, h)),
        _ => None,
    };
    WindowEntry {
        folder: text("/folder"),
        workspace: text("/workspaceIdentifier/configURIPath"),
        // Modes: 0 maximized, 1 normal, 3 full screen (which restores as maximized here).
        maximized: matches!(num("mode"), Some(0 | 3)),
        bounds,
    }
}

/// The name VS Code shows for a folder or workspace in window titles (`${rootName}`), or ""
/// where that name is generic or localized (untitled workspaces, empty windows).
pub fn root_label(folder: Option<&str>, workspace: Option<&str>) -> String {
    match (folder, workspace) {
        (Some(f), _) => last_segment(f),
        (None, Some(w)) => {
            let name = last_segment(w);
            let stem = name.len().checked_sub(".code-workspace".len()).filter(|&n| name.is_char_boundary(n));
            match stem {
                Some(n) if name[n..].eq_ignore_ascii_case(".code-workspace") => name[..n].to_string(),
                _ => String::new(),
            }
        }
        (None, None) => String::new(),
    }
}

/// The decoded last segment of the URI's path, or "/" for a root, as VS Code labels one.
fn last_segment(uri: &str) -> String {
    let path = match uri.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("", |i| &rest[i..]),
        None => uri,
    };
    let name = percent_decode(path.trim_end_matches('/').rsplit('/').next().unwrap_or_default());
    if name.is_empty() { "/".to_string() } else { name }
}

/// Where `label` appears in a window title as its root name: the number of " - " separators
/// after it, or `None` if it isn't there. Fewer separators means more likely the root name
/// than an open file of the same name, since the default title ends
/// "<root name> - <profile> - <app name>".
///
/// The label must be a whole part of the title, apart from the suffixes VS Code adds:
/// " [SSH: host]" after a remote folder, " (Workspace)" (localized) after a workspace.
pub fn title_position(title: &str, label: &str, is_workspace: bool) -> Option<usize> {
    const SEP: &str = " - ";
    if label.is_empty() {
        return None;
    }
    let title = title.to_lowercase();
    // VS Code puts a red dot in front while a debug session is paused in an unfocused window.
    let title = title.strip_prefix("\u{1f534} ").unwrap_or(&title);
    let label = label.to_lowercase();
    title.rmatch_indices(label.as_str()).find_map(|(i, _)| {
        let before = &title[..i];
        let after = &title[i + label.len()..];
        let starts = before.is_empty() || before.ends_with(SEP) || before.ends_with("\u{25cf} ");
        let ends = if is_workspace {
            after.starts_with(" (") || after.starts_with(" \u{ff08}")
        } else {
            after.is_empty() || after.starts_with(SEP) || after.starts_with(" [")
        };
        (starts && ends).then(|| after.matches(SEP).count())
    })
}

/// What matching needs to know about a live window.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub title: String,
    /// `rcNormalPosition`: the restored rectangle, also while maximized or minimized.
    pub normal_rect: Rect,
    /// The visible frame right now (a snapped window's tile, say).
    pub frame: Rect,
    pub show: ShowState,
    pub dpi: u32,
}

/// What a live editor window turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// It shows this entry of `windowsState`.
    Entry(usize),
    /// Not a main window: developer tools, a floating editor window, Process Explorer.
    Auxiliary,
    /// Looks like a main window, but its title doesn't say which.
    Unknown,
}

/// Total width + height deviation (relative) within which an unlabeled entry pairs by size.
const MAX_SIZE_DEVIATION: f64 = 0.25;

/// What each window is (each entry used at most once), or `None` when the titles don't
/// follow VS Code's default format (a custom `window.title` or separator, most likely),
/// so no window can be told apart. `app_name` is the product's `nameLong`, which ends
/// every main window's default title.
pub fn match_windows(windows: &[WindowInfo], entries: &[WindowEntry], app_name: &str) -> Option<Vec<Match>> {
    let mut by_window = vec![None; windows.len()];
    let mut used = vec![false; entries.len()];
    let labels: Vec<(String, bool)> = entries.iter().map(|e| (e.label(), e.is_workspace())).collect();

    // Folders and workspaces: the title must show the name. State and bounds only break ties
    // (two folders with the same name; a floating editor window showing the same root name).
    let mut pairs: Vec<((usize, bool, f64), usize, usize)> = Vec::new();
    for (wi, w) in windows.iter().enumerate() {
        for (ei, (e, (label, is_workspace))) in entries.iter().zip(&labels).enumerate() {
            if let Some(pos) = title_position(&w.title, label, *is_workspace) {
                let state_differs = !state_compatible(w.show, e.maximized);
                pairs.push(((pos, state_differs, bounds_distance(w, e, false)), wi, ei));
            }
        }
    }
    assign(pairs, &mut by_window, &mut used);
    if by_window.iter().all(Option::is_none) && labels.iter().any(|(l, _)| !l.is_empty()) {
        return None;
    }

    // A window left over that still shows a folder or workspace name lost it to the window
    // that has it: a floating editor window. Developer tools and Process Explorer don't
    // carry the app name.
    let app_name = app_name.to_lowercase();
    let auxiliary: Vec<bool> = windows
        .iter()
        .map(|w| {
            labels.iter().any(|(l, ws)| title_position(&w.title, l, *ws).is_some())
                || !w.title.to_lowercase().contains(&app_name)
        })
        .collect();

    // Empty windows and untitled workspaces have generic titles: pair the rest by size.
    let mut pairs = Vec::new();
    for (wi, w) in windows.iter().enumerate().filter(|(wi, _)| by_window[*wi].is_none() && !auxiliary[*wi]) {
        for (ei, e) in entries.iter().enumerate().filter(|(ei, _)| !used[*ei] && labels[*ei].0.is_empty()) {
            let size = bounds_distance(w, e, true);
            if size <= MAX_SIZE_DEVIATION {
                pairs.push(((0, !state_compatible(w.show, e.maximized), size), wi, ei));
            }
        }
    }
    assign(pairs, &mut by_window, &mut used);
    if by_window.iter().all(Option::is_none) {
        return None;
    }
    Some(
        by_window
            .into_iter()
            .zip(auxiliary)
            .map(|(m, aux)| match m {
                Some(i) => Match::Entry(i),
                None if aux => Match::Auxiliary,
                None => Match::Unknown,
            })
            .collect(),
    )
}

/// Greedy one-to-one assignment by ascending cost.
fn assign(mut pairs: Vec<((usize, bool, f64), usize, usize)>, by_window: &mut [Option<usize>], used: &mut [bool]) {
    pairs.sort_by(|(a, aw, ae), (b, bw, be)| {
        a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.total_cmp(&b.2)).then(aw.cmp(bw)).then(ae.cmp(be))
    });
    for (_, wi, ei) in pairs {
        if by_window[wi].is_none() && !used[ei] {
            by_window[wi] = Some(ei);
            used[ei] = true;
        }
    }
}

/// VS Code never records a minimized window as such, so minimized fits either mode.
fn state_compatible(show: ShowState, maximized: bool) -> bool {
    match show {
        ShowState::Minimized => true,
        ShowState::Maximized => maximized,
        ShowState::Normal => !maximized,
    }
}

/// Distance between the window's rectangle (physical pixels) and the entry's DIP bounds
/// scaled by the window's DPI, relative to the window's size; `size_only` ignores the
/// position. Infinite when the entry has no bounds.
fn bounds_distance(w: &WindowInfo, e: &WindowEntry, size_only: bool) -> f64 {
    let Some((left, top, width, height)) = e.bounds else { return f64::INFINITY };
    let scale = w.dpi.max(96) as f64 / 96.0;
    let s = |v: i32| v as f64 * scale;
    // VS Code records a normal window's current bounds and a maximized one's restored bounds.
    let r = if w.show == ShowState::Normal { &w.frame } else { &w.normal_rect };
    let mut d = (r.width() as f64 - s(width)).abs() + (r.height() as f64 - s(height)).abs();
    if !size_only {
        d += (r.left as f64 - s(left)).abs() + (r.top as f64 - s(top)).abs();
    }
    d / (s(width) + s(height)).max(1.0)
}

/// Whether two saved or live windows show the same folder or workspace.
pub fn same_target(a: &VsCodeWindow, b: &VsCodeWindow) -> bool {
    same_uri(a.folder.as_deref(), b.folder.as_deref()) && same_uri(a.workspace.as_deref(), b.workspace.as_deref())
}

fn same_uri(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => match (local_path(a), local_path(b)) {
            // Windows paths: case-insensitive, and "C:\x\" is "C:\x".
            (Some(pa), Some(pb)) => {
                let norm = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_string();
                norm(&pa).eq_ignore_ascii_case(&norm(&pb))
            }
            // Remote paths may well be case-sensitive.
            _ => a == b,
        },
        _ => false,
    }
}

/// The local path a `file:` URI names; `None` for remote URIs.
pub fn local_path(uri: &str) -> Option<PathBuf> {
    let mut path = file_url_to_path(uri)?;
    // VS Code writes drive letters in lower case.
    if path.as_bytes().get(1) == Some(&b':') {
        path[..1].make_ascii_uppercase();
    }
    Some(PathBuf::from(path))
}

/// A URI for display: the path for local ones, the URI itself otherwise.
pub fn display_location(uri: &str) -> String {
    local_path(uri).map_or_else(|| uri.to_string(), |p| p.to_string_lossy().into_owned())
}

/// The editor's arguments that open `folder` or `workspace` (or, with neither, an empty
/// window) in a new window.
pub fn launch_args(user_data_dir: Option<&Path>, folder: Option<&str>, workspace: Option<&str>) -> Vec<String> {
    let mut args = vec!["--new-window".to_string()];
    if let Some(dir) = user_data_dir {
        args.push("--user-data-dir".into());
        args.push(dir.to_string_lossy().into_owned());
    }
    let (uri, uri_switch) = match (folder, workspace) {
        (Some(f), _) => (f, "--folder-uri"),
        (None, Some(w)) => (w, "--file-uri"),
        (None, None) => return args,
    };
    match local_path(uri) {
        Some(path) => args.push(path.to_string_lossy().into_owned()),
        None => {
            args.push(uri_switch.into());
            args.push(uri.into());
        }
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("savestates-vscode-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn folder(path: &str) -> WindowEntry {
        WindowEntry { folder: Some(path.into()), ..Default::default() }
    }

    fn win(title: &str, show: ShowState, rect: Rect, dpi: u32) -> WindowInfo {
        WindowInfo { title: title.into(), normal_rect: rect, frame: rect, show, dpi }
    }

    const APP: &str = "Visual Studio Code";

    fn r(left: i32, top: i32, w: i32, h: i32) -> Rect {
        Rect { left, top, right: left + w, bottom: top + h }
    }

    #[test]
    fn detects_versioned_and_portable_installs() {
        let install = temp_dir("install");
        let app = install.join("2d0b7506a3").join("resources").join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("product.json"), r#"{"nameShort":"Code","nameLong":"Visual Studio Code"}"#).unwrap();
        let exe = install.join("Code.exe");

        let p = detect(&exe, &[]).unwrap();
        assert_eq!(p.name, "Visual Studio Code");
        // VSCODE_APPDATA, when the user set one, takes precedence over the rest.
        let vscode_appdata = std::env::var_os("VSCODE_APPDATA").filter(|d| !d.is_empty());
        if vscode_appdata.is_none()
            && let Some(appdata) = std::env::var_os("APPDATA")
        {
            assert_eq!(p.data_dir, PathBuf::from(appdata).join("Code"));
        }

        let custom = std::env::temp_dir().join("custom profile");
        let argv = vec!["Code.exe".to_string(), "--user-data-dir".into(), custom.to_string_lossy().into_owned()];
        let p = detect(&exe, &argv).unwrap();
        assert_eq!(p.user_data_dir_arg, Some(custom.clone()));
        if vscode_appdata.is_none() {
            assert_eq!(p.data_dir, custom);
            let argv = vec!["Code.exe".to_string(), format!("--user-data-dir={}", custom.display())];
            assert_eq!(detect(&exe, &argv).unwrap().data_dir, custom);
        }
        let relative = vec!["Code.exe".to_string(), "--user-data-dir=profile".into()];
        assert!(detect(&exe, &relative).is_none(), "a relative data dir can't be located");

        // A `data` folder makes a zip install portable; installer builds ("target") ignore it.
        fs::create_dir_all(install.join("data")).unwrap();
        assert_eq!(detect(&exe, &[]).unwrap().data_dir, install.join("data").join("user-data"));
        fs::write(app.join("product.json"), r#"{"nameShort":"Code","nameLong":"Visual Studio Code","target":"user"}"#)
            .unwrap();
        assert_ne!(detect(&exe, &[]).unwrap().data_dir, install.join("data").join("user-data"));

        assert!(detect(&temp_dir("not-an-editor").join("slack.exe"), &[]).is_none());
        let _ = fs::remove_dir_all(install);
    }

    #[test]
    fn reads_the_open_windows() {
        // Shaped like a real storage.json from VS Code 1.143.
        let state: Value = serde_json::from_str(
            r#"{
              "lastActiveWindow": {"folder": "file:///c%3A/pers/research-mentor", "uiState": {"mode": 0, "x": 254, "y": 134, "width": 1202, "height": 801}},
              "openedWindows": [
                {"folder": "file:///c%3A/pers/research-mentor", "uiState": {"mode": 0, "x": 254, "y": 134, "width": 1202, "height": 801}},
                {"workspaceIdentifier": {"id": "abc", "configURIPath": "file:///c%3A/w/team.code-workspace"}, "uiState": {"mode": 1, "x": -1500, "y": 20, "width": 1200, "height": 800}},
                {"folder": "vscode-remote://wsl%2Bubuntu/home/me/proj", "remoteAuthority": "wsl+ubuntu", "uiState": {"mode": 1, "x": 0, "y": 0, "width": 800, "height": 600}},
                {"backupPath": "C:\\x\\Backups\\1792719041088", "uiState": {"mode": 1, "x": 10, "y": 10, "width": 800, "height": 600}}
              ]
            }"#,
        )
        .unwrap();
        let entries = parse_windows_state(&state);
        assert_eq!(entries.len(), 4, "openedWindows already includes the last active window");
        assert_eq!(entries[0].folder.as_deref(), Some("file:///c%3A/pers/research-mentor"));
        assert!(entries[0].maximized);
        assert_eq!(entries[0].bounds, Some((254, 134, 1202, 801)));
        assert_eq!(entries[1].workspace.as_deref(), Some("file:///c%3A/w/team.code-workspace"));
        assert!(!entries[1].maximized);
        assert_eq!(entries[2].folder.as_deref(), Some("vscode-remote://wsl%2Bubuntu/home/me/proj"));
        assert_eq!((entries[3].folder.clone(), entries[3].workspace.clone()), (None, None));

        // One window: openedWindows is empty.
        let state: Value = serde_json::from_str(
            r#"{"lastActiveWindow": {"folder": "file:///c%3A/a", "uiState": {"mode": 1, "x": 0, "y": 0, "width": 9, "height": 9}}, "openedWindows": []}"#,
        )
        .unwrap();
        assert_eq!(
            parse_windows_state(&state),
            vec![WindowEntry {
                folder: Some("file:///c%3A/a".into()),
                bounds: Some((0, 0, 9, 9)),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn open_windows_reads_storage_json() {
        let data = temp_dir("data");
        assert!(open_windows(&data).is_none(), "no state file");
        let dir = data.join("User").join("globalStorage");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("storage.json"), r#"{"windowsState": {"lastActiveWindow": {"folder": "file:///c%3A/a"}}}"#)
            .unwrap();
        assert_eq!(open_windows(&data).unwrap().len(), 1);
        fs::write(dir.join("storage.json"), r#"{"windowsState": {"lastActi"#).unwrap();
        assert!(open_windows(&data).is_none(), "a file that stays truncated is unreadable");
        let _ = fs::remove_dir_all(data);
    }

    #[test]
    fn root_labels() {
        assert_eq!(root_label(Some("file:///c%3A/Users/me/saved_states"), None), "saved_states");
        assert_eq!(root_label(Some("file:///c%3A/Users/me/My%20Project/"), None), "My Project");
        assert_eq!(root_label(Some("vscode-remote://wsl%2Bubuntu/home/me/proj"), None), "proj");
        assert_eq!(root_label(None, Some("file:///c%3A/w/team.code-workspace")), "team");
        assert_eq!(
            root_label(None, Some("file:///c%3A/Users/me/AppData/Roaming/Code/Workspaces/17/workspace.json")),
            ""
        );
        assert_eq!(root_label(None, None), "");
        // A root folder is labeled "/" (VS Code's `basename(uri) || "/"`), never the authority.
        assert_eq!(root_label(Some("vscode-remote://ssh-remote%2Bbox/"), None), "/");
        assert_eq!(root_label(Some("file://server/share/proj"), None), "proj");
    }

    #[test]
    fn title_positions() {
        let vsc = " - Visual Studio Code";
        assert_eq!(title_position(&format!("main.rs - saved_states{vsc}"), "saved_states", false), Some(1));
        assert_eq!(title_position(&format!("saved_states{vsc}"), "Saved_States", false), Some(1));
        assert_eq!(title_position(&format!("\u{25cf} main.rs - app - Work{vsc}"), "app", false), Some(2));
        assert_eq!(title_position(&format!("x.py - proj [WSL: Ubuntu]{vsc}"), "proj", false), Some(1));
        assert_eq!(title_position(&format!("a.ts - team (Workspace){vsc}"), "team", true), Some(1));
        assert_eq!(title_position(&format!("a.ts - team{vsc}"), "team", true), None, "a folder, not the workspace");
        assert_eq!(title_position(&format!("a.ts - X - Copy{vsc}"), "X - Copy", false), Some(1));
        assert_eq!(title_position(&format!("a.ts - foo (old){vsc}"), "foo", false), None);
        assert_eq!(title_position(&format!("a.ts - foobar{vsc}"), "foo", false), None);
        // An open file named like another folder sits further from the end.
        assert_eq!(title_position(&format!("notes - app{vsc}"), "notes", false), Some(2));
        assert_eq!(title_position(&format!("notes - app{vsc}"), "app", false), Some(1));
        assert_eq!(title_position("Visual Studio Code", "", false), None);
        // A paused debug session in an unfocused window.
        assert_eq!(title_position(&format!("\u{1f534} proj{vsc}"), "proj", false), Some(1));
        assert_eq!(title_position(&format!("x - / [SSH: box]{vsc}"), "/", false), Some(1));
    }

    const DEVTOOLS: &str =
        "Developer Tools - vscode-file://vscode-app/c:/Code/resources/app/out/vs/code/workbench.html";

    #[test]
    fn matches_by_title_and_sets_other_windows_aside() {
        let entries = [folder("file:///c%3A/pers/research-mentor"), folder("file:///c%3A/x/saved_states")];
        let windows = [
            win(DEVTOOLS, ShowState::Normal, r(0, 0, 900, 700), 96),
            win("main.rs - saved_states - Visual Studio Code", ShowState::Maximized, r(0, 0, 1200, 800), 96),
            win("Process Explorer", ShowState::Normal, r(0, 0, 800, 500), 96),
            win("README.md - research-mentor - Visual Studio Code", ShowState::Normal, r(0, 0, 1200, 800), 96),
            // Opened since VS Code last saved its window list: a main window, but which one is unknown.
            win("notes.md - fresh - Visual Studio Code", ShowState::Normal, r(0, 0, 1200, 800), 96),
        ];
        let m = match_windows(&windows, &entries, APP).unwrap();
        assert_eq!(m, vec![Match::Auxiliary, Match::Entry(1), Match::Auxiliary, Match::Entry(0), Match::Unknown]);
    }

    #[test]
    fn titles_in_another_format_match_nothing() {
        // A custom separator: no title shows a folder the default way. Pairing the empty
        // window by size would be a guess, and the other windows would be lost.
        let entries = [
            folder("file:///c%3A/x/alpha"),
            folder("file:///c%3A/x/beta"),
            WindowEntry { bounds: Some((0, 0, 1200, 800)), ..Default::default() },
        ];
        let windows = [
            win("main.rs | alpha | Visual Studio Code", ShowState::Normal, r(0, 0, 1200, 800), 96),
            win("lib.rs | beta | Visual Studio Code", ShowState::Normal, r(0, 0, 1200, 800), 96),
            win("Visual Studio Code", ShowState::Normal, r(0, 0, 1200, 800), 96),
        ];
        assert_eq!(match_windows(&windows, &entries, APP), None);
        assert_eq!(match_windows(&[win(DEVTOOLS, ShowState::Normal, r(0, 0, 9, 9), 96)], &entries[2..], APP), None);
    }

    #[test]
    fn bounds_break_a_title_tie() {
        // Two folders called "app"; 150% scaling turns 800x600 DIPs at (100,100) into 1200x900 px at (150,150).
        let mut a = folder("file:///c%3A/one/app");
        a.bounds = Some((100, 100, 800, 600));
        let mut b = folder("file:///c%3A/two/app");
        b.bounds = Some((1000, 100, 800, 600));
        let windows = [
            win("x - app - Visual Studio Code", ShowState::Normal, r(1500, 150, 1200, 900), 144),
            win("y - app - Visual Studio Code", ShowState::Normal, r(150, 150, 1200, 900), 144),
        ];
        assert_eq!(match_windows(&windows, &[a, b], APP).unwrap(), vec![Match::Entry(1), Match::Entry(0)]);
    }

    #[test]
    fn a_floating_editor_window_loses_to_the_main_window() {
        let mut e = folder("file:///c%3A/x/proj");
        e.bounds = Some((0, 0, 1200, 800));
        let windows = [
            win("lib.rs - proj - Visual Studio Code", ShowState::Normal, r(1300, 200, 600, 500), 96),
            win("main.rs - proj - Visual Studio Code", ShowState::Normal, r(0, 0, 1200, 800), 96),
        ];
        assert_eq!(match_windows(&windows, &[e], APP).unwrap(), vec![Match::Auxiliary, Match::Entry(0)]);
    }

    #[test]
    fn title_alone_is_enough_when_bounds_disagree() {
        // Minimized on a second monitor at another scale: neither state nor bounds line up.
        let mut e = folder("file:///c%3A/x/proj");
        e.maximized = true;
        e.bounds = Some((2560, 0, 1200, 800));
        let windows = [win("proj - Visual Studio Code", ShowState::Minimized, r(5120, 40, 1800, 1200), 144)];
        assert_eq!(match_windows(&windows, &[e], APP).unwrap(), vec![Match::Entry(0)]);
    }

    #[test]
    fn empty_windows_pair_by_size() {
        let empty = WindowEntry { bounds: Some((0, 0, 1000, 700)), ..Default::default() };
        let windows = [
            win(DEVTOOLS, ShowState::Normal, r(0, 0, 1000, 700), 96),
            win("Welcome - Visual Studio Code", ShowState::Normal, r(300, 300, 1500, 1050), 144),
        ];
        assert_eq!(match_windows(&windows, &[empty], APP).unwrap(), vec![Match::Auxiliary, Match::Entry(0)]);
    }

    #[test]
    fn a_snapped_window_is_compared_by_its_tile() {
        // VS Code records a normal window's current bounds; rcNormalPosition still has the
        // size from before it was snapped.
        let empty = WindowEntry { bounds: Some((0, 0, 960, 516)), ..Default::default() };
        let snapped = WindowInfo {
            frame: r(0, 0, 960, 516),
            ..win("Welcome - Visual Studio Code", ShowState::Normal, r(0, 0, 1200, 800), 96)
        };
        assert_eq!(match_windows(&[snapped], &[empty], APP).unwrap(), vec![Match::Entry(0)]);
    }

    #[test]
    fn a_floating_editor_window_never_takes_an_empty_window_entry() {
        // The empty window is on another desktop, so it isn't among the windows matched.
        let proj = WindowEntry { maximized: true, bounds: Some((0, 0, 1900, 1000)), ..folder("file:///c%3A/x/proj") };
        let entries = [proj, WindowEntry { bounds: Some((0, 0, 1000, 700)), ..Default::default() }];
        let windows = [
            win("main.rs - proj - Visual Studio Code", ShowState::Maximized, r(0, 0, 1900, 1000), 96),
            win("notes.md - proj - Visual Studio Code", ShowState::Normal, r(0, 0, 1000, 700), 96),
        ];
        assert_eq!(match_windows(&windows, &entries, APP).unwrap(), vec![Match::Entry(0), Match::Auxiliary]);
    }

    #[test]
    fn targets_compare_like_paths() {
        let w = |folder: &str| VsCodeWindow { folder: Some(folder.into()), ..Default::default() };
        assert!(same_target(&w("file:///c%3A/Users/Me/proj"), &w("file:///C:/users/me/proj/")));
        assert!(!same_target(&w("file:///c%3A/a"), &w("file:///c%3A/b")));
        assert!(!same_target(
            &w("vscode-remote://wsl%2Bubuntu/home/Proj"),
            &w("vscode-remote://wsl%2Bubuntu/home/proj")
        ));
        assert!(same_target(&VsCodeWindow::default(), &VsCodeWindow::default()));
        let ws = VsCodeWindow { workspace: Some("file:///c%3A/a".into()), ..Default::default() };
        assert!(!same_target(&w("file:///c%3A/a"), &ws), "a folder is not a workspace");
    }

    #[test]
    fn launch_arguments() {
        assert_eq!(
            launch_args(None, Some("file:///c%3A/My%20Projects/app"), None),
            vec!["--new-window", r"C:\My Projects\app"]
        );
        assert_eq!(
            launch_args(None, None, Some("file:///c%3A/w/team.code-workspace")),
            vec!["--new-window", r"C:\w\team.code-workspace"]
        );
        assert_eq!(
            launch_args(None, Some("vscode-remote://wsl%2Bubuntu/home/me/proj"), None),
            vec!["--new-window", "--folder-uri", "vscode-remote://wsl%2Bubuntu/home/me/proj"]
        );
        assert_eq!(
            launch_args(None, None, Some("vscode-remote://ssh-remote%2Bbox/w.code-workspace")),
            vec!["--new-window", "--file-uri", "vscode-remote://ssh-remote%2Bbox/w.code-workspace"]
        );
        assert_eq!(
            launch_args(Some(Path::new(r"D:\profiles\work")), None, None),
            vec!["--new-window", "--user-data-dir", r"D:\profiles\work"]
        );
    }

    #[test]
    fn display_locations() {
        assert_eq!(display_location("file:///c%3A/Users/me/proj"), r"C:\Users\me\proj");
        assert_eq!(display_location("file://server/share/proj"), r"\\server\share\proj");
        assert_eq!(display_location("vscode-remote://wsl%2Bubuntu/home"), "vscode-remote://wsl%2Bubuntu/home");
    }
}
