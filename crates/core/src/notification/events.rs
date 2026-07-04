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

/// Escapes the three HTML-significant characters for Telegram's `HTML`
/// parse mode (`crates/core/src/notification/service.rs` sends
/// `"parse_mode": "HTML"`). External free-text fields (`reason`, `detail`,
/// `ip_match_status`, ... sourced from Dhan REST/WS error bodies and
/// network error strings) are interpolated into message bodies that also
/// contain trusted `<b>`/`<code>` structural tags. Without escaping, an
/// external string containing `<`, `>` or `&` (e.g. a TLS error
/// `peer's certificate contained <...>`, or a literal `a < b`) makes
/// Telegram's HTML parser return 400 Bad Request and the operator never
/// sees the alert.
///
/// Order matters: `&` is escaped FIRST so the `&` characters this function
/// introduces (`&lt;`/`&gt;`/`&amp;`) are not re-escaped. `"` is only
/// significant inside HTML attribute values, which these plain-text
/// messages never use, so it is intentionally left untouched.
///
/// Apply this at the render boundary in `to_message()` (the single place
/// strings become HTML) — NOT at the 130+ event emit sites. Trusted,
/// internally-constructed text (e.g. the operator-controlled
/// `Custom { message }` variant, which may intentionally carry `<b>`
/// formatting) is deliberately NOT routed through this helper.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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

/// Dispatch policy for a notification event — separates "urgency-of-display"
/// (the Severity color/tag) from "should-this-bypass-the-coalescer".
///
/// Pre-2026-05-09 the coalescer made the bypass decision purely from
/// `Severity` (Critical/High/Medium → bypass, Low/Info → coalesce). That
/// conflated TWO orthogonal axes:
///
/// * **Color** — operator's at-a-glance urgency cue. Boot-success events
///   ("✅ TickVault is live") want green = `Severity::Low`.
/// * **Routing** — should this message be sent immediately, or batched
///   into a 60-second coalesced summary. Boot-success events want
///   immediate dispatch so the operator sees the boot sequence in real
///   time, not a minute later.
///
/// PR #523 (2026-05-09 holiday-gate fix) bumped `AuthenticationSuccess` +
/// `InstrumentBuildSuccess` from `Low` → `Medium` to force immediate
/// dispatch. The side effect: those messages turned amber 🔵 even though
/// they were boot-success pings. Operator complained 2026-05-09:
/// "why MEDIUM when this is first fresh clone first fresh run app and
/// first fresh docker run, this should be low green right when this is
/// successful".
///
/// This enum is the fix: events declare their dispatch policy independently
/// of severity. Boot-success events render as `Severity::Low` (green ✅)
/// AND set `DispatchPolicy::Immediate` so they bypass the coalescer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchPolicy {
    /// Bypass the coalescer — dispatch immediately regardless of severity.
    /// Used for boot-success milestones and other events where freshness
    /// matters more than batching.
    Immediate,
    /// Use the severity-based default routing:
    /// `Critical/High/Medium` → immediate, `Low/Info` → coalesce.
    /// This is the catch-all for events that don't have a specific
    /// freshness requirement.
    Default,
}

/// Which boot path was used to bring the WebSocket pool online.
///
/// Per operator policy 2026-05-04 + `boot_helpers::should_fast_boot`:
/// - `Slow` is the DEFAULT path. Used pre-09:00 IST AND post-15:30 IST,
///   AND mid-market when the auth/instrument cache is missing. Runs the
///   full sequential init (auth → instrument master → QuestDB → universe
///   → WS pool).
/// - `Fast` is the EMERGENCY path. Used ONLY between 09:00–15:30 IST when
///   a valid auth token cache + binary instrument cache are present.
///   Skips ~30s of init by trusting the on-disk cache; intended for
///   mid-market crash recovery so ticks resume flowing fast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootPathLabel {
    Slow,
    Fast,
}

impl BootPathLabel {
    /// Human-readable label for the Telegram message.
    pub const fn human(self) -> &'static str {
        match self {
            Self::Slow => "Normal start (full init)",
            Self::Fast => "Mid-market crash recovery (cached init)",
        }
    }

    /// Short label for compact contexts.
    pub const fn short(self) -> &'static str {
        match self {
            Self::Slow => "slow",
            Self::Fast => "fast",
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

    /// WebSocket connection truly established — fires only when the
    /// connection's state in `pool.health()` has transitioned to
    /// `Connected` (TLS handshake done, subscribe acked, first frame
    /// received). The fields carry per-connection capacity utilisation
    /// and ping/pong heartbeat freshness so an operator sees subscription
    /// + liveness without opening Grafana.
    ///
    /// PR #458 (2026-05-04): the pre-existing payload was just
    /// `{ connection_index }` and was emitted from a tight loop right
    /// after `pool.spawn_handles()` returned — i.e., before any actual
    /// handshake had completed. That was a false-OK. The new payload
    /// + emit gating makes this a truthful per-connection signal.
    WebSocketConnected {
        connection_index: usize,
        subscribed_count: usize,
        capacity: usize,
        last_activity_secs_ago: Option<u32>,
    },

    /// Aggregate pool-online summary — fires on rising-edge when ALL
    /// spawned connections have actually reached `Connected` state.
    /// Carries the per-connection breakdown so the operator sees pool
    /// capacity utilisation + heartbeat liveness at a glance.
    ///
    /// `boot_path` distinguishes pre-9am normal slow boot from the
    /// mid-market crash-recovery fast boot (per operator intent
    /// 2026-05-04: slow boot is the default, fast boot is reserved for
    /// emergency restart between 09:00–15:30 IST when cache is valid).
    ///
    /// `per_connection[i] = (subscribed_count, capacity, last_activity_secs_ago)`
    /// indexed by `connection_index`. Length == `total`.
    WebSocketPoolOnline {
        connected: usize,
        total: usize,
        per_connection: Vec<(usize, usize, Option<u32>)>,
        boot_path: BootPathLabel,
        boot_wall_clock_secs: f64,
        /// Age (secs) of the most-recent REAL tick across all instruments,
        /// or `None` if zero real ticks captured yet. Sourced from
        /// `TickGapDetector::freshest_tick_age_secs` (real ticks only —
        /// never pings). Closes the 2026-06-02 false-OK where the per-feed
        /// "last update Xs ago" counted Dhan keep-alive pings and could
        /// read healthy while no real ticks were captured.
        last_real_tick_age_secs: Option<u32>,
        /// One status line per ENABLED market-data feed (Dhan, Groww, any
        /// future feed) — operator directive 2026-07-03: the "ready to
        /// trade" readiness message must name EVERY feed, not just the
        /// Dhan connection pool. Built at emit time from the same
        /// per-feed health registry the feed-control page reads.
        feeds: Vec<FeedStatusLine>,
    },

    /// Aggregate pool-degraded summary — fires when only a subset of
    /// connections reached `Connected` after the per-conn deadline.
    /// `stuck[i] = (connection_index, state_label)` describes each
    /// non-Connected slot. Replaces the pre-existing
    /// `WebSocketPoolDegraded { down_secs }` for the boot-time path
    /// (the watchdog still emits the pre-existing variant for
    /// mid-session pool-down events).
    WebSocketPoolPartialAfterDeadline {
        connected: usize,
        total: usize,
        per_connection: Vec<(usize, usize, Option<u32>)>,
        stuck: Vec<(usize, String)>,
        boot_path: BootPathLabel,
    },

    /// Off-hours boot-complete confirmation — fires when the per-conn
    /// deadline elapses outside `[09:00, 15:30) IST` and not all
    /// connections reached `Connected`. The non-connected slots are
    /// expected (`Deferred`) — Dhan resets idle pre-/post-market sockets,
    /// so the WebSocket pool intentionally waits until the next market
    /// open before opening any TCP connection. Severity::Low because
    /// this is by design, not a fault.
    ///
    /// 2026-05-09 (operator complaint): the pre-2026-05-09 path emitted
    /// `WebSocketPoolPartialAfterDeadline` (Severity::High) at every
    /// Saturday/Sunday/holiday boot, paging the operator with a false
    /// `[HIGH] 0/5 feeds connected` alarm despite the connections being
    /// correctly DEFERRED. This variant routes that scenario to a
    /// single Severity::Low ✅ ping instead.
    WebSocketPoolDeferredOffHours {
        deferred: usize,
        total: usize,
        boot_path: BootPathLabel,
        /// One status line per ENABLED market-data feed (operator directive
        /// 2026-07-04 — Groww boot-visibility parity: the off-hours "Boot
        /// complete" message previously counted only the Dhan connection
        /// pool, so a connected-and-subscribed Groww feed was invisible on a
        /// Saturday boot). Built from the same per-feed health registry the
        /// feed-control page reads.
        feeds: Vec<FeedStatusLine>,
    },

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

    /// Plan item #5 (2026-04-22): once-per-day positive confirmation that
    /// every feed is actually streaming live data at market open. Fires
    /// at 09:15:30 IST on each trading day. Answers Parthiban's "how do I
    /// know if connected" question directly — without this, operators only
    /// see disconnect/reconnect EDGES, never a positive "we are healthy"
    /// signal. Severity = Info so it never wakes anyone up.
    MarketOpenStreamingConfirmation {
        main_feed_active: usize,
        main_feed_total: usize,
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
        order_update_active: bool,
    },

    /// Audit Finding #5 (2026-05-03): pre-market positive-readiness ping.
    /// Fires once per trading day at 09:14:00 IST (exactly 1 minute
    /// before the NSE opening bell). Reports current subscription counts
    /// and token expiry headroom so the operator has a positive "we are
    /// READY for the open" signal, not just the existing 09:15:30
    /// post-open confirmation. Closes the false-OK gap from
    /// audit-findings-2026-04-17.md Rule 11. Severity = Info so it never
    /// pages (it is purely a positive signal). Edge-trigger: fires
    /// exactly once per trading day, never on mid-session boot past
    /// 09:14:00 IST.
    MarketOpenReadinessConfirmation {
        main_feed_active: usize,
        main_feed_total: usize,
        order_update_active: bool,
        token_remaining_secs: u64,
    },

    /// Phase 0 Item 22d (2026-05-15): once-per-trading-day end-of-day
    /// digest. Fires at 15:31:30 IST — 90s after the 15:30 close so the
    /// market-close shutdown signal has settled but before the operator
    /// goes to sleep. Reports the final state of the live feed
    /// connection + the JWT token headroom so the operator can answer
    /// two questions at a glance:
    ///
    ///   1. "Did the feed stay up through close?" — non-zero
    ///      `main_feed_active` confirms yes.
    ///   2. "Do I need to refresh TOTP before tomorrow?" — if
    ///      `token_remaining_hours < 12` the next session will boot
    ///      with stale credentials.
    ///
    /// Severity = Info so it never pages (it is a daily positive
    /// signal, not an alert). Edge-trigger: fires exactly once per
    /// trading day, never on mid-evening boot past 15:31:30 IST.
    EndOfDayDigest {
        /// Trading date in `YYYY-MM-DD` IST format. Stamped into the
        /// Telegram body so the operator can correlate against
        /// historical digests.
        trading_date_ist: String,
        /// Final main-feed connection count at 15:31:30 IST.
        main_feed_active: usize,
        /// Operator's expected total — same `effective_main_feed_pool_size`
        /// value as `MarketOpenReadinessConfirmation` for parity.
        main_feed_total: usize,
        /// JWT remaining lifetime in whole hours. The 24h SEBI cycle
        /// means anything < 12h after market close needs a TOTP-driven
        /// refresh before the next opening bell.
        token_remaining_hours: u64,
        /// One status line per ENABLED market-data feed at digest time
        /// (operator directive 2026-07-03: the digest previously reported
        /// only the Dhan connection count; Groww — running and subscribed —
        /// was invisible).
        feeds: Vec<FeedStatusLine>,
    },

    /// Post-market 1-minute cross-verify daily summary (operator directive
    /// 2026-06-10 — "make the 15:31 cross-verify VISIBLE"). Fires exactly
    /// once per trading day after `run_cross_verify_1m` completes, carrying
    /// the aggregated outcome the run previously only logged.
    ///
    /// Severity is data-dependent (hardened per the 2026-06-10 pre-impl
    /// adversarial review): `Info` ONLY when the run compared at least one
    /// minute AND found zero mismatches AND zero missing minutes AND was
    /// not degraded. A `compared == 0` run must NEVER read as PASS (audit
    /// Rule 11 false-OK class — Dhan serving empty 200-responses yields
    /// `compared = 0` with `fetch_failures = 0`), and `missing > 0` means
    /// minutes of live candles were missed — the very signal this audit
    /// exists for.
    CrossVerify1mSummary {
        /// Trading date in `YYYY-MM-DD` IST format.
        trading_date_ist: String,
        /// Spot instruments the run attempted to verify.
        instruments: usize,
        /// 1-minute buckets present on BOTH sides and compared exactly.
        compared: usize,
        /// OHLCV field-cells that disagreed with the exchange record.
        mismatches: usize,
        /// Minutes the exchange has that our live candles are missing.
        missing: usize,
        /// True when fetch coverage was partial (false-OK guard) — the
        /// run cannot vouch for the full universe.
        degraded: bool,
    },

    /// The post-market cross-verify TASK died (panicked) before producing
    /// its daily summary. High so the absence of the daily summary is
    /// impossible to miss (operator directive 2026-06-10). NOT fired on
    /// graceful shutdown/cancellation (the 16:30 IST auto-stop is a normal
    /// teardown, not an abort).
    CrossVerify1mAborted {
        /// Plain-English description of how the task died.
        detail: String,
    },

    // PR #4 (2026-05-19): DepthSpotPriceStale variant retired alongside
    // the deleted depth-20/200 infrastructure (operator lock 2026-05-15).
    // PR #5 (2026-05-19): 7 Phase2* variants retired alongside the
    // deleted Phase 2 stock-F&O dispatcher chain (operator lock 2026-05-15):
    // Phase2Started, Phase2RunImmediate, Phase2Complete, Phase2Failed,
    // Phase2Skipped, Phase2ReadinessPassed, Phase2ReadinessFailed.
    /// Wave 5 Item 26 L2 — NSE bhavcopy daily volume cross-check completed.
    /// Fires once at 16:30 IST after the post-market scheduler drains the
    /// bhavcopy diff into `volume_nse_audit`. Counts MUST sum to total
    /// rows compared. Severity::Info on green run; Severity::High when
    /// `mismatched > 0` (typed via the dispatcher per audit-findings
    /// Rule 11 — false-OK class bugs).
    // PR #6a (2026-05-19): NseBhavcopyCheckComplete + NseBhavcopyCheckFailed
    // RETIRED — bhavcopy 16:30 IST cross-check deleted under 4-IDX_I scope.

    // `MidMarketBootComplete` retired in PR #509c (Wave-5 §R.1) — the
    // 4-mode boot resolver was deleted under indices-only scope. Use
    // `MarketOpenStreamingConfirmation` for the once-per-day market-open
    // positive heartbeat.
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
    ///
    /// PR #790b (2026-05-25): now carries the disconnect-reason context so
    /// the operator sees WHY the disconnect happened + how long it was down +
    /// how many backoff attempts it took. All three diagnostic fields default
    /// gracefully (`reason=None`, `down_secs=0`, `attempts=0`) for callers
    /// that lack context (tests, manual probes).
    WebSocketReconnected {
        connection_index: usize,
        /// Reason recorded at the most recent matching disconnect site, or
        /// `None` if no disconnect was recorded (rare initial path).
        reason: Option<String>,
        /// Wall-clock seconds between disconnect and successful reconnect.
        /// `0` if no disconnect timestamp was recorded.
        down_secs: u64,
        /// Number of reconnect attempts (including this successful one) since
        /// the last disconnect. `0` if connect-on-first-try.
        attempts: u32,
    },

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

    /// Wave 3-D Item 13 — composite real-time guarantee score recovered
    /// to `Healthy` (≥ 0.95) on the falling edge from a degraded period.
    /// Severity::Info; edge-triggered (sustained-healthy ticks do NOT
    /// spam Telegram). Maps to ErrorCode `SLO-01`.
    RealtimeGuaranteeHealthy {
        /// Composite score in `[0.95, 1.0]`.
        score: f64,
    },

    /// Wave 3-D Item 13 — composite real-time guarantee score crossed
    /// below `0.95` (Degraded band `[0.80, 0.95)`). Severity::High.
    /// Edge-triggered on rising edge into a worse tier. Maps to
    /// ErrorCode `SLO-02`.
    RealtimeGuaranteeDegraded {
        /// Composite score in `[0.80, 0.95)`.
        score: f64,
        /// Static label of the lowest-input dimension (e.g. `"ws_health"`).
        weakest: &'static str,
    },

    /// Wave 3-D Item 13 — composite real-time guarantee score crossed
    /// below `0.80` (Critical). Severity::Critical so the operator pages
    /// immediately. Maps to ErrorCode `SLO-02`.
    RealtimeGuaranteeCritical {
        /// Composite score in `[0.0, 0.80)`.
        score: f64,
        /// Static label of the lowest-input dimension.
        weakest: &'static str,
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

    /// A market-data feed finished loading + resolving its instrument set
    /// (operator directive 2026-07-03 — Telegram feed parity: "whatever we
    /// have provided for dhan the same should be provided for groww also …
    /// instruments load message"). Mirrors `InstrumentBuildSuccess` (the
    /// Dhan universe-build ping) for every OTHER feed: fired once when a
    /// feed's watch-set resolves at boot/activation. Severity::Info.
    FeedInstrumentsLoaded {
        /// Feed display name (e.g. "Groww").
        feed: String,
        /// Instruments actually subscribed by this feed today.
        subscribed: usize,
        /// Index instruments inside `subscribed`.
        indices: usize,
        /// Stock instruments inside `subscribed`.
        stocks: usize,
        /// Instruments that could not be matched/resolved today (skipped —
        /// logged by name in the app log, counted here for the operator).
        skipped: usize,
    },

    /// A market-data feed's access token was read and accepted at
    /// boot/activation (operator directive 2026-07-04 — Groww boot-visibility
    /// parity: "i need the same view display everything even for groww").
    /// Mirrors Dhan's `AuthenticationSuccess` ("Auth OK") ping for every
    /// OTHER feed. Fired once per activation (once per boot when the feed is
    /// enabled at boot; again after a genuine disable → re-enable).
    /// Severity::Low, immediate dispatch (boot milestone).
    FeedAuthOk {
        /// Feed display name (e.g. "Groww").
        feed: String,
    },

    /// A market-data feed's socket connected + subscribe completed — BEFORE
    /// any tick has streamed (operator directive 2026-07-04 — Groww
    /// boot-visibility parity). On a closed market this is the ONLY honest
    /// "the feed is up" signal: the streaming confirmation (first tick) may
    /// be days away. The message text deliberately says "awaiting first
    /// tick" — socket-connected ≠ streaming, so this event NEVER claims
    /// ticks are flowing (audit Rule 11 — no false-OK). Edge-latched: once
    /// per connected episode, re-armed only on a genuine disconnect.
    FeedConnectedAwaitingTicks {
        /// Feed display name (e.g. "Groww").
        feed: String,
        /// Instruments the feed's subscribe completed with.
        subscribed: u64,
        /// Whether the market was open at connect time — flips the honest
        /// suffix between "market closed — idle is normal" and "market open
        /// — ticks should arrive shortly".
        market_open: bool,
    },

    /// Instrument build failed — includes manual trigger URL for retry.
    InstrumentBuildFailed {
        /// Error description.
        reason: String,
        /// URL for manual one-shot retry.
        manual_trigger_url: String,
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

    /// Phase 0 Item 18 — Dhan-side static-IP boot gate passed:
    /// `/v2/ip/getIP` reported `ordersAllowed = true` and `MATCH`.
    /// Fired once at boot. Severity::Low.
    StaticIpBootCheckPassed {
        /// Whitelisted IP slot Dhan returned (`PRIMARY` or `SECONDARY`).
        ip_flag: String,
    },

    /// Phase 0 Item 18 — Dhan-side static-IP boot gate FAILED.
    /// Boot must halt because exchange will reject orders from this IP.
    /// Severity::Critical.
    StaticIpBootCheckFailed {
        /// Stable wire-format reason (`empty_response`,
        /// `orders_not_allowed`, `match_status_not_ok`). Maps directly
        /// to `StaticIpBootFailureReason::as_str()`.
        reason: String,
        /// Whether Dhan reported `ordersAllowed = true`. Surfaced for
        /// operator triage clarity even though it's redundant with `reason`.
        orders_allowed: bool,
        /// Literal `ipMatchStatus` Dhan returned. Empty if response was
        /// empty.
        ip_match_status: String,
        /// Phase 0 Item 18b — how many attempts the retry loop made
        /// before declaring failure. `1` means immediate halt (e.g.
        /// `empty_response` did not warrant retry); `> 1` means we
        /// waited through the propagation window and Dhan still
        /// reported denied.
        attempts_made: u32,
    },

    /// Phase 0 Item 18b — boot-time static-IP gate is retrying because
    /// Dhan returned `orders_allowed = false`. Bucket-coalesced
    /// `Severity::Low` so the operator sees occasional progress
    /// without being paged.
    StaticIpBootCheckRetrying {
        /// 1-indexed current attempt number.
        attempt: u32,
        /// Total attempts the boot loop will make before halting.
        max_attempts: u32,
    },

    /// Phase 0 Item 19 — another `tickvault` process holds the SSM
    /// dual-instance lock for this client-id at boot time. Two processes
    /// running against the same Dhan account fight over static-IP
    /// enforcement and fragment the WebSocket connection budget — boot
    /// must HALT. The lock has a 90s TTL so a hard-crashed peer's lock
    /// expires automatically; this event fires only when a LIVE peer is
    /// holding the lock. Severity::Critical.
    DualInstanceDetected {
        /// Best-effort holder identity (e.g.,
        /// `i-0123abc:42:deadbeefcafef00d` or `local:42:...`). Empty
        /// string is a valid value: the SSM lock read may race
        /// with the other instance's release between our compare-and-set
        /// and the diagnostic read. The boot-halt decision is correct
        /// regardless; operator uses `make doctor` to inspect the SSM
        /// lock and identify the winner in that case.
        holder: String,
        /// The env-qualified lock key (e.g. `tickvault:instance:lock:prod`).
        /// Surfaced so the operator can run `make doctor` to inspect the
        /// SSM lock without needing to know the key construction rule.
        lock_key: String,
    },

    /// Boot health check completed — infrastructure services verified.
    BootHealthCheck {
        /// Number of services that passed health check.
        services_healthy: usize,
        /// Total services checked.
        services_total: usize,
    },

    /// Phase 0 Item 20 — orphan position 15:25 IST watchdog observed
    /// one or more open positions (`net_qty != 0`) at 15:25:00 IST.
    /// Strategy is intraday option-buying — overnight positions are
    /// NOT allowed. Severity::Critical. In Phase 0 (dry-run) this is
    /// the terminal signal; operator MUST manually exit before 15:30.
    /// In Phase 1+ the watchdog auto-exits and this Telegram is
    /// supplemented by `OrphanPositionAutoClosed` / `OrphanPositionExitFailed`.
    OrphanPositionDetected {
        /// Count of open positions found.
        count: usize,
        /// Sum of `|net_qty|` across all orphans.
        total_abs_net_qty: i64,
        /// Up to 5 sample trading symbols for the Telegram body.
        /// Full list lives in the `orphan_position_audit` QuestDB
        /// table — operator queries `SELECT * FROM orphan_position_audit
        /// WHERE trading_date_ist = today()`.
        sample_symbols: Vec<String>,
        /// Whether `[strategy] dry_run = true` was set when the
        /// watchdog ran. In Phase 0 this is always `true` and the
        /// Telegram body warns the operator to exit manually.
        dry_run: bool,
    },

    /// Phase 0 Item 20 — watchdog confirms account is flat at 15:25
    /// IST. Severity::Info. Positive-ping per audit-findings Rule 11
    /// — without this the operator can't tell whether the watchdog
    /// ran (the absence of `OrphanPositionDetected` is ambiguous).
    OrphanPositionsClean,

    /// Phase 0 Items 15+28+29 — 09:16:05 IST post-open cross-check
    /// found mismatched 09:15 bar(s) vs Dhan REST `/v2/charts/intraday`
    /// and corrected them. Authoritative Dhan values written to
    /// `candles_1m_shadow` + `historical_candles` (mirror) +
    /// `bar_correction_audit`. Strategy gate has flipped to OPEN.
    /// Severity::Critical so operator sees the corrections before
    /// trading. Coalesced — ONE event per 09:16:05 run regardless of
    /// how many SIDs were corrected.
    BarMismatchCorrectedFromHistorical {
        /// Total bars compared this pass (out of 222 universe).
        compared_count: usize,
        /// Bars that disagreed and were corrected.
        mismatches_count: usize,
        /// Up to 10 sample mismatches for the Telegram body
        /// (`field_label`, `local_value`, `dhan_value`, `trading_symbol`).
        /// Full list lives in `bar_correction_audit` for operator query.
        sample_symbols: Vec<String>,
        /// Cross-check pass label — `post_open_09_16_05` or
        /// `boot_catch_up` for mid-day startup catch-up runs.
        cross_check_pass: &'static str,
    },

    /// Phase 0 Items 15+28+29 — 09:16:05 IST cross-check INCONCLUSIVE.
    /// Dhan REST returned valid data for fewer than 200/222 SIDs.
    /// Strategy gate stays CLOSED for the trading day. Severity::Critical.
    BarMismatchCrossCheckInconclusive {
        compared_count: usize,
        expected_count: usize,
    },

    /// Phase 0 Items 15+28+29 — 09:16:05 IST cross-check HARD-FAILED.
    /// All 222 Dhan REST fetches errored. Strategy gate stays CLOSED.
    /// Severity::Critical. Operator must diagnose Dhan REST health.
    BarMismatchCrossCheckFailed {
        /// Typed failure reason from `CrossCheckOutcome::Failed`.
        reason: String,
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

    // RETIRED 2026-06-12: TokenRenewalDeadlineMissed deleted — it was defined
    // but NEVER emitted (0 production sites) and is redundant with the live
    // `TokenRenewalFailed` Critical path in token_manager::renewal_loop, which
    // already pages on renewal exhaustion + circuit-breaker halt. Wiring a
    // second Critical alert for the same event would double-page the operator
    // (violates the anti-spam rule). See session 2026-06-12.
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

    /// QuestDB persistence reconnected — buffered data draining. Emitted on
    /// the recovery edge of a `QuestDbDisconnected` incident (symmetric with
    /// the Critical disconnect alert, so the operator is told the incident
    /// resolved — audit-findings Rule 11, no false-OK / anxiety gap).
    QuestDbReconnected {
        /// Which writer recovered.
        writer: String,
        /// Consecutive failed liveness checks observed before recovery
        /// (≈ downtime in liveness-poll intervals). The drained-record
        /// count is NOT knowable at the liveness-loop site (it lives in
        /// the tick-persistence rescue ring), so this carries the honest,
        /// available downtime signal rather than a hardcoded zero.
        failed_checks_before_recovery: u32,
    },

    /// The Groww Python sidecar printed a diagnostic line classifying as a
    /// genuine auth / entitlement / error reject (e.g. its watchdog's
    /// "account lacks a live market-data feed entitlement" line, or a
    /// "sidecar error [auth]" line). Previously these lines reached only the
    /// container logs because the supervisor inherited the child's stdio — so
    /// the operator was blind to WHY Groww streamed 0 ticks. The supervisor now
    /// fires this ONCE per running-child reject (edge-triggered) with a fixed
    /// plain-English `reason` per class (never the raw child text — defense in
    /// depth so no runtime/credential data reaches Telegram). Severity::High.
    GrowwSidecarRejected { reason: String },

    /// Custom alert from any component.
    Custom { message: String },
    // RETIRED 2026-06-12: LastTickAfterBoundary deleted — it was defined but
    // NEVER emitted (0 production sites). The post-close-tick anomaly it
    // describes is ALREADY tracked by the `tv_late_tick_after_boundary_total`
    // counter in tick_processor.rs (ratchet-tested), which increments on every
    // tick stamped >= 15:30:00 IST. Wiring the Telegram form would require
    // threading a NotificationService into the per-tick HOT PATH
    // (`run_tick_processor` has no notifier) for an Info-severity event that is
    // already counted — a hot-path risk for marginal value. The correct,
    // hot-path-safe alert is a CloudWatch alarm on the existing counter.

    // RETIRED 2026-06-28: OptionChainFetchFailed / OptionChainCacheFallback /
    // OptionChainStaleHalt / OptionChainConfigInvalid deleted with the entire
    // option_chain REST subsystem (operator directive 2026-06-28 — "drop the
    // option chain entire implementations and its table also"). The subsystem
    // was disabled since 2026-06-02 with no live consumer.
}

/// Formats a `usize` with comma thousand separators for human-readable
/// Telegram messages (e.g. `24324` -> `"24,324"`).
fn format_with_commas(n: usize) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, byte) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*byte as char);
    }
    out
}

