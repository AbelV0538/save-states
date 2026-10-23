//! Starting apps and arranging their windows.

use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_ELEVATION_REQUIRED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, LPARAM,
    WPARAM,
};
use windows::Win32::System::Com::{CLSCTX_LOCAL_SERVER, CoCreateInstance};
use windows::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, PROCESS_CREATION_FLAGS,
    PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::Shell::{AO_NONE, ApplicationActivationManager, IApplicationActivationManager, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowPlacement, PostMessageW, SW_SHOWMAXIMIZED, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, SW_SHOWNORMAL,
    SetWindowPlacement, WINDOWPLACEMENT, WM_CLOSE,
};
use windows::core::{PCWSTR, PWSTR, w};

use super::{hwnd, pcwstr, wide};
use crate::model::{Geometry, ShowState};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    #[error("needs administrator rights (skipped rather than prompting)")]
    NeedsElevation,
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Other(String),
}

impl From<windows::core::Error> for LaunchError {
    fn from(e: windows::core::Error) -> Self {
        let code = e.code();
        if code == ERROR_ELEVATION_REQUIRED.to_hresult() {
            LaunchError::NeedsElevation
        } else if code == ERROR_FILE_NOT_FOUND.to_hresult() || code == ERROR_PATH_NOT_FOUND.to_hresult() {
            LaunchError::NotFound
        } else {
            LaunchError::Other(e.message())
        }
    }
}

/// Start `exe` directly with `CreateProcessW`, returning its PID.
///
/// Deliberately not `ShellExecute`: an exe whose manifest demands elevation fails with
/// `ERROR_ELEVATION_REQUIRED` here instead of throwing a UAC prompt mid-restore.
pub fn spawn(exe: &Path, args: &[String], cwd: Option<&Path>) -> Result<u32, LaunchError> {
    if !exe.exists() {
        return Err(LaunchError::NotFound);
    }
    let mut cmdline = quote_arg(&exe.to_string_lossy());
    for a in args {
        cmdline.push(' ');
        cmdline.push_str(&quote_arg(a));
    }
    let cwd = cwd.filter(|d| d.is_dir()).or_else(|| exe.parent());
    let exe_w = wide(exe);
    let cwd_w = cwd.map(wide);
    let env = launch_environment(std::env::vars_os());

    let base = CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT;
    // Break away from any job we were started in (terminals, IDEs), so restored apps
    // don't die with us; fall back if the job forbids breakaway.
    match create_process(&exe_w, &cmdline, cwd_w.as_deref(), &env, base | CREATE_BREAKAWAY_FROM_JOB) {
        Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => {
            create_process(&exe_w, &cmdline, cwd_w.as_deref(), &env, base).map_err(Into::into)
        }
        other => other.map_err(Into::into),
    }
}

