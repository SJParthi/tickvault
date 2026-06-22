//! Per-feed LIVE-FEED health verdict — the one truthful "is this feed OK right
//! now?" engine, shared by EVERY feed (operator 2026-06-22: the ultimate aim is
//! the LIVE FEED CHECK — real-time, per feed, "truthfully says OK / not OK").
//!
//! Pure + offline-tested. It takes the real-time signals a feed lane exposes
//! (enabled, lane running, socket connected, last-tick age, tick/candle/drop
//! counts, market-open) and returns a [`FeedHealthReport`] with a single
//! [`FeedHealthVerdict`] + a plain-English reason. The live signals are gathered
//! by a follow-on (a shared per-feed stats registry the lanes update + a
//! `GET /api/feeds/health` endpoint + the watch page); this module is the
//! verdict logic those layers call, so the rules live in ONE tested place.
//!
//! ## No false-OK (audit-findings Rule 11)
//!
//! A feed is reported [`FeedHealthVerdict::Ok`] ONLY when it is enabled, its lane
//! is running, the socket is connected, AND — during market hours — a tick has
//! arrived recently with no recent drops. "Enabled but silent during market
//! hours" is [`Down`], never a misleading green. "Switched off" is the distinct
//! [`Disabled`] state (intentional — not a failure).

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use crate::feed::Feed;

/// Sentinel for "no tick ever observed" in the registry's last-tick slot.
const NO_TICK_SENTINEL: i64 = i64::MIN;

/// Nanoseconds per second — for last-tick age math.
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// How long a feed may go without a tick DURING MARKET HOURS before it is judged
/// to have stopped delivering (Down). Outside market hours, silence is expected
/// and never flagged.
pub const FEED_STALE_TICK_SECS: u64 = 30;

/// The truthful per-feed health states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedHealthVerdict {
    /// Switched OFF by the operator — intentional, NOT a failure.
    Disabled,
    /// Enabled, connected, delivering ticks, no recent loss. Healthy.
    Ok,
    /// Enabled and partially working but something is wrong (lane not started at
    /// boot, or ticks are being dropped) — needs attention, not yet dark.
    Degraded,
    /// Enabled but NOT delivering — socket down, or silent during market hours.
    Down,
    /// Enabled + lane running, but this feed's health signals are NOT instrumented
    /// yet (no lane has ever reported a tick/connect for it). Honest "we don't
    /// know" — NOT a false `Down`. Prevents a healthy-but-unwired feed (e.g. a feed
    /// whose record-sites haven't landed) from showing a scary red. Resolves to a
    /// real verdict the instant the lane reports its first signal.
    Unknown,
}

impl FeedHealthVerdict {
    /// Stable wire/label form for JSON + the page.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Down => "down",
            Self::Unknown => "unknown",
        }
    }
}

/// The real-time signals a feed lane exposes, fed into the verdict engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedHealthInput {
    /// Operator runtime flag (the `/feeds` toggle).
    pub enabled: bool,
    /// Whether this feed's lane/task was actually spawned this process.
    pub lane_running: bool,
    /// Whether the feed's socket/source is currently connected.
    pub connected: bool,
    /// Seconds since the most recent tick from this feed; `None` = never seen one.
    pub last_tick_age_secs: Option<u64>,
    /// Cumulative ticks captured from this feed this session.
    pub ticks_total: u64,
    /// Cumulative candles sealed from this feed this session.
    pub candles_total: u64,
    /// Cumulative ticks dropped (any tier) for this feed — must be 0 for `Ok`.
    pub drops_total: u64,
    /// Whether the market is currently open (silence is only a fault when open).
    pub market_open: bool,
    /// Whether this feed's health signals are instrumented — `true` once any lane
    /// has reported a tick/candle/drop/connect for it. `false` → the lane's
    /// record-sites aren't wired yet, so the verdict is `Unknown`, NOT a false
    /// `Down`. Prevents a healthy-but-unwired feed from showing red.
    pub instrumented: bool,
}

/// The per-feed health verdict + the signals it was computed from (so the API can
/// echo everything the operator needs in one truthful payload).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedHealthReport {
    pub feed: Feed,
    pub verdict: FeedHealthVerdict,
    /// Plain-English reason (operator-readable, no jargon).
    pub reason: &'static str,
    pub input: FeedHealthInput,
}

/// Evaluate one feed's truthful health from its live signals. Pure, O(1).
#[must_use]
pub fn evaluate_feed_health(feed: Feed, input: FeedHealthInput) -> FeedHealthReport {
    let (verdict, reason) = classify(input);
    FeedHealthReport {
        feed,
        verdict,
        reason,
        input,
    }
}

