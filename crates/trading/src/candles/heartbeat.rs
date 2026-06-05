//! Wave 6 Sub-PR #1 item 1.4h — per-minute aggregator heartbeat
//! counter primitive.
//!
//! Three-counter snapshot shared between the multi-TF aggregator task
//! (writer, items 1.4d/1.4e) and a once-per-minute heartbeat task
//! (reader-resetter, wired in `crates/app/src/main.rs`).
//!
//! Provides a coalesced 60s positive-signal log + Telegram event
//! per `AGGREGATOR-HB-01` (`wave-6-error-codes.md`) so the operator
//! can tail `data/logs/app.YYYY-MM-DD.log | grep "aggregator
//! heartbeat"` and confirm the master switch is producing sealed
//! candles for the shadow tables.
//!
//! ## Hot-path budget
//!
//! - `record_emit`/`record_drop`/`record_late_ticks` are
//!   `AtomicU64::fetch_add(_, Ordering::Relaxed)` → ~3 ns per call
//!   on x86-64. Called once per seal (not per tick), so amortised
//!   across the per-minute boundary burst.
//! - `drain` is `AtomicU64::swap(0, Ordering::AcqRel)` → ~5 ns.
//!   Called once per 60s tick from the heartbeat task; not on any
//!   hot path.
//!
//! ## Concurrency
//!
//! Lock-free. Multiple writers (one per Tokio worker if the
//! aggregator subscription closure runs on multiple threads) can
//! `record_*` concurrently. The reader-resetter `drain` is also
//! lock-free; it atomically reads-and-zeros each counter. Counter
//! increments observed *between* the three `swap` calls of one
//! `drain` correctly land in the NEXT minute's snapshot.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Three-counter snapshot accumulator. `Clone` is cheap (atomic
/// refcount bump on the inner `Arc`) so the aggregator task and the
/// heartbeat task each hold a clone.
#[derive(Clone, Default)]
pub struct AggregatorHeartbeatCounters {
    inner: Arc<AggregatorHeartbeatInner>,
}

#[derive(Default)]
struct AggregatorHeartbeatInner {
    seals_emitted: AtomicU64,
    seals_dropped: AtomicU64,
    late_ticks_discarded: AtomicU64,
    /// Option B: 1-bucket-late ticks that re-folded their OWN minute's
    /// high/low/close and were re-emitted (UPSERT) rather than discarded.
    amended_ticks: AtomicU64,
    /// G3 positive-signal counter: sealed candles this interval whose
    /// `close_pct_from_prev_day` was NON-ZERO. The false-OK guard
    /// (audit-findings Rule 11) for the percentage-change feature — if
    /// `seals_emitted` advances during market hours but this stays 0,
    /// the % column is silently broken (the Concern-C regression class),
    /// the exact false-OK the operator must never miss.
    close_pct_nonzero: AtomicU64,
}

