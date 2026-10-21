//! One-time Chrome setup: registering the native-messaging host and locating the
//! extension folder the user loads with "Load unpacked".

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW,
    RegGetValueW, RegSetValueExW,
};
use windows::core::PCWSTR;

use crate::paths::{AppPaths, write_atomic};
use crate::win::{pcwstr, wide};

pub const HOST_NAME: &str = "com.savestates.bridge";
/// Derived from the public `key` in `extension/manifest.json`, so it is the same on
/// every machine and for every profile the extension is loaded into.
pub const EXTENSION_ID: &str = "jccabdhhkjoagelckkicmglgpfglcofc";
const HOST_KEY: &str = r"Software\Google\Chrome\NativeMessagingHosts\com.savestates.bridge";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostStatus {
    /// Registered and pointing at a bridge executable that exists.
    Ready {
        manifest: PathBuf,
    },
    /// Registered, but the manifest or the bridge it names is missing (e.g. the app moved).
    Broken {
        manifest: PathBuf,
    },
    NotRegistered,
}

/// `savestates-bridge.exe`, expected next to the running app.
pub fn bridge_exe() -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?.with_file_name("savestates-bridge.exe");
    p.exists().then_some(p)
}

/// The unpacked extension: next to the exe in an install, or in the repo during development.
pub fn extension_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors().skip(1).take(4).map(|dir| dir.join("extension")).find(|d| d.join("manifest.json").exists())
}

pub fn host_status() -> HostStatus {
    let Some(manifest) = read_registered_manifest() else { return HostStatus::NotRegistered };
    let bridge_ok = std::fs::read(&manifest)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["path"].as_str().map(PathBuf::from))
        .is_some_and(|p| p.exists());
    if bridge_ok { HostStatus::Ready { manifest } } else { HostStatus::Broken { manifest } }
}

/// Write the host manifest and point Chrome at it (per-user, no admin needed).
pub fn register_host(paths: &AppPaths, bridge: &Path) -> Result<PathBuf> {
    let manifest_path = paths.bridge_dir().join(format!("{HOST_NAME}.json"));
    let manifest = serde_json::json!({
        "name": HOST_NAME,
        "description": "Save States - reports open Chrome tabs to the Save States app",
        "path": bridge,
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{EXTENSION_ID}/")],
    });
    write_atomic(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)
        .with_context(|| format!("writing {}", manifest_path.display()))?;
    set_default_value(HOST_KEY, &manifest_path.to_string_lossy())?;
    Ok(manifest_path)
}

fn set_default_value(subkey: &str, value: &str) -> Result<()> {
    let subkey_w = wide(subkey);
    let mut key = HKEY::default();
    unsafe {
        let err = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            pcwstr(&subkey_w),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        );
        if err != ERROR_SUCCESS {
            bail!("creating HKCU\\{subkey} failed ({})", err.0);
        }
        let data: Vec<u8> = wide(value).iter().flat_map(|u| u.to_le_bytes()).collect();
        let err = RegSetValueExW(key, PCWSTR::null(), None, REG_SZ, Some(&data));
        let _ = RegCloseKey(key);
        if err != ERROR_SUCCESS {
            bail!("writing HKCU\\{subkey} failed ({})", err.0);
        }
    }
    Ok(())
}

fn read_registered_manifest() -> Option<PathBuf> {
    let subkey_w = wide(HOST_KEY);
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            pcwstr(&subkey_w),
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    if err != ERROR_SUCCESS {
        return None;
    }
    let chars = (size as usize / 2).saturating_sub(1);
    let s = String::from_utf16_lossy(&buf[..chars.min(buf.len())]);
    let p = PathBuf::from(s.trim_end_matches('\0'));
    p.exists().then_some(p)
}
