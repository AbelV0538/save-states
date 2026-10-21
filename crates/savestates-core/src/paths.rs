//! Where everything lives on disk.
//!
//! `%APPDATA%\SaveStates` holds `states.json` (it roams with the user profile);
//! `%LOCALAPPDATA%\SaveStates` holds thumbnails, logs, and the Chrome bridge's live
//! snapshots. The bridge computes the live directory from `%LOCALAPPDATA%` itself,
//! so [`AppPaths::live_dir`] must stay in sync with `savestates-bridge`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const APP_DIR: &str = "SaveStates";

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub roaming: PathBuf,
    pub local: PathBuf,
}

impl AppPaths {
    /// The standard locations, or `$SAVESTATES_HOME\{roaming,local}` when that variable
    /// is set (tests and side-by-side experiments; the bridge honours it too).
    pub fn resolve() -> Result<Self> {
        if let Some(home) = std::env::var_os("SAVESTATES_HOME") {
            let home = PathBuf::from(home);
            return Ok(Self::at(home.join("roaming"), home.join("local")));
        }
        let base = directories::BaseDirs::new().context("could not resolve the user profile directories")?;
        Ok(Self::at(base.config_dir().join(APP_DIR), base.data_local_dir().join(APP_DIR)))
    }

    pub fn at(roaming: PathBuf, local: PathBuf) -> Self {
        Self { roaming, local }
    }

    pub fn ensure(&self) -> Result<()> {
        for dir in [self.roaming.clone(), self.thumbs_dir(), self.live_dir(), self.logs_dir(), self.bridge_dir()] {
            fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn states_file(&self) -> PathBuf {
        self.roaming.join("states.json")
    }

    pub fn thumbs_dir(&self) -> PathBuf {
        self.local.join("thumbs")
    }

    pub fn live_dir(&self) -> PathBuf {
        self.local.join("live")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.local.join("logs")
    }

    pub fn bridge_dir(&self) -> PathBuf {
        self.local.join("bridge")
    }

    /// Learned mapping of extension instance -> Chrome profile directory.
    pub fn chrome_instances_file(&self) -> PathBuf {
        self.local.join("chrome_instances.json")
    }
}

/// Write `bytes` to `path` so that readers only ever see the old or the new content:
/// write a sibling temp file, flush it to disk, then rename over the target.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{file_name}.{}.tmp", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}
