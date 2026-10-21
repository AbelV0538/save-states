//! Modal dialogs. Each dialog function only edits its own fields and returns an
//! [`Outcome`]; acting on it happens afterwards with full access to the app.

use eframe::egui::{self, Align, Button, Id, Key, Layout, Modal, RichText, ScrollArea, TextEdit};
use savestates_core::model::CaptureScope;
use savestates_core::restore::{RestorePlan, RestoreReport};
use uuid::Uuid;

use super::{Dialog, SaveStatesApp};
use crate::shell::default_state_name;

pub(super) enum Outcome {
    Keep,
    Close,
    Capture { name: String, scope: CaptureScope, remember: bool, replace: Option<Uuid> },
    Restore { close_extras: bool },
    Rename { id: Uuid, name: String },
    Delete { id: Uuid },
    Update { id: Uuid, name: String, scope: CaptureScope },
}

impl SaveStatesApp<'_> {
    pub(super) fn dialogs(&mut self, ctx: &egui::Context) {
        let outcome = match &mut self.dialog {
            Dialog::None => return,
            Dialog::NewState { name, scope, remember, ask_scope, replace } => {
                new_state(ctx, name, scope, remember, ask_scope, *replace)
            }
            Dialog::Planning { name } => planning(ctx, name),
            Dialog::ConfirmLoad { plan, close_extras } => confirm_load(ctx, plan, close_extras),
            Dialog::Rename { id, name } => rename(ctx, *id, name),
            Dialog::ConfirmDelete { id, name } => confirm(
                ctx,
                "Delete save state?",
                &format!("\"{name}\" and its thumbnail will be deleted. This can't be undone."),
                "Delete",
                Outcome::Delete { id: *id },
            ),
            Dialog::ConfirmUpdate { id, name, scope } => confirm(
                ctx,
                "Update save state?",
                &format!(
                    "Replace what \"{name}\" contains with the apps and tabs open right now? \
                     Its name, favorite and custom thumbnail are kept."
                ),
                "Update",
                Outcome::Update { id: *id, name: name.clone(), scope: *scope },
            ),
            Dialog::Report { name, report } => report_dialog(ctx, name, report),
            Dialog::ChromeSetup(view) => view.show(ctx, &self.shell.paths, &self.shell.hub),
            Dialog::Message { title, body } => message(ctx, title, body),
        };
        self.resolve(ctx, outcome);
    }

    fn resolve(&mut self, ctx: &egui::Context, outcome: Outcome) {
        match outcome {
            Outcome::Keep => {}
            Outcome::Close => self.dialog = Dialog::None,
            Outcome::Capture { name, scope, remember, replace } => {
                self.dialog = Dialog::None;
                let wanted = remember.then_some(scope);
                if self.default_scope() != wanted {
                    let r = self
                        .shell
                        .store
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .update_settings(|s| s.default_scope = wanted);
                    if let Err(e) = r {
                        self.notify(format!("Could not save settings: {e:#}"));
                    }
                }
                let name = if name.trim().is_empty() { default_state_name() } else { name.trim().to_string() };
                self.start_capture(ctx, name, scope, replace, true);
            }
            Outcome::Restore { close_extras } => {
                if let Dialog::ConfirmLoad { plan, .. } = std::mem::replace(&mut self.dialog, Dialog::None) {
                    self.start_restore(ctx, plan, close_extras);
                }
            }
            Outcome::Rename { id, name } => {
                self.dialog = Dialog::None;
                let name = name.trim().to_string();
                if !name.is_empty() {
                    let r = self.shell.store.lock().unwrap_or_else(|e| e.into_inner()).update(id, |s| s.name = name);
                    if let Err(e) = r {
                        self.notify(format!("Could not rename: {e:#}"));
                    }
                }
            }
            Outcome::Delete { id } => {
                self.dialog = Dialog::None;
                let removed = self.shell.store.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
                match removed {
                    Ok(_) => {
                        let thumbs = self.shell.paths.thumbs_dir();
                        let _ = std::fs::remove_file(thumbs.join(format!("{id}.jpg")));
                        let _ = std::fs::remove_file(thumbs.join(format!("{id}-custom.jpg")));
                        self.thumbs.invalidate(id);
                    }
                    Err(e) => self.notify(format!("Could not delete: {e:#}")),
                }
            }
            Outcome::Update { id, name, scope } => {
                self.dialog = Dialog::None;
                self.start_capture(ctx, name, scope, Some(id), true);
            }
        }
    }
}

