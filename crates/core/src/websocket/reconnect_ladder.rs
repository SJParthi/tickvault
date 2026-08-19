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
//! - [`damped_reconnect_delay`] / [`damped_reconnect_delay_with_jitter`] — the
//!   FLAP DAMPER: the ladder plus the conditions under which rung 0 is
//!   actually earned. This is what a reconnect loop must call.
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

/// Upper bound on any value [`damped_reconnect_delay_with_jitter`] can return.
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
// Flap damper constants (2026-08-19)
// ---------------------------------------------------------------------------
//
// WHY THIS EXISTS. `connection.rs` reports EVERY transport error — including a
// bare TCP reset with no close frame — as `Closed { code: None }`, and
// `classify_disconnect` maps an absent code to `Transient`, which is the right
// fail-safe default. But `Transient` walks this ladder, and rung 0 is `0ms`.
// So a socket killed by Dhan's 805 ("too many connections") delivered as an
// RST re-dials INSTANTLY.
//
// That is not merely impolite, it is self-amplifying. Dhan's 805 semantics
// (`15-live-market-feed.md:209`) kill the OLDEST socket per extra connection,
// so an instant re-dial evicts a healthy sibling, which is itself RST'd, which
// re-dials instantly, which evicts another. Sixteen sockets, one damper-free
// ladder, and a cascade that sustains itself. This repo has already recorded
// the raw pattern: `docs/dhan-support/2026-07-02-mainfeed-tcp-resets.md`, and
// `websocket-connection-scope-lock.md` §E row 6 ("7 bare-RST main-feed
// disconnect cycles"). Nothing coordinates the sixteen supervisors after boot
// — they are `mem::replace`d out before spawning — so the damper has to be
// PER-SOCKET and needs no cross-socket channel to work.
//
// The instant rung is NOT removed. It is made CONDITIONAL: it is a latency win
// for a clean, isolated drop after a genuinely healthy session, and that case
// keeps it exactly. What it stops being is the default for a socket that never
// proved anything.

/// How long a connection must have been carrying frames before its next drop
/// earns the instant first retry.
///
/// 30 seconds is not a round number picked for feel: it is one full idle-
/// watchdog window plus margin. `idle_watchdog::IDLE_RECONNECT_TIMEOUT_SECS`
/// is 27s, so a session that delivered frames for 30s has demonstrably
/// survived a complete silence window without the watchdog firing — it was a
/// working socket, not a connect blip. Anything shorter cannot make that claim.
///
/// This is the constant that makes "proven healthy" mean *carried frames for a
/// while* rather than the old *received exactly one frame*, which a socket that
/// dies immediately after its prev-close packet satisfies trivially.
pub const MIN_HEALTHY_SESSION_MS: u64 = 30_000;

/// Floor applied to the re-dial delay when the previous session was shorter
/// than [`MIN_HEALTHY_SESSION_MS`].
///
/// Equal to the ladder's first non-zero rung: enough to break a tight
/// connect/RST/connect loop (at 16 sockets that is at most 16 dials/second
/// worst case instead of an unbounded spin), small enough that a socket which
/// really did hit an isolated transient loses only a second of recovery.
pub const SHORT_SESSION_REDIAL_FLOOR_MS: u64 = RECONNECT_LADDER_MS[1];

/// Rolling window over which re-dials are counted for the flap ceiling.
///
/// Five minutes. Long enough that three drops inside it is genuinely
/// pathological rather than bad luck (a healthy socket in this pool is
/// expected to reconnect on the order of once per session, plus the one daily
/// reconnect the 24h JWT forces by law), and short enough that a socket which
/// recovers is released from the ceiling within a single market hour rather
/// than being punished for the rest of the day.
pub const FLAP_WINDOW_MS: u64 = 300_000;

