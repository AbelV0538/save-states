//! Hidden command-line entry points for exercising the backend without the UI:
//!
//! ```text
//! savestates-app --debug-windows              every visible window and the filter's verdict
//! savestates-app --debug-capture [--all]      print what a save would capture, as JSON
//! savestates-app --debug-save [name] [--all]  capture and save a state; prints its id
//! savestates-app --debug-list                 list saved states
//! savestates-app --debug-plan <state-id>      print the restore plan for a saved state
//! savestates-app --debug-restore <state-id>   restore a saved state (additive)
//! ```
//!
//! Set `SAVESTATES_HOME` to work against a scratch data folder instead of the real one.

use anyhow::{Context, Result, bail};
use savestates_core::capture::{self, CaptureContext};
use savestates_core::model::CaptureScope;
use savestates_core::paths::AppPaths;
use savestates_core::restore;
use savestates_core::store::Store;
use savestates_core::win::ComApartment;
use serde_json::json;

pub fn run(args: &[String]) -> Result<()> {
    let _com = ComApartment::init();
    let paths = AppPaths::resolve()?;
    paths.ensure()?;
    let ctx = CaptureContext { paths: &paths, hub: None };
    match args.first().map(String::as_str) {
        Some("--debug-windows") => {
            for (title, class, exe, verdict) in savestates_core::win::enumerate::diagnose(std::process::id()) {
                println!("{verdict:<36} {exe:<26} {class:<32} {title}");
            }
        }
        Some("--debug-capture") => {
            let scope = if args.iter().any(|a| a == "--all") {
                CaptureScope::AllDesktops
            } else {
                CaptureScope::CurrentDesktop
            };
            let started = std::time::Instant::now();
            let session = capture::collect(scope, &ctx);
            let items: Vec<_> = session
                .items
                .iter()
                .map(|li| {
                    json!({
                        "label": li.item.label(),
                        "hwnd": format!("{:#x}", li.window.hwnd),
                        "class": li.window.class,
                        "on_current_desktop": li.window.on_current_desktop,
                        "item": li.item,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "scope": scope,
                    "elapsed_ms": started.elapsed().as_millis(),
                    "desktops": session.desktops,
                    "chrome_tabs_known": session.chrome_tabs_known,
                    "chrome_windows_without_tabs": session.chrome_windows_without_tabs,
                    "items": items,
                }))?
            );
        }
        Some("--debug-save") => {
            let name = args.get(1).filter(|a| !a.starts_with("--")).cloned().unwrap_or_else(|| "debug".into());
            let scope = if args.iter().any(|a| a == "--all") {
                CaptureScope::AllDesktops
            } else {
                CaptureScope::CurrentDesktop
            };
            let state = capture::capture(name, scope, &ctx)?;
            let id = state.id;
            let (mut store, _) = Store::open(paths.states_file());
            store.insert(state)?;
            println!("{id}");
        }
        Some("--debug-list") => {
            let (store, _) = Store::open(paths.states_file());
            for s in store.states() {
                println!("{}  {:<30} {:?}", s.id, s.name, s.summary());
            }
        }
        Some(cmd @ ("--debug-plan" | "--debug-restore")) => {
            let id: uuid::Uuid = args.get(1).context("missing state id")?.parse().context("bad state id")?;
            let (store, _) = Store::open(paths.states_file());
            let state = store.get(id).context("no such state")?.clone();
            let live = capture::collect(CaptureScope::AllDesktops, &ctx);
            let plan = restore::plan(&state, &live);
            if cmd == "--debug-plan" {
                println!("launch:");
                for u in &plan.launches {
                    println!("  {}  ({} window(s) to place)", u.label, u.windows.len());
                }
                println!("already open: {:#?}", plan.already_open);
                println!("not in state (close candidates):");
                for e in &plan.extras {
                    println!("  {}", e.label);
                }
            } else {
                let report = restore::execute(&plan, false, &ctx, &mut |done, total, label| {
                    eprintln!("[{done}/{total}] {label}");
                });
                println!("{report:#?}");
            }
        }
        Some(other) => bail!("unknown debug command {other}"),
        None => bail!("no debug command"),
    }
    Ok(())
}