/// Standard footer: primary action on the right, Cancel next to it. Enter confirms.
fn buttons(ui: &mut egui::Ui, primary: &str, enabled: bool) -> (bool, bool) {
    let mut ok = false;
    let mut cancel = false;
    ui.add_space(8.0);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ok = ui.add_enabled(enabled, Button::new(RichText::new(primary).strong())).clicked();
        cancel = ui.button("Cancel").clicked();
    });
    ok |= enabled && ui.input(|i| i.key_pressed(Key::Enter));
    (ok, cancel)
}

fn new_state(
    ctx: &egui::Context,
    name: &mut String,
    scope: &mut CaptureScope,
    remember: &mut bool,
    ask_scope: &mut bool,
    replace: Option<Uuid>,
) -> Outcome {
    let mut outcome = Outcome::Keep;
    let r = Modal::new(Id::unique("new_state")).show(ctx, |ui| {
        ui.set_width(400.0);
        ui.heading("New save state");
        ui.add_space(6.0);
        ui.label("Name");
        let edit = ui.add(TextEdit::singleline(name).desired_width(f32::INFINITY));
        if ui.memory(|m| m.focused().is_none()) {
            edit.request_focus();
        }
        ui.add_space(8.0);
        if *ask_scope {
            ui.label("Which windows should be saved?");
            ui.radio_value(scope, CaptureScope::CurrentDesktop, "Only this virtual desktop");
            ui.radio_value(
                scope,
                CaptureScope::AllDesktops,
                "All virtual desktops (each window goes back to its desktop)",
            );
            ui.checkbox(remember, "Remember this choice and don't ask again")
                .on_hover_text("You can change it later under Settings");
        } else {
            // The user chose not to be asked; show the remembered choice, one click to change.
            ui.horizontal(|ui| {
                let what = match scope {
                    CaptureScope::CurrentDesktop => "only this virtual desktop",
                    CaptureScope::AllDesktops => "all virtual desktops",
                };
                ui.label(format!("Saving windows from {what}."));
                if ui.small_button("Change").clicked() {
                    *ask_scope = true;
                }
            });
        }
        ui.add_space(4.0);
        ui.label(RichText::new("Save States hides while it takes the screenshot for the thumbnail.").small().weak());
        let (ok, cancel) = buttons(ui, "Save", true);
        if ok {
            outcome = Outcome::Capture { name: name.clone(), scope: *scope, remember: *remember, replace };
        } else if cancel {
            outcome = Outcome::Close;
        }
    });
    if r.should_close() && matches!(outcome, Outcome::Keep) { Outcome::Close } else { outcome }
}

fn planning(ctx: &egui::Context, name: &str) -> Outcome {
    let mut outcome = Outcome::Keep;
    Modal::new(Id::unique("planning")).show(ctx, |ui| {
        ui.set_width(360.0);
        ui.heading(format!("Switch to \"{name}\""));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Checking what's already open...");
        });
        ui.add_space(6.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Cancel").clicked() {
                outcome = Outcome::Close;
            }
        });
    });
    outcome
}

fn confirm_load(ctx: &egui::Context, plan: &RestorePlan, close_extras: &mut bool) -> Outcome {
    let mut outcome = Outcome::Keep;
    let r = Modal::new(Id::unique("confirm_load")).show(ctx, |ui| {
        ui.set_width(480.0);
        ui.heading(format!("Switch to \"{}\"?", plan.state_name));
        ui.add_space(6.0);
        if plan.launches.is_empty() {
            ui.label("Everything in this state is already open.");
        } else {
            ui.label(format!("This will open {}:", count(plan.launches.len(), "item", "items")));
            ScrollArea::vertical().id_salt("to_open").max_height(180.0).show(ui, |ui| {
                for unit in &plan.launches {
                    ui.label(format!("\u{2022} {}", unit.label));
                }
            });
        }
        if !plan.already_open.is_empty() {
            ui.collapsing(format!("{} already open (left as they are)", plan.already_open.len()), |ui| {
                for label in &plan.already_open {
                    ui.label(label);
                }
            });
        }
        if !plan.extras.is_empty() {
            ui.separator();
            ui.checkbox(
                close_extras,
                format!(
                    "Also close the {} that aren't part of this state",
                    count(plan.extras.len(), "window", "windows")
                ),
            );
            ui.label(
                RichText::new(
                    "Apps get a normal close request, so they can still ask you to save. Nothing is force-closed.",
                )
                .small()
                .weak(),
            );
            ui.collapsing("Which windows?", |ui| {
                ScrollArea::vertical().id_salt("extras").max_height(140.0).show(ui, |ui| {
                    for e in &plan.extras {
                        ui.label(&e.label);
                    }
                });
            });
        }
        let nothing_to_do = plan.launches.is_empty() && !*close_extras;
        let (ok, cancel) = buttons(ui, "Switch", !nothing_to_do);
        if ok {
            outcome = Outcome::Restore { close_extras: *close_extras };
        } else if cancel {
            outcome = Outcome::Close;
        }
    });
    if r.should_close() && matches!(outcome, Outcome::Keep) { Outcome::Close } else { outcome }
}

