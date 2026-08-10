//! Dhan live-feed reconnect scheduling — PURE LOGIC, no I/O, no tasks.
//!
//! Rebuilt for the 16-connection revival authorized by the operator on
//! 2026-08-09 (see `.claude/rules/project/websocket-connection-scope-lock.md`,
//! section "2026-08-09 (SAME DAY, SECOND QUOTE) — 16 CONNECTIONS +
//! depth-20/depth-200 AUTHORIZED"). The Dhan main-feed reconnect machinery was
//! deleted 2026-07-17 with the retired lane; this module restores the *decision*
//! half of it so the ladder can be exhaustively tested before anything opens a
//! socket.
//!
//! # What lives here
//! - [`reconnect_delay_ms`] — the ladder itself: `0ms, 1s, 2s, 5s, 15s`, then a
//!   `30s` cap forever.
//! - [`reconnect_jitter_ms`] — deterministic per-connection stagger that stops
//!   sixteen simultaneously-dropped sockets from reconnecting in lockstep.
//! - [`reconnect_delay_with_jitter_ms`] — the two composed.
//!
//! # Why the first attempt is instant
//! House precedent, unchanged since Phase 0 Item 4 (2026-05-15): the first
//! reconnect attempt of both surviving WebSocket surfaces is `0ms`
//! (`order_update_connection::compute_reconnect_backoff_ms` returns 0 for the
//! first failure). A socket reset is overwhelmingly a transient TCP event and
//! Dhan accepts an immediate re-dial; sleeping first would add latency to the
//! common case for no benefit. Backoff exists to protect Dhan (and our
//! account-level rate limit) from a *persistent* failure, which the ladder's
//! later rungs cover.
//!
//! # Why the cap is 30 seconds and not unbounded exponential
//! An unbounded ladder eventually parks a dead pool for minutes, and this feed
//! has NO snapshot-on-subscribe (documented only for the US global-stocks
//! socket, feed code 29) — a reconnected instrument shows a price only when it
//! next trades. A long parked window therefore compounds into a long blind
//! window. 30s bounds the worst case while still being ~30x cheaper than a
//! 1s hammer against a Dhan-side outage.
//!
//! # Allocation
//! Every function here is integer arithmetic over a `const` table. No heap
//! allocation, no locks, no clock reads. Safe to call from a reconnect
//! decision point at any rate.

use super::types::ConnectionId;

// ---------------------------------------------------------------------------
// Ladder constants
// ---------------------------------------------------------------------------

/// The reconnect ladder, in milliseconds, indexed by zero-based attempt number.
///
/// `attempt 0` is the FIRST reconnect attempt after a drop and is deliberately
/// `0ms` — see the module docs for why. Attempts beyond the end of this table
/// use [`RECONNECT_LADDER_CAP_MS`].
///
/// Shape (`0, 1s, 2s, 5s, 15s`) is the approved value from
/// `.claude/plans/proposals/2026-08-09-dhan-16-connection-architecture.md`
/// ("Thresholds" table). It is intentionally NOT a pure doubling: the 2s→5s
/// and 5s→15s steps widen faster than 2x so a genuine Dhan-side outage reaches
/// the cheap steady state in five attempts rather than nine.
pub const RECONNECT_LADDER_MS: [u64; 5] = [0, 1_000, 2_000, 5_000, 15_000];

/// Steady-state reconnect delay once the ladder is exhausted. Applied forever —
/// the pool never gives up on its own; a give-up decision belongs to the
/// market-hours gate, not to the ladder.
pub const RECONNECT_LADDER_CAP_MS: u64 = 30_000;

// ---------------------------------------------------------------------------
// Jitter constants (thundering-herd prevention)
// ---------------------------------------------------------------------------

/// Milliseconds of stagger added per connection slot.
///
/// 25ms x 15 slots = 375ms of total spread. Chosen so the spread is
/// (a) large enough that sixteen TCP handshakes plus sixteen JSON subscribe
/// bursts do not land inside the same scheduler tick, and
/// (b) small enough to be negligible against the multi-second reconnect +
/// resubscribe + first-frame path, so it never meaningfully delays recovery.
pub const RECONNECT_JITTER_STEP_MS: u64 = 25;

