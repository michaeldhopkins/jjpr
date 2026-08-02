//! End-to-end: a second `jjpr watch` on a repo where one is already running
//! exits instead of piling on. Drives the real binary; the heartbeat check runs
//! before any forge interaction, so no network/forge is needed.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tempfile::TempDir;

mod common;
use common::jj_available;

#[test]
fn second_watch_exits_when_one_is_already_running() {
    if !jj_available() {
        return;
    }
    let dir = TempDir::new().unwrap();
    assert!(
        Command::new("jj").args(["git", "init"]).current_dir(dir.path()).output().unwrap().status.success(),
        "jj git init failed"
    );
    // Simulate a live watcher: a fresh heartbeat next to jjpr's repo metadata.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    std::fs::write(
        dir.path().join(".jj").join("jjpr-watch.json"),
        format!(r#"{{"pid":999999,"started_at":{now},"last_seen":{now}}}"#),
    )
    .unwrap();

    // A second watcher. The bookmark need not exist — the guard fires first.
    // Spawn with a timeout: the guard must exit promptly, and if a future change
    // ever let this reach the poll loop, the test kills it instead of hanging CI.
    let mut child = Command::new(env!("CARGO_BIN_EXE_jjpr"))
        .args(["watch", "feature"])
        .current_dir(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jjpr");

    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("jjpr watch did not exit — the single-watcher guard did not fire");
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let mut stdout = String::new();
    child.stdout.take().unwrap().read_to_string(&mut stdout).unwrap();
    assert!(
        stdout.contains("jjpr watch is already running on this repo in another window"),
        "expected the already-running message; stdout={stdout:?}"
    );
    assert!(status.success(), "should exit cleanly");
    // The pre-existing heartbeat must be left intact (we didn't claim it).
    assert!(dir.path().join(".jj").join("jjpr-watch.json").exists());
}
