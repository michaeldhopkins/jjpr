//! Bounded parallel execution for independent I/O-bound calls.
//!
//! Forge status lookups are independent per pull request but each costs a full
//! network round trip, so issuing them serially makes a stack's latency scale
//! with its height. These helpers fan the calls out across a small thread pool.
//!
//! The concurrency cap is not just politeness: GitHub enforces a secondary rate
//! limit on concurrent requests, and a burst wide enough to trip it costs far
//! more than the serialization it saved.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Maximum simultaneous in-flight requests to a single forge.
///
/// GitHub documents a secondary rate limit at 100 concurrent requests; staying
/// an order of magnitude under it leaves room for other tools sharing the token.
pub const MAX_CONCURRENT_REQUESTS: usize = 8;

/// Apply `f` to every item on up to `limit` threads, returning results in
/// **input order** regardless of completion order.
///
/// Order matters to callers that zip the results back against the stack's
/// segments; a completion-ordered result would silently mislabel PR statuses.
pub fn map_bounded<T, R, F>(items: &[T], limit: usize, f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }

    // Spawning threads costs more than it saves for a single item, which is the
    // common case for a one-PR stack.
    let workers = limit.clamp(1, n);
    if workers == 1 {
        return items.iter().map(f).collect();
    }

    let slots: Vec<Mutex<Option<R>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let cursor = AtomicUsize::new(0);
    let f = &f;
    let slots_ref = &slots;
    let items_ref = items;
    let cursor = &cursor;

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(move || {
                loop {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let value = f(&items_ref[i]);
                    *slots_ref[i].lock().expect("slot mutex poisoned") = Some(value);
                }
            });
        }
    });

    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .expect("slot mutex poisoned")
                .expect("every index is claimed exactly once")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[test]
    fn empty_input_spawns_nothing() {
        let items: Vec<u32> = Vec::new();
        let out = map_bounded(&items, 4, |x| *x);
        assert!(out.is_empty());
    }

    #[test]
    fn preserves_input_order_despite_completion_order() {
        // Reverse the sleep durations so later items finish first. A naive
        // push-as-you-finish implementation would return them backwards.
        let items: Vec<u64> = (0..8).collect();
        let out = map_bounded(&items, 8, |i| {
            std::thread::sleep(Duration::from_millis((8 - *i) * 10));
            *i * 10
        });
        assert_eq!(out, vec![0, 10, 20, 30, 40, 50, 60, 70]);
    }

    #[test]
    fn applies_f_exactly_once_per_item() {
        let items: Vec<u32> = (0..64).collect();
        let calls = AtomicUsize::new(0);
        let out = map_bounded(&items, 8, |x| {
            calls.fetch_add(1, Ordering::SeqCst);
            *x
        });
        assert_eq!(calls.load(Ordering::SeqCst), 64);
        assert_eq!(out, items);
    }

    #[test]
    fn never_exceeds_the_concurrency_limit() {
        let items: Vec<u32> = (0..40).collect();
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        map_bounded(&items, 4, |_| {
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(5));
            live.fetch_sub(1, Ordering::SeqCst);
        });
        assert!(
            peak.load(Ordering::SeqCst) <= 4,
            "peak concurrency {} exceeded limit 4",
            peak.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn single_worker_still_covers_every_item() {
        let items: Vec<u32> = (0..5).collect();
        let out = map_bounded(&items, 1, |x| *x * 2);
        assert_eq!(out, vec![0, 2, 4, 6, 8]);
    }

    #[test]
    fn limit_larger_than_input_is_clamped() {
        let items: Vec<u32> = (0..3).collect();
        let out = map_bounded(&items, 999, |x| *x + 1);
        assert_eq!(out, vec![1, 2, 3]);
    }
}
