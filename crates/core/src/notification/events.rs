//! Typed notification events for structured Telegram messages.
//!
//! Every system event that should produce a Telegram alert is represented
//! here. Callers pass events to `NotificationService::notify` — message
//! formatting lives in this module, not at callsites.
//!
//! Defense-in-depth: `to_message()` redacts URL query parameters from
//! `AuthenticationFailed` and `TokenRenewalFailed` reasons to prevent
//! credential leaks in Telegram even if callers pass unsanitized strings.

use tickvault_common::sanitize::redact_url_params;

/// Masks the last two octets of an IPv4 address for safe display in
/// Telegram messages. Prevents full IP exposure while still confirming
/// the network prefix for verification.
///
/// Example: `"59.92.114.17"` → `"59.92.XXX.XX"`
fn mask_ip_for_notification(ip: &str) -> String {
    let parts: Vec<&str> = ip.split('.').collect();
    if parts.len() == 4 {
        format!("{}.{}.XXX.XX", parts[0], parts[1])
    } else {
        "XXX.XXX.XXX.XXX".to_string()
    }
}

/// Alert severity level — determines which notification channels fire.
///
/// `Critical` and `High` → Telegram + SNS SMS.
/// `Medium`, `Low`, `Info` → Telegram only.
///
/// Ordered for comparison: `Info < Low < Medium < High < Critical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Lifecycle events — startup complete, shutdown complete.
    Info,
    /// Normal operations — auth success, token renewed, WS connected.
    Low,
    /// Notable state changes — WS reconnected, shutdown initiated.
    Medium,
    /// Degraded state — WS disconnected, custom alerts.
    High,
    /// System cannot trade — auth failure, token renewal exhausted.
    Critical,
}

impl Severity {
    /// UX tag prefixed to every Telegram message so the operator can
    /// instantly tell at-a-glance how urgent an alert is. Added 2026-04-17
    /// after Parthiban noted he couldn't distinguish boot-progress pings
    /// from real incidents without reading the full message body.
    ///
    /// Mapping: emoji icon + square-bracket tag + exact severity name.
    /// Operator workflow: any `[HIGH]` or `[CRITICAL]` message goes to
    /// Claude Code for debugging; `[INFO]`/`[LOW]`/`[MEDIUM]` is scroll-by.
    /// Lowercase Prometheus label for this severity. Used by the
    /// `tv_telegram_dispatched_total` and `tv_telegram_dropped_total`
    /// counters (Wave 3-B Item 11).
    pub const fn as_label(&self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    pub fn tag(&self) -> &'static str {
        match self {
            Self::Info => "\u{2139}\u{fe0f} [INFO]",  // ℹ️
            Self::Low => "\u{2705} [LOW]",            // ✅
            Self::Medium => "\u{1f535} [MEDIUM]",     // 🔵
            Self::High => "\u{26a0}\u{fe0f} [HIGH]",  // ⚠️
            Self::Critical => "\u{1f6a8} [CRITICAL]", // 🚨
        }
    }
}

/// Which depth levels participate in a routine rebalance swap.
///
/// `TwentyOnly` — the underlying has only a 20-level feed (FINNIFTY,
/// MIDCPNIFTY today).
/// `TwentyAndTwoHundred` — NIFTY and BANKNIFTY. Both 20-level (ATM ± 24)
/// and 200-level (ATM CE + PE) are swapped on the same socket without
/// disconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthRebalanceLevels {
    /// 20-level only (FINNIFTY, MIDCPNIFTY).
    TwentyOnly,
    /// 20-level + 200-level (NIFTY, BANKNIFTY).
    TwentyAndTwoHundred,
}

impl DepthRebalanceLevels {
    /// Title fragment used in the Telegram message headline so the operator
    /// can tell the swap scope at a glance without reading the Action line.
    pub fn title_fragment(&self) -> &'static str {
        match self {
            Self::TwentyOnly => "Depth-20",
            Self::TwentyAndTwoHundred => "Depth-20+200",
        }
    }

    /// Body line describing the action taken. Kept consistent with the
    /// prior inline `Custom` wording (zero-disconnect swap on same socket)
    /// so operators skimming history see continuity.
    pub fn action_line(&self) -> &'static str {
        match self {
            Self::TwentyOnly => {
                "Action: zero-disconnect swap — 20-level unsub old / sub new on same socket \
                 (no 200-level for this underlying)"
            }
            Self::TwentyAndTwoHundred => {
                "Action: zero-disconnect swap — 20-level + 200-level unsub old / sub new on same socket"
            }
        }
    }
}

/// All events that produce a Telegram alert.
///
/// Adding a new event: add a variant here, add its message arm in
/// `NotificationEvent::to_message`, add a callsite in `main.rs`.
#[derive(Debug, Clone)]
pub enum NotificationEvent {
    /// Application boot completed successfully.
    StartupComplete {
        /// "LIVE" or "OFFLINE".
        mode: &'static str,
    },

    /// Dhan authentication token acquired at boot.
    AuthenticationSuccess,

    /// Dhan authentication failed at boot — system started in offline mode.
    AuthenticationFailed { reason: String },

    /// Dhan authentication attempt failed transiently (network blip, DNS
    /// hiccup, TLS handshake timeout) but the retry loop is still active and
    /// will attempt again in `next_retry_ms` milliseconds. Fires at
    /// `Severity::Low` — visible in Telegram but no SMS escalation. If all
    /// retries exhaust (e.g., TOTP secret genuinely wrong, permanent auth
    /// rejection), the final failure fires `AuthenticationFailed` at
    /// `Severity::Critical` instead. This split prevents a single 100-ms
    /// blip on `auth.dhan.co` from triggering a `CRITICAL` alert that the
    /// very next retry resolves. Queue item Q2 (2026-04-23 session).
    AuthenticationTransientFailure {
        attempt: u32,
        reason: String,
        next_retry_ms: u64,
    },

    /// Pre-market profile check FAILED — dataPlan, activeSegment, or token
    /// expiry is not acceptable for today's trading session. Fires CRITICAL
    /// Telegram on every failure. If the check runs during market hours
    /// AND fails, the boot sequence HALTS (Parthiban directive 2026-04-21).
    ///
    /// Common causes:
    /// - `dataPlan != "Active"` — subscription expired over weekend
    /// - `activeSegment` lacks `"Derivative"` — F&O access revoked
    /// - Token has < 4h until expiry — rotate before market open
    ///
    /// `within_market_hours = true` means HALT fired; `false` means operator
    /// has until market open to investigate.
    PreMarketProfileCheckFailed {
        reason: String,
        within_market_hours: bool,
    },

    /// Mid-session profile check failed during market hours (queue item
    /// I7, 2026-04-21). Fires CRITICAL on the rising edge only — if
    /// the profile recovers on a subsequent check, an INFO log is
    /// emitted (no Telegram). The app does NOT HALT mid-session — a
    /// mid-session HALT would drop the live WS feed, which costs more
    /// than the silent-failure risk. Operator remediation is manual.
    MidSessionProfileInvalidated { reason: String },

    /// JWT token renewed successfully by background task.
    TokenRenewed,

    /// Token renewal failed — background task will retry.
    TokenRenewalFailed { attempts: u32, reason: String },

    /// WebSocket connection established.
    WebSocketConnected { connection_index: usize },

    /// Aggregate summary of the live-market-feed pool after spawn.
    /// Emitted once per boot path so operators survive Telegram rate-limit
    /// drops of the per-connection `WebSocketConnected` events.
    /// `total` is the expected count (== connected on healthy boot).
    WebSocketPoolOnline { connected: usize, total: usize },

    /// Pool-level CRITICAL alert: every connection has been down longer than
    /// `POOL_DEGRADED_ALERT_SECS` (default 60). One alert per down-cycle.
    /// Fired by the pool watchdog on `WatchdogVerdict::Degraded`.
    WebSocketPoolDegraded { down_secs: u64 },

    /// Pool-level INFO: the pool recovered from an all-down state.
    /// Fired on `WatchdogVerdict::Recovered`.
    WebSocketPoolRecovered { was_down_secs: u64 },

    /// Pool-level FATAL: down longer than `POOL_HALT_SECS` (default 300).
    /// The process will exit with status 2 so the supervisor restarts.
    /// Fired on `WatchdogVerdict::Halt`.
    WebSocketPoolHalt { down_secs: u64 },

    /// Depth setup timed out waiting for the main-feed index LTPs needed
    /// for ATM strike selection. Depth connections proceed with a fallback
    /// strike (median). `waited_secs` reflects how long we waited before
    /// giving up.
    DepthIndexLtpTimeout { waited_secs: u64 },

    /// Plan item #5 (2026-04-22): once-per-day positive confirmation that
    /// every feed is actually streaming live data at market open. Fires
    /// at 09:15:30 IST on each trading day. Answers Parthiban's "how do I
    /// know if connected" question directly — without this, operators only
    /// see disconnect/reconnect EDGES, never a positive "we are healthy"
    /// signal. Severity = Info so it never wakes anyone up.
    MarketOpenStreamingConfirmation {
        main_feed_active: usize,
        main_feed_total: usize,
        depth_20_active: usize,
        depth_200_active: usize,
        order_update_active: bool,
    },

    /// Audit finding #8 (2026-04-24): when the 09:15:30 heartbeat fires
    /// but the main-feed connection count is 0, the situation is the
    /// OPPOSITE of "streaming OK" — it's a catastrophic missed market
    /// open. The heartbeat codepath previously fired `MarketOpenStreaming
    /// Confirmation` (Severity::Info) even in that case, reading as
    /// "Streaming live @ 09:15:30 IST / Main feed: 0/5" which is confusing
    /// and under-alerted. This variant fires at Severity::High so the
    /// operator pages immediately instead of mistaking it for a heartbeat.
    MarketOpenStreamingFailed {
        main_feed_active: usize,
        main_feed_total: usize,
        depth_20_active: usize,
        depth_200_active: usize,
        order_update_active: bool,
    },

    /// Plan item C (2026-04-22, visibility version): once-per-day audit
    /// trail of what NIFTY + BANKNIFTY 09:12 closes were used as the
    /// authoritative anchor for the day's depth ATM. The 60s depth
    /// rebalancer is what actually keeps depth subscribed to the right
    /// strikes during the session — this Telegram is for operator
    /// visibility into "what 09:12 close grounded today's depth" so
    /// drift complaints can be cross-checked against a known anchor.
    /// Severity = Info (no wake-ups).
    MarketOpenDepthAnchor {
        /// "NIFTY" or "BANKNIFTY"
        underlying: String,
        /// 09:12 close from the preopen buffer (or backtracked 09:11/10/09/08).
        close_0912: Option<f64>,
        /// Strike price of the ATM contract derived from `close_0912`.
        atm_strike: Option<f64>,
        /// Source minute: 4 = 09:12, 3 = 09:11, ..., 0 = 09:08, None = no data.
        source_minute_slot: Option<usize>,
    },

    /// Option C (2026-04-17): Depth setup dropped a specific underlying —
    /// the grace window expired without a valid spot price OR the option
    /// chain was missing. Complements `DepthIndexLtpTimeout` which fires
    /// once for the bundle; this fires per missing underlying so operators
    /// see exactly which ones were skipped (e.g. FINNIFTY).
    DepthUnderlyingMissing { underlying: String, reason: String },

    /// O3 (2026-04-17): The depth rebalancer detected a stale spot price
    /// for this underlying and skipped the rebalance decision. A stale
    /// price likely means the main-feed WebSocket isn't delivering index
    /// LTPs for this symbol — acting on it would swap the 200-level
    /// connection to the wrong ATM strike. `age_secs` is the observed age
    /// at the moment of detection.
    DepthSpotPriceStale { underlying: String, age_secs: u64 },

    /// O1 (2026-04-17): The Phase 2 scheduler woke up to do its run.
    /// `minutes_late` = 0 on the normal 09:12 path, > 0 when fired via the
    /// `RunImmediate` recovery path (crash mid-market or fresh late start).
    Phase2Started { minutes_late: u64 },

    /// O1: The Phase 2 scheduler fired via the `RunImmediate` path instead
    /// of the normal 09:12 schedule — this is informational so operators
    /// know the system crashed or started late and is catching up.
    Phase2RunImmediate { minutes_late: u64 },

    /// O1: Phase 2 completed — unified 09:12:30 IST dispatch for stock
    /// F&O + depth-20 + depth-200 (plan item G, 2026-04-22). Counts are
    /// zero when the respective feed wasn't dispatched this run (e.g.
    /// depth underlyings that had no 09:12 tick).
    Phase2Complete {
        added_count: usize,
        duration_ms: u64,
        /// Plan item G: number of depth-20 underlyings subscribed at
        /// 09:12:30. Zero on recovery from snapshot that predates
        /// depth-in-Phase-2 wiring.
        depth_20_underlyings: usize,
        /// Plan item G: number of depth-200 contracts subscribed at
        /// 09:12:30. Max 4 today (NIFTY CE/PE + BANKNIFTY CE/PE).
        depth_200_contracts: usize,
    },

    /// O1: Phase 2 failed — LTPs never arrived within `MAX_LTP_ATTEMPTS`
    /// × `LTP_WAIT_SECS_PER_ATTEMPT`. Stock F&O remains unsubscribed for
    /// this session. Operator should investigate the main-feed WebSocket.
    Phase2Failed { reason: String, attempts: u32 },

    /// O1: Phase 2 was skipped today (weekend, holiday, or post-market
    /// restart). Low-noise — informational only.
    Phase2Skipped { reason: String },

    /// 2026-04-25: Mid-market cold start completed. Fires once when the app
    /// boots between 09:13 and 15:30 IST and the live-tick ATM resolver +
    /// REST fallback + QuestDB-prev-close chain has resolved (or skipped)
    /// every F&O stock. Counts MUST sum to the F&O stock universe size.
    /// Severity::Info.
    MidMarketBootComplete {
        /// Stocks resolved via live cash-equity tick (primary path).
        stocks_live_tick: usize,
        /// Stocks resolved via REST `/v2/marketfeed/ltp` fallback (stragglers).
        stocks_rest: usize,
        /// Stocks resolved via QuestDB previous close (last-resort fallback).
        stocks_quest_db: usize,
        /// Stocks skipped for the day (no LTP from any source).
        stocks_skipped: usize,
        /// Total subscribed instruments after Phase 2 dispatch.
        total_subscribed: usize,
        /// Wall-clock latency from boot to fully subscribed (millis).
        latency_ms: u64,
    },

    /// WebSocket disconnected (unexpected, will reconnect).
    WebSocketDisconnected {
        connection_index: usize,
        reason: String,
    },

    /// WebSocket disconnected OUTSIDE market hours — Dhan-side idle reset
    /// (e.g. TCP `ResetWithoutClosingHandshake` during pre-market before
    /// 09:00 IST). Auto-reconnected by the retry loop; not actionable for
    /// the operator. Kept as an INFO-level audit trail (severity Low) per
    /// Parthiban override (2026-04-22): "as long as it's not our
    /// implementation causing the disconnect, don't alert HIGH off-hours".
    ///
    /// Supersedes the 2026-04-21 directive that fired HIGH on every
    /// disconnect regardless of market hours.
    WebSocketDisconnectedOffHours {
        connection_index: usize,
        reason: String,
    },

    /// WebSocket reconnected after disconnection.
    WebSocketReconnected { connection_index: usize },

    /// Wave 2 Item 5 (G1, WS-GAP-04) — main-feed connection entered
    /// post-close sleep mode (instead of legacy give-up `return false`).
    /// Severity::Low — informational, the connection is dormant not
    /// failed and will auto-reconnect at next NSE market open.
    WebSocketSleepEntered {
        feed: String,
        connection_index: usize,
        sleep_secs: u64,
    },

    /// Wave 2 Item 5 (G1) — connection woke from post-close sleep and
    /// is about to attempt reconnect. Severity::Info.
    WebSocketSleepResumed {
        feed: String,
        connection_index: usize,
        slept_for_secs: u64,
    },

    /// Wave 2 Item 5.4 (AUTH-GAP-03) — TokenManager force-renewed the
    /// JWT immediately after a WebSocket woke from post-close sleep
    /// because the cached token had less than the configured headroom
    /// remaining. Severity::Low — informational; the renewal succeeded.
    /// Use `WebSocketTokenForceRenewalFailed`-style routing via the
    /// adjacent error log for failure cases.
    WebSocketTokenForceRenewedOnWake {
        feed: String,
        connection_index: usize,
        remaining_secs_before: i64,
        threshold_secs: i64,
    },

    /// 20-level depth WebSocket connected for an underlying.
    DepthTwentyConnected { underlying: String },

    /// 20-level depth WebSocket disconnected.
    DepthTwentyDisconnected { underlying: String, reason: String },

    /// 20-level depth WebSocket disconnected OUTSIDE market hours, OR with a
    /// placeholder (SecurityId=0) subscription. Fires at `Severity::Low` so
    /// the operator sees the event in Telegram but gets no SMS escalation.
    ///
    /// Rationale (Q5, 2026-04-23): a post-market boot cannot populate
    /// `SharedSpotPrices` (main feed streams zero ticks after 15:30 IST),
    /// so the ATM selector falls through to the "deferred" placeholder
    /// (SecurityId=0). The depth connection then hits Dhan's TCP-reset
    /// after 60 reconnect attempts and fires a `[HIGH]` disconnect alert
    /// — which is exactly the anti-pattern we killed for the 15:45 IST
    /// depth-stale-spot storm. Same rule as `WebSocketDisconnectedOffHours`.
    DepthTwentyDisconnectedOffHours { underlying: String, reason: String },