/// Formats the per-connection ping/pong heartbeat freshness for the
/// `WebSocketConnected` Telegram payload.
///
/// `last_activity_secs_ago` is `None` when no inbound frame has been
/// seen yet on this socket — render an explicit "no data yet" message
/// rather than a misleading "0s ago".
fn format_ping_freshness(secs: Option<u32>) -> String {
    match secs {
        None => "—".to_string(),
        Some(0..=10) => format!("{}s ago ✓", secs.unwrap_or(0)),
        // 11–30s: still alive (Dhan pings @10s; library auto-pongs)
        Some(s @ 11..=30) => format!("{s}s ago ⚠"),
        // > 30s: ping/pong watchdog will fire shortly
        Some(s) => format!("{s}s ago ❌"),
    }
}

/// One operator-facing status line for a market-data feed (Dhan, Groww, any
/// future feed) inside the readiness + end-of-day Telegram messages.
///
/// Honest by construction: `None` fields render as "unknown" / "no tick seen
/// yet" — the renderer never invents numbers for a feed whose health signals
/// are not wired (operator directive 2026-07-03 + audit Rule 11 mirror).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedStatusLine {
    /// Feed display name (e.g. "Dhan", "Groww").
    pub name: String,
    /// Instruments this feed subscribed today; `None` = unknown.
    pub instruments: Option<u64>,
    /// Seconds since this feed's most recent tick; `None` = no tick seen yet.
    pub last_tick_age_secs: Option<u64>,
}

/// Renders the per-feed status block shared by the readiness ("ready to
/// trade") and end-of-day digest messages. One line per enabled feed:
///
/// ```text
/// Market-data feeds:
///   Dhan: 776 instruments — last tick 0s ago ✓
///   Groww: 768 instruments — last tick 2s ago ✓
/// ```
///
/// Freshness wording: ≤30s gets a ✓; 31–120s renders plain seconds; older
/// ages render minutes with NO fault emoji (post-close silence is normal —
/// the digest must not read as a false red). Unknown data says "unknown" /
/// "no tick seen yet" — never a fabricated number. An empty list (no feed
/// data available) renders "unknown" so the block can never silently vanish.
fn format_feed_status_block(feeds: &[FeedStatusLine]) -> String {
    if feeds.is_empty() {
        return "Market-data feeds: unknown\n".to_string();
    }
    let mut out = String::from("Market-data feeds:\n");
    for feed in feeds {
        let instruments = match feed.instruments {
            Some(n) => format!("{} instruments", format_with_commas(n as usize)),
            None => "instruments unknown".to_string(),
        };
        let tick = match feed.last_tick_age_secs {
            None => "no tick seen yet".to_string(),
            Some(age @ 0..=30) => format!("last tick {age}s ago ✓"),
            Some(age @ 31..=120) => format!("last tick {age}s ago"),
            Some(age) => format!("last tick {}m ago", age / 60),
        };
        out.push_str(&format!(
            "  {name}: {instruments} — {tick}\n",
            name = feed.name
        ));
    }
    out
}

/// Renders the WebSocket pool-online (HEALTHY) Telegram message in the
/// Tier-2 human-readable format. Single message at boot — eye instantly
/// catches the verdict, then per-feed breakdown for at-a-glance status.
fn format_pool_online_message(
    connected: usize,
    total: usize,
    per_connection: &[(usize, usize, Option<u32>)],
    boot_path: BootPathLabel,
    boot_wall_clock_secs: f64,
    last_real_tick_age_secs: Option<u32>,
    feeds: &[FeedStatusLine],
) -> String {
    let total_subscribed: usize = per_connection.iter().map(|(s, _, _)| *s).sum();
    let total_capacity: usize = per_connection.iter().map(|(_, c, _)| *c).sum();
    let pool_pct = if total_capacity == 0 {
        0.0
    } else {
        (total_subscribed as f64 / total_capacity as f64) * 100.0
    };
    let mut out = String::new();
    out.push_str("<b>✅ TickVault is live and ready to trade</b>\n\n");
    // "Dhan:" prefix (2026-07-03 feed parity): this count is the Dhan
    // connection pool ONLY — the per-feed block below covers every enabled
    // market-data feed (Dhan, Groww, future), so the header must not claim
    // to be the full feed count.
    out.push_str(&format!(
        "📡 Dhan: All {connected} of {total} connections up\n"
    ));
    out.push_str(&format!(
        "📊 Tracking {sub} instruments out of {cap} max ({pct:.0}% full)\n\n",
        sub = format_with_commas(total_subscribed),
        cap = format_with_commas(total_capacity),
        pct = pool_pct,
    ));
    // Real-tick freshness (NOT frame freshness). The per-feed "last update"
    // line below counts ANY frame incl. Dhan keep-alive pings, so it can read
    // "0s ago" while zero real ticks are captured. This line reports the
    // most-recent GENUINE tick so "live" can never look green on a ping-only
    // feed (2026-06-02 false-OK fix).
    match last_real_tick_age_secs {
        Some(age) => out.push_str(&format!("📈 Real market ticks: last one {age}s ago\n\n")),
        None => out.push_str(
            "⚠️ Real market ticks: NONE captured yet — feed may be sending only \
             keep-alive pings. If this persists after market open, the feed is \
             connected but not delivering data.\n\n",
        ),
    }
    out.push_str("Dhan connections:\n");
    for (idx, (sub, cap, last)) in per_connection.iter().enumerate() {
        let display = idx.saturating_add(1);
        let pct = if *cap == 0 {
            0.0
        } else {
            (*sub as f64 / *cap as f64) * 100.0
        };
        let pct_label = if (pct - 100.0).abs() < f64::EPSILON {
            "full".to_string()
        } else {
            format!("{pct:.0}% full")
        };
        let ping = format_ping_freshness(*last);
        out.push_str(&format!(
            "  Connection {display}: tracking {sub_fmt} instruments ({pct_label}) — last update {ping}\n",
            sub_fmt = format_with_commas(*sub),
        ));
    }
    // 2026-07-03 feed parity: one line per ENABLED market-data feed —
    // Groww (and any future feed) appears alongside Dhan, or the operator
    // cannot know the second feed came up.
    out.push('\n');
    out.push_str(&format_feed_status_block(feeds));
    out.push_str("\n💚 Heartbeat healthy on all feeds (Dhan pings every 10s, auto-pong)\n");
    out.push_str(&format!(
        "⏱️ Boot took {boot_wall_clock_secs:.1} seconds — {path}\n",
        path = boot_path.human(),
    ));
    out.push_str("\nYou're good to go.");
    out
}