/// Re-dials within [`FLAP_WINDOW_MS`] at or above which a socket is treated as
/// flapping, whatever its apparent health.
///
/// SIX, and the value is chosen so the ceiling never fights the ladder.
/// [`RECONNECT_LADDER_MS`] has five rungs, so a socket whose attempt counter is
/// genuinely climbing — a plain Dhan-side outage, where every dial fails —
/// reaches the 30s steady state on its own by the sixth re-dial. Setting the
/// ceiling at or below five would let it swallow the `5s` and `15s` rungs and
/// jump straight to the cap, turning a ten-second Dhan blip into a ~30s blind
/// window on a feed that has no snapshot-on-subscribe. That would be the
/// damper making recovery WORSE on the most common failure it will ever see.
///
/// At six it bites only the case the ladder structurally cannot see: a socket
/// whose attempt counter is RESET on every cycle because a frame arrived, so
/// the ladder restarts at rung 0 forever and never escalates at all. That is
/// the second defect this damper exists for, and it is unbounded without a
/// ceiling.
///
/// Six re-dials inside five minutes is unambiguously pathological: a healthy
/// socket in this pool reconnects on the order of once per session, plus the
/// one daily reconnect the 24h JWT forces by law.
pub const FLAP_REDIAL_CEILING: u32 = 6;

/// Delay floor forced on a socket that has crossed [`FLAP_REDIAL_CEILING`].
///
/// Deliberately the ladder's own steady-state cap rather than a new magic
/// number: a confirmed flapper IS the "persistent failure" the cap was written
/// for, so it should sit where a persistently failing socket sits. This bounds
/// a flapping socket to at most two dials per minute regardless of how healthy
/// each short-lived session looked, which is the actual ceiling on the cascade.
pub const FLAP_CEILING_REDIAL_FLOOR_MS: u64 = RECONNECT_LADDER_CAP_MS;

/// Counter: a re-dial the flap damper slowed down. Labels: `endpoint`, `reason`.
///
/// The damper must never act silently — a socket quietly held at 30s while the
/// operator believes the ladder is running is the false-OK class the house
/// rules forbid. Every non-`Ladder` verdict increments this.
pub const FLAP_DAMPED_METRIC: &str = "tv_dhan_ws_reconnect_damped_total";

// ---------------------------------------------------------------------------
// Pure decision functions
// ---------------------------------------------------------------------------

/// Why a re-dial delay is what it is.
///
/// Returned alongside the delay so the caller can both sleep and COUNT without
/// re-deriving the decision (and without the two drifting apart).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlapVerdict {
    /// The damper did not change the delay: either it had nothing to say, or
    /// its floor was already at or below the ladder rung the socket had
    /// earned. Either way the raw ladder value applies and NOTHING is counted.
    Ladder,
    /// The previous session was shorter than [`MIN_HEALTHY_SESSION_MS`], so the
    /// instant rung was withheld and [`SHORT_SESSION_REDIAL_FLOOR_MS`] applied.
    ShortSession,
    /// The socket has re-dialled at least [`FLAP_REDIAL_CEILING`] times inside
    /// [`FLAP_WINDOW_MS`]; [`FLAP_CEILING_REDIAL_FLOOR_MS`] applied regardless
    /// of apparent health.
    FlapCeiling,
}

impl FlapVerdict {
    /// Every verdict, for baseline pre-registration of [`FLAP_DAMPED_METRIC`].
    pub const ALL: [Self; 3] = [Self::Ladder, Self::ShortSession, Self::FlapCeiling];

    /// Whether the damper actually RAISED the delay. Only these are counted —
    /// see `verdict_for` in [`damped_reconnect_delay`] for why a no-op floor
    /// must not be reported as an action.
    #[must_use]
    pub const fn is_damped(self) -> bool {
        !matches!(self, Self::Ladder)
    }

    /// Stable lowercase tag for logs and metric labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ladder => "ladder",
            Self::ShortSession => "short_session",
            Self::FlapCeiling => "flap_ceiling",
        }
    }
}

/// A re-dial delay together with the reason it was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectDecision {
    /// Milliseconds the caller must sleep before re-dialing.
    pub delay_ms: u64,
    /// Why. See [`FlapVerdict`].
    pub verdict: FlapVerdict,
}