/// Number of distinct jitter slots. Equal to the operator-authorized total
/// connection ceiling (16), so every live connection lands in its own slot and
/// no two connections ever share a delay.
pub const RECONNECT_JITTER_SLOTS: u8 = 16;

/// Largest stagger any connection can receive: `STEP * (SLOTS - 1)`.
/// Pinned against its factors by
/// `test_reconnect_jitter_ms_max_constant_matches_step_and_slots`.
pub const RECONNECT_JITTER_MAX_MS: u64 = 375;

/// Upper bound on any value [`reconnect_delay_with_jitter_ms`] can return.
///
/// NOTE: this deliberately EXCEEDS [`RECONNECT_LADDER_CAP_MS`]. Clamping the
/// jittered value back down to the cap would collapse all sixteen connections
/// onto the same 30s delay in the steady state — i.e. it would re-create the
/// thundering herd precisely in the situation (a long Dhan outage) where the
/// herd is most likely. The cap is therefore a cap on the *ladder*, and the
/// jitter rides on top of it.
pub const RECONNECT_DELAY_WITH_JITTER_MAX_MS: u64 =
    RECONNECT_LADDER_CAP_MS + RECONNECT_JITTER_MAX_MS;

// ---------------------------------------------------------------------------
// Pure decision functions
// ---------------------------------------------------------------------------

/// Returns the base reconnect delay in milliseconds for a zero-based `attempt`.
///
/// `attempt` counts reconnect attempts since the connection was last healthy:
/// `0` is the first attempt after the drop, `1` the second, and so on. The
/// caller resets it to `0` on every successful connect.
///
/// Total for all `u32` inputs — saturates onto [`RECONNECT_LADDER_CAP_MS`]
/// rather than overflowing or panicking.
///
/// ```
/// # use tickvault_core::websocket::reconnect_ladder::reconnect_delay_ms;
/// assert_eq!(reconnect_delay_ms(0), 0);
/// assert_eq!(reconnect_delay_ms(4), 15_000);
/// assert_eq!(reconnect_delay_ms(u32::MAX), 30_000);
/// ```
#[must_use]
pub fn reconnect_delay_ms(attempt: u32) -> u64 {
    // `try_from` rather than `as`: total on every platform width, and a value
    // that cannot be represented falls through to the cap, which is the
    // fail-safe direction (wait longer, never hammer).
    let idx = usize::try_from(attempt).unwrap_or(usize::MAX);
    match RECONNECT_LADDER_MS.get(idx) {
        Some(&ms) => ms,
        None => RECONNECT_LADDER_CAP_MS,
    }
}

/// Returns the deterministic per-connection stagger in milliseconds.
///
/// Derived from the connection's GLOBAL index (0..16 across all four pools —
/// see `pool_budget::ConnectionSlot::global_index`), NOT from randomness.
/// Randomness would make the reconnect schedule untestable and irreproducible
/// in incident forensics; a fixed derivation gives the same de-synchronisation
/// with none of that cost.
///
/// Indices at or beyond [`RECONNECT_JITTER_SLOTS`] wrap. That is unreachable in
/// production — the pool budget refuses to open a seventeenth connection — but
/// the function stays total rather than relying on that.
#[must_use]
pub fn reconnect_jitter_ms(global_connection_index: ConnectionId) -> u64 {
    let slot = global_connection_index % RECONNECT_JITTER_SLOTS;
    u64::from(slot).saturating_mul(RECONNECT_JITTER_STEP_MS)
}