/// Renders the WebSocket pool-degraded Telegram message in the Tier-2
/// human-readable format. Includes per-slot stuck reason + retry plan.
fn format_pool_partial_message(
    connected: usize,
    total: usize,
    per_connection: &[(usize, usize, Option<u32>)],
    stuck: &[(usize, String)],
    boot_path: BootPathLabel,
) -> String {
    let total_subscribed: usize = per_connection.iter().map(|(s, _, _)| *s).sum();
    let total_capacity: usize = per_connection.iter().map(|(_, c, _)| *c).sum();
    let pool_pct = if total_capacity == 0 {
        0.0
    } else {
        (total_subscribed as f64 / total_capacity as f64) * 100.0
    };
    let stuck_count = stuck.len();
    let mut out = String::new();
    out.push_str("<b>⚠️ TickVault is partially online — needs your attention</b>\n\n");
    out.push_str(&format!(
        "📡 {connected} of {total} market-data feeds connected ({stuck_count} stuck)\n"
    ));
    out.push_str(&format!(
        "📊 Tracking {sub} instruments out of {cap} max ({pct:.0}%)\n\n",
        sub = format_with_commas(total_subscribed),
        cap = format_with_commas(total_capacity),
        pct = pool_pct,
    ));
    out.push_str("Working feeds:\n");
    let stuck_idxs: std::collections::HashSet<usize> = stuck.iter().map(|(i, _)| *i).collect();
    for (idx, (sub, cap, last)) in per_connection.iter().enumerate() {
        if stuck_idxs.contains(&idx) {
            continue;
        }
        let display = idx.saturating_add(1);
        let pct = if *cap == 0 {
            0.0
        } else {
            (*sub as f64 / *cap as f64) * 100.0
        };
        let ping = format_ping_freshness(*last);
        out.push_str(&format!(
            "  Feed {display}: tracking {sub_fmt} instruments ({pct:.0}% full) — last update {ping}\n",
            sub_fmt = format_with_commas(*sub),
        ));
    }
    out.push_str("\nBroken feeds:\n");
    for (idx, reason) in stuck {
        let display = idx.saturating_add(1);
        out.push_str(&format!("  Feed {display}: {}\n", html_escape(reason)));
    }
    out.push_str(&format!(
        "\n🔄 The system will keep retrying every 5 seconds.\n   Boot path: {path}",
        path = boot_path.human(),
    ));
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
                // Dashboards line trimmed in #O1/#O2/#O3/#O4 — Grafana,
                // Prometheus, Alertmanager, Valkey all removed in the
                // CloudWatch-only migration. Operator-facing observability
                // is now the QuestDB Console (local dev) + CloudWatch
                // Dashboards (AWS prod).
                // B9 deploy provenance: the `Build:` short-SHA line is a
                // conscious operator-approved override of the "no version
                // numbers in body" Telegram commandment (B9 directive
                // 2026-07-03) — it answers "WHICH code booted?" at a glance.
                format!(
                    "<b>tickvault started</b>\nMode: {mode}\nBuild: {build}\n\n\
                     Dashboards: QuestDB Console (local) / CloudWatch (prod)",
                    build = tickvault_common::build_info::build_git_sha_short(),
                )
            }
            Self::AuthenticationSuccess => "<b>Auth OK</b> — Dhan JWT acquired".to_string(),
            Self::AuthenticationFailed { reason } => {
                format!(
                    "<b>Auth FAILED</b> — offline mode\n{}",
                    html_escape(&redact_url_params(reason))
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
                    reason = html_escape(&redact_url_params(reason)),
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
                let reason = html_escape(reason);
                format!(
                    "{header}\n{reason}\n\
                     Run:\n  curl -H \"access-token: $TOKEN\" https://api.dhan.co/v2/profile\n\
                     Check: dataPlan == \"Active\", activeSegment contains \"Derivative\", tokenValidity > 4h."
                )
            }
            Self::MidSessionProfileInvalidated { reason } => {
                let reason = html_escape(reason);
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
                    html_escape(&redact_url_params(reason))
                )
            }
            Self::WebSocketConnected {
                connection_index,
                subscribed_count,
                capacity,
                last_activity_secs_ago,
            } => {
                // 1-indexed for human readability (Feed 1..N).
                let display = connection_index.saturating_add(1);
                // PR #790a (2026-05-25): under indices_4_only scope lock the
                // effective main-feed pool is 1, not 5 (the Dhan account-wide
                // cap). Displaying "1/5" misleads the operator into thinking
                // 4 connections are silently down. Use the scope-locked
                // constant.
                let total = tickvault_common::constants::PHASE_0_MAIN_FEED_CONNECTION_COUNT;
                let pct = if *capacity == 0 {
                    0.0
                } else {
                    (*subscribed_count as f64 / *capacity as f64) * 100.0
                };
                let ping = format_ping_freshness(*last_activity_secs_ago);
                format!(
                    "<b>📡 Feed {display}/{total} is now live</b>\n  \
                     Tracking {sub} of {cap} instruments ({pct:.0}% full)\n  \
                     Last update {ping}",
                    sub = format_with_commas(*subscribed_count),
                    cap = format_with_commas(*capacity),
                )
            }
            Self::WebSocketPoolOnline {
                connected,
                total,
                per_connection,
                boot_path,
                boot_wall_clock_secs,
                last_real_tick_age_secs,
                feeds,
            } => format_pool_online_message(
                *connected,
                *total,
                per_connection,
                *boot_path,
                *boot_wall_clock_secs,
                *last_real_tick_age_secs,
                feeds,
            ),
            Self::WebSocketPoolPartialAfterDeadline {
                connected,
                total,
                per_connection,
                stuck,
                boot_path,
            } => format_pool_partial_message(*connected, *total, per_connection, stuck, *boot_path),
            Self::WebSocketPoolDeferredOffHours {
                deferred,
                total,
                boot_path,
                feeds,
            } => format!(
                "<b>✅ Boot complete — {deferred}/{total} Dhan connections DEFERRED until 09:00 IST</b>\n\n\
                 📡 Outside [09:00, 15:30) IST. Dhan resets idle pre-/post-market sockets, \
                 so the WebSocket pool intentionally waits for the next market open before \
                 opening any TCP connection.\n\n\
                 {feed_block}\n  \
                 Boot path: {path}",
                feed_block = format_feed_status_block(feeds),
                path = boot_path.human(),
            ),
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
                order_update_active,
            } => {
                let oms = if *order_update_active { "1/1" } else { "0/1" };
                format!(
                    "<b>Streaming live @ 09:15:30 IST</b>\n\
                     Main feed: {main_feed_active}/{main_feed_total}\n\
                     Order updates: {oms}"
                )
            }
            Self::MarketOpenStreamingFailed {
                main_feed_active,
                main_feed_total,
                order_update_active,
            } => {
                let oms = if *order_update_active { "1/1" } else { "0/1" };
                format!(
                    "<b>MARKET OPEN STREAMING FAILED @ 09:15:30 IST</b>\n\
                     Main feed: {main_feed_active}/{main_feed_total} — NO CONNECTIONS\n\
                     Order updates: {oms}\n\
                     Action: check pool watchdog, token validity, Dhan status."
                )
            }
            Self::MarketOpenReadinessConfirmation {
                main_feed_active,
                main_feed_total,
                order_update_active,
                token_remaining_secs,
            } => {
                let oms = if *order_update_active { "1/1" } else { "0/1" };
                let token_hours = *token_remaining_secs as f64 / 3600.0;
                format!(
                    "<b>READY for market open @ 09:14:00 IST</b>\n\
                     Main feed: {main_feed_active}/{main_feed_total}\n\
                     Order updates: {oms}\n\
                     Token headroom: {token_hours:.1}h\n\
                     Bell rings in 60s."
                )
            }
            Self::EndOfDayDigest {
                trading_date_ist,
                main_feed_active,
                main_feed_total,
                token_remaining_hours,
                feeds,
            } => {
                // Operator-facing wording per operator-charter §G: no
                // library names, no file paths, plain English action
                // when the token headroom is low.
                let token_warning = if *token_remaining_hours < 12 {
                    "\nToken expires before tomorrow's open — refresh TOTP before 09:00 IST."
                } else {
                    ""
                };
                let close_status = if *main_feed_active > 0 {
                    "Feed stayed up through close."
                } else {
                    "Feed was disconnected at close — check overnight logs."
                };
                // 2026-07-03 feed parity: the digest names EVERY enabled
                // market-data feed, not just the Dhan connection count.
                let feed_block = format_feed_status_block(feeds);
                format!(
                    "<b>End-of-day digest @ 15:31:30 IST</b>\n\
                     Trading date: {trading_date_ist}\n\
                     Main feed: {main_feed_active}/{main_feed_total}\n\
                     Token headroom: {token_remaining_hours}h\n\
                     {feed_block}\
                     {close_status}{token_warning}"
                )
            }
            Self::CrossVerify1mSummary {
                trading_date_ist,
                instruments,
                compared,
                mismatches,
                missing,
                degraded,
            } => {
                // Operator-charter §G wording: plain English, IST 12-hour
                // time, real numbers, no library names, no file paths
                // (points at the portal card, not a path).
                let clean = *mismatches == 0 && *missing == 0 && !*degraded && *compared > 0;
                if clean {
                    format!(
                        "\u{2705} <b>Daily candle check @ 3:31 PM IST — PASS</b>\n\
                         Date: {trading_date_ist}\n\
                         Instruments: {instruments} | Minutes compared: {compared}\n\
                         Mismatches: 0 | Missing: 0\n\
                         Every 1-minute candle matches the exchange record exactly."
                    )
                } else if *compared == 0 {
                    // DHAN-REST-400 (2026-06-10): the BLIND day. 776/776
                    // fetch failures produced a header-only report file that
                    // LOOKED like a perfect day. Say BLIND loudly — an empty
                    // mismatch list must never read as a pass.
                    format!(
                        "\u{1f198} <b>Daily candle check @ 3:31 PM IST — BLIND</b>\n\
                         Date: {trading_date_ist}\n\
                         Checked NOTHING today. \
                         Nothing could be compared — today's check cannot \
                         vouch for the day's candles.\n\
                         The day's report is empty because every data request \
                         failed or returned nothing, NOT because the data was \
                         perfect.\n\
                         Instruments: {instruments} | Minutes compared: 0\n\
                         What to do RIGHT NOW:\n\
                         1. Check the REST health alerts for why requests failed.\n\
                         2. Open the Cross-verify card on the operator portal.\n\
                         3. Confirm the live feed itself stayed healthy today."
                    )
                } else {
                    let coverage_note = if *degraded {
                        "\nCoverage was PARTIAL — some instrument data could \
                         not be fetched, so today's check cannot vouch for \
                         the full universe."
                    } else {
                        ""
                    };
                    format!(
                        "\u{26a0}\u{fe0f} <b>Daily candle check @ 3:31 PM IST — \
                         NEEDS ATTENTION</b>\n\
                         Date: {trading_date_ist}\n\
                         Instruments: {instruments} | Minutes compared: {compared}\n\
                         Mismatches: {mismatches} | Missing: {missing}\
                         {coverage_note}\n\
                         What to do RIGHT NOW:\n\
                         1. Open the Cross-verify card on the operator portal.\n\
                         2. Review which minutes differ from the exchange record.\n\
                         3. If many instruments differ, check the day's feed health."
                    )
                }
            }
            Self::CrossVerify1mAborted { detail } => {
                let detail = html_escape(detail);
                format!(
                    "\u{26a0}\u{fe0f} <b>Daily candle check did NOT run</b>\n\
                     The 3:31 PM IST check against the exchange record died \
                     before finishing.\n\
                     Reason: {detail}\n\
                     What to do RIGHT NOW:\n\
                     1. Check the app is still running.\n\
                     2. Restart the app to re-arm tomorrow's check."
                )
            }
            // PR #4/#5 (2026-05-19): DepthSpotPriceStale + 7 Phase2*
            // Display arms retired with their variants.
            // PR #6a (2026-05-19): NseBhavcopyCheck* Display arms retired.
            Self::WebSocketDisconnected {
                connection_index,
                reason,
            } => {
                // PR #790a — use scope-locked pool size (1), not Dhan cap (5).
                // 2026-06-12 — classify the likely SOURCE (Dhan / network / token)
                // from the raw reason so the operator knows WHO caused it. The raw
                // error is always shown above, so a wrong guess never hides truth.
                let cause =
                    tickvault_common::disconnect_cause::classify_disconnect_cause(reason, None);
                // SECURITY (review 2026-06-12, HIGH): a WS connect error can embed
                // the feed URL with the token query-param — redact before display,
                // matching the auth-event arms.
                let reason = html_escape(&redact_url_params(reason));
                format!(
                    "<b>WebSocket {}/{} disconnected</b>\n{reason}\nLikely source: {} — {}\nConfirm: {}",
                    connection_index.saturating_add(1),
                    tickvault_common::constants::PHASE_0_MAIN_FEED_CONNECTION_COUNT,
                    cause.label(),
                    cause.explanation(),
                    cause.confirm_hint()
                )
            }
            Self::WebSocketDisconnectedOffHours {
                connection_index,
                reason,
            } => {
                // PR #790a — use scope-locked pool size (1), not Dhan cap (5).
                // SECURITY (review 2026-06-12, HIGH): redact token-in-URL before display.
                let reason = html_escape(&redact_url_params(reason));
                format!(
                    "<b>WebSocket {}/{} disconnected [off-hours, auto-reconnecting]</b>\n{reason}",
                    connection_index.saturating_add(1),
                    tickvault_common::constants::PHASE_0_MAIN_FEED_CONNECTION_COUNT
                )
            }
            Self::WebSocketReconnected {
                connection_index,
                reason,
                down_secs,
                attempts,
            } => {
                // PR #790a — use scope-locked pool size (1), not Dhan cap (5).
                // PR #790b — append disconnect-reason context so the operator
                // sees WHY + how-long + how-many-attempts in the same message.
                let pool = tickvault_common::constants::PHASE_0_MAIN_FEED_CONNECTION_COUNT;
                let header = format!(
                    "<b>WebSocket {}/{} reconnected</b>",
                    connection_index.saturating_add(1),
                    pool
                );
                // Only render the diagnostic line when we actually have
                // context (the cold path always sets reason; tests + initial
                // probes may not).
                // 2026-06-12 — classify the likely SOURCE from the raw reason
                // (the recovery message keeps it to label + explanation; the
                // full confirm-hint lives on the disconnect message).
                let source = reason
                    .as_deref()
                    .map(|r| {
                        let c =
                            tickvault_common::disconnect_cause::classify_disconnect_cause(r, None);
                        format!("\nLikely source: {} — {}", c.label(), c.explanation())
                    })
                    .unwrap_or_default();
                // SECURITY (review 2026-06-12, HIGH): redact token-in-URL before display.
                let reason = reason
                    .as_deref()
                    .map(|r| html_escape(&redact_url_params(r)));
                match (reason.as_deref(), *down_secs, *attempts) {
                    (Some(r), d, a) if d > 0 || a > 0 => {
                        format!("{header}\nReason: {r}{source}\nDown: {d}s · Attempts: {a}")
                    }
                    (Some(r), _, _) => format!("{header}\nReason: {r}{source}"),
                    (None, d, a) if d > 0 || a > 0 => {
                        format!("{header}\nDown: {d}s · Attempts: {a}")
                    }
                    (None, _, _) => header,
                }
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
                format!(
                    "<b>Order Update WS DISCONNECTED</b>\n{}",
                    html_escape(reason)
                )
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
            Self::FeedInstrumentsLoaded {
                feed,
                subscribed,
                indices,
                stocks,
                skipped,
            } => {
                // Operator-charter §G wording: plain English, provider name
                // only, real numbers, one glance = "the second feed loaded
                // its instruments and subscribed them".
                let skipped_line = if *skipped > 0 {
                    format!("\nSkipped {skipped} that could not be matched today")
                } else {
                    "\nEvery instrument matched".to_string()
                };
                format!(
                    "<b>✅ {feed_name} instruments loaded</b>\n\
                     Subscribed {sub} instruments ({indices} indices + {stocks} stocks)\
                     {skipped_line}",
                    feed_name = html_escape(feed),
                    sub = format_with_commas(*subscribed),
                )
            }
            Self::FeedAuthOk { feed } => {
                // Groww boot-visibility parity (operator 2026-07-04): the
                // exact mirror of Dhan's "Auth OK" boot ping — access token
                // in hand, feed can connect. Plain English, provider name
                // only (charter §D).
                format!(
                    "✅ <b>{feed_name} Auth OK</b> — access token in hand, feed can connect",
                    feed_name = html_escape(feed),
                )
            }
            Self::FeedConnectedAwaitingTicks {
                feed,
                subscribed,
                market_open,
            } => {
                // HONEST wording (no false-OK): socket-connected ≠ streaming.
                // This message must ALWAYS say "awaiting first tick" — the
                // streaming confirmation is a separate event that fires only
                // when a real tick is observed.
                let suffix = if *market_open {
                    "market open — ticks should arrive shortly"
                } else {
                    "market closed — idle is normal"
                };
                format!(
                    "✅ <b>{feed_name} feed connected</b>\n\
                     Subscribed {sub} instruments — awaiting first tick ({suffix})",
                    feed_name = html_escape(feed),
                    sub = format_with_commas(*subscribed as usize),
                )
            }
            Self::InstrumentBuildFailed {
                reason,
                manual_trigger_url,
            } => {
                // SECURITY: Do not expose internal API URL in Telegram.
                let _ = manual_trigger_url; // kept in struct for internal use
                let reason = html_escape(reason);
                format!(
                    "<b>Instruments FAILED</b>\n{reason}\n\nRetry via /instruments/rebuild API endpoint"
                )
            }
            Self::IpVerificationFailed { reason } => {
                let reason = html_escape(reason);
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
            Self::StaticIpBootCheckPassed { ip_flag } => {
                format!(
                    "<b>Static IP boot check passed</b>\nDhan reports orders allowed from this IP ({ip_flag} slot)."
                )
            }
            Self::StaticIpBootCheckFailed {
                reason,
                orders_allowed,
                ip_match_status,
                attempts_made,
            } => {
                let plain_reason = match reason.as_str() {
                    "empty_response" => {
                        "Dhan returned an empty IP response. Check your account on the Dhan portal."
                    }
                    "orders_not_allowed" => {
                        "Dhan marked this IP as NOT ALLOWED for orders. Set or refresh the static IP on web.dhan.co."
                    }
                    "match_status_not_ok" => {
                        "Dhan's IP match check did not return MATCH. Confirm the registered IP on the Dhan portal."
                    }
                    _ => "Static IP boot check failed.",
                };
                let attempt_line = if *attempts_made > 1 {
                    format!(
                        "\nWaited through {attempts_made} retry attempts (~{} minutes) before giving up.",
                        attempts_made.saturating_sub(1)
                    )
                } else {
                    String::new()
                };
                // `ip_match_status` is a Dhan-returned string — escape it.
                let ip_match_status = html_escape(ip_match_status);
                format!(
                    "<b>STATIC IP BOOT CHECK FAILED</b>\n{plain_reason}{attempt_line}\n\nDhan reply: ordersAllowed={orders_allowed}, ipMatchStatus=\"{ip_match_status}\"\n\nBoot blocked — no orders will be placed until this is fixed."
                )
            }
            Self::StaticIpBootCheckRetrying {
                attempt,
                max_attempts,
            } => {
                format!(
                    "<b>Static IP check waiting</b>\nDhan still reports this IP as not allowed for orders. Retry {attempt} of {max_attempts} (every minute)."
                )
            }
            Self::DualInstanceDetected { holder, lock_key } => {
                // `holder` is read from the Valkey/SSM instance lock (set by a
                // previous process) and `lock_key` is the lock key string —
                // both external-origin, so escape before HTML interpolation.
                let holder_line = if holder.is_empty() {
                    "(holder identity not retrievable — the lock check raced; run `make doctor`)"
                        .to_string()
                } else {
                    format!("Live peer: {}", html_escape(holder))
                };
                let lock_key = html_escape(lock_key);
                format!(
                    "<b>DUAL-INSTANCE DETECTED</b>\nAnother tickvault process is already running for this Dhan account.\n{holder_line}\nLock key: {lock_key}\n\nBoot blocked — running two instances against one client-id breaks order auth, depth state, and reconciliation. Stop the other instance, then restart this one."
                )
            }
            Self::BootHealthCheck {
                services_healthy,
                services_total,
            } => {
                format!("<b>Boot health check</b>\nHealthy: {services_healthy}/{services_total}")
            }
            Self::OrphanPositionDetected {
                count,
                total_abs_net_qty,
                sample_symbols,
                dry_run,
            } => {
                // `sample_symbols` are tradingSymbol strings from the Dhan
                // REST `GET /v2/positions` response — external-origin.
                let sample = if sample_symbols.is_empty() {
                    "(no sample symbols captured)".to_string()
                } else {
                    html_escape(&sample_symbols.join(", "))
                };
                let action = if *dry_run {
                    "DRY-RUN: no auto-exit attempted. EXIT MANUALLY via Dhan web UI before 15:30 IST close."
                } else {
                    "Auto-exit attempted — see follow-up Telegram for per-position outcome."
                };
                format!(
                    "<b>ORPHAN POSITIONS AT 15:25 IST</b>\n\
                     Open positions: {count}\n\
                     Total |net_qty|: {total_abs_net_qty}\n\
                     Sample: {sample}\n\n\
                     {action}"
                )
            }
            Self::OrphanPositionsClean => {
                "<b>Orphan-position watchdog</b>\n15:25 IST: account is flat. \u{2705}".to_string()
            }
            Self::BarMismatchCorrectedFromHistorical {
                compared_count,
                mismatches_count,
                sample_symbols,
                cross_check_pass,
            } => {
                // `sample_symbols` derive from matching against Dhan
                // historical REST data — external-origin.
                let sample = if sample_symbols.is_empty() {
                    "(no samples captured)".to_string()
                } else {
                    html_escape(&sample_symbols.join(", "))
                };
                format!(
                    "<b>09:15 BAR CORRECTED FROM DHAN HISTORICAL</b>\n\
                     Pass: {cross_check_pass}\n\
                     Compared: {compared_count} / Mismatched: {mismatches_count}\n\
                     Sample: {sample}\n\n\
                     Authoritative Dhan values written to candles_1m_shadow + \
                     historical_candles. Strategy gate is OPEN. Full forensic \
                     chain in bar_correction_audit."
                )
            }
            Self::BarMismatchCrossCheckInconclusive {
                compared_count,
                expected_count,
            } => {
                format!(
                    "<b>09:16:05 CROSS-CHECK INCONCLUSIVE — TRADING BLOCKED</b>\n\
                     Compared: {compared_count} / Expected: {expected_count}\n\
                     Coverage below 200/222 threshold. Dhan REST appears \
                     degraded. Strategy gate stays CLOSED for the day. \
                     Inspect Dhan REST health + manually authorize if safe."
                )
            }
            Self::BarMismatchCrossCheckFailed { reason } => {
                let reason = html_escape(reason);
                format!(
                    "<b>09:16:05 CROSS-CHECK HARD-FAILED — TRADING BLOCKED</b>\n\
                     Reason: {reason}\n\
                     All Dhan REST fetches errored. Strategy gate CLOSED. \
                     Inspect token expiry + DH-904 backoff + network."
                )
            }
            Self::BootDeadlineMissed {
                deadline_secs,
                step,
            } => {
                let step = html_escape(step);
                format!(
                    "<b>BOOT DEADLINE MISSED</b>\nDeadline: {deadline_secs}s\nBlocked at: {step}"
                )
            }
            Self::BootClockSkewExceeded {
                skew_secs,
                threshold_secs,
                source,
            } => {
                let source = html_escape(source);
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
            Self::RealtimeGuaranteeHealthy { score } => {
                format!(
                    "<b>Real-time guarantee score recovered</b>\n\
                     Composite score: {score:.3} (≥ 0.95).\n\
                     Code: SLO-01"
                )
            }
            Self::RealtimeGuaranteeDegraded { score, weakest } => {
                // Telegram HTML parse mode treats `<` and `>` as tag
                // delimiters; literal `<` / `>` in the body returns 400
                // Bad Request and the operator never sees the alert.
                // Phrase the band as plain text instead.
                format!(
                    "<b>Real-time guarantee score DEGRADED</b>\n\
                     Composite score: {score:.3} (band 0.80 to 0.95).\n\
                     Weakest dimension: {weakest}\n\
                     Code: SLO-02\n\
                     Action: see runbook .claude/rules/project/wave-3-d-error-codes.md"
                )
            }
            Self::RealtimeGuaranteeCritical { score, weakest } => {
                // Telegram HTML parse mode treats `<` and `>` as tag
                // delimiters; literal `<` / `>` in the body returns 400
                // Bad Request and the operator never sees the alert.
                // Phrase the threshold as plain text instead.
                format!(
                    "<b>REAL-TIME GUARANTEE SCORE CRITICAL</b>\n\
                     Composite score: {score:.3} (below 0.80).\n\
                     Weakest dimension: {weakest}\n\
                     Code: SLO-02\n\
                     Action: see runbook .claude/rules/project/wave-3-d-error-codes.md"
                )
            }
            Self::OrderRejected {
                correlation_id,
                reason,
            } => {
                let correlation_id = html_escape(correlation_id);
                let reason = html_escape(reason);
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
                format!("<b>RISK HALT</b>\nTrading stopped: {}", html_escape(reason))
            }
            Self::WebSocketReconnectionExhausted {
                connection_index,
                attempts,
            } => {
                // PR #790a — use scope-locked pool size (1), not Dhan cap (5).
                format!(
                    "<b>WebSocket {}/{} RECONNECTION EXHAUSTED</b>\nAttempts: {attempts}\nNo market data",
                    connection_index.saturating_add(1),
                    tickvault_common::constants::PHASE_0_MAIN_FEED_CONNECTION_COUNT
                )
            }
            Self::QuestDbDisconnected {
                writer,
                signal,
                signal_kind,
            } => {
                // Defensive HTML-escape: both are String (not &'static str),
                // so a future call site could pass dynamic text — consistent
                // with the html_escape() pattern used across to_message().
                let writer = html_escape(writer);
                let signal_kind = html_escape(signal_kind);
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
                failed_checks_before_recovery,
            } => {
                // Defensive HTML-escape (writer is String); numeric field needs none.
                let writer = html_escape(writer);
                format!(
                    "<b>QuestDB {writer} RECOVERED</b>\n\
                     Was unreachable for {failed_checks_before_recovery} consecutive liveness checks.\n\
                     Buffered ticks are now draining from the rescue ring to QuestDB.\n\
                     No action needed unless this recurs."
                )
            }
            Self::GrowwSidecarRejected { reason } => {
                // `reason` is a fixed per-class &'static str mapped to String by
                // the supervisor (never raw child text), but html_escape it
                // anyway for defense-in-depth, consistent with every String arm.
                format!(
                    "🆘 <b>Groww live feed rejected</b>\n{}\n\nThe Groww feed is \
                     connected but receiving nothing. Until this is fixed, Groww \
                     prices will not flow.",
                    html_escape(reason)
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
            Self::WebSocketPoolPartialAfterDeadline { .. } => "WebSocketPoolPartialAfterDeadline",
            Self::WebSocketPoolDeferredOffHours { .. } => "WebSocketPoolDeferredOffHours",
            Self::WebSocketPoolDegraded { .. } => "WebSocketPoolDegraded",
            Self::WebSocketPoolRecovered { .. } => "WebSocketPoolRecovered",
            Self::WebSocketPoolHalt { .. } => "WebSocketPoolHalt",
            Self::MarketOpenStreamingConfirmation { .. } => "MarketOpenStreamingConfirmation",
            Self::MarketOpenStreamingFailed { .. } => "MarketOpenStreamingFailed",
            Self::MarketOpenReadinessConfirmation { .. } => "MarketOpenReadinessConfirmation",
            Self::EndOfDayDigest { .. } => "EndOfDayDigest",
            Self::CrossVerify1mSummary { .. } => "CrossVerify1mSummary",
            Self::CrossVerify1mAborted { .. } => "CrossVerify1mAborted",
            // PR #4/#5 (2026-05-19): DepthSpotPriceStale + 7 Phase2*
            // name arms retired.
            // PR #6a (2026-05-19): NseBhavcopyCheck* name arms retired.
            Self::WebSocketDisconnected { .. } => "WebSocketDisconnected",
            Self::WebSocketDisconnectedOffHours { .. } => "WebSocketDisconnectedOffHours",
            Self::WebSocketReconnected { .. } => "WebSocketReconnected",
            Self::WebSocketSleepEntered { .. } => "WebSocketSleepEntered",
            Self::WebSocketSleepResumed { .. } => "WebSocketSleepResumed",
            Self::WebSocketTokenForceRenewedOnWake { .. } => "WebSocketTokenForceRenewedOnWake",
            Self::OrderUpdateConnected => "OrderUpdateConnected",
            Self::OrderUpdateAuthenticated => "OrderUpdateAuthenticated",
            Self::OrderUpdateDisconnected { .. } => "OrderUpdateDisconnected",
            Self::OrderUpdateReconnected { .. } => "OrderUpdateReconnected",
            Self::NoLiveTicksDuringMarketHours { .. } => "NoLiveTicksDuringMarketHours",
            Self::ShutdownInitiated => "ShutdownInitiated",
            Self::ShutdownComplete => "ShutdownComplete",
            Self::InstrumentBuildSuccess { .. } => "InstrumentBuildSuccess",
            Self::FeedInstrumentsLoaded { .. } => "FeedInstrumentsLoaded",
            Self::FeedAuthOk { .. } => "FeedAuthOk",
            Self::FeedConnectedAwaitingTicks { .. } => "FeedConnectedAwaitingTicks",
            Self::InstrumentBuildFailed { .. } => "InstrumentBuildFailed",
            Self::IpVerificationFailed { .. } => "IpVerificationFailed",
            Self::IpVerificationSuccess { .. } => "IpVerificationSuccess",
            Self::StaticIpBootCheckPassed { .. } => "StaticIpBootCheckPassed",
            Self::StaticIpBootCheckFailed { .. } => "StaticIpBootCheckFailed",
            Self::StaticIpBootCheckRetrying { .. } => "StaticIpBootCheckRetrying",
            Self::DualInstanceDetected { .. } => "DualInstanceDetected",
            Self::BootHealthCheck { .. } => "BootHealthCheck",
            Self::OrphanPositionDetected { .. } => "OrphanPositionDetected",
            Self::OrphanPositionsClean => "OrphanPositionsClean",
            Self::BarMismatchCorrectedFromHistorical { .. } => "BarMismatchCorrectedFromHistorical",
            Self::BarMismatchCrossCheckInconclusive { .. } => "BarMismatchCrossCheckInconclusive",
            Self::BarMismatchCrossCheckFailed { .. } => "BarMismatchCrossCheckFailed",
            Self::BootDeadlineMissed { .. } => "BootDeadlineMissed",
            Self::BootClockSkewExceeded { .. } => "BootClockSkewExceeded",
            Self::OrderRejected { .. } => "OrderRejected",
            Self::CircuitBreakerOpened { .. } => "CircuitBreakerOpened",
            Self::CircuitBreakerClosed => "CircuitBreakerClosed",
            Self::RateLimitExhausted { .. } => "RateLimitExhausted",
            Self::RiskHalt { .. } => "RiskHalt",
            Self::WebSocketReconnectionExhausted { .. } => "WebSocketReconnectionExhausted",
            Self::QuestDbDisconnected { .. } => "QuestDbDisconnected",
            Self::QuestDbReconnected { .. } => "QuestDbReconnected",
            Self::SelfTestPassed { .. } => "SelfTestPassed",
            Self::SelfTestDegraded { .. } => "SelfTestDegraded",
            Self::SelfTestCritical { .. } => "SelfTestCritical",
            Self::RealtimeGuaranteeHealthy { .. } => "RealtimeGuaranteeHealthy",
            Self::RealtimeGuaranteeDegraded { .. } => "RealtimeGuaranteeDegraded",
            Self::RealtimeGuaranteeCritical { .. } => "RealtimeGuaranteeCritical",
            Self::GrowwSidecarRejected { .. } => "GrowwSidecarRejected",
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
            Self::StaticIpBootCheckFailed { .. } => Severity::Critical,
            Self::DualInstanceDetected { .. } => Severity::Critical,
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
            Self::Custom { .. } => Severity::High,
            Self::RiskHalt { .. } => Severity::Critical,
            Self::WebSocketReconnectionExhausted { .. } => Severity::Critical,
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
            // Routine zero-disconnect drift swap — green by design. Prior
            // `Custom` routing made every 60s drift fire [HIGH] amber; see
            // Fix #9 in .claude/plans/active-plan.md and .claude/rules/project/
            // depth-subscription.md for the working-as-designed rationale.
            // Swap itself failed — depth quality degraded until next rebalance.
            Self::OrderUpdateDisconnected { .. } => Severity::High,
            Self::OrderUpdateReconnected { .. } => Severity::Low,
            Self::NoLiveTicksDuringMarketHours { .. } => Severity::Critical,
            Self::ShutdownInitiated => Severity::Medium,
            Self::CircuitBreakerClosed => Severity::Medium,
            Self::WebSocketConnected { .. } => Severity::Low,
            // 2026-05-09: demoted Medium → Low so the boot-success summary
            // renders as ✅ [LOW] (green). Immediate dispatch is now
            // controlled separately via `dispatch_policy()`.
            Self::WebSocketPoolOnline { .. } => Severity::Low,
            Self::WebSocketPoolPartialAfterDeadline { .. } => Severity::High,
            Self::WebSocketPoolDeferredOffHours { .. } => Severity::Low,
            Self::WebSocketPoolDegraded { .. } => Severity::High,
            Self::WebSocketPoolRecovered { .. } => Severity::Medium,
            Self::WebSocketPoolHalt { .. } => Severity::High,
            Self::MarketOpenStreamingConfirmation { .. } => Severity::Info,
            Self::MarketOpenStreamingFailed { .. } => Severity::High,
            Self::MarketOpenReadinessConfirmation { .. } => Severity::Info,
            Self::EndOfDayDigest { .. } => Severity::Info,
            // Hardened per the 2026-06-10 pre-impl adversarial review:
            // compared == 0 and missing > 0 must NEVER render as a green
            // PASS (audit Rule 11 — false-OK class).
            Self::CrossVerify1mSummary {
                compared,
                mismatches,
                missing,
                degraded,
                ..
            } => {
                if *mismatches == 0 && *missing == 0 && !*degraded && *compared > 0 {
                    Severity::Info
                } else {
                    Severity::High
                }
            }
            Self::CrossVerify1mAborted { .. } => Severity::High,
            Self::SelfTestPassed { .. } => Severity::Info,
            Self::SelfTestDegraded { .. } => Severity::High,
            Self::RealtimeGuaranteeHealthy { .. } => Severity::Info,
            // 2026-05-11 hotfix v2 — BOTH `RealtimeGuaranteeDegraded`
            // and `RealtimeGuaranteeCritical` are SUMMARY signals over the
            // six dimensions. The composite score can flap to 0.0 multiple
            // times per minute under normal market load when
            // `tick_freshness` momentarily reads 0 (some IDX_I instruments
            // legitimately don't tick every 30s). Per operator feedback
            // 2026-05-11 13:14 IST: the underlying typed errors
            // (WS-GAP-06 tick gaps, BOOT-01 QuestDB, AUTH-GAP-03 token)
            // ALREADY fire Telegram on their own — SLO-02 paging is
            // duplicate noise. Both demoted to `Low`: the events still
            // emit to Loki + Grafana + audit table for trend visibility,
            // but do NOT page Telegram. The composite score remains
            // available for the operator-health dashboard and the
            // `tv_realtime_guarantee_evaluations_total` counter.
            // Ratcheted by
            // `test_realtime_guarantee_degraded_severity_is_low_not_high`
            // and
            // `test_realtime_guarantee_critical_severity_is_low_not_critical`.
            Self::RealtimeGuaranteeDegraded { .. } => Severity::Low,
            Self::RealtimeGuaranteeCritical { .. } => Severity::Low,
            Self::SelfTestCritical { .. } => Severity::Critical,
            // PR #4/#5 (2026-05-19): DepthSpotPriceStale + 7 Phase2*
            // severity arms retired.
            // Wave 5 Item 26 L2 — operator triages FAIL/MISSING_OUR rows
            // via the audit table; the summary itself is Info on the
            // happy path. The dedicated `NseBhavcopyCheckFailed` variant
            // is High (per audit-findings Rule 11 — no false-OK class).
            // PR #6a (2026-05-19): NseBhavcopyCheck* severity arms retired.
            Self::OrderUpdateConnected => Severity::Low,
            Self::OrderUpdateAuthenticated => Severity::Medium,
            Self::TokenRenewed => Severity::Low,
            Self::IpVerificationSuccess { .. } => Severity::Low,
            Self::StaticIpBootCheckPassed { .. } => Severity::Low,
            Self::StaticIpBootCheckRetrying { .. } => Severity::Low,
            // Wave-Holiday-Gate (2026-05-09): bumped from Low → Medium so
            // these once-per-boot positive signals dispatch immediately
            // (Low coalesces in 60s window). Operator's 2026-05-09 complaint:
            // "before instruments and jwt fetch itself first message live
            // is connected and the message is showing I can't understand"
            // — i.e. WebSocketPoolOnline (Medium, immediate) was arriving
            // BEFORE Auth/Instruments (Low, drained 60s later). Promoting
            // these two to Medium restores the natural boot ordering.
            // 2026-05-09: PR #523 had bumped these two to Medium specifically
            // to force immediate dispatch (they used to coalesce 60s as Low,
            // breaking the boot-Telegram ordering). With the new
            // `DispatchPolicy::Immediate` mechanism the severity stays Low
            // (green ✅) AND the message ships immediately. Operator's
            // 2026-05-09 complaint resolved.
            Self::AuthenticationSuccess => Severity::Low,
            Self::InstrumentBuildSuccess { .. } => Severity::Low,
            // 2026-07-03 feed parity: once-per-activation positive ping that
            // a non-Dhan feed loaded + subscribed its instruments. Info so
            // it never pages — a purely positive signal, like the EOD digest.
            Self::FeedInstrumentsLoaded { .. } => Severity::Info,
            // 2026-07-04 Groww boot-visibility parity: green ✅ boot-stage
            // pings, exact mirrors of `AuthenticationSuccess` (Low) and the
            // Dhan connect signals — never page, ship immediately via
            // `dispatch_policy()`.
            Self::FeedAuthOk { .. } => Severity::Low,
            Self::FeedConnectedAwaitingTicks { .. } => Severity::Low,
            Self::BootHealthCheck { .. } => Severity::Low,
            Self::OrphanPositionDetected { .. } => Severity::Critical,
            Self::OrphanPositionsClean => Severity::Info,
            Self::BarMismatchCorrectedFromHistorical { .. } => Severity::Critical,
            Self::BarMismatchCrossCheckInconclusive { .. } => Severity::Critical,
            Self::BarMismatchCrossCheckFailed { .. } => Severity::Critical,
            Self::StartupComplete { .. } => Severity::Info,
            Self::ShutdownComplete => Severity::Info,
            // A genuine Groww feed reject (auth / entitlement / error) is
            // operator-actionable — pages so the 0-ticks cause is visible.
            Self::GrowwSidecarRejected { .. } => Severity::High,
        }
    }

    /// Returns the dispatch policy for this event — independent of severity.
    ///
    /// Most events return `DispatchPolicy::Default` so the coalescer falls
    /// back to severity-based routing (Critical/High/Medium → immediate;
    /// Low/Info → 60s coalescer window).
    ///
    /// A small set of boot-success events return
    /// `DispatchPolicy::Immediate` so they bypass the coalescer regardless
    /// of their `Severity::Low` color. This preserves the boot-Telegram
    /// ordering operators expect (Auth → Instruments → TickVault is live →
    /// Phase 2 complete) without forcing the messages to render in amber
    /// `[MEDIUM]` instead of green `[LOW]`. See `DispatchPolicy` doc for
    /// the full rationale (operator complaint 2026-05-09).
    pub fn dispatch_policy(&self) -> DispatchPolicy {
        match self {
            // Boot-success milestones — operator's at-a-glance "is the
            // boot OK?" Telegram pings. PR #526 added the first 4
            // (Auth/Instruments/PoolOnline/Phase2Complete). PR #6
            // (2026-05-09 operator request) made per-WS connect events
            // ship as instant green ✅ pings instead of coalescing for
            // 60s (Saturday mock-session boot showed `WebSocketConnected
            // x5` arriving 60s after `TickVault is live`). The depth-20 /
            // depth-200 connect events were deleted 2026-06-10 (Phase B
            // batch 3); OrderUpdateConnected is the surviving WS ping.
            Self::AuthenticationSuccess
            | Self::InstrumentBuildSuccess { .. }
            // 2026-07-03 feed parity: the Groww (any-feed) instruments-load
            // ping is a boot-success milestone — ship instantly like the
            // Dhan `InstrumentBuildSuccess` it mirrors, never coalesced.
            | Self::FeedInstrumentsLoaded { .. }
            // 2026-07-04 Groww boot-visibility parity: the per-feed Auth OK
            // + connected-awaiting-ticks pings are boot-success milestones —
            // ship instantly (Low would otherwise coalesce 60s and break the
            // boot-Telegram ordering the operator reads).
            | Self::FeedAuthOk { .. }
            | Self::FeedConnectedAwaitingTicks { .. }
            | Self::WebSocketPoolOnline { .. }
            | Self::WebSocketPoolDeferredOffHours { .. }
            // PR #5 (2026-05-19): Phase2Complete retired.
            | Self::OrderUpdateConnected => DispatchPolicy::Immediate,
            // 2026-06-10 (pre-impl finding M1): the once-per-day post-market
            // summary must arrive AT 15:31, not coalesced into a 60s bucket
            // that re-renders the body — Severity::Info would otherwise be
            // batched by the default routing.
            Self::CrossVerify1mSummary { .. } => DispatchPolicy::Immediate,
            _ => DispatchPolicy::Default,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// PR-D (2026-05-26): `append_detail_lines` helper retired alongside its
// sole callers (CandleVerificationFailed / HistoricalFetchFailed
// render arms). Tests below were the only consumer.

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
        // B9 deploy provenance: strip the `Build: <sha>` line BEFORE the
        // port-leak assertions — a random commit SHA can legitimately
        // contain digit runs like "9000"/"3000" as substrings, and the
        // ratchet must stay deterministic across commits.
        let msg_sans_build: String = msg
            .lines()
            .filter(|line| !line.starts_with("Build: "))
            .collect::<Vec<_>>()
            .join("\n");
        // SECURITY: ports and URLs must NOT appear in Telegram message
        assert!(
            !msg_sans_build.contains("3000"),
            "internal port leaked: {msg}"
        );
        assert!(
            !msg_sans_build.contains("9090"),
            "internal port leaked: {msg}"
        );
        assert!(
            !msg_sans_build.contains("16686"),
            "internal port leaked: {msg}"
        );
        assert!(
            !msg_sans_build.contains("9000"),
            "internal port leaked: {msg}"
        );
        assert!(!msg.contains("localhost"), "localhost leaked: {msg}");
        assert!(msg.contains("Dashboards"));
        assert!(msg.contains("QuestDB"));
        assert!(msg.contains("CloudWatch"));
        // CloudWatch-only migration #O1/#O2/#O3/#O4: the retired
        // dashboard stack MUST NOT reappear in operator-facing text.
        assert!(!msg.contains("Grafana"), "retired in #O1: {msg}");
        assert!(!msg.contains("Prometheus"), "retired in #O3: {msg}");
        assert!(!msg.contains("Alertmanager"), "retired in #O2: {msg}");
        assert!(!msg.contains("Valkey"), "retired in #O4: {msg}");
    }

    #[test]
    fn test_startup_complete_message_contains_build_sha() {
        // B9 deploy provenance: the boot Telegram must carry the short git
        // SHA of the running binary so the operator sees WHICH code booted.
        let event = NotificationEvent::StartupComplete { mode: "LIVE" };
        let msg = event.to_message();
        let expected = format!(
            "Build: {}",
            tickvault_common::build_info::build_git_sha_short()
        );
        assert!(
            msg.contains(&expected),
            "boot message must carry '{expected}': {msg}"
        );
    }

    #[test]
    fn test_groww_sidecar_rejected_renders_reason_and_topic_and_severity() {
        let event = NotificationEvent::GrowwSidecarRejected {
            reason: "account lacks live market-data feed entitlement".to_string(),
        };
        let msg = event.to_message();
        // The plain-English reason reaches the operator…
        assert!(
            msg.contains("account lacks live market-data feed entitlement"),
            "reason missing from message: {msg}"
        );
        // …with a status emoji at the start (10 commandments rule 5/10)…
        assert!(msg.contains("🆘"), "severity emoji missing: {msg}");
        assert!(msg.contains("Groww"), "feed name missing: {msg}");
        // …no library names / file paths / version numbers (10 commandments).
        assert!(!msg.contains("Stdio"), "lib jargon leaked: {msg}");
        assert!(!msg.contains(".rs"), "file path leaked: {msg}");
        assert!(!msg.contains("growwapi"), "lib name leaked: {msg}");
        // Stable topic + High severity so it pages.
        assert_eq!(event.topic(), "GrowwSidecarRejected");
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_groww_sidecar_rejected_html_escapes_reason() {
        // Defense-in-depth: even though the supervisor only passes a fixed
        // &'static str reason, any angle brackets are escaped at the render
        // boundary (consistent with every other String arm).
        let event = NotificationEvent::GrowwSidecarRejected {
            reason: "<script>".to_string(),
        };
        let msg = event.to_message();
        assert!(!msg.contains("<script>"), "raw HTML not escaped: {msg}");
        assert!(
            msg.contains("&lt;script&gt;"),
            "expected escaped form: {msg}"
        );
    }

    #[test]
    fn test_startup_complete_offline_message() {
        let event = NotificationEvent::StartupComplete { mode: "OFFLINE" };
        let msg = event.to_message();
        assert!(msg.contains("OFFLINE"));
        // Post-#O1/#O2/#O3/#O4: dashboard line names QuestDB + CloudWatch
        // only. Grafana / Prometheus / Alertmanager / Valkey are gone.
        assert!(msg.contains("QuestDB"));
        assert!(msg.contains("CloudWatch"));
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
        // UX fix 2026-04-17: display is 1-indexed (connection_index=2 → "3/N")
        // PR #458 (2026-05-04): payload now includes subscribed/capacity/ping.
        // PR #790a (2026-05-25): pool size is the scope-locked
        // PHASE_0_MAIN_FEED_CONNECTION_COUNT (= 1), not the Dhan account
        // cap MAX_WEBSOCKET_CONNECTIONS (= 5).
        let event = NotificationEvent::WebSocketConnected {
            connection_index: 2,
            subscribed_count: 5_000,
            capacity: 5_000,
            last_activity_secs_ago: Some(2),
        };
        let msg = event.to_message();
        assert!(msg.contains("3"), "1-indexed display: 2 -> 3; got: {msg}");
        assert!(
            msg.contains("Feed"),
            "Tier-2 wording uses 'Feed'; got: {msg}"
        );
        assert!(
            msg.contains("5,000"),
            "comma-formatted subscribed count required; got: {msg}"
        );
    }

    #[test]
    fn test_websocket_disconnected_includes_index_and_reason() {
        // UX fix 2026-04-17: display is 1-indexed (connection_index=1 → "2/N")
        // PR #790a (2026-05-25): "N" is the scope-locked pool size (1) under
        // indices_4_only, not the Dhan account cap (5).
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
        // PR #790a (2026-05-25): under indices_4_only scope the effective
        // main-feed pool is 1, so the label is "1/1" not "1/5". Pinned by
        // `test_websocket_reconnected_uses_scope_locked_pool_size` below.
        let event = NotificationEvent::WebSocketReconnected {
            connection_index: 0,
            reason: None,
            down_secs: 0,
            attempts: 0,
        };
        let msg = event.to_message();
        assert!(msg.contains("1"), "1-indexed display: 0 -> 1; got: {msg}");
        assert!(msg.contains("reconnected"));
    }

    /// PR #790a (2026-05-25) — ratchet pinning honest pool size in main-feed
    /// Telegram labels.
    ///
    /// Operator-reported 2026-05-25 13:03 IST: live Telegram showed
    /// "WebSocket 1/5 reconnected" even though `websocket-connection-scope-lock.md`
    /// locks the main-feed pool to ONE connection under `indices_4_only`.
    /// The `/5` was the Dhan account-wide cap (`MAX_WEBSOCKET_CONNECTIONS`),
    /// which is misleading — implies 4 connections are silently down.
    ///
    /// This ratchet fails the build if any main-feed Telegram event regresses
    /// back to displaying "/5" (or any value other than the scope-locked
    /// `PHASE_0_MAIN_FEED_CONNECTION_COUNT = 1`).
    #[test]
    fn test_main_feed_events_use_scope_locked_pool_size_not_dhan_cap() {
        let disconnect = NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: "test".to_string(),
        }
        .to_message();
        let off_hours = NotificationEvent::WebSocketDisconnectedOffHours {
            connection_index: 0,
            reason: "test".to_string(),
        }
        .to_message();
        let reconnected = NotificationEvent::WebSocketReconnected {
            connection_index: 0,
            reason: None,
            down_secs: 0,
            attempts: 0,
        }
        .to_message();
        let exhausted = NotificationEvent::WebSocketReconnectionExhausted {
            connection_index: 0,
            attempts: 60,
        }
        .to_message();

        // All four main-feed events MUST show "1/1" — the scope-locked
        // effective main-feed pool size under indices_4_only — not "1/5"
        // (the Dhan account-wide cap).
        for (label, msg) in [
            ("WebSocketDisconnected", &disconnect),
            ("WebSocketDisconnectedOffHours", &off_hours),
            ("WebSocketReconnected", &reconnected),
            ("WebSocketReconnectionExhausted", &exhausted),
        ] {
            assert!(
                msg.contains("1/1"),
                "{label} must display scope-locked pool size '1/1' under \
                 indices_4_only, got: {msg}"
            );
            assert!(
                !msg.contains("1/5"),
                "{label} must NOT display Dhan account-wide cap '1/5' — \
                 this misleads the operator under indices_4_only scope. Got: {msg}"
            );
        }
    }

    /// PR #790b (2026-05-25) — diagnostic carry-through ratchet.
    ///
    /// Pins that when `WebSocketReconnected` carries `reason`, `down_secs`, or
    /// `attempts`, the Telegram message surfaces them on a dedicated line so
    /// the operator immediately sees WHY the disconnect happened + how long
    /// it was down + how many backoff attempts it took.
    ///
    /// This answers the operator's 2026-05-25 13:03 IST question:
    /// > "why and how this just one connection of 4 instruments websocket
    /// >  got reconnected"
    #[test]
    fn test_reconnect_message_carries_disconnect_diagnostic_when_provided() {
        let full = NotificationEvent::WebSocketReconnected {
            connection_index: 0,
            reason: Some("Reset by peer".to_string()),
            down_secs: 110,
            attempts: 6,
        }
        .to_message();
        assert!(
            full.contains("Reason: Reset by peer"),
            "must show why the disconnect happened, got: {full}"
        );
        assert!(
            full.contains("Down: 110s"),
            "must show how long down, got: {full}"
        );
        assert!(
            full.contains("Attempts: 6"),
            "must show retry attempts, got: {full}"
        );
        assert!(
            full.contains("1/1"),
            "must keep scope-locked pool size from PR #790a, got: {full}"
        );

        // Backwards-compat: empty diagnostic context renders as the original
        // single-line message (no spurious "Down: 0s · Attempts: 0" line).
        let bare = NotificationEvent::WebSocketReconnected {
            connection_index: 0,
            reason: None,
            down_secs: 0,
            attempts: 0,
        }
        .to_message();
        assert!(
            !bare.contains("Down:"),
            "no diagnostic line when context is empty, got: {bare}"
        );
        assert!(
            !bare.contains("Reason:"),
            "no Reason line when reason is None, got: {bare}"
        );

        // Partial context: reason only.
        let reason_only = NotificationEvent::WebSocketReconnected {
            connection_index: 0,
            reason: Some("Token expired (DH-807)".to_string()),
            down_secs: 0,
            attempts: 0,
        }
        .to_message();
        assert!(
            reason_only.contains("Reason: Token expired (DH-807)"),
            "reason-only renders reason line, got: {reason_only}"
        );
        assert!(
            !reason_only.contains("Down:"),
            "no Down line when down_secs+attempts both zero, got: {reason_only}"
        );

        // Partial context: timing only (no reason captured — rare initial path).
        let timing_only = NotificationEvent::WebSocketReconnected {
            connection_index: 0,
            reason: None,
            down_secs: 15,
            attempts: 2,
        }
        .to_message();
        assert!(
            timing_only.contains("Down: 15s · Attempts: 2"),
            "timing-only renders Down+Attempts, got: {timing_only}"
        );
        assert!(
            !timing_only.contains("Reason:"),
            "no Reason line when reason is None, got: {timing_only}"
        );
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

    // -- Phase 0 Item 18 — StaticIpBootCheck event tests --

    #[test]
    fn test_static_ip_boot_check_passed_message_mentions_orders() {
        let event = NotificationEvent::StaticIpBootCheckPassed {
            ip_flag: "PRIMARY".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("Static IP boot check passed"));
        assert!(msg.contains("orders allowed"));
        assert!(msg.contains("PRIMARY"));
    }

    #[test]
    fn test_static_ip_boot_check_passed_is_low() {
        let event = NotificationEvent::StaticIpBootCheckPassed {
            ip_flag: "PRIMARY".to_string(),
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_static_ip_boot_check_failed_message_uses_plain_english() {
        // operator-charter §G: every Telegram message must be readable
        // by an auto-driver. Verify the reason maps to plain language.
        let event = NotificationEvent::StaticIpBootCheckFailed {
            reason: "orders_not_allowed".to_string(),
            orders_allowed: false,
            ip_match_status: "MATCH".to_string(),
            attempts_made: 1,
        };
        let msg = event.to_message();
        assert!(msg.contains("STATIC IP BOOT CHECK FAILED"));
        assert!(msg.contains("NOT ALLOWED for orders"));
        assert!(msg.contains("web.dhan.co"));
        assert!(msg.contains("Boot blocked"));
        // Surface the raw API reply so triage has the literal Dhan response.
        assert!(msg.contains("ordersAllowed=false"));
        assert!(msg.contains("ipMatchStatus=\"MATCH\""));
    }

    #[test]
    fn test_static_ip_boot_check_failed_is_critical() {
        let event = NotificationEvent::StaticIpBootCheckFailed {
            reason: "empty_response".to_string(),
            orders_allowed: false,
            ip_match_status: String::new(),
            attempts_made: 1,
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_static_ip_boot_check_failed_message_branches_by_reason() {
        for (reason, expected_phrase) in [
            ("empty_response", "empty IP response"),
            ("orders_not_allowed", "NOT ALLOWED for orders"),
            ("match_status_not_ok", "match check did not return MATCH"),
        ] {
            let event = NotificationEvent::StaticIpBootCheckFailed {
                reason: reason.to_string(),
                orders_allowed: false,
                ip_match_status: String::new(),
                attempts_made: 1,
            };
            let msg = event.to_message();
            assert!(
                msg.contains(expected_phrase),
                "reason={reason} should produce {expected_phrase:?}, got: {msg}"
            );
        }
    }

    #[test]
    fn test_static_ip_boot_check_failed_message_after_exhausted_retry() {
        // Phase 0 Item 18b — when the loop exhausts its retry budget
        // the operator sees how long we waited before giving up.
        let event = NotificationEvent::StaticIpBootCheckFailed {
            reason: "orders_not_allowed".to_string(),
            orders_allowed: false,
            ip_match_status: "MISMATCH".to_string(),
            attempts_made: 30,
        };
        let msg = event.to_message();
        assert!(
            msg.contains("30 retry attempts"),
            "expected attempts_made count in message, got: {msg}"
        );
        assert!(msg.contains("minutes"));
    }

    #[test]
    fn test_static_ip_boot_check_retrying_message_mentions_count() {
        let event = NotificationEvent::StaticIpBootCheckRetrying {
            attempt: 7,
            max_attempts: 30,
        };
        let msg = event.to_message();
        assert!(msg.contains("Static IP check waiting"));
        assert!(msg.contains("Retry 7 of 30"));
        assert!(msg.contains("every minute"));
    }

    #[test]
    fn test_static_ip_boot_check_retrying_is_low_severity() {
        // Severity::Low coalesces into 60s windows — the operator
        // sees one summary per minute instead of 30 individual pages.
        let event = NotificationEvent::StaticIpBootCheckRetrying {
            attempt: 1,
            max_attempts: 30,
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_static_ip_boot_check_topics_stable() {
        // Wire-format names used by structured log + audit pipelines.
        assert_eq!(
            NotificationEvent::StaticIpBootCheckPassed {
                ip_flag: "PRIMARY".to_string()
            }
            .topic(),
            "StaticIpBootCheckPassed"
        );
        assert_eq!(
            NotificationEvent::StaticIpBootCheckFailed {
                reason: "empty_response".to_string(),
                orders_allowed: false,
                ip_match_status: String::new(),
                attempts_made: 1,
            }
            .topic(),
            "StaticIpBootCheckFailed"
        );
        assert_eq!(
            NotificationEvent::StaticIpBootCheckRetrying {
                attempt: 1,
                max_attempts: 30,
            }
            .topic(),
            "StaticIpBootCheckRetrying"
        );
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

    // RETIRED 2026-06-12: test_token_renewal_deadline_missed_notification deleted
    // with the TokenRenewalDeadlineMissed variant (redundant with TokenRenewalFailed).

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
    fn test_instrument_build_success_severity_and_dispatch() {
        // 2026-05-09 (PR #523 → this PR): the Wave-Holiday-Gate fix
        // had bumped this severity Low → Medium to force immediate
        // dispatch through the coalescer. That made the message render
        // amber 🔵 instead of green ✅ — operator complaint 2026-05-09.
        // The new `DispatchPolicy` mechanism decouples color from
        // routing: severity is back to Low (green) AND dispatch policy
        // is `Immediate` (bypasses coalescer).
        let event = NotificationEvent::InstrumentBuildSuccess {
            source: "primary".to_string(),
            derivative_count: 100,
            underlying_count: 10,
        };
        assert_eq!(event.severity(), Severity::Low);
        assert_eq!(event.dispatch_policy(), DispatchPolicy::Immediate);
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
            subscribed_count: 5_000,
            capacity: 5_000,
            last_activity_secs_ago: Some(1),
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_ws_reconnected_severity() {
        let event = NotificationEvent::WebSocketReconnected {
            connection_index: 0,
            reason: None,
            down_secs: 0,
            attempts: 0,
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
    fn test_auth_success_severity_and_dispatch() {
        // 2026-05-09: see `test_instrument_build_success_severity_and_dispatch`
        // for the full rationale. Severity is Low (green ✅), dispatch
        // policy is `Immediate` (bypasses coalescer).
        assert_eq!(
            NotificationEvent::AuthenticationSuccess.severity(),
            Severity::Low
        );
        assert_eq!(
            NotificationEvent::AuthenticationSuccess.dispatch_policy(),
            DispatchPolicy::Immediate
        );
    }

    /// Default dispatch policy returns `Default` — exercised here so the
    /// `pub fn dispatch_policy` has dedicated test coverage. Most events
    /// fall through to this arm; the coalescer applies the severity-based
    /// routing fallback in that case.
    #[test]
    fn test_dispatch_policy_default_returns_default() {
        let event = NotificationEvent::TokenRenewed;
        assert_eq!(event.dispatch_policy(), DispatchPolicy::Default);
    }

    /// 2026-05-09 ratchet — assert that every event with
    /// `DispatchPolicy::Immediate` AND `Severity::Low` retains the green
    /// ✅ tag. Pre-this-PR these events were force-bumped to Medium for
    /// immediate dispatch, which broke the color contract. This test
    /// blocks regression to the old severity-overloads-routing pattern.
    #[test]
    fn test_immediate_dispatch_low_severity_events_render_green_tag() {
        let events = [
            NotificationEvent::AuthenticationSuccess,
            NotificationEvent::InstrumentBuildSuccess {
                source: "primary".to_string(),
                derivative_count: 100,
                underlying_count: 10,
            },
        ];
        for event in events {
            assert_eq!(
                event.dispatch_policy(),
                DispatchPolicy::Immediate,
                "{} must request immediate dispatch",
                event.topic()
            );
            assert_eq!(
                event.severity(),
                Severity::Low,
                "{} must render green ✅ [LOW] (color decoupled from routing)",
                event.topic()
            );
            assert!(
                event.severity().tag().contains("[LOW]"),
                "tag drift detected"
            );
        }
    }

    /// 2026-05-09 PR 6 ratchet (operator request): every per-WS connect
    /// event MUST render green ✅ [LOW] AND ship immediately. Pre-this-PR
    /// these events used `DispatchPolicy::Default` → 60s coalesce, so the
    /// operator's Saturday boot showed `WebSocketConnected x5` arriving
    /// 60s after `TickVault is live`. Each WS surface now confirms in
    /// real time. Blocks regression to the coalesced-by-default pattern.
    #[test]
    fn test_per_ws_connect_events_are_immediate_low() {
        let events = [NotificationEvent::OrderUpdateConnected];
        for event in events {
            assert_eq!(
                event.dispatch_policy(),
                DispatchPolicy::Immediate,
                "{} must request immediate dispatch (operator request 2026-05-09 — \
                 per-WS connect should not coalesce 60s alongside the boot summary)",
                event.topic()
            );
            assert_eq!(
                event.severity(),
                Severity::Low,
                "{} must render green ✅ [LOW] (success ping)",
                event.topic()
            );
            assert!(
                event.severity().tag().contains("[LOW]"),
                "tag drift detected for {}",
                event.topic()
            );
        }
    }

    /// 2026-05-09 off-hours-gate ratchet (operator complaint): an
    /// off-hours boot used to fire `[HIGH] TickVault is partially
    /// online — 0/5 feeds connected (5 stuck)` because the boot-time
    /// `WebSocketPoolPartialAfterDeadline` (Severity::High) emitter
    /// did not gate on market hours. The new
    /// `WebSocketPoolDeferredOffHours` variant replaces it
    /// outside `[09:00, 15:30) IST` and MUST be Severity::Low +
    /// DispatchPolicy::Immediate (single ✅ ping, no coalesce).
    /// Blocks regression to the false-HIGH alarm.
    #[test]
    fn test_pool_deferred_off_hours_is_immediate_low_not_high() {
        let event = NotificationEvent::WebSocketPoolDeferredOffHours {
            deferred: 5,
            total: 5,
            boot_path: BootPathLabel::Slow,
            feeds: feed_lines(),
        };
        assert_eq!(
            event.severity(),
            Severity::Low,
            "off-hours boot deferred MUST render green ✅ [LOW] — \
             pre-2026-05-09 it incorrectly fired [HIGH] partial-online"
        );
        assert_eq!(
            event.dispatch_policy(),
            DispatchPolicy::Immediate,
            "off-hours boot deferred is a boot-success milestone — \
             must ship instantly alongside the other 7 immediate events"
        );
        assert_eq!(event.topic(), "WebSocketPoolDeferredOffHours");
        let body = event.to_message();
        assert!(
            body.contains("DEFERRED") || body.contains("deferred") || body.contains("until 09:00"),
            "body must explain WHY the connections are not yet up: {body}"
        );
        // Negative ratchet: the partial-after-deadline (in-market) variant
        // MUST keep firing High so genuine in-market faults still page.
        let in_market = NotificationEvent::WebSocketPoolPartialAfterDeadline {
            connected: 4,
            total: 5,
            per_connection: vec![(0, 5000, None); 5],
            stuck: vec![(4, "Disconnected".to_string())],
            boot_path: BootPathLabel::Slow,
        };
        assert_eq!(
            in_market.severity(),
            Severity::High,
            "in-market partial pool MUST stay High — regression block"
        );
    }

    #[test]
    fn test_circuit_breaker_closed_message() {
        let event = NotificationEvent::CircuitBreakerClosed;
        let msg = event.to_message();
        assert!(msg.contains("Circuit breaker CLOSED"));
        assert!(msg.contains("resumed"));
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
        let event = NotificationEvent::WebSocketDisconnected {
            connection_index: 1,
            reason: "reset".to_string(),
        };
        let cloned = event.clone();
        assert_eq!(event.to_message(), cloned.to_message());
        assert_eq!(event.severity(), cloned.severity());
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

    // RETIRED 2026-06-12: test_token_renewal_deadline_missed_severity_is_critical
    // deleted with the variant.

    // PR #6a (2026-05-19): 3 NseBhavcopyCheck* tests retired with the variants.

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

    // PR #5 (2026-05-19): test_phase2_complete_message_includes_depth_counts
    // retired — Phase2Complete event removed with the Phase 2 dispatcher
    // chain under operator-locked 4-IDX_I LOCKED_UNIVERSE.

    #[test]
    fn test_market_open_streaming_confirmation_severity_is_info() {
        // Plan #5 (2026-04-22): once-per-day positive heartbeat — must be
        // INFO so it never wakes anyone up but still appears in Telegram.
        let ev = NotificationEvent::MarketOpenStreamingConfirmation {
            main_feed_active: 5,
            main_feed_total: 5,
            order_update_active: true,
        };
        assert_eq!(ev.severity(), Severity::Info);
    }

    #[test]
    fn test_market_open_streaming_confirmation_message_lists_every_feed() {
        let ev = NotificationEvent::MarketOpenStreamingConfirmation {
            main_feed_active: 5,
            main_feed_total: 5,
            order_update_active: true,
        };
        let msg = ev.to_message();
        assert!(msg.contains("Streaming live"), "got: {msg}");
        assert!(msg.contains("Main feed: 5/5"), "got: {msg}");
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
            order_update_active: false,
        };
        let msg = ev.to_message();
        assert!(
            msg.contains("Order updates: 0/1"),
            "must show OMS disconnected when active=false; got: {msg}"
        );
    }

    // -- MarketOpenReadinessConfirmation (audit Finding #5, 2026-05-03) ----

    #[test]
    fn test_market_open_readiness_confirmation_severity_is_info() {
        // Audit Finding #5 (2026-05-03): pre-market positive-readiness
        // ping at 09:14:00 IST. Severity::Info — never pages, only
        // confirms the system is ready 60s before the bell. Severity
        // ratchet so a future change cannot silently escalate this to
        // High and turn it into a noisy pager event.
        let ev = NotificationEvent::MarketOpenReadinessConfirmation {
            main_feed_active: 5,
            main_feed_total: 5,
            order_update_active: true,
            token_remaining_secs: 8 * 3600,
        };
        assert_eq!(ev.severity(), Severity::Info);
    }

    #[test]
    fn test_market_open_readiness_confirmation_includes_subscription_counts() {
        let ev = NotificationEvent::MarketOpenReadinessConfirmation {
            main_feed_active: 5,
            main_feed_total: 5,
            order_update_active: true,
            token_remaining_secs: 8 * 3600,
        };
        let msg = ev.to_message();
        assert!(msg.contains("READY for market open"), "got: {msg}");
        assert!(msg.contains("09:14:00 IST"), "got: {msg}");
        assert!(msg.contains("Main feed: 5/5"), "got: {msg}");
        assert!(msg.contains("Order updates: 1/1"), "got: {msg}");
    }

    #[test]
    fn test_market_open_readiness_confirmation_shows_token_headroom_in_hours() {
        // Token expiry headroom must be human-readable (hours, 1 decimal),
        // not raw seconds. Operator should see "Token headroom: 8.0h" not
        // "Token headroom: 28800". Catches future regressions where the
        // formatter forgets to convert seconds → hours.
        let ev = NotificationEvent::MarketOpenReadinessConfirmation {
            main_feed_active: 5,
            main_feed_total: 5,
            order_update_active: true,
            token_remaining_secs: 8 * 3600 + 1800, // 8.5 hours
        };
        let msg = ev.to_message();
        assert!(
            msg.contains("Token headroom: 8.5h"),
            "must format token headroom as hours with 1 decimal; got: {msg}"
        );
        assert!(
            !msg.contains("30600"),
            "must NOT show raw seconds; got: {msg}"
        );
    }

    #[test]
    fn test_market_open_readiness_confirmation_variant_name_is_stable() {
        // ratchet: variant Debug-format is on the wire (Telegram/audit),
        // so it must be stable. Catches accidental rename.
        let ev = NotificationEvent::MarketOpenReadinessConfirmation {
            main_feed_active: 5,
            main_feed_total: 5,
            order_update_active: true,
            token_remaining_secs: 0,
        };
        let dbg = format!("{ev:?}");
        assert!(
            dbg.starts_with("MarketOpenReadinessConfirmation"),
            "variant debug-name must remain MarketOpenReadinessConfirmation; got: {dbg}"
        );
    }

    #[test]
    fn test_market_open_readiness_confirmation_mentions_60s_to_bell() {
        // Operator-facing wording: the 09:14:00 ping is 60s before the
        // 09:15:00 NSE opening bell. Pin this in the message so reviewers
        // know why the time is 09:14 not 09:15.
        let ev = NotificationEvent::MarketOpenReadinessConfirmation {
            main_feed_active: 5,
            main_feed_total: 5,
            order_update_active: true,
            token_remaining_secs: 8 * 3600,
        };
        let msg = ev.to_message();
        assert!(
            msg.contains("60s") || msg.contains("Bell rings"),
            "must explain why 09:14 not 09:15; got: {msg}"
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

    // `test_midmarket_boot_complete_*` retired in PR #509c — variant deleted.

    /// Regression: 2026-04-28 — `RealtimeGuaranteeCritical` body contained
    /// the literal phrase `(< 0.80)` which Telegram HTML parse mode treats
    /// as the start of an unclosed tag, returning 400 Bad Request. The
    /// operator never saw the SLO-02 page during a real CRITICAL event.
    /// Strip valid `<b>`/`</b>` tags, then assert no other `<` / `>` remain.
    #[test]
    fn test_realtime_guarantee_critical_body_has_no_unescaped_html_brackets() {
        let event = NotificationEvent::RealtimeGuaranteeCritical {
            score: 0.0,
            weakest: "phase2_health",
        };
        let body = event.to_message();
        let stripped = body.replace("<b>", "").replace("</b>", "");
        assert!(
            !stripped.contains('<'),
            "Telegram HTML mode rejects unescaped '<' — body was: {body}"
        );
        assert!(
            !stripped.contains('>'),
            "Telegram HTML mode rejects unescaped '>' — body was: {body}"
        );
    }

    /// Same regression check for the Degraded variant — its body used to
    /// contain `[0.80, 0.95)` which is bracket-safe but the parens-with-
    /// `<` pattern is what crashed the Critical variant; keep this guard
    /// so a future edit can't reintroduce a literal `<` here either.
    /// 2026-05-11 hotfix — `RealtimeGuaranteeDegraded` MUST stay at
    /// `Severity::Low` so the 10s SLO scheduler does NOT flap-page
    /// the operator on the 0.95 boundary. `RealtimeGuaranteeCritical`
    /// stays at `Severity::Critical` for genuine fires (score < 0.80).
    /// Ratchet — a future "tighten alerting" PR cannot silently
    /// re-enable the pager spam without flipping this test.
    #[test]
    fn test_realtime_guarantee_degraded_severity_is_low_not_high() {
        let event = NotificationEvent::RealtimeGuaranteeDegraded {
            score: 0.85,
            weakest: "ws_health",
        };
        assert_eq!(
            event.severity(),
            Severity::Low,
            "Degraded MUST be Low (Loki + Grafana only, no Telegram)"
        );
    }

    #[test]
    fn test_realtime_guarantee_critical_severity_is_low_not_critical() {
        // 2026-05-11 hotfix v2 — operator directive: SLO-02 is a SUMMARY
        // signal; the underlying typed errors (WS-GAP-06, BOOT-01/02,
        // AUTH-GAP-03, STORAGE-GAP-03, PHASE2-01) already fire Telegram
        // on their own. The composite-score scheduler flaps multiple
        // times per minute under normal market load (tick_freshness can
        // read 0 when illiquid IDX_I instruments don't tick within 30s).
        // Both Degraded AND Critical demoted to Low (no Telegram page).
        // Composite score still computed + emitted as counter for the
        // operator-health Grafana dashboard.
        // Defense-in-depth — main.rs also drops the `notify()` call site
        // entirely (Telegram-suppressed at the call site). This severity
        // ratchet guards against a future regression that re-adds a
        // notify() call without re-evaluating the operator directive.
        let event = NotificationEvent::RealtimeGuaranteeCritical {
            score: 0.50,
            weakest: "qdb_health",
        };
        assert_eq!(
            event.severity(),
            Severity::Low,
            "Critical MUST be Low — no Telegram page (Loki + audit only)"
        );
    }

    #[test]
    fn test_realtime_guarantee_degraded_body_has_no_unescaped_html_brackets() {
        let event = NotificationEvent::RealtimeGuaranteeDegraded {
            score: 0.85,
            weakest: "ws_health",
        };
        let body = event.to_message();
        let stripped = body.replace("<b>", "").replace("</b>", "");
        assert!(
            !stripped.contains('<'),
            "Telegram HTML mode rejects unescaped '<' — body was: {body}"
        );
        assert!(
            !stripped.contains('>'),
            "Telegram HTML mode rejects unescaped '>' — body was: {body}"
        );
    }

    /// Healthy variant uses the unicode `≥` glyph (not ASCII `>`), so it
    /// has always been safe — pin it as a guard so a future "simplify to
    /// >= " edit cannot regress it back into a 400-Bad-Request landmine.
    #[test]
    fn test_realtime_guarantee_healthy_body_has_no_unescaped_html_brackets() {
        let event = NotificationEvent::RealtimeGuaranteeHealthy { score: 1.0 };
        let body = event.to_message();
        let stripped = body.replace("<b>", "").replace("</b>", "");
        assert!(
            !stripped.contains('<'),
            "Telegram HTML mode rejects unescaped '<' — body was: {body}"
        );
        assert!(
            !stripped.contains('>'),
            "Telegram HTML mode rejects unescaped '>' — body was: {body}"
        );
    }

    // -----------------------------------------------------------------------
    // html_escape — external-text Telegram HTML hardening (named follow-up
    // from #1089 batch-3 security review). Telegram sends parse_mode=HTML;
    // an external `reason`/`detail`/`ip_match_status` containing `<`/`>`/`&`
    // would 400-Bad-Request the alert so the operator never sees it. These
    // tests inject a hostile string into each fixed arm and assert it is
    // escaped (no raw external bracket) while the trusted `<b>` structural
    // tag survives.
    // -----------------------------------------------------------------------

    /// Hostile external string used across the escaping tests.
    const HOSTILE: &str = "<script>alert(1)</script> a < b & c > d";

    #[test]
    fn test_html_escape_amp_first_no_double_escape() {
        // `&` must be escaped before `<`/`>` so the `&` we introduce in
        // `&lt;`/`&gt;` is not re-escaped into `&amp;lt;`.
        assert_eq!(super::html_escape("<"), "&lt;");
        assert_eq!(super::html_escape(">"), "&gt;");
        assert_eq!(super::html_escape("&"), "&amp;");
        assert_eq!(super::html_escape("a < b"), "a &lt; b");
        assert_eq!(super::html_escape("x & y"), "x &amp; y");
        // The crux: a literal `<` becomes exactly `&lt;`, NOT `&amp;lt;`.
        assert_eq!(super::html_escape("<b>"), "&lt;b&gt;");
        assert!(!super::html_escape("<b>").contains("&amp;lt;"));
    }

    #[test]
    fn test_html_escape_plain_passthrough_and_empty() {
        // Common case: no metacharacters → byte-identical output.
        assert_eq!(super::html_escape(""), "");
        assert_eq!(
            super::html_escape("Connection reset, retrying in 200ms"),
            "Connection reset, retrying in 200ms"
        );
        // UTF-8 multi-byte is untouched.
        assert_eq!(super::html_escape("score ≥ 0.95"), "score ≥ 0.95");
    }

    /// Asserts a rendered body escaped the injected hostile string: the raw
    /// `<script>` / external `<`/`>` must be absent, the escaped entities
    /// present, and the trusted `<b>` structural tag preserved.
    fn assert_external_text_escaped(body: &str) {
        assert!(
            !body.contains("<script>"),
            "raw external <script> tag leaked into HTML body: {body}"
        );
        assert!(
            body.contains("&lt;script&gt;"),
            "external '<'/'>' not escaped to entities: {body}"
        );
        assert!(
            body.contains("&amp; c"),
            "external '&' not escaped to &amp;: {body}"
        );
        assert!(
            body.contains("<b>"),
            "trusted <b> structural tag must NOT be escaped: {body}"
        );
    }

    #[test]
    fn test_websocket_disconnected_escapes_external_reason() {
        let ev = NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: HOSTILE.to_string(),
        };
        assert_external_text_escaped(&ev.to_message());
    }

    #[test]
    fn test_websocket_disconnected_offhours_escapes_external_reason() {
        let ev = NotificationEvent::WebSocketDisconnectedOffHours {
            connection_index: 0,
            reason: HOSTILE.to_string(),
        };
        assert_external_text_escaped(&ev.to_message());
    }

    #[test]
    fn test_order_update_disconnected_escapes_external_reason() {
        let ev = NotificationEvent::OrderUpdateDisconnected {
            reason: HOSTILE.to_string(),
        };
        assert_external_text_escaped(&ev.to_message());
    }

    #[test]
    fn test_order_rejected_escapes_external_reason_and_correlation() {
        let ev = NotificationEvent::OrderRejected {
            correlation_id: "id<x>&y".to_string(),
            reason: HOSTILE.to_string(),
        };
        let body = ev.to_message();
        assert_external_text_escaped(&body);
        assert!(
            !body.contains("id<x>"),
            "correlation_id not escaped: {body}"
        );
        assert!(
            body.contains("id&lt;x&gt;&amp;y"),
            "correlation_id escaping wrong: {body}"
        );
    }

    #[test]
    fn test_risk_halt_escapes_external_reason() {
        let ev = NotificationEvent::RiskHalt {
            reason: HOSTILE.to_string(),
        };
        assert_external_text_escaped(&ev.to_message());
    }

    #[test]
    fn test_authentication_failed_escapes_external_reason() {
        let ev = NotificationEvent::AuthenticationFailed {
            reason: HOSTILE.to_string(),
        };
        assert_external_text_escaped(&ev.to_message());
    }

    #[test]
    fn test_mid_session_profile_invalidated_escapes_external_reason() {
        let ev = NotificationEvent::MidSessionProfileInvalidated {
            reason: HOSTILE.to_string(),
        };
        assert_external_text_escaped(&ev.to_message());
    }

    #[test]
    fn test_cross_verify_1m_aborted_escapes_external_detail() {
        let ev = NotificationEvent::CrossVerify1mAborted {
            detail: HOSTILE.to_string(),
        };
        assert_external_text_escaped(&ev.to_message());
    }

    #[test]
    fn test_static_ip_boot_check_failed_escapes_ip_match_status() {
        let ev = NotificationEvent::StaticIpBootCheckFailed {
            reason: "match_status_not_ok".to_string(),
            orders_allowed: false,
            ip_match_status: HOSTILE.to_string(),
            attempts_made: 1,
        };
        assert_external_text_escaped(&ev.to_message());
    }

    // Security-review (PR #1102) follow-on: external-text arms the first
    // sweep missed — `holder`/`lock_key` from the Valkey/SSM instance lock
    // and `sample_symbols` from Dhan REST positions / historical responses.

    #[test]
    fn test_dual_instance_detected_escapes_external_holder_and_lock_key() {
        let ev = NotificationEvent::DualInstanceDetected {
            holder: HOSTILE.to_string(),
            lock_key: "key<x>&y".to_string(),
        };
        let body = ev.to_message();
        assert_external_text_escaped(&body);
        assert!(!body.contains("key<x>"), "lock_key not escaped: {body}");
        assert!(
            body.contains("key&lt;x&gt;&amp;y"),
            "lock_key escaping wrong: {body}"
        );
    }

    #[test]
    fn test_orphan_position_detected_escapes_external_sample_symbols() {
        let ev = NotificationEvent::OrphanPositionDetected {
            count: 1,
            total_abs_net_qty: 50,
            sample_symbols: vec![HOSTILE.to_string()],
            dry_run: true,
        };
        assert_external_text_escaped(&ev.to_message());
    }

    #[test]
    fn test_bar_mismatch_corrected_escapes_external_sample_symbols() {
        let ev = NotificationEvent::BarMismatchCorrectedFromHistorical {
            compared_count: 222,
            mismatches_count: 1,
            sample_symbols: vec![HOSTILE.to_string()],
            cross_check_pass: "post_open_09_16_05",
        };
        assert_external_text_escaped(&ev.to_message());
    }

    // -----------------------------------------------------------------------
    // Phase 0 Item 19c — DualInstanceDetected notification variant
    // -----------------------------------------------------------------------

    #[test]
    fn test_dual_instance_detected_message_includes_holder_and_key() {
        let event = NotificationEvent::DualInstanceDetected {
            holder: "i-0123abc:42:deadbeefcafef00d".to_string(),
            lock_key: "tickvault:instance:lock:prod".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("DUAL-INSTANCE DETECTED"));
        assert!(msg.contains("i-0123abc"));
        assert!(msg.contains("tickvault:instance:lock:prod"));
        assert!(msg.contains("Boot blocked"));
    }

    #[test]
    fn test_dual_instance_detected_empty_holder_uses_fallback_text() {
        // The SSM lock read can return an empty holder (race
        // between SET-NX-EX and the diagnostic GET). The Telegram
        // payload must still be operator-readable in that case —
        // pin the fallback phrasing.
        let event = NotificationEvent::DualInstanceDetected {
            holder: String::new(),
            lock_key: "tickvault:instance:lock:dev".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("not retrievable"));
        assert!(msg.contains("tickvault:instance:lock:dev"));
        // No "Live peer:" line on the empty-holder path (would read
        // weird with an empty identity).
        assert!(!msg.contains("Live peer:"));
    }

    #[test]
    fn test_dual_instance_detected_severity_is_critical() {
        let event = NotificationEvent::DualInstanceDetected {
            holder: "x".to_string(),
            lock_key: "y".to_string(),
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_dual_instance_detected_topic_is_stable() {
        // operator-charter forbids renaming wire-format identifiers
        // without explicit migration; the topic flows into Telegram
        // coalescer bucket keys + log lines + audit rows.
        let event = NotificationEvent::DualInstanceDetected {
            holder: String::new(),
            lock_key: String::new(),
        };
        assert_eq!(event.topic(), "DualInstanceDetected");
    }

    // -----------------------------------------------------------------------
    // Phase 0 Item 20 — OrphanPositionDetected / OrphanPositionsClean
    // -----------------------------------------------------------------------

    #[test]
    fn test_orphan_position_detected_is_critical() {
        let event = NotificationEvent::OrphanPositionDetected {
            count: 1,
            total_abs_net_qty: 50,
            sample_symbols: vec!["NIFTY-Mar2026-24500-CE".to_string()],
            dry_run: true,
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_orphan_positions_clean_is_info() {
        let event = NotificationEvent::OrphanPositionsClean;
        assert_eq!(event.severity(), Severity::Info);
    }

    #[test]
    fn test_orphan_position_topics_are_stable() {
        // operator-charter forbids renaming wire-format identifiers.
        let detected = NotificationEvent::OrphanPositionDetected {
            count: 0,
            total_abs_net_qty: 0,
            sample_symbols: vec![],
            dry_run: true,
        };
        assert_eq!(detected.topic(), "OrphanPositionDetected");
        let clean = NotificationEvent::OrphanPositionsClean;
        assert_eq!(clean.topic(), "OrphanPositionsClean");
    }

    #[test]
    fn test_orphan_position_detected_message_includes_manual_exit_instruction_in_dry_run() {
        let event = NotificationEvent::OrphanPositionDetected {
            count: 2,
            total_abs_net_qty: 75,
            sample_symbols: vec![
                "NIFTY-Mar2026-24500-CE".to_string(),
                "BANKNIFTY-Mar2026-52000-PE".to_string(),
            ],
            dry_run: true,
        };
        let msg = event.to_message();
        assert!(msg.contains("ORPHAN POSITIONS AT 15:25 IST"));
        assert!(msg.contains("Open positions: 2"));
        assert!(msg.contains("Total |net_qty|: 75"));
        assert!(msg.contains("NIFTY-Mar2026-24500-CE"));
        assert!(msg.contains("BANKNIFTY-Mar2026-52000-PE"));
        assert!(
            msg.contains("DRY-RUN"),
            "Phase 0 message MUST flag dry-run mode so operator exits manually"
        );
        assert!(
            msg.contains("EXIT MANUALLY"),
            "Phase 0 message MUST instruct operator to exit before 15:30"
        );
    }

    #[test]
    fn test_orphan_position_detected_message_in_live_mode_says_auto_exit_attempted() {
        let event = NotificationEvent::OrphanPositionDetected {
            count: 1,
            total_abs_net_qty: 50,
            sample_symbols: vec!["NIFTY-Mar2026-24500-CE".to_string()],
            dry_run: false,
        };
        let msg = event.to_message();
        assert!(msg.contains("Auto-exit attempted"));
        assert!(
            !msg.contains("DRY-RUN"),
            "live-mode message MUST NOT mention dry-run"
        );
    }

    #[test]
    fn test_orphan_positions_clean_message_is_positive_ping() {
        let event = NotificationEvent::OrphanPositionsClean;
        let msg = event.to_message();
        assert!(msg.contains("15:25 IST"));
        assert!(msg.contains("flat"));
    }

    // -----------------------------------------------------------------------
    // Phase 0 Items 15+28+29 — BarMismatch* (2026-05-18)
    // -----------------------------------------------------------------------

    #[test]
    fn test_bar_mismatch_corrected_topic_and_severity() {
        let event = NotificationEvent::BarMismatchCorrectedFromHistorical {
            compared_count: 222,
            mismatches_count: 5,
            sample_symbols: vec!["SENSEX".to_string()],
            cross_check_pass: "post_open_09_16_05",
        };
        assert_eq!(event.topic(), "BarMismatchCorrectedFromHistorical");
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_bar_mismatch_corrected_message_includes_counts_and_sample() {
        let event = NotificationEvent::BarMismatchCorrectedFromHistorical {
            compared_count: 222,
            mismatches_count: 5,
            sample_symbols: vec!["SENSEX".to_string(), "NIFTY".to_string()],
            cross_check_pass: "post_open_09_16_05",
        };
        let msg = event.to_message();
        assert!(msg.contains("222"));
        assert!(msg.contains("5"));
        assert!(msg.contains("SENSEX"));
        assert!(msg.contains("post_open_09_16_05"));
        assert!(msg.contains("Strategy gate is OPEN"));
    }

    #[test]
    fn test_bar_mismatch_inconclusive_topic_and_severity() {
        let event = NotificationEvent::BarMismatchCrossCheckInconclusive {
            compared_count: 150,
            expected_count: 222,
        };
        assert_eq!(event.topic(), "BarMismatchCrossCheckInconclusive");
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_bar_mismatch_inconclusive_message_says_trading_blocked() {
        let event = NotificationEvent::BarMismatchCrossCheckInconclusive {
            compared_count: 150,
            expected_count: 222,
        };
        let msg = event.to_message();
        assert!(msg.contains("INCONCLUSIVE"));
        assert!(msg.contains("TRADING BLOCKED"));
        assert!(msg.contains("150"));
        assert!(msg.contains("222"));
    }

    #[test]
    fn test_bar_mismatch_failed_topic_and_severity() {
        let event = NotificationEvent::BarMismatchCrossCheckFailed {
            reason: "all 222 Dhan REST fetches failed".to_string(),
        };
        assert_eq!(event.topic(), "BarMismatchCrossCheckFailed");
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_bar_mismatch_failed_message_includes_reason() {
        let event = NotificationEvent::BarMismatchCrossCheckFailed {
            reason: "all 222 Dhan REST fetches failed".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("HARD-FAILED"));
        assert!(msg.contains("all 222 Dhan REST fetches failed"));
        assert!(msg.contains("TRADING BLOCKED"));
    }

    // -----------------------------------------------------------------------
    // Phase 0 Item 22d — EndOfDayDigest (2026-05-15)
    // -----------------------------------------------------------------------

    #[test]
    fn test_end_of_day_digest_topic_and_severity_pinned() {
        let event = NotificationEvent::EndOfDayDigest {
            trading_date_ist: "2026-05-15".to_string(),
            main_feed_active: 1,
            main_feed_total: 1,
            token_remaining_hours: 20,
            feeds: vec![],
        };
        assert_eq!(event.topic(), "EndOfDayDigest");
        assert_eq!(event.severity(), Severity::Info);
    }

    #[test]
    fn test_end_of_day_digest_happy_path_message() {
        // Feed up, token headroom > 12h — no warning lines.
        let event = NotificationEvent::EndOfDayDigest {
            trading_date_ist: "2026-05-15".to_string(),
            main_feed_active: 1,
            main_feed_total: 1,
            token_remaining_hours: 20,
            feeds: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("End-of-day digest @ 15:31:30 IST"));
        assert!(msg.contains("Trading date: 2026-05-15"));
        assert!(msg.contains("Main feed: 1/1"));
        assert!(msg.contains("Token headroom: 20h"));
        assert!(msg.contains("Feed stayed up through close."));
        // No token warning when headroom is healthy.
        assert!(!msg.contains("refresh TOTP"));
    }

    #[test]
    fn test_end_of_day_digest_low_token_headroom_adds_action_line() {
        // < 12h until expiry — must instruct operator to refresh TOTP.
        let event = NotificationEvent::EndOfDayDigest {
            trading_date_ist: "2026-05-15".to_string(),
            main_feed_active: 1,
            main_feed_total: 1,
            token_remaining_hours: 8,
            feeds: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("Token headroom: 8h"));
        assert!(msg.contains("refresh TOTP before 09:00 IST"));
    }

    #[test]
    fn test_end_of_day_digest_feed_down_at_close_swaps_status_line() {
        // Critical-looking signal in a Severity::Info envelope —
        // operator must SEE "check overnight logs" but never get
        // paged (that's what MarketOpenStreamingFailed at next open
        // is for).
        let event = NotificationEvent::EndOfDayDigest {
            trading_date_ist: "2026-05-15".to_string(),
            main_feed_active: 0,
            main_feed_total: 1,
            token_remaining_hours: 20,
            feeds: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("Main feed: 0/1"));
        assert!(msg.contains("check overnight logs"));
        assert!(!msg.contains("stayed up"));
    }

    #[test]
    fn test_end_of_day_digest_boundary_12h_uses_warning_path() {
        // The boundary `< 12` is strict — exactly 12h passes the
        // healthy check (operator does NOT need to refresh until
        // less than 12h are left).
        let healthy = NotificationEvent::EndOfDayDigest {
            trading_date_ist: "2026-05-15".to_string(),
            main_feed_active: 1,
            main_feed_total: 1,
            token_remaining_hours: 12,
            feeds: vec![],
        };
        assert!(!healthy.to_message().contains("refresh TOTP"));
        let warn = NotificationEvent::EndOfDayDigest {
            trading_date_ist: "2026-05-15".to_string(),
            main_feed_active: 1,
            main_feed_total: 1,
            token_remaining_hours: 11,
            feeds: vec![],
        };
        assert!(warn.to_message().contains("refresh TOTP"));
    }

    // -----------------------------------------------------------------------
    // Feed-agnostic Telegram parity (operator directive 2026-07-03):
    // FeedStatusLine block + FeedInstrumentsLoaded
    // -----------------------------------------------------------------------

    fn feed_lines() -> Vec<FeedStatusLine> {
        vec![
            FeedStatusLine {
                name: "Dhan".to_string(),
                instruments: Some(776),
                last_tick_age_secs: Some(0),
            },
            FeedStatusLine {
                name: "Groww".to_string(),
                instruments: Some(768),
                last_tick_age_secs: Some(2),
            },
        ]
    }

    #[test]
    fn test_feed_status_block_renders_known_values() {
        let block = format_feed_status_block(&feed_lines());
        assert!(block.contains("Market-data feeds:"), "got: {block}");
        assert!(
            block.contains("Dhan: 776 instruments — last tick 0s ago ✓"),
            "got: {block}"
        );
        assert!(
            block.contains("Groww: 768 instruments — last tick 2s ago ✓"),
            "got: {block}"
        );
    }

    #[test]
    fn test_feed_status_block_renders_unknowns_honestly() {
        // A feed with no wired health data must say "unknown" — never a
        // fabricated number (audit Rule 11 mirror).
        let block = format_feed_status_block(&[FeedStatusLine {
            name: "Groww".to_string(),
            instruments: None,
            last_tick_age_secs: None,
        }]);
        assert!(
            block.contains("Groww: instruments unknown — no tick seen yet"),
            "got: {block}"
        );
        // Ages past 120s render as minutes with NO fault emoji (post-close
        // silence is normal — the digest must not read as a false red).
        let block = format_feed_status_block(&[FeedStatusLine {
            name: "Dhan".to_string(),
            instruments: Some(776),
            last_tick_age_secs: Some(180),
        }]);
        assert!(block.contains("last tick 3m ago"), "got: {block}");
        assert!(!block.contains('❌'), "no false red: {block}");
        // Empty list must render an explicit "unknown", never vanish.
        let empty = format_feed_status_block(&[]);
        assert!(empty.contains("Market-data feeds: unknown"), "got: {empty}");
    }

    #[test]
    fn test_pool_online_message_lists_every_enabled_feed() {
        let ev = NotificationEvent::WebSocketPoolOnline {
            connected: 1,
            total: 1,
            per_connection: vec![(776, 5_000, Some(0))],
            boot_path: BootPathLabel::Slow,
            boot_wall_clock_secs: 1.2,
            last_real_tick_age_secs: Some(0),
            feeds: feed_lines(),
        };
        let msg = ev.to_message();
        assert!(msg.contains("Market-data feeds:"), "got: {msg}");
        assert!(msg.contains("Dhan: 776 instruments"), "got: {msg}");
        assert!(msg.contains("Groww: 768 instruments"), "got: {msg}");
        // The header now honestly labels the connection count as Dhan's.
        assert!(msg.contains("Dhan: All 1 of 1"), "got: {msg}");
    }

    #[test]
    fn test_end_of_day_digest_lists_every_enabled_feed() {
        let ev = NotificationEvent::EndOfDayDigest {
            trading_date_ist: "2026-07-03".to_string(),
            main_feed_active: 1,
            main_feed_total: 1,
            token_remaining_hours: 20,
            feeds: feed_lines(),
        };
        let msg = ev.to_message();
        assert!(msg.contains("Market-data feeds:"), "got: {msg}");
        assert!(msg.contains("Dhan: 776 instruments"), "got: {msg}");
        assert!(msg.contains("Groww: 768 instruments"), "got: {msg}");
    }

    #[test]
    fn test_feed_instruments_loaded_message_and_severity() {
        let ev = NotificationEvent::FeedInstrumentsLoaded {
            feed: "Groww".to_string(),
            subscribed: 768,
            indices: 2,
            stocks: 766,
            skipped: 5,
        };
        assert_eq!(ev.topic(), "FeedInstrumentsLoaded");
        assert_eq!(ev.severity(), Severity::Info);
        // Boot-success milestone — must ship instantly like the Dhan
        // instruments ping it mirrors, never coalesced 60s.
        assert_eq!(ev.dispatch_policy(), DispatchPolicy::Immediate);
        let msg = ev.to_message();
        assert!(msg.contains("Groww instruments loaded"), "got: {msg}");
        assert!(
            msg.contains("Subscribed 768 instruments (2 indices + 766 stocks)"),
            "got: {msg}"
        );
        assert!(
            msg.contains("Skipped 5 that could not be matched today"),
            "got: {msg}"
        );
    }

    #[test]
    fn test_feed_instruments_loaded_skipped_zero_wording() {
        let ev = NotificationEvent::FeedInstrumentsLoaded {
            feed: "Groww".to_string(),
            subscribed: 770,
            indices: 2,
            stocks: 768,
            skipped: 0,
        };
        let msg = ev.to_message();
        assert!(msg.contains("Every instrument matched"), "got: {msg}");
        assert!(!msg.contains("Skipped"), "got: {msg}");
    }

    // -----------------------------------------------------------------------
    // Groww boot-visibility parity (operator directive 2026-07-04):
    // FeedAuthOk + FeedConnectedAwaitingTicks + feed-aware off-hours boot
    // -----------------------------------------------------------------------

    #[test]
    fn test_feed_auth_ok_message_topic_severity() {
        let ev = NotificationEvent::FeedAuthOk {
            feed: "Groww".to_string(),
        };
        assert_eq!(ev.topic(), "FeedAuthOk");
        assert_eq!(ev.severity(), Severity::Low);
        // Boot milestone — ships instantly like Dhan's AuthenticationSuccess.
        assert_eq!(ev.dispatch_policy(), DispatchPolicy::Immediate);
        let msg = ev.to_message();
        assert!(msg.contains("Groww Auth OK"), "got: {msg}");
        assert!(msg.contains("access token in hand"), "got: {msg}");
    }

    #[test]
    fn test_feed_connected_awaiting_ticks_market_closed_wording() {
        let ev = NotificationEvent::FeedConnectedAwaitingTicks {
            feed: "Groww".to_string(),
            subscribed: 768,
            market_open: false,
        };
        assert_eq!(ev.topic(), "FeedConnectedAwaitingTicks");
        assert_eq!(ev.severity(), Severity::Low);
        assert_eq!(ev.dispatch_policy(), DispatchPolicy::Immediate);
        let msg = ev.to_message();
        assert!(msg.contains("Groww feed connected"), "got: {msg}");
        assert!(msg.contains("Subscribed 768 instruments"), "got: {msg}");
        // FALSE-OK TRAP (operator 2026-07-04 + audit Rule 11): the boot-time
        // connect ping must say "awaiting first tick" — socket-connected is
        // NOT streaming, and on a closed market the idle is normal.
        assert!(msg.contains("awaiting first tick"), "got: {msg}");
        assert!(msg.contains("market closed — idle is normal"), "got: {msg}");
        assert!(
            !msg.contains("streaming"),
            "must never claim streaming: {msg}"
        );
    }

    #[test]
    fn test_feed_connected_awaiting_ticks_market_open_wording_never_claims_streaming() {
        let ev = NotificationEvent::FeedConnectedAwaitingTicks {
            feed: "Groww".to_string(),
            subscribed: 768,
            market_open: true,
        };
        let msg = ev.to_message();
        assert!(msg.contains("awaiting first tick"), "got: {msg}");
        assert!(
            msg.contains("market open — ticks should arrive shortly"),
            "got: {msg}"
        );
        assert!(
            !msg.contains("streaming"),
            "must never claim streaming: {msg}"
        );
        assert!(!msg.contains("idle is normal"), "wrong suffix: {msg}");
    }

    #[test]
    fn test_pool_deferred_off_hours_lists_enabled_feeds() {
        // Operator 2026-07-04: the Saturday "Boot complete" message must name
        // EVERY enabled feed — not just the Dhan connection count.
        let ev = NotificationEvent::WebSocketPoolDeferredOffHours {
            deferred: 1,
            total: 1,
            boot_path: BootPathLabel::Slow,
            feeds: feed_lines(),
        };
        let msg = ev.to_message();
        assert!(msg.contains("Boot complete"), "got: {msg}");
        // The header count is honestly labelled as the Dhan pool.
        assert!(msg.contains("Dhan connections DEFERRED"), "got: {msg}");
        assert!(msg.contains("Market-data feeds:"), "got: {msg}");
        assert!(msg.contains("Groww: 768 instruments"), "got: {msg}");
    }

    // -----------------------------------------------------------------------
    // CrossVerify1mSummary + CrossVerify1mAborted (2026-06-10 — visibility)
    // -----------------------------------------------------------------------

    fn cv_summary(
        compared: usize,
        mismatches: usize,
        missing: usize,
        degraded: bool,
    ) -> NotificationEvent {
        NotificationEvent::CrossVerify1mSummary {
            trading_date_ist: "2026-06-10".to_string(),
            instruments: 243,
            compared,
            mismatches,
            missing,
            degraded,
        }
    }

    #[test]
    fn test_cross_verify_1m_summary_clean_is_info() {
        let event = cv_summary(91_230, 0, 0, false);
        assert_eq!(event.topic(), "CrossVerify1mSummary");
        assert_eq!(event.severity(), Severity::Info);
    }

    #[test]
    fn test_cross_verify_1m_summary_mismatch_is_high() {
        // Exactly one mismatch must already page (no tolerance band).
        assert_eq!(cv_summary(91_230, 1, 0, false).severity(), Severity::High);
    }

    #[test]
    fn test_cross_verify_1m_summary_missing_is_high() {
        // Pre-impl finding H1: missing minutes ARE missed live candles —
        // the very signal this audit exists for. Never a green PASS.
        assert_eq!(cv_summary(91_230, 0, 1, false).severity(), Severity::High);
    }

    #[test]
    fn test_cross_verify_1m_summary_degraded_is_high() {
        // False-OK guard: degraded coverage with zero mismatches must
        // still page (audit Rule 11).
        assert_eq!(cv_summary(91_230, 0, 0, true).severity(), Severity::High);
    }

    #[test]
    fn test_cross_verify_1m_summary_compared_zero_is_high() {
        // Pre-impl finding C1: Dhan serving empty 200-responses yields
        // compared=0 with fetch_failures=0 — must NEVER read as PASS.
        let event = cv_summary(0, 0, 0, false);
        assert_eq!(event.severity(), Severity::High);
        assert!(
            event.to_message().contains("Nothing could be compared"),
            "compared==0 must carry the honest no-coverage wording"
        );
    }

    /// RATCHET (task DHAN-REST-400 item 2, 2026-06-10 incident): a
    /// 776/776-fetch-failure day must say BLIND loudly — the empty-report
    /// trap is spelled out so a header-only CSV can never again read as a
    /// perfect day.
    #[test]
    fn test_cross_verify_1m_summary_blind_message_is_loud() {
        let msg = cv_summary(0, 0, 0, true).to_message();
        assert!(msg.contains("BLIND"), "{msg}");
        assert!(msg.contains("Checked NOTHING today"), "{msg}");
        assert!(
            msg.contains("NOT because the data was perfect"),
            "the empty-report trap must be spelled out: {msg}"
        );
        assert!(msg.contains("REST health alerts"), "{msg}");
        assert!(!msg.contains("PASS"), "BLIND must never render PASS: {msg}");
    }

    #[test]
    fn test_cross_verify_1m_summary_clean_message_format() {
        // 10-commandments check: emoji-first subject, IST 12-hour time,
        // real numbers, no file paths, no library names.
        let msg = cv_summary(91_230, 0, 0, false).to_message();
        assert!(msg.starts_with('\u{2705}'), "clean summary leads with ✅");
        assert!(msg.contains("3:31 PM IST"), "IST 12-hour time");
        assert!(msg.contains("PASS"));
        assert!(msg.contains("Date: 2026-06-10"));
        assert!(msg.contains("Instruments: 243"));
        assert!(msg.contains("Minutes compared: 91230"));
        assert!(msg.contains("matches the exchange record exactly"));
        assert!(!msg.contains("data/"), "no file paths in operator text");
        assert!(!msg.contains("QuestDB"), "no infrastructure names");
    }

    #[test]
    fn test_cross_verify_1m_summary_failure_message_has_action_lines() {
        let msg = cv_summary(91_230, 42, 15, false).to_message();
        assert!(msg.starts_with('\u{26a0}'), "failure summary leads with ⚠️");
        assert!(msg.contains("NEEDS ATTENTION"));
        assert!(msg.contains("Mismatches: 42 | Missing: 15"));
        assert!(msg.contains("What to do RIGHT NOW"));
        assert!(msg.contains("Cross-verify card on the operator portal"));
    }

    #[test]
    fn test_cross_verify_1m_summary_degraded_message_has_partial_wording() {
        // Pre-impl finding L3: the no-JWT path returns degraded with
        // instruments=0 — the message must read honestly, not as a PASS.
        let event = NotificationEvent::CrossVerify1mSummary {
            trading_date_ist: "2026-06-10".to_string(),
            instruments: 243,
            compared: 80_000,
            mismatches: 0,
            missing: 0,
            degraded: true,
        };
        let msg = event.to_message();
        assert!(msg.contains("Coverage was PARTIAL"));
        assert!(msg.contains("cannot vouch for"));
        assert!(!msg.contains("PASS"));
    }

    #[test]
    fn test_cross_verify_1m_summary_dispatch_is_immediate() {
        // Pre-impl finding M1: the Info variant must NOT coalesce for 60s
        // and re-render as a bucket summary — it is the once-per-day
        // 15:31 deliverable.
        assert_eq!(
            cv_summary(91_230, 0, 0, false).dispatch_policy(),
            DispatchPolicy::Immediate
        );
        assert_eq!(
            cv_summary(91_230, 42, 0, false).dispatch_policy(),
            DispatchPolicy::Immediate
        );
    }

    #[test]
    fn test_cross_verify_1m_aborted_is_high_with_action_lines() {
        let event = NotificationEvent::CrossVerify1mAborted {
            detail: "task panicked".to_string(),
        };
        assert_eq!(event.topic(), "CrossVerify1mAborted");
        assert_eq!(event.severity(), Severity::High);
        let msg = event.to_message();
        assert!(msg.contains("did NOT run"));
        assert!(msg.contains("Reason: task panicked"));
        assert!(msg.contains("What to do RIGHT NOW"));
    }

    // -----------------------------------------------------------------------
    // Coverage for the boot-message rendering branches adjacent to the
    // 2026-07-04 Groww boot-parity change (PR #1400 coverage gate: core
    // dipped 90.16% vs the 90.2% ratcheted floor — these pin the previously
    // uncovered cold arms; the floor is NEVER lowered).
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_ping_freshness_bands() {
        // None = no inbound frame yet — explicit dash, never a fake "0s".
        assert_eq!(format_ping_freshness(None), "—");
        // 0–10s: healthy (Dhan pings every 10s).
        assert!(format_ping_freshness(Some(5)).contains('✓'));
        // 11–30s: still alive but flagged.
        assert!(format_ping_freshness(Some(20)).contains('⚠'));
        // >30s: watchdog territory.
        assert!(format_ping_freshness(Some(45)).contains('❌'));
    }

    #[test]
    fn test_feed_status_block_mid_age_band_is_neutral() {
        // 31–120s band: age shown WITHOUT a verdict emoji (neither the
        // healthy ✓ nor a false-red ❌ — post-lull silence is normal).
        let block = format_feed_status_block(&[FeedStatusLine {
            name: "Groww".to_string(),
            instruments: Some(768),
            last_tick_age_secs: Some(90),
        }]);
        assert!(block.contains("last tick 90s ago"), "got: {block}");
        assert!(
            !block.contains('✓'),
            "31-120s band must not claim healthy: {block}"
        );
        assert!(
            !block.contains('❌'),
            "31-120s band must not claim broken: {block}"
        );
    }

    #[test]
    fn test_pool_online_message_zero_capacity_and_no_real_ticks_is_honest() {
        // Zero-capacity per-connection rows must render 0% (no div-by-zero)
        // and zero REAL ticks must render the loud NONE warning (2026-06-02
        // false-OK fix), never a fabricated freshness.
        let ev = NotificationEvent::WebSocketPoolOnline {
            connected: 1,
            total: 1,
            per_connection: vec![(0, 0, None)],
            boot_path: BootPathLabel::Slow,
            boot_wall_clock_secs: 1.0,
            last_real_tick_age_secs: None,
            feeds: vec![],
        };
        let msg = ev.to_message();
        assert!(msg.contains("NONE captured yet"), "got: {msg}");
        assert!(msg.contains("0% full"), "zero capacity renders 0%: {msg}");
    }

    #[test]
    fn test_pool_online_message_full_connection_and_fresh_tick() {
        let ev = NotificationEvent::WebSocketPoolOnline {
            connected: 1,
            total: 1,
            per_connection: vec![(100, 100, Some(2))],
            boot_path: BootPathLabel::Slow,
            boot_wall_clock_secs: 1.0,
            last_real_tick_age_secs: Some(3),
            feeds: vec![],
        };
        let msg = ev.to_message();
        assert!(msg.contains("(full)"), "100% renders as 'full': {msg}");
        assert!(
            msg.contains("Real market ticks: last one 3s ago"),
            "got: {msg}"
        );
    }

    #[test]
    fn test_ws_pool_degraded_recovered_halt_messages() {
        let d = NotificationEvent::WebSocketPoolDegraded { down_secs: 42 }.to_message();
        assert!(
            d.contains("WS POOL DEGRADED") && d.contains("42"),
            "got: {d}"
        );
        let r = NotificationEvent::WebSocketPoolRecovered { was_down_secs: 7 }.to_message();
        assert!(r.contains("recovered") && r.contains('7'), "got: {r}");
        let h = NotificationEvent::WebSocketPoolHalt { down_secs: 301 }.to_message();
        assert!(h.contains("WS POOL HALT") && h.contains("301"), "got: {h}");
    }

    #[test]
    fn test_market_open_streaming_and_readiness_messages() {
        let ok = NotificationEvent::MarketOpenStreamingConfirmation {
            main_feed_active: 1,
            main_feed_total: 1,
            order_update_active: true,
        }
        .to_message();
        assert!(
            ok.contains("Streaming live") && ok.contains("1/1"),
            "got: {ok}"
        );
        let fail = NotificationEvent::MarketOpenStreamingFailed {
            main_feed_active: 0,
            main_feed_total: 1,
            order_update_active: false,
        }
        .to_message();
        assert!(
            fail.contains("STREAMING FAILED") && fail.contains("0/1"),
            "got: {fail}"
        );
        let ready = NotificationEvent::MarketOpenReadinessConfirmation {
            main_feed_active: 1,
            main_feed_total: 1,
            order_update_active: true,
            token_remaining_secs: 7200,
        }
        .to_message();
        assert!(
            ready.contains("READY for market open") && ready.contains("2.0h"),
            "got: {ready}"
        );
    }

    #[test]
    fn test_ws_sleep_wake_and_order_update_lifecycle_messages() {
        // WS-GAP-04 dormant sleep/wake + AUTH-GAP-03 wake-time renewal +
        // order-update lifecycle arms (previously uncovered cold branches).
        let sleep = NotificationEvent::WebSocketSleepEntered {
            feed: "main".to_string(),
            connection_index: 0,
            sleep_secs: 3600,
        }
        .to_message();
        assert!(
            sleep.contains("sleeping (slot 1)") && sleep.contains("3600"),
            "got: {sleep}"
        );
        let wake = NotificationEvent::WebSocketSleepResumed {
            feed: "main".to_string(),
            connection_index: 0,
            slept_for_secs: 3600,
        }
        .to_message();
        assert!(
            wake.contains("waking (slot 1)") && wake.contains("attempting reconnect"),
            "got: {wake}"
        );
        let renewed = NotificationEvent::WebSocketTokenForceRenewedOnWake {
            feed: "main".to_string(),
            connection_index: 0,
            remaining_secs_before: 100,
            threshold_secs: 14400,
        }
        .to_message();
        assert!(
            renewed.contains("wake-time token renewed") && renewed.contains("14400"),
            "got: {renewed}"
        );
        let connected = NotificationEvent::OrderUpdateConnected.to_message();
        assert!(
            connected.contains("Order Update WS connected"),
            "got: {connected}"
        );
        let authed = NotificationEvent::OrderUpdateAuthenticated.to_message();
        assert!(authed.contains("streaming live"), "got: {authed}");
        let reconnected = NotificationEvent::OrderUpdateReconnected {
            consecutive_failures: 3,
        }
        .to_message();
        assert!(
            reconnected.contains("reconnected") && reconnected.contains('3'),
            "got: {reconnected}"
        );
        let silent = NotificationEvent::NoLiveTicksDuringMarketHours {
            silent_for_secs: 120,
            threshold_secs: 60,
        }
        .to_message();
        assert!(
            silent.contains("zero live ticks") && silent.contains("120"),
            "got: {silent}"
        );
    }
}
