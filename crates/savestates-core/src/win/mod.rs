//! Thin, safe-ish wrappers over the Win32 and shell COM APIs the app needs.

pub mod enumerate;
pub mod explorer;
pub mod launch;
pub mod process;

use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, GetCurrentThreadId, SetProcessWorkingSetSize,
};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PostThreadMessageW, TranslateMessage, WM_APP,
};
use windows::core::{PCWSTR, PWSTR};

/// NUL-terminated UTF-16 for passing to `W` APIs.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

pub fn pcwstr(buf: &[u16]) -> PCWSTR {
    PCWSTR(buf.as_ptr())
}

/// Read a `CoTaskMemAlloc`ed string and free it.
pub(crate) unsafe fn take_co_string(p: PWSTR) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { p.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(p.0 as *const c_void)) };
    s
}

pub(crate) fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

/// Initializes COM (single-threaded apartment) for the current thread for its lifetime.
pub struct ComApartment {
    initialized: bool,
}

impl ComApartment {
    pub fn init() -> Self {
        // S_FALSE ("already initialized") still needs a balancing CoUninitialize;
        // RPC_E_CHANGED_MODE means someone else owns the apartment, so we must not.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        Self { initialized: hr.is_ok() }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}

/// Window geometry only round-trips if we see physical pixels, so opt in to
/// per-monitor DPI awareness before any window is touched. Harmless if already set.
pub fn enable_per_monitor_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Thread message that ends [`pump_until_woken`].
const WM_APP_WAKE: u32 = WM_APP + 0x51;

pub fn current_thread_id() -> u32 {
    unsafe { GetCurrentThreadId() }
}

/// Wake a thread that is sitting in [`pump_until_woken`]. Safe to call from any thread.
pub fn post_wake(thread_id: u32) {
    unsafe {
        let _ = PostThreadMessageW(thread_id, WM_APP_WAKE, WPARAM(0), LPARAM(0));
    }
}

/// Run this thread's message loop until [`post_wake`] is called for it.
///
/// The thread keeps dispatching messages for its windows (the tray icon, and the event
/// loop's hidden helper window) the whole time, so broadcasts and shutdown queries are
/// answered promptly instead of timing out on an unresponsive window.
pub fn pump_until_woken() {
    let mut msg = MSG::default();
    loop {
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 <= 0 {
            return; // WM_QUIT or error
        }
        if msg.hwnd.is_invalid() && msg.message == WM_APP_WAKE {
            return;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Hand the pages we no longer touch back to the OS, e.g. after the window closes.
pub fn trim_working_set() {
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

/// Holds a named mutex for the life of the process; `None` if another instance has it.
pub struct SingleInstance(HANDLE);

impl SingleInstance {
    pub fn acquire(name: &str) -> Option<Self> {
        let name = wide(name);
        let handle = unsafe { CreateMutexW(None, true, pcwstr(&name)) }.ok()?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return None;
        }
        Some(Self(handle))
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

// The mutex handle is only closed on drop; moving it between threads is fine.
unsafe impl Send for SingleInstance {}