/// The ladder delay for `attempt`, plus this connection's fixed stagger.
///
/// This is the value a reconnect loop should actually sleep. Connection index
/// `0` always receives zero stagger, so at least one connection in every pool
/// keeps the exact "instant first attempt" behaviour; the remaining fifteen
/// fan out over [`RECONNECT_JITTER_MAX_MS`].
#[must_use]
pub fn reconnect_delay_with_jitter_ms(attempt: u32, global_connection_index: ConnectionId) -> u64 {
    reconnect_delay_ms(attempt).saturating_add(reconnect_jitter_ms(global_connection_index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    // -- exact sequence -----------------------------------------------------

    #[test]
    fn test_reconnect_delay_ms_exact_ladder_sequence() {
        // The approved sequence, asserted literally so a table edit is a
        // deliberate, visible change rather than a silent drift.
        assert_eq!(reconnect_delay_ms(0), 0, "attempt 0 must be instant");
        assert_eq!(reconnect_delay_ms(1), 1_000);
        assert_eq!(reconnect_delay_ms(2), 2_000);
        assert_eq!(reconnect_delay_ms(3), 5_000);
        assert_eq!(reconnect_delay_ms(4), 15_000);
    }

    #[test]
    fn test_reconnect_delay_ms_caps_at_thirty_seconds_forever() {
        for attempt in [5_u32, 6, 7, 8, 100, 10_000, u32::MAX - 1, u32::MAX] {
            assert_eq!(
                reconnect_delay_ms(attempt),
                RECONNECT_LADDER_CAP_MS,
                "attempt {attempt} must sit on the 30s cap"
            );
        }
    }

    #[test]
    fn test_reconnect_delay_ms_first_attempt_is_instant() {
        assert_eq!(reconnect_delay_ms(0), 0);
    }

    #[test]
    fn test_reconnect_delay_ms_boundary_at_end_of_table() {
        let last = RECONNECT_LADDER_MS.len() - 1;
        let last_u32 = u32::try_from(last).unwrap_or(u32::MAX);
        assert_eq!(reconnect_delay_ms(last_u32), 15_000, "last table rung");
        assert_eq!(
            reconnect_delay_ms(last_u32 + 1),
            RECONNECT_LADDER_CAP_MS,
            "first attempt past the table must be the cap"
        );
    }

    // -- table invariants ---------------------------------------------------

    #[test]
    fn test_reconnect_delay_ms_table_is_itself_non_decreasing() {
        // Guards a future table edit: monotonicity of the public function is
        // only true while the table itself is sorted.
        for pair in RECONNECT_LADDER_MS.windows(2) {
            if let [a, b] = pair {
                assert!(a <= b, "ladder table must be non-decreasing: {a} > {b}");
            }
        }
        let tail = RECONNECT_LADDER_MS.last().copied().unwrap_or(0);
        assert!(
            tail <= RECONNECT_LADDER_CAP_MS,
            "last rung {tail} must not exceed the cap {RECONNECT_LADDER_CAP_MS}"
        );
    }

    #[test]
    fn test_reconnect_jitter_ms_max_constant_matches_step_and_slots() {
        let expected = RECONNECT_JITTER_STEP_MS * u64::from(RECONNECT_JITTER_SLOTS - 1);
        assert_eq!(
            RECONNECT_JITTER_MAX_MS, expected,
            "RECONNECT_JITTER_MAX_MS must equal STEP * (SLOTS - 1)"
        );
        assert_eq!(
            RECONNECT_DELAY_WITH_JITTER_MAX_MS,
            RECONNECT_LADDER_CAP_MS + RECONNECT_JITTER_MAX_MS
        );
    }

    // -- jitter -------------------------------------------------------------

    #[test]
    fn test_reconnect_jitter_ms_zero_for_first_connection() {
        assert_eq!(
            reconnect_jitter_ms(0),
            0,
            "connection 0 keeps the exact instant-first-attempt behaviour"
        );
    }

    #[test]
    fn test_reconnect_jitter_ms_distinct_across_sixteen_connections() {
        let mut seen = BTreeSet::new();
        for idx in 0..RECONNECT_JITTER_SLOTS {
            let jitter = reconnect_jitter_ms(idx);
            assert!(
                seen.insert(jitter),
                "connection {idx} collided on jitter {jitter}ms — thundering-herd \
                 prevention requires all 16 to differ"
            );
        }
        assert_eq!(seen.len(), 16);
        assert_eq!(
            seen.iter().next_back().copied(),
            Some(RECONNECT_JITTER_MAX_MS)
        );
    }

    #[test]
    fn test_reconnect_jitter_ms_wraps_beyond_slot_count() {
        // Unreachable in production (the pool budget refuses a 17th socket)
        // but the function must stay total.
        assert_eq!(reconnect_jitter_ms(16), reconnect_jitter_ms(0));
        assert_eq!(reconnect_jitter_ms(17), reconnect_jitter_ms(1));
        assert_eq!(reconnect_jitter_ms(u8::MAX), reconnect_jitter_ms(15));
    }

    #[test]
    fn test_reconnect_jitter_ms_is_deterministic_across_calls() {
        for idx in 0..=u8::MAX {
            assert_eq!(
                reconnect_jitter_ms(idx),
                reconnect_jitter_ms(idx),
                "jitter must be a pure function of the index — no randomness"
            );
        }
    }

    // -- composed -----------------------------------------------------------

    #[test]
    fn test_reconnect_delay_with_jitter_ms_preserves_instant_for_connection_zero() {
        assert_eq!(reconnect_delay_with_jitter_ms(0, 0), 0);
    }

    #[test]
    fn test_reconnect_delay_with_jitter_ms_distinct_across_sixteen_at_every_rung() {
        // The herd scenario: all sixteen sockets drop at the same instant and
        // walk the ladder together. At EVERY rung they must remain separated.
        for attempt in [0_u32, 1, 2, 3, 4, 5, 50, u32::MAX] {
            let mut seen = BTreeSet::new();
            for idx in 0..RECONNECT_JITTER_SLOTS {
                let delay = reconnect_delay_with_jitter_ms(attempt, idx);
                assert!(
                    seen.insert(delay),
                    "attempt {attempt}: connection {idx} collided on {delay}ms"
                );
            }
            assert_eq!(
                seen.len(),
                16,
                "attempt {attempt} must yield 16 distinct delays"
            );
        }
    }

    #[test]
    fn test_reconnect_delay_with_jitter_ms_exact_values_at_the_cap() {
        assert_eq!(reconnect_delay_with_jitter_ms(5, 0), 30_000);
        assert_eq!(reconnect_delay_with_jitter_ms(5, 1), 30_025);
        assert_eq!(
            reconnect_delay_with_jitter_ms(5, 15),
            RECONNECT_DELAY_WITH_JITTER_MAX_MS
        );
    }

    // -- properties ---------------------------------------------------------

    proptest! {
        #[test]
        fn test_reconnect_delay_ms_never_exceeds_cap(attempt in any::<u32>()) {
            prop_assert!(reconnect_delay_ms(attempt) <= RECONNECT_LADDER_CAP_MS);
        }

        #[test]
        fn test_reconnect_delay_ms_is_monotonic_non_decreasing(
            a in any::<u32>(),
            b in any::<u32>(),
        ) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            prop_assert!(
                reconnect_delay_ms(lo) <= reconnect_delay_ms(hi),
                "delay({lo}) = {} must be <= delay({hi}) = {}",
                reconnect_delay_ms(lo),
                reconnect_delay_ms(hi),
            );
        }

        #[test]
        fn test_reconnect_delay_ms_adjacent_attempts_never_shrink(attempt in any::<u32>()) {
            let next = attempt.saturating_add(1);
            prop_assert!(reconnect_delay_ms(attempt) <= reconnect_delay_ms(next));
        }

        #[test]
        fn test_reconnect_jitter_ms_bounded(idx in any::<u8>()) {
            let jitter = reconnect_jitter_ms(idx);
            prop_assert!(jitter <= RECONNECT_JITTER_MAX_MS);
            prop_assert_eq!(jitter % RECONNECT_JITTER_STEP_MS, 0);
        }

        #[test]
        fn test_reconnect_delay_with_jitter_ms_bounded(
            attempt in any::<u32>(),
            idx in any::<u8>(),
        ) {
            let delay = reconnect_delay_with_jitter_ms(attempt, idx);
            prop_assert!(delay <= RECONNECT_DELAY_WITH_JITTER_MAX_MS);
            prop_assert!(delay >= reconnect_delay_ms(attempt));
        }

        #[test]
        fn test_reconnect_delay_with_jitter_ms_monotonic_in_attempt(
            attempt in any::<u32>(),
            idx in any::<u8>(),
        ) {
            let next = attempt.saturating_add(1);
            prop_assert!(
                reconnect_delay_with_jitter_ms(attempt, idx)
                    <= reconnect_delay_with_jitter_ms(next, idx)
            );
        }
    }
}
