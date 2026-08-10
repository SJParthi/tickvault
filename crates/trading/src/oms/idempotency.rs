//! Order idempotency using UUID v4 correlation IDs.
//!
//! Each order placed through the OMS gets a unique correlation ID.
//! Dhan echoes this ID back in the place response and in WebSocket order updates,
//! enabling matching between our request and the resulting order.
//!
//! Dhan `correlationId` max length is **30 characters**, charset `[a-zA-Z0-9 _-]`.
//! UUID v4 (36 chars) exceeds this limit, so we strip hyphens to produce a
//! 32-char hex string, then truncate to 30 chars. This preserves 120 bits of
//! entropy (30 hex chars = 120 bits), which is collision-safe for our volume.
//!
//! # Phase 1 (current)
//! In-memory map of `correlation_id → order_id`, **bounded** at
//! [`MAX_TRACKED_CORRELATIONS`]. Sufficient for single-instance deployment.
//!
//! ## Space (2026-08-10 — the bound and the inline key)
//!
//! Before: `HashMap<String, String>` with no cap, no eviction, and — the
//! actual defect — no flag saying so. `track()` only ever inserted; the sole
//! removal was the wholesale [`CorrelationTracker::clear`] on daily reset.
//! Growth was therefore bounded only by the EXTERNAL convention that the
//! broker's daily order ceiling is a few thousand; the type enforced nothing,
//! and a runaway retry loop (or a future multi-day process) would grow it
//! without limit and without a single log line.
//!
//! Now, two changes:
//!
//! 1. **Hard cap, fail-closed and LOUD.** At [`MAX_TRACKED_CORRELATIONS`] the
//!    tracker REFUSES the new correlation — it never evicts an existing one.
//!    A correlation ID is an ORDER-TRACKING IDENTITY: evicting one silently
//!    orphans a broker response that can no longer be matched back to the
//!    request that produced it. The refusal returns [`TrackOutcome`], fires
//!    `error!(code = OMS-GAP-05)` **carrying both IDs** (so the mapping the
//!    tracker refused to hold is still recoverable by hand from the log), and
//!    increments `tv_oms_correlations_refused_total`.
//! 2. **Inline `Copy` key, no per-entry key allocation.** The key is a
//!    [`CorrelationKey`] — `[u8; 30] + len`, sized by Dhan's own 30-char
//!    `correlationId` limit — instead of a heap `String`. One fewer
//!    allocation per order, and a per-entry footprint that is now a fixed,
//!    statable number rather than "however long the caller's string was".
//!    The VALUE stays a `String`: Dhan's `order_id` has no documented length
//!    bound, and inventing one would be the same unflagged assumption this
//!    change exists to remove.
//!
//! **Known limitation (unchanged):** On crash, all mappings are lost. The
//! reconciliation module (`reconciliation.rs`) re-syncs state from Dhan REST
//! API on startup. Phase 2 persistence: reuse the file-based token-cache
//! pattern (`crates/core/src/auth/token_cache.rs`) — Valkey was removed in #O4.

use std::collections::HashMap;

use tickvault_common::constants::DHAN_CORRELATION_ID_MAX_LENGTH;
use tickvault_common::error_code::ErrorCode;
use tracing::error;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Capacity bound
// ---------------------------------------------------------------------------

/// Hard cap on distinct tracked correlations.
///
/// Sized as ~7x the documented Dhan daily order ceiling (7,000 orders/day —
/// CLAUDE.md "rate limiter (GCRA: 10/sec, 7000/day)"), and the map is cleared
/// on every daily reset. It therefore CANNOT bite normal operation — which is
/// the point: it exists so that an abnormal one (a retry storm, a reset that
/// never runs, a process living across days) is BOUNDED and LOUD instead of
/// growing until the box dies.
///
/// Memory ceiling at the cap: 50,000 × (32 B inline key + 24 B `String`
/// header + the order-id bytes) ≈ 5 MB — a stated ceiling, not a hope.
/// Raising it is a deliberate, reviewable edit.
pub const MAX_TRACKED_CORRELATIONS: usize = 50_000;