    /// 20-level depth WebSocket reconnected after a transient disconnect.
    /// Fires on every successful reconnect, inside or outside market
    /// hours (Parthiban directive 2026-04-21 — full audit trail on all
    /// WS events).
    DepthTwentyReconnected { underlying: String },

    /// 200-level depth WebSocket connected.
    ///
    /// `contract` is the precise contract label (e.g. `NIFTY-Apr2026-22500-CE`)
    /// and `security_id` is the exact Dhan security ID — both are required so
    /// the operator can quote the exact instrument when escalating to Dhan
    /// support. 200-depth is 1 instrument per connection, so a generic
    /// underlying label would lose the strike/expiry/side information.
    DepthTwoHundredConnected { contract: String, security_id: u32 },

    /// 200-level depth WebSocket disconnected.
    DepthTwoHundredDisconnected {
        contract: String,
        security_id: u32,
        reason: String,
    },

    /// 200-level depth disconnected OUTSIDE market hours, OR with a
    /// placeholder (SecurityId=0) subscription. Fires at `Severity::Low` so
    /// a post-market boot storm (4× contracts × 60 reconnect attempts)
    /// doesn't escalate to SMS. See [`Self::DepthTwentyDisconnectedOffHours`]
    /// for the full rationale — same rule, 200-level variant.
    DepthTwoHundredDisconnectedOffHours {
        contract: String,
        security_id: u32,
        reason: String,
    },

    /// 200-level depth WebSocket reconnected after a transient disconnect.
    /// Fires on every successful reconnect, inside or outside market
    /// hours (Parthiban directive 2026-04-21 — full audit trail on all
    /// WS events).
    DepthTwoHundredReconnected { contract: String, security_id: u32 },

    /// Depth rebalance SUCCESS — routine zero-disconnect swap on spot drift.
    ///
    /// Fires at `Severity::Low` (green): the swap is a planned working-as-
    /// designed event per `.claude/rules/project/depth-subscription.md`. The
    /// previous `[HIGH]` amber alert via `Custom { message: … }` was alert
    /// noise (fired every 60s on drift) and has been replaced by this typed
    /// variant (Parthiban directive 2026-04-24).
    ///
    /// Title format: `Depth-20 rebalance: <UL>` for indices without 200-level,
    /// `Depth-20+200 rebalance: <UL>` for NIFTY / BANKNIFTY. Level is visible
    /// at a glance — operator doesn't have to read the Action line.
    DepthRebalanced {
        /// Underlying symbol (e.g. `NIFTY`, `BANKNIFTY`, `FINNIFTY`, `MIDCPNIFTY`).
        underlying: String,
        /// Previous spot price (from drift check).
        previous_spot: f64,
        /// Current spot price (from drift check).
        current_spot: f64,
        /// Old CE contract label (e.g. `NIFTY 28 APR 25000 CALL (SID 12345)`).
        old_ce: String,
        /// Old PE contract label.
        old_pe: String,
        /// New CE contract label.
        new_ce: String,
        /// New PE contract label.
        new_pe: String,
        /// Which depth levels participate in the swap.
        levels: DepthRebalanceLevels,
    },

    /// Depth rebalance FAILURE — command channel broken, new ATM unresolved,
    /// or Swap command not acknowledged. Fires at `Severity::High` because
    /// depth-subscription quality degrades until next successful rebalance.
    DepthRebalanceFailed {
        /// Underlying symbol.
        underlying: String,
        /// Human-readable failure reason.
        reason: String,
    },

    /// Order update WebSocket connected.
    OrderUpdateConnected,

    /// O2 (2026-04-17): Order Update WebSocket has completed the Dhan auth
    /// handshake — fires exactly once per process lifetime when the server
    /// sends the first message that classifies as `AuthResponseKind::Success`
    /// OR the first successful `parse_order_update`. This is the earliest
    /// proof Dhan accepted the token; the earlier `OrderUpdateConnected`
    /// event only signals that the tokio task started.
    OrderUpdateAuthenticated,

    /// Order update WebSocket disconnected.
    OrderUpdateDisconnected { reason: String },

    /// Order update WebSocket reconnected after a transient disconnect.
    /// Fires on every successful reconnect, inside or outside market
    /// hours (Parthiban directive 2026-04-21 — full audit trail on all
    /// WS events).
    ///
    /// Severity intentionally downgraded to `Severity::Low` (was Medium
    /// before 2026-04-26). Dhan's order-update server idle-disconnects
    /// accounts that haven't placed an order in 30-60 minutes, so on a
    /// dry-run / paper-trading day this event fires 5-10 times. A
    /// successful recovery is operator-informational, not pager-worthy
    /// — MEDIUM at that volume is pure pager fatigue. The companion
    /// `OrderUpdateDisconnected` (Severity::High) still pages on the
    /// disconnect leg, and `OrderUpdateReconnectionExhausted` (if/when
    /// added) would page CRITICAL on actual loss-of-feed.
    OrderUpdateReconnected { consecutive_failures: u32 },

    /// CRITICAL: zero live ticks received during market hours past the
    /// configured silence threshold. Fires edge-triggered (once on rising
    /// edge — when ticks resume, an INFO recovery log fires but no
    /// Telegram). This event would have caught the 2026-04-21 morning
    /// failure where the WS was connected but Dhan stopped streaming
    /// (likely data-plan issue).
    NoLiveTicksDuringMarketHours {
        /// How long the heartbeat has been stale, in seconds.
        silent_for_secs: u64,
        /// Threshold that triggered the alert, in seconds.
        threshold_secs: u64,
    },

    /// Graceful shutdown initiated.
    ShutdownInitiated,

    /// Application stopped.
    ShutdownComplete,

    /// Wave 3-C Item 12 — market-open self-test all-green positive ping
    /// at 09:16:00 IST. Severity::Info; never wakes the operator. Maps
    /// to ErrorCode `SELFTEST-01`.
    SelfTestPassed {
        /// Number of sub-checks evaluated and passed (always equal to
        /// the total today, but carried forward for schema evolution).
        checks_passed: usize,
    },

    /// Wave 3-C Item 12 — market-open self-test detected one or more
    /// non-critical sub-check failures at 09:16:00 IST. Severity::High.
    /// `failed` is a list of static strings naming the sub-checks that
    /// tripped (no user-controllable data — safe for log + Telegram).
    /// Maps to ErrorCode `SELFTEST-02`.
    SelfTestDegraded {
        /// Number of green sub-checks (out of `TOTAL_SUB_CHECKS`).
        checks_passed: usize,
        /// Number of red sub-checks.
        checks_failed: usize,
        /// Names of the red sub-checks (static strings).
        failed: Vec<&'static str>,
    },

    /// Wave 3-C Item 12 — market-open self-test detected a critical
    /// sub-check failure (no main feed / QuestDB down / token expired)
    /// at 09:16:00 IST. Severity::Critical so the operator pages
    /// immediately. Maps to ErrorCode `SELFTEST-02`.
    SelfTestCritical {
        /// Number of red sub-checks (≥ 1, with at least one critical).
        checks_failed: usize,
        /// Names of the red sub-checks (static strings).
        failed: Vec<&'static str>,
    },

    /// Instrument build succeeded (first build of the day).
    InstrumentBuildSuccess {
        /// CSV source: "primary", "fallback", or "cache".
        source: String,
        /// Total derivative contracts built.
        derivative_count: usize,
        /// Total F&O underlyings built.
        underlying_count: usize,
    },

    /// Instrument build failed — includes manual trigger URL for retry.
    InstrumentBuildFailed {
        /// Error description.
        reason: String,
        /// URL for manual one-shot retry.
        manual_trigger_url: String,
    },

    /// Historical candle fetch completed successfully (all instruments OK).
    HistoricalFetchComplete {
        /// Number of instruments fetched successfully.
        instruments_fetched: usize,
        /// Number of instruments skipped (derivatives, already fetched).
        instruments_skipped: usize,
        /// Total candles ingested across all timeframes.
        total_candles: usize,
        /// Number of QuestDB write failures (candles lost during persist).
        persist_failures: usize,
    },

    /// 2026-04-24 — idempotent re-run detected. The fetch call returned
    /// `instruments_fetched == 0 && total_candles == 0`, but QuestDB
    /// already has today's historical candles from a prior run, so the
    /// zero result is "nothing to do" not "fetch broke". Fired at
    /// [`Severity::Low`] instead of the HIGH `HistoricalFetchFailed`.
    ///
    /// See `count_historical_candles_for_ist_day` in `cross_verify.rs`
    /// for the presence check.
    HistoricalFetchAlreadyAvailable {
        /// IST date of the trading day (e.g. `2026-04-24`).
        today_ist: String,
        /// Count of rows in `historical_candles` with `ts >= today
        /// IST midnight`. Bounded above by ~60M (universe × timeframes
        /// × candles-per-day) — `u64` is ample headroom.
        today_candles: u64,
    },

    /// Historical candle fetch completed with failures.
    HistoricalFetchFailed {
        /// Number of instruments that succeeded.
        instruments_fetched: usize,
        /// Number of instruments that failed.
        instruments_failed: usize,
        /// Total candles ingested.
        total_candles: usize,
        /// Number of QuestDB write failures (candles lost during persist).
        persist_failures: usize,
        /// Symbol names of failed instruments (up to 50).
        failed_instruments: Vec<String>,
        /// Breakdown of failure reasons: "token_expired", "network_or_api", "persist".
        failure_reasons: std::collections::HashMap<String, usize>,
    },

    /// Candle cross-verification passed — all timeframes have expected coverage.
    CandleVerificationPassed {
        /// Instruments checked across all timeframes.
        instruments_checked: usize,
        /// Total candles in QuestDB.
        total_candles: usize,
        /// Per-timeframe breakdown (pre-formatted lines).
        timeframe_details: String,
        /// OHLC violations found (high < low).
        ohlc_violations: usize,
        /// Data violations (non-positive prices).
        data_violations: usize,
        /// Timestamp violations (outside market hours).
        timestamp_violations: usize,
        /// Weekend violations (candles on Saturday/Sunday).
        weekend_violations: usize,
    },

    /// Candle cross-verification found gaps in stored data.
    CandleVerificationFailed {
        /// Instruments checked.
        instruments_checked: usize,
        /// Instruments with gaps.
        instruments_with_gaps: usize,
        /// Per-timeframe breakdown (pre-formatted lines).
        timeframe_details: String,
        /// OHLC violations found (high < low).
        ohlc_violations: usize,
        /// Data violations (non-positive prices).
        data_violations: usize,
        /// Timestamp violations (outside market hours).
        timestamp_violations: usize,
        /// Pre-formatted OHLC violation detail lines for Telegram.
        ohlc_details: Vec<String>,
        /// Pre-formatted data violation detail lines for Telegram.
        data_details: Vec<String>,
        /// Pre-formatted timestamp violation detail lines for Telegram.
        timestamp_details: Vec<String>,
        /// Weekend violations (candles on Saturday/Sunday).
        weekend_violations: usize,
        /// Pre-formatted weekend violation detail lines for Telegram.
        weekend_details: Vec<String>,
    },

    /// Historical vs Live candle cross-match passed — all OHLCV values match.
    CandleCrossMatchPassed {
        /// Number of timeframes compared.
        timeframes_checked: usize,
        /// Total candles compared.
        candles_compared: usize,
        /// Human-readable IST session label, e.g. `"2026-04-20 09:15–15:30 IST"`.
        today_ist_label: String,
        /// Number of IDX_I (index) instruments in the verified scope.
        /// Defaults to 0 for legacy callers — renders a blank scope breakdown.
        scope_indices: usize,
        /// Number of NSE_EQ (stock equity) instruments in the verified scope.
        scope_equities: usize,
        /// Per-timeframe cell counts: `(timeframe, cells)` e.g.
        /// `[("1m", 79125), ("5m", 15825)]`. Rendered as a monospace
        /// coverage table in Telegram. Empty for legacy callers.
        per_tf_cells: Vec<(String, usize)>,
    },

    /// Historical vs Live candle cross-match found mismatches.
    CandleCrossMatchFailed {
        /// Total candles compared.
        candles_compared: usize,
        /// Total mismatches found.
        mismatches: usize,
        /// Historical candle exists but no live data (WebSocket missed ticks).
        missing_live: usize,
        /// Pre-formatted mismatch detail lines for Telegram.
        /// Full list — no truncation (Parthiban directive 2026-04-21).
        /// Chunked by the notification layer when >4000 chars.
        mismatch_details: Vec<String>,
        /// Human-readable IST session label, e.g. `"2026-04-20 09:15–15:30 IST"`.
        today_ist_label: String,
        /// Number of IDX_I (index) instruments in the verified scope.
        scope_indices: usize,
        /// Number of NSE_EQ (stock equity) instruments in the verified scope.
        scope_equities: usize,
        /// Live candle exists but historical doesn't (rare — Dhan REST gap).
        missing_historical: usize,
        /// Both present but OHLCV differs.
        value_mismatches: usize,
        /// Per-timeframe gap counts: `(timeframe, gaps)`.
        per_tf_gaps: Vec<(String, usize)>,
    },

    /// Historical vs Live candle cross-match skipped — no live data in
    /// materialized views yet (first run, fresh DB, or post-market boot
    /// before any live ticks have been collected during market hours).
    CandleCrossMatchSkipped {
        /// Human-readable reason for skipping (e.g. "no live data in materialized views").
        reason: String,
        /// LEFT JOIN row count from the cross-match query. Meaningless on its
        /// own when no live data exists, but surfaced for diagnostic parity
        /// with `CandleCrossMatchPassed`/`Failed`.
        candles_compared: usize,
    },

    /// Public IP verification failed — static IP mismatch or detection failure.
    IpVerificationFailed {
        /// Human-readable reason for the failure.
        reason: String,
    },

    /// Public IP verification succeeded at boot.
    IpVerificationSuccess {
        /// The verified public IP address.
        verified_ip: String,
    },

    /// Boot health check completed — infrastructure services verified.
    BootHealthCheck {
        /// Number of services that passed health check.
        services_healthy: usize,
        /// Total services checked.
        services_total: usize,
    },

    /// Boot deadline missed — system did not complete startup within allowed time.
    BootDeadlineMissed {
        /// Deadline in seconds that was exceeded.
        deadline_secs: u64,
        /// Step that was running when deadline hit.
        step: String,
    },

    /// Wave 2-C Item 7.3 (G8) — wall-clock skew vs trusted source exceeded
    /// `CLOCK_SKEW_HALT_THRESHOLD_SECS`. Boot HALTS. Severity::Critical.
    BootClockSkewExceeded {
        /// Observed signed skew (positive = local clock ahead of trusted source).
        skew_secs: f64,
        /// Threshold that was exceeded (mirrors
        /// `CLOCK_SKEW_HALT_THRESHOLD_SECS` from common::constants).
        threshold_secs: f64,
        /// Probe source that observed the skew (e.g., "chronyc", "questdb_now").
        source: String,
    },

    /// Order rejected by Dhan API or OMS validation.
    OrderRejected {
        /// Correlation ID of the rejected order.
        correlation_id: String,
        /// Reason for rejection.
        reason: String,
    },

    /// OMS circuit breaker opened — order API calls halted.
    CircuitBreakerOpened {
        /// Number of consecutive failures that triggered the open.
        consecutive_failures: u64,
    },

    /// OMS circuit breaker closed — order API calls resumed.
    CircuitBreakerClosed,

    /// OMS rate limit exhausted — order rejected due to SEBI limits.
    RateLimitExhausted {
        /// Which limit was hit (e.g., "per_second", "daily").
        limit_type: String,
    },

    /// Risk engine halted trading — daily loss breach or position limit.
    RiskHalt {
        /// Reason for the halt (e.g., "daily_loss_breach", "position_limit").
        reason: String,
    },

    /// WebSocket reconnection exhausted — all retry attempts failed.
    WebSocketReconnectionExhausted {
        /// Connection index that failed.
        connection_index: usize,
        /// Total reconnection attempts made.
        attempts: u64,
    },

    /// Token renewal deadline missed — renewal failed past safe window.
    TokenRenewalDeadlineMissed {
        /// IST hour when the deadline was crossed.
        deadline_hour_ist: u32,
    },

    /// QuestDB persistence disconnected — ticks being buffered, not persisted.
    QuestDbDisconnected {
        /// Which writer lost connection (e.g., "tick", "depth", "candle",
        /// "liveness-check").
        writer: String,
        /// Numeric degradation signal. Interpretation depends on `signal_kind`:
        /// for the liveness-check path this is the consecutive-failure count;
        /// for the tick-writer lag path it's the number of dropped records.
        /// Never a hardcoded zero — always reflects reality at alert time.
        signal: u64,
        /// Human-readable description of what `signal` represents (e.g.
        /// `"consecutive liveness failures"`, `"ticks dropped by broadcast lag"`).
        /// Rendered in the Telegram message so operators aren't guessing.
        signal_kind: String,
    },