fn rename(ctx: &egui::Context, id: Uuid, name: &mut String) -> Outcome {
    let mut outcome = Outcome::Keep;
    let r = Modal::new(Id::unique("rename")).show(ctx, |ui| {
        ui.set_width(360.0);
        ui.heading("Rename");
        let edit = ui.add(TextEdit::singleline(name).desired_width(f32::INFINITY));
        if ui.memory(|m| m.focused().is_none()) {
            edit.request_focus();
        }
        let (ok, cancel) = buttons(ui, "Rename", !name.trim().is_empty());
        if ok {
            outcome = Outcome::Rename { id, name: name.clone() };
        } else if cancel {
            outcome = Outcome::Close;
        }
    });
    if r.should_close() && matches!(outcome, Outcome::Keep) { Outcome::Close } else { outcome }
}

fn confirm(ctx: &egui::Context, title: &str, body: &str, action: &str, on_ok: Outcome) -> Outcome {
    let r = Modal::new(Id::unique(("confirm", title))).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.heading(title);
        ui.add_space(6.0);
        ui.label(body);
        match buttons(ui, action, true) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        }
    });
    match r.inner {
        Some(true) => on_ok,
        Some(false) => Outcome::Close,
        None if r.should_close() => Outcome::Close,
        None => Outcome::Keep,
    }
}

fn report_dialog(ctx: &egui::Context, name: &str, report: &RestoreReport) -> Outcome {
    let mut outcome = Outcome::Keep;
    let r = Modal::new(Id::unique("report")).show(ctx, |ui| {
        ui.set_width(480.0);
        ui.heading(format!("Switched to \"{name}\""));
        ui.add_space(6.0);
        ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
            if !report.opened.is_empty() {
                ui.label(RichText::new(format!("Opened {}", count(report.opened.len(), "item", "items"))).strong());
                for l in &report.opened {
                    ui.label(format!("\u{2022} {l}"));
                }
            }
            if report.already_open > 0 {
                ui.label(format!("{} already open", count(report.already_open, "item was", "items were")));
            }
            if !report.failed.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new("Could not open").strong());
                for (l, why) in &report.failed {
                    ui.label(format!("\u{2022} {l}: {why}"));
                }
            }
            if !report.no_window.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new("Started, but no window appeared yet").strong());
                ui.label(
                    RichText::new("Some apps start minimized to the tray or take a while to load.").small().weak(),
                );
                for l in &report.no_window {
                    ui.label(format!("\u{2022} {l}"));
                }
            }
            if report.closed > 0 || !report.declined_close.is_empty() {
                ui.add_space(6.0);
                ui.label(format!("Closed {}", count(report.closed, "window", "windows")));
                for l in &report.declined_close {
                    ui.label(format!("\u{2022} Still open (it may be asking to save): {l}"));
                }
            }
            for note in &report.notes {
                ui.add_space(6.0);
                ui.label(RichText::new(note).small());
            }
        });
        ui.add_space(8.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("OK").clicked() {
                outcome = Outcome::Close;
            }
        });
    });
    if r.should_close() { Outcome::Close } else { outcome }
}

fn message(ctx: &egui::Context, title: &str, body: &str) -> Outcome {
    let mut outcome = Outcome::Keep;
    let r = Modal::new(Id::unique("message")).show(ctx, |ui| {
        ui.set_width(400.0);
        ui.heading(title);
        ui.add_space(6.0);
        ui.label(body);
        ui.add_space(8.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("OK").clicked() {
                outcome = Outcome::Close;
            }
        });
    });
    if r.should_close() { Outcome::Close } else { outcome }
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}