/// The flap damper: the ladder rung for `attempt`, raised when the connection
/// has not earned an instant retry.
///
/// PURE. No clock reads, no allocation, no locks — the caller measures elapsed
/// time and passes it in, which is what makes every branch here reachable from
/// a unit test without a socket or a sleep.
///
/// - `healthy_duration_ms` — how long the connection that just dropped was
///   delivering frames. Zero if it never delivered any.
/// - `recent_redial_count` — re-dials by THIS socket inside [`FLAP_WINDOW_MS`],
///   counted by the caller.
///
/// The flap ceiling is checked FIRST and wins outright: a socket flapping
/// three times in five minutes is slowed down even when each individual
/// session looked healthy, which is exactly the case a health check alone
/// cannot catch.
///
/// ```
/// # use tickvault_core::websocket::reconnect_ladder::{
/// #     damped_reconnect_delay, FlapVerdict, MIN_HEALTHY_SESSION_MS,
/// # };
/// // A long healthy session that drops once: instant retry preserved.
/// let d = damped_reconnect_delay(0, MIN_HEALTHY_SESSION_MS, 0);
/// assert_eq!(d.delay_ms, 0);
/// assert_eq!(d.verdict, FlapVerdict::Ladder);
///
/// // One frame then gone: no instant retry.
/// let d = damped_reconnect_delay(0, 40, 0);
/// assert_eq!(d.verdict, FlapVerdict::ShortSession);
/// assert!(d.delay_ms > 0);
/// ```
#[must_use]
pub fn damped_reconnect_delay(
    attempt: u32,
    healthy_duration_ms: u64,
    recent_redial_count: u32,
) -> ReconnectDecision {
    let base = reconnect_delay_ms(attempt);

    // A floor that does not actually raise the delay is NOT a damping action,
    // and must not be reported as one. At attempt 1 the raw rung is already
    // 1,000ms, which equals SHORT_SESSION_REDIAL_FLOOR_MS — reporting that as
    // `ShortSession` would increment FLAP_DAMPED_METRIC for a re-dial the
    // damper left completely untouched, inflating the one number an operator
    // would use to judge whether the damper is doing anything. `verdict_for`
    // keeps the label honest: it names WHY only when there is a change to name.
    fn verdict_for(base: u64, floored: u64, verdict: FlapVerdict) -> ReconnectDecision {
        ReconnectDecision {
            delay_ms: floored,
            verdict: if floored > base {
                verdict
            } else {
                FlapVerdict::Ladder
            },
        }
    }

    if recent_redial_count >= FLAP_REDIAL_CEILING {
        return verdict_for(
            base,
            base.max(FLAP_CEILING_REDIAL_FLOOR_MS),
            FlapVerdict::FlapCeiling,
        );
    }

    if healthy_duration_ms < MIN_HEALTHY_SESSION_MS {
        return verdict_for(
            base,
            base.max(SHORT_SESSION_REDIAL_FLOOR_MS),
            FlapVerdict::ShortSession,
        );
    }

    ReconnectDecision {
        delay_ms: base,
        verdict: FlapVerdict::Ladder,
    }
}