/// Outcome of [`CorrelationTracker::track`].
///
/// Deliberately NOT `#[must_use]`: existing call sites in the OMS engine
/// invoke `track()` as a statement, and a `must_use` here would break the
/// build for every one of them without making any of them safer today. The
/// refusal arms are already loud on their own (coded `error!` + counter); the
/// return value exists so a caller CAN branch once there is something useful
/// to do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackOutcome {
    /// New correlation recorded.
    Tracked,
    /// An existing correlation ID's order ID was overwritten (the pre-2026-08-10
    /// `HashMap::insert` behaviour, now named).
    Replaced,
    /// REFUSED — the tracker is at [`MAX_TRACKED_CORRELATIONS`]. The mapping
    /// is NOT held in memory; the coded `error!` carries both IDs.
    CapacityExhausted,
    /// REFUSED — the correlation ID exceeds [`DHAN_CORRELATION_ID_MAX_LENGTH`]
    /// bytes and cannot be stored inline.
    ///
    /// Truncating to fit was rejected: two distinct IDs sharing a 30-byte
    /// prefix would collide and match the WRONG order. An over-long ID is
    /// also one Dhan itself would reject, so it can never round-trip through
    /// the broker anyway — refusing loudly is the honest answer.
    IdTooLong,
}

// ---------------------------------------------------------------------------
// CorrelationKey — inline, Copy, no heap
// ---------------------------------------------------------------------------

/// Fixed-size correlation-ID key: at most [`DHAN_CORRELATION_ID_MAX_LENGTH`]
/// bytes held INLINE, so a tracked correlation costs zero key allocations.
///
/// Unused tail bytes are always zero, so the derived `Eq`/`Hash` over
/// `(len, bytes)` agree with byte-string equality of the used prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CorrelationKey {
    len: u8,
    bytes: [u8; DHAN_CORRELATION_ID_MAX_LENGTH],
}

impl CorrelationKey {
    /// Returns `None` when the ID does not fit — NEVER a truncation (see
    /// [`TrackOutcome::IdTooLong`]).
    fn new(id: &str) -> Option<Self> {
        let src = id.as_bytes();
        if src.len() > DHAN_CORRELATION_ID_MAX_LENGTH {
            return None;
        }
        let mut bytes = [0u8; DHAN_CORRELATION_ID_MAX_LENGTH];
        bytes[..src.len()].copy_from_slice(src);
        Some(Self {
            len: src.len() as u8,
            bytes,
        })
    }
}

// ---------------------------------------------------------------------------
// CorrelationTracker
// ---------------------------------------------------------------------------

/// Tracks correlation ID to order ID mappings for idempotency.
///
/// Used to match Dhan's asynchronous order responses with our requests.
/// Bounded at [`MAX_TRACKED_CORRELATIONS`] — see the module docs for the
/// refuse-never-evict rationale.
pub struct CorrelationTracker {
    /// Maps correlation_id (our UUID, inline) → order_id (Dhan's ID).
    index: HashMap<CorrelationKey, String>,
}

impl Default for CorrelationTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CorrelationTracker {
    /// Creates a new empty tracker.
    pub fn new() -> Self {
        Self {
            index: HashMap::with_capacity(256),
        }
    }

    /// Generates a new correlation ID from UUID v4, truncated to 30 chars.
    ///
    /// Dhan `correlationId` max length is 30 characters with charset `[a-zA-Z0-9 _-]`.
    /// UUID v4 simple (no hyphens) = 32 hex chars; we take first 30 for Dhan compliance.
    ///
    /// # Returns
    /// A 30-character hex string (120 bits of entropy).
    pub fn generate_id(&self) -> String {
        let uuid = Uuid::new_v4();
        let hex = uuid.as_simple().to_string(); // 32 hex chars, no hyphens
        hex[..DHAN_CORRELATION_ID_MAX_LENGTH].to_string()
    }

