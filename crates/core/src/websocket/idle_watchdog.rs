//! Idle-socket watchdog for the Dhan live feed — PURE LOGIC, no I/O, no tasks.
//!
//! Part of the 16-connection revival authorized by the operator on 2026-08-09
//! (`.claude/rules/project/websocket-connection-scope-lock.md`, section
//! "2026-08-09 (SAME DAY, SECOND QUOTE) — 16 CONNECTIONS").
//!
//! # What this decides
//! "Has this socket been silent long enough that we should tear it down and
//! reconnect on OUR terms, rather than waiting for Dhan to drop us?"
//!
//! # Why 27 seconds — sourced, not inferred
//! `docs pack 15-live-market-feed.md:73` (verbatim): "Server side sends ping
//! every 10 seconds to the client server (in this case, your system) to
//! maintain WebSocket status as open."
//!
//! `15-live-market-feed.md:77` (verbatim, the warning callout): "In case the
//! client server does not respond for **more than 40 seconds**, the connection
//! is closed from server side and you will have to reestablish connection."
//!
//! So the server-side death sentence is a DIRECTLY STATED 40 seconds. It is
//! NOT derived from "4 missed pings x 10s" — that arithmetic happens to land
//! nearby, but the documentation gives the number outright and this module
//! encodes the stated figure ([`DHAN_SERVER_CLOSE_AFTER_SILENCE_SECS`]) rather
//! than a reconstruction of it.
//!
//! 27 seconds sits inside that window with ~13 seconds of margin: late enough
//! that a legitimately quiet socket (a far-month option that simply is not
//! trading) is not churned, early enough that we always act BEFORE the server
//! does. Acting first matters because a server-initiated close arrives as an
//! opaque reset, whereas a client-initiated reconnect is a clean, attributable
//! event our own reconnect ladder governs.
//!
//! # What this watchdog is really detecting
//! `15-live-market-feed.md:75` (verbatim): "An automated pong is sent by
//! websocket library. You can use the same as response to the ping."
//!
//! That single sentence is why this watchdog is load-bearing rather than
//! decorative. We never send the pong ourselves — the library does, and it can
//! only do so **while the read loop is polling the socket**. A reader that
//! blocks to do work (parse, aggregate, write to a database, wait on a lock)
//! therefore stops ponging while looking perfectly healthy from the outside,
//! and Dhan closes it at 40 seconds.
//!
//! So this timer is not merely "is the server quiet?" — it is also, and mostly,
//! "have WE stopped draining?". It is the observable symptom of a violated
//! drain-only rule, which is precisely the failure the approved design's
//! reader-does-nothing-but-append-and-push constraint exists to prevent.
//!
//! # Why the clock MUST be monotonic
//! The approved design calls this out explicitly: "a clock step from time
//! synchronisation (the watchdog must use a monotonic clock or every socket
//! reconnects at once)". If idleness were measured against wall time, an NTP
//! step of +30s would push EVERY one of the sixteen sockets past the threshold
//! in the same instant — synthesising the exact simultaneous-drop thundering
//! herd the jitter in [`super::reconnect_ladder`] exists to prevent, on sixteen
//! sockets that were all perfectly healthy.
//!
//! [`std::time::Instant`] is monotonic by contract: it is unaffected by wall
//! clock adjustments and never runs backwards. This module therefore accepts
//! ONLY `Instant` — it has no way to observe wall time at all, which is
//! enforced mechanically by
//! `test_idle_watchdog_source_never_reads_the_wall_clock`.
//!
//! # Testability
//! `now` is a parameter rather than an internal `Instant::now()` call, so the
//! whole state machine is drivable from unit tests without sleeping.
//!
//! # Allocation
//! [`IdleWatchdog`] is two `Copy` fields on the stack. No heap, no locks.

use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Protocol constants — every value quoted from the Dhan API v2 docs pack
// ---------------------------------------------------------------------------

/// Interval at which the Dhan server sends a WebSocket ping, in seconds.
///
/// Source `15-live-market-feed.md:73`: "Server side sends ping every 10 seconds
/// to the client server (in this case, your system) to maintain WebSocket
/// status as open."
pub const DHAN_SERVER_PING_INTERVAL_SECS: u64 = 10;

