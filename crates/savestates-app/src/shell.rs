//! Application state that outlives any one window.
//!
//! The window only exists while it is on screen. Closing it ends eframe's run loop and
//! drops the window together with its GPU context, so Save States sitting in the tray
//! holds no GPU resources and little memory. Everything long-lived (the store, the
//! worker thread, the Chrome bridge hub, the tray icon, status text, a dialog queued
//! for the next window) lives here on the main thread and is borrowed by each window.
//!
//! While no window is open the main thread runs [`win::pump_until_woken`], so the
//! tray icon keeps working; anything that sends an [`AppMsg`] also wakes that pump.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Local;
use eframe::egui;
use savestates_core::chrome::{BridgeHub, HubEvent, InstanceMap, profiles};
use savestates_core::model::CaptureScope;
use savestates_core::paths::AppPaths;
use savestates_core::restore::RestorePlan;
use savestates_core::store::{SharedStore, Store};
use savestates_core::win;
use tracing::warn;
use uuid::Uuid;

use crate::app::Dialog;
use crate::app::chrome_setup::SetupView;
use crate::tray::{Tray, TrayAction, TrayMsg};
use crate::worker::{Command, Event, Worker};

/// How long to wait after closing our window before taking the screenshot.
const SCREENSHOT_SETTLE: Duration = Duration::from_millis(400);

pub enum AppMsg {
    Tray(TrayMsg),
    Hub(HubEvent),
    Worker(Event),
}

/// Wakes the main thread: the egui loop while a window is open, otherwise the
/// hidden-mode message pump.
pub struct Waker {
    ctx: Mutex<Option<egui::Context>>,
    main_thread: u32,
}

impl Waker {
    fn new() -> Arc<Self> {
        Arc::new(Self { ctx: Mutex::new(None), main_thread: win::current_thread_id() })
    }

    pub fn attach(&self, ctx: &egui::Context) {
        *self.ctx.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctx.clone());
    }

    pub fn detach(&self) {
        *self.ctx.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn wake(&self) {
        match &*self.ctx.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(ctx) => ctx.request_repaint(),
            None => win::post_wake(self.main_thread),
        }
    }
}

/// How any thread talks to the main thread: deliver a message, then wake it.
#[derive(Clone)]
pub struct Sink {
    tx: Sender<AppMsg>,
    waker: Arc<Waker>,
}

impl Sink {
    pub fn send(&self, msg: AppMsg) {
        let _ = self.tx.send(msg);
        self.waker.wake();
    }
}

/// What the window, if one is open, should do about a message.
pub enum Reaction {
    Nothing,
    Show,
    /// Close the window, e.g. so it isn't in the screenshot.
    Hide,
    Dialog(Dialog),
    Planned(Box<RestorePlan>),
    Quit,
}

pub enum Hidden {
    Show,
    Quit,
}

pub struct Shell {
    pub paths: AppPaths,
    pub store: SharedStore,
    pub worker: Worker,
    pub hub: BridgeHub,
    pub waker: Arc<Waker>,
    tray: Option<Tray>,
    rx: Receiver<AppMsg>,

    pub busy: Option<String>,
    pub progress: Option<(usize, usize)>,
    pub notice: Option<(String, Instant)>,
    pub highlight: Option<Uuid>,
    /// Thumbnails that changed on disk since a window last looked.
    pub thumbs_dirty: Vec<Uuid>,
    /// A dialog the next window opens with.
    pub pending_dialog: Option<Dialog>,
    /// Window size when it last closed, so it reopens the same size.
    pub last_size: Option<egui::Vec2>,
    pub quitting: bool,
    reveal_after_capture: bool,
    tray_generation: Option<u64>,
}

