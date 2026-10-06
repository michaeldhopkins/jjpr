//! The journal's lock: one undo or redo at a time.

use std::path::PathBuf;

use anyhow::{Context, Result};

use super::{Journal, alive};

impl Journal {
    /// Hold the journal for one undo or redo, so two cannot interleave. A
    /// lock left by a process that has gone is taken over.
    pub fn lock(&self) -> Result<Lock> {
        std::fs::create_dir_all(&self.dir).context("failed to create the undo journal")?;
        let path = self.dir.join("undo.lock");
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    write!(file, "{}", std::process::id())?;
                    return Ok(Lock { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder = std::fs::read_to_string(&path).ok();
                    let pid = holder.and_then(|s| s.trim().parse::<u32>().ok());
                    if pid.is_some_and(alive) {
                        anyhow::bail!(
                            "another jjpr undo or redo is running in this repo (pid {})",
                            pid.unwrap_or_default()
                        );
                    }
                    // Move the dead holder's lock aside under a name of our
                    // own, rather than deleting it: two processes deleting
                    // could each delete the other's fresh lock.
                    // What was moved is checked again: another process may
                    // have taken over in between, and its lock goes back.
                    let aside = self.dir.join(format!("undo.lock.{}", std::process::id()));
                    if std::fs::rename(&path, &aside).is_ok() {
                        let moved = std::fs::read_to_string(&aside).ok();
                        let pid = moved.and_then(|s| s.trim().parse::<u32>().ok());
                        if let Some(pid) = pid.filter(|&p| alive(p)) {
                            let _ = std::fs::rename(&aside, &path);
                            anyhow::bail!(
                                "another jjpr undo or redo is running in this repo (pid {pid})"
                            );
                        }
                        let _ = std::fs::remove_file(&aside);
                    }
                }
                Err(e) => return Err(e).context("failed to lock the undo journal"),
            }
        }
        anyhow::bail!("could not lock the undo journal")
    }
}

/// Held while an undo or redo runs; removed on drop.
pub struct Lock {
    path: PathBuf,
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
