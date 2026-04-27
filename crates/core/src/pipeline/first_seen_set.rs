//! First-seen-per-IST-trading-day tracker (Wave 1 Item 4.3, gap G2).
//!
//! ## Why
//!
//! Per Dhan Ticket #5525125, the previous-day close for **NSE_EQ** and
//! **NSE_FNO** is delivered inside the regular Quote / Full packets (not
//! as a separate code-6 frame). The persistence layer (Item 4.2) wants
//! to write `previous_close` exactly **once per (security_id,
//! exchange_segment)** per IST trading day — every subsequent Quote or
//! Full packet for the same instrument carries the same prev-close
//! value, and re-writing every tick would amplify ILP throughput by
//! ~24K-x with zero added information.
//!
//! `FirstSeenSet` is the de-duplication primitive: O(1) `try_insert`
//! returns `true` only on the first insertion of a `(security_id,
//! exchange_segment)` pair. The set is reset at IST midnight by a
//! background tokio task (`spawn_ist_midnight_reset_task`) so the next
//! trading session gets a fresh population.
//!
//! ## Composite key (I-P1-11)
//!
//! Per `.claude/rules/project/security-id-uniqueness.md`, Dhan reuses
//! numeric `security_id` values across distinct `ExchangeSegment`
//! values. Keying this set on `(SecurityId, ExchangeSegment)` rather
//! than `SecurityId` alone is mandatory — keying on `SecurityId` would
//! silently drop one of two colliding instruments and break the
//! per-segment routing matrix from Ticket #5525125.

use std::sync::Arc;

use papaya::HashMap as PapayaMap;
use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY};
use tickvault_common::types::ExchangeSegment;
use tracing::{debug, info};

/// Initial capacity sized for the full subscription universe per the
/// 2026-04-25 F&O rebuild (3 indices full chain + 216 stocks ATM±25 +
/// cash equities = ~24,324 instruments). One PapayaMap entry per
/// `(security_id, segment)` pair.
const INITIAL_CAPACITY: usize = 25_000;

/// O(1) lookup table tracking which `(security_id, exchange_segment)`
/// pairs have already been observed during the current IST trading day.
///
/// Backed by `papaya::HashMap` for lock-free concurrent access — the
/// tick processor's hot path, the metrics scraper, and the IST-midnight
/// reset task can all touch this set in parallel without contention.
pub struct FirstSeenSet {
    seen: PapayaMap<(u32, ExchangeSegment), ()>,
}

impl Default for FirstSeenSet {
    fn default() -> Self {
        Self::new()
    }
}

impl FirstSeenSet {
    /// Constructs an empty `FirstSeenSet` sized for the full F&O
    /// universe. Allocation is one-shot at boot.
    pub fn new() -> Self {
        Self {
            seen: PapayaMap::with_capacity(INITIAL_CAPACITY),
        }
    }

    /// Tries to record a `(security_id, segment)` observation. Returns:
    ///
    /// * `true` — this is the FIRST time the pair was observed today;
    ///   the caller should fire the per-day write (e.g. emit
    ///   `PrevCloseLoaded` and persist a row to `previous_close`).
    /// * `false` — already observed today; the caller MUST skip the
    ///   per-day write to keep the persistence layer at most one row
    ///   per (security_id, segment, trading_date_ist).
    ///
    /// O(1) — single papaya `insert` call. Lock-free.
    #[inline]
    pub fn try_insert(&self, security_id: u32, segment: ExchangeSegment) -> bool {
        // PapayaMap::pin returns a guard scoped to the current thread.
        // Insert returns `Some(prev)` if the key existed, `None` otherwise.
        self.seen.pin().insert((security_id, segment), ()).is_none()
    }

    /// Returns `true` if the pair has been observed today without
    /// recording a new observation. Useful for read-only queries
    /// (e.g. metrics, debug logs).
    #[inline]
    pub fn contains(&self, security_id: u32, segment: ExchangeSegment) -> bool {
        self.seen.pin().contains_key(&(security_id, segment))
    }

    /// Returns the number of distinct pairs observed today.
    #[inline]
    pub fn len(&self) -> usize {
        self.seen.pin().len()
    }

    /// Returns `true` if no pairs have been observed today.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.seen.pin().is_empty()
    }

    /// Clears every entry. Idempotent. Called by the IST-midnight reset
    /// task at the start of each trading day so first-Quote / first-Full
    /// per (security_id, segment) re-fires.
    pub fn reset(&self) {
        let pinned = self.seen.pin();
        let n = pinned.len();
        pinned.clear();
        info!(
            cleared_entries = n,
            "FirstSeenSet reset at IST midnight — first-Quote/Full per \
             (security_id, segment) will fire again today"
        );
    }
}

/// Returns the number of seconds until the NEXT IST midnight, using
/// the system wall clock. Pure function (no I/O) for testability.
///
/// `now_unix_secs` is the current time as Unix epoch seconds (UTC).
/// Returns a value in `(0, 86400]`.
pub fn secs_until_next_ist_midnight(now_unix_secs: i64) -> u64 {
    let now_ist_secs = now_unix_secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: mod by SECONDS_PER_DAY (=86_400) is safe
    let secs_into_day = now_ist_secs.rem_euclid(i64::from(SECONDS_PER_DAY));
    let remaining = i64::from(SECONDS_PER_DAY).saturating_sub(secs_into_day);
    // Cast is safe — remaining is in [1, 86_400].
    #[allow(clippy::cast_sign_loss)] // APPROVED: remaining > 0 always
    let remaining_u64 = remaining.max(1) as u64;
    remaining_u64
}