impl Shell {
    pub fn start(paths: AppPaths, store: Store, warning: Option<String>) -> Self {
        let first_run = store.states().is_empty();
        let store = store.into_shared();
        let waker = Waker::new();
        let (tx, rx) = mpsc::channel();
        let sink = Sink { tx, waker: waker.clone() };

        let hub_sink = sink.clone();
        let hub = BridgeHub::start(move |e| hub_sink.send(AppMsg::Hub(e)));
        let worker = Worker::spawn(paths.clone(), store.clone(), hub.clone(), sink.clone());
        let tray = Tray::new(sink);
        if tray.is_none() {
            warn!("could not create the tray icon; closing the window will quit");
        }

        let pending_dialog = if let Some(body) = warning {
            Some(Dialog::Message { title: "Save States".into(), body })
        } else if first_run
            && profiles::find_chrome_exe().is_some()
            && InstanceMap::load(&paths.chrome_instances_file()).profiles.is_empty()
        {
            Some(Dialog::ChromeSetup(Box::new(SetupView::new(&paths, &hub))))
        } else {
            None
        };

        Self {
            paths,
            store,
            worker,
            hub,
            waker,
            tray,
            rx,
            busy: None,
            progress: None,
            notice: None,
            highlight: None,
            thumbs_dirty: Vec::new(),
            pending_dialog,
            last_size: None,
            quitting: false,
            reveal_after_capture: false,
            tray_generation: None,
        }
    }

    pub fn has_tray(&self) -> bool {
        self.tray.is_some()
    }

    pub fn try_recv(&self) -> Option<AppMsg> {
        self.rx.try_recv().ok()
    }

    /// Wait, without a window, until something needs one (or the app should quit).
    pub fn wait_hidden(&mut self) -> Hidden {
        self.waker.detach();
        loop {
            while let Ok(msg) = self.rx.try_recv() {
                match self.handle(msg) {
                    Reaction::Nothing | Reaction::Hide | Reaction::Planned(_) => {}
                    Reaction::Show => return Hidden::Show,
                    Reaction::Dialog(d) => {
                        self.pending_dialog = Some(d);
                        return Hidden::Show;
                    }
                    Reaction::Quit => {
                        self.quitting = true;
                        return Hidden::Quit;
                    }
                }
            }
            self.sync_tray();
            win::pump_until_woken();
        }
    }

    pub fn handle(&mut self, msg: AppMsg) -> Reaction {
        match msg {
            AppMsg::Tray(t) => self.on_tray(t),
            AppMsg::Hub(HubEvent::Activate) => Reaction::Show,
            AppMsg::Hub(HubEvent::Quit) => Reaction::Quit,
            AppMsg::Hub(HubEvent::BridgesChanged) => {
                self.worker.send(Command::LinkChrome);
                Reaction::Nothing
            }
            AppMsg::Worker(e) => self.on_worker(e),
        }
    }

    fn on_tray(&mut self, msg: TrayMsg) -> Reaction {
        let action = match msg {
            TrayMsg::Activate => Some(TrayAction::Open),
            TrayMsg::Menu(id) => self.tray.as_ref().and_then(|t| t.action_for(&id)),
        };
        match action {
            Some(TrayAction::Open) => Reaction::Show,
            Some(TrayAction::QuickSave) => {
                if self.busy.is_some() {
                    return Reaction::Show;
                }
                let scope = self.default_scope().unwrap_or_default();
                self.start_capture(default_state_name(), scope, None, false);
                Reaction::Hide
            }
            Some(TrayAction::Load(id)) => self.begin_load(id),
            Some(TrayAction::Quit) => Reaction::Quit,
            None => Reaction::Nothing,
        }
    }

