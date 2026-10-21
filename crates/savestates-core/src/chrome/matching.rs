//! Pairing live Chrome HWNDs with windows from extension snapshots.
//!
//! The extension knows tabs but not HWNDs; Win32 knows HWNDs (and, via the window's
//! AppUserModelID, the profile) but not tabs. Two independent signals join them:
//! - title: Chrome titles a window "<active tab title> - Google Chrome";
//! - bounds: the extension reports DIPs, Win32 reports physical pixels, so DIPs are
//!   scaled by the window's DPI. Both the outer rect and the DWM frame are tried,
//!   since which one Chrome's bounds line up with depends on the frame style.
//!
//! Either signal alone can be wrong (two windows on the same page; two maximized
//! windows), together they are reliable. Assignment is greedy by ascending cost,
//! which is optimal enough for the handful of windows a desktop has.

use crate::chrome::snapshot::ExtWindow;
use crate::model::{Rect, ShowState};

#[derive(Debug, Clone)]
pub struct HwndInfo {
    pub title: String,
    pub rect: Rect,
    pub frame: Rect,
    pub dpi: u32,
    pub show: ShowState,
    pub profile_dir: String,
}

#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub window: &'a ExtWindow,
    /// Profile the snapshot's extension instance is known to belong to, if learned.
    pub profile: Option<&'a str>,
}

/// Total cost above which a pairing is rejected: one signal may disagree, not both.
const MAX_COST: f64 = 1.2;

/// For each HWND, the index of the candidate it matched (each candidate used at most once).
pub fn match_windows(hwnds: &[HwndInfo], candidates: &[Candidate<'_>]) -> Vec<Option<usize>> {
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (hi, h) in hwnds.iter().enumerate() {
        for (ci, c) in candidates.iter().enumerate() {
            if let Some(cost) = cost(h, c)
                && cost <= MAX_COST
            {
                pairs.push((cost, hi, ci));
            }
        }
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));

    let mut by_hwnd = vec![None; hwnds.len()];
    let mut used = vec![false; candidates.len()];
    for (_, hi, ci) in pairs {
        if by_hwnd[hi].is_none() && !used[ci] {
            by_hwnd[hi] = Some(ci);
            used[ci] = true;
        }
    }
    by_hwnd
}

/// Whether the title alone identifies the window exactly (used to learn instance -> profile).
pub fn title_matches_exactly(h: &HwndInfo, w: &ExtWindow) -> bool {
    title_cost(&h.title, w.active_title()) == 0.0
}

fn cost(h: &HwndInfo, c: &Candidate<'_>) -> Option<f64> {
    // DevTools windows have no restorable content; incognito is never restored.
    let kind_ok = matches!(c.window.kind.as_str(), "" | "normal" | "popup" | "app");
    if c.window.incognito || !kind_ok {
        return None;
    }
    if c.profile.is_some_and(|p| p != h.profile_dir) {
        return None;
    }
    let title = title_cost(&h.title, c.window.active_title());
    let state = if state_matches(h.show, &c.window.state) { 0.0 } else { 0.5 };
    // A maximized or minimized window's bounds say nothing useful about identity.
    let bounds = if h.show == ShowState::Normal { bounds_cost(h, c.window) } else { 0.0 };
    Some(title + state + bounds)
}

fn title_cost(hwnd_title: &str, active_tab_title: &str) -> f64 {
    let active = active_tab_title.trim();
    if active.is_empty() {
        return if hwnd_title == "Google Chrome" { 0.2 } else { 1.0 };
    }
    if hwnd_title == format!("{active} - Google Chrome") {
        0.0
    } else if hwnd_title.starts_with(active) {
        // Same page, decorated differently (profile name, audio indicator, ...).
        0.2
    } else {
        1.0
    }
}

fn state_matches(show: ShowState, state: &str) -> bool {
    match show {
        ShowState::Maximized => state == "maximized" || state == "fullscreen",
        ShowState::Minimized => state == "minimized",
        ShowState::Normal => state == "normal" || state.is_empty(),
    }
}

