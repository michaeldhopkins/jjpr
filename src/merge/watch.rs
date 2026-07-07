use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::forge::types::PullRequest;
use crate::forge::{Forge, ForgeKind};

use super::execute::format_block_reason;
use super::plan::BlockReason;

pub(crate) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
pub(crate) const MAX_CONSECUTIVE_ERRORS: u32 = 10;

/// Sleep in small increments so Ctrl+C is responsive.
/// Returns `true` if interrupted by the shutdown flag.
pub fn interruptible_sleep(duration: Duration, shutdown: &AtomicBool) -> bool {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        if shutdown.load(Ordering::Relaxed) {
            return true;
        }
        thread::sleep(Duration::from_millis(500));
    }
    false
}

pub(crate) fn format_resolved_reason(reason: &BlockReason) -> &'static str {
    match reason {
        BlockReason::ChecksPending | BlockReason::ChecksFailing => "CI now passing",
        BlockReason::Draft => "No longer a draft",
        BlockReason::InsufficientApprovals { .. } => "Approval received",
        BlockReason::ChangesRequested => "Changes-requested resolved",
        BlockReason::Conflicted => "Conflicts resolved",
        BlockReason::MergeabilityUnknown => "Mergeability computed",
        BlockReason::NoPr => "PR now exists",
        BlockReason::LocalSyncFailed => "Local sync recovered",
        BlockReason::ForgeReconcileFailed => "Forge reconcile recovered",
    }
}

/// Report status changes between previous and current block reasons.
///
/// Returns `Some(displayed_reasons)` if any output was printed, where the
/// vec contains the reasons that were shown to the user. Returns `None` if
/// nothing was printed. Callers should use the returned vec as `prev_reasons`
/// for the next call, so delta reporting tracks what the user actually saw.
///
/// On the first evaluation (prev is None), `MergeabilityUnknown` is suppressed
/// because it's expected immediately after a push and resolves within seconds.
pub(crate) fn report_status_changes(
    bookmark: &str,
    prev: Option<&[BlockReason]>,
    current: &[BlockReason],
    fk: ForgeKind,
) -> Option<Vec<BlockReason>> {
    let Some(prev) = prev else {
        // First time blocked — suppress MergeabilityUnknown (expected after push)
        let reportable: Vec<_> = current
            .iter()
            .filter(|r| !matches!(r, BlockReason::MergeabilityUnknown))
            .collect();
        if reportable.is_empty() {
            return None;
        }
        println!("\n  Waiting for '{bookmark}':");
        for reason in &reportable {
            println!("    - {}", format_block_reason(reason, fk));
        }
        return Some(reportable.into_iter().cloned().collect());
    };

    if prev == current {
        return None;
    }

    let mut printed = false;

    // Report resolved reasons
    for old in prev {
        if !current.iter().any(|c| std::mem::discriminant(c) == std::mem::discriminant(old)) {
            println!("  {bookmark}: {}", format_resolved_reason(old));
            printed = true;
        }
    }

    // Report new reasons
    for new in current {
        if !prev.iter().any(|p| std::mem::discriminant(p) == std::mem::discriminant(new)) {
            println!("  {bookmark}: {}", format_block_reason(new, fk));
            printed = true;
        }
    }

    // Report approval count changes within InsufficientApprovals
    for new in current {
        if let BlockReason::InsufficientApprovals { have: new_have, need } = new {
            for old in prev {
                if let BlockReason::InsufficientApprovals { have: old_have, .. } = old
                    && new_have != old_have
                {
                    if *new_have >= *need {
                        println!("  {bookmark}: Approval threshold reached ({new_have}/{need})");
                    } else {
                        println!("  {bookmark}: Approvals now {new_have}/{need}");
                    }
                    printed = true;
                }
            }
        }
    }

    if printed {
        Some(current.to_vec())
    } else {
        None
    }
}

/// Braille spinner frames — the same cycle `indicatif` and most CLIs use.
const SPINNER_FRAMES: [&str; 10] =
    ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How long each spinner frame is shown. ~80ms reads as a smooth spin.
const SPINNER_TICK: Duration = Duration::from_millis(80);

