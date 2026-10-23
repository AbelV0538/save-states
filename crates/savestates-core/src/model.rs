//! The persisted data model. Everything here is serde, and every field added after
//! v1 must carry `#[serde(default)]` so older `states.json` files keep loading.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreFile {
    pub version: u32,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub states: Vec<SaveState>,
}

impl Default for StoreFile {
    fn default() -> Self {
        Self { version: STORE_VERSION, settings: Settings::default(), states: Vec::new() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Remembered answer to the save-time "which desktops?" prompt. `None` = ask every time.
    pub default_scope: Option<CaptureScope>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureScope {
    #[default]
    CurrentDesktop,
    AllDesktops,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveState {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub thumbnail: Thumbnail,
    #[serde(default)]
    pub scope: CaptureScope,
    #[serde(default)]
    pub desktops: Vec<DesktopRef>,
    #[serde(default)]
    pub items: Vec<StoredItem>,
    /// Fields written by a newer version of the app, preserved on save.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl SaveState {
    pub fn known_items(&self) -> impl Iterator<Item = &CapturedItem> {
        self.items.iter().filter_map(|slot| match slot {
            StoredItem::Known(item) => Some(item),
            StoredItem::Unknown(_) => None,
        })
    }

    pub fn summary(&self) -> ItemSummary {
        let mut s = ItemSummary::default();
        for item in self.known_items() {
            match item {
                CapturedItem::Chrome(w) => {
                    s.chrome_windows += 1;
                    s.chrome_tabs += w.tabs.len();
                }
                CapturedItem::Explorer(_) => s.folders += 1,
                CapturedItem::VsCode(_) => s.vscode_windows += 1,
                CapturedItem::Uwp(_) | CapturedItem::Process(_) => s.apps += 1,
            }
        }
        s
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemSummary {
    pub apps: usize,
    pub folders: usize,
    pub chrome_windows: usize,
    pub chrome_tabs: usize,
    pub vscode_windows: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Thumbnail {
    /// File name inside the thumbnails directory; `None` if no screenshot could be taken.
    pub file: Option<String>,
    /// A user-chosen image. Re-capturing a state never overwrites a custom thumbnail.
    pub custom: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopRef {
    pub index: u32,
    pub guid: Option<String>,
    pub name: Option<String>,
}

/// One captured item, or an item of a kind this version doesn't understand.
/// Unknown kinds are kept verbatim so a downgrade never destroys data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StoredItem {
    Known(CapturedItem),
    Unknown(serde_json::Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapturedItem {
    Chrome(ChromeWindow),
    Uwp(UwpWindow),
    Explorer(ExplorerWindow),
    VsCode(VsCodeWindow),
    Process(ProcessWindow),
}

impl CapturedItem {
    pub fn geometry(&self) -> &Geometry {
        match self {
            CapturedItem::Chrome(w) => &w.geometry,
            CapturedItem::Uwp(w) => &w.geometry,
            CapturedItem::Explorer(w) => &w.geometry,
            CapturedItem::VsCode(w) => &w.geometry,
            CapturedItem::Process(w) => &w.geometry,
        }
    }

    pub fn geometry_mut(&mut self) -> &mut Geometry {
        match self {
            CapturedItem::Chrome(w) => &mut w.geometry,
            CapturedItem::Uwp(w) => &mut w.geometry,
            CapturedItem::Explorer(w) => &mut w.geometry,
            CapturedItem::VsCode(w) => &mut w.geometry,
            CapturedItem::Process(w) => &mut w.geometry,
        }
    }

    /// Short human-readable label for lists and reports.
    pub fn label(&self) -> String {
        match self {
            CapturedItem::Chrome(w) => {
                let what = if w.app_id.is_some() { "Chrome app" } else { "Chrome" };
                if w.tabs.is_empty() {
                    format!("{what} ({}) - {}", w.profile_label, w.title)
                } else {
                    format!("{what} ({}) - {} tabs", w.profile_label, w.tabs.len())
                }
            }
            CapturedItem::Uwp(w) => w.title.clone(),
            CapturedItem::Explorer(w) => match w.folders.first() {
                Some(f) if w.folders.len() > 1 => format!("Explorer - {f} (+{})", w.folders.len() - 1),
                Some(f) => format!("Explorer - {f}"),
                None => "Explorer".to_string(),
            },
            CapturedItem::VsCode(w) => {
                let product = if w.product.is_empty() { "VS Code" } else { w.product.as_str() };
                match (&w.folder, &w.workspace) {
                    (Some(f), _) => format!("{product} - {}", crate::vscode::display_location(f)),
                    (None, Some(ws)) => format!("{product} - {} (workspace)", crate::vscode::display_location(ws)),
                    (None, None) => format!("{product} (empty window)"),
                }
            }
            CapturedItem::Process(w) => {
                let exe = w.exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if w.title.is_empty() { exe } else { format!("{} ({exe})", w.title) }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShowState {
    #[default]
    Normal,
    Maximized,
    Minimized,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Geometry {
    /// `GetWindowPlacement().rcNormalPosition`, in physical pixels (workspace coordinates).
    pub normal_rect: Option<Rect>,
    pub show: ShowState,
    pub dpi: u32,
    /// Index into [`SaveState::desktops`].
    pub desktop: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChromeWindow {
    /// What `--profile-directory` needs: "Default", "Profile 2", ...
    pub profile_dir: String,
    /// The profile's display name, for the UI.
    pub profile_label: String,
    /// chrome.exe at capture time.
    pub exe: Option<PathBuf>,
    /// Set for installed web apps (PWAs); restored with `--app-id`.
    pub app_id: Option<String>,
    pub title: String,
    /// False when the companion extension wasn't available, so `tabs` is unknown rather than empty.
    pub tabs_captured: bool,
    pub tabs: Vec<ChromeTab>,
    pub groups: Vec<TabGroup>,
    pub geometry: Geometry,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChromeTab {
    pub url: String,
    pub title: String,
    pub pinned: bool,
    pub active: bool,
    /// Id of a [`TabGroup`] in the same window.
    pub group: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabGroup {
    pub id: i64,
    pub title: String,
    pub color: String,
    pub collapsed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UwpWindow {
    pub aumid: String,
    pub title: String,
    pub geometry: Geometry,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExplorerWindow {
    /// Parsing names: plain paths, or `::{GUID}` shell locations like This PC. One per tab.
    pub folders: Vec<String>,
    pub title: String,
    pub geometry: Geometry,
}

/// A window of VS Code or an editor built from it (Insiders, VSCodium, Cursor, ...).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VsCodeWindow {
    /// The editor's executable at capture time (`Code.exe`, ...).
    pub exe: PathBuf,
    /// The product's display name ("Visual Studio Code"), for the UI.
    pub product: String,
    /// Set only when the editor was started with `--user-data-dir`; passed back on restore.
    pub user_data_dir: Option<PathBuf>,
    /// The open folder's URI as VS Code reports it (`file:///c%3A/...`, `vscode-remote://...`).
    pub folder: Option<String>,
    /// The open workspace file's URI. Neither this nor `folder` set means an empty window.
    pub workspace: Option<String>,
    pub title: String,
    pub geometry: Geometry,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProcessWindow {
    pub exe: PathBuf,
    /// Arguments that are safe to replay: existing files/folders only.
    pub args: Vec<String>,
    /// The full original command line, kept for reference but never replayed.
    pub cmdline: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub title: String,
    pub class: String,
    pub geometry: Geometry,
}
