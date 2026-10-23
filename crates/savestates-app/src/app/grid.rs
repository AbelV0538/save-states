//! The top bar, status bar, and the grid of save-state cards.

use std::collections::HashMap;
use std::path::Path;

use chrono::Local;
use eframe::egui::{self, Align, Align2, Button, FontId, Layout, RichText, Sense, Stroke, TextEdit, Vec2};
use savestates_core::model::{CaptureScope, ItemSummary};
use savestates_core::win::launch;
use uuid::Uuid;

use super::{Dialog, Filter, SaveStatesApp, chrome_setup};
use crate::worker::Command;

const CARD_INNER_W: f32 = 240.0;
const THUMB: Vec2 = Vec2::new(240.0, 135.0);

/// Thumbnails decoded into textures, keyed by state and file name so that a new file
/// (custom image, re-capture) is picked up and an unchanged one is never re-decoded.
#[derive(Default)]
pub(super) struct ThumbCache {
    map: HashMap<Uuid, (String, Option<egui::TextureHandle>)>,
}

impl ThumbCache {
    pub(super) fn invalidate(&mut self, id: Uuid) {
        self.map.remove(&id);
    }

    fn get(&mut self, ctx: &egui::Context, dir: &Path, id: Uuid, file: Option<&str>) -> Option<egui::TextureHandle> {
        let file = file?;
        if let Some((cached, tex)) = self.map.get(&id)
            && cached == file
        {
            return tex.clone();
        }
        let tex = load_texture(ctx, &dir.join(file), id);
        self.map.insert(id, (file.to_string(), tex.clone()));
        tex
    }
}

fn load_texture(ctx: &egui::Context, path: &Path, id: Uuid) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(&std::fs::read(path).ok()?).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    Some(ctx.load_texture(format!("thumb-{id}"), color, egui::TextureOptions::LINEAR))
}

struct Card {
    id: Uuid,
    name: String,
    favorite: bool,
    custom_thumb: bool,
    thumb_file: Option<String>,
    when: String,
    summary: String,
    scope: CaptureScope,
}

enum CardAction {
    Load,
    ToggleFavorite,
    Rename,
    Update,
    ChangeThumbnail,
    ResetThumbnail,
    Delete,
}

