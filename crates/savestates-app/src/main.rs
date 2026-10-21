// Release builds are a tray app with no console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod debug;
mod icon;
mod logging;
mod shell;
mod tray;
mod worker;

use std::process::ExitCode;

use eframe::egui;
use savestates_core::chrome::{activate_running_instance, quit_running_instance, setup};
use savestates_core::paths::AppPaths;
use savestates_core::store::Store;
use savestates_core::win::{self, SingleInstance};

use crate::shell::{Hidden, Shell};

fn main() -> ExitCode {
    win::enable_per_monitor_dpi_awareness();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a.starts_with("--debug-")) {
        return match debug::run(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::FAILURE
            }
        };
    }
    if args.iter().any(|a| a == "--quit") {
        quit_running_instance();
        return ExitCode::SUCCESS;
    }
    // `--minimized` is for starting with Windows: straight to the tray.
    let mut show_window = !args.iter().any(|a| a == "--minimized");

    let paths = match AppPaths::resolve().and_then(|p| p.ensure().map(|_| p)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Save States cannot start: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    logging::init(&paths.logs_dir());

    let Some(_instance) = SingleInstance::acquire(r"Local\SaveStates.SingleInstance") else {
        // Already running (probably in the tray): bring that one forward instead.
        activate_running_instance();
        return ExitCode::SUCCESS;
    };
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");

    register_chrome_helper(&paths);
    let (store, warning) = Store::open(paths.states_file());
    if let Some(w) = &warning {
        tracing::warn!("{w}");
    }
    let mut shell = Shell::start(paths, store, warning);

    // Alternate between a window (eframe's loop) and the tray (a plain message pump).
    // Each window is created on open and destroyed on close, GPU context included.
    loop {
        if show_window {
            let options = native_options(shell.last_size);
            let result = eframe::run_native(
                "Save States",
                options,
                Box::new(|cc| Ok(Box::new(app::SaveStatesApp::new(cc, &mut shell)))),
            );
            if let Err(e) = result {
                tracing::error!("window failed: {e}");
                return ExitCode::FAILURE;
            }
            // Without a tray icon there is no way back to a closed window, so quit.
            if shell.quitting || !shell.has_tray() {
                break;
            }
            win::trim_working_set();
        }
        match shell.wait_hidden() {
            Hidden::Show => show_window = true,
            Hidden::Quit => break,
        }
    }
    tracing::info!("exiting");
    ExitCode::SUCCESS
}

fn native_options(size: Option<egui::Vec2>) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Save States")
            .with_app_id("SaveStates")
            .with_inner_size(size.unwrap_or(egui::vec2(1040.0, 700.0)))
            .with_min_inner_size([560.0, 380.0])
            .with_icon(icon::window_icon()),
        centered: true,
        ..Default::default()
    }
}

/// Make sure Chrome can find the native-messaging helper. Per-user registry only, so it
/// needs no admin rights, and it self-heals if the app has moved.
fn register_chrome_helper(paths: &AppPaths) {
    if matches!(setup::host_status(), setup::HostStatus::Ready { .. }) {
        return;
    }
    match setup::bridge_exe() {
        Some(bridge) => match setup::register_host(paths, &bridge) {
            Ok(manifest) => tracing::info!(manifest = %manifest.display(), "registered Chrome helper"),
            Err(e) => tracing::warn!("could not register the Chrome helper: {e:#}"),
        },
        None => tracing::warn!("savestates-bridge.exe not found next to the app; Chrome tabs can't be saved"),
    }
}
