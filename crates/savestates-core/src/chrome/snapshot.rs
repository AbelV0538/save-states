//! Tab snapshots pushed by the companion extension (via the bridge), and the learned
//! mapping from extension instance to profile directory.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::paths::write_atomic;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExtSnapshot {
    /// Random id the extension generates once per profile install.
    pub instance: String,
    pub email: String,
    pub ext_version: String,
    /// Milliseconds since the Unix epoch, from the extension.
    pub ts: f64,
    pub windows: Vec<ExtWindow>,
    pub groups: Vec<ExtGroup>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExtWindow {
    pub id: i64,
    pub focused: bool,
    pub incognito: bool,
    /// "normal" | "minimized" | "maximized" | "fullscreen"
    pub state: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// Bounds in Chrome's DIP screen coordinates.
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub tabs: Vec<ExtTab>,
}

impl ExtWindow {
    pub fn active_title(&self) -> &str {
        self.tabs.iter().find(|t| t.active).map(|t| t.title.as_str()).unwrap_or("")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExtTab {
    pub id: i64,
    pub index: i32,
    pub url: String,
    pub title: String,
    pub pinned: bool,
    pub active: bool,
    /// -1 when the tab is not in a group.
    pub group_id: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExtGroup {
    pub id: i64,
    pub window_id: i64,
    pub title: String,
    pub color: String,
    pub collapsed: bool,
}

/// Snapshots the bridges cached on disk (`live\<instance>.json`).
pub fn load_cached(live_dir: &Path) -> Vec<ExtSnapshot> {
    let Ok(entries) = fs::read_dir(live_dir) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice::<ExtSnapshot>(&fs::read(e.path()).ok()?).ok())
        .filter(|s| !s.instance.is_empty())
        .collect()
}

/// Extension instance id -> Chrome profile directory, learned from window matches.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InstanceMap {
    pub profiles: HashMap<String, String>,
}

impl InstanceMap {
    pub fn load(path: &Path) -> Self {
        fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self).unwrap_or_default())
    }

    pub fn profile_of(&self, instance: &str) -> Option<&str> {
        self.profiles.get(instance).map(String::as_str)
    }

    pub fn instance_for(&self, profile_dir: &str) -> Vec<&str> {
        self.profiles.iter().filter(|(_, p)| p.as_str() == profile_dir).map(|(i, _)| i.as_str()).collect()
    }

    /// Returns true if this changed the map.
    pub fn learn(&mut self, instance: &str, profile_dir: &str) -> bool {
        if instance.is_empty() || self.profile_of(instance) == Some(profile_dir) {
            return false;
        }
        self.profiles.insert(instance.to_string(), profile_dir.to_string());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_extension_snapshot() {
        let json = r#"{"type":"snapshot","instance":"abc","ts":1.7e12,"windows":[
            {"id":7,"state":"maximized","type":"normal","left":-8,"top":-8,"width":1936,"height":1048,
             "tabs":[{"id":1,"index":0,"url":"https://a.com/","title":"A","pinned":true,"active":false,"groupId":-1},
                     {"id":2,"index":1,"url":"https://b.com/","title":"B","active":true,"groupId":4}]}],
            "groups":[{"id":4,"windowId":7,"title":"Work","color":"blue","collapsed":false}]}"#;
        let s: ExtSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(s.instance, "abc");
        assert_eq!(s.windows[0].active_title(), "B");
        assert_eq!(s.windows[0].tabs[1].group_id, 4);
        assert_eq!(s.groups[0].window_id, 7);
    }

    #[test]
    fn instance_map_learns_once() {
        let mut m = InstanceMap::default();
        assert!(m.learn("i1", "Profile 2"));
        assert!(!m.learn("i1", "Profile 2"));
        assert_eq!(m.instance_for("Profile 2"), vec!["i1"]);
    }
}