    /// QuestDB persistence reconnected — buffered data draining.
    QuestDbReconnected {
        /// Which writer recovered.
        writer: String,
        /// Total ticks/records drained from buffer.
        drained_count: usize,
    },

    /// Custom alert from any component.
    Custom { message: String },
}

// ---------------------------------------------------------------------------
// Cross-match Telegram rendering (extracted — Parthiban 2026-04-21)
// ---------------------------------------------------------------------------

fn render_scope_line(scope_indices: usize, scope_equities: usize) -> String {
    let total = scope_indices.saturating_add(scope_equities);
    if total == 0 {
        return "🎯 Scope    Indices + Stock Equities only\n            (F&O not in Dhan historical API)".to_string();
    }
    format!(
        "🎯 Scope    Indices + Stock Equities only\n            (F&O not in Dhan historical API)\n📊 Universe {total} instruments\n            • {scope_indices} indices  • {scope_equities} equities"
    )
}

fn render_window_line(today_ist_label: &str) -> String {
    if today_ist_label.is_empty() {
        "📅 Window   (not supplied)".to_string()
    } else {
        format!("📅 Window   {today_ist_label}")
    }
}

fn render_cross_match_ok(
    timeframes_checked: usize,
    candles_compared: usize,
    today_ist_label: &str,
    scope_indices: usize,
    scope_equities: usize,
    per_tf_cells: &[(String, usize)],
) -> String {
    let mut out = String::new();
    out.push_str("✅ <b>CROSS-MATCH OK</b>\n\n");
    out.push_str(&render_window_line(today_ist_label));
    out.push_str("\n\n");
    out.push_str(&render_scope_line(scope_indices, scope_equities));
    out.push_str("\n\n ─── COVERAGE ──────────────────────\n<pre>");
    if per_tf_cells.is_empty() {
        out.push_str(&format!(
            "Timeframes: {timeframes_checked}  Candles: {candles_compared}"
        ));
    } else {
        out.push_str("   TF  │  Cells    │ Gaps │ Diffs\n");
        out.push_str("  ─────┼───────────┼──────┼───────\n");
        for (tf, cells) in per_tf_cells {
            out.push_str(&format!(
                "  {tf:>3}  │ {cells:>9}  │   0  │   0\n",
                tf = tf,
                cells = cells
            ));
        }
    }
    out.push_str("</pre>\n ────────────────────────────────────\n\n");
    out.push_str("✓ All OHLCV values match bit-for-bit\n");
    out.push_str("  zero tolerance · precision-verified");
    out
}

