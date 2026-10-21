//! Top-level window enumeration with alt-tab-style filtering.
//!
//! Filter rules, each for a reason seen on a real desktop:
//! - invisible, tool, and owned (dialog/popup) windows are not "apps";
//! - empty titles are shell plumbing (`Program Manager`, tray windows, ...);
//! - `Windows.UI.Core.CoreWindow` at top level is a UWP app's content window that the
//!   shell detached from its `ApplicationFrameWindow` (it happens for apps on other
//!   desktops); the frame is what we capture, so the CoreWindow would be a duplicate;
//! - DWM cloaking means either "on another virtual desktop" (keep) or "hidden by the
//!   shell while claiming the current desktop" (a ghost: drop). Most windows on a
//!   multi-desktop setup are cloaked, so cloaking alone must never exclude a window.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;

use windows::Win32::Foundation::{HWND, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::Shell::{IVirtualDesktopManager, VirtualDesktopManager};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetWindow, GetWindowLongPtrW, GetWindowPlacement, GetWindowRect,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed, WINDOWPLACEMENT,
    WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};
use windows::core::BOOL;

use super::{hwnd, process, take_co_string};
use crate::model::{Rect, ShowState};

const EXCLUDED_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Windows.UI.Core.CoreWindow",
    "ApplicationManager_ImmersiveShellWindow",
    "Windows.Internal.Shell.TabProxyWindow",
];

#[derive(Debug, Clone)]
pub struct LiveWindow {
    pub hwnd: isize,
    pub pid: u32,
    pub title: String,
    pub class: String,
    pub exe: Option<PathBuf>,
    /// The window's own AppUserModelID (property store). Chrome sets a per-profile
    /// one on every browser window; UWP frames carry the hosted app's.
    pub aumid: Option<String>,
    pub cloaked: bool,
    pub on_current_desktop: bool,
    pub show: ShowState,
    /// `rcNormalPosition`: the restored-size rectangle, valid even when maximized/minimized.
    pub normal_rect: Rect,
    /// Current outer rectangle (`GetWindowRect`).
    pub rect: Rect,
    /// Current visible frame (DWM extended frame bounds), without invisible resize borders.
    pub frame: Rect,
    pub dpi: u32,
}

impl LiveWindow {
    pub fn exe_name(&self) -> String {
        self.exe.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    pub fn exe_is(&self, name: &str) -> bool {
        self.exe_name().eq_ignore_ascii_case(name)
    }
}

/// All app windows on all virtual desktops, excluding windows of `exclude_pid`.
///
/// Call from a thread with COM initialized; without COM the virtual desktop check is
/// skipped and every window is treated as being on the current desktop.
pub fn app_windows(exclude_pid: u32) -> Vec<LiveWindow> {
    let vdm = desktop_manager();
    let mut exe_cache: HashMap<u32, Option<PathBuf>> = HashMap::new();
    let mut out = Vec::new();

    for h in raw_top_level_windows() {
        let Ok(Candidate { title, class, pid, cloaked, on_current_desktop }) = assess(h, exclude_pid, vdm.as_ref())
        else {
            continue;
        };
        let exe = exe_cache.entry(pid).or_insert_with(|| process::exe_path(pid)).clone();
        let (normal_rect, show) = placement(h);
        out.push(LiveWindow {
            hwnd: h.0 as isize,
            pid,
            title,
            class,
            exe,
            aumid: window_aumid(h),
            cloaked,
            on_current_desktop,
            show,
            normal_rect,
            rect: window_rect(h),
            frame: frame_rect(h).unwrap_or_else(|| window_rect(h)),
            dpi: unsafe { GetDpiForWindow(h) }.max(96),
        });
    }
    out
}

/// Every top-level window handle, unfiltered.
pub fn raw_top_level_windows() -> Vec<HWND> {
    unsafe extern "system" fn collect(h: HWND, lparam: LPARAM) -> BOOL {
        let list = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
        list.push(h);
        TRUE
    }
    let mut list: Vec<HWND> = Vec::with_capacity(256);
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut list as *mut Vec<HWND> as isize));
    }
    list
}

