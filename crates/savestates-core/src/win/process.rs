//! Process facts: image path, package identity, and command line.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS};
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::PWSTR;

/// Full image path of `pid`. Works for elevated processes too: the limited-information
/// right is granted across integrity levels (protected processes excepted).
pub fn exe_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }
}

/// The package AppUserModelID of a packaged process (Store apps, packaged Win32 apps
/// such as the new Notepad or Windows Terminal). `None` for ordinary processes.
pub fn package_aumid(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 512];
        let mut len = buf.len() as u32;
        let err = GetApplicationUserModelId(h, &mut len, Some(PWSTR(buf.as_mut_ptr())));
        let _ = CloseHandle(h);
        if err != ERROR_SUCCESS || len == 0 {
            return None;
        }
        // `len` includes the terminating NUL.
        let s = String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)]);
        (!s.is_empty()).then_some(s)
    }
}

/// Executables under `WindowsApps` cannot be started directly; they need package activation.
pub fn is_packaged_path(exe: &Path) -> bool {
    exe.components().any(|c| c.as_os_str().eq_ignore_ascii_case("WindowsApps"))
}

#[derive(Debug, Clone, Default)]
pub struct CommandInfo {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
}

/// Command lines and working directories for `pids`, in one scoped `sysinfo` refresh.
/// Processes we can't read (elevated, protected) are simply absent.
pub fn command_info(pids: &[u32]) -> HashMap<u32, CommandInfo> {
    if pids.is_empty() {
        return HashMap::new();
    }
    let pids: Vec<Pid> = pids.iter().map(|&p| Pid::from_u32(p)).collect();
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always).with_cwd(UpdateKind::Always),
    );
    pids.iter()
        .filter_map(|pid| {
            let p = sys.process(*pid)?;
            Some((
                pid.as_u32(),
                CommandInfo {
                    argv: p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect(),
                    cwd: p.cwd().map(Path::to_path_buf),
                },
            ))
        })
        .collect()
}

/// The subset of a captured command line that is safe to replay.
///
/// Replaying whole command lines is a trap (child-process switches, stale temp paths,
/// one-shot flags). We keep only arguments that name an existing file or folder, which
/// is what "reopen the document this app had open" needs.
pub fn replayable_args(argv: &[String], exe: &Path) -> Vec<String> {
    let exe_dir = exe.parent();
    argv.iter()
        .skip(1)
        .map(|a| a.trim().trim_matches('"').to_string())
        .filter(|a| !a.starts_with('-') && !a.starts_with('/'))
        .filter(|a| {
            let p = Path::new(a);
            let is_binary = p
                .extension()
                .map(|e| ["exe", "dll", "sys", "com"].iter().any(|b| e.eq_ignore_ascii_case(b)))
                .unwrap_or(false);
            p.is_absolute() && !is_binary && Some(p) != exe_dir && p.exists()
        })
        .take(8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replayable_args_keeps_only_existing_documents() {
        let dir = std::env::temp_dir();
        let doc = dir.join(format!("savestates-doc-{}.txt", std::process::id()));
        std::fs::write(&doc, b"x").unwrap();
        let doc_s = doc.to_string_lossy().into_owned();
        let argv = vec![
            r"C:\Program Files\App\app.exe".to_string(),
            "--type=renderer".into(),
            "/n".into(),
            format!("\"{doc_s}\""),
            r"C:\definitely\missing\file.txt".into(),
            r"C:\Windows\System32\kernel32.dll".into(),
            "relative.txt".into(),
        ];
        let kept = replayable_args(&argv, Path::new(r"C:\Program Files\App\app.exe"));
        assert_eq!(kept, vec![doc_s]);
        let _ = std::fs::remove_file(doc);
    }

    #[test]
    fn packaged_path_detection() {
        assert!(is_packaged_path(Path::new(
            r"C:\Program Files\WindowsApps\Microsoft.WindowsNotepad_11.2507.26.0_x64__8wekyb3d8bbwe\Notepad\Notepad.exe"
        )));
        assert!(!is_packaged_path(Path::new(r"C:\Windows\notepad.exe")));
    }
}
