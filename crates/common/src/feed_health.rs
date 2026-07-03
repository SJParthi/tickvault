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
    /// Whether the feed's auth credential was REJECTED by the provider (e.g. Groww
    /// answered HTTP 400 "Token key not found or inactive"). A confirmed rejection
    /// is a `Down` with an actionable "refresh the SSM api-key" message — it wins
    /// over the generic disconnected/no-ticks reasons but NOT over `Disabled`. Set
    /// by the feed's activation path ONLY on a genuine provider rejection (never on
    /// a transient network blip), and cleared on a successful auth.
    pub auth_rejected: bool,
    /// Whether this feed's health signals are instrumented — `true` once any lane
    /// has reported a tick/candle/drop/connect for it. `false` → the lane's
    /// record-sites aren't wired yet, so the verdict is `Unknown`, NOT a false
    /// `Down`. Prevents a healthy-but-unwired feed from showing red.
    pub instrumented: bool,
    /// Number of STOCK instruments this feed subscribed at startup (the live
    /// connect+subscribe PROOF — operator 2026-06-28). `0` until the feed reports
    /// a subscribe count (e.g. the Groww bridge observes the sidecar's `subscribed`
    /// status). Display-only — it does NOT change the verdict, it answers "how
    /// many SIDs did the feed actually subscribe today?".
    pub subscribed_stocks: u64,
    /// Number of INDEX instruments this feed subscribed at startup. See
    /// `subscribed_stocks`.
    pub subscribed_indices: u64,
    /// Number of records the producer DECODED and successfully EMITTED into the
    /// pipeline (the honest-feed PROOF — operator 2026-06-29). `0` until the feed
    /// reports an emit count (e.g. the Groww bridge reads the sidecar's `emitted`
    /// status field). Display-only — it does NOT change the verdict; it answers
    /// "is the feed actually decoding+emitting ticks, or just connected?".
    pub decoded_emitted: u64,
    /// Number of records the producer DECODED but DROPPED (a `sid_map` miss or a
    /// missing price field) — previously a silent `continue`. A non-zero value
    /// makes a key-map mismatch visible ("streaming but 0 ticks"). Display-only.
    pub decoded_dropped: u64,
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
    // Ticks being dropped is a fault at ANY time — the cumulative drop count
    // reflects a real problem that happened today (checked before the
    // market-hours gate so a post-close report still surfaces today's drops).
    if i.drops_total > 0 {
        return (Degraded, "ticks are being dropped — investigate spill/DB");
    }
    // C1 fix (hostile 3-agent review 2026-06-22) + market-closed-idle fix
    // (operator 2026-06-29): the market-open gate MUST precede BOTH the
    // connected/last-tick checks AND the auth-rejected check. Outside market
    // hours the feed legitimately sleeps + disconnects (the WS pool
    // sleeps-until-open by design) and goes silent — that is EXPECTED, never a
    // fault. Critically, a latched `auth_rejected` flag is *unverifiable stale
    // state* when the market is closed: no auth is being attempted and no tick
    // can flow to re-confirm or self-heal it (the record_ticks recovery-clear
    // can only fire on a live tick). Surfacing the actionable "refresh the SSM
    // api-key" Down then is a false-RED — the mirror of the no-false-OK rule —
    // and it sent the operator chasing a non-existent key problem at 22:30 IST
    // for a feed that had authed + subscribed 767 fine. Idle outside market
    // hours = Ok, regardless of a stale auth-reject flag.
    if !i.market_open {
        return (Ok, "market closed — idle is normal");
    }
    // --- Market hours below: silence / disconnection IS a real fault. ---
    // The provider REJECTED our auth credential (Groww answered with a non-2xx,
    // e.g. "Token key not found or inactive"). This is a confirmed, actionable
    // failure — it must win over the generic disconnected / no-ticks reasons so
    // the operator sees WHAT to fix, not just a bare Down. It is checked DURING
    // market hours only (above the connected/last-tick checks but below the
    // market-closed gate) so a stale flag never produces a false "refresh the
    // api-key" Down outside trading hours. It does NOT win over Disabled (a
    // switched-off feed is intentional, handled above) and is placed after the
    // not-started / not-instrumented states so a feed that never came up does
    // not show a misleading auth-rejected. The message is a fixed `&'static str`
    // (security contract — no runtime/error data interpolated).
    if i.auth_rejected {
        return (Down, "auth rejected — refresh the Groww SSM api-key");
    }
    // Enabled + lane running + instrumented, but the socket is down → not delivering.
    if !i.connected {
        return (Down, "enabled but disconnected — reconnecting");
    }
    // Silence past the stale threshold (or never a tick) is a real fault — the
    // feed is connected but delivering nothing.
    match i.last_tick_age_secs {
        None => (Down, "connected but no ticks received yet"),
        Some(age) if age > FEED_STALE_TICK_SECS => (Down, "connected but ticks have gone silent"),
        Some(_) => (Ok, "live — ticks flowing, no loss"),
    }
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
    /// STOCK instruments subscribed at startup (the live connect+subscribe PROOF).
    subscribed_stocks: [AtomicU64; Feed::COUNT],
    /// INDEX instruments subscribed at startup.
    subscribed_indices: [AtomicU64; Feed::COUNT],
    /// Records the producer DECODED + EMITTED into the pipeline (honest-feed PROOF).
    decoded_emitted: [AtomicU64; Feed::COUNT],
    /// Records the producer DECODED but DROPPED (sid-map miss / missing field).
    decoded_dropped: [AtomicU64; Feed::COUNT],
    /// `true` when the provider rejected this feed's auth credential. Set on a
    /// confirmed rejection, cleared on a successful auth.
    auth_rejected: [AtomicBool; Feed::COUNT],
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
            subscribed_stocks: std::array::from_fn(|_| AtomicU64::new(0)),
            subscribed_indices: std::array::from_fn(|_| AtomicU64::new(0)),
            decoded_emitted: std::array::from_fn(|_| AtomicU64::new(0)),
            decoded_dropped: std::array::from_fn(|_| AtomicU64::new(0)),
            auth_rejected: std::array::from_fn(|_| AtomicBool::new(false)),
            instrumented: std::array::from_fn(|_| AtomicBool::new(false)),
        }
    }

    /// Mark a feed as instrumented (its lane has reported at least one signal). O(1).
    #[inline]
    fn mark_instrumented(&self, i: usize) {
        self.instrumented[i].store(true, Ordering::Relaxed);
    }

    /// Clear `feed`'s auth-rejected flag on a GENUINE same-session recovery — the
    /// edge where ticks start flowing again. Without this, an alert-class sidecar
    /// line (set via [`Self::set_auth_rejected`]`(true)`) latches the flag forever
    /// and the `/feeds` page shows a stuck false-RED "auth rejected" Down even
    /// after the feed recovers and is streaming fine (the only other clear site is
    /// the START edge in `groww_activation`, which never fires on same-session
    /// recovery).
    ///
    /// Edge-triggered + hot-path-safe: it `load`s the flag and only attempts a
    /// single `compare_exchange` true→false. In the overwhelming common case
    /// (already false) the `load` short-circuits and NO store happens — O(1), no
    /// per-tick churn.
    ///
    /// "Rows actually flowing again" IS the recovery signal: it clears only on a
    /// genuine tick (`record_ticks(n>0)` counts rows flushed to QuestDB;
    /// `record_tick` is one captured tick), so an idle/dead feed never self-heals.
    /// A feed held down by a HARD provider auth rejection produces zero ticks, so
    /// the clear cannot fire for it. (The Groww supervisor also raises the flag on
    /// any alert-class line, not only a hard rejection — for those transient
    /// cases, a resumed tick flow IS the correct "the alert is over" recovery
    /// edge, which is exactly what this clears.)
    #[inline]
    fn clear_auth_rejected_on_recovery(&self, i: usize) {
        if self.auth_rejected[i].load(Ordering::Relaxed) {
            // true→false once. A racing re-set (the provider re-rejects in the
            // same instant) just wins via Err — that is the correct outcome, so
            // the must-use Result is intentionally consumed without acting on it.
            let _cas_outcome = self.auth_rejected[i].compare_exchange(
                true,
                false,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
    }

    /// Record one captured tick for `feed` at IST-nanos `ts`. Hot-path O(1).
    pub fn record_tick(&self, feed: Feed, ts_ist_nanos: i64) {
        let i = feed.index();
        self.last_tick_ist_nanos[i].store(ts_ist_nanos, Ordering::Relaxed);
        self.ticks_total[i].fetch_add(1, Ordering::Relaxed);
        self.mark_instrumented(i);
        // A tick flowing is a genuine recovery edge → clear any stale auth-reject
        // so the page stops showing a false-RED Down (edge-triggered, no-op when
        // already clear).
        self.clear_auth_rejected_on_recovery(i);
    }

    /// Record `n` captured ticks for `feed` in ONE O(1) atomic add (NOT a loop),
    /// stamping `ts_ist_nanos` as the most-recent tick time. Used by feeds that
    /// count PERSISTED rows in batches (e.g. the Groww bridge increments by the
    /// number of rows actually flushed to QuestDB, so the dashboard "ticks"
    /// reflects rows written, not in-memory buffer appends). `n == 0` is a no-op
    /// on the count and does not stamp the time.
    pub fn record_ticks(&self, feed: Feed, n: u64, ts_ist_nanos: i64) {
        if n == 0 {
            return;
        }
        let i = feed.index();
        self.last_tick_ist_nanos[i].store(ts_ist_nanos, Ordering::Relaxed);
        self.ticks_total[i].fetch_add(n, Ordering::Relaxed);
        self.mark_instrumented(i);
        // Rows actually flowed (n > 0) → genuine recovery edge; clear any stale
        // auth-reject so the page resolves back to live (edge-triggered, no-op
        // when already clear). `n == 0` returned early above, so a 0-row flush
        // never clears (no false-recovery — audit Rule 11).
        self.clear_auth_rejected_on_recovery(i);
    }

    /// Record feed LIVENESS only — stamp `ts_ist_nanos` as the most-recent tick
    /// time WITHOUT bumping the persisted-tick count (2026-07-02 adversarial-
    /// sweep fix). Called at PARSE time by the Groww bridge (lines parsed from
    /// the sidecar NDJSON this wake — proof the feed DELIVERED), independent of
    /// whether the QuestDB flush succeeds. This decouples the sidecar
    /// stall-watchdog (`should_restart_on_stall`, which reads
    /// `last_tick_age_secs`) from persistence: a QuestDB outage no longer
    /// mimics a dead socket and can no longer trigger a false FEED-STALL-01
    /// kill of a healthy sidecar. Tick COUNTS stay persist-honest via
    /// [`Self::record_ticks`] in the flush arm. O(1), one relaxed store.
    pub fn record_feed_liveness(&self, feed: Feed, ts_ist_nanos: i64) {
        let i = feed.index();
        self.last_tick_ist_nanos[i].store(ts_ist_nanos, Ordering::Relaxed);
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

    /// Record how many STOCK + INDEX instruments `feed` subscribed at startup —
    /// the live connect+subscribe PROOF (operator 2026-06-28). Marks the feed
    /// instrumented (a subscribe count is a real signal). O(1), Relaxed. Idempotent
    /// overwrite (a re-subscribe on reconnect updates the counts). Does NOT change
    /// the verdict — it is the "how many SIDs did we actually subscribe?" answer.
    pub fn set_subscribed(&self, feed: Feed, stocks: u64, indices: u64) {
        let i = feed.index();
        self.subscribed_stocks[i].store(stocks, Ordering::Relaxed);
        self.subscribed_indices[i].store(indices, Ordering::Relaxed);
        self.mark_instrumented(i);
    }

    /// Record how many records `feed` DECODED+EMITTED vs DECODED-but-DROPPED — the
    /// honest-feed PROOF (operator 2026-06-29). Makes the previously-SILENT
    /// decode-but-drop (a `sid_map` miss / missing price field) a visible number,
    /// so "streaming but 0 ticks" has an immediate cause. Marks the feed
    /// instrumented (a decode count is a real signal). O(1), Relaxed. Idempotent
    /// overwrite (the producer reports cumulative totals). Does NOT change the
    /// verdict — it is the "is the feed actually emitting ticks?" answer.
    pub fn set_decode_counts(&self, feed: Feed, emitted: u64, dropped: u64) {
        let i = feed.index();
        self.decoded_emitted[i].store(emitted, Ordering::Relaxed);
        self.decoded_dropped[i].store(dropped, Ordering::Relaxed);
        self.mark_instrumented(i);
    }

    /// Set `feed`'s auth-rejected state. Pass `true` on a confirmed provider
    /// credential rejection (e.g. Groww HTTP 400) and `false` on a successful
    /// auth. Marks the feed instrumented so a rejection surfaces as the
    /// actionable `Down` ("refresh the api-key"), never `Unknown`. O(1).
    pub fn set_auth_rejected(&self, feed: Feed, rejected: bool) {
        let i = feed.index();
        self.auth_rejected[i].store(rejected, Ordering::Relaxed);
        self.mark_instrumented(i);
    }

    /// Read `feed`'s auth-rejected state (§34 auto-scale advance gate: any
    /// fleet-wide credential rejection blocks laddering up). O(1) load.
    #[must_use]
    pub fn is_auth_rejected(&self, feed: Feed) -> bool {
        self.auth_rejected[feed.index()].load(Ordering::Relaxed)
    }

    /// Clear `feed`'s auth-rejected flag on a NON-TICK recovery edge — a
    /// confirmed re-auth / streaming signal (e.g. the Groww sidecar's
    /// `groww auth OK` / NDJSON-append line) that proves the alert is over even
    /// when no tick has been persisted yet.
    ///
    /// Why this exists (false-RED durable fix, 2026-06-29): the only other
    /// mid-session clear is a real persisted tick
    /// ([`Self::record_tick`]/[`Self::record_ticks`]`(n>0)`), so in a
    /// legitimately tick-silent window a stale `auth_rejected` (e.g. latched by a
    /// transient blip) stuck for the whole session. A confirmed streaming/auth-OK
    /// signal is a genuine non-tick recovery edge that this clears. It does NOT
    /// instrument the slot or clear on anything but a real positive signal (the
    /// caller decides the edge), so the no-false-OK rule (audit Rule 11) holds.
    ///
    /// Edge-triggered + O(1): delegates to the same `load` →
    /// single-`compare_exchange(true→false)` primitive as the tick-based
    /// recovery clear, so it is a no-op (one relaxed load) when already clear.
    pub fn clear_auth_rejected(&self, feed: Feed) {
        self.clear_auth_rejected_on_recovery(feed.index());
    }

    /// Feed-level age (seconds) of the MOST-RECENT tick across `feed`'s whole
    /// subscribed universe, or `None` if no tick has been seen yet this session.
    /// O(1) — one relaxed atomic load + saturating arithmetic. This is the
    /// FEED-AGNOSTIC liveness signal the sidecar stall-watchdog polls to tell a
    /// genuinely-dead socket (feed-level gap across all SIDs during market hours)
    /// from an illiquid single instrument (the feed-level tick is still fresh).
    ///
    /// A BACKWARD clock step (NTP) yields age 0 (the `.max(0)` clamp), so a clock
    /// step can never false-trigger a stall restart. `None` (no tick yet) means a
    /// cold pre-open feed that has not streamed its first tick — the caller MUST
    /// NOT restart on `None` (a feed with no first tick is covered by the
    /// silent-feed diagnostic, not a kill loop).
    #[must_use]
    pub fn last_tick_age_secs(&self, feed: Feed, now_ist_nanos: i64) -> Option<u64> {
        let last_tick = self.last_tick_ist_nanos[feed.index()].load(Ordering::Relaxed);
        if last_tick == NO_TICK_SENTINEL {
            return None;
        }
        Some((now_ist_nanos.saturating_sub(last_tick).max(0) / NANOS_PER_SEC) as u64)
    }

    /// Build `feed`'s truthful health report from the live signals + the
    /// caller-supplied operator state (`enabled`/`lane_running` from
    /// `FeedRuntimeState`) and the clock/market context. Pure read, O(1).
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
            auth_rejected: self.auth_rejected[i].load(Ordering::Relaxed),
            instrumented: self.instrumented[i].load(Ordering::Relaxed),
            subscribed_stocks: self.subscribed_stocks[i].load(Ordering::Relaxed),
            subscribed_indices: self.subscribed_indices[i].load(Ordering::Relaxed),
            decoded_emitted: self.decoded_emitted[i].load(Ordering::Relaxed),
            decoded_dropped: self.decoded_dropped[i].load(Ordering::Relaxed),
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
            auth_rejected: false,
            instrumented: true,
            subscribed_stocks: 0,
            subscribed_indices: 0,
            decoded_emitted: 0,
            decoded_dropped: 0,
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
    fn test_disconnected_outside_market_hours_is_not_down() {
        // C1 fix (2026-06-22): pre-market / post-close the WS pool legitimately
        // sleeps + disconnects, so the watchdog pushes connected=false BEFORE the
        // pool connects. That must NOT read as a scary Down — idle outside market
        // hours is expected, not a fault. (Same input that IS Down during market
        // hours via test_registry_disconnected_is_down.)
        let r = evaluate_feed_health(
            Feed::Dhan,
            FeedHealthInput {
                connected: false,
                last_tick_age_secs: None,
                market_open: false,
                ..base()
            },
        );
        assert_eq!(
            r.verdict,
            FeedHealthVerdict::Ok,
            "disconnected outside market hours is idle, not a false-RED"
        );
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
    fn test_auth_rejected_is_down_with_refresh_message() {
        // A confirmed provider rejection surfaces as an actionable Down naming
        // the fix — not a bare "disconnected".
        let r = evaluate_feed_health(
            Feed::Groww,
            FeedHealthInput {
                auth_rejected: true,
                ..base()
            },
        );
        assert_eq!(r.verdict, FeedHealthVerdict::Down);
        assert_eq!(r.reason, "auth rejected — refresh the Groww SSM api-key");
    }

    #[test]
    fn test_auth_rejected_false_does_not_trigger() {
        // The default (no rejection) must NOT show the auth-rejected message —
        // a healthy feed stays Ok.
        let r = evaluate_feed_health(
            Feed::Groww,
            FeedHealthInput {
                auth_rejected: false,
                ..base()
            },
        );
        assert_eq!(r.verdict, FeedHealthVerdict::Ok);
        assert_ne!(r.reason, "auth rejected — refresh the Groww SSM api-key");
    }

    #[test]
    fn test_disabled_wins_over_auth_rejected() {
        // A switched-off feed is intentional even if a stale auth-rejected flag
        // lingers — show the calm Disabled, never the scary auth-rejected Down.
        let r = evaluate_feed_health(
            Feed::Groww,
            FeedHealthInput {
                enabled: false,
                auth_rejected: true,
                ..base()
            },
        );
        assert_eq!(r.verdict, FeedHealthVerdict::Disabled);
    }

    #[test]
    fn test_market_closed_idle_wins_over_stale_auth_rejected() {
        // The operator's exact 2026-06-29 22:30 IST scenario: Groww authed +
        // subscribed 767, but a stale auth_rejected flag was latched. Market is
        // CLOSED → no auth attempt, no tick possible, so the flag is unverifiable
        // stale state. It must NOT produce the scary "refresh the SSM api-key"
        // Down — market-closed idle wins (false-RED avoidance). During market
        // hours the SAME flag IS a real Down (see the two tests below).
        let r = evaluate_feed_health(
            Feed::Groww,
            FeedHealthInput {
                auth_rejected: true,
                connected: false,
                last_tick_age_secs: None,
                market_open: false,
                ..base()
            },
        );
        assert_eq!(
            r.verdict,
            FeedHealthVerdict::Ok,
            "stale auth-reject outside market hours must not be a false-RED Down"
        );
        assert_eq!(r.reason, "market closed — idle is normal");
        assert_ne!(r.reason, "auth rejected — refresh the Groww SSM api-key");
    }

    #[test]
    fn test_auth_rejected_wins_over_disconnected_during_market() {
        // During market hours, a confirmed auth-rejection must name the fix
        // rather than the generic "disconnected — reconnecting".
        let r = evaluate_feed_health(
            Feed::Groww,
            FeedHealthInput {
                auth_rejected: true,
                connected: false,
                last_tick_age_secs: None,
                ..base()
            },
        );
        assert_eq!(r.verdict, FeedHealthVerdict::Down);
        assert_eq!(r.reason, "auth rejected — refresh the Groww SSM api-key");
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
    // tests below exercise record_tick record_ticks record_candle record_drops set_connected set_auth_rejected clear_auth_rejected set_subscribed set_decode_counts snapshot new
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
    fn test_registry_default_matches_new_for_unwired_feed() {
        // `Default` must be identical to `new()`: an un-instrumented feed
        // reports no last-tick and zero counters (never a false Down).
        let reg = FeedHealthRegistry::default();
        assert!(reg.last_tick_age_secs(Feed::Dhan, T0).is_none());
        let r = reg.snapshot(Feed::Dhan, true, true, true, T0);
        assert_eq!(r.input.ticks_total, 0);
        assert_eq!(r.input.candles_total, 0);
    }

    #[test]
    fn test_record_ticks_bumps_total_by_n() {
        // The honest-counter helper: increment ticks_total by exactly N in one
        // atomic add (Groww counts PERSISTED rows in flush-sized batches).
        let reg = FeedHealthRegistry::new();
        reg.set_connected(Feed::Groww, true);
        reg.record_ticks(Feed::Groww, 5, T0);
        let r = reg.snapshot(Feed::Groww, true, true, true, T0 + 1_000_000_000);
        assert_eq!(r.input.ticks_total, 5, "record_ticks must add exactly N");
        assert_eq!(r.input.last_tick_age_secs, Some(1), "ts stamped");
    }

    #[test]
    fn test_record_feed_liveness_updates_age_not_counts() {
        // Parse-time liveness (2026-07-02): stamps the last-tick time so the
        // stall watchdog sees a DELIVERING feed even while QuestDB is down —
        // but never bumps the persist-honest tick count.
        let reg = FeedHealthRegistry::new();
        reg.set_connected(Feed::Groww, true);
        reg.record_feed_liveness(Feed::Groww, T0);
        let r = reg.snapshot(Feed::Groww, true, true, true, T0 + 2_000_000_000);
        assert_eq!(
            r.input.last_tick_age_secs,
            Some(2),
            "liveness must stamp the last-tick time"
        );
        assert_eq!(
            r.input.ticks_total, 0,
            "liveness must NOT bump the persisted-tick count"
        );
    }

    #[test]
    fn test_record_feed_liveness_interleaves_with_record_ticks() {
        // Flush success later stamps a newer ts + the count; both helpers write
        // the same last-tick slot (last-writer-wins, both are "now").
        let reg = FeedHealthRegistry::new();
        reg.set_connected(Feed::Groww, true);
        reg.record_feed_liveness(Feed::Groww, T0);
        reg.record_ticks(Feed::Groww, 3, T0 + 1_000_000_000);
        let r = reg.snapshot(Feed::Groww, true, true, true, T0 + 1_000_000_000);
        assert_eq!(r.input.ticks_total, 3);
        assert_eq!(r.input.last_tick_age_secs, Some(0), "newest stamp wins");
    }

    #[test]
    fn test_record_ticks_zero_is_noop() {
        // A flush that persisted 0 rows must NOT bump the count or stamp the time
        // (no false "tick arrived" signal — audit Rule 11).
        let reg = FeedHealthRegistry::new();
        reg.set_connected(Feed::Groww, true);
        reg.record_ticks(Feed::Groww, 0, T0);
        let r = reg.snapshot(Feed::Groww, true, true, true, T0);
        assert_eq!(r.input.ticks_total, 0, "n=0 must not bump the count");
        assert_eq!(r.input.last_tick_age_secs, None, "n=0 must not stamp ts");
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
    fn test_registry_unknown_until_first_dhan_signal_then_resolves() {
        // SP5: before any Dhan lane signal, a fresh registry slot is NOT
        // instrumented → Unknown (never a false Down), even during market hours.
        let reg = FeedHealthRegistry::new();
        let pre = reg.snapshot(Feed::Dhan, true, true, true, T0);
        assert_eq!(
            pre.verdict,
            FeedHealthVerdict::Unknown,
            "un-wired Dhan slot is Unknown, not a false Down"
        );
        // The instant the watchdog reports connected + a tick lands, it resolves
        // to a real verdict.
        reg.set_connected(Feed::Dhan, true);
        reg.record_tick(Feed::Dhan, T0);
        let post = reg.snapshot(Feed::Dhan, true, true, true, T0 + 2 * 1_000_000_000);
        assert_eq!(post.verdict, FeedHealthVerdict::Ok);
    }

    #[test]
    fn test_registry_is_auth_rejected_round_trip_and_clear() {
        let reg = FeedHealthRegistry::new();
        reg.set_connected(Feed::Groww, true);
        reg.record_tick(Feed::Groww, T0);
        // Provider rejected the credential → actionable Down even with a fresh tick.
        reg.set_auth_rejected(Feed::Groww, true);
        assert!(reg.is_auth_rejected(Feed::Groww));
        let r = reg.snapshot(Feed::Groww, true, true, true, T0 + 1_000_000_000);
        assert!(r.input.auth_rejected);
        assert_eq!(r.verdict, FeedHealthVerdict::Down);
        assert_eq!(r.reason, "auth rejected — refresh the Groww SSM api-key");
        // Auth recovers → flag cleared → resolves back to a normal verdict.
        reg.set_auth_rejected(Feed::Groww, false);
        assert!(!reg.is_auth_rejected(Feed::Groww));
        let r2 = reg.snapshot(Feed::Groww, true, true, true, T0 + 2 * 1_000_000_000);
        assert!(!r2.input.auth_rejected);
        assert_eq!(r2.verdict, FeedHealthVerdict::Ok);
    }

    #[test]
    fn test_registry_record_ticks_clears_auth_rejected_on_recovery() {
        // The stuck-false-RED fix: once an alert-class sidecar line latches
        // auth_rejected=true, the next SUCCESSFUL tick flow (record_ticks with
        // n>0 — rows actually flushed) must clear it so the page stops showing a
        // permanent "auth rejected" Down after the feed recovered + is streaming.
        let reg = FeedHealthRegistry::new();
        reg.set_connected(Feed::Groww, true);
        reg.set_auth_rejected(Feed::Groww, true);
        let down = reg.snapshot(Feed::Groww, true, true, true, T0);
        assert!(down.input.auth_rejected);
        assert_eq!(down.verdict, FeedHealthVerdict::Down);
        // Rows flow again → flag self-heals, verdict resolves to live.
        reg.record_ticks(Feed::Groww, 3, T0);
        let live = reg.snapshot(Feed::Groww, true, true, true, T0 + 1_000_000_000);
        assert!(
            !live.input.auth_rejected,
            "a successful tick flow must clear the stale auth-rejected flag"
        );
        assert_eq!(live.verdict, FeedHealthVerdict::Ok);
        assert_eq!(live.input.ticks_total, 3);
    }

    #[test]
    fn test_registry_record_ticks_zero_does_not_clear_auth_rejected() {
        // A 0-row flush is NOT a recovery — it must NOT clear the flag (no false
        // "recovered" signal; audit Rule 11). The early-return on n==0 means the
        // clear is never reached.
        let reg = FeedHealthRegistry::new();
        reg.set_connected(Feed::Groww, true);
        reg.set_auth_rejected(Feed::Groww, true);
        reg.record_ticks(Feed::Groww, 0, T0);
        let r = reg.snapshot(Feed::Groww, true, true, true, T0 + 1_000_000_000);
        assert!(
            r.input.auth_rejected,
            "a 0-row flush must NOT clear the auth-rejected flag"
        );
        assert_eq!(r.verdict, FeedHealthVerdict::Down);
        assert_eq!(r.input.ticks_total, 0);
    }

    #[test]
    fn test_registry_clear_auth_rejected_clears_when_set() {
        // HIGH-2: a non-tick recovery edge (a confirmed streaming / auth-OK
        // sidecar line) must clear a stale auth_rejected WITHOUT needing a tick,
        // so a tick-silent window can no longer latch the false-RED for the
        // whole session.
        let reg = FeedHealthRegistry::new();
        reg.set_connected(Feed::Groww, true);
        assert!(!reg.is_auth_rejected(Feed::Groww));
        reg.set_auth_rejected(Feed::Groww, true);
        // §34 getter mirrors the stored flag (the ladder's advance gate).
        assert!(reg.is_auth_rejected(Feed::Groww));
        let down = reg.snapshot(Feed::Groww, true, true, true, T0);
        assert!(down.input.auth_rejected);
        assert_eq!(down.verdict, FeedHealthVerdict::Down);
        // Streaming/auth-OK edge — NO tick recorded.
        reg.clear_auth_rejected(Feed::Groww);
        let after = reg.snapshot(Feed::Groww, true, true, true, T0 + 1_000_000_000);
        assert!(
            !after.input.auth_rejected,
            "a confirmed non-tick recovery edge must clear the stale auth-reject"
        );
        // Note: ticks_total stays 0 — this clear did NOT fabricate a tick.
        assert_eq!(after.input.ticks_total, 0);
    }

    #[test]
    fn test_registry_clear_auth_rejected_is_noop_when_clear() {
        // When the flag is already clear, clear_auth_rejected is a pure no-op
        // (no spurious instrumentation, no state change) — the load short-
        // circuits before any compare_exchange.
        let reg = FeedHealthRegistry::new();
        reg.set_connected(Feed::Groww, true);
        reg.record_tick(Feed::Groww, T0);
        assert!(
            !reg.snapshot(Feed::Groww, true, true, true, T0)
                .input
                .auth_rejected
        );
        reg.clear_auth_rejected(Feed::Groww);
        let r = reg.snapshot(Feed::Groww, true, true, true, T0 + 1_000_000_000);
        assert!(!r.input.auth_rejected, "no-op clear leaves the flag clear");
        assert_eq!(r.verdict, FeedHealthVerdict::Ok);
    }

    #[test]
    fn test_registry_record_tick_clears_auth_rejected_on_recovery() {
        // The single-tick path clears the stale flag symmetrically.
        let reg = FeedHealthRegistry::new();
        reg.set_connected(Feed::Groww, true);
        reg.set_auth_rejected(Feed::Groww, true);
        assert!(
            reg.snapshot(Feed::Groww, true, true, true, T0)
                .input
                .auth_rejected
        );
        reg.record_tick(Feed::Groww, T0);
        let r = reg.snapshot(Feed::Groww, true, true, true, T0 + 1_000_000_000);
        assert!(
            !r.input.auth_rejected,
            "a single recovered tick must clear the stale auth-rejected flag"
        );
        assert_eq!(r.verdict, FeedHealthVerdict::Ok);
    }

    #[test]
    fn test_registry_auth_rejected_marks_instrumented() {
        // A confirmed rejection is a real signal — it must instrument the slot so
        // the page shows the actionable Down, not the Unknown placeholder.
        let reg = FeedHealthRegistry::new();
        reg.set_auth_rejected(Feed::Groww, true);
        let r = reg.snapshot(Feed::Groww, true, true, true, T0);
        assert!(r.input.instrumented);
        assert_eq!(r.verdict, FeedHealthVerdict::Down);
    }

    #[test]
    fn test_registry_set_subscribed_surfaces_counts_and_instruments() {
        // The connect+subscribe PROOF (2026-06-28): set_subscribed records the
        // stock/index counts, marks the feed instrumented, and the snapshot
        // surfaces them. It is display-only — it does NOT by itself flip the
        // verdict (a subscribed-but-silent feed during market hours is still Down).
        let reg = FeedHealthRegistry::new();
        reg.set_subscribed(Feed::Groww, 765, 2);
        let r = reg.snapshot(Feed::Groww, true, true, true, T0);
        assert_eq!(r.input.subscribed_stocks, 765);
        assert_eq!(r.input.subscribed_indices, 2);
        assert!(
            r.input.instrumented,
            "a subscribe count is a real signal → instrumented"
        );
        // Per-feed isolation: Dhan slot untouched.
        let d = reg.snapshot(Feed::Dhan, true, true, true, T0);
        assert_eq!(d.input.subscribed_stocks, 0);
        assert_eq!(d.input.subscribed_indices, 0);
        // Idempotent overwrite (a re-subscribe updates the counts).
        reg.set_subscribed(Feed::Groww, 770, 3);
        let r2 = reg.snapshot(Feed::Groww, true, true, true, T0);
        assert_eq!(r2.input.subscribed_stocks, 770);
        assert_eq!(r2.input.subscribed_indices, 3);
    }

    #[test]
    fn test_registry_set_decode_counts_round_trip() {
        // The honest-feed PROOF (2026-06-29): set_decode_counts records the
        // emitted/dropped counts, marks the feed instrumented, the snapshot
        // surfaces them, and it is per-feed isolated. It is display-only — it does
        // NOT by itself change the verdict (a connected feed with a fresh tick is
        // still Ok even with drops surfaced; drops drive Degraded via record_drops,
        // which is a separate signal — decode_dropped is a producer-side count).
        let reg = FeedHealthRegistry::new();
        reg.set_connected(Feed::Groww, true);
        reg.record_tick(Feed::Groww, T0);
        reg.set_decode_counts(Feed::Groww, 765, 12);
        let r = reg.snapshot(Feed::Groww, true, true, true, T0 + 1_000_000_000);
        assert_eq!(r.input.decoded_emitted, 765);
        assert_eq!(r.input.decoded_dropped, 12);
        assert!(
            r.input.instrumented,
            "a decode count is a real signal → instrumented"
        );
        // Display-only: surfacing 12 producer-side drops does NOT flip the verdict
        // (the feed is connected with a fresh tick → Ok). drops_total is the
        // separate spill/DB drop signal that drives Degraded.
        assert_eq!(r.verdict, FeedHealthVerdict::Ok);
        // Per-feed isolation: Dhan slot untouched.
        let d = reg.snapshot(Feed::Dhan, true, true, true, T0 + 1_000_000_000);
        assert_eq!(d.input.decoded_emitted, 0);
        assert_eq!(d.input.decoded_dropped, 0);
        // Idempotent overwrite (the producer reports cumulative totals).
        reg.set_decode_counts(Feed::Groww, 800, 13);
        let r2 = reg.snapshot(Feed::Groww, true, true, true, T0 + 2_000_000_000);
        assert_eq!(r2.input.decoded_emitted, 800);
        assert_eq!(r2.input.decoded_dropped, 13);
    }

    #[test]
    fn test_registry_disconnected_is_down() {
        let reg = FeedHealthRegistry::new();
        reg.record_tick(Feed::Dhan, T0);
        reg.set_connected(Feed::Dhan, false);
        let r = reg.snapshot(Feed::Dhan, true, true, true, T0 + 1_000_000_000);
        assert_eq!(r.verdict, FeedHealthVerdict::Down);
    }

    #[test]
    fn test_last_tick_age_secs_none_when_no_tick_yet() {
        // The FEED-AGNOSTIC liveness signal must report None (not 0) for a feed
        // that has never streamed a tick — the stall-watchdog must NOT restart a
        // cold pre-open feed.
        let reg = FeedHealthRegistry::new();
        for &feed in Feed::ALL {
            assert_eq!(reg.last_tick_age_secs(feed, T0), None);
        }
    }

    #[test]
    fn test_last_tick_age_secs_reports_correct_age() {
        let reg = FeedHealthRegistry::new();
        reg.record_ticks(Feed::Groww, 5, T0);
        // 31s later → age 31 (NANOS_PER_SEC apart).
        let age = reg.last_tick_age_secs(Feed::Groww, T0 + 31 * NANOS_PER_SEC);
        assert_eq!(age, Some(31));
        // A different feed is independent (per-feed array).
        assert_eq!(
            reg.last_tick_age_secs(Feed::Dhan, T0 + 31 * NANOS_PER_SEC),
            None
        );
    }

    #[test]
    fn test_last_tick_age_secs_clamps_backward_clock_step_to_zero() {
        // A BACKWARD NTP step (now < last_tick) must clamp to age 0 so a clock
        // step can never false-trigger a stall restart.
        let reg = FeedHealthRegistry::new();
        reg.record_tick(Feed::Groww, T0);
        let age = reg.last_tick_age_secs(Feed::Groww, T0 - 5 * NANOS_PER_SEC);
        assert_eq!(age, Some(0));
    }
}