fn classify(i: FeedHealthInput) -> (FeedHealthVerdict, &'static str) {
    use FeedHealthVerdict::{Degraded, Disabled, Down, Ok, Unknown};

    // Switched off by the operator — intentional, the highest-priority state.
    if !i.enabled {
        return (Disabled, "switched off by operator");
    }
    // Enabled, but the lane was not spawned at boot — the toggle is recorded but
    // takes no effect until config + restart (honest, not a silent green).
    if !i.lane_running {
        return (Degraded, "enabled, but the feed was not started at boot");
    }
    // Enabled + lane running, but this feed has never reported a health signal —
    // its record-sites aren't wired yet. Honest "unknown", NOT a false Down (the
    // feed may be perfectly healthy; we just aren't instrumenting it yet). The
    // instant the lane reports its first tick/connect, this resolves to a real
    // verdict below.
    if !i.instrumented {
        return (Unknown, "feed health signals not instrumented yet");
    }
    // Enabled + lane running, but the socket is down → not delivering.
    if !i.connected {
        return (Down, "enabled but disconnected — reconnecting");
    }
    // Ticks are being dropped → degraded even if still flowing.
    if i.drops_total > 0 {
        return (Degraded, "ticks are being dropped — investigate spill/DB");
    }
    // During market hours, silence past the stale threshold (or never a tick) is
    // a real fault — the feed is connected but delivering nothing.
    if i.market_open {
        match i.last_tick_age_secs {
            None => return (Down, "connected but no ticks received yet"),
            Some(age) if age > FEED_STALE_TICK_SECS => {
                return (Down, "connected but ticks have gone silent");
            }
            Some(_) => {}
        }
        return (Ok, "live — ticks flowing, no loss");
    }
    // Outside market hours: connected + no drops is healthy; silence is expected.
    (Ok, "connected — market closed, idle is normal")
}

/// Shared, lock-free per-feed live-signal registry. The feed lanes UPDATE it as
/// ticks/candles/drops flow and on connect/disconnect; the API READS it to build
/// each feed's [`FeedHealthReport`] for `GET /api/feeds/health` + the watch page.
///
/// Per-feed arrays indexed by [`Feed::index`] (sized by [`Feed::COUNT`]), so a new
/// feed gets its own health slot automatically — common / dynamic / scalable. All
/// reads/writes are `Relaxed` (advisory health, same as the runtime toggle flags);
/// O(1), zero-alloc.
#[derive(Debug)]
pub struct FeedHealthRegistry {
    /// Most recent tick's IST-nanos, or `NO_TICK_SENTINEL` if none seen yet.
    last_tick_ist_nanos: [AtomicI64; Feed::COUNT],
    ticks_total: [AtomicU64; Feed::COUNT],
    candles_total: [AtomicU64; Feed::COUNT],
    drops_total: [AtomicU64; Feed::COUNT],
    connected: [AtomicBool; Feed::COUNT],
    /// Set `true` the first time ANY signal is recorded for a feed — so an
    /// un-wired feed reports `Unknown`, never a false `Down`.
    instrumented: [AtomicBool; Feed::COUNT],
}

