//! Native-messaging host for the Save States Chrome extension.
//!
//! Chrome starts one of these per profile when the extension connects, talking over
//! stdin/stdout with 4-byte-length-prefixed JSON. The bridge:
//! - caches the extension's latest tab snapshot to `%LOCALAPPDATA%\SaveStates\live\`,
//!   so the app has data even when it starts after Chrome;
//! - connects to the app's named pipe (when the app is running) and serves its
//!   requests by relaying them to the extension;
//! - pings the extension periodically so its service worker stays awake.
//!
//! It exits as soon as Chrome closes stdin. stdout belongs to Chrome: never print.

#![windows_subsystem = "windows"]

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

const PIPE_NAME: &str = r"\\.\pipe\SaveStates.v1";
const MAX_FRAME: u32 = 64 * 1024 * 1024;
const PING_EVERY: Duration = Duration::from_secs(20);
const RECONNECT_EVERY: Duration = Duration::from_secs(3);

#[derive(Default)]
struct Shared {
    hello: Option<Value>,
    latest: Option<Value>,
    next_req: u64,
    pending: HashMap<u64, Sender<Value>>,
}

type State = Arc<Mutex<Shared>>;
type Out = Arc<Mutex<io::Stdout>>;

/// Must match `AppPaths::resolve` and `hub::pipe_name` in savestates-core, including the
/// test overrides.
fn local_dir() -> PathBuf {
    match std::env::var_os("SAVESTATES_HOME") {
        Some(home) => PathBuf::from(home).join("local"),
        None => std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default().join("SaveStates"),
    }
}

fn pipe_name() -> String {
    std::env::var("SAVESTATES_PIPE").unwrap_or_else(|_| PIPE_NAME.to_string())
}

fn main() {
    let base = local_dir();
    let log = Log::open(base.join("logs").join("bridge.log"));
    log.line(&format!("started, args {:?}", std::env::args().skip(1).collect::<Vec<_>>()));

    let state: State = Arc::default();
    let out: Out = Arc::new(Mutex::new(io::stdout()));

    {
        let out = out.clone();
        thread::spawn(move || {
            loop {
                thread::sleep(PING_EVERY);
                if send_to_extension(&out, &json!({"type": "ping"})).is_err() {
                    return;
                }
            }
        });
    }
    {
        let (state, out, log) = (state.clone(), out.clone(), log.clone());
        thread::spawn(move || serve_app(state, out, log));
    }

    // The main thread owns stdin. When Chrome closes it, we are done.
    let live_dir = base.join("live");
    let mut stdin = io::stdin().lock();
    while let Ok(msg) = read_frame(&mut stdin).and_then(parse) {
        on_extension_message(msg, &state, &live_dir, &log);
    }
    log.line("stdin closed; exiting");
}

fn on_extension_message(msg: Value, state: &State, live_dir: &std::path::Path, log: &Log) {
    match msg["type"].as_str() {
        Some("hello") => {
            log.line(&format!("extension hello: {}", msg["instance"]));
            state.lock().unwrap().hello = Some(msg);
        }
        Some("snapshot") => {
            if let Some(instance) = msg["instance"].as_str().filter(|i| is_safe_file_stem(i))
                && let Err(e) = write_atomic(&live_dir.join(format!("{instance}.json")), &msg)
            {
                log.line(&format!("caching snapshot failed: {e}"));
            }
            let mut s = state.lock().unwrap();
            if let Some(tx) = msg["req"].as_u64().and_then(|r| s.pending.remove(&r)) {
                let _ = tx.send(msg.clone());
            }
            s.latest = Some(msg);
        }
        Some("result") => {
            let tx = msg["req"].as_u64().and_then(|r| state.lock().unwrap().pending.remove(&r));
            if let Some(tx) = tx {
                let _ = tx.send(msg);
            }
        }
        _ => {} // pong and anything newer than us
    }
}

/// Keep a connection to the app open whenever the app is running.
fn serve_app(state: State, out: Out, log: Log) {
    loop {
        // The app identifies us by the extension's hello, so wait for it first.
        let hello = state.lock().unwrap().hello.clone();
        let Some(hello) = hello else {
            thread::sleep(Duration::from_millis(200));
            continue;
        };
        let mut pipe = match OpenOptions::new().read(true).write(true).open(pipe_name()) {
            Ok(p) => p,
            Err(_) => {
                thread::sleep(RECONNECT_EVERY); // app not running (or all instances busy)
                continue;
            }
        };
        log.line("connected to app");
        let mut hello = hello;
        hello["bridgePid"] = json!(std::process::id());
        if write_frame(&mut pipe, &hello).is_ok() {
            // Strict request/response: read one request, write one response.
            while let Ok(request) = read_frame(&mut pipe).and_then(parse) {
                let response = handle_app_request(&request, &state, &out);
                if write_frame(&mut pipe, &response).is_err() {
                    break;
                }
            }
        }
        log.line("app connection closed");
        thread::sleep(RECONNECT_EVERY);
    }
}

