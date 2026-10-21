//! The state store: `states.json`, loaded once and rewritten atomically on every change.
//!
//! Shared between the UI thread and the worker as [`SharedStore`]; every mutation
//! saves immediately, so correctness never depends on the UI loop running.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::Utc;
use uuid::Uuid;

use crate::model::{STORE_VERSION, SaveState, Settings, StoreFile};
use crate::paths::write_atomic;

pub type SharedStore = Arc<Mutex<Store>>;

pub struct Store {
    path: PathBuf,
    file: StoreFile,
    generation: u64,
}

impl Store {
    /// Load the store. A missing file is an empty store. A file that fails to parse is
    /// moved aside (never overwritten) and reported through the returned warning.
    pub fn open(path: PathBuf) -> (Self, Option<String>) {
        let (file, warning) = match fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (StoreFile::default(), None),
            Err(e) => (StoreFile::default(), Some(format!("Could not read {}: {e}", path.display()))),
            Ok(bytes) => match serde_json::from_slice::<StoreFile>(&bytes) {
                Ok(file) => (file, None),
                Err(e) => {
                    let backup = quarantine(&path);
                    let where_ = backup.map(|b| format!(" It was moved to {}.", b.display())).unwrap_or_default();
                    (StoreFile::default(), Some(format!("states.json could not be parsed ({e}).{where_}")))
                }
            },
        };
        let warning = warning.or_else(|| {
            (file.version > STORE_VERSION).then(|| {
                format!("states.json was written by a newer version (v{}); unknown data is preserved.", file.version)
            })
        });
        (Self { path, file, generation: 0 }, warning)
    }

    pub fn into_shared(self) -> SharedStore {
        Arc::new(Mutex::new(self))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bumped on every change, so the UI can cheaply tell when to refresh.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn states(&self) -> &[SaveState] {
        &self.file.states
    }

    pub fn get(&self, id: Uuid) -> Option<&SaveState> {
        self.file.states.iter().find(|s| s.id == id)
    }

    pub fn settings(&self) -> &Settings {
        &self.file.settings
    }

    pub fn insert(&mut self, state: SaveState) -> Result<()> {
        self.file.states.push(state);
        self.commit()
    }

    /// Replace an existing state wholesale (e.g. "update with current session").
    pub fn replace(&mut self, state: SaveState) -> Result<bool> {
        match self.file.states.iter_mut().find(|s| s.id == state.id) {
            Some(slot) => {
                *slot = state;
                self.commit().map(|_| true)
            }
            None => Ok(false),
        }
    }

    pub fn update(&mut self, id: Uuid, f: impl FnOnce(&mut SaveState)) -> Result<bool> {
        match self.file.states.iter_mut().find(|s| s.id == id) {
            Some(state) => {
                f(state);
                state.updated_at = Utc::now();
                self.commit().map(|_| true)
            }
            None => Ok(false),
        }
    }

    pub fn remove(&mut self, id: Uuid) -> Result<Option<SaveState>> {
        let Some(pos) = self.file.states.iter().position(|s| s.id == id) else {
            return Ok(None);
        };
        let removed = self.file.states.remove(pos);
        self.commit().map(|_| Some(removed))
    }

    pub fn update_settings(&mut self, f: impl FnOnce(&mut Settings)) -> Result<()> {
        f(&mut self.file.settings);
        self.commit()
    }

    fn commit(&mut self) -> Result<()> {
        self.generation += 1;
        // Never downgrade the on-disk version marker written by a newer build.
        self.file.version = self.file.version.max(STORE_VERSION);
        let json = serde_json::to_vec_pretty(&self.file).context("serializing states")?;
        write_atomic(&self.path, &json).with_context(|| format!("writing {}", self.path.display()))
    }
}

fn quarantine(path: &Path) -> Option<PathBuf> {
    let stamp = Utc::now().format("%Y%m%d-%H%M%S");
    let backup = path.with_extension(format!("json.corrupt-{stamp}"));
    fs::rename(path, &backup).ok().map(|_| backup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CapturedItem, ProcessWindow, StoredItem, Thumbnail};

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("savestates-test-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join("states.json")
    }

    fn sample_state(name: &str) -> SaveState {
        SaveState {
            id: Uuid::new_v4(),
            name: name.into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            favorite: false,
            thumbnail: Thumbnail::default(),
            scope: Default::default(),
            desktops: vec![],
            items: vec![StoredItem::Known(CapturedItem::Process(ProcessWindow {
                exe: r"C:\Windows\notepad.exe".into(),
                title: "notes.txt - Notepad".into(),
                ..Default::default()
            }))],
            extra: Default::default(),
        }
    }

    #[test]
    fn round_trip_and_mutations() {
        let path = temp_path("roundtrip");
        let (mut store, warning) = Store::open(path.clone());
        assert!(warning.is_none());
        let state = sample_state("Work");
        let id = state.id;
        store.insert(state).unwrap();
        store.update(id, |s| s.favorite = true).unwrap();

        let (reloaded, warning) = Store::open(path.clone());
        assert!(warning.is_none());
        let s = reloaded.get(id).unwrap();
        assert!(s.favorite);
        assert_eq!(s.known_items().count(), 1);

        let mut reloaded = reloaded;
        assert!(reloaded.remove(id).unwrap().is_some());
        assert!(Store::open(path).0.states().is_empty());
    }

    #[test]
    fn unknown_kinds_and_fields_survive_a_round_trip() {
        let path = temp_path("forward");
        let json = r#"{
          "version": 7,
          "settings": {},
          "states": [{
            "id": "5f0c8a3e-1c1e-4b7a-9f00-000000000001",
            "name": "From the future",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "future_field": {"x": 1},
            "items": [
              {"kind": "process", "exe": "C:\\Windows\\notepad.exe"},
              {"kind": "hologram", "depth": 3}
            ]
          }]
        }"#;
        fs::write(&path, json).unwrap();
        let (mut store, warning) = Store::open(path.clone());
        assert!(warning.unwrap().contains("newer version"));
        let state = &store.states()[0];
        assert_eq!(state.known_items().count(), 1);
        assert_eq!(state.items.len(), 2);
        let id = state.id;
        store.update(id, |s| s.name = "renamed".into()).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("hologram"), "unknown item kind was dropped");
        assert!(text.contains("future_field"), "unknown field was dropped");
        assert!(text.contains("\"version\": 7"), "version marker was downgraded");
    }

    #[test]
    fn corrupt_file_is_quarantined_not_overwritten() {
        let path = temp_path("corrupt");
        fs::write(&path, b"{ this is not json").unwrap();
        let (store, warning) = Store::open(path.clone());
        assert!(store.states().is_empty());
        assert!(warning.unwrap().contains("moved to"));
        assert!(!path.exists(), "corrupt file should have been moved aside");
    }
}