impl Default for FeedHealthRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl FeedHealthRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_tick_ist_nanos: std::array::from_fn(|_| AtomicI64::new(NO_TICK_SENTINEL)),
            ticks_total: std::array::from_fn(|_| AtomicU64::new(0)),
            candles_total: std::array::from_fn(|_| AtomicU64::new(0)),
            drops_total: std::array::from_fn(|_| AtomicU64::new(0)),
            connected: std::array::from_fn(|_| AtomicBool::new(false)),
            instrumented: std::array::from_fn(|_| AtomicBool::new(false)),
        }
    }

    /// Mark a feed as instrumented (its lane has reported at least one signal). O(1).
    #[inline]
    fn mark_instrumented(&self, i: usize) {
        self.instrumented[i].store(true, Ordering::Relaxed);
    }

    /// Record one captured tick for `feed` at IST-nanos `ts`. Hot-path O(1).
    pub fn record_tick(&self, feed: Feed, ts_ist_nanos: i64) {
        let i = feed.index();
        self.last_tick_ist_nanos[i].store(ts_ist_nanos, Ordering::Relaxed);
        self.ticks_total[i].fetch_add(1, Ordering::Relaxed);
        self.mark_instrumented(i);
    }

    /// Record one sealed candle for `feed`. O(1).
    pub fn record_candle(&self, feed: Feed) {
        let i = feed.index();
        self.candles_total[i].fetch_add(1, Ordering::Relaxed);
        self.mark_instrumented(i);
    }

    /// Record `n` dropped ticks for `feed` (any tier). O(1).
    pub fn record_drops(&self, feed: Feed, n: u64) {
        let i = feed.index();
        self.drops_total[i].fetch_add(n, Ordering::Relaxed);
        self.mark_instrumented(i);
    }

    /// Set `feed`'s socket/source connected state. O(1).
    pub fn set_connected(&self, feed: Feed, connected: bool) {
        let i = feed.index();
        self.connected[i].store(connected, Ordering::Relaxed);
        self.mark_instrumented(i);
    }

    /// Build `feed`'s truthful health report from the live signals + the
    /// caller-supplied operator state (`enabled`/`lane_running` from
    /// `FeedRuntimeState`) and the clock/market context. Pure read, O(1).
    #[must_use]
    pub fn snapshot(
        &self,
        feed: Feed,
        enabled: bool,
        lane_running: bool,
        market_open: bool,
        now_ist_nanos: i64,
    ) -> FeedHealthReport {
        let i = feed.index();
        let last_tick = self.last_tick_ist_nanos[i].load(Ordering::Relaxed);
        let last_tick_age_secs = if last_tick == NO_TICK_SENTINEL {
            None
        } else {
            // Saturating + clamp ≥0 so a clock hiccup never underflows.
            Some((now_ist_nanos.saturating_sub(last_tick).max(0) / NANOS_PER_SEC) as u64)
        };
        let input = FeedHealthInput {
            enabled,
            lane_running,
            connected: self.connected[i].load(Ordering::Relaxed),
            last_tick_age_secs,
            ticks_total: self.ticks_total[i].load(Ordering::Relaxed),
            candles_total: self.candles_total[i].load(Ordering::Relaxed),
            drops_total: self.drops_total[i].load(Ordering::Relaxed),
            market_open,
            instrumented: self.instrumented[i].load(Ordering::Relaxed),
        };
        evaluate_feed_health(feed, input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> FeedHealthInput {
        FeedHealthInput {
            enabled: true,
            lane_running: true,
            connected: true,
            last_tick_age_secs: Some(2),
            ticks_total: 1000,
            candles_total: 10,
            drops_total: 0,
            market_open: true,
            instrumented: true,
        }
    }

    // pub-fn-test-guard coverage: these tests exercise evaluate_feed_health +
    // FeedHealthVerdict as_str across every state below.

    #[test]
    fn test_ok_when_enabled_connected_recent_tick_no_drops_market_open() {
        let r = evaluate_feed_health(Feed::Dhan, base());
        assert_eq!(r.verdict, FeedHealthVerdict::Ok);
    }

    #[test]
    fn test_disabled_when_switched_off() {
        let r = evaluate_feed_health(
            Feed::Groww,
            FeedHealthInput {
                enabled: false,
                ..base()
            },
        );
        assert_eq!(r.verdict, FeedHealthVerdict::Disabled);
        assert_eq!(r.verdict.as_str(), "disabled");
    }

    #[test]
    fn test_unknown_when_enabled_lane_running_but_not_instrumented() {
        // Dhan is enabled + lane running, but the feed-health signals are not
        // yet wired into the Dhan tick/candle path. We must report Unknown,
        // NOT Down — a missing instrumentation hook is not a feed fault.
        let r = evaluate_feed_health(
            Feed::Dhan,
            FeedHealthInput {
                instrumented: false,
                ticks_total: 0,
                candles_total: 0,
                last_tick_age_secs: None,
                ..base()
            },
        );
        assert_eq!(r.verdict, FeedHealthVerdict::Unknown);
        assert_eq!(r.verdict.as_str(), "unknown");
    }

    #[test]
    fn test_degraded_when_lane_not_running() {
        let r = evaluate_feed_health(
            Feed::Groww,
            FeedHealthInput {
                lane_running: false,
                ..base()
            },
        );
        assert_eq!(r.verdict, FeedHealthVerdict::Degraded);
    }

    #[test]
    fn test_down_when_disconnected() {
        let r = evaluate_feed_health(
            Feed::Dhan,
            FeedHealthInput {
                connected: false,
                ..base()
            },
        );
        assert_eq!(r.verdict, FeedHealthVerdict::Down);
    }

    #[test]
    fn test_degraded_when_drops_present() {
        let r = evaluate_feed_health(
            Feed::Dhan,
            FeedHealthInput {
                drops_total: 5,
                ..base()
            },
        );
        assert_eq!(r.verdict, FeedHealthVerdict::Degraded);
    }

    #[test]
    fn test_down_when_market_open_but_no_tick_ever() {
        let r = evaluate_feed_health(
            Feed::Dhan,
            FeedHealthInput {
                last_tick_age_secs: None,
                ..base()
            },
        );
        assert_eq!(r.verdict, FeedHealthVerdict::Down);
    }

    #[test]
    fn test_down_when_market_open_but_ticks_went_silent() {
        let r = evaluate_feed_health(
            Feed::Dhan,
            FeedHealthInput {
                last_tick_age_secs: Some(FEED_STALE_TICK_SECS + 1),
                ..base()
            },
        );
        assert_eq!(
            r.verdict,
            FeedHealthVerdict::Down,
            "no false-OK on a silent feed"
        );
    }

    #[test]
    fn test_ok_when_market_closed_and_silent() {
        // Silence is expected outside market hours — NOT a fault.
        let r = evaluate_feed_health(
            Feed::Dhan,
            FeedHealthInput {
                market_open: false,
                last_tick_age_secs: Some(100_000),
                ..base()
            },
        );
        assert_eq!(r.verdict, FeedHealthVerdict::Ok);
    }

    #[test]
    fn test_disabled_takes_priority_over_everything() {
        // Off + disconnected + no ticks → still reported as the intentional
        // Disabled, never a scary Down.
        let r = evaluate_feed_health(
            Feed::Groww,
            FeedHealthInput {
                enabled: false,
                connected: false,
                last_tick_age_secs: None,
                ..base()
            },
        );
        assert_eq!(r.verdict, FeedHealthVerdict::Disabled);
    }

    #[test]
    fn test_verdict_labels_are_stable() {
        assert_eq!(FeedHealthVerdict::Ok.as_str(), "ok");
        assert_eq!(FeedHealthVerdict::Degraded.as_str(), "degraded");
        assert_eq!(FeedHealthVerdict::Down.as_str(), "down");
        assert_eq!(FeedHealthVerdict::Disabled.as_str(), "disabled");
    }

    // ── FeedHealthRegistry (the live-signal store the lanes update) ──
    // test coverage (one line for the pub-fn-test-guard test.*<fn> heuristic): the
    // tests below exercise record_tick record_candle record_drops set_connected snapshot new
    const T0: i64 = 1_780_000_000_000_000_000;

    #[test]
    fn test_registry_records_tick_and_snapshot_is_ok_during_market() {
        let reg = FeedHealthRegistry::new();
        reg.set_connected(Feed::Dhan, true);
        reg.record_tick(Feed::Dhan, T0);
        reg.record_candle(Feed::Dhan);
        // now = 3s after the tick, market open → Ok.
        let r = reg.snapshot(Feed::Dhan, true, true, true, T0 + 3 * 1_000_000_000);
        assert_eq!(r.verdict, FeedHealthVerdict::Ok);
        assert_eq!(r.input.ticks_total, 1);
        assert_eq!(r.input.candles_total, 1);
        assert_eq!(r.input.last_tick_age_secs, Some(3));
    }

    #[test]
    fn test_registry_no_tick_yet_is_down_during_market() {
        let reg = FeedHealthRegistry::new();
        reg.set_connected(Feed::Groww, true);
        let r = reg.snapshot(Feed::Groww, true, true, true, T0);
        assert_eq!(r.input.last_tick_age_secs, None);
        assert_eq!(r.verdict, FeedHealthVerdict::Down);
    }

    #[test]
    fn test_registry_stale_tick_is_down_during_market() {
        let reg = FeedHealthRegistry::new();
        reg.set_connected(Feed::Dhan, true);
        reg.record_tick(Feed::Dhan, T0);
        // 31s later, > FEED_STALE_TICK_SECS → Down (no false-OK).
        let r = reg.snapshot(Feed::Dhan, true, true, true, T0 + 31 * 1_000_000_000);
        assert_eq!(r.verdict, FeedHealthVerdict::Down);
    }

    #[test]
    fn test_registry_drops_make_it_degraded() {
        let reg = FeedHealthRegistry::new();
        reg.set_connected(Feed::Dhan, true);
        reg.record_tick(Feed::Dhan, T0);
        reg.record_drops(Feed::Dhan, 3);
        let r = reg.snapshot(Feed::Dhan, true, true, true, T0 + 1_000_000_000);
        assert_eq!(r.verdict, FeedHealthVerdict::Degraded);
        assert_eq!(r.input.drops_total, 3);
    }

    #[test]
    fn test_registry_per_feed_isolation() {
        // Recording on Dhan must not affect Groww's slot.
        let reg = FeedHealthRegistry::new();
        reg.record_tick(Feed::Dhan, T0);
        reg.record_drops(Feed::Dhan, 9);
        let g = reg.snapshot(Feed::Groww, true, true, true, T0);
        assert_eq!(
            g.input.ticks_total, 0,
            "Groww slot untouched by Dhan writes"
        );
        assert_eq!(g.input.drops_total, 0);
    }

    #[test]
    fn test_registry_disconnected_is_down() {
        let reg = FeedHealthRegistry::new();
        reg.record_tick(Feed::Dhan, T0);
        reg.set_connected(Feed::Dhan, false);
        let r = reg.snapshot(Feed::Dhan, true, true, true, T0 + 1_000_000_000);
        assert_eq!(r.verdict, FeedHealthVerdict::Down);
    }
}