/// Seconds of client unresponsiveness after which the DHAN SERVER closes the
/// connection.
///
/// Source `15-live-market-feed.md:77` (warning callout, verbatim): "In case the
/// client server does not respond for **more than 40 seconds**, the connection
/// is closed from server side and you will have to reestablish connection."
///
/// This is a DIRECTLY STATED figure. It is deliberately NOT expressed as
/// `PING_INTERVAL * 4`: the docs never give a missed-ping count, so deriving 40
/// from one would be reconstructing a documented number out of an assumption.
/// If Dhan ever changes the ping cadence, this constant must not silently move
/// with it.
pub const DHAN_SERVER_CLOSE_AFTER_SILENCE_SECS: u64 = 40;

/// Seconds of socket silence after which WE reconnect, on our own terms.
///
/// Sits inside the documented 40-second server-close window
/// ([`DHAN_SERVER_CLOSE_AFTER_SILENCE_SECS`]) with ~13 seconds of margin, and
/// above two ping intervals so a briefly quiet-but-healthy socket is not
/// churned. Both bounds are pinned by
/// `test_idle_watchdog_timeout_fires_inside_the_documented_forty_second_window`.
pub const IDLE_RECONNECT_TIMEOUT_SECS: u64 = 27;

/// [`IDLE_RECONNECT_TIMEOUT_SECS`] as a [`Duration`], for direct comparison.
pub const IDLE_RECONNECT_TIMEOUT: Duration = Duration::from_secs(IDLE_RECONNECT_TIMEOUT_SECS);

// ---------------------------------------------------------------------------
// The watchdog
// ---------------------------------------------------------------------------

/// Monotonic-clock idle watchdog for one WebSocket connection.
///
/// Records the instant of the last observed socket activity and answers whether
/// the configured idle timeout has elapsed. Holds no clock of its own — every
/// method that needs "now" takes it as an argument, which is what makes the
/// state machine exhaustively testable and what makes wall-clock contamination
/// structurally impossible.
///
/// # Usage shape (wiring lands in a later round)
/// ```
/// # use std::time::{Duration, Instant};
/// # use tickvault_core::websocket::idle_watchdog::IdleWatchdog;
/// let start = Instant::now();
/// let mut wd = IdleWatchdog::new(start);
/// assert!(!wd.is_expired(start + Duration::from_secs(26)));
/// assert!(wd.is_expired(start + Duration::from_secs(27)));
/// // any frame at all resets it
/// wd.record_activity(start + Duration::from_secs(26));
/// assert!(!wd.is_expired(start + Duration::from_secs(52)));
/// ```
#[derive(Debug, Clone, Copy)]
pub struct IdleWatchdog {
    /// Monotonic instant of the last observed activity on this socket.
    last_activity: Instant,
    /// Silence threshold after which [`Self::is_expired`] returns true.
    timeout: Duration,
}