/// Our environment as a `CreateProcess` block, minus what makes a relaunched app misbehave:
/// `ELECTRON_RUN_AS_NODE` turns any Electron app (VS Code included) into a windowless Node
/// process, and `VSCODE_*` variables leak in when Save States was started from VS Code.
/// The two VS Code reads that users set on purpose stay.
pub(crate) fn launch_environment(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<u16> {
    const KEEP: [&str; 2] = ["VSCODE_APPDATA", "VSCODE_EXTENSIONS"];
    let mut vars: Vec<(OsString, OsString)> = vars
        .into_iter()
        .filter(|(k, _)| {
            let k = k.to_string_lossy().to_ascii_uppercase();
            k != "ELECTRON_RUN_AS_NODE" && (!k.starts_with("VSCODE_") || KEEP.contains(&k.as_str()))
        })
        .collect();
    // The block must be sorted by name, ignoring case.
    vars.sort_by_cached_key(|(k, _)| k.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (k, v) in &vars {
        block.extend(k.encode_wide());
        block.push(u16::from(b'='));
        block.extend(v.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

fn create_process(
    exe: &[u16],
    cmdline: &str,
    cwd: Option<&[u16]>,
    env: &[u16],
    flags: PROCESS_CREATION_FLAGS,
) -> windows::core::Result<u32> {
    let mut cmd = wide(cmdline);
    let si = STARTUPINFOW { cb: size_of::<STARTUPINFOW>() as u32, ..Default::default() };
    let mut pi = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            pcwstr(exe),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            flags,
            Some(env.as_ptr() as *const c_void),
            cwd.map(pcwstr).unwrap_or(PCWSTR::null()),
            &si,
            &mut pi,
        )?;
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
    }
    Ok(pi.dwProcessId)
}

/// Launch a packaged app (Store/UWP or packaged Win32) by AppUserModelID.
pub fn activate_app(aumid: &str) -> Result<u32, LaunchError> {
    let primary = unsafe {
        CoCreateInstance::<_, IApplicationActivationManager>(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER)
            .and_then(|m| {
                let id = wide(aumid);
                m.ActivateApplication(pcwstr(&id), PCWSTR::null(), AO_NONE)
            })
    };
    match primary {
        Ok(pid) => Ok(pid),
        // Some apps refuse direct activation but still launch through the shell.
        Err(_) => shell_open("explorer.exe", Some(&format!("shell:AppsFolder\\{aumid}"))).map(|_| 0),
    }
}

/// `ShellExecute` "open" - used for folders and shell locations.
pub fn shell_open(file: &str, params: Option<&str>) -> Result<(), LaunchError> {
    let file_w = wide(file);
    let params_w = params.map(wide);
    let r = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            pcwstr(&file_w),
            params_w.as_deref().map(pcwstr).unwrap_or(PCWSTR::null()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute's HINSTANCE is really an error code; > 32 means success.
    let code = r.0 as isize;
    if code > 32 {
        Ok(())
    } else if code == 2 || code == 3 {
        Err(LaunchError::NotFound)
    } else {
        Err(LaunchError::Other(format!("ShellExecute failed ({code})")))
    }
}

/// Put a window where `geom` says, without stealing focus for normal/minimized windows.
pub fn apply_geometry(raw: isize, geom: &Geometry) -> bool {
    let Some(rect) = geom.normal_rect else { return false };
    let h = hwnd(raw);
    let mut wp = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    unsafe {
        let _ = GetWindowPlacement(h, &mut wp);
        wp.flags = Default::default();
        wp.rcNormalPosition.left = rect.left;
        wp.rcNormalPosition.top = rect.top;
        wp.rcNormalPosition.right = rect.right;
        wp.rcNormalPosition.bottom = rect.bottom;
        wp.showCmd = match geom.show {
            ShowState::Normal => SW_SHOWNOACTIVATE.0 as u32,
            ShowState::Maximized => SW_SHOWMAXIMIZED.0 as u32,
            ShowState::Minimized => SW_SHOWMINNOACTIVE.0 as u32,
        };
        // Windows moves a placement that would land entirely off-screen back into view,
        // which covers restoring onto a different monitor layout.
        SetWindowPlacement(h, &wp).is_ok()
    }
}

/// Ask a window to close the way its close button would, so apps can prompt to save.
pub fn request_close(raw: isize) -> bool {
    unsafe { PostMessageW(Some(hwnd(raw)), WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok() }
}

/// Quote one argument per the MSVCRT rules that `CommandLineToArgvW` and most apps use.
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_environment_drops_vscode_and_electron_variables() {
        let vars = [
            ("Path", r"C:\x"),
            ("ELECTRON_RUN_AS_NODE", "1"),
            ("VSCODE_PID", "4"),
            ("vscode_cwd", r"C:\"),
            ("APPDATA", r"C:\a"),
            ("VSCODE_APPDATA", r"D:\v"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let block = String::from_utf16(&launch_environment(vars)).unwrap();
        assert_eq!(block, "APPDATA=C:\\a\0Path=C:\\x\0VSCODE_APPDATA=D:\\v\0\0");
        assert_eq!(launch_environment([]), vec![0, 0]);
    }

    #[test]
    fn quoting_follows_msvcrt_rules() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg(r"C:\Program Files\x"), r#""C:\Program Files\x""#);
        assert_eq!(quote_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(""), r#""""#);
        assert_eq!(quote_arg("https://example.com/?q=a b&x=1"), r#""https://example.com/?q=a b&x=1""#);
    }
}
