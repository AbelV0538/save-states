//! File Explorer windows and the folders they show, via `IShellWindows`.
//!
//! On Windows 11 every Explorer *tab* is its own shell-window entry sharing the
//! top-level `CabinetWClass` HWND, so one window maps to a list of folders.

use std::collections::HashMap;

use windows::Win32::System::Com::{CLSCTX_LOCAL_SERVER, CoCreateInstance, IServiceProvider};
use windows::Win32::System::Variant::{VARIANT, VT_I4};
use windows::Win32::UI::Shell::{
    IFolderView, IPersistFolder2, IShellBrowser, IShellWindows, IWebBrowserApp, SHGetNameFromIDList,
    SID_STopLevelBrowser, SIGDN_DESKTOPABSOLUTEPARSING, ShellWindows,
};
use windows::core::Interface;

use super::take_co_string;

/// Folders per Explorer top-level HWND. Needs COM on the calling thread.
pub fn open_folders() -> HashMap<isize, Vec<String>> {
    let mut map: HashMap<isize, Vec<String>> = HashMap::new();
    let Ok(shell) = (unsafe { CoCreateInstance::<_, IShellWindows>(&ShellWindows, None, CLSCTX_LOCAL_SERVER) }) else {
        return map;
    };
    let count = unsafe { shell.Count() }.unwrap_or(0);
    for i in 0..count {
        let Some(browser) = item(&shell, i) else { continue };
        let Ok(hwnd) = (unsafe { browser.HWND() }) else { continue };
        if let Some(location) = parsing_name(&browser).or_else(|| location_url_path(&browser)) {
            let folders = map.entry(hwnd.0).or_default();
            if !folders.iter().any(|f| f.eq_ignore_ascii_case(&location)) {
                folders.push(location);
            }
        }
    }
    map
}

fn item(shell: &IShellWindows, index: i32) -> Option<IWebBrowserApp> {
    let mut v = VARIANT::default();
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_I4;
        inner.Anonymous.lVal = index;
        shell.Item(&v).ok()?.cast::<IWebBrowserApp>().ok()
    }
}

/// The current folder's absolute parsing name: a path like `C:\Users\me`, or a shell
/// namespace location like `::{20D04FE0-3AEA-1069-A2D8-08002B30309D}` (This PC).
fn parsing_name(browser: &IWebBrowserApp) -> Option<String> {
    unsafe {
        let sp: IServiceProvider = browser.cast().ok()?;
        let shell_browser: IShellBrowser = sp.QueryService(&SID_STopLevelBrowser).ok()?;
        let view = shell_browser.QueryActiveShellView().ok()?;
        let folder_view: IFolderView = view.cast().ok()?;
        let folder: IPersistFolder2 = folder_view.GetFolder().ok()?;
        let pidl = folder.GetCurFolder().ok()?;
        let name = SHGetNameFromIDList(pidl, SIGDN_DESKTOPABSOLUTEPARSING).ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(pidl as *const _));
        name.and_then(|p| take_co_string(p)).filter(|s| !s.is_empty())
    }
}

fn location_url_path(browser: &IWebBrowserApp) -> Option<String> {
    let url = unsafe { browser.LocationURL() }.ok()?.to_string();
    file_url_to_path(&url)
}

/// `file:///C:/My%20Docs` -> `C:\My Docs`; `file://server/share` -> `\\server\share`.
pub fn file_url_to_path(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("file:///")
        .map(str::to_string)
        .or_else(|| url.strip_prefix("file://").map(|unc| format!("//{unc}")))?;
    let decoded = percent_decode(&rest);
    (!decoded.is_empty()).then(|| decoded.replace('/', "\\"))
}

pub(crate) fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        (b as char).to_digit(16).map(|d| d as u8)
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_urls() {
        assert_eq!(file_url_to_path("file:///C:/Users/me/My%20Docs").as_deref(), Some(r"C:\Users\me\My Docs"));
        assert_eq!(file_url_to_path("file://server/share/x").as_deref(), Some(r"\\server\share\x"));
        assert_eq!(file_url_to_path("file:///C:/100%25").as_deref(), Some(r"C:\100%"));
        assert_eq!(file_url_to_path("https://example.com"), None);
    }
}