/// Render one spinner frame in place. On a TTY this rewrites the current line
/// (carriage return + clear-to-end); off a TTY it writes nothing, so piped or
/// captured output stays clean.
pub(crate) fn render_spinner(out: &mut impl Write, is_tty: bool, frame: usize) {
    if !is_tty {
        return;
    }
    let glyph = SPINNER_FRAMES[frame % SPINNER_FRAMES.len()];
    let _ = write!(out, "\r{glyph} Waiting...\x1b[K");
    let _ = out.flush();
}

/// Erase the spinner line so the next full-line message starts on a clean
/// line. No-op off a TTY.
pub(crate) fn clear_status_line(out: &mut impl Write, is_tty: bool) {
    if !is_tty {
        return;
    }
    let _ = write!(out, "\r\x1b[K");
    let _ = out.flush();
}

/// Whether the scrolling "Still waiting" heartbeat should print. Only off a
/// TTY: on a TTY the live spinner already conveys liveness, so the heartbeat
/// would just scroll needlessly.
pub(crate) fn should_print_heartbeat(is_tty: bool, elapsed: Duration, interval: Duration) -> bool {
    !is_tty && elapsed >= interval
}

/// Sleep for `duration`, animating a live spinner on a TTY. Off a TTY this is
/// exactly `interruptible_sleep` (renders nothing). `frame` persists across
/// calls so the spin stays continuous between polls. On normal completion the
/// last frame is left in place (the caller clears it before printing a real
/// message); on shutdown the line is cleared. Returns true if interrupted.
pub(crate) fn spinner_sleep(
    duration: Duration,
    shutdown: &AtomicBool,
    is_tty: bool,
    frame: &mut usize,
) -> bool {
    if !is_tty {
        return interruptible_sleep(duration, shutdown);
    }
    let mut out = std::io::stdout();
    let end = Instant::now() + duration;
    while Instant::now() < end {
        if shutdown.load(Ordering::Relaxed) {
            clear_status_line(&mut out, is_tty);
            return true;
        }
        render_spinner(&mut out, is_tty, *frame);
        *frame = frame.wrapping_add(1);
        thread::sleep(SPINNER_TICK);
    }
    false
}

pub(crate) fn refresh_pr_map(
    forge: &dyn Forge,
    owner: &str,
    repo: &str,
) -> Result<HashMap<String, PullRequest>> {
    let fresh_prs = forge.list_open_prs(owner, repo)?;
    Ok(crate::forge::build_pr_map(fresh_prs, owner))
}

pub struct WatchOptions {
    pub shutdown: Arc<AtomicBool>,
    pub timeout: Option<Duration>,
    pub poll_interval: Duration,
    /// Whether stdout is a terminal. Gates the live in-place spinner (TTY)
    /// versus the scrolling heartbeat (pipes, CI, captured output).
    pub is_tty: bool,
}

