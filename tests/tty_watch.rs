//! TTY vs non-TTY behavior of watch's between-poll spinner.
//!
//! The spinner is a live braille animation that rewrites its line in place.
//! It must appear ONLY on a real terminal — never in piped or captured
//! output, where the carriage returns would be garbage and would pollute
//! logs and test assertions.
//!
//! These drive the real `jjpr` binary through the forge-free "waiting for a
//! bookmark" poll (no network, no cleanup): once under a pseudo-terminal (so
//! `is_terminal()` is true) and once piped. Gated behind `JJPR_E2E` to keep
//! the default `cargo test` fast, since each spawns the binary.
#![cfg(unix)]

use std::io::Read;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

mod common;
use common::jj_available;

fn e2e_enabled() -> bool {
    std::env::var("JJPR_E2E").is_ok()
}

fn jjpr_bin() -> &'static str {
    env!("CARGO_BIN_EXE_jjpr")
}

/// A bare jj repo with no bookmark set, so `jjpr watch` falls into the
/// forge-free "waiting for a bookmark in the working copy's ancestry" poll.
fn init_bare_jj_repo() -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    let ok = Command::new("jj")
        .args(["git", "init"])
        .current_dir(dir.path())
        .output()
        .expect("jj git init")
        .status
        .success();
    assert!(ok, "jj git init failed");
    dir
}

fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

/// Drain `reader` (already set non-blocking) until `stop` returns true on the
/// accumulated text, the process ends (EOF/EIO), or the deadline passes.
fn read_until(reader: &mut impl Read, deadline: Instant, stop: impl Fn(&str) -> bool) -> String {
    let mut captured = String::new();
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF: the child is gone
            Ok(n) => {
                captured.push_str(&String::from_utf8_lossy(&buf[..n]));
                if stop(&captured) {
                    break;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            // A dead PTY master reports EIO rather than EOF on some platforms.
            Err(_) => break,
        }
    }
    captured
}

#[test]
fn spinner_shows_on_a_tty() {
    if !e2e_enabled() {
        eprintln!("skip: set JJPR_E2E=1 to run TTY spinner tests");
        return;
    }
    if !jj_available() {
        eprintln!("skip: jj not available");
        return;
    }
    let repo = init_bare_jj_repo();

    // Allocate a pseudo-terminal. The child's stdout is the slave side, so
    // is_terminal() is true and the live spinner renders.
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty failed");

    // Give the child its own dups of the slave for stdout+stderr, then drop the
    // parent's original slave so `master` sees EOF once the child exits.
    let (child_out, child_err) = unsafe { (libc::dup(slave), libc::dup(slave)) };
    unsafe { libc::close(slave) };

    let mut child = Command::new(jjpr_bin())
        .args(["watch", "--timeout", "1"])
        .current_dir(repo.path())
        .stdin(Stdio::null())
        .stdout(unsafe { Stdio::from_raw_fd(child_out) })
        .stderr(unsafe { Stdio::from_raw_fd(child_err) })
        .spawn()
        .expect("spawn jjpr under pty");

    let mut master_file = unsafe { std::fs::File::from_raw_fd(master) };
    set_nonblocking(master);

    // Read until the spinner has advanced through at least two distinct frames,
    // proving it animates in place.
    let captured = read_until(&mut master_file, Instant::now() + Duration::from_secs(20), |s| {
        s.contains('⠋') && s.contains('⠙')
    });

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        captured.contains('⠋') && captured.contains('⠙'),
        "expected an animated spinner (multiple frames) on a TTY; got:\n{captured:?}"
    );
    assert!(
        captured.contains("Waiting..."),
        "spinner line should carry a label; got:\n{captured:?}"
    );
    assert!(
        captured.contains('\r'),
        "spinner must rewrite its line in place (carriage return); got:\n{captured:?}"
    );
}

#[test]
fn spinner_absent_when_piped() {
    if !e2e_enabled() {
        eprintln!("skip: set JJPR_E2E=1 to run TTY spinner tests");
        return;
    }
    if !jj_available() {
        eprintln!("skip: jj not available");
        return;
    }
    let repo = init_bare_jj_repo();

    let mut child = Command::new(jjpr_bin())
        .args(["watch", "--timeout", "1"])
        .current_dir(repo.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn jjpr piped");

    let mut out = child.stdout.take().expect("piped stdout");
    set_nonblocking(out.as_raw_fd());

    // Fixed window: long enough that a spinner would have appeared (it starts
    // within the first poll) if the piped path wrongly emitted one.
    let captured = read_until(&mut out, Instant::now() + Duration::from_secs(8), |_| false);

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        captured.contains("Waiting for a bookmark"),
        "watch should reach the bookmark-wait poll; got:\n{captured}"
    );
    assert!(
        !captured.contains('⠋') && !captured.contains('⠙'),
        "no spinner frames when stdout is piped; got:\n{captured:?}"
    );
    assert!(
        !captured.contains('\r'),
        "no carriage-return control chars in piped output; got:\n{captured:?}"
    );
}