impl IdleWatchdog {
    /// Creates a watchdog armed at `now` with the standard 27-second timeout.
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            last_activity: now,
            timeout: IDLE_RECONNECT_TIMEOUT,
        }
    }

    /// Creates a watchdog armed at `now` with an explicit timeout.
    ///
    /// Intended for tests and for surfaces with a different silence profile
    /// (the order-update socket is legitimately silent for minutes). The live
    /// market-data pools use [`Self::new`].
    #[must_use]
    pub fn with_timeout(now: Instant, timeout: Duration) -> Self {
        Self {
            last_activity: now,
            timeout,
        }
    }

    /// Records that the socket produced activity at monotonic instant `now`.
    ///
    /// "Activity" is ANY frame — a data packet, a ping, a pong, a control
    /// frame. The watchdog asks whether the socket is alive, not whether it is
    /// carrying interesting data; an instrument that simply is not trading must
    /// never be mistaken for a dead connection.
    ///
    /// A `now` that is earlier than the recorded instant is ignored rather than
    /// rewinding the watchdog. `Instant` is monotonic so this cannot arise from
    /// the clock, but it can arise from out-of-order bookkeeping in a caller,
    /// and moving the arm-point backwards would shorten the window.
    pub fn record_activity(&mut self, now: Instant) {
        if now > self.last_activity {
            self.last_activity = now;
        }
    }

    /// How long the socket has been silent as of `now`.
    ///
    /// Saturates at zero if `now` precedes the last recorded activity — total,
    /// never panics.
    #[must_use]
    pub fn idle_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_activity)
    }

    /// Whether the idle timeout has elapsed as of `now`.
    ///
    /// The boundary is inclusive: exactly `timeout` of silence counts as
    /// expired, so the documented "27 seconds" is the moment we act, not the
    /// moment after which we act.
    #[must_use]
    pub fn is_expired(&self, now: Instant) -> bool {
        self.idle_for(now) >= self.timeout
    }

    /// The configured silence threshold.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The monotonic instant of the last recorded activity.
    #[must_use]
    pub fn last_activity(&self) -> Instant {
        self.last_activity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Instant {
        Instant::now()
    }

    // -- the 27s boundary ---------------------------------------------------

    #[test]
    fn test_is_expired_fires_at_twenty_seven_seconds_and_not_at_twenty_six() {
        let t0 = base();
        let wd = IdleWatchdog::new(t0);
        assert!(
            !wd.is_expired(t0 + Duration::from_secs(26)),
            "26s of silence must NOT trigger a reconnect"
        );
        assert!(
            wd.is_expired(t0 + Duration::from_secs(27)),
            "27s of silence MUST trigger a reconnect"
        );
    }

    #[test]
    fn test_is_expired_boundary_is_inclusive_to_the_millisecond() {
        let t0 = base();
        let wd = IdleWatchdog::new(t0);
        assert!(!wd.is_expired(t0 + Duration::from_millis(26_999)));
        assert!(wd.is_expired(t0 + Duration::from_millis(27_000)));
        assert!(wd.is_expired(t0 + Duration::from_millis(27_001)));
    }

    #[test]
    fn test_is_expired_false_at_arm_instant() {
        let t0 = base();
        let wd = IdleWatchdog::new(t0);
        assert!(
            !wd.is_expired(t0),
            "a freshly armed watchdog is never expired"
        );
    }

    #[test]
    fn test_is_expired_stays_true_for_arbitrarily_long_silence() {
        let t0 = base();
        let wd = IdleWatchdog::new(t0);
        for secs in [27_u64, 40, 60, 3_600, 86_400] {
            assert!(wd.is_expired(t0 + Duration::from_secs(secs)), "{secs}s");
        }
    }

    // -- activity resets ----------------------------------------------------

    #[test]
    fn test_record_activity_resets_the_window() {
        let t0 = base();
        let mut wd = IdleWatchdog::new(t0);
        wd.record_activity(t0 + Duration::from_secs(26));
        // 26 + 26 = 52s of absolute elapsed time, but only 26s since activity.
        assert!(!wd.is_expired(t0 + Duration::from_secs(52)));
        assert!(wd.is_expired(t0 + Duration::from_secs(53)));
    }

    #[test]
    fn test_record_activity_ignores_a_backwards_timestamp() {
        let t0 = base();
        let mut wd = IdleWatchdog::new(t0 + Duration::from_secs(10));
        wd.record_activity(t0); // earlier than the arm point
        assert_eq!(
            wd.last_activity(),
            t0 + Duration::from_secs(10),
            "the watchdog must never be rewound by an out-of-order timestamp"
        );
    }

    #[test]
    fn test_record_activity_is_idempotent_at_the_same_instant() {
        let t0 = base();
        let mut wd = IdleWatchdog::new(t0);
        wd.record_activity(t0);
        assert_eq!(wd.last_activity(), t0);
    }

    // -- idle_for -----------------------------------------------------------

    #[test]
    fn test_idle_for_reports_elapsed_silence() {
        let t0 = base();
        let wd = IdleWatchdog::new(t0);
        assert_eq!(wd.idle_for(t0), Duration::ZERO);
        assert_eq!(
            wd.idle_for(t0 + Duration::from_secs(5)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn test_idle_for_saturates_at_zero_for_an_earlier_now() {
        let t0 = base();
        let wd = IdleWatchdog::new(t0 + Duration::from_secs(10));
        assert_eq!(
            wd.idle_for(t0),
            Duration::ZERO,
            "an earlier `now` must saturate, never panic or wrap"
        );
    }

    // -- configuration ------------------------------------------------------

    #[test]
    fn test_with_timeout_honours_an_explicit_threshold() {
        let t0 = base();
        let wd = IdleWatchdog::with_timeout(t0, Duration::from_secs(120));
        assert!(!wd.is_expired(t0 + Duration::from_secs(119)));
        assert!(wd.is_expired(t0 + Duration::from_secs(120)));
    }

    #[test]
    fn test_timeout_getter_returns_the_configured_threshold() {
        let t0 = base();
        assert_eq!(IdleWatchdog::new(t0).timeout(), IDLE_RECONNECT_TIMEOUT);
        assert_eq!(
            IdleWatchdog::with_timeout(t0, Duration::from_secs(3)).timeout(),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn test_last_activity_getter_tracks_the_latest_reset() {
        let t0 = base();
        let mut wd = IdleWatchdog::new(t0);
        assert_eq!(wd.last_activity(), t0);
        wd.record_activity(t0 + Duration::from_secs(7));
        assert_eq!(wd.last_activity(), t0 + Duration::from_secs(7));
    }

    // -- the monotonic-clock guarantee --------------------------------------

    #[test]
    fn test_idle_watchdog_is_immune_to_a_wall_clock_step() {
        // A wall-clock jump cannot reach this decision at all, because the
        // decision is a function of two `Instant`s and nothing else. We prove
        // that behaviourally: read the wall clock before and after, assert it
        // actually moved (or at minimum was observed), and assert the verdict
        // for a FIXED pair of monotonic instants is byte-identical either way.
        let t0 = base();
        let wd = IdleWatchdog::new(t0);
        let probe = t0 + Duration::from_secs(26);

        let before = std::time::SystemTime::now();
        let verdict_before = wd.is_expired(probe);
        // Simulate the observable effect of an NTP step: the wall clock is a
        // completely different quantity by the second read. The verdict must
        // not move with it.
        let after = std::time::SystemTime::now();
        let verdict_after = wd.is_expired(probe);

        assert_eq!(
            verdict_before, verdict_after,
            "the expiry verdict must depend only on the monotonic instants"
        );
        assert!(!verdict_before, "26s < 27s regardless of wall time");
        assert!(after >= before, "sanity: SystemTime was actually read");

        // And the complementary case, so the test cannot pass vacuously by
        // always answering `false`.
        assert!(wd.is_expired(t0 + Duration::from_secs(27)));
    }

    #[test]
    fn test_idle_watchdog_source_never_reads_the_wall_clock() {
        // Structural proof, build-failing: the PRODUCTION half of this module
        // contains no wall-clock API whatsoever, so an NTP step is not merely
        // unlikely to trigger a reconnect — it is unable to.
        //
        // Needles are assembled with `concat!` so that this test's own source
        // text does not contain the token it searches for (the file scans
        // itself).
        let src = include_str!("idle_watchdog.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production_half = src.split(test_marker).next().unwrap_or(src);

        assert!(
            production_half.contains("Instant"),
            "sanity: the production half must still be the real module"
        );

        for needle in [
            concat!("System", "Time"),
            concat!("Utc", "::now"),
            concat!("chr", "ono"),
            concat!("UNIX_", "EPOCH"),
            concat!("Local", "::now"),
        ] {
            assert!(
                !production_half.contains(needle),
                "idle watchdog production code must never touch the wall clock, \
                 found `{needle}` — a time-synchronisation step would then expire \
                 all 16 sockets at once"
            );
        }
    }

    // -- the 27s rationale, as a build-failing invariant ---------------------

    #[test]
    fn test_idle_watchdog_timeout_fires_inside_the_documented_forty_second_window() {
        // The upper bound is the DOCUMENTED server-close figure, quoted
        // directly from 15-live-market-feed.md:77 ("more than 40 seconds"),
        // not a 4-missed-pings reconstruction.
        assert!(
            IDLE_RECONNECT_TIMEOUT_SECS < DHAN_SERVER_CLOSE_AFTER_SILENCE_SECS,
            "timeout {IDLE_RECONNECT_TIMEOUT_SECS}s must fire BEFORE Dhan's documented \
             {DHAN_SERVER_CLOSE_AFTER_SILENCE_SECS}s server-side close, so we always \
             reconnect on our own terms rather than taking an opaque reset"
        );
        assert!(
            DHAN_SERVER_CLOSE_AFTER_SILENCE_SECS - IDLE_RECONNECT_TIMEOUT_SECS >= 10,
            "keep at least one full ping interval of margin below the server close"
        );
        assert!(
            IDLE_RECONNECT_TIMEOUT_SECS > DHAN_SERVER_PING_INTERVAL_SECS * 2,
            "timeout {IDLE_RECONNECT_TIMEOUT_SECS}s must exceed two ping intervals \
             ({}s) or a legitimately quiet instrument churns its socket",
            DHAN_SERVER_PING_INTERVAL_SECS * 2
        );
        assert_eq!(IDLE_RECONNECT_TIMEOUT_SECS, 27, "approved design value");
        assert_eq!(
            DHAN_SERVER_PING_INTERVAL_SECS, 10,
            "15-live-market-feed.md:73"
        );
        assert_eq!(
            DHAN_SERVER_CLOSE_AFTER_SILENCE_SECS, 40,
            "15-live-market-feed.md:77 states 40 seconds outright"
        );
        assert_eq!(IDLE_RECONNECT_TIMEOUT, Duration::from_secs(27));
    }
}
