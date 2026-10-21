//! The notification-area icon and its menu.
//!
//! tray-icon must live on the thread that pumps its messages: the main thread, which
//! runs either eframe's loop (window open) or the hidden-mode pump (window closed).
//! Its event handlers fire on that thread while a message is dispatched; they only
//! forward the event, and the shell handles it.

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use uuid::Uuid;

use crate::icon;
use crate::shell::{AppMsg, Sink};

pub enum TrayMsg {
    Menu(MenuId),
    /// Left click on the icon itself.
    Activate,
}

pub enum TrayAction {
    Open,
    QuickSave,
    Load(Uuid),
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
    load_menu: Submenu,
    open: MenuId,
    quick_save: MenuId,
    quit: MenuId,
    load_items: Vec<(MenuId, Uuid)>,
}

impl Tray {
    pub fn new(sink: Sink) -> Option<Self> {
        let open = MenuItem::new("Open Save States", true, None);
        let quick_save = MenuItem::new("Quick save", true, None);
        let load_menu = Submenu::new("Load", true);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        menu.append_items(&[&open, &quick_save, &load_menu, &PredefinedMenuItem::separator(), &quit]).ok()?;

        let icon = TrayIconBuilder::new()
            .with_tooltip("Save States")
            .with_icon(icon::tray_icon()?)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .ok()?;

        let menu_sink = sink.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| menu_sink.send(AppMsg::Tray(TrayMsg::Menu(e.id)))));
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            let left_click =
                matches!(e, TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. });
            if left_click {
                sink.send(AppMsg::Tray(TrayMsg::Activate));
            }
        }));

        Some(Self {
            icon,
            load_menu,
            open: open.id().clone(),
            quick_save: quick_save.id().clone(),
            quit: quit.id().clone(),
            load_items: Vec::new(),
        })
    }

    pub fn action_for(&self, id: &MenuId) -> Option<TrayAction> {
        if *id == self.open {
            Some(TrayAction::Open)
        } else if *id == self.quick_save {
            Some(TrayAction::QuickSave)
        } else if *id == self.quit {
            Some(TrayAction::Quit)
        } else {
            self.load_items.iter().find(|(m, _)| m == id).map(|(_, s)| TrayAction::Load(*s))
        }
    }

    /// Rebuild the Load submenu: favorites first, then the most recent states.
    pub fn set_states(&mut self, states: &[(Uuid, String, bool)]) {
        while self.load_menu.remove_at(0).is_some() {}
        self.load_items.clear();
        if states.is_empty() {
            let _ = self.load_menu.append(&MenuItem::new("(no saved states)", false, None));
            return;
        }
        for (id, name, favorite) in states.iter().take(15) {
            let label = if *favorite { format!("\u{2605} {name}") } else { name.clone() };
            let item = MenuItem::new(label, true, None);
            self.load_items.push((item.id().clone(), *id));
            let _ = self.load_menu.append(&item);
        }
    }

    pub fn set_tooltip(&self, text: &str) {
        let _ = self.icon.set_tooltip(Some(text));
    }
}
