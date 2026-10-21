//! Virtual desktops, via `winvd` (VirtualDesktopAccessor).
//!
//! `winvd` drives undocumented shell COM interfaces whose IDs change between Windows
//! builds. Everything here is probed once and latched off on failure, so a future
//! build that breaks the API degrades to "everything on the current desktop" instead
//! of failing a capture or restore.
//!
//! `winvd` caches its COM objects per thread; call these from one thread (the worker).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU8, Ordering};

use tracing::warn;
use windows058::Win32::Foundation::HWND as Hwnd058;
use windows058::core::GUID as Guid058;

use crate::model::DesktopRef;

const UNPROBED: u8 = 0;
const AVAILABLE: u8 = 1;
const UNAVAILABLE: u8 = 2;
static STATE: AtomicU8 = AtomicU8::new(UNPROBED);

pub fn available() -> bool {
    match STATE.load(Ordering::Acquire) {
        AVAILABLE => true,
        UNAVAILABLE => false,
        _ => {
            let ok = match winvd::get_desktop_count() {
                Ok(n) => n > 0,
                Err(e) => {
                    warn!("virtual desktop API unavailable ({e:?}); treating all windows as on one desktop");
                    false
                }
            };
            STATE.store(if ok { AVAILABLE } else { UNAVAILABLE }, Ordering::Release);
            ok
        }
    }
}

fn h(raw: isize) -> Hwnd058 {
    Hwnd058(raw as *mut c_void)
}

pub fn list() -> Vec<DesktopRef> {
    if !available() {
        return Vec::new();
    }
    winvd::get_desktops()
        .map(|ds| {
            ds.iter()
                .enumerate()
                .map(|(i, d)| DesktopRef {
                    index: d.get_index().unwrap_or(i as u32),
                    guid: d.get_id().ok().map(|g| guid_to_string(&g)),
                    name: d.get_name().ok().filter(|n| !n.trim().is_empty()),
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn current_index() -> Option<u32> {
    if !available() {
        return None;
    }
    winvd::get_current_desktop().ok()?.get_index().ok()
}

pub fn index_of_window(hwnd: isize) -> Option<u32> {
    if !available() {
        return None;
    }
    winvd::get_desktop_by_window(h(hwnd)).ok()?.get_index().ok()
}

/// Create desktops until at least `n` exist. Returns the resulting count.
pub fn ensure_count(n: u32) -> u32 {
    if !available() {
        return 0;
    }
    let mut count = winvd::get_desktop_count().unwrap_or(0);
    while count < n {
        if winvd::create_desktop().is_err() {
            break;
        }
        count += 1;
    }
    count
}

/// Move a window to the desktop that `target` refers to: the same desktop by GUID if it
/// still exists, otherwise the desktop at the same position.
pub fn move_window(hwnd: isize, target: &DesktopRef) -> bool {
    if !available() {
        return false;
    }
    let current = list();
    let by_guid = target.guid.as_deref().and_then(|g| current.iter().find(|d| d.guid.as_deref() == Some(g)));
    let dest = by_guid.or_else(|| current.iter().find(|d| d.index == target.index));
    let Some(guid) = dest.and_then(|d| d.guid.as_deref()).and_then(parse_guid) else {
        return false;
    };
    winvd::move_window_to_desktop(guid, &h(hwnd)).is_ok()
}

fn guid_to_string(g: &Guid058) -> String {
    let v = g.to_u128();
    format!(
        "{:08X}-{:04X}-{:04X}-{:04X}-{:012X}",
        (v >> 96) as u32,
        (v >> 80) as u16,
        (v >> 64) as u16,
        (v >> 48) as u16,
        v & 0xFFFF_FFFF_FFFF
    )
}

fn parse_guid(s: &str) -> Option<Guid058> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    (hex.len() == 32).then(|| u128::from_str_radix(&hex, 16).ok()).flatten().map(Guid058::from_u128)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_round_trip() {
        let g = Guid058::from_u128(0x20D04FE0_3AEA_1069_A2D8_08002B30309D);
        let s = guid_to_string(&g);
        assert_eq!(s, "20D04FE0-3AEA-1069-A2D8-08002B30309D");
        assert_eq!(parse_guid(&s), Some(g));
        assert_eq!(parse_guid("{20d04fe0-3aea-1069-a2d8-08002b30309d}"), Some(g));
        assert_eq!(parse_guid("nope"), None);
    }
}
