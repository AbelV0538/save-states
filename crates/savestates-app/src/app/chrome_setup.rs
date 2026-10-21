//! The one-time Chrome setup screen: helper registration, where the extension lives,
//! and a live per-profile "connected" indicator so the user can see each step work.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Grid, Id, Layout, Modal, RichText};
use savestates_core::chrome::setup::{self, HostStatus};
use savestates_core::chrome::{BridgeHub, ChromeEnv, InstanceMap, profiles};
use savestates_core::paths::AppPaths;
use savestates_core::win::launch;

use super::dialogs::Outcome;

pub(crate) struct SetupView {
    host: HostStatus,
    extension_dir: Option<PathBuf>,
    chrome_exe: Option<PathBuf>,
    rows: Vec<Row>,
    unlinked: usize,
    refreshed: Instant,
    stale: bool,
    message: Option<String>,
}

struct Row {
    dir: String,
    label: String,
    live: bool,
    connected: bool,
}

impl SetupView {
    pub(crate) fn new(paths: &AppPaths, hub: &BridgeHub) -> Self {
        let mut view = Self {
            host: HostStatus::NotRegistered,
            extension_dir: setup::extension_dir(),
            chrome_exe: profiles::find_chrome_exe(),
            rows: Vec::new(),
            unlinked: 0,
            refreshed: Instant::now(),
            stale: true,
            message: None,
        };
        view.refresh(paths, hub);
        view
    }

    fn refresh(&mut self, paths: &AppPaths, hub: &BridgeHub) {
        self.host = setup::host_status();
        let env = ChromeEnv::load();
        let map = InstanceMap::load(&paths.chrome_instances_file());
        let bridges = hub.bridges();
        let connected: HashSet<&str> = bridges.iter().filter_map(|b| map.profile_of(&b.instance)).collect();
        self.unlinked = bridges.iter().filter(|b| map.profile_of(&b.instance).is_none()).count();
        self.rows = env
            .profiles
            .iter()
            .map(|p| Row {
                dir: p.dir.clone(),
                label: env.label(&p.dir),
                live: env.is_profile_live(&p.dir),
                connected: connected.contains(p.dir.as_str()),
            })
            .collect();
        self.refreshed = Instant::now();
        self.stale = false;
    }

    pub(super) fn show(&mut self, ctx: &egui::Context, paths: &AppPaths, hub: &BridgeHub) -> Outcome {
        if self.stale || self.refreshed.elapsed() > Duration::from_secs(2) {
            self.refresh(paths, hub);
        }
        // Keep the connection indicators live while the screen is open.
        ctx.request_repaint_after(Duration::from_secs(1));

        let mut outcome = Outcome::Keep;
        let r = Modal::new(Id::unique("chrome_setup")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading("Chrome integration");
            ui.add_space(4.0);
            ui.label(
                "Save States always reopens Chrome windows in the right profile. To also save the tabs \
                 inside them, each Chrome profile needs the small companion extension. This is a one-time \
                 step; Chrome doesn't let apps install extensions automatically.",
            );
            ui.separator();

            ui.label(RichText::new("1. Helper").strong());
            ui.horizontal(|ui| {
                match &self.host {
                    HostStatus::Ready { .. } => {
                        ui.label("\u{2714} Registered with Chrome");
                    }
                    HostStatus::Broken { .. } => {
                        ui.label("\u{26A0} Registered, but the helper program is missing");
                    }
                    HostStatus::NotRegistered => {
                        ui.label("\u{2716} Not registered");
                    }
                }
                let label = if matches!(self.host, HostStatus::Ready { .. }) { "Re-register" } else { "Register" };
                if ui.button(label).clicked() {
                    self.message = Some(match setup::bridge_exe() {
                        None => "savestates-bridge.exe was not found next to Save States.".into(),
                        Some(bridge) => match setup::register_host(paths, &bridge) {
                            Ok(_) => "Helper registered.".into(),
                            Err(e) => format!("Registration failed: {e:#}"),
                        },
                    });
                    self.stale = true;
                }
            });

            ui.add_space(6.0);
            ui.label(RichText::new("2. Extension, in each Chrome profile").strong());
            match &self.extension_dir {
                Some(dir) => {
                    let dir_s = dir.to_string_lossy().into_owned();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&dir_s).monospace());
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Open folder").clicked() {
                            let _ = launch::shell_open("explorer.exe", Some(&launch::quote_arg(&dir_s)));
                        }
                        if ui.button("Copy path").clicked() {
                            ui.ctx().copy_text(dir_s.clone());
                        }
                    });
                }
                None => {
                    ui.label("The extension folder was not found next to Save States.");
                }
            }
            ui.horizontal(|ui| {
                ui.label("a. Go to");
                ui.label(RichText::new("chrome://extensions").monospace());
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text("chrome://extensions".into());
                }
            });
            ui.label("b. Turn on Developer mode (top right)");
            ui.label("c. Click Load unpacked and choose the folder above");

            ui.add_space(6.0);
            ui.label(RichText::new("Profiles").strong());
            Grid::new("profiles").num_columns(3).spacing([16.0, 4.0]).show(ui, |ui| {
                for row in &self.rows {
                    ui.label(&row.label);
                    let status = match (row.connected, row.live) {
                        (true, _) => "\u{2714} Tabs are saved",
                        (false, true) => "Running, extension not connected",
                        (false, false) => "Not running",
                    };
                    ui.label(status);
                    if let Some(chrome) = &self.chrome_exe
                        && ui.small_button("Open in Chrome").clicked()
                    {
                        let args = [format!("--profile-directory={}", row.dir), "chrome://extensions/".into()];
                        let _ = launch::spawn(chrome, &args, None);
                    }
                    ui.end_row();
                }
            });
            if self.unlinked > 0 {
                ui.label(
                    RichText::new(format!(
                        "{} extension connection(s) not matched to a profile yet. They match automatically \
                         once that profile has a window open.",
                        self.unlinked
                    ))
                    .small()
                    .weak(),
                );
            }
            if let Some(msg) = &self.message {
                ui.add_space(4.0);
                ui.label(RichText::new(msg).italics());
            }
            ui.add_space(8.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Done").clicked() {
                    outcome = Outcome::Close;
                }
            });
        });
        if r.should_close() { Outcome::Close } else { outcome }
    }
}
