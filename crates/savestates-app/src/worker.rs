//! The background worker. Everything slow (capture, restore, image work) runs on this
//! one thread, which also owns COM and the virtual desktop objects - `winvd` caches
//! its COM proxies per thread, so all desktop calls must come from here.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use savestates_core::capture::{self, CaptureContext};
use savestates_core::chrome::BridgeHub;
use savestates_core::model::{CaptureScope, Thumbnail};
use savestates_core::paths::AppPaths;
use savestates_core::restore::{self, RestorePlan, RestoreReport};
use savestates_core::screenshot;
use savestates_core::store::SharedStore;
use savestates_core::win::ComApartment;
use tracing::{error, info};
use uuid::Uuid;

use crate::shell::{AppMsg, Sink};

pub enum Command {
    Capture { name: String, scope: CaptureScope, replace: Option<Uuid>, settle: Duration },
    Plan { id: Uuid },
    Restore { plan: Box<RestorePlan>, close_extras: bool },
    SetThumbnail { id: Uuid, source: PathBuf },
    ResetThumbnail { id: Uuid },
    LinkChrome,
}

pub enum Event {
    Captured { id: Uuid, name: String, replaced: bool, chrome_without_tabs: usize },
    Planned(Box<RestorePlan>),
    Progress { done: usize, total: usize, label: String },
    Restored { name: String, report: RestoreReport },
    ThumbnailChanged(Uuid),
    Failed(String),
}

pub struct Worker {
    tx: Sender<Command>,
}

impl Worker {
    pub fn spawn(paths: AppPaths, store: SharedStore, hub: BridgeHub, sink: Sink) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("worker".into())
            .spawn(move || run(rx, paths, store, hub, sink))
            .expect("spawning the worker thread");
        Self { tx }
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }
}

fn run(rx: Receiver<Command>, paths: AppPaths, store: SharedStore, hub: BridgeHub, sink: Sink) {
    let _com = ComApartment::init();
    let emit = |e: Event| sink.send(AppMsg::Worker(e));
    for cmd in rx {
        let job = AssertUnwindSafe(|| handle(cmd, &paths, &store, &hub, &emit));
        if let Err(panic) = catch_unwind(job) {
            let msg =
                panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned());
            error!("worker panicked: {msg:?}");
            emit(Event::Failed("Something went wrong (details are in the log).".into()));
        }
    }
}

fn handle(cmd: Command, paths: &AppPaths, store: &SharedStore, hub: &BridgeHub, emit: &dyn Fn(Event)) {
    let ctx = CaptureContext { paths, hub: Some(hub) };
    match cmd {
        Command::Capture { name, scope, replace, settle } => {
            // Give the compositor time to remove our window before the screenshot.
            thread::sleep(settle);
            match capture::capture(name, scope, &ctx) {
                Ok(mut state) => {
                    let chrome_without_tabs = state
                        .known_items()
                        .filter(|i| matches!(i, savestates_core::model::CapturedItem::Chrome(c) if !c.tabs_captured))
                        .count();
                    let mut lock = store.lock().unwrap_or_else(|e| e.into_inner());
                    let result = match replace.and_then(|id| lock.get(id).cloned()) {
                        Some(old) => {
                            // Keep identity, name, favorite and any custom thumbnail; take
                            // the new contents and screenshot.
                            let thumbs = paths.thumbs_dir();
                            if let Some(new_file) = state.thumbnail.file.take() {
                                let target = format!("{}.jpg", old.id);
                                let _ = std::fs::rename(thumbs.join(&new_file), thumbs.join(&target));
                                state.thumbnail = Thumbnail { file: Some(target), custom: false };
                            }
                            if old.thumbnail.custom {
                                state.thumbnail = old.thumbnail.clone();
                            }
                            state.id = old.id;
                            state.name = old.name.clone();
                            state.favorite = old.favorite;
                            state.created_at = old.created_at;
                            state.extra = old.extra.clone();
                            lock.replace(state.clone()).map(|_| true)
                        }
                        None => lock.insert(state.clone()).map(|_| false),
                    };
                    drop(lock);
                    match result {
                        Ok(replaced) => {
                            info!(id = %state.id, items = state.items.len(), replaced, "state saved");
                            emit(Event::Captured { id: state.id, name: state.name, replaced, chrome_without_tabs });
                        }
                        Err(e) => emit(Event::Failed(format!("Saving failed: {e:#}"))),
                    }
                }
                Err(e) => emit(Event::Failed(format!("Capture failed: {e:#}"))),
            }
        }
        Command::Plan { id } => {
            let state = store.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned();
            match state {
                Some(state) => {
                    let live = capture::collect(CaptureScope::AllDesktops, &ctx);
                    emit(Event::Planned(Box::new(restore::plan(&state, &live))));
                }
                None => emit(Event::Failed("That save state no longer exists.".into())),
            }
        }
        Command::Restore { plan, close_extras } => {
            let report = restore::execute(&plan, close_extras, &ctx, &mut |done, total, label| {
                emit(Event::Progress { done, total, label: label.to_string() });
            });
            emit(Event::Restored { name: plan.state_name.clone(), report });
        }
        Command::SetThumbnail { id, source } => {
            let file = format!("{id}-custom.jpg");
            match screenshot::import_thumbnail(&source, &paths.thumbs_dir().join(&file)) {
                Ok(()) => {
                    let r = store
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .update(id, |s| s.thumbnail = Thumbnail { file: Some(file), custom: true });
                    match r {
                        Ok(_) => emit(Event::ThumbnailChanged(id)),
                        Err(e) => emit(Event::Failed(format!("Saving failed: {e:#}"))),
                    }
                }
                Err(e) => emit(Event::Failed(format!("Could not use that image: {e:#}"))),
            }
        }
        Command::ResetThumbnail { id } => {
            let thumbs = paths.thumbs_dir();
            let screenshot = format!("{id}.jpg");
            if !thumbs.join(&screenshot).exists() {
                emit(Event::Failed("This state has no screenshot to go back to.".into()));
                return;
            }
            let r = store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .update(id, |s| s.thumbnail = Thumbnail { file: Some(screenshot), custom: false });
            let _ = std::fs::remove_file(thumbs.join(format!("{id}-custom.jpg")));
            match r {
                Ok(_) => emit(Event::ThumbnailChanged(id)),
                Err(e) => emit(Event::Failed(format!("Saving failed: {e:#}"))),
            }
        }
        Command::LinkChrome => capture::link_chrome_instances(&ctx),
    }
}