    fn on_worker(&mut self, event: Event) -> Reaction {
        match event {
            Event::Captured { id, name, replaced, chrome_without_tabs } => {
                self.busy = None;
                self.highlight = Some(id);
                self.thumbs_dirty.push(id);
                let verb = if replaced { "Updated" } else { "Saved" };
                if chrome_without_tabs > 0 {
                    self.notify(format!(
                        "{verb} \"{name}\". Tabs of {chrome_without_tabs} Chrome window(s) were not captured: \
                         set up Chrome integration."
                    ));
                } else {
                    self.notify(format!("{verb} \"{name}\""));
                }
                if std::mem::take(&mut self.reveal_after_capture) { Reaction::Show } else { Reaction::Nothing }
            }
            Event::Planned(plan) => Reaction::Planned(plan),
            Event::Progress { done, total, label } => {
                self.progress = Some((done, total));
                self.busy = Some(format!("Restoring: {label}"));
                if let Some(t) = &self.tray {
                    t.set_tooltip(&format!("Save States - restoring {done}/{total}"));
                }
                Reaction::Nothing
            }
            Event::Restored { name, report } => {
                self.busy = None;
                self.progress = None;
                self.notify(format!("Switched to \"{name}\""));
                let problems = !report.failed.is_empty()
                    || !report.no_window.is_empty()
                    || !report.declined_close.is_empty()
                    || !report.notes.is_empty();
                if problems { Reaction::Dialog(Dialog::Report { name, report }) } else { Reaction::Nothing }
            }
            Event::ThumbnailChanged(id) => {
                self.thumbs_dirty.push(id);
                Reaction::Nothing
            }
            Event::Failed(body) => {
                self.busy = None;
                self.progress = None;
                self.reveal_after_capture = false;
                Reaction::Dialog(Dialog::Message { title: "Something went wrong".into(), body })
            }
        }
    }

    /// Start a capture. The caller closes the window first, so the screenshot shows
    /// the desktop rather than Save States; `reveal` reopens it afterwards.
    pub fn start_capture(&mut self, name: String, scope: CaptureScope, replace: Option<Uuid>, reveal: bool) {
        self.busy = Some(format!("Saving \"{name}\"..."));
        self.reveal_after_capture = reveal;
        self.worker.send(Command::Capture { name, scope, replace, settle: SCREENSHOT_SETTLE });
    }

    /// Ask the worker what loading `id` would do; the answer opens the confirmation.
    pub fn begin_load(&mut self, id: Uuid) -> Reaction {
        if self.busy.is_some() {
            return Reaction::Show;
        }
        let name = self.store.lock().unwrap_or_else(|e| e.into_inner()).get(id).map(|s| s.name.clone());
        match name {
            Some(name) => {
                self.worker.send(Command::Plan { id });
                Reaction::Dialog(Dialog::Planning { name })
            }
            None => Reaction::Nothing,
        }
    }

    pub fn start_restore(&mut self, plan: Box<RestorePlan>, close_extras: bool) {
        self.busy = Some(format!("Switching to \"{}\"...", plan.state_name));
        self.progress = None;
        self.worker.send(Command::Restore { plan, close_extras });
    }

    pub fn notify(&mut self, text: impl Into<String>) {
        let text = text.into();
        if let Some(t) = &self.tray {
            t.set_tooltip(&format!("Save States - {text}"));
        }
        self.notice = Some((text, Instant::now()));
    }

    pub fn default_scope(&self) -> Option<CaptureScope> {
        self.store.lock().unwrap_or_else(|e| e.into_inner()).settings().default_scope
    }

    /// Keep the tray's Load submenu in step with the store.
    pub fn sync_tray(&mut self) {
        let Some(tray) = &mut self.tray else { return };
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if self.tray_generation == Some(store.generation()) {
            return;
        }
        let mut states: Vec<_> =
            store.states().iter().map(|s| (s.id, s.name.clone(), s.favorite, s.updated_at)).collect();
        self.tray_generation = Some(store.generation());
        drop(store);
        states.sort_by(|a, b| b.2.cmp(&a.2).then(b.3.cmp(&a.3)));
        tray.set_states(&states.into_iter().map(|(id, name, fav, _)| (id, name, fav)).collect::<Vec<_>>());
    }
}

pub fn default_state_name() -> String {
    format!("Session {}", Local::now().format("%b %-d, %-I:%M %p"))
}
