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

use crate::feed::Feed;

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
    use FeedHealthVerdict::{Degraded, Disabled, Down, Ok};

    // Switched off by the operator — intentional, the highest-priority state.
    if !i.enabled {
        return (Disabled, "switched off by operator");
    }
    // Enabled, but the lane was not spawned at boot — the toggle is recorded but
    // takes no effect until config + restart (honest, not a silent green).
    if !i.lane_running {
        return (Degraded, "enabled, but the feed was not started at boot");
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
}
