//! File logging. Release builds have no console, so everything goes to
//! `%LOCALAPPDATA%\SaveStates\logs\app.log` (rotated once it passes 2 MB).
//! Set `SAVESTATES_LOG=debug` for more detail.

use std::fs::{self, OpenOptions};
use std::path::Path;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;

pub fn init(dir: &Path) {
    let path = dir.join("app.log");
    if fs::metadata(&path).is_ok_and(|m| m.len() > 2 * 1024 * 1024) {
        let _ = fs::rename(&path, dir.join("app.old.log"));
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else { return };
    let filter = EnvFilter::try_from_env("SAVESTATES_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_writer(Mutex::new(file)).with_ansi(false).with_env_filter(filter).try_init();

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default_hook(info);
    }));
}