/// [`damped_reconnect_delay`] plus this connection's fixed per-slot stagger.
///
/// This is the value a reconnect loop should actually sleep. The stagger rides
/// ON TOP of the damped delay for the same reason it rides on top of the
/// ladder cap (see [`RECONNECT_DELAY_WITH_JITTER_MAX_MS`]): clamping it away
/// would collapse sixteen damped sockets onto one identical delay, re-creating
/// the herd in precisely the situation the damper was entered for.
#[must_use]
pub fn damped_reconnect_delay_with_jitter(
    attempt: u32,
    healthy_duration_ms: u64,
    recent_redial_count: u32,
    global_connection_index: ConnectionId,
) -> ReconnectDecision {
    let decision = damped_reconnect_delay(attempt, healthy_duration_ms, recent_redial_count);
    ReconnectDecision {
        delay_ms: decision
            .delay_ms
            .saturating_add(reconnect_jitter_ms(global_connection_index)),
        verdict: decision.verdict,
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    /// The composed delay for a socket the damper has nothing to say about:
    /// a proven-healthy, non-flapping connection. For that socket the damper
    /// is the identity, so this is exactly the raw ladder plus the stagger —
    /// which is what these herd/monotonicity tests are about.
    fn healthy_composed_ms(attempt: u32, idx: ConnectionId) -> u64 {
        damped_reconnect_delay_with_jitter(attempt, MIN_HEALTHY_SESSION_MS, 0, idx).delay_ms
    }

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
    fn test_damped_reconnect_delay_with_jitter_preserves_instant_for_connection_zero() {
        assert_eq!(healthy_composed_ms(0, 0), 0);
    }

    #[test]
    fn test_damped_reconnect_delay_with_jitter_distinct_across_sixteen_at_every_rung() {
        // The herd scenario: all sixteen sockets drop at the same instant and
        // walk the ladder together. At EVERY rung they must remain separated.
        for attempt in [0_u32, 1, 2, 3, 4, 5, 50, u32::MAX] {
            let mut seen = BTreeSet::new();
            for idx in 0..RECONNECT_JITTER_SLOTS {
                let delay = healthy_composed_ms(attempt, idx);
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
    fn test_damped_reconnect_delay_with_jitter_exact_values_at_the_cap() {
        assert_eq!(healthy_composed_ms(5, 0), 30_000);
        assert_eq!(healthy_composed_ms(5, 1), 30_025);
        assert_eq!(
            healthy_composed_ms(5, 15),
            RECONNECT_DELAY_WITH_JITTER_MAX_MS
        );
    }

    // -- flap damper --------------------------------------------------------

    /// The case the instant rung EXISTS for: a socket that carried frames for
    /// a genuinely healthy stretch, then took one isolated drop. It keeps 0ms.
    #[test]
    fn test_damped_reconnect_delay_preserves_the_instant_rung_after_a_healthy_session() {
        let d = damped_reconnect_delay(0, MIN_HEALTHY_SESSION_MS, 0);
        assert_eq!(
            d.delay_ms, 0,
            "a proven-healthy socket keeps the latency win"
        );
        assert_eq!(d.verdict, FlapVerdict::Ladder);
        assert!(
            !d.verdict.is_damped(),
            "an untouched rung is not a damped one"
        );

        // Far past the threshold — a whole trading session — still instant.
        let d = damped_reconnect_delay(0, 6 * 60 * 60 * 1_000, 0);
        assert_eq!(d.delay_ms, 0);
        assert_eq!(d.verdict, FlapVerdict::Ladder);
    }

    /// THE SECOND DEFECT. A socket that yields exactly one frame and drops has
    /// its attempt counter reset to 0 by the supervisor, so the raw ladder
    /// hands it 0ms — forever, with no ceiling. The damper must refuse.
    #[test]
    fn test_damped_reconnect_delay_refuses_the_instant_rung_after_one_frame() {
        // 40ms of "health": a prev-close packet on subscribe, then gone.
        let d = damped_reconnect_delay(0, 40, 0);
        assert_eq!(d.verdict, FlapVerdict::ShortSession);
        assert_eq!(d.delay_ms, SHORT_SESSION_REDIAL_FLOOR_MS);
        assert!(
            d.delay_ms > 0,
            "a one-frame socket must never re-dial instantly"
        );
        assert!(d.verdict.is_damped());
    }

    /// THE FIRST DEFECT, in its purest form: a bare TCP reset on a socket that
    /// never delivered anything at all.
    #[test]
    fn test_damped_reconnect_delay_refuses_the_instant_rung_with_zero_health() {
        let d = damped_reconnect_delay(0, 0, 0);
        assert_eq!(d.verdict, FlapVerdict::ShortSession);
        assert!(d.delay_ms >= SHORT_SESSION_REDIAL_FLOOR_MS);
    }

    /// The boundary is exact and inclusive on the healthy side.
    #[test]
    fn test_damped_reconnect_delay_boundary_at_min_healthy_session() {
        assert_eq!(
            damped_reconnect_delay(0, MIN_HEALTHY_SESSION_MS - 1, 0).verdict,
            FlapVerdict::ShortSession,
            "one millisecond short is still short"
        );
        assert_eq!(
            damped_reconnect_delay(0, MIN_HEALTHY_SESSION_MS, 0).verdict,
            FlapVerdict::Ladder,
            "exactly the threshold counts as healthy"
        );
    }

    /// The flap ceiling wins over apparent health — that is the entire point.
    /// A socket can look perfectly healthy on each individual session and
    /// still be flapping, and a health check alone cannot see it.
    #[test]
    fn test_damped_reconnect_delay_flap_ceiling_forces_backoff_despite_health() {
        let d = damped_reconnect_delay(0, MIN_HEALTHY_SESSION_MS * 10, FLAP_REDIAL_CEILING);
        assert_eq!(d.verdict, FlapVerdict::FlapCeiling);
        assert_eq!(d.delay_ms, FLAP_CEILING_REDIAL_FLOOR_MS);
        assert!(d.verdict.is_damped());

        // One below the ceiling is NOT clamped.
        let d = damped_reconnect_delay(0, MIN_HEALTHY_SESSION_MS * 10, FLAP_REDIAL_CEILING - 1);
        assert_eq!(d.verdict, FlapVerdict::Ladder);
        assert_eq!(d.delay_ms, 0);
    }

    /// The damper only ever LENGTHENS a delay. If it could shorten one it
    /// would be a new cascade source rather than a fix for the old one.
    #[test]
    fn test_damped_reconnect_delay_never_shortens_the_raw_ladder() {
        for attempt in [0_u32, 1, 2, 3, 4, 5, 9, 1_000, u32::MAX] {
            for health in [
                0_u64,
                1,
                MIN_HEALTHY_SESSION_MS - 1,
                MIN_HEALTHY_SESSION_MS,
                u64::MAX,
            ] {
                for count in [
                    0_u32,
                    1,
                    FLAP_REDIAL_CEILING - 1,
                    FLAP_REDIAL_CEILING,
                    u32::MAX,
                ] {
                    let d = damped_reconnect_delay(attempt, health, count);
                    assert!(
                        d.delay_ms >= reconnect_delay_ms(attempt),
                        "attempt {attempt} health {health} count {count}: damper shortened \
                         {} below the raw rung {}",
                        d.delay_ms,
                        reconnect_delay_ms(attempt),
                    );
                }
            }
        }
    }

    /// The ceiling of 6 is chosen so it never swallows a ladder rung on a
    /// socket that is genuinely escalating. If this fails, the damper has
    /// started making an ordinary Dhan outage recover more slowly.
    #[test]
    fn test_damped_reconnect_delay_flap_ceiling_never_swallows_a_climbing_ladder() {
        // A socket failing every dial climbs one rung per re-dial, so at
        // re-dial N its raw rung is RECONNECT_LADDER_MS[N] (then the cap).
        // By the time the ceiling engages, the raw rung must already be at
        // least as large — otherwise the ceiling is escalating for it.
        let ceiling = usize::try_from(FLAP_REDIAL_CEILING).unwrap_or(usize::MAX);
        assert!(
            ceiling >= RECONNECT_LADDER_MS.len(),
            "FLAP_REDIAL_CEILING ({FLAP_REDIAL_CEILING}) must be at least the ladder length \
             ({}) or it clamps rungs a climbing socket has legitimately earned",
            RECONNECT_LADDER_MS.len(),
        );
        assert_eq!(
            damped_reconnect_delay(FLAP_REDIAL_CEILING, 0, FLAP_REDIAL_CEILING).delay_ms,
            FLAP_CEILING_REDIAL_FLOOR_MS,
            "at the ceiling the raw ladder is already at its cap, so clamping is a no-op"
        );
    }

    #[test]
    fn test_damped_reconnect_delay_with_jitter_keeps_sixteen_sockets_separated() {
        // The herd case the damper must not re-create: sixteen short-lived
        // sockets all damped to the same floor would wake in one tick.
        let mut seen = BTreeSet::new();
        for idx in 0..RECONNECT_JITTER_SLOTS {
            let d = damped_reconnect_delay_with_jitter(0, 0, 0, idx);
            assert_eq!(d.verdict, FlapVerdict::ShortSession);
            assert!(seen.insert(d.delay_ms), "connection {idx} collided");
        }
        assert_eq!(seen.len(), 16);
    }

    #[test]
    fn test_damped_reconnect_delay_with_jitter_matches_the_undamped_core_plus_stagger() {
        for idx in [0_u8, 1, 7, 15] {
            for health in [0_u64, MIN_HEALTHY_SESSION_MS] {
                for count in [0_u32, FLAP_REDIAL_CEILING] {
                    let core = damped_reconnect_delay(2, health, count);
                    let jittered = damped_reconnect_delay_with_jitter(2, health, count, idx);
                    assert_eq!(jittered.verdict, core.verdict);
                    assert_eq!(
                        jittered.delay_ms,
                        core.delay_ms + reconnect_jitter_ms(idx),
                        "the stagger must ride ON TOP of the damped delay"
                    );
                }
            }
        }
    }

    /// `is_damped` gates the counter, so it must be exactly "the delay moved".
    /// A verdict that reports undamped while raising the delay would be a
    /// silent damper — the false-OK class the house rules forbid.
    #[test]
    fn test_flap_verdict_is_damped_matches_whether_the_delay_actually_moved() {
        for (health, count) in [
            (MIN_HEALTHY_SESSION_MS, 0_u32),
            (0, 0),
            (MIN_HEALTHY_SESSION_MS, FLAP_REDIAL_CEILING),
            (0, FLAP_REDIAL_CEILING),
        ] {
            for attempt in [0_u32, 1, 3, 9] {
                let d = damped_reconnect_delay(attempt, health, count);
                let raw = reconnect_delay_ms(attempt);
                assert_eq!(
                    d.verdict.is_damped(),
                    d.delay_ms != raw,
                    "attempt {attempt} health {health} count {count}: verdict {:?} claims                      damped={} but the delay went {raw} -> {}",
                    d.verdict,
                    d.verdict.is_damped(),
                    d.delay_ms,
                );
            }
        }
    }

    #[test]
    fn test_flap_verdict_labels_are_distinct_and_all_is_complete() {
        assert_eq!(FlapVerdict::ALL.len(), 3);
        let labels: BTreeSet<&'static str> = FlapVerdict::ALL.iter().map(|v| v.as_str()).collect();
        assert_eq!(labels.len(), 3, "metric labels must not collide");
        assert!(!FlapVerdict::Ladder.is_damped());
        assert!(FlapVerdict::ShortSession.is_damped());
        assert!(FlapVerdict::FlapCeiling.is_damped());
    }

    #[test]
    fn test_flap_damper_constants_are_internally_consistent() {
        assert_eq!(
            SHORT_SESSION_REDIAL_FLOOR_MS, RECONNECT_LADDER_MS[1],
            "the short-session floor is the ladder's first non-zero rung by construction"
        );
        assert!(
            SHORT_SESSION_REDIAL_FLOOR_MS > 0,
            "a zero floor would be no damper at all"
        );
        assert!(
            FLAP_CEILING_REDIAL_FLOOR_MS >= SHORT_SESSION_REDIAL_FLOOR_MS,
            "the flap ceiling must be at least as strict as the short-session floor"
        );
        assert!(
            MIN_HEALTHY_SESSION_MS
                > super::super::idle_watchdog::IDLE_RECONNECT_TIMEOUT_SECS * 1_000,
            "a 'healthy' session must outlive one full idle-watchdog window"
        );
        assert!(
            FLAP_WINDOW_MS > MIN_HEALTHY_SESSION_MS,
            "the flap window must span several sessions to be able to count them"
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
        fn test_damped_reconnect_delay_with_jitter_bounded(
            attempt in any::<u32>(),
            idx in any::<u8>(),
        ) {
            let delay = healthy_composed_ms(attempt, idx);
            prop_assert!(delay <= RECONNECT_DELAY_WITH_JITTER_MAX_MS);
            prop_assert!(delay >= reconnect_delay_ms(attempt));
        }

        #[test]
        fn test_damped_reconnect_delay_is_total_and_never_below_the_raw_rung(
            attempt in any::<u32>(),
            healthy_duration_ms in any::<u64>(),
            recent_redial_count in any::<u32>(),
        ) {
            let d = damped_reconnect_delay(attempt, healthy_duration_ms, recent_redial_count);
            prop_assert!(d.delay_ms >= reconnect_delay_ms(attempt));
            // Instant is reachable ONLY on rung 0 of a healthy, non-flapping
            // socket. Every other zero would be a cascade door left open.
            if d.delay_ms == 0 {
                prop_assert_eq!(attempt, 0);
                prop_assert!(healthy_duration_ms >= MIN_HEALTHY_SESSION_MS);
                prop_assert!(recent_redial_count < FLAP_REDIAL_CEILING);
                prop_assert_eq!(d.verdict, FlapVerdict::Ladder);
            }
        }

        #[test]
        fn test_damped_reconnect_delay_with_jitter_is_bounded(
            attempt in any::<u32>(),
            healthy_duration_ms in any::<u64>(),
            recent_redial_count in any::<u32>(),
            idx in any::<u8>(),
        ) {
            let d = damped_reconnect_delay_with_jitter(
                attempt, healthy_duration_ms, recent_redial_count, idx,
            );
            // The damper's own floors are all <= the ladder cap, so the
            // existing global bound still holds and no new one is needed.
            prop_assert!(d.delay_ms <= RECONNECT_DELAY_WITH_JITTER_MAX_MS);
        }

        #[test]
        fn test_damped_reconnect_delay_with_jitter_monotonic_in_attempt(
            attempt in any::<u32>(),
            idx in any::<u8>(),
        ) {
            let next = attempt.saturating_add(1);
            prop_assert!(
                healthy_composed_ms(attempt, idx)
                    <= healthy_composed_ms(next, idx)
            );
        }
    }
}