    /// Records a mapping from correlation ID to order ID.
    ///
    /// Called after a successful place order response from Dhan.
    ///
    /// FAIL-CLOSED at [`MAX_TRACKED_CORRELATIONS`]: the new correlation is
    /// REFUSED (never evicting an existing one), a coded `error!` carrying
    /// BOTH IDs is emitted so the mapping stays recoverable from the log, and
    /// `tv_oms_correlations_refused_total{reason}` increments. Overwriting an
    /// ID that is already tracked is always allowed — it consumes no new
    /// capacity and is the documented re-place/retry behaviour.
    ///
    /// O(1) average: one hash + one bucket probe. The `String` argument is
    /// consumed and its allocation released — the stored key is inline.
    pub fn track(&mut self, correlation_id: String, order_id: String) -> TrackOutcome {
        let Some(key) = CorrelationKey::new(&correlation_id) else {
            metrics::counter!("tv_oms_correlations_refused_total", "reason" => "id_too_long")
                .increment(1);
            error!(
                code = ErrorCode::OmsGapIdempotency.code_str(),
                correlation_id = %correlation_id,
                order_id = %order_id,
                len = correlation_id.len(),
                max_len = DHAN_CORRELATION_ID_MAX_LENGTH,
                "REFUSED to track correlation — id exceeds the Dhan correlationId limit; \
                 this order's broker response cannot be matched back automatically \
                 (both ids are in this line for manual reconciliation)"
            );
            return TrackOutcome::IdTooLong;
        };

        // An overwrite is not new capacity, so it is checked FIRST — a
        // full tracker must still accept updates for orders it already holds.
        if let Some(slot) = self.index.get_mut(&key) {
            *slot = order_id;
            return TrackOutcome::Replaced;
        }

        if self.index.len() >= MAX_TRACKED_CORRELATIONS {
            metrics::counter!("tv_oms_correlations_refused_total", "reason" => "capacity")
                .increment(1);
            error!(
                code = ErrorCode::OmsGapIdempotency.code_str(),
                correlation_id = %correlation_id,
                order_id = %order_id,
                cap = MAX_TRACKED_CORRELATIONS,
                live = self.index.len(),
                "REFUSED to track correlation — the tracker is at capacity; \
                 this order's broker response cannot be matched back automatically \
                 (both ids are in this line for manual reconciliation). \
                 Existing correlations are NEVER evicted to make room."
            );
            return TrackOutcome::CapacityExhausted;
        }

        self.index.insert(key, order_id);
        TrackOutcome::Tracked
    }

    /// Looks up the order ID for a given correlation ID. O(1) average.
    ///
    /// An ID too long to have been storable returns `None` — consistent with
    /// the [`TrackOutcome::IdTooLong`] refusal, never a truncated near-match.
    pub fn get_order_id(&self, correlation_id: &str) -> Option<&String> {
        self.index.get(&CorrelationKey::new(correlation_id)?)
    }

    /// Returns true if the correlation ID is tracked. O(1) average.
    pub fn contains(&self, correlation_id: &str) -> bool {
        CorrelationKey::new(correlation_id).is_some_and(|key| self.index.contains_key(&key))
    }