fn bounds_cost(h: &HwndInfo, w: &ExtWindow) -> f64 {
    let scale = h.dpi.max(96) as f64 / 96.0;
    let s = |v: i32| (v as f64 * scale).round();
    let (l, t, wd, ht) = (s(w.left), s(w.top), s(w.width), s(w.height));
    let distance = |r: &Rect| {
        let d = (r.left as f64 - l).abs()
            + (r.top as f64 - t).abs()
            + (r.width() as f64 - wd).abs()
            + (r.height() as f64 - ht).abs();
        d / (wd + ht).max(1.0)
    };
    // 25% total deviation costs 0.5; anything past 50% is a full mismatch.
    (distance(&h.rect).min(distance(&h.frame)) * 2.0).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::snapshot::ExtTab;

    fn win(title: &str, state: &str, (left, top, width, height): (i32, i32, i32, i32)) -> ExtWindow {
        ExtWindow {
            state: state.into(),
            kind: "normal".into(),
            left,
            top,
            width,
            height,
            tabs: vec![ExtTab { title: title.into(), active: true, ..Default::default() }],
            ..Default::default()
        }
    }

    fn hwnd(title: &str, show: ShowState, rect: Rect, dpi: u32, profile: &str) -> HwndInfo {
        HwndInfo { title: title.into(), rect, frame: rect, dpi, show, profile_dir: profile.into() }
    }

    fn r(left: i32, top: i32, w: i32, h: i32) -> Rect {
        Rect { left, top, right: left + w, bottom: top + h }
    }

    #[test]
    fn matches_by_title_and_dpi_scaled_bounds() {
        // 150% scaling: the extension reports 800x600 DIPs at (100,100) -> 1200x900 px at (150,150).
        let windows = [win("Docs", "normal", (100, 100, 800, 600)), win("Mail", "normal", (0, 0, 640, 480))];
        let hwnds = [
            hwnd("Mail - Google Chrome", ShowState::Normal, r(0, 0, 960, 720), 144, "Default"),
            hwnd("Docs - Google Chrome", ShowState::Normal, r(150, 150, 1200, 900), 144, "Default"),
        ];
        let cands: Vec<_> = windows.iter().map(|w| Candidate { window: w, profile: None }).collect();
        assert_eq!(match_windows(&hwnds, &cands), vec![Some(1), Some(0)]);
    }

    #[test]
    fn bounds_break_a_title_tie() {
        let windows = [win("New Tab", "normal", (0, 0, 800, 600)), win("New Tab", "normal", (900, 0, 800, 600))];
        let hwnds = [
            hwnd("New Tab - Google Chrome", ShowState::Normal, r(900, 0, 800, 600), 96, "Default"),
            hwnd("New Tab - Google Chrome", ShowState::Normal, r(0, 0, 800, 600), 96, "Default"),
        ];
        let cands: Vec<_> = windows.iter().map(|w| Candidate { window: w, profile: None }).collect();
        assert_eq!(match_windows(&hwnds, &cands), vec![Some(1), Some(0)]);
    }

    #[test]
    fn known_profile_prevents_cross_profile_matches() {
        let windows = [win("Same page", "maximized", (0, 0, 1920, 1040))];
        let hwnds = [hwnd("Same page - Google Chrome", ShowState::Maximized, r(-8, -8, 1936, 1056), 96, "Profile 2")];
        let wrong = [Candidate { window: &windows[0], profile: Some("Default") }];
        assert_eq!(match_windows(&hwnds, &wrong), vec![None]);
        let right = [Candidate { window: &windows[0], profile: Some("Profile 2") }];
        assert_eq!(match_windows(&hwnds, &right), vec![Some(0)]);
    }

    #[test]
    fn rejects_when_title_and_bounds_both_disagree() {
        let windows = [win("Something else", "normal", (0, 0, 800, 600))];
        let hwnds = [hwnd("Docs - Google Chrome", ShowState::Normal, r(2000, 900, 400, 300), 96, "Default")];
        let cands = [Candidate { window: &windows[0], profile: None }];
        assert_eq!(match_windows(&hwnds, &cands), vec![None]);
    }

    #[test]
    fn incognito_and_devtools_windows_are_never_candidates() {
        let mut incog = win("Docs", "normal", (0, 0, 800, 600));
        incog.incognito = true;
        let mut devtools = win("Docs", "normal", (0, 0, 800, 600));
        devtools.kind = "devtools".into();
        let hwnds = [hwnd("Docs - Google Chrome", ShowState::Normal, r(0, 0, 800, 600), 96, "Default")];
        for w in [&incog, &devtools] {
            assert_eq!(match_windows(&hwnds, &[Candidate { window: w, profile: None }]), vec![None]);
        }
    }
}
