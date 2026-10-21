//! End-to-end test of the Chrome path without Chrome: this test plays the extension on
//! the bridge's stdio, runs the app's real pipe hub, and checks every hop.

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use savestates_core::chrome::BridgeHub;
use serde_json::{Value, json};

fn write(to: &mut ChildStdin, msg: Value) {
    let bytes = serde_json::to_vec(&msg).unwrap();
    to.write_all(&(bytes.len() as u32).to_le_bytes()).unwrap();
    to.write_all(&bytes).unwrap();
    to.flush().unwrap();
}

/// Next message from the bridge to "the extension", skipping keepalive pings.
fn read(from: &mut ChildStdout) -> Value {
    loop {
        let mut len = [0u8; 4];
        from.read_exact(&mut len).unwrap();
        let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
        from.read_exact(&mut buf).unwrap();
        let msg: Value = serde_json::from_slice(&buf).unwrap();
        if msg["type"] != "ping" {
            return msg;
        }
    }
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_exit(child: &mut Child) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status.success();
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    false
}

fn snapshot(req: Option<&Value>, urls: &[&str]) -> Value {
    let tabs: Vec<Value> = urls
        .iter()
        .enumerate()
        .map(|(i, u)| json!({"id": 10 + i, "index": i, "url": u, "title": u, "active": i == 0, "groupId": -1}))
        .collect();
    let mut msg = json!({
        "type": "snapshot", "instance": "test-instance", "ts": 1.0,
        "windows": [{"id": 1, "type": "normal", "state": "normal", "left": 0, "top": 0, "width": 800, "height": 600, "tabs": tabs}],
        "groups": []
    });
    if let Some(req) = req {
        msg["req"] = req.clone();
    }
    msg
}

#[test]
fn bridge_relays_between_extension_and_app() {
    let pipe = format!(r"\\.\pipe\SaveStates.test.{}", std::process::id());
    let home = std::env::temp_dir().join(format!("savestates-relay-{}", std::process::id()));
    let hub = BridgeHub::start_on(pipe.clone(), |_| {});

    // Chrome launches the host with the caller's origin as the first argument.
    let mut child = Command::new(env!("CARGO_BIN_EXE_savestates-bridge"))
        .arg("chrome-extension://jccabdhhkjoagelckkicmglgpfglcofc/")
        .env("SAVESTATES_PIPE", &pipe)
        .env("SAVESTATES_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawning the bridge");
    let mut to_bridge = child.stdin.take().unwrap();
    let mut from_bridge = child.stdout.take().unwrap();

    // The extension says hello and pushes its first snapshot.
    write(&mut to_bridge, json!({"type": "hello", "instance": "test-instance", "extVersion": "0.1.0"}));
    write(&mut to_bridge, snapshot(None, &["https://example.com/"]));

    let cached = home.join("local").join("live").join("test-instance.json");
    wait_until("snapshot cached on disk", || cached.exists());
    wait_until("bridge connected to the app", || hub.bridges().iter().any(|b| b.instance == "test-instance"));

    // App asks for a fresh snapshot: app -> bridge -> extension -> back.
    let asking = hub.clone();
    let asker = thread::spawn(move || asking.fresh_snapshots(Duration::from_secs(5)));
    let request = read(&mut from_bridge);
    assert_eq!((request["type"].as_str(), request["op"].as_str()), (Some("request"), Some("snapshot")));
    write(&mut to_bridge, snapshot(Some(&request["req"]), &["https://example.com/", "https://rust-lang.org/"]));
    let snaps = asker.join().unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].windows[0].tabs.len(), 2, "the fresh snapshot, not the cached one");

    // App asks the extension to apply pins/groups to a restored window.
    let applying = hub.clone();
    let applier = thread::spawn(move || {
        applying.request(
            "test-instance",
            json!({"op": "apply", "spec": {"urls": ["https://example.com"]}}),
            Duration::from_secs(5),
        )
    });
    let request = read(&mut from_bridge);
    assert_eq!(request["op"], "apply");
    assert_eq!(request["spec"]["urls"][0], "https://example.com");
    write(&mut to_bridge, json!({"type": "result", "req": request["req"], "ok": true}));
    assert_eq!(applier.join().unwrap().expect("apply answered")["ok"], true);

    // Chrome closing the port (stdin) must end the bridge cleanly.
    drop(to_bridge);
    assert!(wait_for_exit(&mut child), "bridge did not exit after stdin closed");

    let _ = std::fs::remove_dir_all(&home);
}
