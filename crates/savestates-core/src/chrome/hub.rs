//! The app side of the Chrome bridge: a named-pipe server that each
//! `savestates-bridge` process (one per Chrome profile, spawned by Chrome) connects to.
//!
//! Protocol on each connection: the bridge sends one `hello` frame, then the app drives
//! strict request/response pairs. Strict alternation matters: these are synchronous
//! pipe handles, and Windows serializes synchronous I/O per handle, so a blocking read
//! on one thread would stall a write from another. One thread per connection, never
//! reading and writing at the same time, sidesteps that entirely.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use super::framing::{read_json, write_json};
use super::snapshot::ExtSnapshot;
use crate::win::{pcwstr, wide};

pub const PIPE_NAME: &str = r"\\.\pipe\SaveStates.v1";
const PING_EVERY: Duration = Duration::from_secs(15);

/// The pipe both sides use: [`PIPE_NAME`], unless `SAVESTATES_PIPE` overrides it so
/// tests never talk to a real running instance.
pub fn pipe_name() -> String {
    std::env::var("SAVESTATES_PIPE").unwrap_or_else(|_| PIPE_NAME.to_string())
}

#[derive(Debug, Clone)]
pub struct BridgeInfo {
    pub conn_id: u64,
    pub instance: String,
    pub ext_version: String,
    pub connected_at: Instant,
}

struct Job {
    request: Value,
    reply: mpsc::Sender<io::Result<Value>>,
}

struct Conn {
    info: BridgeInfo,
    jobs: mpsc::Sender<Job>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubEvent {
    /// A bridge connected or dropped.
    BridgesChanged,
    /// A second launch of the app asked this (already running) instance to show itself.
    Activate,
    /// `savestates-app --quit` asked this instance to exit.
    Quit,
}

type OnEvent = Arc<dyn Fn(HubEvent) + Send + Sync>;

#[derive(Clone)]
pub struct BridgeHub {
    conns: Arc<Mutex<HashMap<u64, Conn>>>,
    on_event: OnEvent,
}

/// Ask the running instance to show its window. False if none is listening.
pub fn activate_running_instance() -> bool {
    signal_running_instance("activate")
}

/// Ask the running instance to exit. False if none is listening.
pub fn quit_running_instance() -> bool {
    signal_running_instance("quit")
}

fn signal_running_instance(kind: &str) -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe_name())
        .and_then(|mut pipe| write_json(&mut pipe, &json!({"type": kind})))
        .is_ok()
}

impl BridgeHub {
    /// Start serving the pipe. `on_event` runs on a hub thread; keep it short.
    pub fn start(on_event: impl Fn(HubEvent) + Send + Sync + 'static) -> Self {
        Self::start_on(pipe_name(), on_event)
    }

    /// [`Self::start`] on an explicit pipe name.
    pub fn start_on(pipe: String, on_event: impl Fn(HubEvent) + Send + Sync + 'static) -> Self {
        let hub = Self { conns: Arc::default(), on_event: Arc::new(on_event) };
        let acceptor = hub.clone();
        thread::Builder::new()
            .name("bridge-accept".into())
            .spawn(move || acceptor.accept_loop(&pipe))
            .expect("spawning the bridge acceptor thread");
        hub
    }

    pub fn bridges(&self) -> Vec<BridgeInfo> {
        let mut v: Vec<_> = self.conns.lock().unwrap().values().map(|c| c.info.clone()).collect();
        v.sort_by_key(|b| b.conn_id);
        v
    }

    /// Ask every connected extension for a fresh snapshot, in parallel.
    pub fn fresh_snapshots(&self, timeout: Duration) -> Vec<ExtSnapshot> {
        let pending: Vec<_> =
            self.bridges().into_iter().filter_map(|b| self.send(b.conn_id, json!({"op": "snapshot"}))).collect();
        let deadline = Instant::now() + timeout;
        pending
            .into_iter()
            .filter_map(|rx| rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok()?.ok())
            .filter_map(|v| serde_json::from_value::<ExtSnapshot>(v).ok())
            .filter(|s| !s.instance.is_empty())
            .collect()
    }

    /// Send a request to the bridge of a specific extension instance.
    pub fn request(&self, instance: &str, request: Value, timeout: Duration) -> Option<Value> {
        let conn_id = self.bridges().into_iter().rev().find(|b| b.instance == instance)?.conn_id;
        self.send(conn_id, request)?.recv_timeout(timeout).ok()?.ok()
    }

    fn send(&self, conn_id: u64, request: Value) -> Option<mpsc::Receiver<io::Result<Value>>> {
        let (tx, rx) = mpsc::channel();
        let conns = self.conns.lock().unwrap();
        conns.get(&conn_id)?.jobs.send(Job { request, reply: tx }).ok()?;
        Some(rx)
    }

    fn accept_loop(self, pipe: &str) {
        let name = wide(pipe);
        loop {
            let pipe = unsafe {
                CreateNamedPipeW(
                    pcwstr(&name),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    PIPE_UNLIMITED_INSTANCES,
                    64 * 1024,
                    64 * 1024,
                    0,
                    None,
                )
            };
            if pipe.is_invalid() {
                warn!("CreateNamedPipeW failed: {}", io::Error::last_os_error());
                thread::sleep(Duration::from_secs(2));
                continue;
            }
            let connected = match unsafe { ConnectNamedPipe(pipe, None) } {
                Ok(()) => true,
                // The client connected between CreateNamedPipe and ConnectNamedPipe.
                Err(e) => e.code() == ERROR_PIPE_CONNECTED.to_hresult(),
            };
            if !connected {
                unsafe {
                    let _ = CloseHandle(pipe);
                }
                continue;
            }
            // The File owns the handle from here and closes it on drop.
            let file = unsafe { File::from_raw_handle(pipe.0 as RawHandle) };
            let hub = self.clone();
            let _ = thread::Builder::new().name("bridge-conn".into()).spawn(move || hub.serve(file));
        }
    }

    fn serve(self, mut pipe: File) {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let hello = match read_json(&mut pipe) {
            Ok(v) if v["type"] == "hello" => v,
            Ok(v) if v["type"] == "activate" => return (self.on_event)(HubEvent::Activate),
            Ok(v) if v["type"] == "quit" => return (self.on_event)(HubEvent::Quit),
            Ok(v) => return debug!("pipe client sent {v} instead of hello; dropping"),
            Err(e) => return debug!("bridge hello failed: {e}"),
        };
        let s = |k: &str| hello[k].as_str().unwrap_or_default().to_string();
        let info = BridgeInfo {
            conn_id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            instance: s("instance"),
            ext_version: s("extVersion"),
            connected_at: Instant::now(),
        };
        info!(instance = %info.instance, "Chrome bridge connected");
        let (tx, rx) = mpsc::channel::<Job>();
        self.conns.lock().unwrap().insert(info.conn_id, Conn { info: info.clone(), jobs: tx });
        (self.on_event)(HubEvent::BridgesChanged);

        loop {
            match rx.recv_timeout(PING_EVERY) {
                Ok(job) => {
                    let result = round_trip(&mut pipe, &job.request);
                    let broken = result.is_err();
                    let _ = job.reply.send(result);
                    if broken {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if round_trip(&mut pipe, &json!({"op": "ping"})).is_err() {
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        self.conns.lock().unwrap().remove(&info.conn_id);
        info!(instance = %info.instance, "Chrome bridge disconnected");
        (self.on_event)(HubEvent::BridgesChanged);
    }
}

fn round_trip(pipe: &mut File, request: &Value) -> io::Result<Value> {
    write_json(pipe, request)?;
    read_json(pipe)
}
