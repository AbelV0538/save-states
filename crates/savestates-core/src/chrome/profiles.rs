//! Chrome profiles: discovery, display names, and mapping a window to its profile.
//!
//! Chrome stamps every browser window with a per-profile AppUserModelID, built by
//! `GetProfileIdFromPath` in Chromium's `shell_integration_win.cc`: the Default
//! profile gets the bare browser id (`Chrome`); any other profile gets
//! `Chrome.<user-data-dir name>.<profile dir name>` with everything but ASCII
//! letters, digits and dots stripped - `Profile 2` becomes `Chrome.UserData.Profile2`.
//! Installed web apps add a `_crx_<app id>` component. That gives an exact
//! window -> profile mapping straight from the HWND, with no extension involved.

use std::fs;
use std::io::ErrorKind;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromeProfile {
    /// Directory name inside `User Data`: "Default", "Profile 2", ...
    pub dir: String,
    /// Display name from `Local State` ("Abel", "school", ...).
    pub name: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChromeEnv {
    pub user_data_dir: PathBuf,
    pub profiles: Vec<ChromeProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromeAumid {
    pub profile_dir: String,
    pub app_id: Option<String>,
}

impl ChromeEnv {
    pub fn load() -> Self {
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default();
        Self::load_from(base.join("Google").join("Chrome").join("User Data"))
    }

    pub fn load_from(user_data_dir: PathBuf) -> Self {
        let mut profiles = fs::read(user_data_dir.join("Local State"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .map(|v| parse_info_cache(&v))
            .unwrap_or_default();
        // Profiles missing from Local State still exist on disk and still get windows.
        if let Ok(entries) = fs::read_dir(&user_data_dir) {
            for e in entries.flatten() {
                let dir = e.file_name().to_string_lossy().into_owned();
                let looks_like_profile = dir == "Default" || dir.starts_with("Profile ");
                if looks_like_profile && e.path().join("Preferences").exists() && !profiles.iter().any(|p| p.dir == dir)
                {
                    profiles.push(ChromeProfile { name: dir.clone(), dir, email: None });
                }
            }
        }
        profiles.sort_by_key(|a| profile_order(&a.dir));
        Self { user_data_dir, profiles }
    }

    /// Display label, disambiguated when two profiles share a name (common: the same
    /// person signed into two profiles).
    pub fn label(&self, dir: &str) -> String {
        let Some(p) = self.profiles.iter().find(|p| p.dir == dir) else {
            return dir.to_string();
        };
        let dup = self.profiles.iter().filter(|q| q.name == p.name).count() > 1;
        if dup { format!("{} ({})", p.name, p.dir) } else { p.name.clone() }
    }

    /// Map a window's AppUserModelID to its profile, or `None` if it isn't a Chrome window.
    pub fn parse_window_aumid(&self, aumid: &str) -> Option<ChromeAumid> {
        if aumid != "Chrome" && !aumid.starts_with("Chrome.") {
            return None;
        }
        let app_id = aumid.split('.').find_map(|c| c.strip_prefix("_crx_")).map(str::to_string);
        let udd_name = self.user_data_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let profile_dir = self
            .profiles
            .iter()
            .filter(|p| p.dir != "Default")
            .find(|p| aumid.ends_with(&format!(".{}", sanitize_profile_id(&udd_name, &p.dir))))
            .map(|p| p.dir.clone())
            .unwrap_or_else(|| "Default".to_string());
        Some(ChromeAumid { profile_dir, app_id })
    }

    /// True while Chrome has this profile open: Chrome holds the profile's current
    /// session file with an exclusive share mode, so opening it fails with a
    /// sharing violation for as long as the profile is running.
    pub fn is_profile_live(&self, dir: &str) -> bool {
        let Some(newest) = newest_session_file(&self.user_data_dir.join(dir).join("Sessions")) else {
            return false;
        };
        const FILE_SHARE_ALL: u32 = 0x7;
        const ERROR_SHARING_VIOLATION: i32 = 32;
        match fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_ALL).open(newest) {
            Ok(_) => false,
            Err(e) => e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) || e.kind() == ErrorKind::PermissionDenied,
        }
    }
}

/// Chromium's sanitization: keep ASCII letters, digits and dots of `"<udd>.<profile>"`.
pub fn sanitize_profile_id(user_data_dir_name: &str, profile_dir: &str) -> String {
    format!("{user_data_dir_name}.{profile_dir}").chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.').collect()
}

pub fn find_chrome_exe() -> Option<PathBuf> {
    let rel = Path::new("Google").join("Chrome").join("Application").join("chrome.exe");
    ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|base| PathBuf::from(base).join(&rel))
        .find(|p| p.exists())
}

fn parse_info_cache(local_state: &serde_json::Value) -> Vec<ChromeProfile> {
    let Some(cache) = local_state.pointer("/profile/info_cache").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    cache
        .iter()
        .map(|(dir, info)| {
            let s = |k: &str| info.get(k).and_then(|v| v.as_str()).map(str::to_string).filter(|s| !s.is_empty());
            ChromeProfile { dir: dir.clone(), name: s("name").unwrap_or_else(|| dir.clone()), email: s("user_name") }
        })
        .collect()
}

fn newest_session_file(sessions_dir: &Path) -> Option<PathBuf> {
    fs::read_dir(sessions_dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("Session_"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

/// "Default" first, then "Profile N" in numeric order.
fn profile_order(dir: &str) -> (u32, String) {
    if dir == "Default" {
        return (0, String::new());
    }
    let n = dir.strip_prefix("Profile ").and_then(|n| n.parse::<u32>().ok()).unwrap_or(u32::MAX - 1);
    (n.saturating_add(1), dir.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> ChromeEnv {
        let p = |dir: &str, name: &str| ChromeProfile { dir: dir.into(), name: name.into(), email: None };
        ChromeEnv {
            user_data_dir: PathBuf::from(r"C:\Users\x\AppData\Local\Google\Chrome\User Data"),
            profiles: vec![
                p("Default", "Abel"),
                p("Profile 1", "Abel"),
                p("Profile 2", "school"),
                p("Profile 3", "guest"),
            ],
        }
    }

    #[test]
    fn sanitization_matches_chromium() {
        assert_eq!(sanitize_profile_id("User Data", "Profile 2"), "UserData.Profile2");
        assert_eq!(sanitize_profile_id("User Data", "Profile 12"), "UserData.Profile12");
    }

    #[test]
    fn aumids_observed_on_a_real_machine() {
        let env = env();
        // Both of these were read off live Chrome 155 windows.
        assert_eq!(env.parse_window_aumid("Chrome").unwrap().profile_dir, "Default");
        assert_eq!(env.parse_window_aumid("Chrome.UserData.Profile2").unwrap().profile_dir, "Profile 2");
        assert!(env.parse_window_aumid("RealtekSemiconductorCorp.RealtekAudioControl_dt26b99r8h8gj!App").is_none());
        assert!(env.parse_window_aumid("ChromeSomethingElse").is_none());
    }

    #[test]
    fn profile_suffix_does_not_collide_with_longer_numbers() {
        let mut env = env();
        env.profiles.push(ChromeProfile { dir: "Profile 12".into(), name: "x".into(), email: None });
        assert_eq!(env.parse_window_aumid("Chrome.UserData.Profile12").unwrap().profile_dir, "Profile 12");
        assert_eq!(env.parse_window_aumid("Chrome.UserData.Profile1").unwrap().profile_dir, "Profile 1");
    }

    #[test]
    fn web_app_windows_carry_their_app_id() {
        let a = env().parse_window_aumid("Chrome._crx_agimnkijcaahngcdmfeangaknmldooml.UserData.Profile2").unwrap();
        assert_eq!(a.profile_dir, "Profile 2");
        assert_eq!(a.app_id.as_deref(), Some("agimnkijcaahngcdmfeangaknmldooml"));
    }

    #[test]
    fn duplicate_names_are_disambiguated() {
        let env = env();
        assert_eq!(env.label("Default"), "Abel (Default)");
        assert_eq!(env.label("Profile 1"), "Abel (Profile 1)");
        assert_eq!(env.label("Profile 2"), "school");
    }

    #[test]
    fn parses_local_state() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"profile":{"info_cache":{"Default":{"name":"Abel","user_name":"a@x.com"},"Profile 2":{"name":"school"}}}}"#,
        )
        .unwrap();
        let mut p = parse_info_cache(&v);
        p.sort_by(|a, b| a.dir.cmp(&b.dir));
        assert_eq!(p[0], ChromeProfile { dir: "Default".into(), name: "Abel".into(), email: Some("a@x.com".into()) });
        assert_eq!(p[1].name, "school");
    }
}