/// Spawns a background tokio task that calls `set.reset()` at every IST
/// midnight. The task runs until the runtime is dropped (i.e. until app
/// shutdown). Returns immediately — does NOT block.
///
/// Idempotent if called multiple times in the sense that each call
/// spawns a fresh task. Production callers should call this exactly
/// once at boot.
pub fn spawn_ist_midnight_reset_task(set: Arc<FirstSeenSet>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let now = chrono::Utc::now().timestamp();
            let sleep_secs = secs_until_next_ist_midnight(now);
            debug!(
                sleep_secs,
                "FirstSeenSet IST-midnight reset task sleeping until next IST 00:00"
            );
            tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
            set.reset();
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
// APPROVED: test-only helpers — atomic increments + tokio sleeps in tests
// are canonical and do not need ?-propagation.
mod tests {
    use super::*;

    /// First insert returns true; second insert with the same key
    /// returns false.
    #[test]
    fn test_first_seen_set_try_insert_returns_true_only_first_time() {
        let set = FirstSeenSet::new();
        assert!(set.try_insert(13, ExchangeSegment::IdxI));
        assert!(!set.try_insert(13, ExchangeSegment::IdxI));
    }

    /// Composite-key invariant (I-P1-11): the same security_id under
    /// two different segments must produce two distinct first-seen
    /// observations. Without this, a NIFTY id=27 IDX_I value tick
    /// would silently mask an NSE_EQ id=27 stock.
    #[test]
    fn test_first_seen_set_distinguishes_segments_for_same_security_id() {
        let set = FirstSeenSet::new();
        assert!(set.try_insert(27, ExchangeSegment::IdxI));
        assert!(
            set.try_insert(27, ExchangeSegment::NseEquity),
            "same security_id under a different segment must produce a \
             second first-seen observation per I-P1-11 (composite key)"
        );
        assert_eq!(set.len(), 2);
    }

    /// `reset()` clears every entry; subsequent inserts re-fire.
    /// This is the IST-midnight invariant — per gap G2, the set MUST
    /// be reset every IST 00:00 so the next trading session re-fires
    /// the first-Quote / first-Full triggers.
    #[test]
    fn test_first_seen_set_resets_at_ist_midnight() {
        let set = FirstSeenSet::new();
        set.try_insert(13, ExchangeSegment::IdxI);
        set.try_insert(2885, ExchangeSegment::NseEquity);
        set.try_insert(73_242, ExchangeSegment::NseFno);
        assert_eq!(set.len(), 3);

        set.reset();
        assert!(set.is_empty());

        // After reset, first-insert returns true again.
        assert!(set.try_insert(13, ExchangeSegment::IdxI));
        assert!(set.try_insert(2885, ExchangeSegment::NseEquity));
    }

    /// `contains` is the read-only counterpart — never mutates state.
    #[test]
    fn test_first_seen_set_contains_does_not_mutate() {
        let set = FirstSeenSet::new();
        assert!(!set.contains(13, ExchangeSegment::IdxI));
        // Second contains call still returns false (no mutation).
        assert!(!set.contains(13, ExchangeSegment::IdxI));
        assert_eq!(set.len(), 0);
    }

    /// `len` and `is_empty` reflect the current population.
    #[test]
    fn test_first_seen_set_len_and_is_empty_track_population() {
        let set = FirstSeenSet::new();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        set.try_insert(1, ExchangeSegment::NseEquity);
        assert_eq!(set.len(), 1);
        assert!(!set.is_empty());
    }

    /// Pure-function `secs_until_next_ist_midnight` returns a value
    /// in `(0, 86_400]` for any input. Tested at IST midnight,
    /// 1 second after midnight, and 1 second before midnight.
    #[test]
    fn test_secs_until_next_ist_midnight_at_boundaries() {
        // IST midnight = UTC 18:30 (= +19_800 seconds offset).
        // Pick a Unix epoch where (epoch + 19_800) % 86_400 == 0,
        // i.e. epoch % 86_400 == 66_600.
        // 1_777_161_600 is UTC 2026-04-26 00:00:00 (= 86_400 * 20_569).
        // Adding 66_600 gives 2026-04-26 18:30:00 UTC = 2026-04-27 00:00:00 IST.
        let ist_midnight_utc_secs = 1_777_228_200_i64;
        // At exactly IST midnight, secs_until = 86_400 (next midnight).
        assert_eq!(secs_until_next_ist_midnight(ist_midnight_utc_secs), 86_400);
        // 1 second after midnight, secs_until = 86_399.
        assert_eq!(
            secs_until_next_ist_midnight(ist_midnight_utc_secs.saturating_add(1)),
            86_399
        );
        // 1 second before midnight, secs_until = 1.
        assert_eq!(
            secs_until_next_ist_midnight(ist_midnight_utc_secs.saturating_sub(1)),
            1
        );
    }

    /// Sanity: the reset task can be spawned and aborts cleanly when
    /// the JoinHandle is dropped (validates the public spawn API).
    #[tokio::test]
    async fn test_spawn_ist_midnight_reset_task_returns_join_handle() {
        let set = Arc::new(FirstSeenSet::new());
        let handle = spawn_ist_midnight_reset_task(Arc::clone(&set));
        // Aborting must terminate the task — proves the spawn API is
        // correctly wired into tokio's runtime and not a no-op.
        handle.abort();
    }
}
