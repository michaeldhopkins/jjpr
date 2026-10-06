//! Single-watcher coordination for `jjpr watch`.
//!
//! Two `jjpr watch` on one repo can't corrupt anything (the divergent-based
//! recovery handles that), but they double the forge API load every poll — which
//! on GitHub burns the user's personal rate limit. So a watcher records a small
//! heartbeat next to jjpr's other repo-local metadata (`.jj/jjpr-watch.json`),
//! refreshed each poll; a second `jjpr watch` that sees a fresh heartbeat exits
//! instead of piling on. Purely out-of-band from the jj operation log.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct HeartbeatData {
    pid: u32,
    started_at: u64,
    last_seen: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The heartbeat path for a repo, alongside `.jj/jjpr.toml`.
pub fn heartbeat_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".jj").join("jjpr-watch.json")
}

/// Whether a heartbeat is fresh — last refreshed within `window` seconds of
/// `now`. Pure, so the freshness rule is unit-testable without wall-clock games.
fn is_fresh(data: &HeartbeatData, now: u64, window: u64) -> bool {
    now.saturating_sub(data.last_seen) < window
}

fn read(path: &Path) -> Result<Option<HeartbeatData>> {
    match std::fs::read_to_string(path) {
        // A corrupt/half-written file is treated as absent — never a reason to
        // block a new watcher.
        Ok(s) => Ok(serde_json::from_str(&s).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write(path: &Path, data: &HeartbeatData) -> Result<()> {
    std::fs::write(path, serde_json::to_string(data)?)?;
    Ok(())
}

/// Whether a `jjpr watch` is running in this repo: the process that wrote the
/// heartbeat is alive, however long ago it last refreshed it (a long poll or
/// a slow forge is not an exit). A heartbeat a dead watcher left is ignored.
/// Watchers in other workspaces of the repo are found by their markers in
/// the shared store.
pub fn watch_running(repo_root: &Path) -> bool {
    if matches!(read(&heartbeat_path(repo_root)), Ok(Some(d)) if alive(d.pid)) {
        return true;
    }
    let Some(dir) = watchers_dir(repo_root) else {
        return false;
    };
    let Ok(read) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut running = false;
    for f in read.flatten() {
        let name = f.file_name().to_string_lossy().into_owned();
        let Some(pid) = name
            .strip_prefix("watch-")
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        if alive(pid) {
            running = true;
        } else {
            // A watcher that crashed left it; a reused pid would revive it.
            let _ = std::fs::remove_file(f.path());
        }
    }
    running
}

/// Where watchers leave their markers: the repo's own store, which every
/// workspace shares.
fn watchers_dir(repo_root: &Path) -> Option<PathBuf> {
    crate::undo::journal::repo_store_dir(repo_root)
        .ok()
        .map(|d| d.join("jjpr"))
}

/// Whether process `pid` is alive. Where that cannot be told, it is assumed
/// alive, the side that refuses rather than discards.
pub fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return true;
        };
        // SAFETY: signal 0 delivers nothing; it only asks whether the process
        // exists, and touches no memory.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// A held watch heartbeat: refreshed while watching, removed on drop.
pub struct WatchHeartbeat {
    path: PathBuf,
    pid: u32,
    started_at: u64,
    /// `watch-<pid>` in the repo's shared store, so `jjpr undo` in any
    /// workspace of the repo sees this watcher.
    marker: Option<PathBuf>,
}

impl WatchHeartbeat {
    /// Try to claim the watcher slot for `repo_root`. Returns `None` only when
    /// another watcher's heartbeat is *demonstrably* fresh (successfully read and
    /// within `window`) — the caller should exit. Best-effort otherwise: a
    /// read/parse error, or a failed write to `.jj`, is not a reason to refuse to
    /// watch (this is a politeness guard, not a lock), so we err toward running.
    pub fn claim(repo_root: &Path, window: u64) -> Option<Self> {
        let path = heartbeat_path(repo_root);
        if let Ok(Some(data)) = read(&path)
            && is_fresh(&data, now_secs(), window)
        {
            return None;
        }
        let now = now_secs();
        let pid = std::process::id();
        let _ = write(
            &path,
            &HeartbeatData {
                pid,
                started_at: now,
                last_seen: now,
            },
        );
        let marker = watchers_dir(repo_root).map(|d| d.join(format!("watch-{pid}")));
        if let Some(m) = &marker {
            let _ = m.parent().map(std::fs::create_dir_all);
            let _ = std::fs::write(m, pid.to_string());
        }
        Some(Self {
            path,
            pid,
            started_at: now,
            marker,
        })
    }

    /// Mark the watcher alive as of now. Best-effort: a transient write failure
    /// must never take down the watch loop.
    pub fn refresh(&self) {
        let _ = write(
            &self.path,
            &HeartbeatData {
                pid: self.pid,
                started_at: self.started_at,
                last_seen: now_secs(),
            },
        );
    }
}

impl Drop for WatchHeartbeat {
    fn drop(&mut self) {
        // Only remove our own heartbeat — never one a later watcher took over
        // after we went stale.
        if let Ok(Some(d)) = read(&self.path)
            && d.pid == self.pid
        {
            let _ = std::fs::remove_file(&self.path);
        }
        if let Some(m) = &self.marker {
            let _ = std::fs::remove_file(m);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(last_seen: u64) -> HeartbeatData {
        HeartbeatData {
            pid: 42,
            started_at: 0,
            last_seen,
        }
    }

    #[test]
    fn is_fresh_within_window() {
        assert!(is_fresh(&data(100), 100, 30), "same instant is fresh");
        assert!(is_fresh(&data(100), 129, 30), "29s < 30s window is fresh");
        assert!(
            !is_fresh(&data(100), 130, 30),
            "exactly the window is not fresh"
        );
        assert!(
            !is_fresh(&data(100), 200, 30),
            "well past the window is stale"
        );
    }

    #[test]
    fn claim_when_absent_succeeds_and_writes() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        let hb = WatchHeartbeat::claim(dir.path(), 30);
        assert!(hb.is_some(), "no heartbeat present -> claim succeeds");
        assert!(heartbeat_path(dir.path()).exists());
    }

    #[test]
    fn claim_blocked_by_fresh_heartbeat() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        write(&heartbeat_path(dir.path()), &data(now_secs())).unwrap();
        let hb = WatchHeartbeat::claim(dir.path(), 30);
        assert!(hb.is_none(), "a fresh heartbeat blocks a new claim");
    }

    #[test]
    fn claim_takes_over_stale_heartbeat() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        write(
            &heartbeat_path(dir.path()),
            &data(now_secs().saturating_sub(120)),
        )
        .unwrap();
        let hb = WatchHeartbeat::claim(dir.path(), 30);
        assert!(hb.is_some(), "a stale heartbeat is taken over");
    }

    #[test]
    fn claim_ignores_corrupt_heartbeat() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        std::fs::write(heartbeat_path(dir.path()), "not json {{{").unwrap();
        let hb = WatchHeartbeat::claim(dir.path(), 30);
        assert!(hb.is_some(), "a corrupt heartbeat is treated as absent");
    }

    #[test]
    fn claim_proceeds_when_metadata_is_unwritable() {
        // No `.jj` dir, so the write fails. A politeness guard must never refuse
        // to watch over that — claim still returns a (best-effort) guard.
        let dir = tempfile::TempDir::new().unwrap();
        let hb = WatchHeartbeat::claim(dir.path(), 30);
        assert!(
            hb.is_some(),
            "an unwritable metadata dir must not block watching"
        );
    }

    #[test]
    fn drop_removes_own_heartbeat() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        let path = heartbeat_path(dir.path());
        {
            let _hb = WatchHeartbeat::claim(dir.path(), 30).unwrap();
            assert!(path.exists());
        }
        assert!(!path.exists(), "dropping the guard removes its heartbeat");
    }

    #[test]
    fn drop_leaves_a_takeover_heartbeat_alone() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        let path = heartbeat_path(dir.path());
        let hb = WatchHeartbeat::claim(dir.path(), 30).unwrap();
        // Another watcher takes over (different pid) after we go stale.
        write(
            &path,
            &HeartbeatData {
                pid: hb.pid + 1,
                started_at: 0,
                last_seen: now_secs(),
            },
        )
        .unwrap();
        drop(hb);
        assert!(
            path.exists(),
            "must not delete a heartbeat another watcher owns"
        );
    }
}