impl AggregatorHeartbeatCounters {
    /// Build an empty counter set. All three counters start at 0.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one successfully-emitted seal (master switch produced a
    /// `BufferedSeal` and the mpsc `try_send` succeeded). Hot path:
    /// called once per timeframe at every minute boundary.
    pub fn record_emit(&self) {
        self.inner.seals_emitted.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one dropped seal (mpsc `try_send` returned `Err` because
    /// the writer task fell behind ~20s at peak burst). Hot path; rare.
    pub fn record_drop(&self) {
        self.inner.seals_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Record `n` late ticks discarded in one `consume_tick` call
    /// (`ConsumeStats::late_count`). Hot path.
    pub fn record_late_ticks(&self, n: u64) {
        self.inner
            .late_ticks_discarded
            .fetch_add(n, Ordering::Relaxed);
    }

    /// Record `n` late ticks that AMENDED their own minute's high/low/close
    /// (Option B; `ConsumeStats::amended_count`). Hot path.
    pub fn record_amended_ticks(&self, n: u64) {
        self.inner.amended_ticks.fetch_add(n, Ordering::Relaxed);
    }

    /// Record one emitted seal whose `close_pct_from_prev_day` was
    /// non-zero (G3 real-time proof that the percentage-change column is
    /// populating). Seal path; called once per timeframe at a boundary,
    /// only when the computed % is non-zero. ~3 ns.
    pub fn record_close_pct_nonzero(&self) {
        self.inner.close_pct_nonzero.fetch_add(1, Ordering::Relaxed);
    }

    /// Atomic snapshot + reset.
    ///
    /// Returns the counts accumulated since the last `drain` call and
    /// resets each counter to `0`. Called once per 60s by the
    /// heartbeat task.
    ///
    /// Increments observed between the three internal `swap` calls
    /// land in the NEXT drain's snapshot — correct for monotonically
    /// increasing event counts.
    pub fn drain(&self) -> AggregatorHeartbeatSnapshot {
        AggregatorHeartbeatSnapshot {
            seals_emitted: self.inner.seals_emitted.swap(0, Ordering::AcqRel),
            seals_dropped: self.inner.seals_dropped.swap(0, Ordering::AcqRel),
            late_ticks_discarded: self.inner.late_ticks_discarded.swap(0, Ordering::AcqRel),
            amended_ticks: self.inner.amended_ticks.swap(0, Ordering::AcqRel),
            close_pct_nonzero: self.inner.close_pct_nonzero.swap(0, Ordering::AcqRel),
        }
    }
}

/// Coalesced 60s heartbeat sample. The heartbeat task gates emission
/// on `is_active()` — pre-market / post-market silence does NOT spam
/// the log per Rule 3 (market-hours awareness) and Rule 11 (no
/// false-OK signals) from `audit-findings-2026-04-17.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregatorHeartbeatSnapshot {
    /// Sealed bars successfully forwarded to the mpsc writer task.
    pub seals_emitted: u64,
    /// Sealed bars dropped because the mpsc channel was full.
    pub seals_dropped: u64,
    /// Ticks arriving after their bucket's seal had already fired AND too late
    /// to amend (≥ 2 buckets late, or post-force_seal) — genuinely dropped.
    pub late_ticks_discarded: u64,
    /// Option B: 1-bucket-late ticks that re-folded their own minute's
    /// high/low/close and were re-emitted (UPSERT) instead of discarded.
    pub amended_ticks: u64,
    /// Emitted seals this interval with a non-zero `close_pct_from_prev_day`.
    /// Compare against `seals_emitted`: `seals_emitted > 0` while
    /// `close_pct_nonzero == 0` during market hours is the percentage-change
    /// false-OK smell (G3 / audit-findings Rule 11).
    pub close_pct_nonzero: u64,
}

impl AggregatorHeartbeatSnapshot {
    /// `true` when at least one counter is non-zero.
    ///
    /// Gate predicate for the once-per-minute log/Telegram emission —
    /// silent minutes (no ticks arrived, e.g. 16:00–09:00 IST or
    /// during 08:30–09:14:59 IST pre-market gap) produce a zero
    /// snapshot and MUST NOT emit (audit-findings Rule 11).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.seals_emitted > 0
            || self.seals_dropped > 0
            || self.late_ticks_discarded > 0
            || self.amended_ticks > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_counters_are_all_zero() {
        let c = AggregatorHeartbeatCounters::new();
        let snap = c.drain();
        assert_eq!(snap.seals_emitted, 0);
        assert_eq!(snap.seals_dropped, 0);
        assert_eq!(snap.late_ticks_discarded, 0);
    }

    #[test]
    fn test_record_emit_increments_seals_emitted() {
        let c = AggregatorHeartbeatCounters::new();
        c.record_emit();
        c.record_emit();
        c.record_emit();
        let snap = c.drain();
        assert_eq!(snap.seals_emitted, 3);
        assert_eq!(snap.seals_dropped, 0);
        assert_eq!(snap.late_ticks_discarded, 0);
    }

    #[test]
    fn test_record_drop_increments_seals_dropped() {
        let c = AggregatorHeartbeatCounters::new();
        c.record_drop();
        c.record_drop();
        let snap = c.drain();
        assert_eq!(snap.seals_emitted, 0);
        assert_eq!(snap.seals_dropped, 2);
        assert_eq!(snap.late_ticks_discarded, 0);
    }

    #[test]
    fn test_record_late_ticks_adds_count() {
        let c = AggregatorHeartbeatCounters::new();
        c.record_late_ticks(7);
        c.record_late_ticks(3);
        let snap = c.drain();
        assert_eq!(snap.seals_emitted, 0);
        assert_eq!(snap.seals_dropped, 0);
        assert_eq!(snap.late_ticks_discarded, 10);
    }

    #[test]
    fn test_drain_resets_all_three_counters() {
        let c = AggregatorHeartbeatCounters::new();
        c.record_emit();
        c.record_drop();
        c.record_late_ticks(5);
        let first = c.drain();
        assert!(first.is_active());
        let second = c.drain();
        assert_eq!(second.seals_emitted, 0);
        assert_eq!(second.seals_dropped, 0);
        assert_eq!(second.late_ticks_discarded, 0);
        assert!(!second.is_active());
    }

    #[test]
    fn test_snapshot_is_active_when_any_counter_non_zero() {
        let s = AggregatorHeartbeatSnapshot {
            seals_emitted: 1,
            seals_dropped: 0,
            late_ticks_discarded: 0,
            amended_ticks: 0,
            close_pct_nonzero: 0,
        };
        assert!(s.is_active());
        let s = AggregatorHeartbeatSnapshot {
            seals_emitted: 0,
            seals_dropped: 1,
            late_ticks_discarded: 0,
            amended_ticks: 0,
            close_pct_nonzero: 0,
        };
        assert!(s.is_active());
        let s = AggregatorHeartbeatSnapshot {
            seals_emitted: 0,
            seals_dropped: 0,
            late_ticks_discarded: 1,
            amended_ticks: 0,
            close_pct_nonzero: 0,
        };
        assert!(s.is_active());
        // Option B: an amend alone makes the heartbeat active.
        let s = AggregatorHeartbeatSnapshot {
            seals_emitted: 0,
            seals_dropped: 0,
            late_ticks_discarded: 0,
            amended_ticks: 1,
            close_pct_nonzero: 0,
        };
        assert!(s.is_active());
    }

    #[test]
    fn test_snapshot_is_active_false_for_all_zero() {
        let s = AggregatorHeartbeatSnapshot {
            seals_emitted: 0,
            seals_dropped: 0,
            late_ticks_discarded: 0,
            amended_ticks: 0,
            close_pct_nonzero: 0,
        };
        assert!(!s.is_active());
    }

    #[test]
    fn test_record_amended_ticks_adds_count() {
        let c = AggregatorHeartbeatCounters::new();
        c.record_amended_ticks(4);
        c.record_amended_ticks(6);
        let snap = c.drain();
        assert_eq!(snap.amended_ticks, 10);
        // drain reset
        assert_eq!(c.drain().amended_ticks, 0);
    }

    #[test]
    fn test_record_close_pct_nonzero_increments_only_that_counter() {
        let c = AggregatorHeartbeatCounters::new();
        c.record_close_pct_nonzero();
        c.record_close_pct_nonzero();
        let snap = c.drain();
        assert_eq!(snap.close_pct_nonzero, 2);
        assert_eq!(snap.seals_emitted, 0);
        assert_eq!(snap.seals_dropped, 0);
        assert_eq!(snap.late_ticks_discarded, 0);
    }

    #[test]
    fn test_drain_resets_close_pct_nonzero() {
        let c = AggregatorHeartbeatCounters::new();
        c.record_close_pct_nonzero();
        assert_eq!(c.drain().close_pct_nonzero, 1);
        assert_eq!(c.drain().close_pct_nonzero, 0);
    }

    // G3 false-OK contract: a non-zero close_pct_nonzero implies at least
    // one seal carried a real % change. `seals_emitted > 0` with
    // `close_pct_nonzero == 0` during market hours is the smell the
    // operator/CloudWatch alarm watches for.
    #[test]
    fn test_close_pct_nonzero_is_bounded_by_seals_emitted_when_wired() {
        let c = AggregatorHeartbeatCounters::new();
        // simulate the seal site: every nonzero-pct seal is also an emit
        c.record_emit();
        c.record_close_pct_nonzero();
        c.record_emit(); // a flat (0%) seal — emit without nonzero
        let snap = c.drain();
        assert!(snap.close_pct_nonzero <= snap.seals_emitted);
        assert_eq!(snap.seals_emitted, 2);
        assert_eq!(snap.close_pct_nonzero, 1);
    }

    /// `Clone` shares state via `Arc<AtomicU64>` so writer / reader
    /// task each holding a clone observe the same counters.
    #[test]
    fn test_clone_shares_state_via_arc() {
        let writer = AggregatorHeartbeatCounters::new();
        let reader = writer.clone();
        writer.record_emit();
        writer.record_emit();
        let snap = reader.drain();
        assert_eq!(snap.seals_emitted, 2);
        // After reader drained, writer's view is also zeroed.
        let snap2 = writer.drain();
        assert_eq!(snap2.seals_emitted, 0);
    }

    /// Concurrent increments from multiple threads must not lose
    /// counts (AtomicU64 with `fetch_add(_, Relaxed)` is a wait-free
    /// counter — but the test pins the invariant against future
    /// regressions).
    #[test]
    fn test_concurrent_record_does_not_lose_counts() {
        use std::thread;
        const THREADS: u32 = 4;
        const PER_THREAD: u32 = 1_000;
        let c = AggregatorHeartbeatCounters::new();
        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let cc = c.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    cc.record_emit();
                }
            }));
        }
        for h in handles {
            h.join().expect("thread join");
        }
        let snap = c.drain();
        assert_eq!(snap.seals_emitted, u64::from(THREADS * PER_THREAD));
    }

    /// Drain happening concurrently with an increment must not lose
    /// the increment — `swap(0, AcqRel)` returns the prior value, and
    /// any increment that hits the counter AFTER the swap lands in
    /// the next drain's snapshot. Asserts the total across both
    /// drains equals the total recorded.
    #[test]
    fn test_concurrent_drain_does_not_lose_increments() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::AtomicBool;
        use std::thread;
        let c = AggregatorHeartbeatCounters::new();
        let stop = StdArc::new(AtomicBool::new(false));
        let stop_w = StdArc::clone(&stop);
        let writer = c.clone();
        let writer_handle = thread::spawn(move || {
            let mut sent: u64 = 0;
            while !stop_w.load(Ordering::Relaxed) {
                writer.record_emit();
                sent += 1;
                if sent >= 50_000 {
                    break;
                }
            }
            sent
        });
        let mut total_drained: u64 = 0;
        // Drain a few times while the writer is running.
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_micros(50));
            total_drained += c.drain().seals_emitted;
        }
        stop.store(true, Ordering::Relaxed);
        let sent = writer_handle.join().expect("writer join");
        // Final drain to collect any remaining increments.
        total_drained += c.drain().seals_emitted;
        assert_eq!(total_drained, sent);
    }
}
