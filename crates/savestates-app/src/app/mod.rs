//! The window: a grid of save states and the dialogs around saving and loading.
//!
//! A window is created each time Save States is opened and destroyed when it closes;
//! it borrows the long-lived [`Shell`] for its lifetime. Anything that must survive
//! the window (status, a dialog to show next time) is kept on the shell.

pub(crate) mod chrome_setup;
mod dialogs;
mod grid;

use std::time::Duration;

use eframe::egui;
use savestates_core::model::CaptureScope;
use savestates_core::restore::{RestorePlan, RestoreReport};
use uuid::Uuid;

use crate::shell::{Reaction, Shell, default_state_name};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Favorites,
}

pub(crate) enum Dialog {
    None,
    NewState { name: String, scope: CaptureScope, remember: bool, ask_scope: bool, replace: Option<Uuid> },
    Planning { name: String },
    ConfirmLoad { plan: Box<RestorePlan>, close_extras: bool },
    Rename { id: Uuid, name: String },
    ConfirmDelete { id: Uuid, name: String },
    ConfirmUpdate { id: Uuid, name: String, scope: CaptureScope },
    Report { name: String, report: RestoreReport },
    ChromeSetup(Box<chrome_setup::SetupView>),
    Message { title: String, body: String },
}

pub struct SaveStatesApp<'a> {
    shell: &'a mut Shell,
    filter: Filter,
    search: String,
    dialog: Dialog,
    thumbs: grid::ThumbCache,
}

/// egui's bundled fonts lack common UI symbols (★ ✔ ⚠), and the system font fallback
/// only searches generic families, none of which has them on Windows. Segoe UI Symbol
/// ships with every Windows install and covers them, so add it as the last fallback.
fn add_symbol_font(ctx: &egui::Context) {
    let Some(windir) = std::env::var_os("WINDIR") else { return };
    let path = std::path::Path::new(&windir).join("Fonts").join("seguisym.ttf");
    match std::fs::read(&path) {
        Ok(bytes) => ctx.add_font(egui::FontInsert::new(
            "segoe-ui-symbol",
            egui::FontData::from_owned(bytes),
            [egui::FontFamily::Proportional, egui::FontFamily::Monospace]
                .into_iter()
                .map(|family| egui::InsertFontFamily { family, priority: egui::FontPriority::Lowest })
                .collect(),
        )),
        Err(e) => tracing::debug!("no symbol font at {}: {e}", path.display()),
    }
}

impl<'a> SaveStatesApp<'a> {
    pub fn new(cc: &eframe::CreationContext<'_>, shell: &'a mut Shell) -> Self {
        add_symbol_font(&cc.egui_ctx);
        shell.waker.attach(&cc.egui_ctx);
        shell.thumbs_dirty.clear();
        let dialog = shell.pending_dialog.take().unwrap_or(Dialog::None);
        Self { shell, filter: Filter::All, search: String::new(), dialog, thumbs: grid::ThumbCache::default() }
    }

    fn close_window(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.shell.quitting = true;
        self.close_window(ctx);
    }

    fn react(&mut self, ctx: &egui::Context, reaction: Reaction) {
        match reaction {
            Reaction::Nothing => {}
            Reaction::Show => ctx.send_viewport_cmd(egui::ViewportCommand::Focus),
            Reaction::Hide => self.close_window(ctx),
            Reaction::Dialog(d) => {
                self.dialog = d;
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            Reaction::Planned(plan) => {
                if matches!(self.dialog, Dialog::Planning { .. }) {
                    self.dialog = Dialog::ConfirmLoad { plan, close_extras: false };
                }
            }
            Reaction::Quit => self.quit(ctx),
        }
    }

    fn notify(&mut self, text: impl Into<String>) {
        self.shell.notify(text);
    }

    fn default_scope(&self) -> Option<CaptureScope> {
        self.shell.default_scope()
    }

    fn open_new_state_dialog(&mut self) {
        let remembered = self.default_scope();
        self.dialog = Dialog::NewState {
            name: default_state_name(),
            scope: remembered.unwrap_or_default(),
            remember: remembered.is_some(),
            ask_scope: remembered.is_none(),
            replace: None,
        };
    }

    /// Close the window (so it isn't in the screenshot), then capture; it reopens after.
    fn start_capture(
        &mut self,
        ctx: &egui::Context,
        name: String,
        scope: CaptureScope,
        replace: Option<Uuid>,
        reveal: bool,
    ) {
        self.shell.start_capture(name, scope, replace, reveal);
        self.close_window(ctx);
    }

    fn begin_load(&mut self, ctx: &egui::Context, id: Uuid) {
        let reaction = self.shell.begin_load(id);
        self.react(ctx, reaction);
    }

    /// Get out of the way: the user is heading into the restored session.
    fn start_restore(&mut self, ctx: &egui::Context, plan: Box<RestorePlan>, close_extras: bool) {
        self.shell.start_restore(plan, close_extras);
        self.close_window(ctx);
    }
}

impl Drop for SaveStatesApp<'_> {
    fn drop(&mut self) {
        self.shell.waker.detach();
    }
}

impl eframe::App for SaveStatesApp<'_> {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Some(msg) = self.shell.try_recv() {
            let reaction = self.shell.handle(msg);
            self.react(ctx, reaction);
        }
        for id in self.shell.thumbs_dirty.drain(..) {
            self.thumbs.invalidate(id);
        }
        self.shell.sync_tray();
        if self.shell.notice.as_ref().is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(12)) {
            self.shell.notice = None;
        }
        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
            self.shell.last_size = Some(rect.size());
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("top_bar").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status_bar").show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| self.grid(ui));
        self.dialogs(&ctx);
        if self.shell.busy.is_some() || self.shell.notice.is_some() {
            // Keep the spinner moving and let notices expire.
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}