    /// Returns the number of tracked correlations.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Returns true if no correlations are tracked.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Clears all tracked correlations (daily reset).
    pub fn clear(&mut self) {
        self.index.clear();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_id_fits_dhan_correlation_id_limit() {
        let tracker = CorrelationTracker::new();
        let id = tracker.generate_id();
        assert_eq!(id.len(), DHAN_CORRELATION_ID_MAX_LENGTH);
        // Must be valid hex (subset of Dhan's allowed charset [a-zA-Z0-9 _-])
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_id_returns_unique_ids() {
        let tracker = CorrelationTracker::new();
        let id1 = tracker.generate_id();
        let id2 = tracker.generate_id();
        assert_ne!(id1, id2);
    }

    #[test]
    fn track_and_lookup() {
        let mut tracker = CorrelationTracker::new();
        tracker.track("corr-1".to_owned(), "order-100".to_owned());

        assert_eq!(
            tracker.get_order_id("corr-1"),
            Some(&"order-100".to_owned())
        );
        assert!(tracker.contains("corr-1"));
        assert!(!tracker.contains("corr-2"));
    }

    #[test]
    fn new_tracker_is_empty() {
        let tracker = CorrelationTracker::new();
        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn clear_removes_all() {
        let mut tracker = CorrelationTracker::new();
        tracker.track("c1".to_owned(), "o1".to_owned());
        tracker.track("c2".to_owned(), "o2".to_owned());
        assert_eq!(tracker.len(), 2);

        tracker.clear();
        assert!(tracker.is_empty());
        assert!(tracker.get_order_id("c1").is_none());
    }

    #[test]
    fn default_trait_creates_empty_tracker() {
        let tracker = CorrelationTracker::default();
        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);
    }

    // -----------------------------------------------------------------
    // 2026-08-10 — bounded space + inline key.
    // -----------------------------------------------------------------

    /// Fills the tracker to exactly `n` distinct correlations.
    fn fill(tracker: &mut CorrelationTracker, n: usize) {
        for i in 0..n {
            assert_eq!(
                tracker.track(format!("corr-{i}"), format!("order-{i}")),
                TrackOutcome::Tracked,
                "entry {i} must land below the cap"
            );
        }
    }

    #[test]
    fn track_refuses_at_capacity_and_never_evicts() {
        // Boundary: exactly at the cap everything lands; ONE past the cap is
        // REFUSED — and the refusal must not cost an existing correlation.
        // Evicting would orphan an in-flight order's identity, which is worse
        // than refusing the new one loudly.
        let mut tracker = CorrelationTracker::new();
        fill(&mut tracker, MAX_TRACKED_CORRELATIONS);
        assert_eq!(tracker.len(), MAX_TRACKED_CORRELATIONS);

        assert_eq!(
            tracker.track("one-past".to_owned(), "order-one-past".to_owned()),
            TrackOutcome::CapacityExhausted
        );
        assert_eq!(
            tracker.len(),
            MAX_TRACKED_CORRELATIONS,
            "a refusal must not grow the map"
        );
        assert!(
            !tracker.contains("one-past"),
            "the refused correlation is NOT held — never a silent half-track"
        );
        assert!(tracker.get_order_id("one-past").is_none());

        // Nothing was evicted: the first and last entries both survive.
        assert_eq!(
            tracker.get_order_id("corr-0"),
            Some(&"order-0".to_owned()),
            "the OLDEST correlation must survive — refuse-new, never evict-old"
        );
        assert_eq!(
            tracker.get_order_id(&format!("corr-{}", MAX_TRACKED_CORRELATIONS - 1)),
            Some(&format!("order-{}", MAX_TRACKED_CORRELATIONS - 1))
        );

        // Repeated refusals stay stable (no drift, no partial insert).
        for _ in 0..5 {
            assert_eq!(
                tracker.track("one-past".to_owned(), "order-one-past".to_owned()),
                TrackOutcome::CapacityExhausted
            );
        }
        assert_eq!(tracker.len(), MAX_TRACKED_CORRELATIONS);
    }

    #[test]
    fn track_at_capacity_still_replaces_an_already_tracked_correlation() {
        // A full tracker must still accept UPDATES for orders it already
        // holds — an overwrite consumes no new capacity, and refusing it
        // would freeze a stale order id in place at exactly the moment the
        // system is under stress.
        let mut tracker = CorrelationTracker::new();
        fill(&mut tracker, MAX_TRACKED_CORRELATIONS);
        assert_eq!(
            tracker.track("corr-7".to_owned(), "order-7-REPLACED".to_owned()),
            TrackOutcome::Replaced
        );
        assert_eq!(
            tracker.get_order_id("corr-7"),
            Some(&"order-7-REPLACED".to_owned())
        );
        assert_eq!(
            tracker.len(),
            MAX_TRACKED_CORRELATIONS,
            "replace never grows"
        );
    }

    #[test]
    fn clear_restores_capacity_after_exhaustion() {
        // The daily reset is the documented way back: after `clear()` the
        // tracker accepts new correlations again.
        let mut tracker = CorrelationTracker::new();
        fill(&mut tracker, MAX_TRACKED_CORRELATIONS);
        assert_eq!(
            tracker.track("blocked".to_owned(), "o".to_owned()),
            TrackOutcome::CapacityExhausted
        );
        tracker.clear();
        assert!(tracker.is_empty());
        assert_eq!(
            tracker.track("blocked".to_owned(), "o".to_owned()),
            TrackOutcome::Tracked
        );
        assert_eq!(tracker.get_order_id("blocked"), Some(&"o".to_owned()));
    }

    #[test]
    fn track_refuses_over_long_correlation_id_instead_of_truncating() {
        // Truncating to the 30-byte inline key would make two distinct IDs
        // that share a 30-byte prefix collide — and a collision here matches
        // the WRONG order. Refuse instead, loudly.
        let mut tracker = CorrelationTracker::new();
        let base = "a".repeat(DHAN_CORRELATION_ID_MAX_LENGTH);
        let too_long_a = format!("{base}X");
        let too_long_b = format!("{base}Y");
        assert_eq!(
            tracker.track(too_long_a.clone(), "order-A".to_owned()),
            TrackOutcome::IdTooLong
        );
        assert_eq!(
            tracker.track(too_long_b.clone(), "order-B".to_owned()),
            TrackOutcome::IdTooLong
        );
        assert!(tracker.is_empty(), "neither over-long id may be stored");
        // And neither is findable under the truncated prefix.
        assert!(tracker.get_order_id(&too_long_a).is_none());
        assert!(tracker.get_order_id(&too_long_b).is_none());
        assert!(!tracker.contains(&base));

        // The exact-limit id IS storable, and does not answer for either.
        assert_eq!(
            tracker.track(base.clone(), "order-EXACT".to_owned()),
            TrackOutcome::Tracked
        );
        assert_eq!(tracker.get_order_id(&base), Some(&"order-EXACT".to_owned()));
        assert!(
            tracker.get_order_id(&too_long_a).is_none(),
            "an over-long id must MISS, never match its own 30-byte prefix"
        );
    }

    #[test]
    fn track_boundary_lengths_empty_one_byte_and_exactly_max() {
        // Empty + maximal inputs.
        let mut tracker = CorrelationTracker::new();
        assert_eq!(
            tracker.track(String::new(), "order-empty".to_owned()),
            TrackOutcome::Tracked
        );
        assert_eq!(tracker.get_order_id(""), Some(&"order-empty".to_owned()));
        assert_eq!(
            tracker.track("x".to_owned(), "order-1".to_owned()),
            TrackOutcome::Tracked
        );
        let exact = "z".repeat(DHAN_CORRELATION_ID_MAX_LENGTH);
        assert_eq!(
            tracker.track(exact.clone(), "order-30".to_owned()),
            TrackOutcome::Tracked
        );
        assert_eq!(
            tracker.len(),
            3,
            "empty, 1-byte and 30-byte ids are distinct"
        );
        // The generated id (the real production shape) is exactly at the limit.
        let generated = tracker.generate_id();
        assert_eq!(generated.len(), DHAN_CORRELATION_ID_MAX_LENGTH);
        assert_eq!(
            tracker.track(generated.clone(), "order-gen".to_owned()),
            TrackOutcome::Tracked,
            "a generate_id() value must always be trackable"
        );
        assert_eq!(
            tracker.get_order_id(&generated),
            Some(&"order-gen".to_owned())
        );
    }

    #[test]
    fn track_distinguishes_ids_sharing_a_long_prefix() {
        // The inline key must compare the LENGTH as well as the bytes: a
        // 29-byte id and the same 29 bytes plus one more are different orders.
        let mut tracker = CorrelationTracker::new();
        let short = "b".repeat(DHAN_CORRELATION_ID_MAX_LENGTH - 1);
        let long = "b".repeat(DHAN_CORRELATION_ID_MAX_LENGTH);
        tracker.track(short.clone(), "order-short".to_owned());
        tracker.track(long.clone(), "order-long".to_owned());
        assert_eq!(tracker.len(), 2);
        assert_eq!(
            tracker.get_order_id(&short),
            Some(&"order-short".to_owned())
        );
        assert_eq!(tracker.get_order_id(&long), Some(&"order-long".to_owned()));
        // Differ in the LAST byte only.
        let mut tail_differs = long.clone();
        tail_differs.pop();
        tail_differs.push('c');
        tracker.track(tail_differs.clone(), "order-tail".to_owned());
        assert_eq!(tracker.len(), 3);
        assert_eq!(tracker.get_order_id(&long), Some(&"order-long".to_owned()));
        assert_eq!(
            tracker.get_order_id(&tail_differs),
            Some(&"order-tail".to_owned())
        );
    }

    proptest::proptest! {
        /// Arbitrary in-range IDs round-trip, distinct IDs never alias, and
        /// the map size equals the number of DISTINCT IDs seen — the property
        /// the `String` key gave for free and the inline key must preserve.
        #[test]
        fn track_inline_key_roundtrips_arbitrary_in_range_ids(
            ids in proptest::collection::vec("[a-zA-Z0-9 _-]{0,30}", 1..60)
        ) {
            let mut tracker = CorrelationTracker::new();
            let mut expected: std::collections::HashMap<String, String> =
                std::collections::HashMap::with_capacity(ids.len());
            for (i, id) in ids.iter().enumerate() {
                let order = format!("order-{i}");
                let outcome = tracker.track(id.clone(), order.clone());
                proptest::prop_assert!(
                    matches!(outcome, TrackOutcome::Tracked | TrackOutcome::Replaced),
                    "an in-range id must never be refused: {outcome:?}"
                );
                expected.insert(id.clone(), order);
            }
            proptest::prop_assert_eq!(tracker.len(), expected.len());
            for (id, order) in &expected {
                proptest::prop_assert_eq!(tracker.get_order_id(id), Some(order));
                proptest::prop_assert!(tracker.contains(id));
            }
        }
    }

    #[test]
    fn multiple_correlations() {
        let mut tracker = CorrelationTracker::new();
        tracker.track("c1".to_owned(), "o1".to_owned());
        tracker.track("c2".to_owned(), "o2".to_owned());
        tracker.track("c3".to_owned(), "o3".to_owned());

        assert_eq!(tracker.len(), 3);
        assert_eq!(tracker.get_order_id("c1"), Some(&"o1".to_owned()));
        assert_eq!(tracker.get_order_id("c2"), Some(&"o2".to_owned()));
        assert_eq!(tracker.get_order_id("c3"), Some(&"o3".to_owned()));
    }
}