struct Candidate {
    title: String,
    class: String,
    pid: u32,
    cloaked: bool,
    on_current_desktop: bool,
}

fn desktop_manager() -> Option<IVirtualDesktopManager> {
    unsafe { CoCreateInstance(&VirtualDesktopManager, None, CLSCTX_ALL) }.ok()
}

/// Decide whether `h` is an app window a user would alt-tab to; `Err` says why not.
fn assess(h: HWND, exclude_pid: u32, vdm: Option<&IVirtualDesktopManager>) -> Result<Candidate, &'static str> {
    unsafe {
        if !IsWindowVisible(h).as_bool() {
            return Err("invisible");
        }
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return Err("tool window");
        }
        let owned = GetWindow(h, GW_OWNER).is_ok_and(|o| !o.is_invalid());
        if owned && ex & WS_EX_APPWINDOW.0 == 0 {
            return Err("owned popup/dialog");
        }
    }
    let title = window_text(h);
    if title.trim().is_empty() {
        return Err("untitled");
    }
    let class = class_name(h);
    if EXCLUDED_CLASSES.contains(&class.as_str()) {
        return Err("shell class");
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    if pid == 0 || pid == exclude_pid {
        return Err("own process");
    }
    let cloaked = is_cloaked(h);
    let on_current_desktop =
        vdm.and_then(|m| unsafe { m.IsWindowOnCurrentVirtualDesktop(h) }.ok()).map(|b| b.as_bool()).unwrap_or(true);
    if cloaked && on_current_desktop {
        return Err("cloaked on current desktop (ghost)");
    }
    Ok(Candidate { title, class, pid, cloaked, on_current_desktop })
}

/// Every visible top-level window with the filter's verdict, for diagnostics.
pub fn diagnose(exclude_pid: u32) -> Vec<(String, String, String, &'static str)> {
    let vdm = desktop_manager();
    raw_top_level_windows()
        .into_iter()
        .filter(|&h| unsafe { IsWindowVisible(h).as_bool() })
        .map(|h| {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
            let exe = process::exe_path(pid)
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default();
            let verdict = match assess(h, exclude_pid, vdm.as_ref()) {
                Ok(c) if c.cloaked => "kept (on another desktop)",
                Ok(_) => "kept",
                Err(why) => why,
            };
            (window_text(h), class_name(h), exe, verdict)
        })
        .collect()
}

pub fn window_text(h: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn is_cloaked(h: HWND) -> bool {
    let mut cloaked: u32 = 0;
    let ok = unsafe {
        DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut c_void, size_of::<u32>() as u32)
    };
    ok.is_ok() && cloaked != 0
}

fn placement(h: HWND) -> (Rect, ShowState) {
    let mut wp = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    let normal = match unsafe { GetWindowPlacement(h, &mut wp) } {
        Ok(()) => rect_from(wp.rcNormalPosition),
        Err(_) => window_rect(h),
    };
    let show = unsafe {
        if IsIconic(h).as_bool() {
            ShowState::Minimized
        } else if IsZoomed(h).as_bool() {
            ShowState::Maximized
        } else {
            ShowState::Normal
        }
    };
    (normal, show)
}

fn window_rect(h: HWND) -> Rect {
    let mut r = RECT::default();
    let _ = unsafe { GetWindowRect(h, &mut r) };
    rect_from(r)
}

fn frame_rect(h: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            h,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut c_void,
            size_of::<RECT>() as u32,
        )
    }
    .ok()
    .map(|_| rect_from(r))
}

pub(crate) fn rect_from(r: RECT) -> Rect {
    Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom }
}

/// The AppUserModelID set on the window itself, if any.
pub fn window_aumid(h: HWND) -> Option<String> {
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(h).ok()?;
        let mut value = store.GetValue(&PKEY_AppUserModel_ID).ok()?;
        let s = PropVariantToStringAlloc(&value).ok().and_then(|p| take_co_string(p));
        let _ = PropVariantClear(&mut value);
        s.filter(|s| !s.is_empty())
    }
}

pub fn is_alive(raw: isize) -> bool {
    let h = hwnd(raw);
    unsafe { IsWindow(Some(h)).as_bool() && IsWindowVisible(h).as_bool() }
}