#[allow(clippy::too_many_arguments)] // APPROVED: struct-equivalent unpacking of event fields; grouping adds a named struct for a private helper.
fn render_cross_match_failed(
    candles_compared: usize,
    mismatches: usize,
    missing_live: usize,
    missing_historical: usize,
    value_mismatches: usize,
    mismatch_details: &[String],
    today_ist_label: &str,
    scope_indices: usize,
    scope_equities: usize,
    per_tf_gaps: &[(String, usize)],
) -> String {
    let mut out = String::new();
    out.push_str("❌ <b>CROSS-MATCH FAILED</b>\n\n");
    out.push_str(&render_window_line(today_ist_label));
    out.push_str("\n\n");
    out.push_str(&render_scope_line(scope_indices, scope_equities));
    out.push_str("\n\n ─── SUMMARY ───────────────────────\n<pre>");
    out.push_str(&format!("   🔴 Missing live       {missing_live:>6}\n"));
    out.push_str(&format!(
        "   🔴 Missing historical {missing_historical:>6}\n"
    ));
    out.push_str(&format!("   🟡 Value mismatches   {value_mismatches:>6}\n"));
    out.push_str("  ─────────────────────────────────\n");
    out.push_str(&format!("      Total issues       {mismatches:>6}\n"));
    out.push_str(&format!("      Cells compared     {candles_compared:>6}"));
    out.push_str("</pre>\n");

    if !per_tf_gaps.is_empty() {
        out.push_str("\n ─── GAPS BY TIMEFRAME ─────────────\n<pre>");
        for (tf, gaps) in per_tf_gaps {
            out.push_str(&format!("   {tf:>3}   {gaps:>5}\n", tf = tf, gaps = gaps));
        }
        out.push_str("</pre>\n");
    }

    if !mismatch_details.is_empty() {
        out.push_str("\n📋 <b>Full list below</b>");
        out.push_str("\n   Order: MISSING LIVE → VALUE DIFF → MISSING HIST\n\n<pre>");
        for line in mismatch_details {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("</pre>\n🏁 End of report");
    }
    out
}

impl NotificationEvent {
    /// Formats the event as a Telegram message.
    ///
    /// HTML parse_mode is used by the sender, so basic `<b>` tags are safe.
    /// Keep messages short — they appear as phone notifications.
    pub fn to_message(&self) -> String {
        match self {
            Self::StartupComplete { mode } => {
                // SECURITY: Do not expose internal service ports in Telegram.
                format!(
                    "<b>tickvault started</b>\nMode: {mode}\n\n\
                     Dashboards: Grafana / Prometheus / QuestDB available"
                )
            }
            Self::AuthenticationSuccess => "<b>Auth OK</b> — Dhan JWT acquired".to_string(),
            Self::AuthenticationFailed { reason } => {
                format!(
                    "<b>Auth FAILED</b> — offline mode\n{}",
                    redact_url_params(reason)
                )
            }
            Self::AuthenticationTransientFailure {
                attempt,
                reason,
                next_retry_ms,
            } => {
                // Render sub-second waits with ms precision so the operator
                // does not see the misleading "retrying in 0s" message that
                // `.as_secs()` produced on the 100ms→200ms→... early retries.
                let wait_str = if *next_retry_ms < 1_000 {
                    format!("{next_retry_ms}ms")
                } else {
                    format!("{:.1}s", (*next_retry_ms as f64) / 1_000.0)
                };
                format!(
                    "<b>Auth retry {attempt}</b> — transient\n{reason} — retrying in {wait_str}",
                    reason = redact_url_params(reason),
                )
            }
            Self::PreMarketProfileCheckFailed {
                reason,
                within_market_hours,
            } => {
                let header = if *within_market_hours {
                    "<b>CRITICAL: Pre-market profile check FAILED — BOOT HALTED</b>"
                } else {
                    "<b>CRITICAL: Pre-market profile check FAILED — investigate before 09:15 IST</b>"
                };
                format!(
                    "{header}\n{reason}\n\
                     Run:\n  curl -H \"access-token: $TOKEN\" https://api.dhan.co/v2/profile\n\
                     Check: dataPlan == \"Active\", activeSegment contains \"Derivative\", tokenValidity > 4h."
                )
            }
            Self::MidSessionProfileInvalidated { reason } => {
                format!(
                    "<b>CRITICAL: Mid-session profile INVALIDATED</b>\n{reason}\n\
                     Live WS still running — operator action required.\n\
                     Run:\n  curl -H \"access-token: $TOKEN\" https://api.dhan.co/v2/profile\n\
                     Check: dataPlan == \"Active\", activeSegment contains \"Derivative\", tokenValidity > 4h.\n\
                     If the profile is confirmed bad, restart the app so the boot-time HALT gate triggers \
                     (the no-tick watchdog will then page again if ticks stop)."
                )
            }
            Self::TokenRenewed => "<b>Token renewed</b>".to_string(),
            Self::TokenRenewalFailed { attempts, reason } => {
                format!(
                    "<b>Token renewal FAILED</b> (attempt {attempts})\n{}",
                    redact_url_params(reason)
                )
            }
            Self::WebSocketConnected { connection_index } => {
                // UX fix 2026-04-17: display as 1-indexed (WebSocket 1/5)
                // so operators read a natural 1..N range instead of 0..N-1.
                // Internal `connection_index` stays 0-based (array index).
                let display = connection_index.saturating_add(1);
                let total = tickvault_common::constants::MAX_WEBSOCKET_CONNECTIONS;
                format!("<b>WebSocket {display}/{total} connected</b>")
            }
            Self::WebSocketPoolOnline { connected, total } => {
                format!("<b>WS pool online:</b> {connected}/{total} connected")
            }
            Self::WebSocketPoolDegraded { down_secs } => {
                format!(
                    "<b>WS POOL DEGRADED</b>\nAll connections down for {down_secs}s. \
                     Investigate immediately."
                )
            }
            Self::WebSocketPoolRecovered { was_down_secs } => {
                format!(
                    "<b>WS pool recovered</b>\nAll connections back up (was down {was_down_secs}s)."
                )
            }
            Self::WebSocketPoolHalt { down_secs } => {
                format!(
                    "<b>WS POOL HALT</b>\nAll connections down for {down_secs}s. \
                     Exiting process so the supervisor restarts us."
                )
            }
            Self::MarketOpenStreamingConfirmation {
                main_feed_active,
                main_feed_total,
                depth_20_active,
                depth_200_active,
                order_update_active,
            } => {
                let oms = if *order_update_active { "1/1" } else { "0/1" };
                format!(
                    "<b>Streaming live @ 09:15:30 IST</b>\n\
                     Main feed: {main_feed_active}/{main_feed_total}\n\
                     Depth-20: {depth_20_active}/4\n\
                     Depth-200: {depth_200_active}/4\n\
                     Order updates: {oms}"
                )
            }
            Self::MarketOpenStreamingFailed {
                main_feed_active,
                main_feed_total,
                depth_20_active,
                depth_200_active,
                order_update_active,
            } => {
                let oms = if *order_update_active { "1/1" } else { "0/1" };
                format!(
                    "<b>MARKET OPEN STREAMING FAILED @ 09:15:30 IST</b>\n\
                     Main feed: {main_feed_active}/{main_feed_total} — NO CONNECTIONS\n\
                     Depth-20: {depth_20_active}/4\n\
                     Depth-200: {depth_200_active}/4\n\
                     Order updates: {oms}\n\
                     Action: check pool watchdog, token validity, Dhan status."
                )
            }
            Self::MarketOpenDepthAnchor {
                underlying,
                close_0912,
                atm_strike,
                source_minute_slot,
            } => {
                let close_str = close_0912
                    .map(|p| format!("{p:.2}"))
                    .unwrap_or_else(|| "N/A".to_string());
                let atm_str = atm_strike
                    .map(|s| format!("{s:.2}"))
                    .unwrap_or_else(|| "N/A".to_string());
                let source_str = match source_minute_slot {
                    Some(4) => "09:12 close",
                    Some(3) => "09:11 close (backtrack)",
                    Some(2) => "09:10 close (backtrack)",
                    Some(1) => "09:09 close (backtrack)",
                    Some(0) => "09:08 close (backtrack)",
                    _ => "no preopen tick",
                };
                format!(
                    "<b>Depth anchor: {underlying} @ 09:13 IST</b>\n\
                     Close: {close_str}\n\
                     ATM strike: {atm_str}\n\
                     Source: {source_str}"
                )
            }
            Self::DepthIndexLtpTimeout { waited_secs } => {
                format!(
                    "<b>Depth ATM timeout</b>\nWaited {waited_secs}s for index LTPs \
                     — proceeding with partial set. See DepthUnderlyingMissing \
                     alerts for the specific symbols that were dropped."
                )
            }
            Self::DepthUnderlyingMissing { underlying, reason } => {
                format!(
                    "<b>Depth underlying missing</b>\nUnderlying: {underlying}\n\
                     Reason: {reason}\nDepth connections for this symbol were NOT \
                     spawned this boot — feed will have no order-book depth for \
                     {underlying} until the next restart."
                )
            }
            Self::DepthSpotPriceStale {
                underlying,
                age_secs,
            } => {
                format!(
                    "<b>Depth spot price STALE</b>\nUnderlying: {underlying}\n\
                     Age: {age_secs}s (threshold 180s). Depth rebalance skipped — \
                     main-feed LTP feed may be stalled for this symbol."
                )
            }
            Self::Phase2Started { minutes_late } => {
                if *minutes_late == 0 {
                    "<b>Phase 2 started</b>\nSubscribing stock F&O (09:12 IST trigger).".to_string()
                } else {
                    format!(
                        "<b>Phase 2 started</b>\nSubscribing stock F&O — {minutes_late} min late \
                         (crash-recovery or late-start path)."
                    )
                }
            }
            Self::Phase2RunImmediate { minutes_late } => {
                format!(
                    "<b>Phase 2 RunImmediate</b>\n{minutes_late} min past 09:12 IST — \
                     running now to catch up after restart."
                )
            }
            Self::Phase2Complete {
                added_count,
                duration_ms,
                depth_20_underlyings,
                depth_200_contracts,
            } => {
                format!(
                    "<b>Phase 2 complete @ 09:12:30</b>\n\
                     Stock F&O: +{added_count}\n\
                     Depth-20: {depth_20_underlyings} underlyings\n\
                     Depth-200: {depth_200_contracts} contracts\n\
                     Duration: {duration_ms} ms"
                )
            }
            Self::Phase2Failed { reason, attempts } => {
                format!(
                    "<b>Phase 2 FAILED</b> after {attempts} attempts\n{reason}\n\
                     Stock F&O remains unsubscribed for this session — investigate main feed."
                )
            }
            Self::Phase2Skipped { reason } => {
                format!("<b>Phase 2 skipped</b>\n{reason}")
            }
            Self::MidMarketBootComplete {
                stocks_live_tick,
                stocks_rest,
                stocks_quest_db,
                stocks_skipped,
                total_subscribed,
                latency_ms,
            } => {
                format!(
                    "<b>Mid-market boot complete</b>\n\
                     Live-tick: {stocks_live_tick} | REST: {stocks_rest} | \
                     QuestDB: {stocks_quest_db} | Skipped: {stocks_skipped}\n\
                     Total subscribed: {total_subscribed}\n\
                     Boot latency: {latency_ms} ms"
                )
            }
            Self::WebSocketDisconnected {
                connection_index,
                reason,
            } => {
                format!(
                    "<b>WebSocket {}/{} disconnected</b>\n{reason}",
                    connection_index.saturating_add(1),
                    tickvault_common::constants::MAX_WEBSOCKET_CONNECTIONS
                )
            }
            Self::WebSocketDisconnectedOffHours {
                connection_index,
                reason,
            } => {
                format!(
                    "<b>WebSocket {}/{} disconnected [off-hours, auto-reconnecting]</b>\n{reason}",
                    connection_index.saturating_add(1),
                    tickvault_common::constants::MAX_WEBSOCKET_CONNECTIONS
                )
            }
            Self::WebSocketReconnected { connection_index } => {
                format!(
                    "<b>WebSocket {}/{} reconnected</b>",
                    connection_index.saturating_add(1),
                    tickvault_common::constants::MAX_WEBSOCKET_CONNECTIONS
                )
            }
            Self::WebSocketSleepEntered {
                feed,
                connection_index,
                sleep_secs,
            } => {
                format!(
                    "<b>WS-GAP-04 {feed} feed sleeping (slot {})</b>\nSleep for {sleep_secs}s until next NSE market open",
                    connection_index.saturating_add(1)
                )
            }
            Self::WebSocketSleepResumed {
                feed,
                connection_index,
                slept_for_secs,
            } => {
                format!(
                    "<b>WS-GAP-04 {feed} feed waking (slot {})</b>\nSlept {slept_for_secs}s — attempting reconnect",
                    connection_index.saturating_add(1)
                )
            }
            Self::WebSocketTokenForceRenewedOnWake {
                feed,
                connection_index,
                remaining_secs_before,
                threshold_secs,
            } => {
                format!(
                    "<b>AUTH-GAP-03 {feed} feed (slot {}) wake-time token renewed</b>\nRemaining before renewal: {remaining_secs_before}s (threshold {threshold_secs}s)",
                    connection_index.saturating_add(1)
                )
            }
            Self::DepthTwentyConnected { underlying } => {
                format!("<b>Depth 20-level connected</b>\nUnderlying: {underlying}")
            }
            Self::DepthTwentyDisconnected { underlying, reason } => {
                format!("<b>Depth 20-level DISCONNECTED</b>\nUnderlying: {underlying}\n{reason}")
            }
            Self::DepthTwentyDisconnectedOffHours { underlying, reason } => {
                format!(
                    "<b>Depth 20-level disconnected (off-hours / no-data)</b>\nUnderlying: {underlying}\n{reason}"
                )
            }
            Self::DepthTwentyReconnected { underlying } => {
                format!("<b>Depth 20-level reconnected</b>\nUnderlying: {underlying}")
            }
            Self::DepthTwoHundredConnected {
                contract,
                security_id,
            } => {
                format!(
                    "<b>Depth 200-level connected</b>\nContract: {contract}\nSecurityId: {security_id}"
                )
            }
            Self::DepthTwoHundredDisconnected {
                contract,
                security_id,
                reason,
            } => {
                format!(
                    "<b>Depth 200-level DISCONNECTED</b>\nContract: {contract}\nSecurityId: {security_id}\n{reason}"
                )
            }
            Self::DepthTwoHundredDisconnectedOffHours {
                contract,
                security_id,
                reason,
            } => {
                format!(
                    "<b>Depth 200-level disconnected (off-hours / no-data)</b>\nContract: {contract}\nSecurityId: {security_id}\n{reason}"
                )
            }
            Self::DepthTwoHundredReconnected {
                contract,
                security_id,
            } => {
                format!(
                    "<b>Depth 200-level reconnected</b>\nContract: {contract}\nSecurityId: {security_id}"
                )
            }
            Self::DepthRebalanced {
                underlying,
                previous_spot,
                current_spot,
                old_ce,
                old_pe,
                new_ce,
                new_pe,
                levels,
            } => {
                format!(
                    "<b>{title}: {underlying}</b>\n\
                     Spot: {previous_spot:.2} → {current_spot:.2}\n\
                     Old CE: {old_ce}\n\
                     Old PE: {old_pe}\n\
                     New CE: {new_ce}\n\
                     New PE: {new_pe}\n\
                     {action}",
                    title = format_args!("{} rebalance", levels.title_fragment()),
                    action = levels.action_line(),
                )
            }
            Self::DepthRebalanceFailed { underlying, reason } => {
                format!(
                    "<b>Depth rebalance FAILED: {underlying}</b>\n\
                     {reason}\n\
                     Depth subscription quality degraded until next successful rebalance."
                )
            }
            Self::OrderUpdateConnected => "<b>Order Update WS connected</b>".to_string(),
            Self::OrderUpdateAuthenticated => {
                "<b>Order Update WS authenticated</b>\nDhan accepted token — streaming live."
                    .to_string()
            }
            Self::OrderUpdateReconnected {
                consecutive_failures,
            } => {
                format!(
                    "<b>Order Update WS reconnected</b>\nRecovered after {consecutive_failures} consecutive failures"
                )
            }
            Self::NoLiveTicksDuringMarketHours {
                silent_for_secs,
                threshold_secs,
            } => {
                format!(
                    "<b>CRITICAL: zero live ticks during market hours</b>\n\
                     Silent for {silent_for_secs}s (threshold {threshold_secs}s).\n\
                     WebSockets may be connected but NO data streaming. \
                     Check Dhan dataPlan + IP allowlist + token validity."
                )
            }
            Self::OrderUpdateDisconnected { reason } => {
                format!("<b>Order Update WS DISCONNECTED</b>\n{reason}")
            }
            Self::InstrumentBuildSuccess {
                source,
                derivative_count,
                underlying_count,
            } => {
                format!(
                    "<b>Instruments OK</b>\nSource: {source}\nDerivatives: {derivative_count}\nUnderlyings: {underlying_count}"
                )
            }
            Self::InstrumentBuildFailed {
                reason,
                manual_trigger_url,
            } => {
                // SECURITY: Do not expose internal API URL in Telegram.
                let _ = manual_trigger_url; // kept in struct for internal use
                format!(
                    "<b>Instruments FAILED</b>\n{reason}\n\nRetry via /instruments/rebuild API endpoint"
                )
            }
            Self::HistoricalFetchComplete {
                instruments_fetched,
                instruments_skipped,
                total_candles,
                persist_failures,
            } => {
                let mut msg = format!(
                    "<b>Historical candles OK</b>\nFetched: {instruments_fetched}\nSkipped: {instruments_skipped}\nCandles: {total_candles}\nTimeframes: 1m, 5m, 15m, 60m, 1d"
                );
                if *persist_failures > 0 {
                    msg.push_str(&format!("\nPersist errors: {persist_failures}"));
                }
                msg
            }
            Self::HistoricalFetchAlreadyAvailable {
                today_ist,
                today_candles,
            } => {
                format!(
                    "<b>Historical candles already fetched</b>\n\
                     Date: {today_ist} IST\n\
                     Today's candles in DB: {today_candles}\n\
                     No new fetch needed (idempotent re-run)"
                )
            }
            Self::HistoricalFetchFailed {
                instruments_fetched,
                instruments_failed,
                total_candles,
                persist_failures,
                failed_instruments,
                failure_reasons,
            } => {
                let mut msg = format!(
                    "<b>Historical candle fetch — partial failure</b>\nFetched: {instruments_fetched}\nFailed: {instruments_failed}\nCandles: {total_candles}"
                );
                if *persist_failures > 0 {
                    msg.push_str(&format!("\nPersist errors: {persist_failures}"));
                }
                if !failure_reasons.is_empty() {
                    msg.push_str("\n\n<b>Failure breakdown:</b>");
                    for (reason, count) in failure_reasons {
                        msg.push_str(&format!("\n\u{2022} {reason}: {count}"));
                    }
                }
                if !failed_instruments.is_empty() {
                    msg.push_str("\n\n<b>Failed instruments:</b>");
                    let show_count = failed_instruments.len().min(10);
                    for name in &failed_instruments[..show_count] {
                        msg.push_str(&format!("\n\u{2022} {name}"));
                    }
                    if failed_instruments.len() > 10 {
                        let remaining = failed_instruments.len().saturating_sub(10);
                        msg.push_str(&format!("\n... +{remaining} more"));
                    }
                }
                msg
            }
            Self::CandleVerificationPassed {
                instruments_checked,
                total_candles,
                timeframe_details,
                ohlc_violations,
                data_violations,
                timestamp_violations,
                weekend_violations,
            } => {
                let mut msg = format!(
                    "<b>Candle verification OK</b>\nInstruments: {instruments_checked}\nTotal candles: {total_candles}"
                );
                if !timeframe_details.is_empty() {
                    msg.push_str("\n\n<b>Timeframes:</b>\n");
                    msg.push_str(timeframe_details);
                }
                if *ohlc_violations == 0
                    && *data_violations == 0
                    && *timestamp_violations == 0
                    && *weekend_violations == 0
                {
                    msg.push_str("\n\nChecks: OHLC \u{2713} | Data \u{2713} | Timestamps \u{2713} | Weekends \u{2713}");
                } else {
                    if *ohlc_violations > 0 {
                        msg.push_str(&format!("\nOHLC violations: {ohlc_violations}"));
                    }
                    if *data_violations > 0 {
                        msg.push_str(&format!(
                            "\nData violations: {data_violations} (non-blocking)"
                        ));
                    }
                    if *timestamp_violations > 0 {
                        msg.push_str(&format!(
                            "\nTimestamp violations: {timestamp_violations} (non-blocking)"
                        ));
                    }
                    if *weekend_violations > 0 {
                        msg.push_str(&format!(
                            "\nWeekend violations: {weekend_violations} (CRITICAL)"
                        ));
                    }
                }
                msg
            }
            Self::CandleVerificationFailed {
                instruments_checked,
                instruments_with_gaps,
                timeframe_details,
                ohlc_violations,
                data_violations,
                timestamp_violations,
                ohlc_details,
                data_details,
                timestamp_details,
                weekend_violations,
                weekend_details,
            } => {
                let mut msg = if *instruments_checked == 0 {
                    "<b>Candle verification FAILED</b>\nChecked: 0\n\nNo instrument data found \u{2014} fetch may have completely failed".to_string()
                } else {
                    format!(
                        "<b>Candle verification FAILED</b>\nChecked: {instruments_checked} | Gaps: {instruments_with_gaps}"
                    )
                };

                // OHLC violations with details
                if *ohlc_violations > 0 {
                    msg.push_str(&format!("\n\n<b>OHLC violations ({ohlc_violations}):</b>"));
                    append_detail_lines(&mut msg, ohlc_details, *ohlc_violations);
                }

                // Data violations with details
                if *data_violations > 0 {
                    msg.push_str(&format!("\n\n<b>Data violations ({data_violations}):</b>"));
                    append_detail_lines(&mut msg, data_details, *data_violations);
                }

                // Timestamp violations with details
                if *timestamp_violations > 0 {
                    msg.push_str(&format!(
                        "\n\n<b>Timestamp violations ({timestamp_violations}):</b>"
                    ));
                    append_detail_lines(&mut msg, timestamp_details, *timestamp_violations);
                }

                // Weekend violations with details (CRITICAL — NSE closed on Sat/Sun)
                if *weekend_violations > 0 {
                    msg.push_str(&format!(
                        "\n\n<b>WEEKEND violations ({weekend_violations}) — CRITICAL:</b>"
                    ));
                    append_detail_lines(&mut msg, weekend_details, *weekend_violations);
                }

                if *instruments_checked > 0 && !timeframe_details.is_empty() {
                    msg.push_str("\n\n<b>Timeframes:</b>\n");
                    msg.push_str(timeframe_details);
                }
                msg
            }
            Self::CandleCrossMatchPassed {
                timeframes_checked,
                candles_compared,
                today_ist_label,
                scope_indices,
                scope_equities,
                per_tf_cells,
            } => render_cross_match_ok(
                *timeframes_checked,
                *candles_compared,
                today_ist_label,
                *scope_indices,
                *scope_equities,
                per_tf_cells,
            ),
            Self::CandleCrossMatchFailed {
                candles_compared,
                mismatches,
                missing_live,
                mismatch_details,
                today_ist_label,
                scope_indices,
                scope_equities,
                missing_historical,
                value_mismatches,
                per_tf_gaps,
            } => render_cross_match_failed(
                *candles_compared,
                *mismatches,
                *missing_live,
                *missing_historical,
                *value_mismatches,
                mismatch_details,
                today_ist_label,
                *scope_indices,
                *scope_equities,
                per_tf_gaps,
            ),
            Self::CandleCrossMatchSkipped {
                reason,
                candles_compared,
            } => {
                // Subtext varies by reason category — the previous hardcoded
                // "first run / fresh DB / post-market boot" line was misleading
                // for weekend, holiday, and pre-market skips. Match the lead-in
                // sentence to the reason so the operator understands WHY the
                // verification was skipped at a glance.
                let lower = reason.to_ascii_lowercase();
                let next_action = if lower.contains("weekend") || lower.contains("holiday") {
                    "Will compare on the next trading day's post-market run."
                } else if lower.contains("pre-market") {
                    "Will compare after market open + post-market historical re-fetch."
                } else {
                    "Will compare on next run after live ticks during market hours."
                };
                format!(
                    "<b>Historical vs Live cross-match SKIPPED</b>\nReason: {reason}\nCandles compared: {candles_compared}\n{next_action}"
                )
            }
            Self::IpVerificationFailed { reason } => {
                format!(
                    "<b>IP VERIFICATION FAILED</b>\n{reason}\n\nBoot blocked — no Dhan API calls will be made."
                )
            }
            Self::IpVerificationSuccess { verified_ip } => {
                // SECURITY: Mask the last two octets to prevent IP exposure
                // in Telegram messages. Show only network prefix for confirmation.
                let masked = mask_ip_for_notification(verified_ip);
                format!("<b>IP verified</b> — {masked}")
            }
            Self::BootHealthCheck {
                services_healthy,
                services_total,
            } => {
                format!("<b>Boot health check</b>\nHealthy: {services_healthy}/{services_total}")
            }
            Self::BootDeadlineMissed {
                deadline_secs,
                step,
            } => {
                format!(
                    "<b>BOOT DEADLINE MISSED</b>\nDeadline: {deadline_secs}s\nBlocked at: {step}"
                )
            }
            Self::BootClockSkewExceeded {
                skew_secs,
                threshold_secs,
                source,
            } => {
                format!(
                    "<b>BOOT-03 CLOCK SKEW EXCEEDED — HALTING</b>\n\
                     Source: {source}\n\
                     Skew: {skew_secs:+.3}s (threshold ±{threshold_secs:.2}s)\n\
                     IST timestamp math + DEDUP keys cannot be trusted.\n\
                     Run on host: `chronyc tracking` then `chronyc -a makestep`.\n\
                     Restart the app once `Last offset` < {threshold_secs:.2}s."
                )
            }
            Self::ShutdownInitiated => "<b>Shutdown initiated</b>".to_string(),
            Self::ShutdownComplete => "<b>tickvault stopped</b>".to_string(),
            Self::SelfTestPassed { checks_passed } => {
                format!(
                    "<b>Market-open self-test PASSED @ 09:16 IST</b>\n\
                     {checks_passed}/{checks_passed} sub-checks green.\n\
                     Code: SELFTEST-01"
                )
            }
            Self::SelfTestDegraded {
                checks_passed,
                checks_failed,
                failed,
            } => {
                let total = checks_passed + checks_failed;
                let failed_list = failed.join(", ");
                format!(
                    "<b>Market-open self-test DEGRADED @ 09:16 IST</b>\n\
                     {checks_passed}/{total} sub-checks green; {checks_failed} failed.\n\
                     Failed: {failed_list}\n\
                     Code: SELFTEST-02\n\
                     Action: investigate the listed sub-checks; trading may continue."
                )
            }
            Self::SelfTestCritical {
                checks_failed,
                failed,
            } => {
                let failed_list = failed.join(", ");
                format!(
                    "<b>MARKET-OPEN SELF-TEST CRITICAL @ 09:16 IST</b>\n\
                     {checks_failed} sub-check(s) failed (≥1 critical).\n\
                     Failed: {failed_list}\n\
                     Code: SELFTEST-02\n\
                     Action: see runbook .claude/rules/project/wave-3-c-error-codes.md"
                )
            }
            Self::OrderRejected {
                correlation_id,
                reason,
            } => {
                format!("<b>Order REJECTED</b>\nCorrelation: {correlation_id}\n{reason}")
            }
            Self::CircuitBreakerOpened {
                consecutive_failures,
            } => {
                format!(
                    "<b>Circuit breaker OPENED</b>\nConsecutive failures: {consecutive_failures}\nOrder API calls halted"
                )
            }
            Self::CircuitBreakerClosed => {
                "<b>Circuit breaker CLOSED</b>\nOrder API calls resumed".to_string()
            }
            Self::RateLimitExhausted { limit_type } => {
                format!("<b>Rate limit EXHAUSTED</b>\nLimit: {limit_type}")
            }
            Self::RiskHalt { reason } => {
                format!("<b>RISK HALT</b>\nTrading stopped: {reason}")
            }
            Self::WebSocketReconnectionExhausted {
                connection_index,
                attempts,
            } => {
                format!(
                    "<b>WebSocket {}/{} RECONNECTION EXHAUSTED</b>\nAttempts: {attempts}\nNo market data",
                    connection_index.saturating_add(1),
                    tickvault_common::constants::MAX_WEBSOCKET_CONNECTIONS
                )
            }
            Self::TokenRenewalDeadlineMissed { deadline_hour_ist } => {
                format!(
                    "<b>TOKEN RENEWAL DEADLINE MISSED</b>\nPast {deadline_hour_ist}:00 IST — token not renewed"
                )
            }
            Self::QuestDbDisconnected {
                writer,
                signal,
                signal_kind,
            } => {
                // The numeric `signal` is factual at alert time — either a
                // consecutive-failure count (liveness-check path) or a dropped
                // record count (tick-writer lag path). `signal_kind` names
                // which one so operators can read the alert without guessing.
                // No hardcoded zero, no fake auto-reconnect cadence.
                format!(
                    "<b>CRITICAL: QuestDB {writer} DEGRADED</b>\n\
                     {signal_kind}: {signal}.\n\
                     Ticks remain durable via WAL — no data loss.\n\
                     Investigate QuestDB container + disk + CPU."
                )
            }
            Self::QuestDbReconnected {
                writer,
                drained_count,
            } => {
                format!(
                    "<b>QuestDB {writer} RECONNECTED</b>\n\
                     Drained {drained_count} buffered records to QuestDB."
                )
            }
            Self::Custom { message } => message.clone(),
        }
    }

    /// Returns a stable `&'static str` topic name used by the Telegram
    /// bucket-coalescer (Wave 3-B Item 11) to group bursts of identical
    /// events into a single summary message.
    ///
    /// The string returned is the variant's PascalCase name. This is
    /// `&'static str` so the bucket key is allocation-free.
    ///
    /// Tested by `test_topic_returns_static_str_for_each_variant_kind`.
    #[must_use]
    pub const fn topic(&self) -> &'static str {
        match self {
            Self::StartupComplete { .. } => "StartupComplete",
            Self::AuthenticationSuccess => "AuthenticationSuccess",
            Self::AuthenticationFailed { .. } => "AuthenticationFailed",
            Self::AuthenticationTransientFailure { .. } => "AuthenticationTransientFailure",
            Self::PreMarketProfileCheckFailed { .. } => "PreMarketProfileCheckFailed",
            Self::MidSessionProfileInvalidated { .. } => "MidSessionProfileInvalidated",
            Self::TokenRenewed => "TokenRenewed",
            Self::TokenRenewalFailed { .. } => "TokenRenewalFailed",
            Self::WebSocketConnected { .. } => "WebSocketConnected",
            Self::WebSocketPoolOnline { .. } => "WebSocketPoolOnline",
            Self::WebSocketPoolDegraded { .. } => "WebSocketPoolDegraded",
            Self::WebSocketPoolRecovered { .. } => "WebSocketPoolRecovered",
            Self::WebSocketPoolHalt { .. } => "WebSocketPoolHalt",
            Self::DepthIndexLtpTimeout { .. } => "DepthIndexLtpTimeout",
            Self::MarketOpenStreamingConfirmation { .. } => "MarketOpenStreamingConfirmation",
            Self::MarketOpenStreamingFailed { .. } => "MarketOpenStreamingFailed",
            Self::MarketOpenDepthAnchor { .. } => "MarketOpenDepthAnchor",
            Self::DepthUnderlyingMissing { .. } => "DepthUnderlyingMissing",
            Self::DepthSpotPriceStale { .. } => "DepthSpotPriceStale",
            Self::Phase2Started { .. } => "Phase2Started",
            Self::Phase2RunImmediate { .. } => "Phase2RunImmediate",
            Self::Phase2Complete { .. } => "Phase2Complete",
            Self::Phase2Failed { .. } => "Phase2Failed",
            Self::Phase2Skipped { .. } => "Phase2Skipped",
            Self::MidMarketBootComplete { .. } => "MidMarketBootComplete",
            Self::WebSocketDisconnected { .. } => "WebSocketDisconnected",
            Self::WebSocketDisconnectedOffHours { .. } => "WebSocketDisconnectedOffHours",
            Self::WebSocketReconnected { .. } => "WebSocketReconnected",
            Self::WebSocketSleepEntered { .. } => "WebSocketSleepEntered",
            Self::WebSocketSleepResumed { .. } => "WebSocketSleepResumed",
            Self::WebSocketTokenForceRenewedOnWake { .. } => "WebSocketTokenForceRenewedOnWake",
            Self::DepthTwentyConnected { .. } => "DepthTwentyConnected",
            Self::DepthTwentyDisconnected { .. } => "DepthTwentyDisconnected",
            Self::DepthTwentyDisconnectedOffHours { .. } => "DepthTwentyDisconnectedOffHours",
            Self::DepthTwentyReconnected { .. } => "DepthTwentyReconnected",
            Self::DepthTwoHundredConnected { .. } => "DepthTwoHundredConnected",
            Self::DepthTwoHundredDisconnected { .. } => "DepthTwoHundredDisconnected",
            Self::DepthTwoHundredDisconnectedOffHours { .. } => {
                "DepthTwoHundredDisconnectedOffHours"
            }
            Self::DepthTwoHundredReconnected { .. } => "DepthTwoHundredReconnected",
            Self::DepthRebalanced { .. } => "DepthRebalanced",
            Self::DepthRebalanceFailed { .. } => "DepthRebalanceFailed",
            Self::OrderUpdateConnected => "OrderUpdateConnected",
            Self::OrderUpdateAuthenticated => "OrderUpdateAuthenticated",
            Self::OrderUpdateDisconnected { .. } => "OrderUpdateDisconnected",
            Self::OrderUpdateReconnected { .. } => "OrderUpdateReconnected",
            Self::NoLiveTicksDuringMarketHours { .. } => "NoLiveTicksDuringMarketHours",
            Self::ShutdownInitiated => "ShutdownInitiated",
            Self::ShutdownComplete => "ShutdownComplete",
            Self::InstrumentBuildSuccess { .. } => "InstrumentBuildSuccess",
            Self::InstrumentBuildFailed { .. } => "InstrumentBuildFailed",
            Self::HistoricalFetchComplete { .. } => "HistoricalFetchComplete",
            Self::HistoricalFetchAlreadyAvailable { .. } => "HistoricalFetchAlreadyAvailable",
            Self::HistoricalFetchFailed { .. } => "HistoricalFetchFailed",
            Self::CandleVerificationPassed { .. } => "CandleVerificationPassed",
            Self::CandleVerificationFailed { .. } => "CandleVerificationFailed",
            Self::CandleCrossMatchPassed { .. } => "CandleCrossMatchPassed",
            Self::CandleCrossMatchFailed { .. } => "CandleCrossMatchFailed",
            Self::CandleCrossMatchSkipped { .. } => "CandleCrossMatchSkipped",
            Self::IpVerificationFailed { .. } => "IpVerificationFailed",
            Self::IpVerificationSuccess { .. } => "IpVerificationSuccess",
            Self::BootHealthCheck { .. } => "BootHealthCheck",
            Self::BootDeadlineMissed { .. } => "BootDeadlineMissed",
            Self::BootClockSkewExceeded { .. } => "BootClockSkewExceeded",
            Self::OrderRejected { .. } => "OrderRejected",
            Self::CircuitBreakerOpened { .. } => "CircuitBreakerOpened",
            Self::CircuitBreakerClosed => "CircuitBreakerClosed",
            Self::RateLimitExhausted { .. } => "RateLimitExhausted",
            Self::RiskHalt { .. } => "RiskHalt",
            Self::WebSocketReconnectionExhausted { .. } => "WebSocketReconnectionExhausted",
            Self::TokenRenewalDeadlineMissed { .. } => "TokenRenewalDeadlineMissed",
            Self::QuestDbDisconnected { .. } => "QuestDbDisconnected",
            Self::QuestDbReconnected { .. } => "QuestDbReconnected",
            Self::Custom { .. } => "Custom",
        }
    }

    /// Returns the severity level for this event.
    ///
    /// Severity drives channel selection in `NotificationService::notify`:
    /// - `Critical` / `High` → Telegram + SNS SMS
    /// - `Medium` / `Low` / `Info` → Telegram only
    pub fn severity(&self) -> Severity {
        match self {
            Self::IpVerificationFailed { .. } => Severity::Critical,
            Self::BootDeadlineMissed { .. } => Severity::Critical,
            Self::BootClockSkewExceeded { .. } => Severity::Critical,
            Self::AuthenticationFailed { .. } => Severity::Critical,
            // Transient auth blip — Low so no SMS escalation. If all retries
            // exhaust, the terminal path fires `AuthenticationFailed` at
            // Critical instead.
            Self::AuthenticationTransientFailure { .. } => Severity::Low,
            Self::PreMarketProfileCheckFailed { .. } => Severity::Critical,
            Self::MidSessionProfileInvalidated { .. } => Severity::Critical,
            Self::TokenRenewalFailed { .. } => Severity::Critical,
            Self::InstrumentBuildFailed { .. } => Severity::High,
            Self::WebSocketDisconnected { .. } => Severity::High,
            Self::WebSocketDisconnectedOffHours { .. } => Severity::Low,
            Self::HistoricalFetchFailed { .. } => Severity::High,
            Self::CandleVerificationFailed { .. } => Severity::High,
            Self::CandleCrossMatchFailed { .. } => Severity::High,
            Self::HistoricalFetchComplete { .. } => Severity::Low,
            Self::HistoricalFetchAlreadyAvailable { .. } => Severity::Low,
            Self::CandleVerificationPassed { .. } => Severity::Low,
            Self::CandleCrossMatchPassed { .. } => Severity::Low,
            Self::CandleCrossMatchSkipped { .. } => Severity::Low,
            Self::Custom { .. } => Severity::High,
            Self::RiskHalt { .. } => Severity::Critical,
            Self::WebSocketReconnectionExhausted { .. } => Severity::Critical,
            Self::TokenRenewalDeadlineMissed { .. } => Severity::Critical,
            Self::CircuitBreakerOpened { .. } => Severity::High,
            Self::OrderRejected { .. } => Severity::High,
            Self::RateLimitExhausted { .. } => Severity::High,
            Self::QuestDbDisconnected { .. } => Severity::Critical,
            Self::QuestDbReconnected { .. } => Severity::Medium,
            Self::WebSocketReconnected { .. } => Severity::Medium,
            Self::WebSocketSleepEntered { .. } | Self::WebSocketSleepResumed { .. } => {
                Severity::Low
            }
            Self::WebSocketTokenForceRenewedOnWake { .. } => Severity::Low,
            Self::DepthTwentyDisconnected { .. } => Severity::High,
            Self::DepthTwentyDisconnectedOffHours { .. } => Severity::Low,
            Self::DepthTwentyReconnected { .. } => Severity::Medium,
            Self::DepthTwoHundredDisconnected { .. } => Severity::High,
            Self::DepthTwoHundredDisconnectedOffHours { .. } => Severity::Low,
            Self::DepthTwoHundredReconnected { .. } => Severity::Medium,
            // Routine zero-disconnect drift swap — green by design. Prior
            // `Custom` routing made every 60s drift fire [HIGH] amber; see
            // Fix #9 in .claude/plans/active-plan.md and .claude/rules/project/
            // depth-subscription.md for the working-as-designed rationale.
            Self::DepthRebalanced { .. } => Severity::Low,
            // Swap itself failed — depth quality degraded until next rebalance.
            Self::DepthRebalanceFailed { .. } => Severity::High,
            Self::OrderUpdateDisconnected { .. } => Severity::High,
            Self::OrderUpdateReconnected { .. } => Severity::Low,
            Self::NoLiveTicksDuringMarketHours { .. } => Severity::Critical,
            Self::ShutdownInitiated => Severity::Medium,
            Self::CircuitBreakerClosed => Severity::Medium,
            Self::WebSocketConnected { .. } => Severity::Low,
            Self::WebSocketPoolOnline { .. } => Severity::Medium,
            Self::WebSocketPoolDegraded { .. } => Severity::High,
            Self::WebSocketPoolRecovered { .. } => Severity::Medium,
            Self::WebSocketPoolHalt { .. } => Severity::High,
            Self::MarketOpenStreamingConfirmation { .. } => Severity::Info,
            Self::MarketOpenStreamingFailed { .. } => Severity::High,
            Self::MarketOpenDepthAnchor { .. } => Severity::Info,
            Self::SelfTestPassed { .. } => Severity::Info,
            Self::SelfTestDegraded { .. } => Severity::High,
            Self::SelfTestCritical { .. } => Severity::Critical,
            Self::DepthIndexLtpTimeout { .. } => Severity::High,
            Self::DepthUnderlyingMissing { .. } => Severity::High,
            Self::DepthSpotPriceStale { .. } => Severity::High,
            Self::Phase2Started { .. } => Severity::Medium,
            Self::Phase2RunImmediate { .. } => Severity::Medium,
            Self::Phase2Complete { .. } => Severity::Medium,
            Self::Phase2Failed { .. } => Severity::High,
            Self::Phase2Skipped { .. } => Severity::Low,
            Self::MidMarketBootComplete { .. } => Severity::Info,
            Self::DepthTwentyConnected { .. } => Severity::Low,
            Self::DepthTwoHundredConnected { .. } => Severity::Low,
            Self::OrderUpdateConnected => Severity::Low,
            Self::OrderUpdateAuthenticated => Severity::Medium,
            Self::TokenRenewed => Severity::Low,
            Self::IpVerificationSuccess { .. } => Severity::Low,
            Self::AuthenticationSuccess => Severity::Low,
            Self::InstrumentBuildSuccess { .. } => Severity::Low,
            Self::BootHealthCheck { .. } => Severity::Low,
            Self::StartupComplete { .. } => Severity::Info,
            Self::ShutdownComplete => Severity::Info,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Appends violation detail lines to a message, with "+N more" truncation.
fn append_detail_lines(msg: &mut String, details: &[String], total_count: usize) {
    let show_count = details.len().min(10);
    for line in &details[..show_count] {
        msg.push_str(&format!("\n{line}"));
    }
    if total_count > show_count {
        let remaining = total_count.saturating_sub(show_count);
        msg.push_str(&format!("\n... +{remaining} more"));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_severity_as_label_returns_lowercase_string() {
        assert_eq!(Severity::Info.as_label(), "info");
        assert_eq!(Severity::Low.as_label(), "low");
        assert_eq!(Severity::Medium.as_label(), "medium");
        assert_eq!(Severity::High.as_label(), "high");
        assert_eq!(Severity::Critical.as_label(), "critical");
    }

    #[test]
    fn test_topic_returns_static_str_for_each_variant_kind() {
        // Spot-check across all severities + several event kinds. The
        // returned string is a `&'static str` literal — Rust will not
        // compile if any arm allocates. We additionally assert two
        // variants return distinct topics so a lazy implementation
        // (e.g. always returning "Event") cannot pass.
        let cases: Vec<(NotificationEvent, &'static str)> = vec![
            (
                NotificationEvent::StartupComplete { mode: "LIVE" },
                "StartupComplete",
            ),
            (
                NotificationEvent::AuthenticationSuccess,
                "AuthenticationSuccess",
            ),
            (NotificationEvent::TokenRenewed, "TokenRenewed"),
            (
                NotificationEvent::WebSocketDisconnectedOffHours {
                    connection_index: 0,
                    reason: "x".into(),
                },
                "WebSocketDisconnectedOffHours",
            ),
            (
                NotificationEvent::DepthRebalanced {
                    underlying: "NIFTY".into(),
                    previous_spot: 25_000.0,
                    current_spot: 25_050.0,
                    old_ce: "old_ce".into(),
                    old_pe: "old_pe".into(),
                    new_ce: "new_ce".into(),
                    new_pe: "new_pe".into(),
                    levels: DepthRebalanceLevels::TwentyOnly,
                },
                "DepthRebalanced",
            ),
            (
                NotificationEvent::RiskHalt {
                    reason: "limit breach".into(),
                },
                "RiskHalt",
            ),
            (NotificationEvent::ShutdownComplete, "ShutdownComplete"),
        ];
        let mut seen: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
        for (event, expected) in cases {
            let actual = event.topic();
            assert_eq!(actual, expected, "topic() mismatch for variant");
            seen.insert(actual);
        }
        // At least 6 distinct topics observed.
        assert!(
            seen.len() >= 6,
            "expected ≥6 distinct topics, got {}",
            seen.len()
        );
    }

    #[test]
    fn test_startup_complete_live_message() {
        let event = NotificationEvent::StartupComplete { mode: "LIVE" };
        let msg = event.to_message();
        assert!(msg.contains("LIVE"));
        assert!(msg.contains("started"));
        // SECURITY: ports and URLs must NOT appear in Telegram message
        assert!(!msg.contains("3000"), "internal port leaked: {msg}");
        assert!(!msg.contains("9090"), "internal port leaked: {msg}");
        assert!(!msg.contains("16686"), "internal port leaked: {msg}");
        assert!(!msg.contains("9000"), "internal port leaked: {msg}");
        assert!(!msg.contains("localhost"), "localhost leaked: {msg}");
        assert!(msg.contains("Dashboards"));
        assert!(msg.contains("Grafana"));
        assert!(msg.contains("Prometheus"));
        assert!(msg.contains("QuestDB"));
    }

    #[test]
    fn test_startup_complete_offline_message() {
        let event = NotificationEvent::StartupComplete { mode: "OFFLINE" };
        let msg = event.to_message();
        assert!(msg.contains("OFFLINE"));
        assert!(msg.contains("Grafana"));
    }

    #[test]
    fn test_auth_success_message() {
        let event = NotificationEvent::AuthenticationSuccess;
        let msg = event.to_message();
        assert!(msg.contains("Auth OK"));
    }

    // Q2 regression pins (2026-04-23): transient auth blips must be Low,
    // not Critical — a 100ms network hiccup on `auth.dhan.co` that the
    // very next retry resolves should NOT fire a `CRITICAL` Telegram.

    #[test]
    fn test_auth_transient_failure_is_low_severity() {
        let event = NotificationEvent::AuthenticationTransientFailure {
            attempt: 1,
            reason: "error sending request".to_string(),
            next_retry_ms: 100,
        };
        assert_eq!(
            event.severity(),
            Severity::Low,
            "transient auth blips must be Low so they do not SMS-escalate — \
             only the terminal AuthenticationFailed is Critical"
        );
    }

    #[test]
    fn test_auth_transient_failure_shows_ms_not_zero_seconds() {
        // Before Q2: `format!("retrying in {}s", wait.as_secs())` rendered
        // 100ms as "retrying in 0s" which was misleading. Now sub-second
        // waits render with ms precision.
        let event = NotificationEvent::AuthenticationTransientFailure {
            attempt: 1,
            reason: "error sending request".to_string(),
            next_retry_ms: 100,
        };
        let msg = event.to_message();
        assert!(msg.contains("100ms"), "expected 100ms in: {msg}");
        assert!(
            !msg.contains(" 0s"),
            "must NOT render sub-second waits as '0s': {msg}"
        );
    }

    #[test]
    fn test_auth_transient_failure_shows_seconds_when_ge_1s() {
        let event = NotificationEvent::AuthenticationTransientFailure {
            attempt: 5,
            reason: "dns hiccup".to_string(),
            next_retry_ms: 3_500,
        };
        let msg = event.to_message();
        assert!(msg.contains("3.5s"), "expected 3.5s in: {msg}");
    }

    #[test]
    fn test_auth_transient_failure_redacts_url_params() {
        let event = NotificationEvent::AuthenticationTransientFailure {
            attempt: 1,
            reason: "request failed for https://auth.dhan.co/app/generateAccessToken?token=secret"
                .to_string(),
            next_retry_ms: 100,
        };
        let msg = event.to_message();
        assert!(
            !msg.contains("token=secret"),
            "URL query params must be redacted: {msg}"
        );
    }

    // Q5 regression pins (2026-04-23): post-market depth disconnects fire
    // Low, not High. A boot at 18:05 IST with 4× [HIGH] depth-200 alerts
    // (all SecurityId=0, contracts labeled "*-deferred") was pure Telegram
    // noise because no live spot LTP arrived after 15:30 IST to populate
    // the ATM selector. These pins enforce the Low routing.

    #[test]
    fn test_depth_20_disconnected_off_hours_is_low() {
        let event = NotificationEvent::DepthTwentyDisconnectedOffHours {
            underlying: "NIFTY".to_string(),
            reason: "Reconnection failed after 20 attempts".to_string(),
        };
        assert_eq!(
            event.severity(),
            Severity::Low,
            "post-market depth-20 disconnect must be Low to avoid SMS escalation"
        );
    }

    #[test]
    fn test_depth_200_disconnected_off_hours_is_low() {
        let event = NotificationEvent::DepthTwoHundredDisconnectedOffHours {
            contract: "NIFTY-PE-deferred".to_string(),
            security_id: 0,
            reason: "Reconnection failed after 60 attempts".to_string(),
        };
        assert_eq!(
            event.severity(),
            Severity::Low,
            "post-market / SID=0 depth-200 disconnect must be Low"
        );
    }

    #[test]
    fn test_depth_20_disconnected_in_hours_still_high() {
        // Severity-flip guard: a real in-market-hours disconnect must
        // stay High so the operator gets SMS. If this ever flips to Low
        // by accident, we'd go silent on real outages.
        let event = NotificationEvent::DepthTwentyDisconnected {
            underlying: "NIFTY".to_string(),
            reason: "Dhan TCP reset".to_string(),
        };
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_depth_200_disconnected_in_hours_still_high() {
        let event = NotificationEvent::DepthTwoHundredDisconnected {
            contract: "NIFTY-Jun2026-25650-CE".to_string(),
            security_id: 72271,
            reason: "Dhan TCP reset".to_string(),
        };
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_depth_200_disconnected_off_hours_message_mentions_off_hours() {
        let event = NotificationEvent::DepthTwoHundredDisconnectedOffHours {
            contract: "BANKNIFTY-CE-deferred".to_string(),
            security_id: 0,
            reason: "no reason".to_string(),
        };
        let msg = event.to_message();
        assert!(
            msg.contains("off-hours") || msg.contains("no-data"),
            "message must distinguish off-hours variant: {msg}"
        );
        assert!(msg.contains("BANKNIFTY-CE-deferred"));
    }

    // -----------------------------------------------------------------
    // Fix #9 (2026-04-24): routine zero-disconnect depth rebalance is
    // `Severity::Low`, not `Severity::High`. Ratchet against regression
    // back to the old `Custom { message: … }` path.
    // -----------------------------------------------------------------

    #[test]
    fn test_depth_rebalance_success_is_low_severity() {
        let event = NotificationEvent::DepthRebalanced {
            underlying: "BANKNIFTY".to_string(),
            previous_spot: 55000.0,
            current_spot: 55150.0,
            old_ce: "BANKNIFTY 24 APR 55000 CALL (SID 11111)".to_string(),
            old_pe: "BANKNIFTY 24 APR 55000 PUT (SID 22222)".to_string(),
            new_ce: "BANKNIFTY 24 APR 55150 CALL (SID 33333)".to_string(),
            new_pe: "BANKNIFTY 24 APR 55150 PUT (SID 44444)".to_string(),
            levels: DepthRebalanceLevels::TwentyAndTwoHundred,
        };
        assert_eq!(
            event.severity(),
            Severity::Low,
            "routine drift swap MUST NOT escalate to High — see \
             .claude/rules/project/depth-subscription.md"
        );
    }

    #[test]
    fn test_depth_rebalance_failure_is_high_severity() {
        // Severity-flip guard: failures MUST stay High so the operator
        // gets SMS when depth quality degrades.
        let event = NotificationEvent::DepthRebalanceFailed {
            underlying: "NIFTY".to_string(),
            reason: "command channel closed".to_string(),
        };
        assert_eq!(event.severity(), Severity::High);
    }

    // -----------------------------------------------------------------
    // Fix #10 (2026-04-24): title fragment includes the level(s).
    // `Depth-20 rebalance: …` for FINNIFTY / MIDCPNIFTY,
    // `Depth-20+200 rebalance: …` for NIFTY / BANKNIFTY.
    // -----------------------------------------------------------------

    #[test]
    fn test_depth_rebalance_title_20_only() {
        let event = NotificationEvent::DepthRebalanced {
            underlying: "FINNIFTY".to_string(),
            previous_spot: 23500.0,
            current_spot: 23610.0,
            old_ce: "FINNIFTY 29 APR 23500 CALL (SID 71111)".to_string(),
            old_pe: "FINNIFTY 29 APR 23500 PUT (SID 71112)".to_string(),
            new_ce: "FINNIFTY 29 APR 23600 CALL (SID 71121)".to_string(),
            new_pe: "FINNIFTY 29 APR 23600 PUT (SID 71122)".to_string(),
            levels: DepthRebalanceLevels::TwentyOnly,
        };
        let msg = event.to_message();
        assert!(
            msg.contains("<b>Depth-20 rebalance: FINNIFTY</b>"),
            "FINNIFTY (no 200-level) title must read `Depth-20 rebalance: …`: {msg}"
        );
        assert!(
            !msg.contains("Depth-20+200"),
            "FINNIFTY must NOT claim 200-level in title: {msg}"
        );
        assert!(msg.contains("no 200-level for this underlying"));
    }

    #[test]
    fn test_depth_rebalance_title_20_plus_200() {
        let event = NotificationEvent::DepthRebalanced {
            underlying: "NIFTY".to_string(),
            previous_spot: 23800.0,
            current_spot: 23710.0,
            old_ce: "NIFTY 24 APR 23800 CALL (SID 62001)".to_string(),
            old_pe: "NIFTY 24 APR 23800 PUT (SID 62002)".to_string(),
            new_ce: "NIFTY 24 APR 23700 CALL (SID 62011)".to_string(),
            new_pe: "NIFTY 24 APR 23700 PUT (SID 62012)".to_string(),
            levels: DepthRebalanceLevels::TwentyAndTwoHundred,
        };
        let msg = event.to_message();
        assert!(
            msg.contains("<b>Depth-20+200 rebalance: NIFTY</b>"),
            "NIFTY (has 200-level) title must read `Depth-20+200 rebalance: …`: {msg}"
        );
        assert!(msg.contains("20-level + 200-level unsub old"));
    }

    #[test]
    fn test_depth_rebalance_levels_title_fragment() {
        assert_eq!(
            DepthRebalanceLevels::TwentyOnly.title_fragment(),
            "Depth-20"
        );
        assert_eq!(
            DepthRebalanceLevels::TwentyAndTwoHundred.title_fragment(),
            "Depth-20+200"
        );
    }

    #[test]
    fn test_depth_rebalance_levels_action_line() {
        // The action line explicitly distinguishes "20-level only" from
        // "20-level + 200-level" so the operator knows the swap scope
        // without reading the full message body.
        let twenty = DepthRebalanceLevels::TwentyOnly.action_line();
        assert!(twenty.contains("20-level unsub old"));
        assert!(twenty.contains("no 200-level for this underlying"));

        let both = DepthRebalanceLevels::TwentyAndTwoHundred.action_line();
        assert!(both.contains("20-level + 200-level unsub old"));
        assert!(!both.contains("no 200-level"));
    }

    #[test]
    fn test_auth_failed_still_critical() {
        // Final / permanent failure still fires Critical — that's the whole
        // point of splitting the variants. If this flips to Low by accident,
        // permanent auth death becomes silent.
        let event = NotificationEvent::AuthenticationFailed {
            reason: "PERMANENT: invalid pin".to_string(),
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_auth_failed_includes_reason() {
        let event = NotificationEvent::AuthenticationFailed {
            reason: "HTTP 401 Unauthorized".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("HTTP 401 Unauthorized"));
        assert!(msg.contains("FAILED"));
    }

    #[test]
    fn test_auth_failed_redacts_credentials_in_url() {
        let event = NotificationEvent::AuthenticationFailed {
            reason: "generateAccessToken request failed: error sending request for url (https://auth.dhan.co/app/generateAccessToken?dhanClientId=0000000000&pin=000000&totp=000000)".to_string(),
        };
        let msg = event.to_message();
        assert!(!msg.contains("0000000000"), "client ID leaked: {msg}");
        assert!(!msg.contains("pin=000000"), "PIN leaked: {msg}");
        assert!(!msg.contains("totp=000000"), "TOTP leaked: {msg}");
        assert!(msg.contains("FAILED"));
        assert!(msg.contains("[REDACTED]"));
    }

    #[test]
    fn test_token_renewal_failed_redacts_credentials() {
        let event = NotificationEvent::TokenRenewalFailed {
            attempts: 3,
            reason: "request for url (https://auth.dhan.co/app/generateAccessToken?pin=123456&totp=654321)".to_string(),
        };
        let msg = event.to_message();
        assert!(!msg.contains("123456"), "PIN leaked: {msg}");
        assert!(!msg.contains("654321"), "TOTP leaked: {msg}");
    }

    #[test]
    fn test_token_renewed_message() {
        let event = NotificationEvent::TokenRenewed;
        let msg = event.to_message();
        assert!(msg.contains("renewed"));
    }

    #[test]
    fn test_token_renewal_failed_includes_attempts_and_reason() {
        let event = NotificationEvent::TokenRenewalFailed {
            attempts: 3,
            reason: "timeout".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("3"));
        assert!(msg.contains("timeout"));
    }

    #[test]
    fn test_websocket_connected_includes_index() {
        // UX fix 2026-04-17: display is 1-indexed (connection_index=2 → "3/5")
        let event = NotificationEvent::WebSocketConnected {
            connection_index: 2,
        };
        let msg = event.to_message();
        assert!(msg.contains("3"), "1-indexed display: 2 -> 3; got: {msg}");
        assert!(msg.contains("connected"));
    }

    #[test]
    fn test_websocket_disconnected_includes_index_and_reason() {
        // UX fix 2026-04-17: display is 1-indexed (connection_index=1 → "2/5")
        let event = NotificationEvent::WebSocketDisconnected {
            connection_index: 1,
            reason: "connection reset by peer".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("2"), "1-indexed display: 1 -> 2; got: {msg}");
        assert!(msg.contains("connection reset by peer"));
    }

    #[test]
    fn test_websocket_reconnected_includes_index() {
        // UX fix 2026-04-17: display is 1-indexed (connection_index=0 → "1/5")
        let event = NotificationEvent::WebSocketReconnected {
            connection_index: 0,
        };
        let msg = event.to_message();
        assert!(msg.contains("1"), "1-indexed display: 0 -> 1; got: {msg}");
        assert!(msg.contains("reconnected"));
    }

    #[test]
    fn test_shutdown_initiated_message() {
        let event = NotificationEvent::ShutdownInitiated;
        let msg = event.to_message();
        assert!(msg.contains("Shutdown"));
    }

    #[test]
    fn test_shutdown_complete_message() {
        let event = NotificationEvent::ShutdownComplete;
        let msg = event.to_message();
        assert!(msg.contains("stopped"));
    }

    #[test]
    fn test_custom_message_passthrough() {
        let event = NotificationEvent::Custom {
            message: "custom alert payload".to_string(),
        };
        let msg = event.to_message();
        assert_eq!(msg, "custom alert payload");
    }

    #[test]
    fn test_instrument_build_success_message() {
        let event = NotificationEvent::InstrumentBuildSuccess {
            source: "primary".to_string(),
            derivative_count: 96948,
            underlying_count: 214,
        };
        let msg = event.to_message();
        assert!(msg.contains("Instruments OK"));
        assert!(msg.contains("primary"));
        assert!(msg.contains("96948"));
        assert!(msg.contains("214"));
    }

    #[test]
    fn test_instrument_build_failed_message() {
        let event = NotificationEvent::InstrumentBuildFailed {
            reason: "HTTP 503 Service Unavailable".to_string(),
            manual_trigger_url: "http://0.0.0.0:3001/api/instruments/rebuild".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("Instruments FAILED"));
        assert!(msg.contains("HTTP 503"));
        // SECURITY: internal URL must NOT appear in Telegram
        assert!(!msg.contains("0.0.0.0"), "internal URL leaked: {msg}");
        assert!(!msg.contains("3001"), "internal port leaked: {msg}");
        assert!(msg.contains("/instruments/rebuild"));
    }

    #[test]
    fn test_historical_fetch_failed_message() {
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 200,
            instruments_failed: 9,
            total_candles: 180000,
            persist_failures: 0,
            failed_instruments: vec![],
            failure_reasons: std::collections::HashMap::new(),
        };
        let msg = event.to_message();
        assert!(msg.contains("partial failure"));
        assert!(msg.contains("200"));
        assert!(msg.contains("9"));
        assert!(msg.contains("180000"));
    }

    #[test]
    fn test_historical_fetch_failed_is_high() {
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 200,
            instruments_failed: 9,
            total_candles: 180000,
            persist_failures: 0,
            failed_instruments: vec![],
            failure_reasons: std::collections::HashMap::new(),
        };
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_historical_fetch_failed_shows_instrument_names() {
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 229,
            instruments_failed: 3,
            total_candles: 172125,
            persist_failures: 0,
            failed_instruments: vec![
                "RELIANCE (NSE_EQ)".to_string(),
                "NIFTY50 (IDX_I)".to_string(),
                "BANKNIFTY (IDX_I)".to_string(),
            ],
            failure_reasons: std::collections::HashMap::new(),
        };
        let msg = event.to_message();
        assert!(msg.contains("Failed instruments:"));
        assert!(msg.contains("RELIANCE (NSE_EQ)"));
        assert!(msg.contains("NIFTY50 (IDX_I)"));
        assert!(msg.contains("BANKNIFTY (IDX_I)"));
    }

    #[test]
    fn test_historical_fetch_failed_truncates_long_list() {
        let names: Vec<String> = (0..15).map(|i| format!("INST_{i} (NSE_EQ)")).collect();
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 217,
            instruments_failed: 15,
            total_candles: 160000,
            persist_failures: 0,
            failed_instruments: names,
            failure_reasons: std::collections::HashMap::new(),
        };
        let msg = event.to_message();
        assert!(msg.contains("INST_0 (NSE_EQ)"));
        assert!(msg.contains("INST_9 (NSE_EQ)"));
        assert!(
            !msg.contains("INST_10 (NSE_EQ)"),
            "11th item should be truncated"
        );
        assert!(msg.contains("+5 more"));
    }

    #[test]
    fn test_historical_fetch_failed_shows_persist_errors() {
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 200,
            instruments_failed: 0,
            total_candles: 180000,
            persist_failures: 42,
            failed_instruments: vec![],
            failure_reasons: std::collections::HashMap::new(),
        };
        let msg = event.to_message();
        assert!(msg.contains("Persist errors: 42"));
    }

    #[test]
    fn test_historical_fetch_failed_shows_failure_reasons() {
        let mut reasons = std::collections::HashMap::new();
        reasons.insert("token_expired".to_string(), 5);
        reasons.insert("network_or_api".to_string(), 3);
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 224,
            instruments_failed: 8,
            total_candles: 168000,
            persist_failures: 0,
            failed_instruments: vec![],
            failure_reasons: reasons,
        };
        let msg = event.to_message();
        assert!(msg.contains("Failure breakdown:"));
        assert!(msg.contains("token_expired: 5"));
        assert!(msg.contains("network_or_api: 3"));
    }

    #[test]
    fn test_historical_fetch_complete_shows_persist_warnings() {
        let event = NotificationEvent::HistoricalFetchComplete {
            instruments_fetched: 232,
            instruments_skipped: 1050,
            total_candles: 187458,
            persist_failures: 42,
        };
        let msg = event.to_message();
        assert!(msg.contains("Historical candles OK"));
        assert!(msg.contains("Persist errors: 42"));
    }

    #[test]
    fn test_candle_verification_failed_message() {
        let event = NotificationEvent::CandleVerificationFailed {
            instruments_checked: 209,
            instruments_with_gaps: 3,
            timeframe_details: "1m: 78,000 (207 instruments)\n5m: 15,600 (209 instruments)"
                .to_string(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            ohlc_details: vec![],
            data_details: vec![],
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("verification FAILED"));
        assert!(msg.contains("209"));
        assert!(msg.contains("Gaps: 3"));
        assert!(msg.contains("Timeframes:"));
        assert!(msg.contains("1m: 78,000"));
    }

    #[test]
    fn test_candle_verification_failed_shows_ohlc_details() {
        let event = NotificationEvent::CandleVerificationFailed {
            instruments_checked: 232,
            instruments_with_gaps: 0,
            timeframe_details: String::new(),
            ohlc_violations: 2,
            data_violations: 0,
            timestamp_violations: 0,
            ohlc_details: vec![
                "\u{2022} RELIANCE (NSE_EQ) 1m @ 2026-03-18 10:15 IST\n  H=2440.0 < L=2450.0"
                    .to_string(),
            ],
            data_details: vec![],
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("OHLC violations (2)"));
        assert!(msg.contains("RELIANCE"));
        assert!(msg.contains("H=2440.0 < L=2450.0"));
    }

    #[test]
    fn test_candle_verification_failed_shows_all_violations() {
        let event = NotificationEvent::CandleVerificationFailed {
            instruments_checked: 232,
            instruments_with_gaps: 3,
            timeframe_details: "1m: 85125 (229 inst)".to_string(),
            ohlc_violations: 2,
            data_violations: 5,
            timestamp_violations: 8,
            ohlc_details: vec!["ohlc line".to_string()],
            data_details: vec!["data line".to_string()],
            timestamp_details: vec!["ts line".to_string()],
            weekend_violations: 0,
            weekend_details: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("OHLC violations (2)"));
        assert!(msg.contains("Data violations (5)"));
        assert!(msg.contains("Timestamp violations (8)"));
    }

    #[test]
    fn test_candle_verification_failed_zero_instruments() {
        let event = NotificationEvent::CandleVerificationFailed {
            instruments_checked: 0,
            instruments_with_gaps: 0,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            ohlc_details: vec![],
            data_details: vec![],
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("Checked: 0"));
        assert!(msg.contains("No instrument data found"));
    }

    #[test]
    fn test_candle_verification_failed_is_high() {
        let event = NotificationEvent::CandleVerificationFailed {
            instruments_checked: 209,
            instruments_with_gaps: 3,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            ohlc_details: vec![],
            data_details: vec![],
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
        };
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_candle_verification_passed_shows_check_marks() {
        let event = NotificationEvent::CandleVerificationPassed {
            instruments_checked: 232,
            total_candles: 187500,
            timeframe_details: "1m: 86250 (232 inst)".to_string(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            weekend_violations: 0,
        };
        let msg = event.to_message();
        assert!(msg.contains("Candle verification OK"));
        assert!(msg.contains("OHLC"));
        assert!(msg.contains("Data"));
        assert!(msg.contains("Timestamps"));
    }

    #[test]
    fn test_candle_verification_passed_shows_warnings_if_any() {
        let event = NotificationEvent::CandleVerificationPassed {
            instruments_checked: 232,
            total_candles: 187500,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 2,
            timestamp_violations: 1,
            weekend_violations: 0,
        };
        let msg = event.to_message();
        assert!(msg.contains("Data violations: 2 (non-blocking)"));
        assert!(msg.contains("Timestamp violations: 1 (non-blocking)"));
    }

    #[test]
    fn test_cross_match_passed_message() {
        let event = NotificationEvent::CandleCrossMatchPassed {
            timeframes_checked: 5,
            candles_compared: 187500,
            today_ist_label: "2026-04-21 09:15→15:30 IST".to_string(),
            scope_indices: 5,
            scope_equities: 206,
            per_tf_cells: vec![("1m".to_string(), 79125), ("5m".to_string(), 15825)],
        };
        let msg = event.to_message();
        assert!(msg.contains("CROSS-MATCH OK"));
        assert!(msg.contains("79125"));
        assert!(msg.contains("bit-for-bit"));
        assert!(msg.contains("<pre>"));
    }

    #[test]
    fn test_cross_match_failed_shows_details() {
        let event = NotificationEvent::CandleCrossMatchFailed {
            candles_compared: 187500,
            mismatches: 12,
            missing_live: 8,
            mismatch_details: vec![
                " RELIANCE    NSE_EQ  1m   10:15:00   (live missing)".to_string(),
            ],
            today_ist_label: "2026-04-21 09:15→15:30 IST".to_string(),
            scope_indices: 5,
            scope_equities: 206,
            missing_historical: 1,
            value_mismatches: 3,
            per_tf_gaps: vec![("1m".to_string(), 9), ("5m".to_string(), 3)],
        };
        let msg = event.to_message();
        assert!(msg.contains("CROSS-MATCH FAILED"));
        assert!(msg.contains("Missing live"));
        assert!(msg.contains("Missing historical"));
        assert!(msg.contains("Value mismatches"));
        assert!(msg.contains("Total issues"));
        assert!(msg.contains("RELIANCE"));
        assert!(msg.contains("(live missing)"));
    }

    #[test]
    fn test_cross_match_failed_is_high() {
        let event = NotificationEvent::CandleCrossMatchFailed {
            candles_compared: 187500,
            mismatches: 12,
            missing_live: 8,
            mismatch_details: vec![],
            today_ist_label: String::new(),
            scope_indices: 0,
            scope_equities: 0,
            missing_historical: 0,
            value_mismatches: 4,
            per_tf_gaps: vec![],
        };
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_cross_match_passed_is_low() {
        let event = NotificationEvent::CandleCrossMatchPassed {
            timeframes_checked: 5,
            candles_compared: 187500,
            today_ist_label: String::new(),
            scope_indices: 0,
            scope_equities: 0,
            per_tf_cells: vec![],
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_cross_match_skipped_message() {
        let event = NotificationEvent::CandleCrossMatchSkipped {
            reason: "no live data in materialized views".to_string(),
            candles_compared: 0,
        };
        let msg = event.to_message();
        assert!(msg.contains("cross-match SKIPPED"));
        assert!(msg.contains("no live data"));
        assert!(msg.contains("Will compare on next run"));
    }

    #[test]
    fn test_cross_match_skipped_weekend_subtext() {
        let event = NotificationEvent::CandleCrossMatchSkipped {
            reason: "weekend or holiday — not a trading day".to_string(),
            candles_compared: 0,
        };
        let msg = event.to_message();
        assert!(
            msg.contains("next trading day"),
            "weekend reason must use trading-day subtext, got: {msg}"
        );
        assert!(
            !msg.contains("first run"),
            "weekend reason must NOT show the first-run subtext"
        );
    }

    #[test]
    fn test_cross_match_skipped_premarket_subtext() {
        let event = NotificationEvent::CandleCrossMatchSkipped {
            reason: "pre-market (before 09:15 IST) — no live data yet".to_string(),
            candles_compared: 0,
        };
        let msg = event.to_message();
        assert!(
            msg.contains("after market open"),
            "pre-market reason must use post-open subtext, got: {msg}"
        );
    }

    #[test]
    fn test_cross_match_skipped_shows_reason() {
        let event = NotificationEvent::CandleCrossMatchSkipped {
            reason: "fresh DB after migration".to_string(),
            candles_compared: 348_968,
        };
        let msg = event.to_message();
        assert!(msg.contains("fresh DB after migration"));
        assert!(msg.contains("348968"));
    }

    #[test]
    fn test_cross_match_skipped_is_low() {
        let event = NotificationEvent::CandleCrossMatchSkipped {
            reason: "no live data".to_string(),
            candles_compared: 0,
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_cross_match_skipped_weekend_reason_renders() {
        // Regression: weekend / non-trading-day boot must emit the typed
        // SKIPPED event so Telegram always shows closure for the
        // post-fetch cross-match step.
        let event = NotificationEvent::CandleCrossMatchSkipped {
            reason: "weekend or holiday — not a trading day".to_string(),
            candles_compared: 0,
        };
        let msg = event.to_message();
        assert!(msg.contains("SKIPPED"));
        assert!(msg.contains("weekend or holiday"));
        assert!(msg.contains("not a trading day"));
        assert_eq!(event.severity(), Severity::Low);
    }

    // -- Severity tests --

    #[test]
    fn test_auth_failed_is_critical() {
        let event = NotificationEvent::AuthenticationFailed {
            reason: "timeout".to_string(),
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_token_renewal_failed_is_critical() {
        let event = NotificationEvent::TokenRenewalFailed {
            attempts: 3,
            reason: "timeout".to_string(),
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_ws_disconnected_is_high() {
        let event = NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: "reset".to_string(),
        };
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_custom_is_high() {
        let event = NotificationEvent::Custom {
            message: "alert".to_string(),
        };
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_startup_complete_is_info() {
        let event = NotificationEvent::StartupComplete { mode: "LIVE" };
        assert_eq!(event.severity(), Severity::Info);
    }

    #[test]
    fn test_shutdown_complete_is_info() {
        let event = NotificationEvent::ShutdownComplete;
        assert_eq!(event.severity(), Severity::Info);
    }

    // -- IP Verification event tests --

    #[test]
    fn test_ip_verification_failed_message() {
        let event = NotificationEvent::IpVerificationFailed {
            reason: "IP mismatch — expected 203.0.XXX.XX, got 198.51.XXX.XX".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("IP VERIFICATION FAILED"));
        assert!(msg.contains("IP mismatch"));
        assert!(msg.contains("Boot blocked"));
    }

    #[test]
    fn test_ip_verification_failed_is_critical() {
        let event = NotificationEvent::IpVerificationFailed {
            reason: "SSM unreachable".to_string(),
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_ip_verification_success_message_masks_ip() {
        let event = NotificationEvent::IpVerificationSuccess {
            verified_ip: "203.0.113.42".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("IP verified"));
        // SECURITY: Full IP must NOT appear in Telegram message
        assert!(
            !msg.contains("203.0.113.42"),
            "full IP must be masked in notification: {msg}"
        );
        // Only network prefix should be visible
        assert!(msg.contains("203.0.XXX.XX"), "masked IP expected: {msg}");
    }

    #[test]
    fn test_ip_verification_success_is_low() {
        let event = NotificationEvent::IpVerificationSuccess {
            verified_ip: "10.0.0.1".to_string(),
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_mask_ip_for_notification_standard() {
        assert_eq!(mask_ip_for_notification("59.92.114.17"), "59.92.XXX.XX");
        assert_eq!(mask_ip_for_notification("10.0.0.1"), "10.0.XXX.XX");
    }

    #[test]
    fn test_mask_ip_for_notification_invalid() {
        assert_eq!(mask_ip_for_notification("not-an-ip"), "XXX.XXX.XXX.XXX");
        assert_eq!(mask_ip_for_notification(""), "XXX.XXX.XXX.XXX");
    }

    #[test]
    fn test_historical_fetch_complete_message() {
        let event = NotificationEvent::HistoricalFetchComplete {
            instruments_fetched: 50,
            instruments_skipped: 200,
            total_candles: 187500,
            persist_failures: 0,
        };
        let msg = event.to_message();
        assert!(msg.contains("Historical candles OK"));
        assert!(msg.contains("50"));
        assert!(msg.contains("200"));
        assert!(msg.contains("187500"));
        assert!(msg.contains("1m, 5m, 15m, 60m, 1d"));
    }

    #[test]
    fn test_historical_fetch_complete_is_low() {
        let event = NotificationEvent::HistoricalFetchComplete {
            instruments_fetched: 50,
            instruments_skipped: 200,
            total_candles: 187500,
            persist_failures: 0,
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_candle_verification_passed_message() {
        let event = NotificationEvent::CandleVerificationPassed {
            instruments_checked: 50,
            total_candles: 187500,
            timeframe_details: "1m: 18,750 (50 inst)\n5m: 3,750 (50 inst)\n15m: 1,250 (50 inst)\n60m: 312 (50 inst)\n1d: 50 (50 inst)".to_string(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            weekend_violations: 0,
        };
        let msg = event.to_message();
        assert!(msg.contains("Candle verification OK"));
        assert!(msg.contains("50"));
        assert!(msg.contains("187500"));
        assert!(msg.contains("Timeframes:"));
        assert!(msg.contains("1m: 18,750"));
        assert!(msg.contains("1d: 50"));
    }

    #[test]
    fn test_candle_verification_passed_is_low() {
        let event = NotificationEvent::CandleVerificationPassed {
            instruments_checked: 50,
            total_candles: 187500,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            weekend_violations: 0,
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_severity_ordering() {
        assert!(Severity::Critical > Severity::High);
        assert!(Severity::High > Severity::Medium);
        assert!(Severity::Medium > Severity::Low);
        assert!(Severity::Low > Severity::Info);
    }

    // -- OMS notification event tests --

    #[test]
    fn test_order_rejected_message() {
        let event = NotificationEvent::OrderRejected {
            correlation_id: "ORD-12345".to_string(),
            reason: "DH-906: Invalid order quantity".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("Order REJECTED"));
        assert!(msg.contains("ORD-12345"));
        assert!(msg.contains("DH-906"));
    }

    #[test]
    fn test_oms_event_severity() {
        let rejected = NotificationEvent::OrderRejected {
            correlation_id: "X".to_string(),
            reason: "bad".to_string(),
        };
        assert_eq!(rejected.severity(), Severity::High);

        let cb_open = NotificationEvent::CircuitBreakerOpened {
            consecutive_failures: 5,
        };
        assert_eq!(cb_open.severity(), Severity::High);

        let cb_close = NotificationEvent::CircuitBreakerClosed;
        assert_eq!(cb_close.severity(), Severity::Medium);

        let rate_limit = NotificationEvent::RateLimitExhausted {
            limit_type: "per_second".to_string(),
        };
        assert_eq!(rate_limit.severity(), Severity::High);
    }

    #[test]
    fn test_oms_event_formatting() {
        let cb = NotificationEvent::CircuitBreakerOpened {
            consecutive_failures: 3,
        };
        let msg = cb.to_message();
        assert!(msg.contains("Circuit breaker OPENED"));
        assert!(msg.contains("3"));

        let rl = NotificationEvent::RateLimitExhausted {
            limit_type: "daily".to_string(),
        };
        let msg = rl.to_message();
        assert!(msg.contains("Rate limit EXHAUSTED"));
        assert!(msg.contains("daily"));
    }

    #[test]
    fn test_circuit_breaker_notify_on_open() {
        let event = NotificationEvent::CircuitBreakerOpened {
            consecutive_failures: 5,
        };
        let msg = event.to_message();
        assert!(msg.contains("OPENED"));
        assert!(msg.contains("halted"));
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_risk_halt_notification() {
        let event = NotificationEvent::RiskHalt {
            reason: "daily_loss_breach: -25000.00 exceeds threshold".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("RISK HALT"));
        assert!(msg.contains("daily_loss_breach"));
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_ws_reconnection_exhausted_notification() {
        // UX fix 2026-04-17: display is 1-indexed "3/5" (no # prefix).
        let event = NotificationEvent::WebSocketReconnectionExhausted {
            connection_index: 2,
            attempts: 10,
        };
        let msg = event.to_message();
        assert!(msg.contains("RECONNECTION EXHAUSTED"));
        assert!(
            msg.contains("3/"),
            "1-indexed display: 2 -> 3/N; got: {msg}"
        );
        assert!(msg.contains("10"));
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_token_renewal_deadline_missed_notification() {
        let event = NotificationEvent::TokenRenewalDeadlineMissed {
            deadline_hour_ist: 14,
        };
        let msg = event.to_message();
        assert!(msg.contains("DEADLINE MISSED"));
        assert!(msg.contains("14:00 IST"));
        assert_eq!(event.severity(), Severity::Critical);
    }

    // =====================================================================
    // Additional coverage: severity for all remaining variants, boot events,
    // append_detail_lines helper, edge cases in message formatting
    // =====================================================================

    #[test]
    fn test_boot_health_check_message_and_severity() {
        let event = NotificationEvent::BootHealthCheck {
            services_healthy: 7,
            services_total: 8,
        };
        let msg = event.to_message();
        assert!(msg.contains("Boot health check"));
        assert!(msg.contains("7/8"));
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_boot_deadline_missed_message_and_severity() {
        let event = NotificationEvent::BootDeadlineMissed {
            deadline_secs: 120,
            step: "QuestDB DDL".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("BOOT DEADLINE MISSED"));
        assert!(msg.contains("120s"));
        assert!(msg.contains("QuestDB DDL"));
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_instrument_build_success_severity() {
        let event = NotificationEvent::InstrumentBuildSuccess {
            source: "primary".to_string(),
            derivative_count: 100,
            underlying_count: 10,
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_instrument_build_failed_severity() {
        let event = NotificationEvent::InstrumentBuildFailed {
            reason: "test".to_string(),
            manual_trigger_url: "http://test".to_string(),
        };
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_ws_connected_severity() {
        let event = NotificationEvent::WebSocketConnected {
            connection_index: 0,
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_ws_reconnected_severity() {
        let event = NotificationEvent::WebSocketReconnected {
            connection_index: 0,
        };
        assert_eq!(event.severity(), Severity::Medium);
    }

    /// Ratchet (added 2026-04-26): order update WS reconnects MUST be
    /// Severity::Low, not Medium. Dhan idle-disconnects accounts every
    /// 30-60 minutes on dry-run / paper-trading days, so this event
    /// fires 5-10x/day and Medium = pure pager fatigue. The disconnect
    /// leg (`OrderUpdateDisconnected`, Severity::High) is what pages
    /// the operator. If this test fails because someone bumped this
    /// back to Medium/High, talk to whoever did it before "fixing"
    /// the test.
    #[test]
    fn test_order_update_reconnected_severity_is_low() {
        let event = NotificationEvent::OrderUpdateReconnected {
            consecutive_failures: 5,
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_shutdown_initiated_severity() {
        assert_eq!(
            NotificationEvent::ShutdownInitiated.severity(),
            Severity::Medium
        );
    }

    #[test]
    fn test_token_renewed_severity() {
        assert_eq!(NotificationEvent::TokenRenewed.severity(), Severity::Low);
    }

    #[test]
    fn test_auth_success_severity() {
        assert_eq!(
            NotificationEvent::AuthenticationSuccess.severity(),
            Severity::Low
        );
    }

    #[test]
    fn test_circuit_breaker_closed_message() {
        let event = NotificationEvent::CircuitBreakerClosed;
        let msg = event.to_message();
        assert!(msg.contains("Circuit breaker CLOSED"));
        assert!(msg.contains("resumed"));
    }

    #[test]
    fn test_historical_fetch_complete_no_persist_failures() {
        let event = NotificationEvent::HistoricalFetchComplete {
            instruments_fetched: 232,
            instruments_skipped: 1050,
            total_candles: 187458,
            persist_failures: 0,
        };
        let msg = event.to_message();
        assert!(!msg.contains("Persist errors"));
    }

    #[test]
    fn test_historical_fetch_failed_empty_reasons_and_instruments() {
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 0,
            instruments_failed: 0,
            total_candles: 0,
            persist_failures: 0,
            failed_instruments: vec![],
            failure_reasons: std::collections::HashMap::new(),
        };
        let msg = event.to_message();
        assert!(msg.contains("partial failure"));
        assert!(!msg.contains("Failure breakdown"));
        assert!(!msg.contains("Failed instruments"));
    }

    #[test]
    fn test_cross_match_failed_no_missing_live() {
        // Missing-live is always rendered in the new format — but as `0` in the
        // SUMMARY block. The top-level FAILED message still appears.
        let event = NotificationEvent::CandleCrossMatchFailed {
            candles_compared: 1000,
            mismatches: 5,
            missing_live: 0,
            mismatch_details: vec![],
            today_ist_label: String::new(),
            scope_indices: 0,
            scope_equities: 0,
            missing_historical: 0,
            value_mismatches: 5,
            per_tf_gaps: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("FAILED"));
        assert!(msg.contains("Missing live       "));
    }

    #[test]
    fn test_cross_match_failed_no_mismatch_details_section() {
        let event = NotificationEvent::CandleCrossMatchFailed {
            candles_compared: 1000,
            mismatches: 0,
            missing_live: 0,
            mismatch_details: vec![],
            today_ist_label: String::new(),
            scope_indices: 0,
            scope_equities: 0,
            missing_historical: 0,
            value_mismatches: 0,
            per_tf_gaps: vec![],
        };
        let msg = event.to_message();
        // No per-row list when mismatch_details empty.
        assert!(!msg.contains("📋"));
    }

    #[test]
    fn test_candle_verification_passed_empty_timeframe_details() {
        let event = NotificationEvent::CandleVerificationPassed {
            instruments_checked: 10,
            total_candles: 1000,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            weekend_violations: 0,
        };
        let msg = event.to_message();
        assert!(!msg.contains("Timeframes:"));
    }

    #[test]
    fn test_candle_verification_passed_ohlc_violations_only() {
        let event = NotificationEvent::CandleVerificationPassed {
            instruments_checked: 10,
            total_candles: 1000,
            timeframe_details: String::new(),
            ohlc_violations: 3,
            data_violations: 0,
            timestamp_violations: 0,
            weekend_violations: 0,
        };
        let msg = event.to_message();
        assert!(msg.contains("OHLC violations: 3"));
        assert!(!msg.contains("Data violations"));
        assert!(!msg.contains("Timestamp violations"));
    }

    #[test]
    fn test_append_detail_lines_truncation() {
        let details: Vec<String> = (0..15).map(|i| format!("line {i}")).collect();
        let mut msg = String::new();
        super::append_detail_lines(&mut msg, &details, 15);
        // Should show first 10 and "+5 more"
        assert!(msg.contains("line 0"));
        assert!(msg.contains("line 9"));
        assert!(!msg.contains("line 10"));
        assert!(msg.contains("+5 more"));
    }

    #[test]
    fn test_append_detail_lines_no_truncation() {
        let details: Vec<String> = (0..5).map(|i| format!("line {i}")).collect();
        let mut msg = String::new();
        super::append_detail_lines(&mut msg, &details, 5);
        assert!(msg.contains("line 0"));
        assert!(msg.contains("line 4"));
        assert!(!msg.contains("more"));
    }

    #[test]
    fn test_append_detail_lines_empty() {
        let mut msg = String::new();
        super::append_detail_lines(&mut msg, &[], 0);
        assert!(msg.is_empty());
    }

    #[test]
    fn test_severity_equality() {
        assert_eq!(Severity::Critical, Severity::Critical);
        assert_ne!(Severity::Critical, Severity::High);
    }

    #[test]
    fn test_severity_debug() {
        let debug = format!("{:?}", Severity::Critical);
        assert_eq!(debug, "Critical");
    }

    #[test]
    fn test_notification_event_clone() {
        let event = NotificationEvent::TokenRenewed;
        let cloned = event.clone();
        assert_eq!(cloned.to_message(), event.to_message());
    }

    #[test]
    fn test_cross_match_failed_shows_every_mismatch_no_truncation() {
        // Parthiban directive 2026-04-21: FULL LIST, no "+N more". 60 rows
        // must all appear in the Telegram body.
        let details: Vec<String> = (0..60).map(|i| format!("mismatch {i}")).collect();
        let event = NotificationEvent::CandleCrossMatchFailed {
            candles_compared: 100,
            mismatches: 60,
            missing_live: 0,
            mismatch_details: details,
            today_ist_label: String::new(),
            scope_indices: 0,
            scope_equities: 0,
            missing_historical: 0,
            value_mismatches: 60,
            per_tf_gaps: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("mismatch 0"));
        assert!(msg.contains("mismatch 49"));
        assert!(msg.contains("mismatch 50"));
        assert!(msg.contains("mismatch 59"));
        assert!(
            !msg.contains("more (full list"),
            "no truncation suffix allowed"
        );
    }

    // =====================================================================
    // Additional coverage: Severity Copy/Clone, Debug impls, event Debug,
    // append_detail_lines total_count > details.len(), edge cases
    // =====================================================================

    #[test]
    fn test_severity_is_copy() {
        let s = Severity::High;
        let copy = s;
        // Both should be usable after copy (Copy trait)
        assert_eq!(s, copy);
        assert_eq!(s, Severity::High);
    }

    #[test]
    fn test_severity_clone() {
        let s = Severity::Medium;
        #[allow(clippy::clone_on_copy)]
        let cloned = s.clone();
        assert_eq!(s, cloned);
    }

    #[test]
    fn test_severity_all_variants_debug() {
        let variants = [
            Severity::Info,
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ];
        let expected = ["Info", "Low", "Medium", "High", "Critical"];
        for (variant, name) in variants.iter().zip(expected.iter()) {
            assert_eq!(format!("{variant:?}"), *name);
        }
    }

    #[test]
    fn test_severity_ord_covers_all_pairs() {
        let ordered = [
            Severity::Info,
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ];
        for i in 0..ordered.len() {
            for j in (i + 1)..ordered.len() {
                assert!(
                    ordered[i] < ordered[j],
                    "{:?} should be < {:?}",
                    ordered[i],
                    ordered[j]
                );
            }
        }
    }

    #[test]
    fn test_notification_event_debug_impl() {
        let event = NotificationEvent::TokenRenewed;
        let debug_str = format!("{event:?}");
        assert!(debug_str.contains("TokenRenewed"));
    }

    #[test]
    fn test_notification_event_debug_with_fields() {
        let event = NotificationEvent::WebSocketDisconnected {
            connection_index: 3,
            reason: "timeout".to_string(),
        };
        let debug_str = format!("{event:?}");
        assert!(debug_str.contains("WebSocketDisconnected"));
        assert!(debug_str.contains("3"));
        assert!(debug_str.contains("timeout"));
    }

    #[test]
    fn test_notification_event_clone_with_complex_fields() {
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 100,
            instruments_failed: 5,
            total_candles: 50000,
            persist_failures: 2,
            failed_instruments: vec!["RELIANCE".to_string(), "TCS".to_string()],
            failure_reasons: {
                let mut m = std::collections::HashMap::new();
                m.insert("network".to_string(), 3);
                m.insert("timeout".to_string(), 2);
                m
            },
        };
        let cloned = event.clone();
        assert_eq!(event.to_message(), cloned.to_message());
        assert_eq!(event.severity(), cloned.severity());
    }

    #[test]
    fn test_append_detail_lines_total_greater_than_details_len() {
        // total_count=50 but only 3 detail strings provided
        // Should show all 3 lines and "+47 more"
        let details = vec![
            "line A".to_string(),
            "line B".to_string(),
            "line C".to_string(),
        ];
        let mut msg = String::new();
        super::append_detail_lines(&mut msg, &details, 50);
        assert!(msg.contains("line A"));
        assert!(msg.contains("line B"));
        assert!(msg.contains("line C"));
        assert!(msg.contains("+47 more"));
    }

    #[test]
    fn test_append_detail_lines_exactly_10_no_truncation() {
        let details: Vec<String> = (0..10).map(|i| format!("detail {i}")).collect();
        let mut msg = String::new();
        super::append_detail_lines(&mut msg, &details, 10);
        assert!(msg.contains("detail 0"));
        assert!(msg.contains("detail 9"));
        assert!(!msg.contains("more"));
    }

    #[test]
    fn test_append_detail_lines_11_triggers_truncation() {
        let details: Vec<String> = (0..11).map(|i| format!("detail {i}")).collect();
        let mut msg = String::new();
        super::append_detail_lines(&mut msg, &details, 11);
        assert!(msg.contains("detail 0"));
        assert!(msg.contains("detail 9"));
        assert!(!msg.contains("detail 10"));
        assert!(msg.contains("+1 more"));
    }

    #[test]
    fn test_candle_verification_failed_data_violations_only() {
        let event = NotificationEvent::CandleVerificationFailed {
            instruments_checked: 50,
            instruments_with_gaps: 0,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 7,
            timestamp_violations: 0,
            ohlc_details: vec![],
            data_details: vec!["bad data line".to_string()],
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("Data violations (7)"));
        assert!(msg.contains("bad data line"));
        assert!(!msg.contains("OHLC violations"));
        assert!(!msg.contains("Timestamp violations"));
    }

    #[test]
    fn test_candle_verification_failed_timestamp_violations_only() {
        let event = NotificationEvent::CandleVerificationFailed {
            instruments_checked: 50,
            instruments_with_gaps: 0,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 4,
            ohlc_details: vec![],
            data_details: vec![],
            timestamp_details: vec!["ts violation".to_string()],
            weekend_violations: 0,
            weekend_details: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("Timestamp violations (4)"));
        assert!(msg.contains("ts violation"));
        assert!(!msg.contains("OHLC violations"));
        assert!(!msg.contains("Data violations"));
    }

    #[test]
    fn test_risk_halt_severity_is_critical() {
        let event = NotificationEvent::RiskHalt {
            reason: "position_limit".to_string(),
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_ws_reconnection_exhausted_severity_is_critical() {
        let event = NotificationEvent::WebSocketReconnectionExhausted {
            connection_index: 0,
            attempts: 5,
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_token_renewal_deadline_missed_severity_is_critical() {
        let event = NotificationEvent::TokenRenewalDeadlineMissed {
            deadline_hour_ist: 10,
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_rate_limit_exhausted_message_format() {
        let event = NotificationEvent::RateLimitExhausted {
            limit_type: "per_second".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("Rate limit EXHAUSTED"));
        assert!(msg.contains("per_second"));
    }

    #[test]
    fn test_risk_halt_message_format() {
        let event = NotificationEvent::RiskHalt {
            reason: "position_limit_breach".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("RISK HALT"));
        assert!(msg.contains("position_limit_breach"));
    }

    #[test]
    fn test_every_severity_variant_is_reachable() {
        // Verify each severity level is returned by at least one event variant
        let info_event = NotificationEvent::StartupComplete { mode: "LIVE" };
        assert_eq!(info_event.severity(), Severity::Info);

        let low_event = NotificationEvent::TokenRenewed;
        assert_eq!(low_event.severity(), Severity::Low);

        let medium_event = NotificationEvent::ShutdownInitiated;
        assert_eq!(medium_event.severity(), Severity::Medium);

        let high_event = NotificationEvent::Custom {
            message: "x".to_string(),
        };
        assert_eq!(high_event.severity(), Severity::High);

        let critical_event = NotificationEvent::RiskHalt {
            reason: "x".to_string(),
        };
        assert_eq!(critical_event.severity(), Severity::Critical);
    }

    #[test]
    fn test_websocket_disconnected_offhours_is_low_severity() {
        // Regression: 2026-04-22 pre-market Telegram spam from Dhan-side
        // TCP resets at 08:32 IST. Off-hours disconnects MUST be Low, not
        // High — HIGH path is reserved for in-market disconnects where
        // missed ticks are operator-actionable.
        let ev = NotificationEvent::WebSocketDisconnectedOffHours {
            connection_index: 0,
            reason: "Connection reset without closing handshake".to_string(),
        };
        assert_eq!(ev.severity(), Severity::Low);
    }

    #[test]
    fn test_websocket_disconnected_in_market_still_high_severity() {
        // In-market disconnects remain HIGH — the off-hours variant is a
        // split, not a replacement.
        let ev = NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: "x".to_string(),
        };
        assert_eq!(ev.severity(), Severity::High);
    }

    #[test]
    fn test_phase2_complete_message_includes_depth_counts() {
        // Plan item G (2026-04-22): unified 09:12 dispatch message must show
        // all three feed counts so the operator knows the full scope of
        // what subscribed at market open.
        let ev = NotificationEvent::Phase2Complete {
            added_count: 6123,
            duration_ms: 450,
            depth_20_underlyings: 4,
            depth_200_contracts: 4,
        };
        let msg = ev.to_message();
        assert!(
            msg.contains("Stock F&O: +6123"),
            "must show stock F&O count; got: {msg}"
        );
        assert!(
            msg.contains("Depth-20: 4 underlyings"),
            "must show depth-20 count; got: {msg}"
        );
        assert!(
            msg.contains("Depth-200: 4 contracts"),
            "must show depth-200 count; got: {msg}"
        );
        assert!(
            msg.contains("09:12:30"),
            "must reference the unified trigger time; got: {msg}"
        );
    }

    #[test]
    fn test_market_open_streaming_confirmation_severity_is_info() {
        // Plan #5 (2026-04-22): once-per-day positive heartbeat — must be
        // INFO so it never wakes anyone up but still appears in Telegram.
        let ev = NotificationEvent::MarketOpenStreamingConfirmation {
            main_feed_active: 5,
            main_feed_total: 5,
            depth_20_active: 4,
            depth_200_active: 4,
            order_update_active: true,
        };
        assert_eq!(ev.severity(), Severity::Info);
    }

    #[test]
    fn test_market_open_streaming_confirmation_message_lists_every_feed() {
        let ev = NotificationEvent::MarketOpenStreamingConfirmation {
            main_feed_active: 5,
            main_feed_total: 5,
            depth_20_active: 4,
            depth_200_active: 4,
            order_update_active: true,
        };
        let msg = ev.to_message();
        assert!(msg.contains("Streaming live"), "got: {msg}");
        assert!(msg.contains("Main feed: 5/5"), "got: {msg}");
        assert!(msg.contains("Depth-20: 4/4"), "got: {msg}");
        assert!(msg.contains("Depth-200: 4/4"), "got: {msg}");
        assert!(msg.contains("Order updates: 1/1"), "got: {msg}");
        assert!(
            msg.contains("09:15:30"),
            "must show trigger time; got: {msg}"
        );
    }

    #[test]
    fn test_market_open_streaming_confirmation_shows_oms_disconnected() {
        let ev = NotificationEvent::MarketOpenStreamingConfirmation {
            main_feed_active: 5,
            main_feed_total: 5,
            depth_20_active: 4,
            depth_200_active: 4,
            order_update_active: false,
        };
        let msg = ev.to_message();
        assert!(
            msg.contains("Order updates: 0/1"),
            "must show OMS disconnected when active=false; got: {msg}"
        );
    }

    #[test]
    fn test_websocket_disconnected_offhours_message_mentions_auto_reconnect() {
        // Message must tell the operator "auto-reconnecting" so they
        // immediately know no action is needed.
        let ev = NotificationEvent::WebSocketDisconnectedOffHours {
            connection_index: 0,
            reason: "Connection reset without closing handshake".to_string(),
        };
        let msg = ev.to_message();
        assert!(
            msg.contains("off-hours") && msg.contains("auto-reconnecting"),
            "off-hours disconnect message must label itself clearly; got: {msg}"
        );
    }

    /// 2026-04-25 ratchet: mid-market boot completion event has Info
    /// severity (no operator wake-up) and surfaces the resolution-source
    /// breakdown so operators can audit which fallback was used.
    #[test]
    fn test_midmarket_boot_complete_severity_is_info() {
        let ev = NotificationEvent::MidMarketBootComplete {
            stocks_live_tick: 200,
            stocks_rest: 14,
            stocks_quest_db: 1,
            stocks_skipped: 1,
            total_subscribed: 24_310,
            latency_ms: 30_500,
        };
        assert_eq!(ev.severity(), Severity::Info);
    }

    #[test]
    fn test_midmarket_boot_complete_message_lists_every_resolution_source() {
        let ev = NotificationEvent::MidMarketBootComplete {
            stocks_live_tick: 200,
            stocks_rest: 14,
            stocks_quest_db: 1,
            stocks_skipped: 1,
            total_subscribed: 24_310,
            latency_ms: 30_500,
        };
        let msg = ev.to_message();
        assert!(
            msg.contains("Live-tick"),
            "message must label live-tick path"
        );
        assert!(msg.contains("REST"), "message must label REST fallback");
        assert!(
            msg.contains("QuestDB"),
            "message must label QuestDB fallback"
        );
        assert!(msg.contains("Skipped"), "message must label skipped count");
        assert!(
            msg.contains("24310") || msg.contains("24,310"),
            "message must include total_subscribed; got: {msg}"
        );
        assert!(msg.contains("30500"), "message must include latency_ms");
    }
}