impl SaveStatesApp<'_> {
    pub(super) fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading("Save States");
            ui.separator();
            let new = ui.add_enabled(self.shell.busy.is_none(), Button::new("+ New save state"));
            if new.on_hover_text("Capture the apps and tabs open right now").clicked() {
                self.open_new_state_dialog();
            }
            ui.separator();
            ui.selectable_value(&mut self.filter, Filter::All, "All");
            ui.selectable_value(&mut self.filter, Filter::Favorites, "\u{2605} Favorites");
            ui.add(TextEdit::singleline(&mut self.search).hint_text("Search").desired_width(160.0))
                .accessible_name("Search save states");

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.menu_button("Settings", |ui| self.settings_menu(ui));
                let connected = self.shell.hub.bridges().len();
                let label = match connected {
                    0 => "Chrome: not connected".to_string(),
                    1 => "Chrome: 1 profile connected".to_string(),
                    n => format!("Chrome: {n} profiles connected"),
                };
                if ui.button(label).on_hover_text("Set up saving Chrome tabs").clicked() {
                    self.dialog =
                        Dialog::ChromeSetup(Box::new(chrome_setup::SetupView::new(&self.shell.paths, &self.shell.hub)));
                }
            });
        });
        ui.add_space(4.0);
    }

    fn settings_menu(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("When saving").strong());
        let current = self.default_scope();
        let mut choice = current;
        ui.radio_value(&mut choice, None, "Ask which desktops each time");
        ui.radio_value(&mut choice, Some(CaptureScope::CurrentDesktop), "Always this desktop only");
        ui.radio_value(&mut choice, Some(CaptureScope::AllDesktops), "Always all desktops");
        if choice != current {
            let r = self
                .shell
                .store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .update_settings(|s| s.default_scope = choice);
            if let Err(e) = r {
                self.notify(format!("Could not save settings: {e:#}"));
            }
        }
        ui.separator();
        if ui.button("Chrome integration...").clicked() {
            self.dialog =
                Dialog::ChromeSetup(Box::new(chrome_setup::SetupView::new(&self.shell.paths, &self.shell.hub)));
        }
        if ui.button("Open data folder").clicked() {
            let _ =
                launch::shell_open("explorer.exe", Some(&launch::quote_arg(&self.shell.paths.local.to_string_lossy())));
        }
        ui.separator();
        if ui.button("Quit Save States").clicked() {
            let ctx = ui.ctx().clone();
            self.quit(&ctx);
        }
    }

    pub(super) fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(busy) = &self.shell.busy {
                ui.spinner();
                ui.label(busy);
                if let Some((done, total)) = self.shell.progress {
                    ui.label(RichText::new(format!("{done}/{total}")).weak());
                }
            } else if let Some((notice, _)) = &self.shell.notice {
                ui.label(notice);
            } else {
                let n = self.shell.store.lock().unwrap_or_else(|e| e.into_inner()).states().len();
                ui.label(RichText::new(format!("{n} saved state{}", if n == 1 { "" } else { "s" })).weak());
            }
        });
    }

    pub(super) fn grid(&mut self, ui: &mut egui::Ui) {
        let cards = self.visible_cards();
        if cards.is_empty() {
            ui.centered_and_justified(|ui| {
                let text = match (self.filter, self.search.is_empty()) {
                    (_, false) => "No save states match your search.",
                    (Filter::Favorites, true) => "No favorites yet. Click the star on a save state to pin it here.",
                    (Filter::All, true) => {
                        "No save states yet.\nClick \"+ New save state\" to capture the apps and tabs open right now."
                    }
                };
                ui.label(RichText::new(text).weak());
            });
            return;
        }
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(12.0, 12.0);
                for card in &cards {
                    if let Some(action) = self.card(ui, card) {
                        self.apply(&ctx, card, action);
                    }
                }
            });
        });
    }

    fn visible_cards(&self) -> Vec<Card> {
        let store = self.shell.store.lock().unwrap_or_else(|e| e.into_inner());
        let needle = self.search.trim().to_lowercase();
        let mut states: Vec<_> = store
            .states()
            .iter()
            .filter(|s| self.filter == Filter::All || s.favorite)
            .filter(|s| needle.is_empty() || s.name.to_lowercase().contains(&needle))
            .collect();
        states.sort_by(|a, b| b.favorite.cmp(&a.favorite).then(b.updated_at.cmp(&a.updated_at)));
        states
            .into_iter()
            .map(|s| Card {
                id: s.id,
                name: s.name.clone(),
                favorite: s.favorite,
                custom_thumb: s.thumbnail.custom,
                thumb_file: s.thumbnail.file.clone(),
                when: s.updated_at.with_timezone(&Local).format("%b %-d, %Y  %-I:%M %p").to_string(),
                summary: describe(s.summary(), s.scope),
                scope: s.scope,
            })
            .collect()
    }

    fn card(&mut self, ui: &mut egui::Ui, card: &Card) -> Option<CardAction> {
        let mut action = None;
        let highlighted = self.shell.highlight == Some(card.id);
        let visuals = ui.visuals().clone();
        let stroke = if highlighted {
            Stroke::new(2.0, visuals.selection.stroke.color)
        } else {
            visuals.widgets.noninteractive.bg_stroke
        };
        let thumbs_dir = self.shell.paths.thumbs_dir();
        egui::Frame::group(ui.style()).stroke(stroke).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
            ui.set_width(CARD_INNER_W);
            ui.vertical(|ui| {
                let tex = self.thumbs.get(ui.ctx(), &thumbs_dir, card.id, card.thumb_file.as_deref());
                let thumb = match tex {
                    Some(tex) => ui.add(
                        egui::Image::from_texture(egui::load::SizedTexture::from_handle(&tex))
                            .fit_to_exact_size(THUMB)
                            .corner_radius(4.0)
                            .sense(Sense::click()),
                    ),
                    None => placeholder(ui),
                };
                if thumb.on_hover_text("Load this state").clicked() {
                    action = Some(CardAction::Load);
                }
                ui.horizontal(|ui| {
                    let star = if card.favorite { "\u{2605}" } else { "\u{2606}" };
                    let tip = if card.favorite { "Remove from favorites" } else { "Add to favorites" };
                    let toggle = ui.add(Button::new(RichText::new(star).size(16.0)).frame(false));
                    // Screen readers should hear "Favorite, checked", not the glyph.
                    toggle.widget_info(|| {
                        egui::WidgetInfo::selected(egui::Role::CheckBox, true, card.favorite, "Favorite")
                    });
                    if toggle.on_hover_text(tip).clicked() {
                        action = Some(CardAction::ToggleFavorite);
                    }
                    ui.add(egui::Label::new(RichText::new(&card.name).strong()).truncate());
                });
                ui.label(RichText::new(&card.when).small().weak());
                ui.label(RichText::new(&card.summary).small());
                ui.horizontal(|ui| {
                    if ui.add_enabled(self.shell.busy.is_none(), Button::new("Load")).clicked() {
                        action = Some(CardAction::Load);
                    }
                    ui.menu_button("More", |ui| {
                        if ui.button("Rename...").clicked() {
                            action = Some(CardAction::Rename);
                        }
                        if ui
                            .add_enabled(self.shell.busy.is_none(), Button::new("Update with what's open now..."))
                            .clicked()
                        {
                            action = Some(CardAction::Update);
                        }
                        if ui.button("Change thumbnail...").clicked() {
                            action = Some(CardAction::ChangeThumbnail);
                        }
                        if card.custom_thumb && ui.button("Use the screenshot as thumbnail").clicked() {
                            action = Some(CardAction::ResetThumbnail);
                        }
                        ui.separator();
                        if ui.button("Delete...").clicked() {
                            action = Some(CardAction::Delete);
                        }
                    });
                });
            });
        });
        action
    }

    fn apply(&mut self, ctx: &egui::Context, card: &Card, action: CardAction) {
        match action {
            CardAction::Load => self.begin_load(ctx, card.id),
            CardAction::ToggleFavorite => {
                let r = self
                    .shell
                    .store
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .update(card.id, |s| s.favorite = !s.favorite);
                if let Err(e) = r {
                    self.notify(format!("Could not save: {e:#}"));
                }
            }
            CardAction::Rename => self.dialog = Dialog::Rename { id: card.id, name: card.name.clone() },
            CardAction::Update => {
                self.dialog = Dialog::ConfirmUpdate { id: card.id, name: card.name.clone(), scope: card.scope };
            }
            CardAction::ChangeThumbnail => {
                let picked = rfd::FileDialog::new()
                    .set_title("Choose a thumbnail image")
                    .add_filter("Images", &["png", "jpg", "jpeg", "bmp", "gif", "webp"])
                    .pick_file();
                if let Some(source) = picked {
                    self.shell.worker.send(Command::SetThumbnail { id: card.id, source });
                }
            }
            CardAction::ResetThumbnail => self.shell.worker.send(Command::ResetThumbnail { id: card.id }),
            CardAction::Delete => self.dialog = Dialog::ConfirmDelete { id: card.id, name: card.name.clone() },
        }
    }
}

fn placeholder(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(THUMB, Sense::click());
    let visuals = ui.visuals();
    ui.painter().rect_filled(rect, 4.0, visuals.faint_bg_color);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        "No preview",
        FontId::proportional(14.0),
        visuals.weak_text_color(),
    );
    response
}

fn describe(s: ItemSummary, scope: CaptureScope) -> String {
    let plural = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut parts = Vec::new();
    if s.apps > 0 {
        parts.push(plural(s.apps, "app", "apps"));
    }
    if s.folders > 0 {
        parts.push(plural(s.folders, "folder", "folders"));
    }
    if s.chrome_windows > 0 {
        let windows = plural(s.chrome_windows, "Chrome window", "Chrome windows");
        parts.push(if s.chrome_tabs > 0 {
            format!("{windows} ({})", plural(s.chrome_tabs, "tab", "tabs"))
        } else {
            windows
        });
    }
    if s.vscode_windows > 0 {
        parts.push(plural(s.vscode_windows, "VS Code window", "VS Code windows"));
    }
    if parts.is_empty() {
        parts.push("Empty".into());
    }
    if scope == CaptureScope::AllDesktops {
        parts.push("all desktops".into());
    }
    parts.join(" \u{b7} ")
}