pub(crate) fn local_time_hhmm() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);

    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    #[cfg(unix)]
    unsafe { libc::localtime_r(&secs, &mut tm) };
    #[cfg(windows)]
    unsafe { libc::localtime_s(&mut tm, &secs) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::ForgeKind;

    #[test]
    fn report_first_time_with_real_reasons_prints_them() {
        let reasons = vec![BlockReason::ChecksPending, BlockReason::Draft];
        let result = report_status_changes("auth", None, &reasons, ForgeKind::GitHub);
        let displayed = result.expect("should print on first eval with real reasons");
        assert!(displayed.contains(&BlockReason::ChecksPending));
        assert!(displayed.contains(&BlockReason::Draft));
    }

    #[test]
    fn report_no_change_returns_none() {
        let reasons = vec![BlockReason::ChecksPending];
        let result = report_status_changes("auth", Some(&reasons), &reasons, ForgeKind::GitHub);
        assert!(result.is_none());
    }

    #[test]
    fn report_resolved_reason_prints() {
        let prev = vec![BlockReason::ChecksPending, BlockReason::Draft];
        let current = vec![BlockReason::Draft];
        let result = report_status_changes("auth", Some(&prev), &current, ForgeKind::GitHub);
        assert!(result.is_some(), "ChecksPending resolution should print");
    }

    #[test]
    fn first_eval_suppresses_mergeability_unknown() {
        // MergeabilityUnknown is expected immediately after a push and
        // resolves within seconds. Suppressing it on the first eval avoids
        // confusing "still being computed" noise.
        let reasons = vec![BlockReason::MergeabilityUnknown];
        let result = report_status_changes("auth", None, &reasons, ForgeKind::GitHub);
        assert!(result.is_none());
    }

    #[test]
    fn first_eval_suppresses_mu_but_shows_others() {
        let reasons = vec![BlockReason::MergeabilityUnknown, BlockReason::ChecksPending];
        let result = report_status_changes("auth", None, &reasons, ForgeKind::GitHub);
        let displayed = result.expect("should print non-MU reasons");
        assert_eq!(displayed.len(), 1);
        assert!(displayed.contains(&BlockReason::ChecksPending));
        assert!(!displayed.contains(&BlockReason::MergeabilityUnknown));
    }

    #[test]
    fn second_eval_shows_persistent_mergeability_unknown() {
        let prev = vec![];
        let current = vec![BlockReason::MergeabilityUnknown];
        let result = report_status_changes("auth", Some(&prev), &current, ForgeKind::GitHub);
        assert!(result.is_some(), "MU should be reported as new on second eval");
    }

    #[test]
    fn approval_count_change_prints() {
        let prev = vec![BlockReason::InsufficientApprovals { have: 0, need: 2 }];
        let current = vec![BlockReason::InsufficientApprovals { have: 1, need: 2 }];
        let result = report_status_changes("auth", Some(&prev), &current, ForgeKind::GitHub);
        assert!(result.is_some(), "approval count change should be reported");
    }

    #[test]
    fn approval_threshold_reached_prints() {
        let prev = vec![BlockReason::InsufficientApprovals { have: 0, need: 1 }];
        let current = vec![BlockReason::InsufficientApprovals { have: 1, need: 1 }];
        let result = report_status_changes("auth", Some(&prev), &current, ForgeKind::GitHub);
        assert!(result.is_some(), "threshold reached should be reported");
    }

    // --- spinner / heartbeat split ---

    #[test]
    fn render_spinner_on_tty_rewrites_line_with_a_frame() {
        let mut buf = Vec::new();
        render_spinner(&mut buf, true, 0);
        let s = String::from_utf8(buf).expect("utf8");
        assert!(s.starts_with('\r'), "should carriage-return to column 0");
        assert!(s.contains(SPINNER_FRAMES[0]), "should render a spinner glyph");
        assert!(s.contains("Waiting..."));
        assert!(s.contains("\x1b[K"), "should clear to end of line");
    }

    #[test]
    fn render_spinner_wraps_frame_index() {
        // The frame index is a free-running counter; it must wrap, not panic.
        let mut buf = Vec::new();
        render_spinner(&mut buf, true, SPINNER_FRAMES.len() + 3);
        let s = String::from_utf8(buf).expect("utf8");
        assert!(s.contains(SPINNER_FRAMES[3]));
    }

    #[test]
    fn render_spinner_off_tty_is_silent() {
        let mut buf = Vec::new();
        render_spinner(&mut buf, false, 0);
        assert!(buf.is_empty(), "no spinner bytes when stdout is not a TTY");
    }

    #[test]
    fn clear_status_off_tty_is_silent() {
        let mut buf = Vec::new();
        clear_status_line(&mut buf, false);
        assert!(buf.is_empty());
    }

    #[test]
    fn clear_status_on_tty_erases_line() {
        let mut buf = Vec::new();
        clear_status_line(&mut buf, true);
        let s = String::from_utf8(buf).expect("utf8");
        assert!(s.starts_with('\r') && s.contains("\x1b[K"));
    }

    #[test]
    fn heartbeat_prints_only_off_tty_after_interval() {
        let interval = Duration::from_secs(60);
        assert!(should_print_heartbeat(false, Duration::from_secs(60), interval));
        assert!(!should_print_heartbeat(false, Duration::from_secs(59), interval));
        assert!(
            !should_print_heartbeat(true, Duration::from_secs(120), interval),
            "the live spinner replaces the heartbeat on a TTY",
        );
    }
}