fn handle_app_request(request: &Value, state: &State, out: &Out) -> Value {
    match request["op"].as_str() {
        Some("ping") => json!({"ok": true}),
        Some("snapshot") => ask_extension(state, out, json!({"op": "snapshot"}), Duration::from_secs(2))
            .or_else(|| state.lock().unwrap().latest.clone())
            .unwrap_or_else(|| json!({"ok": false, "error": "no snapshot yet"})),
        Some("apply") => {
            ask_extension(state, out, json!({"op": "apply", "spec": request["spec"]}), Duration::from_secs(9))
                .unwrap_or_else(|| json!({"ok": false, "error": "extension did not answer"}))
        }
        _ => json!({"ok": false, "error": "unknown op"}),
    }
}

/// Send a request to the extension and wait for the message carrying the same `req`.
fn ask_extension(state: &State, out: &Out, mut request: Value, timeout: Duration) -> Option<Value> {
    let (tx, rx) = mpsc::channel();
    let req = {
        let mut s = state.lock().unwrap();
        s.next_req += 1;
        let req = s.next_req;
        s.pending.insert(req, tx);
        req
    };
    request["type"] = json!("request");
    request["req"] = json!(req);
    let reply = send_to_extension(out, &request).ok().and_then(|_| rx.recv_timeout(timeout).ok());
    state.lock().unwrap().pending.remove(&req);
    reply
}

fn send_to_extension(out: &Out, msg: &Value) -> io::Result<()> {
    let mut stdout = out.lock().unwrap();
    write_frame(&mut *stdout, msg)
}

fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn parse(buf: Vec<u8>) -> io::Result<Value> {
    serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn write_frame(w: &mut impl Write, msg: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}

/// Instance ids come from the extension; only accept ones that are safe as file names.
fn is_safe_file_stem(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn write_atomic(path: &std::path::Path, value: &Value) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_vec(value)?)?;
    fs::rename(&tmp, path)
}

/// Append-only log, truncated at startup once it grows past 512 KB.
#[derive(Clone)]
struct Log(Option<Arc<Mutex<File>>>);

impl Log {
    fn open(path: PathBuf) -> Self {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if fs::metadata(&path).is_ok_and(|m| m.len() > 512 * 1024) {
            let _ = fs::remove_file(&path);
        }
        Self(OpenOptions::new().create(true).append(true).open(path).ok().map(|f| Arc::new(Mutex::new(f))))
    }

    fn line(&self, msg: &str) {
        if let Some(f) = &self.0 {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default();
            let _ = writeln!(f.lock().unwrap(), "[{secs}] [pid {}] {msg}", std::process::id());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &json!({"type": "hello"})).unwrap();
        assert_eq!(parse(read_frame(&mut buf.as_slice()).unwrap()).unwrap()["type"], "hello");
    }

    #[test]
    fn instance_ids_cannot_escape_the_live_directory() {
        assert!(is_safe_file_stem("6f1c2d3e-aaaa-4bbb-8ccc-0123456789ab"));
        assert!(!is_safe_file_stem("..\\..\\evil"));
        assert!(!is_safe_file_stem(""));
        assert!(!is_safe_file_stem("a/b"));
    }

    #[test]
    fn snapshot_is_cached_and_answers_a_pending_request() {
        let dir = std::env::temp_dir().join(format!("savestates-bridge-test-{}", std::process::id()));
        let state: State = Arc::default();
        let (tx, rx) = mpsc::channel();
        state.lock().unwrap().pending.insert(7, tx);
        let log = Log(None);
        on_extension_message(
            json!({"type": "snapshot", "instance": "abc-1", "req": 7, "windows": []}),
            &state,
            &dir,
            &log,
        );
        assert_eq!(rx.try_recv().unwrap()["instance"], "abc-1");
        assert!(dir.join("abc-1.json").exists());
        assert!(state.lock().unwrap().latest.is_some());
        let _ = fs::remove_dir_all(dir);
    }
}
