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
        /// Per-minute spot 1m REST leg armed at boot (config truth —
        /// 2026-07-13 operator visibility rider: the boot Telegram must say
        /// what the per-minute price capture will actually do today).
        spot_1m_enabled: bool,
        /// How many indices the spot leg pulls (4 since 2026-07-13).
        spot_1m_indices: u32,
        /// Per-minute option-chain REST leg armed at boot (config truth).
        chain_1m_enabled: bool,
        /// How many underlyings the chain leg pulls (3 — VIX is spot-only).
        chain_1m_underlyings: u32,
    },

    /// Dhan authentication token acquired at boot.
    AuthenticationSuccess,

    /// Dhan login could not be obtained (family-(3) of the 2026-07-14
    /// Dhan noise lock — the ONE Dhan token condition that pages).
    /// Emitters: the boot-time auth failure AND the mid-session
    /// watchdog's TERMINAL forced-re-mint failure. Body names the broker
    /// + the consequence (the spot-1m / option-chain pulls stop).
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

    // `MidSessionProfileInvalidated` DELETED 2026-07-14 (operator Dhan
    // noise lock — dhan-rest-only-noise-lock-2026-07-14.md): the 900s
    // profile probe runs SILENTLY (coded error! + counters); a terminal
    // forced-re-mint failure routes to the family-(3)
    // `AuthenticationFailed` Critical instead.
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

    /// Daily timeframe-consistency verifier (operator directive 2026-07-13:
    /// *"how will you guarantee that all our defined timeframes internally
    /// are correct"*): the ONE 3:40 PM IST summary covering BOTH passes —
    /// Dhan (today) and Groww (previous trading day). Severity AND wording
    /// derive from `status_label` (the verifier's flush-adjusted verdict —
    /// L6 fix, never re-derived from the counts, which can disagree on
    /// edge days): `Info` only for `pass` / `no_data`; everything else is
    /// `High`. A `buckets_compared == 0` run with data expected must NEVER
    /// read as PASS (audit Rule 11 — the BLIND wording).
    TfConsistencySummary {
        /// The Dhan trading day verified, `YYYY-MM-DD` IST.
        dhan_date_ist: String,
        /// The Groww trading day verified (previous trading day), IST.
        groww_date_ist: String,
        /// Instruments examined across both passes.
        instruments: u64,
        /// Higher-timeframe candles present on BOTH sides and compared.
        buckets_compared: u64,
        /// Field-cells where a stored candle disagrees with its recompute.
        mismatches: u64,
        /// Windows with 1-minute data but NO stored higher-TF candle.
        missing_tf_rows: u64,
        /// Stored higher-TF candles with ZERO 1-minute rows behind them.
        no_coverage: u64,
        /// Stored candles whose timestamp is off the 9:15 AM grid.
        off_grid: u64,
        /// Duplicate rows sharing one storage key.
        duplicates: u64,
        /// Groww end-of-day buckets that are never sealed on the
        /// production schedule (the system stops before midnight) — not
        /// verified BY DESIGN, never a page (H1 carve-out).
        tail_unsealed: u64,
        /// True when any query/flush/budget leg degraded — the run cannot
        /// vouch for the full universe.
        degraded: bool,
        /// True when findings exceeded the stored-detail cap (counts stay
        /// exact; only the per-row detail was truncated).
        truncated: bool,
        /// Stable run verdict: `pass` / `mismatch` / `degraded` / `blind`
        /// / `no_data`.
        status_label: String,
        /// Up to 10 plain-English worst offenders.
        top_detail: Vec<String>,
    },

    /// The daily timeframe-consistency TASK died (panicked/failed) before
    /// producing its summary. High so the absence of the daily verdict is
    /// impossible to miss. NOT fired on graceful shutdown/cancellation
    /// (the 16:30 IST auto-stop is a normal teardown, not an abort).
    TfConsistencyAborted {
        /// Plain-English description of how the task died.
        detail: String,
    },

    /// Per-minute spot 1m REST pipeline (operator grant 2026-07-12): the
    /// per-minute pull of the just-closed minute's official index candle
    /// has fully failed (no index succeeded) for several minutes in a row.
    /// Fires ONCE per failing episode (edge-triggered, audit-findings
    /// Rule 4); re-armed only after a successful minute. Severity::High.
    Spot1mFetchDegraded {
        /// How many minutes in a row have fully failed.
        consecutive_failed_minutes: u32,
        /// The most recent failed minute, IST 12-hour (e.g. "10:42 AM").
        minute_ist: String,
    },

    /// The per-minute index candle pull RECOVERED after a failing episode
    /// (falling edge — one Info ping so the operator knows it self-healed;
    /// the missing minutes stay absent until re-pulled, never fabricated).
    Spot1mFetchRecovered {
        /// The minute that succeeded, IST 12-hour (e.g. "10:45 AM").
        minute_ist: String,
        /// How many minutes had fully failed during the episode.
        failed_minutes: u32,
    },

    /// Groww per-minute spot 1m REST leg (operator grant 2026-07-13): the
    /// per-minute pull of the just-closed minute's official index candle
    /// from the SECOND broker (Groww) has fully failed for several minutes
    /// in a row. Fires ONCE per failing episode (edge-triggered, Rule 4);
    /// re-armed only after a successful minute. Severity::High.
    GrowwSpot1mFetchDegraded {
        /// How many minutes in a row have fully failed.
        consecutive_failed_minutes: u32,
        /// The most recent failed minute, IST 12-hour (e.g. "10:42 AM").
        minute_ist: String,
    },

    /// The Groww per-minute index candle pull RECOVERED after a failing
    /// episode (falling edge — one Info ping; the missing minutes stay
    /// absent until re-pulled, never fabricated).
    GrowwSpot1mFetchRecovered {
        /// The minute that succeeded, IST 12-hour (e.g. "10:45 AM").
        minute_ist: String,
        /// How many minutes had fully failed during the episode.
        failed_minutes: u32,
    },

    /// Per-SID persistent-empty detector (operator scope addition
    /// 2026-07-13, relayed via the coordinator session — the INDIA VIX
    /// live-probe companion): ONE index accumulated N consecutive
    /// empty/failed minutes in the per-minute spot pull WHILE the other
    /// indices succeeded in those same minutes — the vendor is not serving
    /// THIS index, not a general outage. Fires ONCE per SID per episode
    /// (edge-latched, Rule 4); re-armed only by that SID's own recovery.
    /// Severity::High.
    Spot1mSidNotServed {
        /// The affected index (e.g. "INDIA VIX").
        symbol: String,
        /// How many counted minutes in a row this index went unserved.
        consecutive_minutes: u32,
    },

    /// A previously-not-served index is being served again (falling edge —
    /// one Info ping; the missing minutes stay absent until re-pulled,
    /// never fabricated).
    Spot1mSidServedRecovered {
        /// The recovered index (e.g. "INDIA VIX").
        symbol: String,
        /// How many counted minutes the index went unserved.
        not_served_minutes: u32,
    },

    /// Groww per-minute option-chain REST leg (operator grant 2026-07-13,
    /// PR-3 of the Groww per-minute REST plan): the per-minute Groww
    /// option-chain snapshot has fully failed for several minutes in a row
    /// (edge-triggered ONCE per episode, Rule 4; re-armed only after a
    /// successful minute). Severity::High.
    GrowwChain1mFetchDegraded {
        /// How many minutes in a row have fully failed.
        consecutive_failed_minutes: u32,
        /// The most recent failed minute, IST 12-hour (e.g. "10:42 AM").
        minute_ist: String,
    },

    /// The Groww per-minute option-chain snapshot RECOVERED after a
    /// failing episode (falling edge — one Info ping; the missing minutes
    /// stay absent until re-pulled, never fabricated).
    GrowwChain1mFetchRecovered {
        /// The minute that succeeded, IST 12-hour (e.g. "10:45 AM").
        minute_ist: String,
        /// How many minutes had fully failed during the episode.
        failed_minutes: u32,
    },

    /// Per-underlying not-served detector on the Groww chain leg
    /// (2026-07-14 — the NIFTY expiry-day vendor-cutoff companion): ONE
    /// underlying accumulated N consecutive empty/failed minutes in the
    /// per-minute Groww option-chain pull WHILE the other underlyings
    /// succeeded in those same minutes — the vendor is not serving THIS
    /// underlying's chain, not a general outage. Fires ONCE per
    /// underlying per episode (edge-latched, Rule 4); re-armed only by
    /// that underlying's own recovery. Severity::High.
    GrowwChain1mUnderlyingNotServed {
        /// The affected underlying (a pinned plain symbol, e.g. "NIFTY").
        underlying: &'static str,
        /// How many counted minutes in a row this underlying's chain
        /// went unserved.
        empty_minutes: u32,
    },

    /// A previously-not-served underlying's chain is being served again
    /// (falling edge — one Info ping; the missing minutes stay absent
    /// until re-pulled, never fabricated).
    GrowwChain1mUnderlyingServedRecovered {
        /// The recovered underlying (a pinned plain symbol, e.g. "NIFTY").
        underlying: &'static str,
        /// How many counted minutes the underlying's chain went unserved.
        empty_minutes: u32,
    },

    /// The Groww chain leg could not resolve today's option expiry for one
    /// or more underlyings from the daily instruments list (list download
    /// failed after bounded tries, or the list carried no usable option
    /// rows) — those underlyings' chain recording stays OFF for the day
    /// (expiry dates are never guessed). One HIGH page per day.
    GrowwChain1mExpiryUnresolved {
        /// Plain-English detail naming the affected underlyings / cause
        /// (already secret-redacted + bounded at the emit site).
        detail: String,
    },

    /// The boot-time Groww option-chain probe verdict (pipeline switched
    /// OFF, probe-and-report ON): one Info ping carrying the MEASURED
    /// result — whether the chain answered, how many strikes, how fast, or
    /// which reject class — so the operator can decide to turn recording
    /// on. Nothing was recorded either way.
    GrowwChain1mProbeVerdict {
        /// `true` when every underlying's chain call answered with a
        /// parseable chain.
        ok: bool,
        /// Plain-English measured detail (already secret-redacted +
        /// bounded at the emit site).
        detail: String,
    },

    /// Groww per-minute PER-CONTRACT candle REST leg (operator grant
    /// 2026-07-13, PR-4 of the Groww per-minute REST plan — the fill-model
    /// leg): the per-minute contract candle pull has fully failed for
    /// several minutes in a row (edge-triggered ONCE per episode, Rule 4;
    /// re-armed only after a successful minute). Severity::High.
    GrowwContract1mFetchDegraded {
        /// How many minutes in a row have fully failed.
        consecutive_failed_minutes: u32,
        /// The most recent failed minute, IST 12-hour (e.g. "10:42 AM").
        minute_ist: String,
    },

    /// The Groww per-minute contract candle pull RECOVERED after a failing
    /// episode (falling edge — one Info ping; the missing minutes stay
    /// absent until re-pulled, never fabricated).
    GrowwContract1mFetchRecovered {
        /// The minute that succeeded, IST 12-hour (e.g. "10:45 AM").
        minute_ist: String,
        /// How many minutes had fully failed during the episode.
        failed_minutes: u32,
    },

    /// The Groww contract leg could not build today's contract book for
    /// one or more underlyings from the daily instruments list (list
    /// download failed after bounded tries, or the list carried no usable
    /// option contracts at the current expiry) — those underlyings'
    /// per-contract recording stays OFF for the day (contract identities
    /// are never guessed). One HIGH page per day.
    GrowwContract1mBookUnresolved {
        /// Plain-English detail naming the affected underlyings / cause
        /// (already secret-redacted + bounded at the emit site).
        detail: String,
    },

    /// Per-minute option-chain REST pipeline (operator grant 2026-07-12,
    /// PR-3): the per-minute option-chain snapshot has fully failed for
    /// several minutes in a row (edge-triggered ONCE per episode, Rule 4;
    /// re-armed only after a successful minute). Severity::High.
    ChainFetchDegraded {
        /// How many minutes in a row have fully failed.
        consecutive_failed_minutes: u32,
        /// The most recent failed minute, IST 12-hour (e.g. "10:42 AM").
        minute_ist: String,
    },

    /// The per-minute option-chain snapshot RECOVERED after a failing
    /// episode (falling edge — one Info ping; the missing minutes stay
    /// absent until re-pulled, never fabricated).
    ChainFetchRecovered {
        /// The minute that succeeded, IST 12-hour (e.g. "10:45 AM").
        minute_ist: String,
        /// How many minutes had fully failed during the episode.
        failed_minutes: u32,
    },

    /// The broker rejected the option-chain data request because the
    /// account has NO option-chain data subscription (entitlement absent —
    /// the DH-902 / DATA 806 class). Fired ONCE per day. Severity depends
    /// on intent: HIGH when the pipeline was switched ON and expected to
    /// record (actionable — the operator must fix the subscription or
    /// switch the setting off); INFO when it was only the boot-time
    /// probe-and-report verdict for a disabled pipeline.
    ChainEntitlementAbsent {
        /// `true` when the option-chain pipeline was enabled (expected to
        /// run); `false` for the probe-only verdict.
        pipeline_enabled: bool,
        /// Plain-English detail naming the reject class (already
        /// secret-redacted + bounded at the emit site).
        detail: String,
    },

    /// The boot-time option-chain probe CONFIRMED the account IS entitled
    /// to option-chain data while the pipeline is switched OFF — one Info
    /// ping telling the operator the recording can be turned on.
    ChainEntitlementConfirmed,

    /// The day-start expiry-date lookup for the option chain failed after
    /// bounded retries — the chain recording stays OFF for the day
    /// (expiry dates come ONLY from the broker's list, never guessed).
    /// One HIGH page per day (CHAIN-04).
    ChainExpirylistFailed {
        /// Plain-English detail (already secret-redacted + bounded at the
        /// emit site).
        detail: String,
    },

    /// Per-underlying not-served detector on the DHAN chain leg
    /// (2026-07-14 — the Dhan mirror of the Groww #1537 detector; the
    /// NIFTY expiry-day vendor-cutoff companion): ONE underlying
    /// accumulated N consecutive empty/failed minutes in the per-minute
    /// Dhan option-chain pull WHILE the other underlyings succeeded in
    /// those same minutes — the vendor is not serving THIS underlying's
    /// chain, not a general outage. Fires ONCE per underlying per episode
    /// (edge-latched, Rule 4); re-armed only by that underlying's own
    /// recovery. Severity::High. Noise-lock family-(2) extension per
    /// `dhan-rest-only-noise-lock-2026-07-14.md` §2.1.
    Chain1mUnderlyingNotServed {
        /// The affected underlying (a pinned plain symbol, e.g. "NIFTY").
        underlying: &'static str,
        /// How many counted minutes in a row this underlying's chain
        /// went unserved.
        empty_minutes: u32,
    },

    /// A previously-not-served underlying's Dhan chain is being served
    /// again (falling edge — one Info ping; the missing minutes stay
    /// absent until re-pulled, never fabricated, never copied across
    /// brokers).
    Chain1mUnderlyingServedRecovered {
        /// The recovered underlying (a pinned plain symbol, e.g. "NIFTY").
        underlying: &'static str,
        /// How many counted minutes the underlying's chain went unserved.
        empty_minutes: u32,
    },

    /// Once-per-trading-day Dhan-vs-Groww scorecard at 3:45 PM IST
    /// (operator directive 2026-07-10 — run both feeds live for a month,
    /// everything tracked + blame-attributed). Severity::Info +
    /// `DispatchPolicy::Immediate` (the CrossVerify1mSummary precedent —
    /// the daily digest must arrive AT 15:45, never coalesced). Any
    /// degradation is carried loudly in the body (partial/degraded
    /// footnotes), and a task death fires `DualFeedScorecardAborted`
    /// instead — the daily signal can never be silently dropped.
    DualFeedDailyScorecard {
        /// Trading date in `YYYY-MM-DD` IST format.
        trading_date_ist: String,
        /// The Dhan side of the scorecard.
        dhan: FeedScoreLine,
        /// The Groww side of the scorecard.
        groww: FeedScoreLine,
        /// Session denominator in minutes (375 on a regular NSE day).
        session_minutes: i64,
        /// `true` when the day's coverage could not be fully vouched for
        /// (a data source was unavailable mid-run).
        partial_coverage: bool,
        /// `true` when the connection-event record itself under-counted
        /// today — drop counts are a floor, not a truth.
        degraded: bool,
        /// `true` when the operator forced this run BEFORE the daily
        /// trigger — the card covers the day only up to the run time.
        early_run: bool,
        /// `true` when the app restarted mid-day (an in-market process
        /// death was detected) — records from before the restart may
        /// under-count, and the persisted row is stamped partial; the card
        /// must say so too (round-3 hostile review 2026-07-10: the row said
        /// partial while the card stayed silent).
        restart_partial: bool,
        /// `true` when Dhan was switched OFF for the day — a one-horse
        /// race: the verdict says "no contest" instead of declaring a
        /// winner (round-4 hostile review 2026-07-10).
        dhan_feed_off: bool,
        /// `true` when Groww was switched OFF for the day (round 4).
        groww_feed_off: bool,
        /// Official minute-candle pull digest lines (Groww REST plan PR-5,
        /// operator Quote 2 2026-07-13): one plain-English line per
        /// feed/pull-family answering "within how many seconds precisely"
        /// with MEASURED numbers. Empty = the digest is not wired for this
        /// card (older callers / tests) — the section is simply omitted.
        rest_legs: Vec<RestLegScoreLine>,
        /// `true` when the day's per-pull records could not be read while
        /// building this card — the candle-pull lines may under-count or
        /// read "not measured yet" (honest cause footnote, Rule 11).
        rest_legs_read_failed: bool,
    },

    /// The daily dual-feed scorecard TASK died (panicked / errored) before
    /// producing its summary. High so the ABSENCE of the daily scorecard is
    /// impossible to miss (mirrors `CrossVerify1mAborted`). NOT fired on
    /// graceful shutdown/cancellation.
    DualFeedScorecardAborted {
        /// Plain-English description of how the task died.
        detail: String,
    },

    /// Daily BruteX↔TickVault 1-minute cross-verify summary
    /// (BRUTEX-XVERIFY, 2026-07-12). One Info digest per trading day after
    /// the 15:50 IST run — compares BruteX-produced 1-minute candles against
    /// our live capture, minute by minute. Counts carry `-1` sentinels when
    /// a leg could not be measured (rendered as "?", never fabricated zeros
    /// — audit Rule 11).
    BrutexCrossverifySummary {
        /// Trading date in `YYYY-MM-DD` IST format.
        trading_date_ist: String,
        /// Fixed daily outcome slug:
        /// `clean|diverged|partial|no_data|blind|degraded`.
        outcome: String,
        /// BruteX files fetched and read for the day (`-1` = unknown).
        files_read: i64,
        /// Symbols paired and compared across both sides (`-1` = unknown).
        symbols_compared: i64,
        /// Compared minutes where every field agreed within tolerance.
        matched: i64,
        /// Compared minutes with a beyond-tolerance difference.
        diverged: i64,
        /// Minutes we have live but BruteX's files do not.
        missing_brutex: i64,
        /// Minutes BruteX has but our live capture does not.
        missing_live: i64,
        /// 15:28/15:29 minutes absent live only due to close-seal timing —
        /// informational, never counted as missing.
        tail_unsealed: i64,
        /// BruteX symbols that could not be paired to a live instrument.
        unmapped: i64,
        /// 95th-percentile absolute price difference across compared
        /// minutes, in paise (`-1` = not measured).
        noise_p95_paise: i64,
        /// Maximum absolute price difference observed, in paise
        /// (`-1` = not measured).
        noise_max_paise: i64,
        /// Pre-formatted worst-offender lines (plain English, one per
        /// line); empty when there is nothing noteworthy.
        top_offenders: String,
        /// Optional plain-English context line (e.g. a same-day feed stall
        /// that explains a gap); empty when there is none.
        hint: String,
    },

    /// The daily BruteX cross-verify TASK died (panicked / errored) before
    /// producing its summary. High so the ABSENCE of the daily check is
    /// impossible to miss (mirrors `DualFeedScorecardAborted`). NOT fired
    /// on graceful shutdown/cancellation.
    BrutexCrossverifyAborted {
        /// Trading date in `YYYY-MM-DD` IST format.
        trading_date_ist: String,
        /// Short FIXED failure classification (never raw external text).
        reason: String,
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

    // `TokenForcedRemintTriggered` DELETED 2026-07-14 (operator Dhan
    // noise lock): the AUTH-GAP-05 forced re-mint is a SILENT self-heal
    // (coded error! + tv_token_forced_remint_total counters); only a
    // TERMINAL re-mint failure pages, via `AuthenticationFailed`.
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
    ///
    /// 2026-07-06 fix (WS-GAP-10): emitted edge-triggered ONCE per outage
    /// episode from inside the reconnect loop at ≥3 consecutive in-market
    /// failures. The old emit at the main.rs task-exit point was
    /// unreachable — `run_order_update_connection` never returns since
    /// the WS-GAP-04 never-give-up rewrite.
    OrderUpdateDisconnected { reason: String },

    /// Order update WebSocket reconnected after a transient disconnect.
    ///
    /// 2026-07-06 fix: fires ONLY after the reconnected socket survives
    /// the 60s stability window (`ORDER_UPDATE_RECONNECT_STABILITY_SECS`)
    /// — the old connect-edge emission produced false-recovery storms
    /// when Dhan killed each socket ~10ms after login (dead-token
    /// incident, 2026-07-06). Time-survival, never frame-gated: the order
    /// stream is legitimately silent for hours on idle days.
    ///
    /// Severity intentionally downgraded to `Severity::Low` (was Medium
    /// before 2026-04-26). Dhan's order-update server idle-disconnects
    /// accounts that haven't placed an order in 30-60 minutes, so on a
    /// dry-run / paper-trading day this event fires 5-10 times. A
    /// successful recovery is operator-informational, not pager-worthy
    /// — MEDIUM at that volume is pure pager fatigue. The companion
    /// `OrderUpdateDisconnected` (Severity::High) still pages on the
    /// disconnect leg (unchanged: `test_order_update_reconnected_severity_is_low`).
    OrderUpdateReconnected { consecutive_failures: u32 },

    // 2026-07-14 operator Dhan noise lock: `NoLiveTicksDuringMarketHours`
    // (Critical) DELETED with the no-tick watchdog — its heartbeat was fed
    // ONLY by the Dhan tick pipeline; Groww stall detection is
    // FEED-STALL-01 + the market-hours-liveness alarm. See
    // .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md.
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

    /// A market-data feed went DOWN (operator directive 2026-07-06 — Groww
    /// feed-down alerting parity with the Dhan main feed). Fired on the
    /// feed's lifecycle FALLING edge (runtime disable / internal bridge
    /// death), exactly ONCE per DOWN episode (edge-latched via
    /// `GrowwAuditLatches::down_announced`, re-armed only when the feed is
    /// STREAMING again). Never claims upstream tick counts — it only states
    /// that prices from this feed will not flow until recovery. Severity is
    /// field-driven: High in-market (pages + SNS), Low off-hours (60s
    /// coalesced — the 2026-04-22 pre-market-spam precedent class).
    FeedDown {
        /// Feed display name (e.g. "Groww").
        feed: String,
        /// Fixed plain-English cause (never raw child text / URLs).
        reason: String,
        /// Whether the market was open at the falling edge — drives severity
        /// (High in-market, Low off-hours) and the message register.
        market_open: bool,
        /// `true` when the feed was DELIBERATELY switched off (the runtime
        /// feed toggle) — the body then says it stays OFF until re-enabled
        /// from the feeds page. `false` for involuntary falling edges
        /// (bridge death / internal restart) where the auto-retry trailer is
        /// honest. Without this split the operator-disable page falsely
        /// claimed "the system keeps retrying automatically" — a disabled
        /// feed is NEVER retried, so the message actively delayed the one
        /// action (re-enable) that fixes it (2026-07-06 fix, Telegram
        /// commandment 7 + audit Rule 11).
        operator_initiated: bool,
    },

    /// A previously-DOWN market-data feed is STREAMING again (operator
    /// directive 2026-07-06). Fired exactly ONCE per DOWN episode, on the
    /// STREAMING rising edge (a real tick / the feed's own streaming
    /// status) — never on mere socket-connect, preserving the
    /// connected ≠ streaming honesty split (`FeedConnectedAwaitingTicks`
    /// keeps announcing socket-connect separately with "awaiting first
    /// tick"). Severity::Medium — mirrors `WebSocketReconnected`.
    FeedRecovered {
        /// Feed display name (e.g. "Groww").
        feed: String,
        /// Total downtime of the episode in seconds (first-down-wins, so it
        /// spans intermediate retry failures).
        down_secs: u64,
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
    ///
    /// `fleet_summary` (exam-fix hardening 2026-07-06): `true` when `reason`
    /// is a FLEET-coalesced "N of M connections retrying" summary — the body
    /// then describes the partial-fleet condition instead of the single-conn
    /// total-outage trailer ("receiving nothing … prices will not flow"),
    /// which would contradict a partial summary with a false
    /// whole-feed-down claim.
    ///
    /// `detail` (operator demand 2026-07-14 — the 14:59 IST rejection page
    /// said only "the feed reported an error" with no WHY): the SANITIZED
    /// reject-cause signature, rendered into the headline as
    /// "Groww live feed rejected: {detail} — retrying". CONTRACT: the emit
    /// site MUST pass ONLY the output of the supervisor's
    /// `sidecar_line_signature` choke point (control-char + BiDi strip,
    /// credential/JWT redaction, 160-char cap) — NEVER raw child text. A
    /// `None` / empty / whitespace detail degrades to the pre-2026-07-14
    /// generic headline. This field is a conscious, dated override of
    /// Telegram commandment 2 (no library jargon) for the ONE field whose
    /// payload IS the machine cause — recorded in
    /// `.claude/rules/project/feed-stall-watchdog-error-codes.md` §1c.1
    /// (precedent: the B9 "Build:" short-SHA override). The render boundary
    /// re-caps + html-escapes it anyway (defense-in-depth).
    GrowwSidecarRejected {
        reason: String,
        fleet_summary: bool,
        detail: Option<String>,
    },

    /// W2 PR#5 (2026-07-10, audit follow-up row 15): the configured NSE
    /// holiday calendar's coverage horizon is running out (or already ran
    /// out). The holiday list covers one calendar year at a time and the
    /// trading-day gate has NO year bound — past the newest listed year,
    /// every un-listed weekday holiday silently reads as a trading day
    /// (the box starts, feeds connect, a full billable session burns on a
    /// market-closed day). Fired by the calendar-staleness watchdog
    /// (`crates/app/src/calendar_staleness.rs`), edge-latched to at most
    /// one page per process per IST day. Severity::High — demands operator
    /// action (paste the next official NSE circular into the config).
    HolidayCalendarCoverageLow {
        /// Signed days of coverage left (negative = already past the
        /// cliff); `None` = the configured holiday list is EMPTY
        /// (pathological — the config guards prevent it in prod, but the
        /// body renders it honestly instead of a fake number).
        days_remaining: Option<i64>,
        /// Human-readable last covered date, e.g. "31 Dec 2026".
        coverage_end_display: String,
    },

    /// Custom alert from any component. `Severity::High` → pages Telegram +
    /// SNS SMS. Reserve for genuinely operator-actionable conditions; for
    /// informational status pings use [`Self::CustomStatus`] instead.
    Custom { message: String },

    /// Informational, non-actionable status ping from any component
    /// (`Severity::Low` — batches into the digest / off-hours coalescer, never
    /// pages SNS SMS). Introduced 2026-07-10 to stop routine status pings
    /// ("market closed", "recovered saved prices", "service auto-restarted")
    /// firing as High + SMS. Not episode-routed (`episode_key() == None`), so
    /// the episode machinery is untouched.
    CustomStatus { message: String },

    /// Instant informational status ping (`Severity::Low` → NEVER SMS, but
    /// `DispatchPolicy::Immediate` → delivered instantly as its own message,
    /// NOT batched). Introduced 2026-07-10 for operator-notable-but-not-
    /// actionable events that must be SEEN at once without paging: a mid-market
    /// crash/restart recovery ("fast start") and the feed enable/disable toggle
    /// acknowledgements. Like [`Self::CustomStatus`] it is NOT episode-routed
    /// (`episode_key() == None`).
    CustomStatusUrgent { message: String },
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

/// One feed's side of the daily dual-feed scorecard (operator directive
/// 2026-07-10). `-1` on any count means "not measured / unavailable" and
/// renders honestly (never as a fabricated zero — audit Rule 11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedScoreLine {
    /// Feed display name ("Dhan" / "Groww").
    pub name: String,
    /// Ticks captured today; `-1` = unavailable.
    pub ticks: i64,
    /// Session minutes where ONLY this feed delivered prices; `-1` unknown.
    pub exclusive_minutes: i64,
    /// Day median exchange→receipt delay in ms; `-1` = not measured yet.
    pub lag_p50_ms: i64,
    /// Day worst-1% delay in ms; `-1` = not measured yet.
    pub lag_p99_ms: i64,
    /// In-market disconnect episodes; `-1` = record unavailable.
    pub drops_market: i64,
    /// Blame tallies over the day's episodes.
    pub blame_broker: i64,
    pub blame_ours: i64,
    pub blame_unclear: i64,
    /// Stall episodes — measured from the day's stall-restart records
    /// (scoreboard PR-B, 2026-07-10): 0 means a MEASURED zero from this
    /// deploy forward; `-1` = record unavailable. Groww-only by
    /// construction today (Dhan's silent-socket detection reconnects
    /// in-process and lands in the drops count instead).
    pub stalls: i64,
    /// Boot-reconciled process restarts detected today.
    pub restarts: i64,
    /// Session minutes with at least one tick; `-1` unknown.
    pub streaming_minutes: i64,
}

/// One official-minute-candle pull digest line on the daily scorecard
/// (Groww REST plan PR-5, operator Quote 2 2026-07-13: *"always clearly
/// note within a second — or within how many seconds precisely — we are
/// fetching this live real OHLCV, along with the option chain API"*).
///
/// `-1` on any field means "not measured / not recorded" and renders
/// honestly (never a fabricated zero — audit Rule 11). Every number is
/// MEASURED from the day's per-pull records — the line never asserts a
/// freshness it did not observe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestLegScoreLine {
    /// Feed display name ("Dhan" / "Groww").
    pub feed: String,
    /// Pull-family display name ("spot candles" / "option chain" /
    /// "option contracts") — already plain English, never a wire slug.
    pub leg: String,
    /// Pulls that retrieved their target minute; `-1` = counts not
    /// recorded for this feed/family yet.
    pub ok_fetches: i64,
    /// Pulls that ended without the target minute; `-1` = not recorded.
    pub failed_fetches: i64,
    /// Minutes that NEVER got repaired (post-close sweep included);
    /// `-1` = not recorded.
    pub named_gaps: i64,
    /// How many rate-limit rejections the day's pulls hit; `-1` unknown.
    pub rate_limited_hits: i64,
    /// Pulls repaired LATE (retrieved ≥60s after the minute closed — a
    /// later pull's catch-up, not the prompt fetch); `-1` unknown.
    pub late_recovered: i64,
    /// Prompt-pull minute-close → data delay, median ms; `-1` = not
    /// measured.
    pub close_p50_ms: i64,
    /// Prompt-pull worst-1% delay ms; `-1` = not measured.
    pub close_p99_ms: i64,
    /// Prompt-pull slowest delay ms; `-1` = not measured.
    pub close_max_ms: i64,
    /// How many prompt pulls the delay distribution is built from;
    /// `-1` = no delay source at all, `0` = measured zero (every pull
    /// failed).
    pub close_samples: i64,
}

/// The Dhan exchange-clock quantization floor in milliseconds — Dhan LTT
/// carries WHOLE IST seconds, so its measured lag has a uniform [0,1)s
/// inflation Groww's millisecond clock does not. The verdict's delay rung
/// declares a winner ONLY when the cross-feed p99 delta EXCEEDS this floor
/// (PR-C review round 1, 2026-07-11): a sub-floor "Groww faster by <1s" is
/// unprovable against Dhan's whole-second clock — the runbook caveat made
/// mechanical. MUST stay lockstep with the persisted `lag_floor_ms` column
/// value for Dhan (`tickvault_storage::feed_scoreboard_persistence::
/// LAG_FLOOR_MS_DHAN = 1000`; core cannot depend on storage — both values
/// are pinned to 1000 by their own unit tests).
const VERDICT_LAG_CLOCK_FLOOR_MS: i64 = 1000;

/// The scorecard's ONE decision (Telegram commandment 8): who won today.
/// Tiebreak ladder — feed-off no-contest (round 4: a switched-off feed's
/// measured zeros are a one-horse race, never a win for the other) →
/// exclusive minutes → worst-1% delay beyond the clock floor (only when
/// BOTH are measured AND the delta exceeds [`VERDICT_LAG_CLOCK_FLOOR_MS`];
/// a −1 sentinel never decides, and a sub-floor delta is clock asymmetry,
/// not speed) → broker-blamed incidents → even day.
fn scorecard_verdict(
    dhan: &FeedScoreLine,
    groww: &FeedScoreLine,
    dhan_feed_off: bool,
    groww_feed_off: bool,
) -> String {
    // Rung 0 (round 4, 2026-07-10): a feed switched OFF for the day makes
    // every comparison rung a one-horse race — no winner is declared.
    if dhan_feed_off && groww_feed_off {
        return "\u{1f91d} Verdict: not comparable — both feeds were switched \
                off today, no contest."
            .to_string();
    }
    if dhan_feed_off || groww_feed_off {
        let off = if dhan_feed_off {
            &dhan.name
        } else {
            &groww.name
        };
        return format!(
            "\u{1f91d} Verdict: not comparable — {off} was switched off \
             today, no contest."
        );
    }
    // Rung 1: exclusive coverage minutes.
    if dhan.exclusive_minutes >= 0
        && groww.exclusive_minutes >= 0
        && dhan.exclusive_minutes != groww.exclusive_minutes
    {
        let (w, l) = if dhan.exclusive_minutes > groww.exclusive_minutes {
            (dhan, groww)
        } else {
            (groww, dhan)
        };
        return format!(
            "\u{1f3c6} Verdict: {} won today — {} exclusive minutes vs {}.",
            w.name, w.exclusive_minutes, l.exclusive_minutes
        );
    }
    // Rung 2: worst-1% delay (lower wins; only when both are measured AND
    // the delta exceeds the Dhan whole-second clock floor — PR-C review
    // round 1, 2026-07-11: Dhan's p99 physically cannot read below ~1s, so
    // a raw compare would crown Groww "faster" on every healthy day from
    // clock asymmetry, exactly the sub-floor comparison the runbook bans).
    // A sub-floor delta falls through to the incident rung / "Even day".
    if dhan.lag_p99_ms >= 0
        && groww.lag_p99_ms >= 0
        && dhan.lag_p99_ms.saturating_sub(groww.lag_p99_ms).abs() > VERDICT_LAG_CLOCK_FLOOR_MS
    {
        let (w, l) = if dhan.lag_p99_ms < groww.lag_p99_ms {
            (dhan, groww)
        } else {
            (groww, dhan)
        };
        return format!(
            "\u{1f3c6} Verdict: {} won today — faster prices beyond the clock floor \
             (worst 1% delay {} vs {}).",
            w.name,
            render_ms(w.lag_p99_ms),
            render_ms(l.lag_p99_ms)
        );
    }
    // Rung 3: fewer broker-caused INCIDENTS wins — since PR-B the broker
    // blame tally covers drops + stalls (restarts are always ours, so they
    // never inflate it), and the wording must match what is compared: on a
    // stall-heavy zero-drop day "fewer broker-caused drops (0 vs 5)" would
    // contradict the card's own Drops line (review round 1, charter §D
    // commandment 6 — the incident-split line was already reworded for
    // exactly this reason). The blame tallies must BOTH be measured too —
    // a -1 sentinel would otherwise "win" (-1 < N) and render as gibberish
    // (hostile review 2026-07-10); sentinel days fall through to
    // "Even day".
    if dhan.drops_market >= 0
        && groww.drops_market >= 0
        && dhan.blame_broker >= 0
        && groww.blame_broker >= 0
        && dhan.blame_broker != groww.blame_broker
    {
        let (w, l) = if dhan.blame_broker < groww.blame_broker {
            (dhan, groww)
        } else {
            (groww, dhan)
        };
        return format!(
            "\u{1f3c6} Verdict: {} won today — fewer broker-caused incidents ({} vs {}).",
            w.name, w.blame_broker, l.blame_broker
        );
    }
    "\u{1f91d} Verdict: Even day.".to_string()
}

/// Renders a millisecond delay for the scorecard: `-1` = honest "not
/// measured yet"; ≥1s renders in seconds for readability.
fn render_ms(ms: i64) -> String {
    if ms < 0 {
        return "not measured yet".to_string();
    }
    if ms >= 1000 {
        #[allow(clippy::cast_precision_loss)]
        // APPROVED: display-only division of a bounded daily lag value.
        return format!("{:.1} s", ms as f64 / 1000.0);
    }
    format!("{ms} ms")
}

/// Renders one official-minute-candle pull digest line (Groww REST plan
/// PR-5, operator Quote 2). Every number is measured; `-1` sentinels
/// render "not measured yet" / "(pull counts not recorded yet)" — never a
/// fabricated zero (audit Rule 11). Plain-English, no wire slugs.
fn rest_leg_line(l: &RestLegScoreLine) -> String {
    let label = format!("{} {}", l.feed, l.leg);
    // Nothing measured for this feed/family at all → one honest phrase.
    if l.ok_fetches < 0 && l.close_samples < 0 {
        return format!("{label}: not measured yet");
    }
    let mut parts: Vec<String> = Vec::new();
    if l.ok_fetches >= 0 {
        parts.push(format!(
            "{} pulls OK, {} failed",
            render_count(l.ok_fetches),
            render_count(l.failed_fetches)
        ));
    }
    if l.close_p50_ms >= 0 {
        let mut freshness = format!(
            "typical {} / worst 1% {} / slowest {} after close",
            render_ms(l.close_p50_ms),
            render_ms(l.close_p99_ms),
            render_ms(l.close_max_ms)
        );
        if l.ok_fetches < 0 {
            // Latency measured from the stored candles themselves while
            // this feed's per-pull records are not written yet — say so.
            freshness.push_str(" (pull counts not recorded yet)");
        }
        parts.push(freshness);
    } else if l.close_samples == 0 && l.ok_fetches == 0 {
        parts.push("freshness not measurable (no successful pull)".to_string());
    } else {
        parts.push("freshness not measured yet".to_string());
    }
    let mut line = format!("{label}: {}", parts.join(" — "));
    if l.late_recovered > 0 {
        line.push_str(&format!(
            "; {} recovered late",
            render_count(l.late_recovered)
        ));
    }
    if l.rate_limited_hits > 0 {
        line.push_str(&format!(
            "; {} rate-limit hits",
            render_count(l.rate_limited_hits)
        ));
    }
    if l.named_gaps > 0 {
        line.push_str(&format!(
            "; {} never recovered \u{26a0}\u{fe0f}",
            render_count(l.named_gaps)
        ));
    }
    line
}

/// Renders a `-1`-sentinel count: honest "?" instead of a fabricated zero.
/// Large measured counts (≥ 10,000 — the daily tick totals) get thousands
/// separators per Telegram commandment 6 ("11,034 instruments").
fn render_count(v: i64) -> String {
    if v < 0 {
        return "?".to_string();
    }
    if v >= 10_000 {
        // APPROVED: v is non-negative and bounded by daily tick volumes —
        // fits usize on every supported target.
        if let Ok(u) = usize::try_from(v) {
            return format_with_commas(u);
        }
    }
    v.to_string()
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
    ///
    /// Feed-scoped events (operator directive 2026-07-05: "dhan and groww
    /// should be uniquely seen to view easily") lead their body with a feed
    /// badge (`🔷 DHAN` / `🟢 GROWW` — see [`super::feed_badge`]). Applied
    /// HERE so BOTH dispatch paths (immediate bypass + coalescer samples)
    /// carry the badge identically; non-feed events are byte-identical to
    /// before (badge = `None`). The severity emoji stays first on the wire
    /// (commandment 10) — the severity-tag + source-badge prefix is
    /// prepended by the dispatch layer on top of this body.
    pub fn to_message(&self) -> String {
        let body = self.message_body();
        match self.feed_badge() {
            Some(badge) => format!("{badge} — {body}"),
            None => body,
        }
    }

    /// Which feed badge, if any, this event's Telegram body leads with.
    ///
    /// `Some` ONLY for events provably scoped to exactly one feed:
    /// - Dhan-scoped (static): Dhan JWT/auth/profile, main-feed +
    ///   order-update WebSocket lifecycle, Dhan instrument build, the Dhan
    ///   per-minute REST legs (spot 1m + option chain), the market-open
    ///   milestones (they count the Dhan pool), the daily candle
    ///   cross-check + 09:15 bar cross-check (Dhan REST vs our candles),
    ///   the Dhan static-IP / IP-verify gates, the dual-instance lock
    ///   (one Dhan account), and the order-path events (orders, circuit
    ///   breaker, rate limit, orphan positions — Dhan is the only broker
    ///   with orders).
    /// - Groww-scoped (static): the sidecar reject diagnostic + the Groww
    ///   per-minute REST legs (spot 1m + option chain + option contract).
    /// - Feed-generic (dynamic): the `Feed*` events resolve from their
    ///   `feed` field; an unknown feed name renders un-badged (honest —
    ///   never a wrong badge). The WS sleep/wake events ALSO resolve from
    ///   their `feed` field but fall back to the DHAN badge for any
    ///   unrecognized value — the live values are "main"/"order_update"
    ///   (both Dhan WebSocket types) and the emit sites live in the Dhan
    ///   connection code. HONEST LIMIT: if a future non-Dhan feed reuses
    ///   these variants with a feed name the resolver does not know, the
    ///   fallback would mislabel it Dhan — such a reuse must pass a
    ///   resolvable name ("groww") or extend `feed_badge_for_name` first.
    ///
    /// # Host/system convention (operator directive 2026-07-14)
    ///
    /// Genuinely feed-agnostic events (host disk, box shutdown, process
    /// death, QuestDB, boot, self-test, the dual-feed scorecard) stay
    /// UN-badged — never a wrong broker tag — and their titles name the
    /// system component in plain words ("tickvault", "QuestDB", "the
    /// server") so they read as host-level at a glance. The CloudWatch
    /// alarm phrases (Telegram webhook lambda) follow the same convention
    /// with explicit "🔷 DHAN:" / "🖥️ HOST:" phrase prefixes.
    #[must_use]
    pub fn feed_badge(&self) -> Option<&'static str> {
        use super::feed_badge::{FeedBadge, feed_badge_for_name};
        match self {
            // ── Dhan-scoped: auth / token / profile ──
            Self::AuthenticationSuccess
            | Self::AuthenticationFailed { .. }
            | Self::AuthenticationTransientFailure { .. }
            | Self::PreMarketProfileCheckFailed { .. }
            | Self::TokenRenewed
            | Self::TokenRenewalFailed { .. }
            // ── Dhan-scoped: main-feed WebSocket lifecycle ──
            | Self::WebSocketConnected { .. }
            | Self::WebSocketPoolOnline { .. }
            | Self::WebSocketPoolPartialAfterDeadline { .. }
            | Self::WebSocketPoolDeferredOffHours { .. }
            | Self::WebSocketPoolDegraded { .. }
            | Self::WebSocketPoolRecovered { .. }
            | Self::WebSocketPoolHalt { .. }
            | Self::WebSocketDisconnected { .. }
            | Self::WebSocketDisconnectedOffHours { .. }
            | Self::WebSocketReconnected { .. }
            | Self::WebSocketReconnectionExhausted { .. }
            // ── Dhan-scoped: order-update WebSocket ──
            | Self::OrderUpdateConnected
            | Self::OrderUpdateAuthenticated
            | Self::OrderUpdateDisconnected { .. }
            | Self::OrderUpdateReconnected { .. }
            // ── Dhan-scoped: instrument master build ──
            | Self::InstrumentBuildSuccess { .. }
            | Self::InstrumentBuildFailed { .. }
            // ── Dhan-scoped: per-minute REST legs (spot 1m + option
            //    chain) — operator directive 2026-07-14: the pull alerts
            //    must name the feed AND the leg ──
            | Self::Spot1mFetchDegraded { .. }
            | Self::Spot1mFetchRecovered { .. }
            | Self::Spot1mSidNotServed { .. }
            | Self::Spot1mSidServedRecovered { .. }
            | Self::ChainFetchDegraded { .. }
            | Self::ChainFetchRecovered { .. }
            | Self::Chain1mUnderlyingNotServed { .. }
            | Self::Chain1mUnderlyingServedRecovered { .. }
            | Self::ChainEntitlementAbsent { .. }
            | Self::ChainEntitlementConfirmed
            | Self::ChainExpirylistFailed { .. }
            // ── Dhan-scoped: market-open milestones (count the Dhan pool
            //    + order-update WS) ──
            | Self::MarketOpenStreamingConfirmation { .. }
            | Self::MarketOpenStreamingFailed { .. }
            | Self::MarketOpenReadinessConfirmation { .. }
            // ── Dhan-scoped: daily candle cross-check (Dhan REST vs our
            //    candles) + the 09:15 bar cross-check ──
            | Self::CrossVerify1mSummary { .. }
            | Self::CrossVerify1mAborted { .. }
            | Self::BarMismatchCorrectedFromHistorical { .. }
            | Self::BarMismatchCrossCheckInconclusive { .. }
            | Self::BarMismatchCrossCheckFailed { .. }
            // ── Dhan-scoped: static-IP / IP-verify gates + the
            //    dual-instance lock (one Dhan account, one token) ──
            | Self::IpVerificationFailed { .. }
            | Self::IpVerificationSuccess { .. }
            | Self::StaticIpBootCheckPassed { .. }
            | Self::StaticIpBootCheckFailed { .. }
            | Self::StaticIpBootCheckRetrying { .. }
            | Self::DualInstanceDetected { .. }
            // ── Dhan-scoped: order path (Dhan is the only broker with
            //    orders; RiskHalt joins per cluster-C 2026-07-14 — the
            //    risk engine halts the Dhan order path) ──
            | Self::OrderRejected { .. }
            | Self::CircuitBreakerOpened { .. }
            | Self::CircuitBreakerClosed
            | Self::RateLimitExhausted { .. }
            | Self::RiskHalt { .. }
            | Self::OrphanPositionDetected { .. }
            | Self::OrphanPositionsClean => Some(FeedBadge::Dhan.badge()),
            // ── Groww-scoped ──
            Self::GrowwSidecarRejected { .. }
            // ── Groww-scoped: per-minute REST legs (spot 1m + option
            //    chain + option contract) ──
            | Self::GrowwSpot1mFetchDegraded { .. }
            | Self::GrowwSpot1mFetchRecovered { .. }
            | Self::GrowwChain1mFetchDegraded { .. }
            | Self::GrowwChain1mFetchRecovered { .. }
            | Self::GrowwChain1mExpiryUnresolved { .. }
            | Self::GrowwChain1mProbeVerdict { .. }
            | Self::GrowwContract1mFetchDegraded { .. }
            | Self::GrowwContract1mFetchRecovered { .. }
            | Self::GrowwContract1mBookUnresolved { .. } => Some(FeedBadge::Groww.badge()),
            // ── Feed-generic: badge follows the `feed` field ──
            Self::FeedAuthOk { feed }
            | Self::FeedInstrumentsLoaded { feed, .. }
            | Self::FeedConnectedAwaitingTicks { feed, .. }
            | Self::FeedDown { feed, .. }
            | Self::FeedRecovered { feed, .. } => feed_badge_for_name(feed).map(|b| b.badge()),
            // ── WS sleep/wake: badge follows the `feed` field, falling
            //    back to Dhan — the live values are "main"/"order_update"
            //    (both Dhan WebSocket types); a future feed value like
            //    "groww" would badge correctly ──
            Self::WebSocketSleepEntered { feed, .. }
            | Self::WebSocketSleepResumed { feed, .. }
            | Self::WebSocketTokenForceRenewedOnWake { feed, .. } => Some(
                feed_badge_for_name(feed)
                    .unwrap_or(FeedBadge::Dhan)
                    .badge(),
            ),
            _ => None,
        }
    }

    /// The badge-less message body — the per-variant formatting match.
    /// Callers outside this impl use [`Self::to_message`].
    fn message_body(&self) -> String {
        match self {
            Self::StartupComplete {
                mode,
                spot_1m_enabled,
                spot_1m_indices,
                chain_1m_enabled,
                chain_1m_underlyings,
            } => {
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
                // 2026-07-13 operator visibility rider: say what the
                // per-minute price capture will ACTUALLY do today (a
                // midnight boot is silent until 9:16 AM and the operator
                // asked "what happened to it?"). Truthful per-leg wording —
                // a switched-off leg says so.
                // 2026-07-14 operator broker-tag directive: the capture
                // line reports the DHAN per-minute REST legs — say so, and
                // note the Groww per-minute legs report on their own
                // alerts (they are config-gated in their own modules).
                let capture_line = match (spot_1m_enabled, chain_1m_enabled) {
                    (true, true) => format!(
                        "\u{2705} Dhan per-minute price capture — armed \
                         (fires 9:16 AM to 3:30 PM IST on trading days): \
                         Dhan spot candles for {spot_1m_indices} indices + \
                         Dhan option chain for {chain_1m_underlyings} \
                         indices (Groww per-minute legs report separately)"
                    ),
                    (true, false) => format!(
                        "\u{2705} Dhan per-minute price capture — armed \
                         (fires 9:16 AM to 3:30 PM IST on trading days): \
                         Dhan spot candles for {spot_1m_indices} indices; \
                         Dhan option chain — switched off (Groww per-minute \
                         legs report separately)"
                    ),
                    (false, true) => format!(
                        "\u{2705} Dhan per-minute price capture — armed \
                         (fires 9:16 AM to 3:30 PM IST on trading days): \
                         Dhan option chain for {chain_1m_underlyings} \
                         indices; Dhan spot candles — switched off (Groww \
                         per-minute legs report separately)"
                    ),
                    (false, false) => "Dhan per-minute price capture — switched off \
                         (Groww per-minute legs report separately)"
                        .to_string(),
                };
                format!(
                    "<b>tickvault started</b>\nMode: {mode}\nBuild: {build}\n\
                     {capture_line}\n\n\
                     Dashboards: QuestDB Console (local) / CloudWatch (prod)",
                    build = tickvault_common::build_info::build_git_sha_short(),
                )
            }
            Self::AuthenticationSuccess => "<b>Auth OK</b> — Dhan JWT acquired".to_string(),
            Self::AuthenticationFailed { reason } => {
                // 2026-07-14 Dhan noise lock reword: name the broker + the
                // consequence in plain English (family-(3) — the ONE Dhan
                // token condition that still pages).
                format!(
                    "🆘 <b>Dhan login could not be obtained</b>\n\
                     The Dhan spot-1m and option-chain pulls will stop until this is fixed.\n\
                     {}",
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
            Self::TokenRenewed => "<b>Token renewed</b>".to_string(),
            Self::TokenRenewalFailed { attempts, reason } => {
                // 2026-07-14 Dhan noise lock reword: broker + consequence.
                format!(
                    "🆘 <b>Dhan login renewal FAILED</b> (attempt {attempts})\n\
                     If this keeps failing the Dhan spot-1m and option-chain pulls will stop.\n\
                     {}",
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
                     Dhan main feed: {main_feed_active}/{main_feed_total}\n\
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
            Self::TfConsistencySummary {
                dhan_date_ist,
                groww_date_ist,
                instruments,
                buckets_compared,
                mismatches,
                missing_tf_rows,
                no_coverage,
                off_grid,
                duplicates,
                tail_unsealed,
                degraded,
                truncated,
                status_label,
                top_detail,
            } => {
                // H1: name the Groww end-of-day buckets that are never
                // sealed on the production schedule — honest coverage
                // note, never a finding.
                let tail_note = if *tail_unsealed > 0 {
                    format!(
                        "\n{tail_unsealed} Groww end-of-day buckets are not \
                         sealed by design (the system stops before the \
                         midnight seal) — not verified."
                    )
                } else {
                    String::new()
                };
                // L6: wording derives from status_label — the verifier's
                // flush-adjusted verdict — never re-derived from counts.
                if status_label == "pass" {
                    format!(
                        "\u{2705} <b>Daily timeframe check @ 3:40 PM IST — PASS</b>\n\
                         Dhan day: {dhan_date_ist} | Groww day: {groww_date_ist}\n\
                         Instruments: {instruments} | Candles compared: {buckets_compared}\n\
                         Every 2-minute-to-4-hour candle matches its 1-minute \
                         building blocks exactly.{tail_note}"
                    )
                } else if status_label == "no_data" {
                    format!(
                        "\u{1f515} <b>Daily timeframe check @ 3:40 PM IST — nothing to check</b>\n\
                         Dhan day: {dhan_date_ist} | Groww day: {groww_date_ist}\n\
                         No candles were recorded for these days (feeds were \
                         off). This is not a pass and not a failure — there \
                         was simply nothing to verify."
                    )
                } else if *buckets_compared == 0 {
                    // The BLIND day: rows may exist but NOTHING could be
                    // compared. An empty finding list must never read as a
                    // pass (audit Rule 11 — the false-OK class). The Groww
                    // tail note rides along here too (refuter round 2) —
                    // un-catch-up-able tail buckets stay unverified even on
                    // a blind day.
                    format!(
                        "\u{1f198} <b>Daily timeframe check @ 3:40 PM IST — BLIND</b>\n\
                         Dhan day: {dhan_date_ist} | Groww day: {groww_date_ist}\n\
                         Checked NOTHING today — could NOT verify a single \
                         candle. This is not a pass.{tail_note}\n\
                         What to do RIGHT NOW:\n\
                         1. Check the database is up and reachable.\n\
                         2. Confirm the live feeds recorded candles today.\n\
                         3. Re-run the check once the database is healthy."
                    )
                } else {
                    let coverage_note = if *degraded {
                        "\nCoverage was PARTIAL — some data could not be read, \
                         so today's check cannot vouch for everything."
                    } else {
                        ""
                    };
                    let truncated_note = if *truncated {
                        "\nCounts exceed the stored detail — only the first \
                         findings were recorded row-by-row; the totals are exact."
                    } else {
                        ""
                    };
                    let mut detail_block = String::new();
                    for line in top_detail.iter().take(10) {
                        detail_block.push('\n');
                        detail_block.push_str("• ");
                        detail_block.push_str(&html_escape(line));
                    }
                    format!(
                        "\u{26a0}\u{fe0f} <b>Daily timeframe check @ 3:40 PM IST — \
                         NEEDS ATTENTION</b>\n\
                         Dhan day: {dhan_date_ist} | Groww day: {groww_date_ist}\n\
                         Instruments: {instruments} | Candles compared: {buckets_compared}\n\
                         Value differences: {mismatches} | Missing candles: {missing_tf_rows}\n\
                         Candles with no 1-minute data behind them: {no_coverage}\n\
                         Off-grid timestamps: {off_grid} | Duplicates: {duplicates}\
                         {tail_note}{coverage_note}{truncated_note}{detail_block}\n\
                         What to do RIGHT NOW:\n\
                         1. Review the worst offenders above — do they cluster \
                         on one timeframe or one time of day?\n\
                         2. If the app restarted mid-session today, restart \
                         windows explain value differences.\n\
                         3. Any off-grid timestamp means the candle clock \
                         itself is wrong — escalate immediately."
                    )
                }
            }
            Self::TfConsistencyAborted { detail } => {
                let detail = html_escape(detail);
                format!(
                    "\u{26a0}\u{fe0f} <b>Daily timeframe check did NOT run</b>\n\
                     The 3:40 PM IST check that recomputes every higher-timeframe \
                     candle from its 1-minute building blocks died before finishing.\n\
                     Reason: {detail}\n\
                     What to do RIGHT NOW:\n\
                     1. Check the app is still running.\n\
                     2. Restart the app to re-arm tomorrow's check."
                )
            }
            Self::Spot1mFetchDegraded {
                consecutive_failed_minutes,
                minute_ist,
            } => {
                format!(
                    "\u{1f198} <b>Minute-by-minute spot index candle pull is FAILING</b>\n\
                     The per-minute pull of Dhan's official 1-minute candle \
                     for NIFTY, BANKNIFTY and SENSEX has failed \
                     {consecutive_failed_minutes} minutes in a row (latest \
                     failed minute: {minute_ist} IST).\n\
                     Live streaming prices are NOT affected — only the \
                     per-minute official record copy is missing.\n\
                     What to do RIGHT NOW:\n\
                     1. Check the Dhan data subscription is still active.\n\
                     2. If live streaming prices ALSO stopped, treat it as a \
                     full data outage.\n\
                     3. Missing minutes fill in safely once the pull recovers."
                )
            }
            Self::Spot1mFetchRecovered {
                minute_ist,
                failed_minutes,
            } => {
                format!(
                    "\u{2705} <b>Minute-by-minute spot index candle pull recovered</b>\n\
                     The per-minute official candle pull is working again as \
                     of {minute_ist} IST, after {failed_minutes} failed \
                     minute(s). The minutes that failed stay blank in the \
                     record until re-pulled — nothing is made up."
                )
            }
            Self::GrowwSpot1mFetchDegraded {
                consecutive_failed_minutes,
                minute_ist,
            } => {
                format!(
                    "\u{1f198} <b>Groww minute-by-minute spot index candle pull is FAILING</b>\n\
                     The per-minute pull of the official 1-minute candle for \
                     NIFTY, BANKNIFTY and SENSEX from the second broker \
                     (Groww) has failed {consecutive_failed_minutes} minutes \
                     in a row (latest failed minute: {minute_ist} IST).\n\
                     Live streaming prices are NOT affected — only Groww's \
                     per-minute official record copy is missing.\n\
                     What to do RIGHT NOW:\n\
                     1. Check the Groww account's daily access is active \
                     (the shared morning key).\n\
                     2. If Groww live streaming prices ALSO stopped, treat it \
                     as a full Groww outage.\n\
                     3. Missing minutes fill in safely once the pull recovers."
                )
            }
            Self::GrowwSpot1mFetchRecovered {
                minute_ist,
                failed_minutes,
            } => {
                format!(
                    "\u{2705} <b>Groww minute-by-minute spot index candle pull recovered</b>\n\
                     The Groww per-minute official candle pull is working \
                     again as of {minute_ist} IST, after {failed_minutes} \
                     failed minute(s). The minutes that failed stay blank in \
                     the record until re-pulled — nothing is made up."
                )
            }
            Self::Spot1mSidNotServed {
                symbol,
                consecutive_minutes,
            } => {
                format!(
                    "\u{1f198} <b>Dhan is not returning 1-minute candles for \
                     {symbol}</b>\n\
                     For {consecutive_minutes} minutes in a row the official \
                     1-minute candle for {symbol} was missing from the \
                     per-minute pull while the other indices came through \
                     fine — the other indices are unaffected, so this looks \
                     like Dhan not serving THIS index, not a general \
                     outage.\n\
                     Live streaming prices are NOT affected — only the \
                     per-minute official record copy for {symbol} is \
                     missing.\n\
                     What to do RIGHT NOW:\n\
                     1. Nothing urgent — the other indices keep recording \
                     normally.\n\
                     2. If this fires every day, ask Dhan whether \
                     1-minute candles exist for this index at all.\n\
                     3. Missing minutes fill in safely if Dhan starts \
                     serving them."
                )
            }
            Self::Spot1mSidServedRecovered {
                symbol,
                not_served_minutes,
            } => {
                format!(
                    "\u{2705} <b>Dhan is serving 1-minute candles for \
                     {symbol} again</b>\n\
                     The per-minute official candle pull for {symbol} is \
                     working again after {not_served_minutes} missed \
                     minute(s). The minutes that were missed stay blank in \
                     the record until re-pulled — nothing is made up."
                )
            }
            Self::GrowwChain1mFetchDegraded {
                consecutive_failed_minutes,
                minute_ist,
            } => {
                format!(
                    "\u{1f198} <b>Groww minute-by-minute option chain recording is FAILING</b>\n\
                     The per-minute option chain snapshot for NIFTY, BANKNIFTY \
                     and SENSEX from the second broker (Groww) has failed \
                     {consecutive_failed_minutes} minutes in a row (latest \
                     failed minute: {minute_ist} IST).\n\
                     Live streaming prices are NOT affected — only Groww's \
                     per-minute option chain record is missing.\n\
                     What to do RIGHT NOW:\n\
                     1. Check the Groww account's daily access is active \
                     (the shared morning key).\n\
                     2. If Groww live streaming prices ALSO stopped, treat it \
                     as a full Groww outage.\n\
                     3. Missing minutes stay blank — nothing is made up."
                )
            }
            Self::GrowwChain1mFetchRecovered {
                minute_ist,
                failed_minutes,
            } => {
                format!(
                    "\u{2705} <b>Groww minute-by-minute option chain recording recovered</b>\n\
                     The Groww per-minute option chain snapshot is working \
                     again as of {minute_ist} IST, after {failed_minutes} \
                     failed minute(s). The minutes that failed stay blank in \
                     the record until re-pulled — nothing is made up."
                )
            }
            Self::GrowwChain1mUnderlyingNotServed {
                underlying,
                empty_minutes,
            } => {
                format!(
                    "\u{1f198} <b>Groww is not returning the option chain \
                     for {underlying}</b>\n\
                     For {empty_minutes} minutes in a row the per-minute \
                     option chain for {underlying} came back empty from \
                     Groww while the other indices came through fine — the \
                     other indices are unaffected, so this looks like the \
                     broker not serving THIS index's chain, not a general \
                     outage.\n\
                     Live streaming prices are NOT affected — only Groww's \
                     per-minute option chain record for {underlying} is \
                     missing; the same minutes may still be available from \
                     the Dhan side.\n\
                     What to do RIGHT NOW:\n\
                     1. Nothing urgent — the other indices keep recording \
                     normally.\n\
                     2. On an expiry day this is usually the broker cutting \
                     off the expiring chain early — it comes back with the \
                     next expiry.\n\
                     3. Missing minutes stay blank — nothing is made up."
                )
            }
            Self::GrowwChain1mUnderlyingServedRecovered {
                underlying,
                empty_minutes,
            } => {
                format!(
                    "\u{2705} <b>Groww is serving the option chain for \
                     {underlying} again</b>\n\
                     The per-minute option chain for {underlying} is working \
                     again after {empty_minutes} empty minute(s). The \
                     minutes that were missed stay blank in the record until \
                     re-pulled — nothing is made up."
                )
            }
            Self::GrowwChain1mExpiryUnresolved { detail } => {
                let detail = html_escape(detail);
                format!(
                    "\u{1f198} <b>Groww option chain recording could NOT start \
                     for some indices today</b>\n\
                     Today's contract list from Groww did not give a usable \
                     option expiry date, so those indices' option chain \
                     recording stays OFF for today (expiry dates are never \
                     guessed).\n\
                     Detail: {detail}\n\
                     Live streaming prices are NOT affected. Tomorrow's start \
                     retries automatically.\n\
                     What to do RIGHT NOW:\n\
                     1. Nothing urgent — the affected recording is off for \
                     today only.\n\
                     2. If this repeats daily, the Groww contract list has a \
                     problem — check with Groww."
                )
            }
            Self::GrowwChain1mProbeVerdict { ok, detail } => {
                let detail = html_escape(detail);
                if *ok {
                    format!(
                        "\u{2705} <b>Groww option chain check PASSED</b>\n\
                         Today's one-time check pulled the Groww option chain \
                         successfully. Measured: {detail}\n\
                         Recording is currently switched OFF. To start \
                         recording it minute-by-minute: turn ON the Groww \
                         option chain setting and restart the app."
                    )
                } else {
                    format!(
                        "\u{1f514} <b>Groww option chain check did NOT pass</b>\n\
                         Today's one-time check could not pull a usable Groww \
                         option chain. Measured: {detail}\n\
                         Nothing is broken — recording is switched off and \
                         stays off. Tomorrow's start checks again."
                    )
                }
            }
            Self::GrowwContract1mFetchDegraded {
                consecutive_failed_minutes,
                minute_ist,
            } => {
                format!(
                    "\u{1f198} <b>Groww minute-by-minute option CONTRACT price \
                     recording is FAILING</b>\n\
                     The per-minute price pull for the selected NIFTY, \
                     BANKNIFTY and SENSEX option contracts from the second \
                     broker (Groww) has failed {consecutive_failed_minutes} \
                     minutes in a row (latest failed minute: {minute_ist} \
                     IST).\n\
                     Live streaming prices are NOT affected — only Groww's \
                     per-minute contract price record is missing.\n\
                     What to do RIGHT NOW:\n\
                     1. Check the Groww account's daily access is active \
                     (the shared morning key).\n\
                     2. If the Groww option chain recording ALSO failed, the \
                     problem is upstream of this leg.\n\
                     3. Missing minutes stay blank — nothing is made up."
                )
            }
            Self::GrowwContract1mFetchRecovered {
                minute_ist,
                failed_minutes,
            } => {
                format!(
                    "\u{2705} <b>Groww minute-by-minute option contract price \
                     recording recovered</b>\n\
                     The Groww per-minute contract price pull is working \
                     again as of {minute_ist} IST, after {failed_minutes} \
                     failed minute(s). The minutes that failed stay blank in \
                     the record until re-pulled — nothing is made up."
                )
            }
            Self::GrowwContract1mBookUnresolved { detail } => {
                let detail = html_escape(detail);
                format!(
                    "\u{1f198} <b>Groww option contract recording could NOT \
                     start for some indices today</b>\n\
                     Today's contract list from Groww did not give usable \
                     option contracts for the current expiry, so those \
                     indices' per-minute contract recording stays OFF for \
                     today (contract identities are never guessed).\n\
                     Detail: {detail}\n\
                     Live streaming prices are NOT affected. Tomorrow's start \
                     retries automatically.\n\
                     If this repeats for days, the contract list itself has a \
                     problem — check with Groww."
                )
            }
            Self::ChainFetchDegraded {
                consecutive_failed_minutes,
                minute_ist,
            } => {
                format!(
                    "\u{1f198} <b>Minute-by-minute option chain recording is FAILING</b>\n\
                     The per-minute option chain snapshot for NIFTY, BANKNIFTY \
                     and SENSEX has failed {consecutive_failed_minutes} minutes \
                     in a row (latest failed minute: {minute_ist} IST).\n\
                     Live streaming prices are NOT affected — only the \
                     per-minute option chain record is missing.\n\
                     What to do RIGHT NOW:\n\
                     1. Check the Dhan data subscription is still active.\n\
                     2. If live streaming prices ALSO stopped, treat it as a \
                     full data outage.\n\
                     3. Missing minutes stay blank — nothing is made up."
                )
            }
            Self::ChainFetchRecovered {
                minute_ist,
                failed_minutes,
            } => {
                format!(
                    "\u{2705} <b>Minute-by-minute option chain recording recovered</b>\n\
                     The per-minute option chain snapshot is working again as \
                     of {minute_ist} IST, after {failed_minutes} failed \
                     minute(s). The minutes that failed stay blank in the \
                     record until re-pulled — nothing is made up."
                )
            }
            Self::ChainEntitlementAbsent {
                pipeline_enabled,
                detail,
            } => {
                let detail = html_escape(detail);
                if *pipeline_enabled {
                    format!(
                        "\u{1f198} <b>Option chain recording CANNOT run — no data \
                         subscription</b>\n\
                         Dhan refused the option chain data request: this \
                         account has NO option chain data subscription right now.\n\
                         Dhan said: {detail}\n\
                         Option chain recording stays OFF for today. Live \
                         streaming prices are NOT affected.\n\
                         What to do RIGHT NOW:\n\
                         1. Buy/renew the option chain data subscription with \
                         Dhan, OR\n\
                         2. Turn the option chain recording setting off so this \
                         alert stops."
                    )
                } else {
                    format!(
                        "\u{1f514} <b>Option chain check: NOT available on this \
                         account</b>\n\
                         Today's one-time check confirmed the Dhan account has \
                         NO option chain data subscription (Dhan said: \
                         {detail}).\n\
                         Nothing is broken — option chain recording is switched \
                         off and stays off. Buy the subscription with Dhan \
                         if you want this data."
                    )
                }
            }
            Self::ChainEntitlementConfirmed => "\u{2705} <b>Option chain data IS available on \
                 this account</b>\n\
                 Today's one-time check confirmed Dhan WILL serve option \
                 chain data. Recording is currently switched OFF.\n\
                 To start recording it minute-by-minute: turn ON the option \
                 chain setting and restart the app."
                .to_string(),
            Self::ChainExpirylistFailed { detail } => {
                let detail = html_escape(detail);
                format!(
                    "\u{1f198} <b>Option chain recording could NOT start today</b>\n\
                     The day-start lookup of option expiry dates failed after \
                     several tries, so option chain recording stays OFF for \
                     today (expiry dates are never guessed).\n\
                     Dhan said: {detail}\n\
                     Live streaming prices are NOT affected. Tomorrow's start \
                     retries automatically.\n\
                     What to do RIGHT NOW:\n\
                     1. Check the Dhan data connection is healthy.\n\
                     2. If this repeats daily, contact Dhan."
                )
            }
            Self::Chain1mUnderlyingNotServed {
                underlying,
                empty_minutes,
            } => {
                format!(
                    "\u{1f198} <b>Dhan is not returning the option chain for \
                     {underlying}</b>\n\
                     For {empty_minutes} minutes in a row the per-minute \
                     option chain for {underlying} came back empty from Dhan \
                     while the other indices came through fine — the other \
                     indices are unaffected, so this looks like the broker \
                     not serving THIS index's chain, not a general outage.\n\
                     Live streaming prices are NOT affected — only Dhan's \
                     per-minute option chain record for {underlying} is \
                     missing; the same minutes may still be coming in from \
                     the second broker (\u{1f7e2} GROWW), which records into \
                     the same book with its own label.\n\
                     What to do RIGHT NOW:\n\
                     1. Nothing urgent — the other indices keep recording \
                     normally, and the Groww copy covers {underlying} for \
                     these minutes IF Groww is serving it.\n\
                     2. On an expiry day this is usually the broker cutting \
                     off the expiring chain early — it comes back with the \
                     next expiry.\n\
                     3. Missing Dhan minutes stay blank — nothing is made up \
                     and nothing is copied across brokers."
                )
            }
            Self::Chain1mUnderlyingServedRecovered {
                underlying,
                empty_minutes,
            } => {
                format!(
                    "\u{2705} <b>Dhan is serving the option chain for \
                     {underlying} again</b>\n\
                     The per-minute option chain for {underlying} from Dhan \
                     is working again after {empty_minutes} empty minute(s). \
                     The minutes that were missed stay blank in Dhan's record \
                     — nothing is made up; check the Groww copy for those \
                     minutes if they matter."
                )
            }
            Self::DualFeedDailyScorecard {
                trading_date_ist,
                dhan,
                groww,
                session_minutes,
                partial_coverage,
                degraded,
                early_run,
                restart_partial,
                dhan_feed_off,
                groww_feed_off,
                rest_legs,
                rest_legs_read_failed,
            } => {
                // Operator-charter §G wording: plain English, emoji status,
                // IST 12-hour time, specific numbers, ONE decision (the
                // verdict line), no library names, no file paths. −1
                // sentinels render honestly ("?" / "not measured yet") —
                // never fabricated zeros (audit Rule 11).
                let streaming_line = |f: &FeedScoreLine| -> String {
                    if f.streaming_minutes < 0 || *session_minutes <= 0 {
                        return "unknown".to_string();
                    }
                    let ok = f.streaming_minutes.saturating_mul(100)
                        >= session_minutes.saturating_mul(99);
                    let mark = if ok { "\u{2705}" } else { "\u{26a0}\u{fe0f}" };
                    format!(
                        "{} of {} min {}",
                        f.streaming_minutes, session_minutes, mark
                    )
                };
                // The who-caused-them split covers ALL of the day's headline
                // incidents (drops + stalls + restarts), so it lives on its
                // OWN line, decoupled from the drops count — a day with 0
                // drops and 1 restart must never read "0 (ours 1)" (hostile
                // review 2026-07-10).
                let incident_split = |f: &FeedScoreLine| -> String {
                    format!(
                        "broker {} / ours {} / unclear {}",
                        render_count(f.blame_broker),
                        render_count(f.blame_ours),
                        render_count(f.blame_unclear)
                    )
                };
                // Groww REST plan PR-5 (operator Quote 2, 2026-07-13): the
                // official minute-candle pull digest — one plain-English
                // line per feed/pull-family with the MEASURED
                // seconds-after-close numbers. An empty vec (older callers
                // / tests) omits the section entirely; a `-1` sentinel
                // line renders "not measured yet", never a fabricated
                // freshness (Rule 11).
                let rest_section = if rest_legs.is_empty() {
                    String::new()
                } else {
                    let mut s = String::from(
                        "Official minute candles — how fast after each \
                         minute closed:\n",
                    );
                    for l in rest_legs {
                        s.push_str("  ");
                        s.push_str(&rest_leg_line(l));
                        s.push('\n');
                    }
                    s
                };
                let mut footnotes = String::new();
                if *rest_legs_read_failed {
                    footnotes.push_str(
                        "\n\u{26a0}\u{fe0f} Today's minute-candle pull records could \
                         not be read while building this card — the candle-pull \
                         lines may under-count or read \u{201c}not measured \
                         yet\u{201d}.",
                    );
                }
                if *early_run {
                    footnotes.push_str(
                        "\n\u{26a0}\u{fe0f} This card was produced early on operator \
                         request — it covers the day only up to the run time; \
                         re-run after close for the full-day card.",
                    );
                }
                if *partial_coverage {
                    // Honest PR-1 cause (hostile review 2026-07-10): this
                    // flag flips on READ/WRITE failures while building the
                    // card — NOT on "the app was down part of the session"
                    // (restart detection is a later upgrade).
                    footnotes.push_str(
                        "\n\u{26a0}\u{fe0f} Some of today's records could not be read \
                         while building this card — numbers shown as \u{201c}?\u{201d} \
                         are missing, and the rest may under-count.",
                    );
                }
                if *degraded {
                    footnotes.push_str(
                        "\n\u{26a0}\u{fe0f} Some connection events could not be recorded \
                         today — treat the drop counts as a minimum, not a truth.",
                    );
                }
                if *restart_partial {
                    // Round-3 honesty fix: the persisted row is stamped
                    // partial on a restart day — the card must carry the
                    // same caveat, not stay silent.
                    footnotes.push_str(
                        "\n\u{26a0}\u{fe0f} The app restarted during the day — records \
                         from before the restart may under-count, so today's \
                         numbers are a floor, not a truth.",
                    );
                }
                for (off, line) in [(dhan_feed_off, dhan), (groww_feed_off, groww)] {
                    if *off {
                        // Round-4 fix: a switched-off feed's day is a
                        // one-horse race — say so and keep it out of the
                        // month tally (its row is stamped 'feed_off').
                        footnotes.push_str(&format!(
                            "\n\u{26a0}\u{fe0f} {} was switched off today — no contest; \
                             this day does not count toward the month verdict.",
                            line.name
                        ));
                    }
                }
                // Scoreboard PR-C (2026-07-11): delay is MEASURED — the day
                // lag histograms are live. The footnote carries the
                // resolution asymmetry honestly: Dhan's whole-second price
                // clock (≥1 s floor) vs Groww's millisecond clock read one
                // step after the wire (the sidecar writes each price down
                // the instant it arrives). An unmeasured side (backfill
                // re-run / too few samples) renders "not measured yet" with
                // the honest cause — the retired PR-1 "next upgrade" claim
                // never appears. The gate keys on EITHER feed (review
                // round 1, 2026-07-11): a Dhan-off / thin-Dhan day with a
                // measured Groww delay must never assert "Delay could not
                // be measured today" under a card showing real Groww
                // milliseconds (Rule-11 self-contradiction).
                if dhan.lag_p50_ms >= 0
                    || dhan.lag_p99_ms >= 0
                    || groww.lag_p50_ms >= 0
                    || groww.lag_p99_ms >= 0
                {
                    footnotes.push_str(
                        "\nNote: Dhan's price clock ticks in whole seconds, so its \
                         delay can never read below about 1 second; Groww's delay \
                         is millisecond-precise, measured where its helper first \
                         writes each price down (one step after the wire).",
                    );
                } else {
                    footnotes.push_str(
                        "\nDelay could not be measured today (a re-run for a past \
                         day, or too few prices) — it reads \u{201c}not measured \
                         yet\u{201d}.",
                    );
                }
                // Scoreboard PR-B (2026-07-10): the PR-1 stall + Groww-drops
                // sentinel footnotes are RETIRED — stall episodes are
                // measured from this deploy forward (0 = measured 0; the
                // runbook keeps the pre-ship-day caveat), and the Groww
                // socket-death family is now visible in the Stalls column,
                // so the drops count renders as a measurement again. A `-1`
                // still renders as "?" defensively, without a stale claim.
                format!(
                    "\u{1f4ca} <b>Daily feed scorecard @ 3:45 PM IST</b>\n\
                     Date: {trading_date_ist}\n\
                     Ticks today: Dhan {} | Groww {}\n\
                     Minutes only one feed had prices: Dhan {} | Groww {}\n\
                     Typical delay: Dhan {} | Groww {}\n\
                     Worst 1% delay: Dhan {} | Groww {}\n\
                     Drops in market hours: Dhan {} | Groww {}\n\
                     Who caused today's incidents: Dhan {} | Groww {}\n\
                     Stalls: Dhan {} | Groww {}\n\
                     App restarts detected: Dhan {} | Groww {}\n\
                     Streaming: Dhan {} | Groww {}\n\
                     {}{}{}",
                    render_count(dhan.ticks),
                    render_count(groww.ticks),
                    render_count(dhan.exclusive_minutes),
                    render_count(groww.exclusive_minutes),
                    render_ms(dhan.lag_p50_ms),
                    render_ms(groww.lag_p50_ms),
                    render_ms(dhan.lag_p99_ms),
                    render_ms(groww.lag_p99_ms),
                    render_count(dhan.drops_market),
                    render_count(groww.drops_market),
                    incident_split(dhan),
                    incident_split(groww),
                    render_count(dhan.stalls),
                    render_count(groww.stalls),
                    render_count(dhan.restarts),
                    render_count(groww.restarts),
                    streaming_line(dhan),
                    streaming_line(groww),
                    rest_section,
                    scorecard_verdict(dhan, groww, *dhan_feed_off, *groww_feed_off),
                    footnotes
                )
            }
            Self::DualFeedScorecardAborted { detail } => {
                let detail = html_escape(detail);
                format!(
                    "\u{26a0}\u{fe0f} <b>Daily feed scorecard did NOT run</b>\n\
                     The 3:45 PM IST Dhan-vs-Groww scorecard died before \
                     finishing.\n\
                     Reason: {detail}\n\
                     What to do RIGHT NOW:\n\
                     1. Check the app is still running.\n\
                     2. Restart the app to re-arm tomorrow's scorecard."
                )
            }
            Self::BrutexCrossverifySummary {
                trading_date_ist,
                outcome,
                files_read,
                symbols_compared,
                matched,
                diverged,
                missing_brutex,
                missing_live,
                tail_unsealed,
                unmapped,
                noise_p95_paise,
                noise_max_paise,
                top_offenders,
                hint,
            } => {
                // Verdict word from the FIXED outcome slug — a future
                // unknown slug degrades to "UNKNOWN", never a panic.
                let verdict: &str = match outcome.as_str() {
                    "clean" => "CLEAN",
                    "diverged" => "DIVERGED",
                    "partial" => "PARTIAL DATA",
                    "no_data" => "NO DATA",
                    "blind" => "BLIND",
                    "degraded" => "DEGRADED",
                    _ => "UNKNOWN",
                };
                // LOUD first line for the days where the comparison could
                // not vouch for anything (audit Rule 11 — never a quiet
                // false-OK on an empty/partial compare set).
                let loud: &str = match outcome.as_str() {
                    "no_data" => {
                        "\u{26a0}\u{fe0f} No files arrived from BruteX \
                         today — nothing was compared.\n"
                    }
                    "blind" => {
                        "\u{26a0}\u{fe0f} Nothing arrived on our own side \
                         today — the check had nothing to compare against.\n"
                    }
                    "partial" => {
                        "\u{26a0}\u{fe0f} PARTIAL DATA — only part of the \
                         day could be compared; treat today's counts as a \
                         floor.\n"
                    }
                    _ => "",
                };
                let offenders = if top_offenders.is_empty() {
                    String::new()
                } else {
                    format!(
                        "Biggest differences today:\n{}\n",
                        html_escape(top_offenders)
                    )
                };
                let hint_line = if hint.is_empty() {
                    String::new()
                } else {
                    format!("{}\n", html_escape(hint))
                };
                // Commandment 10: severity emoji at the START of the
                // subject, from the canonical set — ✅ only on a
                // fully-measured clean day; every other outcome
                // (diverged/partial/blind/no_data/degraded/unknown)
                // leads with ⚠️.
                let lead: &str = if outcome.as_str() == "clean" {
                    "\u{2705}"
                } else {
                    "\u{26a0}\u{fe0f}"
                };
                format!(
                    "{lead} <b>BruteX vs live 1-minute check — \
                     {trading_date_ist}: {verdict}</b>\n\
                     {loud}\
                     Files read from BruteX: {} | Symbols compared: {}\n\
                     Minutes matched: {} | Diverged: {}\n\
                     Missing on BruteX side: {} | Missing on our side: {}\n\
                     Last minutes still sealing (not counted): {} | \
                     Symbols we could not pair: {}\n\
                     Typical wiggle: within {} paise on 95% of compared \
                     minutes (max {}).\n\
                     {offenders}\
                     {hint_line}\
                     Neither side is the ground truth — small wiggle is \
                     expected; only beyond-tolerance rows are real \
                     divergence.",
                    render_count(*files_read),
                    render_count(*symbols_compared),
                    render_count(*matched),
                    render_count(*diverged),
                    render_count(*missing_brutex),
                    render_count(*missing_live),
                    render_count(*tail_unsealed),
                    render_count(*unmapped),
                    render_count(*noise_p95_paise),
                    render_count(*noise_max_paise),
                )
            }
            Self::BrutexCrossverifyAborted {
                trading_date_ist,
                reason,
            } => {
                let reason = html_escape(reason);
                format!(
                    "\u{1f198} <b>BruteX cross-verify run FAILED — \
                     {trading_date_ist}</b>\n\
                     The 3:50 PM IST BruteX-vs-live 1-minute check died \
                     before finishing.\n\
                     Reason: {reason}\n\
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
            Self::FeedDown {
                feed,
                reason,
                market_open,
                operator_initiated,
            } => {
                // `reason` is a fixed plain-English literal at every emit
                // site (never raw child text / URLs), but html_escape it
                // anyway for defense-in-depth, consistent with every String
                // arm. The trailer BRANCHES on `operator_initiated`
                // (2026-07-06 fix): a deliberately switched-off feed is
                // NEVER retried, so the auto-retry reassurance would be a
                // false signal that delays the one action (re-enable) that
                // fixes it — the operator-disable form names that action
                // instead (Telegram commandment 7). The involuntary form
                // keeps the hostile-review-passed GrowwSidecarRejected
                // honesty wording: state what will NOT flow + that retry is
                // automatic.
                let feed_name = html_escape(feed);
                let reason = html_escape(reason);
                match (*market_open, *operator_initiated) {
                    (true, true) => format!(
                        "🆘 <b>{feed_name} feed is DOWN</b>\n{reason}\n\n\
                         Prices from {feed_name} will not flow while it is switched off. \
                         It stays OFF until you re-enable it from the feeds page."
                    ),
                    (true, false) => format!(
                        "🆘 <b>{feed_name} feed is DOWN</b>\n{reason}\n\n\
                         Prices from {feed_name} will not flow until it recovers. \
                         The system keeps retrying automatically — no restart needed."
                    ),
                    (false, true) => format!(
                        "⚠️ <b>{feed_name} feed is down [off-hours]</b>\n{reason}\n\
                         It stays off until you re-enable it from the feeds page — \
                         idle is normal until then."
                    ),
                    (false, false) => format!(
                        "⚠️ <b>{feed_name} feed is down [off-hours]</b>\n{reason}\n\
                         Reconnect is automatic — idle is normal until market open."
                    ),
                }
            }
            Self::FeedRecovered { feed, down_secs } => {
                // Honest recovery: fired ONLY on the streaming rising edge
                // (real ticks), so "prices are flowing" is never a false-OK
                // claim (connected ≠ streaming split preserved).
                format!(
                    "✅ <b>{feed_name} feed streaming again</b> — down {down_secs}s. \
                     Prices are flowing.",
                    feed_name = html_escape(feed),
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
                // Cluster-C (2026-07-14): self-heal line appended so the
                // operator knows no action is needed if a CLOSED follows.
                format!(
                    "<b>Circuit breaker OPENED</b>\nConsecutive failures: {consecutive_failures}\nOrder API calls halted\nThe system retries by itself in about a minute — if a CLOSED message follows, no action needed."
                )
            }
            Self::CircuitBreakerClosed => {
                "<b>Circuit breaker CLOSED</b>\nOrder API calls resumed".to_string()
            }
            Self::RateLimitExhausted { limit_type } => {
                format!("<b>Rate limit EXHAUSTED</b>\nLimit: {limit_type}")
            }
            Self::RiskHalt { reason } => {
                // Cluster-C (2026-07-14): APPEND-style action lines
                // (Telegram commandment 7 — Critical needs "what to do
                // RIGHT NOW"); the leading "RISK HALT" literal is kept so
                // the 4 pinning contains()-tests survive.
                format!(
                    "<b>RISK HALT</b>\nTrading stopped: {}\nAll new orders are blocked.\nWhat you need to do RIGHT NOW:\n1. Open the broker app and check open positions.\n2. Decide: exit positions now, or accept no trading for the rest of the day.\n3. Trading stays blocked until the daily reset or a restart.",
                    html_escape(reason)
                )
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
            Self::GrowwSidecarRejected {
                reason,
                fleet_summary,
                detail,
            } => {
                // Defensive render-boundary cap (chars) on `detail` —
                // mirrors the supervisor's SIDECAR_LINE_SIGNATURE_MAX_CHARS
                // (core sits BELOW the app crate, so the value is mirrored,
                // not imported). The emit-site contract already caps; this
                // is defense-in-depth so a future emit site can never flood
                // a Telegram headline.
                const GROWW_REJECT_DETAIL_MAX_CHARS: usize = 160;
                // `reason` is a fixed per-class &'static str (or the fleet
                // coalescer's counted summary) mapped to String by the
                // supervisor (never raw child text), but html_escape it
                // anyway for defense-in-depth, consistent with every String arm.
                let reason = html_escape(reason);
                // 2026-07-14 operator demand (the 14:59 IST page carried no
                // WHY): when the SANITIZED reject-cause signature is present
                // the headline names it — "rejected: <cause> — retrying".
                // Truncate BEFORE html_escape so an escape entity is never
                // cut mid-sequence; empty/whitespace degrades to the exact
                // pre-2026-07-14 generic headline (never a hollow
                // "rejected:  — retrying"). Dated commandment-2 override:
                // feed-stall-watchdog-error-codes.md §1c.1.
                let title = match detail.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                    Some(d) => {
                        let capped: String =
                            d.chars().take(GROWW_REJECT_DETAIL_MAX_CHARS).collect();
                        let capped = html_escape(&capped);
                        format!("🆘 <b>Groww live feed rejected: {capped} — retrying</b>")
                    }
                    None => "🆘 <b>Groww live feed rejected</b>".to_string(),
                };
                if *fleet_summary {
                    // Fleet-coalesced partial summary (hostile-review fix
                    // 2026-07-06): the trailer must NOT claim the whole feed
                    // is "receiving nothing" — only the counted connections
                    // reported a problem, and nothing positive is claimed
                    // about the rest.
                    format!(
                        "{title}\n{reason}\n\nThe affected \
                         connections keep retrying automatically. Prices from those \
                         connections will not flow until they recover."
                    )
                } else {
                    format!(
                        "{title}\n{reason}\n\nThe Groww feed is \
                         connected but receiving nothing. Until this is fixed, Groww \
                         prices will not flow."
                    )
                }
            }
            Self::HolidayCalendarCoverageLow {
                days_remaining,
                coverage_end_display,
            } => {
                // Plain English per the 10 Telegram commandments: status
                // emoji first, specific numbers, action verbs, one decision,
                // no file paths / library names. `coverage_end_display` is
                // produced by our own date formatter (never external text)
                // but html_escape it anyway, consistent with every String arm.
                let end = html_escape(coverage_end_display);
                let when = match days_remaining {
                    Some(d) if *d >= 0 => {
                        format!("runs out in {d} days (last covered day: {end})")
                    }
                    Some(d) => {
                        let ago = d.unsigned_abs();
                        format!(
                            "already ran out {ago} days ago (last covered day: {end}) — market \
                             holidays are currently being treated as trading days"
                        )
                    }
                    // Pathological empty-calendar case (config guards prevent
                    // it in prod) — render honestly, never a fake number.
                    None => "is EMPTY — no market holidays are configured at all, so every \
                             weekday is being treated as a trading day"
                        .to_string(),
                };
                format!(
                    "⚠️ <b>Market holiday calendar needs updating</b>\n\
                     The list of NSE market holidays {when}.\n\
                     Without it, the system cannot tell holidays from trading days — it \
                     will start up and run full sessions on closed-market days.\n\
                     What you need to do:\n\
                     1. Get the official NSE trading-holiday circular for the next year.\n\
                     2. Add those dates to the trading calendar settings.\n\
                     3. Restart the app — this reminder stops automatically."
                )
            }
            Self::Custom { message } => message.clone(),
            Self::CustomStatus { message } => message.clone(),
            Self::CustomStatusUrgent { message } => message.clone(),
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
            Self::TfConsistencySummary { .. } => "TfConsistencySummary",
            Self::TfConsistencyAborted { .. } => "TfConsistencyAborted",
            Self::Spot1mFetchDegraded { .. } => "Spot1mFetchDegraded",
            Self::Spot1mFetchRecovered { .. } => "Spot1mFetchRecovered",
            Self::GrowwSpot1mFetchDegraded { .. } => "GrowwSpot1mFetchDegraded",
            Self::GrowwSpot1mFetchRecovered { .. } => "GrowwSpot1mFetchRecovered",
            Self::Spot1mSidNotServed { .. } => "Spot1mSidNotServed",
            Self::Spot1mSidServedRecovered { .. } => "Spot1mSidServedRecovered",
            Self::GrowwChain1mFetchDegraded { .. } => "GrowwChain1mFetchDegraded",
            Self::GrowwChain1mFetchRecovered { .. } => "GrowwChain1mFetchRecovered",
            Self::GrowwChain1mUnderlyingNotServed { .. } => "GrowwChain1mUnderlyingNotServed",
            Self::GrowwChain1mUnderlyingServedRecovered { .. } => {
                "GrowwChain1mUnderlyingServedRecovered"
            }
            Self::GrowwChain1mExpiryUnresolved { .. } => "GrowwChain1mExpiryUnresolved",
            Self::GrowwChain1mProbeVerdict { .. } => "GrowwChain1mProbeVerdict",
            Self::GrowwContract1mFetchDegraded { .. } => "GrowwContract1mFetchDegraded",
            Self::GrowwContract1mFetchRecovered { .. } => "GrowwContract1mFetchRecovered",
            Self::GrowwContract1mBookUnresolved { .. } => "GrowwContract1mBookUnresolved",
            Self::ChainFetchDegraded { .. } => "ChainFetchDegraded",
            Self::ChainFetchRecovered { .. } => "ChainFetchRecovered",
            Self::Chain1mUnderlyingNotServed { .. } => "Chain1mUnderlyingNotServed",
            Self::Chain1mUnderlyingServedRecovered { .. } => "Chain1mUnderlyingServedRecovered",
            Self::ChainEntitlementAbsent { .. } => "ChainEntitlementAbsent",
            Self::ChainEntitlementConfirmed => "ChainEntitlementConfirmed",
            Self::ChainExpirylistFailed { .. } => "ChainExpirylistFailed",
            Self::DualFeedDailyScorecard { .. } => "DualFeedDailyScorecard",
            Self::DualFeedScorecardAborted { .. } => "DualFeedScorecardAborted",
            Self::BrutexCrossverifySummary { .. } => "BrutexCrossverifySummary",
            Self::BrutexCrossverifyAborted { .. } => "BrutexCrossverifyAborted",
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
            Self::ShutdownInitiated => "ShutdownInitiated",
            Self::ShutdownComplete => "ShutdownComplete",
            Self::InstrumentBuildSuccess { .. } => "InstrumentBuildSuccess",
            Self::FeedInstrumentsLoaded { .. } => "FeedInstrumentsLoaded",
            Self::FeedAuthOk { .. } => "FeedAuthOk",
            Self::FeedConnectedAwaitingTicks { .. } => "FeedConnectedAwaitingTicks",
            Self::FeedDown { .. } => "FeedDown",
            Self::FeedRecovered { .. } => "FeedRecovered",
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
            Self::HolidayCalendarCoverageLow { .. } => "HolidayCalendarCoverageLow",
            Self::Custom { .. } => "Custom",
            Self::CustomStatus { .. } => "CustomStatus",
            Self::CustomStatusUrgent { .. } => "CustomStatusUrgent",
        }
    }

    /// Telegram UX Overhaul (2026-07-07): which episode bubble, if any,
    /// this event folds into.
    ///
    /// `Some` ONLY for the WS lifecycle families that produce the observed
    /// 40-message disconnect storms, the boot-milestone set, and (2026-07-14
    /// operator noise directive) the Groww runtime incident family
    /// (`GrowwSidecarRejected` + the Groww `FeedDown`/`FeedRecovered` arms);
    /// EVERY other variant returns `None` so the legacy dispatch path stays
    /// byte-identical. Zero-alloc (`Copy` match) — DHAT-pinned on the
    /// dispatch bypass arm by
    /// `crates/core/tests/dhat_telegram_dispatcher.rs`.
    #[must_use]
    pub fn episode_key(&self) -> Option<super::episode::EpisodeKey> {
        use super::episode::{EpisodeFamily, EpisodeKey};
        match self {
            Self::WebSocketDisconnected {
                connection_index, ..
            }
            | Self::WebSocketDisconnectedOffHours {
                connection_index, ..
            }
            | Self::WebSocketReconnected {
                connection_index, ..
            } => Some(EpisodeKey {
                family: EpisodeFamily::MainFeedWs,
                conn: u8::try_from(*connection_index).unwrap_or(u8::MAX),
            }),
            Self::OrderUpdateDisconnected { .. } | Self::OrderUpdateReconnected { .. } => {
                Some(EpisodeKey {
                    family: EpisodeFamily::OrderUpdateWs,
                    conn: 0,
                })
            }
            // Boot bubble (2026-07-09 operator escalation — the ~8-message
            // boot spray folds into ONE live-edited checklist). Success-only
            // milestones (all ≤ Severity::Medium pre-demotion, Low/Info
            // now); failure events keep their legacy instant paging.
            Self::AuthenticationSuccess
            | Self::InstrumentBuildSuccess { .. }
            | Self::WebSocketPoolOnline { .. }
            | Self::WebSocketPoolDeferredOffHours { .. }
            | Self::OrderUpdateConnected
            | Self::OrderUpdateAuthenticated
            | Self::BootHealthCheck { .. }
            | Self::StartupComplete { .. } => Some(super::episode::BOOT_EPISODE_KEY),
            // Feed-generic boot pings fold ONLY for the Groww feed (str
            // compare — zero-alloc; the DHAT pin holds). Any other feed
            // name keeps the legacy immediate lane.
            Self::FeedAuthOk { feed }
            | Self::FeedInstrumentsLoaded { feed, .. }
            | Self::FeedConnectedAwaitingTicks { feed, .. } => {
                if feed.eq_ignore_ascii_case("groww") {
                    Some(super::episode::BOOT_EPISODE_KEY)
                } else {
                    None
                }
            }
            // Groww runtime incident family (2026-07-14 operator noise
            // directive): the persistent reject storm folds into ONE
            // live-edited bubble — first page still pages (+ SMS at ≥High);
            // recurrences become in-place edits. Distinct from the boot
            // pings above: FeedDown/FeedRecovered are RUNTIME incidents,
            // never boot milestones. Zero-alloc str compare — the DHAT
            // bypass-arm pin holds.
            Self::GrowwSidecarRejected { .. } => Some(EpisodeKey {
                family: EpisodeFamily::GrowwFeed,
                conn: 0,
            }),
            Self::FeedDown {
                feed,
                operator_initiated,
                ..
            } => {
                // FIX-A (hostile review 2026-07-14): a DELIBERATE feeds-page
                // disable is NEVER episode-routed — the bubble's "retrying
                // automatically" edit would falsely claim a disabled feed
                // retries (the 2026-07-06 FeedDown honesty split); the
                // legacy lane carries the honest "stays OFF until
                // re-enabled" body.
                // FIX-D: only a PAGING (≥ High, i.e. in-market) FeedDown
                // opens/folds the incident bubble; the off-hours Low flavor
                // keeps its pre-existing legacy 60s-coalescer path.
                if !*operator_initiated
                    && self.severity() >= Severity::High
                    && feed.eq_ignore_ascii_case("groww")
                {
                    Some(EpisodeKey {
                        family: EpisodeFamily::GrowwFeed,
                        conn: 0,
                    })
                } else {
                    // Non-Groww feeds also keep the legacy immediate lane.
                    None
                }
            }
            Self::FeedRecovered { feed, .. } => {
                // Recovery stays episode-routed (Resolve). With no open
                // episode (e.g. the Down was Low/off-hours and never
                // opened a bubble) the FSM returns SendLegacy — the
                // legacy_passthrough arm delivers it, never a drop.
                if feed.eq_ignore_ascii_case("groww") {
                    Some(EpisodeKey {
                        family: EpisodeFamily::GrowwFeed,
                        conn: 0,
                    })
                } else {
                    // A future feed #3 keeps the legacy immediate lane.
                    None
                }
            }
            _ => None,
        }
    }

    /// Boot bubble (2026-07-09): the typed milestone this event folds into
    /// the boot checklist. `Some` for EXACTLY the variants whose
    /// [`Self::episode_key`] maps to the Boot family; `None` otherwise.
    #[must_use]
    pub fn boot_milestone(&self) -> Option<super::episode::BootMilestone> {
        use super::episode::BootMilestone;
        // Payload counts clamp defensively — never a panic (repo law).
        fn clamp_u32(value: usize) -> u32 {
            u32::try_from(value).unwrap_or(u32::MAX)
        }
        match self {
            Self::BootHealthCheck {
                services_healthy,
                services_total,
            } => Some(BootMilestone::Services {
                healthy: clamp_u32(*services_healthy),
                total: clamp_u32(*services_total),
            }),
            Self::AuthenticationSuccess => Some(BootMilestone::DhanAuth),
            Self::InstrumentBuildSuccess {
                derivative_count,
                underlying_count,
                ..
            } => Some(BootMilestone::Instruments {
                count: clamp_u32(derivative_count.saturating_add(*underlying_count)),
            }),
            Self::WebSocketPoolOnline {
                connected,
                total,
                last_real_tick_age_secs,
                ..
            } => Some(BootMilestone::DhanFeedOnline {
                connected: clamp_u32(*connected),
                total: clamp_u32(*total),
                last_real_tick_age_secs: *last_real_tick_age_secs,
            }),
            Self::WebSocketPoolDeferredOffHours { .. } => {
                Some(BootMilestone::DhanFeedDeferredOffHours)
            }
            Self::OrderUpdateConnected => Some(BootMilestone::OrderUpdateConnected),
            Self::OrderUpdateAuthenticated => Some(BootMilestone::OrderUpdateAuthenticated),
            Self::StartupComplete { mode, .. } => Some(BootMilestone::Complete { mode }),
            Self::FeedAuthOk { feed } if feed.eq_ignore_ascii_case("groww") => {
                Some(BootMilestone::GrowwAuth)
            }
            Self::FeedInstrumentsLoaded {
                feed, subscribed, ..
            } if feed.eq_ignore_ascii_case("groww") => Some(BootMilestone::GrowwInstruments {
                subscribed: clamp_u32(*subscribed),
            }),
            Self::FeedConnectedAwaitingTicks {
                feed, market_open, ..
            } if feed.eq_ignore_ascii_case("groww") => Some(BootMilestone::GrowwConnected {
                market_open: *market_open,
            }),
            _ => None,
        }
    }

    /// Telegram UX Overhaul (2026-07-07): the role this event plays inside
    /// its episode. Disconnect-class → `Open` (the FSM decides open-vs-fold
    /// by state presence); reconnect-class → `Resolve`. Meaningful only for
    /// variants where [`Self::episode_key`] is `Some`.
    #[must_use]
    pub fn episode_role(&self) -> super::episode::EpisodeRole {
        use super::episode::EpisodeRole;
        match self {
            // FeedRecovered is the Groww episode's recovery edge (2026-07-14
            // noise fold); role is consulted only when episode_key() is Some.
            Self::WebSocketReconnected { .. }
            | Self::OrderUpdateReconnected { .. }
            | Self::FeedRecovered { .. } => EpisodeRole::Resolve,
            _ => EpisodeRole::Open,
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
            Self::TokenRenewalFailed { .. } => Severity::Critical,
            Self::InstrumentBuildFailed { .. } => Severity::High,
            Self::WebSocketDisconnected { .. } => Severity::High,
            Self::WebSocketDisconnectedOffHours { .. } => Severity::Low,
            Self::Custom { .. } => Severity::High,
            // Informational status ping — Low so it batches (digest / 60s
            // coalesce) and never fires SNS SMS (gate is `>= High`).
            Self::CustomStatus { .. } => Severity::Low,
            // Instant status ping — Low (never SMS); `dispatch_policy()` forces
            // Immediate so it ships at once instead of batching.
            Self::CustomStatusUrgent { .. } => Severity::Low,
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
            // Daily timeframe-consistency verifier (2026-07-13): severity
            // derives from status_label itself — the verifier's
            // flush-adjusted verdict (L6 fix: a count-derived re-check can
            // contradict the label on edge days, e.g. blind-without-
            // degrade). Info ONLY for a real `pass` or a feed-off
            // `no_data` day (which must never page High every day of a
            // disabled-feed month); everything else — mismatch, degraded,
            // blind — is High (audit Rule 11: an empty compare set never
            // reads as pass).
            Self::TfConsistencySummary { status_label, .. } => {
                if status_label == "pass" || status_label == "no_data" {
                    Severity::Info
                } else {
                    Severity::High
                }
            }
            Self::TfConsistencyAborted { .. } => Severity::High,
            // Per-minute spot 1m REST pipeline (2026-07-12): the degraded
            // page is the edge-triggered escalation (3 consecutive fully-
            // failed minutes); the recovery is a positive Info ping.
            Self::Spot1mFetchDegraded { .. } => Severity::High,
            Self::Spot1mFetchRecovered { .. } => Severity::Info,
            // Groww per-minute spot 1m REST leg (2026-07-13): same edge
            // semantics as the Dhan leg — one High page per episode, one
            // Info ping on the falling edge.
            Self::GrowwSpot1mFetchDegraded { .. } => Severity::High,
            Self::GrowwSpot1mFetchRecovered { .. } => Severity::Info,
            Self::Spot1mSidNotServed { .. } => Severity::High,
            Self::Spot1mSidServedRecovered { .. } => Severity::Info,
            Self::GrowwChain1mFetchDegraded { .. } => Severity::High,
            Self::GrowwChain1mFetchRecovered { .. } => Severity::Info,
            Self::GrowwChain1mUnderlyingNotServed { .. } => Severity::High,
            Self::GrowwChain1mUnderlyingServedRecovered { .. } => Severity::Info,
            // One page per day when an underlying's chain recording could
            // not start (never a guessed expiry) — actionable, not fatal.
            Self::GrowwChain1mExpiryUnresolved { .. } => Severity::High,
            // The probe is informational either way — nothing was expected
            // to record while the pipeline is switched off.
            Self::GrowwChain1mProbeVerdict { .. } => Severity::Info,
            // The contract leg mirrors the chain edge semantics: one HIGH
            // page per failing episode, one Info recovery, one HIGH per day
            // for an unresolvable contract book.
            Self::GrowwContract1mFetchDegraded { .. } => Severity::High,
            Self::GrowwContract1mFetchRecovered { .. } => Severity::Info,
            Self::GrowwContract1mBookUnresolved { .. } => Severity::High,
            Self::ChainFetchDegraded { .. } => Severity::High,
            Self::ChainFetchRecovered { .. } => Severity::Info,
            // 2026-07-14 family-(2) extension (noise-lock §2.1): one HIGH
            // page per underlying per not-served episode; Info recovery.
            Self::Chain1mUnderlyingNotServed { .. } => Severity::High,
            Self::Chain1mUnderlyingServedRecovered { .. } => Severity::Info,
            // HIGH only when the pipeline was ON and expected to record;
            // the probe-only verdict for a disabled pipeline is an Info
            // heads-up, never a page (the operator asked for a report).
            Self::ChainEntitlementAbsent {
                pipeline_enabled, ..
            } => {
                if *pipeline_enabled {
                    Severity::High
                } else {
                    Severity::Info
                }
            }
            Self::ChainEntitlementConfirmed => Severity::Info,
            Self::ChainExpirylistFailed { .. } => Severity::High,
            // Dual-feed scorecard (2026-07-10): Info per the contract — the
            // daily digest is a positive signal; degradation is carried
            // LOUDLY in the body (partial/degraded footnotes) and a task
            // death fires the High Aborted variant below instead.
            Self::DualFeedDailyScorecard { .. } => Severity::Info,
            Self::DualFeedScorecardAborted { .. } => Severity::High,
            // BruteX cross-verify (2026-07-12): Info per the contract — the
            // daily digest is a positive signal; the LOUD body lines carry
            // degraded/no-data days, and a task death fires the High
            // Aborted variant below instead.
            Self::BrutexCrossverifySummary { .. } => Severity::Info,
            Self::BrutexCrossverifyAborted { .. } => Severity::High,
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
            // 2026-07-09 (operator escalation — one boot bubble): demoted
            // Medium → Low. The event now folds into the green boot
            // checklist whose prefix is fixed at [LOW]; a Medium fold
            // would misleadingly flip the bubble amber, and as a
            // standalone (boot_bubble=false rollback) it is a routine
            // once-per-boot success ping, not an operator action.
            Self::OrderUpdateAuthenticated => Severity::Low,
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
            // 2026-07-06 Groww feed-down alerting: field-driven severity
            // (CrossVerify1mSummary precedent) — one variant carries the
            // in-market/off-hours split the Dhan WebSocketDisconnected /
            // ...OffHours pair encodes with two variants. High pages
            // Telegram + SNS mid-session; Low coalesces 60s off-hours
            // (the 2026-04-22 pre-market TCP-reset spam precedent class).
            Self::FeedDown { market_open, .. } => {
                if *market_open {
                    Severity::High
                } else {
                    Severity::Low
                }
            }
            // Mirrors WebSocketReconnected (Medium) — a recovery is
            // operator-visible but never pages SNS.
            Self::FeedRecovered { .. } => Severity::Medium,
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
            // W2 PR#5 (2026-07-10): the holiday-calendar coverage cliff
            // demands operator action (paste the next NSE circular) — High
            // pages Telegram; the watchdog's per-IST-date latch bounds it
            // to one page per process per day (audit Rule 4).
            Self::HolidayCalendarCoverageLow { .. } => Severity::High,
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
            // Daily timeframe-consistency verifier (2026-07-13): the
            // once-per-day post-market summary must arrive AT 3:40 PM IST,
            // not coalesced into a batching window — the exact
            // CrossVerify1mSummary rationale above.
            Self::TfConsistencySummary { .. } => DispatchPolicy::Immediate,
            // Dual-feed scorecard (2026-07-10): the once-per-day 15:45 IST
            // digest must arrive AT 15:45 (post-close = off-hours, so the
            // default Info routing would coalesce it) — same rationale as
            // CrossVerify1mSummary above.
            Self::DualFeedDailyScorecard { .. } => DispatchPolicy::Immediate,
            // BruteX cross-verify (2026-07-12): the once-per-day 15:50 IST
            // digest must arrive AT 15:50 (post-close = off-hours, so the
            // default Info routing would coalesce it) — same rationale as
            // CrossVerify1mSummary / DualFeedDailyScorecard above.
            Self::BrutexCrossverifySummary { .. } => DispatchPolicy::Immediate,
            // 2026-07-08 (verified incident, operator complaint "why every
            // telegram notification is very late"): PR #1439's in-market
            // digest (900s window) swept the three once-per-trading-day
            // market-open confirmations — "READY for market open @ 09:14"
            // arrived at 09:28, "Streaming live @ 09:15:30" at 09:30,
            // "self-test PASSED @ 09:16" at 09:30. Time-critical at the
            // open; ship instantly. Severity stays Info — Immediate wins
            // over severity in `classify_dispatch`, so color is untouched.
            // (SelfTestDegraded/SelfTestCritical are High/Critical and
            // already route immediate via the severity fallback.)
            Self::MarketOpenReadinessConfirmation { .. }
            | Self::MarketOpenStreamingConfirmation { .. }
            | Self::SelfTestPassed { .. } => DispatchPolicy::Immediate,
            // Instant informational status ping (Low severity → never SMS, but
            // ships immediately as its own message, not batched): a mid-market
            // crash/restart recovery + the feed on/off toggle acknowledgements.
            Self::CustomStatusUrgent { .. } => DispatchPolicy::Immediate,
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

    // -----------------------------------------------------------------------
    // Telegram UX Overhaul (2026-07-07) — episode_key / episode_role accessors
    // -----------------------------------------------------------------------

    #[test]
    fn test_episode_role_resolve_for_reconnect_open_for_disconnect() {
        use super::super::episode::EpisodeRole;
        let disc = NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: "reset".to_string(),
        };
        assert_eq!(disc.episode_role(), EpisodeRole::Open);
        let rec = NotificationEvent::WebSocketReconnected {
            connection_index: 0,
            reason: None,
            down_secs: 1,
            attempts: 1,
        };
        assert_eq!(rec.episode_role(), EpisodeRole::Resolve);
        let od_rec = NotificationEvent::OrderUpdateReconnected {
            consecutive_failures: 2,
        };
        assert_eq!(od_rec.episode_role(), EpisodeRole::Resolve);
    }

    #[test]
    fn test_episode_key_ws_lifecycle_variants_map_to_families() {
        use super::super::episode::{EpisodeFamily, EpisodeRole};
        let disc = NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: "reset".to_string(),
        };
        let key = disc.episode_key().expect("main-feed disconnect has a key");
        assert_eq!(key.family, EpisodeFamily::MainFeedWs);
        assert_eq!(key.conn, 0);
        assert_eq!(disc.episode_role(), EpisodeRole::Open);

        let off = NotificationEvent::WebSocketDisconnectedOffHours {
            connection_index: 0,
            reason: "reset".to_string(),
        };
        assert_eq!(
            off.episode_key().map(|k| k.family),
            Some(EpisodeFamily::MainFeedWs)
        );
        assert_eq!(off.episode_role(), EpisodeRole::Open);

        let rec = NotificationEvent::WebSocketReconnected {
            connection_index: 0,
            reason: None,
            down_secs: 10,
            attempts: 3,
        };
        assert_eq!(
            rec.episode_key().map(|k| k.family),
            Some(EpisodeFamily::MainFeedWs)
        );
        assert_eq!(rec.episode_role(), EpisodeRole::Resolve);

        let od = NotificationEvent::OrderUpdateDisconnected {
            reason: "reset".to_string(),
        };
        assert_eq!(
            od.episode_key().map(|k| k.family),
            Some(EpisodeFamily::OrderUpdateWs)
        );
        assert_eq!(od.episode_role(), EpisodeRole::Open);

        let or = NotificationEvent::OrderUpdateReconnected {
            consecutive_failures: 4,
        };
        assert_eq!(
            or.episode_key().map(|k| k.family),
            Some(EpisodeFamily::OrderUpdateWs)
        );
        assert_eq!(or.episode_role(), EpisodeRole::Resolve);
    }

    #[test]
    fn test_episode_key_none_for_non_episode_variants() {
        // Legacy path stays byte-identical for everything else.
        // (2026-07-09: the boot-milestone variants moved to the Boot
        // family — see test_episode_key_boot_variants_map_to_boot_family.)
        for event in [
            NotificationEvent::TokenRenewed,
            NotificationEvent::ShutdownInitiated,
            NotificationEvent::ShutdownComplete,
            NotificationEvent::SelfTestPassed { checks_passed: 8 },
            NotificationEvent::MarketOpenStreamingConfirmation {
                main_feed_active: 1,
                main_feed_total: 1,
                order_update_active: true,
            },
            NotificationEvent::Custom {
                message: "x".to_string(),
            },
        ] {
            assert!(
                event.episode_key().is_none(),
                "non-episode variant must return None: {:?}",
                event.topic()
            );
        }
    }

    // -----------------------------------------------------------------------
    // Boot bubble (2026-07-09) — episode_key / boot_milestone mapping
    // -----------------------------------------------------------------------

    /// Every boot-milestone variant, constructed once for the mapping tests.
    fn boot_mapped_events() -> Vec<NotificationEvent> {
        vec![
            NotificationEvent::BootHealthCheck {
                services_healthy: 3,
                services_total: 3,
            },
            NotificationEvent::AuthenticationSuccess,
            NotificationEvent::InstrumentBuildSuccess {
                source: "cache".to_string(),
                derivative_count: 1000,
                underlying_count: 46,
            },
            NotificationEvent::WebSocketPoolOnline {
                connected: 1,
                total: 1,
                per_connection: vec![(4, 5000, Some(2))],
                boot_path: BootPathLabel::Slow,
                boot_wall_clock_secs: 94.0,
                last_real_tick_age_secs: Some(2),
                feeds: vec![],
            },
            NotificationEvent::WebSocketPoolDeferredOffHours {
                deferred: 1,
                total: 1,
                boot_path: BootPathLabel::Slow,
                feeds: vec![],
            },
            NotificationEvent::OrderUpdateConnected,
            NotificationEvent::OrderUpdateAuthenticated,
            NotificationEvent::StartupComplete {
                mode: "sandbox",
                spot_1m_enabled: true,
                spot_1m_indices: 4,
                chain_1m_enabled: true,
                chain_1m_underlyings: 3,
            },
            NotificationEvent::FeedAuthOk {
                feed: "Groww".to_string(),
            },
            NotificationEvent::FeedInstrumentsLoaded {
                feed: "Groww".to_string(),
                subscribed: 768,
                indices: 2,
                stocks: 766,
                skipped: 0,
            },
            NotificationEvent::FeedConnectedAwaitingTicks {
                feed: "Groww".to_string(),
                subscribed: 768,
                market_open: true,
            },
        ]
    }

    #[test]
    fn test_episode_key_boot_variants_map_to_boot_family() {
        use super::super::episode::{BOOT_EPISODE_KEY, EpisodeRole};
        for event in boot_mapped_events() {
            assert_eq!(
                event.episode_key(),
                Some(BOOT_EPISODE_KEY),
                "boot milestone must map to the Boot key: {}",
                event.topic()
            );
            assert_eq!(
                event.episode_role(),
                EpisodeRole::Open,
                "boot milestones always fold as Open"
            );
            assert!(
                event.boot_milestone().is_some(),
                "boot-mapped variant must carry a milestone: {}",
                event.topic()
            );
        }
    }

    #[test]
    fn test_episode_key_non_groww_feed_is_none() {
        // A future feed #3 (or a renamed feed) keeps the legacy lane —
        // never a wrong fold into the boot bubble.
        for event in [
            NotificationEvent::FeedAuthOk {
                feed: "SomeOtherFeed".to_string(),
            },
            NotificationEvent::FeedInstrumentsLoaded {
                feed: "SomeOtherFeed".to_string(),
                subscribed: 10,
                indices: 1,
                stocks: 9,
                skipped: 0,
            },
            NotificationEvent::FeedConnectedAwaitingTicks {
                feed: "SomeOtherFeed".to_string(),
                subscribed: 10,
                market_open: false,
            },
        ] {
            assert!(event.episode_key().is_none(), "{}", event.topic());
            assert!(event.boot_milestone().is_none(), "{}", event.topic());
        }
    }

    #[test]
    fn test_boot_milestone_mapping_all_variants() {
        use super::super::episode::{BootMilestone, BootMilestone as M};
        let m = NotificationEvent::BootHealthCheck {
            services_healthy: 3,
            services_total: 4,
        }
        .boot_milestone();
        assert_eq!(
            m,
            Some(M::Services {
                healthy: 3,
                total: 4
            })
        );
        assert_eq!(
            NotificationEvent::AuthenticationSuccess.boot_milestone(),
            Some(M::DhanAuth)
        );
        assert_eq!(
            NotificationEvent::InstrumentBuildSuccess {
                source: "cache".to_string(),
                derivative_count: 1000,
                underlying_count: 46,
            }
            .boot_milestone(),
            Some(M::Instruments { count: 1046 }),
            "count = derivatives + underlyings"
        );
        assert_eq!(
            NotificationEvent::WebSocketPoolOnline {
                connected: 1,
                total: 1,
                per_connection: vec![],
                boot_path: BootPathLabel::Slow,
                boot_wall_clock_secs: 1.0,
                last_real_tick_age_secs: None,
                feeds: vec![],
            }
            .boot_milestone(),
            Some(M::DhanFeedOnline {
                connected: 1,
                total: 1,
                last_real_tick_age_secs: None
            })
        );
        assert_eq!(
            NotificationEvent::WebSocketPoolDeferredOffHours {
                deferred: 1,
                total: 1,
                boot_path: BootPathLabel::Slow,
                feeds: vec![],
            }
            .boot_milestone(),
            Some(M::DhanFeedDeferredOffHours)
        );
        assert_eq!(
            NotificationEvent::OrderUpdateConnected.boot_milestone(),
            Some(M::OrderUpdateConnected)
        );
        assert_eq!(
            NotificationEvent::OrderUpdateAuthenticated.boot_milestone(),
            Some(M::OrderUpdateAuthenticated)
        );
        assert_eq!(
            NotificationEvent::StartupComplete {
                mode: "sandbox",
                spot_1m_enabled: true,
                spot_1m_indices: 4,
                chain_1m_enabled: true,
                chain_1m_underlyings: 3
            }
            .boot_milestone(),
            Some(BootMilestone::Complete { mode: "sandbox" })
        );
        assert_eq!(
            NotificationEvent::FeedAuthOk {
                feed: "Groww".to_string()
            }
            .boot_milestone(),
            Some(M::GrowwAuth)
        );
        assert_eq!(
            NotificationEvent::FeedInstrumentsLoaded {
                feed: "Groww".to_string(),
                subscribed: 768,
                indices: 2,
                stocks: 766,
                skipped: 0,
            }
            .boot_milestone(),
            Some(M::GrowwInstruments { subscribed: 768 })
        );
        assert_eq!(
            NotificationEvent::FeedConnectedAwaitingTicks {
                feed: "Groww".to_string(),
                subscribed: 768,
                market_open: false,
            }
            .boot_milestone(),
            Some(M::GrowwConnected { market_open: false })
        );
        // Non-boot events carry no milestone.
        assert!(NotificationEvent::TokenRenewed.boot_milestone().is_none());
    }

    #[test]
    fn test_boot_episode_key_only_success_milestones() {
        // Ratchet: nothing with severity ≥ High may ever map to the Boot
        // bubble — failure events keep their legacy instant paging. The
        // boot bubble's prefix is fixed at [LOW], so folding a paging
        // event would silently demote it.
        for event in boot_mapped_events() {
            assert!(
                event.severity() < Severity::High,
                "a paging-severity event must never fold into the boot bubble: {} ({:?})",
                event.topic(),
                event.severity()
            );
        }
        // The #1443 instant trio stays OUT of the bubble (separate instant
        // messages) — regression pin for the market-open lane.
        for event in [
            NotificationEvent::SelfTestPassed { checks_passed: 8 },
            NotificationEvent::MarketOpenStreamingConfirmation {
                main_feed_active: 1,
                main_feed_total: 1,
                order_update_active: true,
            },
            NotificationEvent::MarketOpenReadinessConfirmation {
                main_feed_active: 1,
                main_feed_total: 1,
                order_update_active: true,
                token_remaining_secs: 20_000,
            },
        ] {
            assert!(
                event.episode_key().is_none(),
                "#1443 instant-lane event must stay standalone: {}",
                event.topic()
            );
            assert_eq!(event.dispatch_policy(), DispatchPolicy::Immediate);
        }
    }

    #[test]
    fn test_order_update_authenticated_severity_is_low() {
        // 2026-07-09 pin: folding into the green [LOW] boot checklist —
        // Medium would flip the bubble amber.
        assert_eq!(
            NotificationEvent::OrderUpdateAuthenticated.severity(),
            Severity::Low
        );
    }

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
                NotificationEvent::StartupComplete {
                    mode: "LIVE",
                    spot_1m_enabled: true,
                    spot_1m_indices: 4,
                    chain_1m_enabled: true,
                    chain_1m_underlyings: 3,
                },
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
        let event = NotificationEvent::StartupComplete {
            mode: "LIVE",
            spot_1m_enabled: true,
            spot_1m_indices: 4,
            chain_1m_enabled: true,
            chain_1m_underlyings: 3,
        };
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

    /// 2026-07-13 operator visibility rider: the boot Telegram states the
    /// per-minute price capture's ACTUAL config state — on/on, on/off,
    /// off/on and off/off arms all worded truthfully.
    #[test]
    fn test_startup_complete_per_minute_capture_wording_arms() {
        let build = |spot: bool, chain: bool| NotificationEvent::StartupComplete {
            mode: "LIVE",
            spot_1m_enabled: spot,
            spot_1m_indices: 4,
            chain_1m_enabled: chain,
            chain_1m_underlyings: 3,
        };

        // 2026-07-14 broker tag: the capture line reports the DHAN REST
        // legs — it must SAY Dhan and note the Groww legs report on their
        // own alerts (every arm).
        let both = build(true, true).to_message();
        assert!(
            both.contains("Dhan per-minute price capture — armed"),
            "got: {both}"
        );
        assert!(both.contains("9:16 AM to 3:30 PM IST"), "got: {both}");
        assert!(
            both.contains("Dhan spot candles for 4 indices"),
            "got: {both}"
        );
        assert!(
            both.contains("Dhan option chain for 3 indices"),
            "got: {both}"
        );
        assert!(!both.contains("switched off"), "got: {both}");
        assert!(
            both.contains("Groww per-minute legs report separately"),
            "got: {both}"
        );

        let chain_off = build(true, false).to_message();
        assert!(
            chain_off.contains("Dhan spot candles for 4 indices"),
            "got: {chain_off}"
        );
        assert!(
            chain_off.contains("Dhan option chain — switched off"),
            "got: {chain_off}"
        );

        let spot_off = build(false, true).to_message();
        assert!(
            spot_off.contains("Dhan option chain for 3 indices"),
            "got: {spot_off}"
        );
        assert!(
            spot_off.contains("Dhan spot candles — switched off"),
            "got: {spot_off}"
        );

        let both_off = build(false, false).to_message();
        assert!(
            both_off.contains("Dhan per-minute price capture — switched off"),
            "got: {both_off}"
        );
        assert!(!both_off.contains("armed"), "got: {both_off}");
        assert!(
            both_off.contains("Groww per-minute legs report separately"),
            "got: {both_off}"
        );
    }

    #[test]
    fn test_startup_complete_message_contains_build_sha() {
        // B9 deploy provenance: the boot Telegram must carry the short git
        // SHA of the running binary so the operator sees WHICH code booted.
        let event = NotificationEvent::StartupComplete {
            mode: "LIVE",
            spot_1m_enabled: true,
            spot_1m_indices: 4,
            chain_1m_enabled: true,
            chain_1m_underlyings: 3,
        };
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
            fleet_summary: false,
            detail: None,
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
    fn test_groww_sidecar_rejected_fleet_summary_body_is_fleet_aware() {
        // Hostile-review fix 2026-07-06: a PARTIAL-fleet coalesced summary
        // must not be wrapped in the single-conn total-outage trailer — the
        // body would simultaneously report a partial count AND claim the
        // whole feed is "receiving nothing" / "prices will not flow".
        let event = NotificationEvent::GrowwSidecarRejected {
            reason: "7 of 40 connections retrying (server session limit or throttle) — \
                     no reject reported from the other 33 connections"
                .to_string(),
            fleet_summary: true,
            detail: None,
        };
        let msg = event.to_message();
        assert!(
            msg.contains("7 of 40 connections retrying"),
            "fleet summary reason missing: {msg}"
        );
        assert!(
            !msg.contains("receiving nothing"),
            "total-outage trailer must not wrap a partial fleet summary: {msg}"
        );
        assert!(
            !msg.contains("Groww prices will not flow"),
            "whole-feed no-flow claim must not wrap a partial fleet summary: {msg}"
        );
        assert!(
            msg.contains("Prices from those connections will not flow"),
            "fleet trailer must scope the no-flow claim to the affected connections: {msg}"
        );
        // Same topic/severity/badge as the single-conn arm — only the body
        // trailer is fleet-aware.
        assert_eq!(event.topic(), "GrowwSidecarRejected");
        assert_eq!(event.severity(), Severity::High);
        // The single-conn arm keeps its original total-outage wording.
        let single = NotificationEvent::GrowwSidecarRejected {
            reason: "access token stale".to_string(),
            fleet_summary: false,
            detail: None,
        };
        assert!(single.to_message().contains("receiving nothing"));
    }

    #[test]
    fn test_groww_sidecar_rejected_html_escapes_reason() {
        // Defense-in-depth: even though the supervisor only passes a fixed
        // &'static str reason, any angle brackets are escaped at the render
        // boundary (consistent with every other String arm).
        let event = NotificationEvent::GrowwSidecarRejected {
            reason: "<script>".to_string(),
            fleet_summary: false,
            detail: None,
        };
        let msg = event.to_message();
        assert!(!msg.contains("<script>"), "raw HTML not escaped: {msg}");
        assert!(
            msg.contains("&lt;script&gt;"),
            "expected escaped form: {msg}"
        );
    }

    #[test]
    fn test_groww_sidecar_rejected_detail_renders_reason_and_retrying() {
        // 2026-07-14 operator demand (the 14:59 IST page carried no WHY):
        // when the SANITIZED reject-cause signature is present, the headline
        // names it verbatim — "live feed rejected: <cause> — retrying" — and
        // the class's plain-English explanation line is KEPT beneath it.
        // The detail legitimately carries SDK/NATS wording (`growwapi`,
        // `nats:`) per the dated commandment-2 override recorded in
        // feed-stall-watchdog-error-codes.md §1c.1 — so the no-jargon pin in
        // test_groww_sidecar_rejected_renders_reason_and_topic_and_severity
        // applies only to the detail-less form.
        let event = NotificationEvent::GrowwSidecarRejected {
            reason: "the feed reported an error and is retrying".to_string(),
            fleet_summary: false,
            detail: Some(
                "ERROR growwapi.groww.nats_client: Error: nats: unexpected EOF".to_string(),
            ),
        };
        let msg = event.to_message();
        assert!(
            msg.contains(
                "Groww live feed rejected: ERROR growwapi.groww.nats_client: \
                 Error: nats: unexpected EOF — retrying"
            ),
            "headline must name the sanitized cause + retrying: {msg}"
        );
        // The existing class explanation stays as the body line.
        assert!(
            msg.contains("the feed reported an error and is retrying"),
            "class explanation must be kept: {msg}"
        );
        // The single-conn total-outage trailer is unchanged.
        assert!(msg.contains("receiving nothing"), "trailer lost: {msg}");
        // Badge ordering untouched (PR #1529 owns the badge arms).
        assert!(
            msg.starts_with("🟢 GROWW — "),
            "badge ordering broke: {msg}"
        );
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_groww_sidecar_rejected_empty_detail_degrades_to_generic() {
        // An empty / whitespace / None detail must render the EXACT generic
        // pre-2026-07-14 headline — never a hollow "rejected:  — retrying".
        let expected = NotificationEvent::GrowwSidecarRejected {
            reason: "the feed reported an error and is retrying".to_string(),
            fleet_summary: false,
            detail: None,
        }
        .to_message();
        assert!(
            expected.contains("Groww live feed rejected</b>"),
            "generic headline missing: {expected}"
        );
        for hollow in [Some(String::new()), Some("   ".to_string())] {
            let msg = NotificationEvent::GrowwSidecarRejected {
                reason: "the feed reported an error and is retrying".to_string(),
                fleet_summary: false,
                detail: hollow,
            }
            .to_message();
            assert_eq!(msg, expected, "empty detail must degrade to generic");
            assert!(
                !msg.contains("rejected:"),
                "hollow 'rejected:' headline leaked: {msg}"
            );
        }
    }

    #[test]
    fn test_groww_sidecar_rejected_detail_html_escaped_and_recapped() {
        // Defense-in-depth at the render boundary: the emit-site contract
        // already sanitizes + caps, but a hostile/oversized detail from any
        // future emit site is re-capped to 160 chars (BEFORE escaping, so an
        // entity is never cut mid-sequence) and html-escaped.
        let event = NotificationEvent::GrowwSidecarRejected {
            reason: "the feed reported an error and is retrying".to_string(),
            fleet_summary: false,
            detail: Some(format!("<script>{}", "x".repeat(400))),
        };
        let msg = event.to_message();
        assert!(!msg.contains("<script>"), "raw HTML not escaped: {msg}");
        assert!(
            msg.contains("rejected: &lt;script&gt;"),
            "escaped detail missing from headline: {msg}"
        );
        // 160-char cap: the raw detail is 408 chars; after the cap the
        // headline's x-run is 160 - "<script>".len() = 152 chars long.
        assert!(
            msg.contains(&"x".repeat(152)) && !msg.contains(&"x".repeat(153)),
            "detail must be re-capped to 160 chars at the render boundary: {msg}"
        );
    }

    #[test]
    fn test_feed_down_in_market_is_high_severity() {
        // Regression pin (2026-07-06 Groww feed-down alerting): a mid-session
        // feed-DOWN must PAGE (High → Telegram + SNS) — the incident class is
        // "Groww died at 10:31 IST, operator learned hours later".
        let ev = NotificationEvent::FeedDown {
            feed: "Groww".to_string(),
            reason: "feed switched off by the operator".to_string(),
            market_open: true,
            operator_initiated: true,
        };
        assert_eq!(ev.severity(), Severity::High);
    }

    #[test]
    fn test_feed_down_off_hours_is_low_severity() {
        // Regression pin: off-hours feed-down must be Low (60s coalesced) —
        // the 2026-04-22 pre-market Telegram-spam precedent class
        // (WebSocketDisconnectedOffHours). The split is field-driven, not a
        // second variant.
        let ev = NotificationEvent::FeedDown {
            feed: "Groww".to_string(),
            reason: "feed switched off by the operator".to_string(),
            market_open: false,
            operator_initiated: true,
        };
        assert_eq!(ev.severity(), Severity::Low);
    }

    #[test]
    fn test_feed_recovered_is_medium_severity() {
        // Mirrors WebSocketReconnected (Medium): recovery is visible
        // immediately but never pages SNS.
        let ev = NotificationEvent::FeedRecovered {
            feed: "Groww".to_string(),
            down_secs: 154,
        };
        assert_eq!(ev.severity(), Severity::Medium);
    }

    #[test]
    fn test_feed_down_renders_reason_and_topic_and_severity() {
        // 10-commandments pin (pattern: GrowwSidecarRejected test above).
        let ev = NotificationEvent::FeedDown {
            feed: "Groww".to_string(),
            reason: "internal restart — recovering automatically".to_string(),
            market_open: true,
            operator_initiated: false,
        };
        let msg = ev.to_message();
        assert!(
            msg.contains("internal restart — recovering automatically"),
            "reason missing from message: {msg}"
        );
        // Status emoji at the start of the body (commandments 5/10) + the
        // honest no-flow + auto-retry trailer.
        assert!(msg.contains("🆘"), "severity emoji missing: {msg}");
        assert!(msg.contains("Groww"), "feed name missing: {msg}");
        assert!(
            msg.contains("will not flow until it recovers"),
            "honest no-flow claim missing: {msg}"
        );
        assert!(
            msg.contains("retrying automatically"),
            "auto-retry reassurance missing: {msg}"
        );
        // No library names / file paths (10 commandments).
        assert!(!msg.contains(".rs"), "file path leaked: {msg}");
        assert!(!msg.contains("mpsc"), "lib jargon leaked: {msg}");
        assert_eq!(ev.topic(), "FeedDown");
        assert_eq!(ev.severity(), Severity::High);
        // Off-hours body flips register: soft warning, idle-is-normal.
        let off = NotificationEvent::FeedDown {
            feed: "Groww".to_string(),
            reason: "feed switched off by the operator".to_string(),
            market_open: false,
            operator_initiated: true,
        };
        let off_msg = off.to_message();
        assert!(
            off_msg.contains("off-hours") && off_msg.contains("idle is normal"),
            "off-hours body must say off-hours + idle is normal: {off_msg}"
        );
    }

    #[test]
    fn test_feed_down_operator_disable_body_names_the_action_not_auto_retry() {
        // Regression pin (2026-07-06 fix): a DELIBERATE runtime disable is
        // never retried by the system — the body must name the ONE action
        // that fixes it (re-enable from the feeds page) and must NOT claim
        // automatic retry/reconnect (Telegram commandment 7; a false
        // auto-retry promise actively delays recovery — audit Rule 11).
        let ev = NotificationEvent::FeedDown {
            feed: "Groww".to_string(),
            reason: "feed switched off by the operator".to_string(),
            market_open: true,
            operator_initiated: true,
        };
        let msg = ev.to_message();
        assert!(
            msg.contains("re-enable it from the feeds page"),
            "operator-disable body must name the re-enable action: {msg}"
        );
        assert!(
            !msg.contains("retrying automatically") && !msg.contains("Reconnect is automatic"),
            "operator-disable body must NOT claim automatic retry: {msg}"
        );
        // Off-hours form of the same deliberate disable: same rule.
        let off = NotificationEvent::FeedDown {
            feed: "Groww".to_string(),
            reason: "feed switched off by the operator".to_string(),
            market_open: false,
            operator_initiated: true,
        };
        let off_msg = off.to_message();
        assert!(
            off_msg.contains("re-enable it from the feeds page"),
            "off-hours operator-disable body must name the re-enable action: {off_msg}"
        );
        assert!(
            !off_msg.contains("Reconnect is automatic"),
            "off-hours operator-disable body must NOT claim automatic reconnect: {off_msg}"
        );
    }

    #[test]
    fn test_feed_down_html_escapes_reason() {
        // Defense-in-depth: reasons are fixed literals at every emit site,
        // but any angle brackets are escaped at the render boundary
        // (consistent with every other String arm).
        let ev = NotificationEvent::FeedDown {
            feed: "Groww".to_string(),
            reason: "<script>".to_string(),
            market_open: true,
            operator_initiated: false,
        };
        let msg = ev.to_message();
        assert!(!msg.contains("<script>"), "raw HTML not escaped: {msg}");
        assert!(
            msg.contains("&lt;script&gt;"),
            "expected escaped form: {msg}"
        );
    }

    #[test]
    fn test_feed_recovered_renders_down_secs_and_topic() {
        let ev = NotificationEvent::FeedRecovered {
            feed: "Groww".to_string(),
            down_secs: 154,
        };
        let msg = ev.to_message();
        assert!(msg.contains("✅"), "recovery emoji missing: {msg}");
        assert!(
            msg.contains("streaming again"),
            "recovery must claim STREAMING (ticks), not just socket-connect: {msg}"
        );
        assert!(msg.contains("154"), "down duration missing: {msg}");
        assert!(msg.contains("Groww"), "feed name missing: {msg}");
        assert_eq!(ev.topic(), "FeedRecovered");
    }

    #[test]
    fn test_feed_down_and_recovered_resolve_groww_badge() {
        // Both variants must resolve the 🟢 GROWW badge via the feed-generic
        // arm — falling to `_ => None` would violate the 2026-07-05
        // "uniquely seen" per-feed badge directive.
        let down = NotificationEvent::FeedDown {
            feed: "Groww".to_string(),
            reason: "x".to_string(),
            market_open: true,
            operator_initiated: false,
        };
        let rec = NotificationEvent::FeedRecovered {
            feed: "Groww".to_string(),
            down_secs: 1,
        };
        assert_eq!(down.feed_badge(), Some("🟢 GROWW"));
        assert_eq!(rec.feed_badge(), Some("🟢 GROWW"));
    }

    #[test]
    fn test_startup_complete_offline_message() {
        let event = NotificationEvent::StartupComplete {
            mode: "OFFLINE",
            spot_1m_enabled: true,
            spot_1m_indices: 4,
            chain_1m_enabled: true,
            chain_1m_underlyings: 3,
        };
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
        // 2026-07-14 noise-lock reword pins: broker named + consequence.
        assert!(
            msg.contains("Dhan login could not be obtained"),
            "got: {msg}"
        );
        assert!(
            msg.contains("spot-1m and option-chain pulls will stop"),
            "consequence line missing: {msg}"
        );
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
        assert!(msg.contains("Dhan login could not be obtained"));
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

    // -- CustomStatus (2026-07-10 Telegram noise cut, F2) --

    #[test]
    fn test_custom_status_severity_is_low() {
        // Informational status pings must be Low so they batch and never SMS.
        let event = NotificationEvent::CustomStatus {
            message: "status".to_string(),
        };
        assert_eq!(event.severity(), Severity::Low);
        // Below the SNS-SMS threshold (`service.rs` gates SMS on `>= High`).
        assert!(event.severity() < Severity::High);
    }

    #[test]
    fn test_custom_status_message_passthrough() {
        let event = NotificationEvent::CustomStatus {
            message: "status payload".to_string(),
        };
        assert_eq!(event.to_message(), "status payload");
    }

    #[test]
    fn test_custom_status_topic_is_distinct_from_custom() {
        let status = NotificationEvent::CustomStatus {
            message: "x".to_string(),
        };
        let custom = NotificationEvent::Custom {
            message: "x".to_string(),
        };
        assert_eq!(status.topic(), "CustomStatus");
        assert_eq!(custom.topic(), "Custom");
        assert_ne!(status.topic(), custom.topic());
    }

    #[test]
    fn test_custom_status_episode_key_is_none() {
        // Not episode-routed — the episode machinery stays untouched (F1 deferred).
        let event = NotificationEvent::CustomStatus {
            message: "x".to_string(),
        };
        assert!(event.episode_key().is_none());
    }

    #[test]
    fn test_low_custom_status_never_immediate_or_sms() {
        use crate::notification::coalescer::{DispatchLane, classify_dispatch};
        let event = NotificationEvent::CustomStatus {
            message: "x".to_string(),
        };
        // Feed the event's OWN severity() + dispatch_policy() into the real
        // classifier — so reverting `CustomStatus.severity()` back to High
        // makes THIS test fail end-to-end (the Immediate lane resolves and the
        // SMS assertion breaks), not just the sibling severity pin.
        let severity = event.severity();
        let policy = event.dispatch_policy();
        // In-market → Digest, off-hours → Coalesce60. Never Immediate — the
        // only SMS-carrying lane for a non-episode event. This is the direct
        // "noise is cut" assertion.
        let in_market = classify_dispatch(
            severity,
            policy,
            event.episode_key().map(|_| event.episode_role()),
            true,
        );
        let off_hours = classify_dispatch(
            severity,
            policy,
            event.episode_key().map(|_| event.episode_role()),
            false,
        );
        assert_eq!(in_market, DispatchLane::Digest);
        assert_eq!(off_hours, DispatchLane::Coalesce60);
        assert_ne!(in_market, DispatchLane::Immediate);
        assert_ne!(off_hours, DispatchLane::Immediate);
        // The SMS gate (`service.rs`) is `severity >= High`; Low is below it.
        assert!(
            severity < Severity::High,
            "CustomStatus must be below the SMS threshold"
        );
    }

    // -- CustomStatusUrgent (2026-07-10, FIX-5): instant but NEVER SMS --

    #[test]
    fn test_custom_status_urgent_is_immediate_and_low() {
        use crate::notification::coalescer::{DispatchLane, classify_dispatch};
        let event = NotificationEvent::CustomStatusUrgent {
            message: "x".to_string(),
        };
        let severity = event.severity();
        let policy = event.dispatch_policy();
        // Instant: ships as its own message regardless of market phase.
        assert_eq!(policy, DispatchPolicy::Immediate);
        assert_eq!(
            classify_dispatch(severity, policy, None, true),
            DispatchLane::Immediate,
        );
        assert_eq!(
            classify_dispatch(severity, policy, None, false),
            DispatchLane::Immediate,
        );
        // But NEVER SMS: severity is Low, strictly below the `>= High` gate.
        assert_eq!(severity, Severity::Low);
        assert!(
            severity < Severity::High,
            "CustomStatusUrgent must be instant but below the SMS threshold"
        );
    }

    #[test]
    fn test_custom_status_urgent_episode_key_is_none_and_topic_distinct() {
        let event = NotificationEvent::CustomStatusUrgent {
            message: "payload".to_string(),
        };
        assert!(event.episode_key().is_none());
        assert_eq!(event.topic(), "CustomStatusUrgent");
        assert_eq!(event.to_message(), "payload");
    }

    #[test]
    fn test_startup_complete_is_info() {
        let event = NotificationEvent::StartupComplete {
            mode: "LIVE",
            spot_1m_enabled: true,
            spot_1m_indices: 4,
            chain_1m_enabled: true,
            chain_1m_underlyings: 3,
        };
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
        // Cluster-C (2026-07-14): the self-heal line — a lone OPENED page
        // must tell the operator the system retries on its own.
        assert!(msg.contains("retries by itself"));
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
        // Cluster-C (2026-07-14): Critical bodies carry action lines
        // (Telegram commandment 7).
        assert!(msg.contains("What you need to do RIGHT NOW"));
        assert!(msg.contains("All new orders are blocked"));
        assert_eq!(event.severity(), Severity::Critical);
    }

    /// Cluster-C (2026-07-14): the 5 OMS/risk events are Dhan-badged —
    /// orders are Dhan-only today, so their pages must carry the same
    /// feed badge as the order-update WS lifecycle events.
    #[test]
    fn test_oms_risk_events_are_dhan_badged() {
        let events = [
            NotificationEvent::OrderRejected {
                correlation_id: "X".to_string(),
                reason: "bad".to_string(),
            },
            NotificationEvent::CircuitBreakerOpened {
                consecutive_failures: 3,
            },
            NotificationEvent::CircuitBreakerClosed,
            NotificationEvent::RateLimitExhausted {
                limit_type: "per_second".to_string(),
            },
            NotificationEvent::RiskHalt {
                reason: "x".to_string(),
            },
        ];
        for event in events {
            let badge = event
                .feed_badge()
                .unwrap_or_else(|| panic!("{} must be feed-badged", event.topic()));
            assert!(
                badge.contains("DHAN"),
                "{} must carry the Dhan badge, got {badge}",
                event.topic()
            );
            assert!(
                event.to_message().starts_with(badge),
                "{} message must start with the badge",
                event.topic()
            );
        }
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

    /// 2026-07-08 ratchet (verified incident, operator complaint "why every
    /// telegram notification is very late"): PR #1439's in-market digest
    /// (900s window) swept the three once-per-trading-day market-open
    /// confirmations — READY @ 09:14 delivered at 09:28, Streaming live
    /// @ 09:15:30 at 09:30, self-test PASSED @ 09:16 at 09:30. These are
    /// time-critical at the open: each MUST request `DispatchPolicy::
    /// Immediate` (bypassing the digest via `classify_dispatch`) while
    /// KEEPING `Severity::Info` (color decoupled from routing). Blocks
    /// regression to the digest-swept pattern.
    #[test]
    fn test_market_open_confirmations_are_immediate_info() {
        let events = [
            NotificationEvent::MarketOpenReadinessConfirmation {
                main_feed_active: 1,
                main_feed_total: 1,
                order_update_active: true,
                token_remaining_secs: 20 * 3600,
            },
            NotificationEvent::MarketOpenStreamingConfirmation {
                main_feed_active: 1,
                main_feed_total: 1,
                order_update_active: true,
            },
            NotificationEvent::SelfTestPassed { checks_passed: 7 },
        ];
        for event in events {
            assert_eq!(
                event.dispatch_policy(),
                DispatchPolicy::Immediate,
                "{} must request immediate dispatch (verified incident 2026-07-08 — \
                 the in-market digest delayed the market-open confirmations ~15m)",
                event.topic()
            );
            assert_eq!(
                event.severity(),
                Severity::Info,
                "{} must stay Info — Immediate wins over severity in \
                 classify_dispatch, so no severity bump is needed",
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
        let info_event = NotificationEvent::StartupComplete {
            mode: "LIVE",
            spot_1m_enabled: true,
            spot_1m_indices: 4,
            chain_1m_enabled: true,
            chain_1m_underlyings: 3,
        };
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

    // test_mid_session_profile_invalidated_escapes_external_reason DELETED
    // 2026-07-14 with the MidSessionProfileInvalidated variant (Dhan noise
    // lock) — the terminal arm routes through AuthenticationFailed, whose
    // escape test is directly above.

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
        // 2026-07-14 broker tag: the digest is a BOTH-feed summary, so the
        // Dhan connection-count line names Dhan explicitly.
        assert!(msg.contains("Dhan main feed: 1/1"));
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
        assert!(msg.contains("Dhan main feed: 0/1"));
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

    // -----------------------------------------------------------------------
    // Per-feed visual identity (operator directive 2026-07-05: "dhan and
    // groww should be uniquely seen to view easily") — the feed badge leads
    // every feed-scoped Telegram body.
    // -----------------------------------------------------------------------

    #[test]
    fn test_dhan_scoped_events_carry_dhan_badge_in_message() {
        // Dhan auth / order-update WS / main-feed WS / instrument events all
        // lead with the Dhan badge so the operator can tell feeds apart at
        // a glance.
        let events = [
            NotificationEvent::AuthenticationSuccess,
            NotificationEvent::OrderUpdateConnected,
            NotificationEvent::TokenRenewed,
            NotificationEvent::InstrumentBuildSuccess {
                source: "fresh_csv_build".to_string(),
                derivative_count: 0,
                underlying_count: 4,
            },
        ];
        for ev in events {
            assert_eq!(ev.feed_badge(), Some("🔷 DHAN"), "event: {}", ev.topic());
            let msg = ev.to_message();
            assert!(
                msg.starts_with("🔷 DHAN — "),
                "Dhan-scoped body must lead with the Dhan badge: {msg}"
            );
        }
    }

    #[test]
    fn test_groww_feed_events_carry_groww_badge_in_message() {
        let sidecar = NotificationEvent::GrowwSidecarRejected {
            reason: "access token stale".to_string(),
            fleet_summary: false,
            detail: None,
        };
        assert_eq!(sidecar.feed_badge(), Some("🟢 GROWW"));
        assert!(
            sidecar.to_message().starts_with("🟢 GROWW — "),
            "got: {}",
            sidecar.to_message()
        );
        let auth = NotificationEvent::FeedAuthOk {
            feed: "Groww".to_string(),
        };
        let msg = auth.to_message();
        assert!(
            msg.starts_with("🟢 GROWW — "),
            "Groww feed events must lead with the Groww badge: {msg}"
        );
    }

    #[test]
    fn test_dynamic_feed_events_badge_follows_feed_field() {
        // The generic Feed* events resolve their badge from the `feed`
        // field — Dhan-tagged renders Dhan, Groww-tagged renders Groww, an
        // unknown future feed renders UN-badged (honest, never wrong).
        let dhan = NotificationEvent::FeedInstrumentsLoaded {
            feed: "Dhan".to_string(),
            subscribed: 243,
            indices: 25,
            stocks: 218,
            skipped: 0,
        };
        assert_eq!(dhan.feed_badge(), Some("🔷 DHAN"));
        let groww = NotificationEvent::FeedConnectedAwaitingTicks {
            feed: "Groww".to_string(),
            subscribed: 768,
            market_open: false,
        };
        assert_eq!(groww.feed_badge(), Some("🟢 GROWW"));
        let unknown = NotificationEvent::FeedAuthOk {
            feed: "SomeFutureFeed".to_string(),
        };
        assert_eq!(unknown.feed_badge(), None);
        assert!(
            !unknown.to_message().contains("DHAN") && !unknown.to_message().contains("GROWW"),
            "unknown feed must render un-badged"
        );
    }

    #[test]
    fn test_non_feed_events_carry_no_feed_badge() {
        // Host/system-level events are byte-identical to before — no badge,
        // no "—" prefix injected (never a wrong broker tag; their titles
        // already name the system component in plain words).
        // NOTE (2026-07-14): IpVerificationSuccess moved OUT of this list —
        // it is Dhan-scoped (static-IP order gate) and now carries 🔷 DHAN.
        let events = [
            NotificationEvent::StartupComplete {
                mode: "sandbox",
                spot_1m_enabled: true,
                spot_1m_indices: 4,
                chain_1m_enabled: true,
                chain_1m_underlyings: 3,
            },
            NotificationEvent::QuestDbReconnected {
                writer: "ticks".to_string(),
                failed_checks_before_recovery: 3,
            },
            NotificationEvent::BootHealthCheck {
                services_healthy: 3,
                services_total: 3,
            },
            NotificationEvent::ShutdownComplete,
        ];
        for ev in events {
            assert_eq!(ev.feed_badge(), None, "event: {}", ev.topic());
            let msg = ev.to_message();
            assert!(
                !msg.starts_with("🔷") && !msg.starts_with("🟢"),
                "non-feed body must not lead with a feed badge: {msg}"
            );
        }
    }

    #[test]
    fn test_dhan_rest_leg_events_carry_dhan_badge() {
        // Operator directive 2026-07-14: the per-minute REST pull alerts
        // must name the feed. Every Dhan REST-leg event (spot 1m + option
        // chain) leads with the Dhan badge — trigger AND recovery pairwise,
        // so an edge-cleared message can never render un-tagged while its
        // trigger is tagged. Ratchet: removing any arm fails this test.
        let events = [
            NotificationEvent::Spot1mFetchDegraded {
                consecutive_failed_minutes: 3,
                minute_ist: "10:15".to_string(),
            },
            NotificationEvent::Spot1mFetchRecovered {
                minute_ist: "10:18".to_string(),
                failed_minutes: 3,
            },
            NotificationEvent::Spot1mSidNotServed {
                symbol: "INDIA VIX".to_string(),
                consecutive_minutes: 10,
            },
            NotificationEvent::Spot1mSidServedRecovered {
                symbol: "INDIA VIX".to_string(),
                not_served_minutes: 10,
            },
            NotificationEvent::ChainFetchDegraded {
                consecutive_failed_minutes: 3,
                minute_ist: "10:15".to_string(),
            },
            NotificationEvent::ChainFetchRecovered {
                minute_ist: "10:18".to_string(),
                failed_minutes: 3,
            },
            NotificationEvent::ChainEntitlementAbsent {
                pipeline_enabled: true,
                detail: "no subscription".to_string(),
            },
            NotificationEvent::ChainEntitlementConfirmed,
            NotificationEvent::ChainExpirylistFailed {
                detail: "lookup failed".to_string(),
            },
        ];
        for ev in events {
            assert_eq!(ev.feed_badge(), Some("🔷 DHAN"), "event: {}", ev.topic());
            let msg = ev.to_message();
            assert!(
                msg.starts_with("🔷 DHAN — "),
                "Dhan REST-leg body must lead with the Dhan badge: {msg}"
            );
        }
    }

    #[test]
    fn test_groww_rest_leg_events_carry_groww_badge() {
        // The Groww per-minute REST legs (spot 1m + option chain + option
        // contract) lead with the Groww badge — trigger AND recovery
        // pairwise. Ratchet: removing any arm fails this test.
        let events = [
            NotificationEvent::GrowwSpot1mFetchDegraded {
                consecutive_failed_minutes: 3,
                minute_ist: "10:15".to_string(),
            },
            NotificationEvent::GrowwSpot1mFetchRecovered {
                minute_ist: "10:18".to_string(),
                failed_minutes: 3,
            },
            NotificationEvent::GrowwChain1mFetchDegraded {
                consecutive_failed_minutes: 3,
                minute_ist: "10:15".to_string(),
            },
            NotificationEvent::GrowwChain1mFetchRecovered {
                minute_ist: "10:18".to_string(),
                failed_minutes: 3,
            },
            NotificationEvent::GrowwChain1mExpiryUnresolved {
                detail: "no usable expiry".to_string(),
            },
            NotificationEvent::GrowwChain1mProbeVerdict {
                ok: true,
                detail: "3 chains".to_string(),
            },
            NotificationEvent::GrowwContract1mFetchDegraded {
                consecutive_failed_minutes: 3,
                minute_ist: "10:15".to_string(),
            },
            NotificationEvent::GrowwContract1mFetchRecovered {
                minute_ist: "10:18".to_string(),
                failed_minutes: 3,
            },
            NotificationEvent::GrowwContract1mBookUnresolved {
                detail: "no usable contracts".to_string(),
            },
        ];
        for ev in events {
            assert_eq!(ev.feed_badge(), Some("🟢 GROWW"), "event: {}", ev.topic());
            let msg = ev.to_message();
            assert!(
                msg.starts_with("🟢 GROWW — "),
                "Groww REST-leg body must lead with the Groww badge: {msg}"
            );
        }
    }

    #[test]
    fn test_dhan_scoped_gate_and_order_events_carry_dhan_badge() {
        // The market-open milestones (they count the Dhan pool), the daily
        // candle cross-checks (Dhan REST vs our candles), the static-IP /
        // IP-verify gates, the dual-instance lock, and the order-path
        // events are all Dhan-scoped — badge ratchet (2026-07-14).
        let events = [
            NotificationEvent::MarketOpenStreamingConfirmation {
                main_feed_active: 1,
                main_feed_total: 1,
                order_update_active: true,
            },
            NotificationEvent::MarketOpenStreamingFailed {
                main_feed_active: 0,
                main_feed_total: 1,
                order_update_active: false,
            },
            NotificationEvent::MarketOpenReadinessConfirmation {
                main_feed_active: 1,
                main_feed_total: 1,
                order_update_active: true,
                token_remaining_secs: 8 * 3600,
            },
            NotificationEvent::CrossVerify1mSummary {
                trading_date_ist: "2026-07-14".to_string(),
                instruments: 4,
                compared: 1500,
                mismatches: 0,
                missing: 0,
                degraded: false,
            },
            NotificationEvent::CrossVerify1mAborted {
                detail: "task died".to_string(),
            },
            NotificationEvent::BarMismatchCrossCheckFailed {
                reason: "all fetches errored".to_string(),
            },
            NotificationEvent::BarMismatchCorrectedFromHistorical {
                compared_count: 222,
                mismatches_count: 3,
                sample_symbols: vec!["NIFTY".to_string()],
                cross_check_pass: "corrected",
            },
            NotificationEvent::BarMismatchCrossCheckInconclusive {
                compared_count: 150,
                expected_count: 222,
            },
            NotificationEvent::IpVerificationFailed {
                reason: "mismatch".to_string(),
            },
            NotificationEvent::IpVerificationSuccess {
                verified_ip: "1.2.3.4".to_string(),
            },
            NotificationEvent::StaticIpBootCheckPassed {
                ip_flag: "PRIMARY".to_string(),
            },
            NotificationEvent::StaticIpBootCheckFailed {
                reason: "ordersAllowed false".to_string(),
                orders_allowed: false,
                ip_match_status: "MISMATCH".to_string(),
                attempts_made: 30,
            },
            NotificationEvent::StaticIpBootCheckRetrying {
                attempt: 2,
                max_attempts: 30,
            },
            NotificationEvent::DualInstanceDetected {
                holder: "local:123:abc".to_string(),
                lock_key: "instance-lock".to_string(),
            },
            NotificationEvent::OrderRejected {
                correlation_id: "abc".to_string(),
                reason: "rejected".to_string(),
            },
            NotificationEvent::CircuitBreakerOpened {
                consecutive_failures: 3,
            },
            NotificationEvent::CircuitBreakerClosed,
            NotificationEvent::RateLimitExhausted {
                limit_type: "per_second".to_string(),
            },
            NotificationEvent::OrphanPositionDetected {
                count: 1,
                total_abs_net_qty: 50,
                sample_symbols: vec!["NIFTY-Jun2026-28500-CE".to_string()],
                dry_run: true,
            },
            NotificationEvent::OrphanPositionsClean,
        ];
        for ev in events {
            assert_eq!(ev.feed_badge(), Some("🔷 DHAN"), "event: {}", ev.topic());
            assert!(
                ev.to_message().starts_with("🔷 DHAN — "),
                "Dhan-scoped body must lead with the Dhan badge: {}",
                ev.to_message()
            );
        }
    }

    #[test]
    fn test_sleep_events_badge_follows_feed_field_with_dhan_fallback() {
        // The WS sleep/wake events carry a `feed` field whose live values
        // are "main"/"order_update" (both Dhan WebSocket types) — those
        // fall back to the Dhan badge; a future "groww" value badges
        // Groww. Never un-badged, never a static lie (2026-07-14).
        let main = NotificationEvent::WebSocketSleepEntered {
            feed: "main".to_string(),
            connection_index: 0,
            sleep_secs: 3600,
        };
        assert_eq!(main.feed_badge(), Some("🔷 DHAN"));
        let order_update = NotificationEvent::WebSocketSleepResumed {
            feed: "order_update".to_string(),
            connection_index: 0,
            slept_for_secs: 3600,
        };
        assert_eq!(order_update.feed_badge(), Some("🔷 DHAN"));
        let groww = NotificationEvent::WebSocketTokenForceRenewedOnWake {
            feed: "groww".to_string(),
            connection_index: 0,
            remaining_secs_before: 100,
            threshold_secs: 14400,
        };
        assert_eq!(groww.feed_badge(), Some("🟢 GROWW"));
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
        // 2026-07-14 broker tag: the Dhan badge leads the body; the
        // severity emoji stays first WITHIN the summary line.
        assert!(
            msg.starts_with("🔷 DHAN — \u{2705}"),
            "clean summary leads with the Dhan badge then ✅: {msg}"
        );
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
        assert!(
            msg.starts_with("🔷 DHAN — \u{26a0}"),
            "failure summary leads with the Dhan badge then ⚠️: {msg}"
        );
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
    // Spot1mFetchDegraded + Spot1mFetchRecovered (2026-07-12 — per-minute
    // spot 1m REST pipeline PR-2)
    // -----------------------------------------------------------------------

    #[test]
    fn test_spot_1m_fetch_degraded_is_high_with_action_lines() {
        let event = NotificationEvent::Spot1mFetchDegraded {
            consecutive_failed_minutes: 3,
            minute_ist: "10:42 AM".to_string(),
        };
        assert_eq!(event.topic(), "Spot1mFetchDegraded");
        assert_eq!(event.severity(), Severity::High);
        let msg = event.to_message();
        assert!(msg.contains("FAILING"), "got: {msg}");
        assert!(msg.contains("3 minutes in a row"), "got: {msg}");
        // IST 12-hour timestamp (Telegram commandment 9).
        assert!(msg.contains("10:42 AM IST"), "got: {msg}");
        assert!(msg.contains("What to do RIGHT NOW"), "got: {msg}");
        // Honest scope line: the live WS pipeline is untouched.
        assert!(msg.contains("NOT affected"), "got: {msg}");
    }

    #[test]
    fn test_spot_1m_fetch_recovered_is_info_positive_ping() {
        let event = NotificationEvent::Spot1mFetchRecovered {
            minute_ist: "10:45 AM".to_string(),
            failed_minutes: 4,
        };
        assert_eq!(event.topic(), "Spot1mFetchRecovered");
        assert_eq!(event.severity(), Severity::Info);
        let msg = event.to_message();
        assert!(msg.contains("recovered"), "got: {msg}");
        assert!(msg.contains("10:45 AM IST"), "got: {msg}");
        assert!(msg.contains("4 failed"), "got: {msg}");
        // No false-OK: recovery never claims the missing minutes came back.
        assert!(msg.contains("nothing is made up"), "got: {msg}");
    }

    // -----------------------------------------------------------------------
    // GrowwSpot1mFetchDegraded + GrowwSpot1mFetchRecovered (2026-07-13 —
    // Groww per-minute spot 1m REST leg, PR-2 of the Groww REST plan)
    // -----------------------------------------------------------------------

    #[test]
    fn test_groww_spot_1m_fetch_degraded_is_high_with_action_lines() {
        let event = NotificationEvent::GrowwSpot1mFetchDegraded {
            consecutive_failed_minutes: 3,
            minute_ist: "10:42 AM".to_string(),
        };
        assert_eq!(event.topic(), "GrowwSpot1mFetchDegraded");
        assert_eq!(event.severity(), Severity::High);
        let msg = event.to_message();
        assert!(msg.contains("Groww"), "got: {msg}");
        assert!(msg.contains("FAILING"), "got: {msg}");
        assert!(msg.contains("3 minutes in a row"), "got: {msg}");
        // IST 12-hour timestamp (Telegram commandment 9).
        assert!(msg.contains("10:42 AM IST"), "got: {msg}");
        assert!(msg.contains("What to do RIGHT NOW"), "got: {msg}");
        // Honest scope line: the live WS pipelines are untouched.
        assert!(msg.contains("NOT affected"), "got: {msg}");
    }

    #[test]
    fn test_groww_spot_1m_fetch_recovered_is_info_positive_ping() {
        let event = NotificationEvent::GrowwSpot1mFetchRecovered {
            minute_ist: "10:45 AM".to_string(),
            failed_minutes: 4,
        };
        assert_eq!(event.topic(), "GrowwSpot1mFetchRecovered");
        assert_eq!(event.severity(), Severity::Info);
        let msg = event.to_message();
        assert!(msg.contains("Groww"), "got: {msg}");
        assert!(msg.contains("recovered"), "got: {msg}");
        assert!(msg.contains("10:45 AM IST"), "got: {msg}");
        assert!(msg.contains("4 failed"), "got: {msg}");
        // No false-OK: recovery never claims the missing minutes came back.
        assert!(msg.contains("nothing is made up"), "got: {msg}");
    }

    // -----------------------------------------------------------------------
    // Spot1mSidNotServed + Spot1mSidServedRecovered (operator scope addition
    // 2026-07-13, relayed via the coordinator session — the INDIA VIX
    // live-probe companion)
    // -----------------------------------------------------------------------

    #[test]
    fn test_spot_1m_sid_not_served_is_high_names_the_index_and_scopes_honestly() {
        let event = NotificationEvent::Spot1mSidNotServed {
            symbol: "INDIA VIX".to_string(),
            consecutive_minutes: 10,
        };
        assert_eq!(event.topic(), "Spot1mSidNotServed");
        assert_eq!(event.severity(), Severity::High);
        let msg = event.to_message();
        // The operator-mandated plain-English core wording.
        assert!(
            msg.contains("not returning 1-minute candles for INDIA VIX"),
            "got: {msg}"
        );
        assert!(msg.contains("other indices are unaffected"), "got: {msg}");
        assert!(msg.contains("10 minutes in a row"), "got: {msg}");
        assert!(msg.contains("What to do RIGHT NOW"), "got: {msg}");
        // Honest scope line: the live WS pipeline is untouched.
        assert!(msg.contains("NOT affected"), "got: {msg}");
    }

    // -----------------------------------------------------------------------
    // GrowwChain1m* events (2026-07-13 — Groww per-minute option-chain
    // leg, PR-3 of the Groww REST plan)
    // -----------------------------------------------------------------------

    #[test]
    fn test_groww_contract_1m_fetch_degraded_is_high_with_action_lines() {
        let event = NotificationEvent::GrowwContract1mFetchDegraded {
            consecutive_failed_minutes: 3,
            minute_ist: "10:42 AM".to_string(),
        };
        assert_eq!(event.topic(), "GrowwContract1mFetchDegraded");
        assert_eq!(event.severity(), Severity::High);
        let msg = event.to_message();
        assert!(msg.contains("Groww"), "got: {msg}");
        assert!(msg.contains("FAILING"), "got: {msg}");
        assert!(msg.contains("3 minutes in a row"), "got: {msg}");
        // IST 12-hour timestamp (Telegram commandment 9).
        assert!(msg.contains("10:42 AM IST"), "got: {msg}");
        assert!(msg.contains("What to do RIGHT NOW"), "got: {msg}");
        // Honest scope line: the live WS pipelines are untouched.
        assert!(msg.contains("NOT affected"), "got: {msg}");
    }

    #[test]
    fn test_groww_contract_1m_fetch_recovered_is_info_positive_ping() {
        let event = NotificationEvent::GrowwContract1mFetchRecovered {
            minute_ist: "10:45 AM".to_string(),
            failed_minutes: 4,
        };
        assert_eq!(event.topic(), "GrowwContract1mFetchRecovered");
        assert_eq!(event.severity(), Severity::Info);
        let msg = event.to_message();
        assert!(msg.contains("Groww"), "got: {msg}");
        assert!(msg.contains("recovered"), "got: {msg}");
        assert!(msg.contains("10:45 AM IST"), "got: {msg}");
        assert!(msg.contains("4 failed"), "got: {msg}");
        // No false-OK: recovery never claims the missing minutes came back.
        assert!(msg.contains("nothing is made up"), "got: {msg}");
    }

    #[test]
    fn test_groww_contract_1m_book_unresolved_is_high_and_escapes_detail() {
        let event = NotificationEvent::GrowwContract1mBookUnresolved {
            detail: "SENSEX: no usable contracts <script>".to_string(),
        };
        assert_eq!(event.topic(), "GrowwContract1mBookUnresolved");
        assert_eq!(event.severity(), Severity::High);
        let msg = event.to_message();
        assert!(msg.contains("could NOT"), "got: {msg}");
        assert!(msg.contains("never guessed"), "got: {msg}");
        // Hostile detail is HTML-escaped, never raw.
        assert!(!msg.contains("<script>"), "got: {msg}");
        assert!(msg.contains("&lt;script&gt;"), "got: {msg}");
    }

    #[test]
    fn test_groww_chain_1m_fetch_degraded_is_high_with_action_lines() {
        let event = NotificationEvent::GrowwChain1mFetchDegraded {
            consecutive_failed_minutes: 3,
            minute_ist: "10:42 AM".to_string(),
        };
        assert_eq!(event.topic(), "GrowwChain1mFetchDegraded");
        assert_eq!(event.severity(), Severity::High);
        let msg = event.to_message();
        assert!(msg.contains("Groww"), "got: {msg}");
        assert!(msg.contains("FAILING"), "got: {msg}");
        assert!(msg.contains("3 minutes in a row"), "got: {msg}");
        // IST 12-hour timestamp (Telegram commandment 9).
        assert!(msg.contains("10:42 AM IST"), "got: {msg}");
        assert!(msg.contains("What to do RIGHT NOW"), "got: {msg}");
        // Honest scope line: the live WS pipelines are untouched.
        assert!(msg.contains("NOT affected"), "got: {msg}");
    }

    #[test]
    fn test_spot_1m_sid_served_recovered_is_info_positive_ping() {
        let event = NotificationEvent::Spot1mSidServedRecovered {
            symbol: "INDIA VIX".to_string(),
            not_served_minutes: 12,
        };
        assert_eq!(event.topic(), "Spot1mSidServedRecovered");
        assert_eq!(event.severity(), Severity::Info);
        let msg = event.to_message();
        assert!(
            msg.contains("serving 1-minute candles for INDIA VIX again"),
            "got: {msg}"
        );
        assert!(msg.contains("12 missed"), "got: {msg}");
        // No false-OK: recovery never claims the missing minutes came back.
        assert!(msg.contains("nothing is made up"), "got: {msg}");
    }

    #[test]
    fn test_groww_chain_1m_fetch_recovered_is_info_positive_ping() {
        let event = NotificationEvent::GrowwChain1mFetchRecovered {
            minute_ist: "10:45 AM".to_string(),
            failed_minutes: 4,
        };
        assert_eq!(event.topic(), "GrowwChain1mFetchRecovered");
        assert_eq!(event.severity(), Severity::Info);
        let msg = event.to_message();
        assert!(msg.contains("Groww"), "got: {msg}");
        assert!(msg.contains("recovered"), "got: {msg}");
        assert!(msg.contains("10:45 AM IST"), "got: {msg}");
        assert!(msg.contains("4 failed"), "got: {msg}");
        // No false-OK: recovery never claims the missing minutes came back.
        assert!(msg.contains("nothing is made up"), "got: {msg}");
    }

    #[test]
    fn test_groww_chain_1m_underlying_not_served_is_high_names_the_underlying() {
        let event = NotificationEvent::GrowwChain1mUnderlyingNotServed {
            underlying: "NIFTY",
            empty_minutes: 10,
        };
        assert_eq!(event.topic(), "GrowwChain1mUnderlyingNotServed");
        assert_eq!(event.severity(), Severity::High);
        let msg = event.to_message();
        // The operator-mandated plain-English core wording.
        assert!(
            msg.contains("not returning the option chain for NIFTY"),
            "got: {msg}"
        );
        assert!(msg.contains("other indices are unaffected"), "got: {msg}");
        assert!(msg.contains("10 minutes in a row"), "got: {msg}");
        assert!(msg.contains("What to do RIGHT NOW"), "got: {msg}");
        // Honest scope lines: the live WS pipeline is untouched, and the
        // Dhan-side availability is stated as MAY (no false-OK — Dhan's
        // chain leg has its own independent state).
        assert!(msg.contains("NOT affected"), "got: {msg}");
        assert!(
            msg.contains("may still be available from the Dhan side"),
            "got: {msg}"
        );
    }

    #[test]
    fn test_groww_chain_1m_underlying_served_recovered_is_info_positive_ping() {
        let event = NotificationEvent::GrowwChain1mUnderlyingServedRecovered {
            underlying: "NIFTY",
            empty_minutes: 12,
        };
        assert_eq!(event.topic(), "GrowwChain1mUnderlyingServedRecovered");
        assert_eq!(event.severity(), Severity::Info);
        let msg = event.to_message();
        assert!(
            msg.contains("serving the option chain for NIFTY again"),
            "got: {msg}"
        );
        assert!(msg.contains("12 empty"), "got: {msg}");
        // No false-OK: recovery never claims the missing minutes came back.
        assert!(msg.contains("nothing is made up"), "got: {msg}");
    }

    #[test]
    fn test_groww_chain_1m_expiry_unresolved_is_high_and_escapes_detail() {
        let event = NotificationEvent::GrowwChain1mExpiryUnresolved {
            detail: "SENSEX: no usable option rows <script>".to_string(),
        };
        assert_eq!(event.topic(), "GrowwChain1mExpiryUnresolved");
        assert_eq!(event.severity(), Severity::High);
        let msg = event.to_message();
        assert!(msg.contains("could NOT start"), "got: {msg}");
        assert!(msg.contains("never"), "expiry never guessed: {msg}");
        // Hostile detail is HTML-escaped, never raw.
        assert!(!msg.contains("<script>"), "got: {msg}");
        assert!(msg.contains("&lt;script&gt;"), "got: {msg}");
        assert!(msg.contains("What to do RIGHT NOW"), "got: {msg}");
    }

    #[test]
    fn test_groww_chain_1m_probe_verdict_is_info_both_ways() {
        let passed = NotificationEvent::GrowwChain1mProbeVerdict {
            ok: true,
            detail: "NIFTY: 102 strikes in 0.8s".to_string(),
        };
        assert_eq!(passed.topic(), "GrowwChain1mProbeVerdict");
        assert_eq!(passed.severity(), Severity::Info);
        let msg = passed.to_message();
        assert!(msg.contains("PASSED"), "got: {msg}");
        assert!(msg.contains("102 strikes"), "got: {msg}");
        assert!(msg.contains("switched OFF"), "honest state: {msg}");

        let failed = NotificationEvent::GrowwChain1mProbeVerdict {
            ok: false,
            detail: "http 403 <b>hostile</b>".to_string(),
        };
        assert_eq!(failed.severity(), Severity::Info);
        let msg = failed.to_message();
        assert!(msg.contains("did NOT pass"), "got: {msg}");
        assert!(!msg.contains("<b>hostile</b>"), "escaped: {msg}");
        assert!(msg.contains("Nothing is broken"), "got: {msg}");
    }

    // -----------------------------------------------------------------------
    // ChainFetchDegraded / ChainFetchRecovered / ChainEntitlementAbsent /
    // ChainEntitlementConfirmed (2026-07-12 — per-minute option-chain REST
    // pipeline PR-3)
    // -----------------------------------------------------------------------

    #[test]
    fn test_chain_fetch_degraded_is_high_with_action_lines() {
        let event = NotificationEvent::ChainFetchDegraded {
            consecutive_failed_minutes: 3,
            minute_ist: "10:42 AM".to_string(),
        };
        assert_eq!(event.topic(), "ChainFetchDegraded");
        assert_eq!(event.severity(), Severity::High);
        let msg = event.to_message();
        assert!(msg.contains("FAILING"), "got: {msg}");
        assert!(msg.contains("3 minutes"), "got: {msg}");
        // IST 12-hour timestamp (Telegram commandment 9).
        assert!(msg.contains("10:42 AM IST"), "got: {msg}");
        assert!(msg.contains("What to do RIGHT NOW"), "got: {msg}");
        // Honest scope line: the live WS pipeline is untouched.
        assert!(msg.contains("NOT affected"), "got: {msg}");
    }

    #[test]
    fn test_chain_fetch_recovered_is_info_positive_ping() {
        let event = NotificationEvent::ChainFetchRecovered {
            minute_ist: "10:45 AM".to_string(),
            failed_minutes: 4,
        };
        assert_eq!(event.topic(), "ChainFetchRecovered");
        assert_eq!(event.severity(), Severity::Info);
        let msg = event.to_message();
        assert!(msg.contains("recovered"), "got: {msg}");
        assert!(msg.contains("10:45 AM IST"), "got: {msg}");
        assert!(msg.contains("4 failed"), "got: {msg}");
        // No false-OK: recovery never claims the missing minutes came back.
        assert!(msg.contains("nothing is made up"), "got: {msg}");
    }

    /// The entitlement-absent verdict is HIGH (actionable) when the
    /// pipeline was switched ON, but only an Info heads-up for the
    /// probe-only path of a disabled pipeline — never a page for a report
    /// the operator asked for.
    #[test]
    fn test_chain_entitlement_absent_severity_splits_on_intent() {
        let paged = NotificationEvent::ChainEntitlementAbsent {
            pipeline_enabled: true,
            detail: "DH-902 access not subscribed".to_string(),
        };
        assert_eq!(paged.topic(), "ChainEntitlementAbsent");
        assert_eq!(paged.severity(), Severity::High);
        let msg = paged.to_message();
        assert!(msg.contains("CANNOT run"), "got: {msg}");
        assert!(msg.contains("DH-902"), "got: {msg}");
        assert!(msg.contains("What to do RIGHT NOW"), "got: {msg}");
        assert!(msg.contains("NOT affected"), "got: {msg}");

        let probe = NotificationEvent::ChainEntitlementAbsent {
            pipeline_enabled: false,
            detail: "DH-902 access not subscribed".to_string(),
        };
        assert_eq!(probe.severity(), Severity::Info);
        let msg = probe.to_message();
        assert!(msg.contains("NOT available"), "got: {msg}");
        assert!(msg.contains("Nothing is broken"), "got: {msg}");
        // Payload detail is HTML-escaped like every String arm.
        let hostile = NotificationEvent::ChainEntitlementAbsent {
            pipeline_enabled: false,
            detail: "<script>x</script>".to_string(),
        };
        assert!(!hostile.to_message().contains("<script>"));
    }

    #[test]
    fn test_chain_expirylist_failed_is_high_with_action_lines() {
        let event = NotificationEvent::ChainExpirylistFailed {
            detail: "http 500 <i>x</i>".to_string(),
        };
        assert_eq!(event.topic(), "ChainExpirylistFailed");
        assert_eq!(event.severity(), Severity::High);
        let msg = event.to_message();
        assert!(msg.contains("could NOT start today"), "got: {msg}");
        // Honest: never a guessed expiry; live prices untouched.
        assert!(msg.contains("never guessed"), "got: {msg}");
        assert!(msg.contains("NOT affected"), "got: {msg}");
        assert!(msg.contains("What to do RIGHT NOW"), "got: {msg}");
        // Payload detail is HTML-escaped like every String arm.
        assert!(!msg.contains("<i>"), "got: {msg}");
    }

    #[test]
    fn test_chain_entitlement_confirmed_is_info_with_flip_instruction() {
        let event = NotificationEvent::ChainEntitlementConfirmed;
        assert_eq!(event.topic(), "ChainEntitlementConfirmed");
        assert_eq!(event.severity(), Severity::Info);
        let msg = event.to_message();
        assert!(msg.contains("IS available"), "got: {msg}");
        // Plain-English action (10 commandments — no config-key jargon in
        // the Telegram body; the exact key lives in the probe's log line).
        assert!(msg.contains("turn ON the option"), "got: {msg}");
        assert!(!msg.contains("option_chain_1m"), "got: {msg}");
        // Honest: recording is NOT running yet.
        assert!(msg.contains("switched OFF"), "got: {msg}");
    }

    // -----------------------------------------------------------------------
    // DualFeedDailyScorecard + DualFeedScorecardAborted (2026-07-10 PR-A)
    // -----------------------------------------------------------------------

    fn score_line(name: &str) -> FeedScoreLine {
        FeedScoreLine {
            name: name.to_string(),
            ticks: 1_842_551,
            exclusive_minutes: 14,
            lag_p50_ms: -1,
            lag_p99_ms: -1,
            drops_market: 3,
            blame_broker: 2,
            blame_ours: 0,
            blame_unclear: 1,
            stalls: 0,
            restarts: 0,
            streaming_minutes: 373,
        }
    }

    fn scorecard(dhan: FeedScoreLine, groww: FeedScoreLine) -> NotificationEvent {
        NotificationEvent::DualFeedDailyScorecard {
            trading_date_ist: "2026-07-10".to_string(),
            dhan,
            groww,
            session_minutes: 375,
            partial_coverage: false,
            degraded: false,
            early_run: false,
            restart_partial: false,
            dhan_feed_off: false,
            groww_feed_off: false,
            rest_legs: vec![],
            rest_legs_read_failed: false,
        }
    }

    fn scorecard_with_rest(rest_legs: Vec<RestLegScoreLine>, read_failed: bool) -> String {
        NotificationEvent::DualFeedDailyScorecard {
            trading_date_ist: "2026-07-13".to_string(),
            dhan: score_line("Dhan"),
            groww: score_line("Groww"),
            session_minutes: 375,
            partial_coverage: false,
            degraded: false,
            early_run: false,
            restart_partial: false,
            dhan_feed_off: false,
            groww_feed_off: false,
            rest_legs,
            rest_legs_read_failed: read_failed,
        }
        .to_message()
    }

    fn rest_line(feed: &str, leg: &str) -> RestLegScoreLine {
        RestLegScoreLine {
            feed: feed.to_string(),
            leg: leg.to_string(),
            ok_fetches: -1,
            failed_fetches: -1,
            named_gaps: -1,
            rate_limited_hits: -1,
            late_recovered: -1,
            close_p50_ms: -1,
            close_p99_ms: -1,
            close_max_ms: -1,
            close_samples: -1,
        }
    }

    /// PR-5 (operator Quote 2): the measured digest line carries pull
    /// counts + the seconds-after-close distribution + the extras, all in
    /// plain English.
    #[test]
    fn test_rest_leg_digest_line_measured_full() {
        let msg = scorecard_with_rest(
            vec![RestLegScoreLine {
                ok_fetches: 1_496,
                failed_fetches: 4,
                named_gaps: 1,
                rate_limited_hits: 3,
                late_recovered: 2,
                close_p50_ms: 1_400,
                close_p99_ms: 3_200,
                close_max_ms: 6_200,
                close_samples: 1_494,
                ..rest_line("Groww", "spot candles")
            }],
            false,
        );
        assert!(
            msg.contains("Official minute candles — how fast after each"),
            "digest section header missing: {msg}"
        );
        assert!(
            msg.contains(
                "Groww spot candles: 1496 pulls OK, 4 failed — typical 1.4 s / \
                 worst 1% 3.2 s / slowest 6.2 s after close; 2 recovered late; \
                 3 rate-limit hits; 1 never recovered \u{26a0}\u{fe0f}"
            ),
            "measured digest line wrong: {msg}"
        );
    }

    /// A latency-only line (the Dhan spot fallback: freshness measured
    /// from the stored candles while per-pull records are not written yet)
    /// says so — and never fabricates pull counts.
    #[test]
    fn test_rest_leg_digest_line_latency_only_and_placeholder() {
        let msg = scorecard_with_rest(
            vec![
                RestLegScoreLine {
                    close_p50_ms: 1_900,
                    close_p99_ms: 5_000,
                    close_max_ms: 61_000,
                    close_samples: 1_480,
                    late_recovered: 1,
                    ..rest_line("Dhan", "spot candles")
                },
                rest_line("Dhan", "option chain"),
            ],
            false,
        );
        assert!(
            msg.contains(
                "Dhan spot candles: typical 1.9 s / worst 1% 5.0 s / slowest \
                 61.0 s after close (pull counts not recorded yet); 1 recovered late"
            ),
            "latency-only digest line wrong: {msg}"
        );
        // The all-sentinel placeholder is the honest Quote-2 answer for a
        // family with no measurement source yet — never a fabricated zero.
        assert!(
            msg.contains("Dhan option chain: not measured yet"),
            "placeholder digest line wrong: {msg}"
        );
        // The -1 sentinels must never render numerically on a digest line
        // (a bare `-1` substring check would trip on the ISO date).
        for leaked in ["-1 pulls", "typical -1", "worst 1% -1", "slowest -1"] {
            assert!(
                !msg.contains(leaked),
                "sentinels must never render numerically ({leaked:?}): {msg}"
            );
        }
    }

    /// An all-pulls-failed day renders a measured-zero, not a fabricated
    /// freshness ("0 pulls OK" + "not measurable"); and the read-failed
    /// flag carries its own honest footnote.
    #[test]
    fn test_rest_leg_digest_zero_ok_day_and_read_failed_footnote() {
        let msg = scorecard_with_rest(
            vec![RestLegScoreLine {
                ok_fetches: 0,
                failed_fetches: 375,
                named_gaps: 375,
                rate_limited_hits: 0,
                late_recovered: 0,
                close_p50_ms: -1,
                close_p99_ms: -1,
                close_max_ms: -1,
                close_samples: 0,
                ..rest_line("Groww", "option chain")
            }],
            true,
        );
        assert!(
            msg.contains(
                "Groww option chain: 0 pulls OK, 375 failed — freshness not \
                 measurable (no successful pull); 375 never recovered \u{26a0}\u{fe0f}"
            ),
            "zero-ok digest line wrong: {msg}"
        );
        assert!(
            msg.contains("minute-candle pull records could \n                         not be read")
                || msg.contains("minute-candle pull records could not be read"),
            "read-failed footnote missing: {msg}"
        );
    }

    /// An EMPTY digest vec omits the section entirely (older callers /
    /// pre-deploy cards) — no header, no stale claim.
    #[test]
    fn test_rest_leg_digest_empty_vec_omits_section() {
        let msg = scorecard(score_line("Dhan"), score_line("Groww")).to_message();
        assert!(
            !msg.contains("Official minute candles"),
            "empty rest_legs must omit the digest section: {msg}"
        );
    }

    /// The digest lines obey the Telegram commandments: plain English, no
    /// wire slugs, no library/infrastructure names, no file paths.
    #[test]
    fn test_rest_leg_digest_obeys_telegram_commandments() {
        let msg = scorecard_with_rest(
            vec![
                RestLegScoreLine {
                    ok_fetches: 1_120,
                    failed_fetches: 5,
                    named_gaps: 0,
                    rate_limited_hits: 0,
                    late_recovered: 0,
                    close_p50_ms: 2_100,
                    close_p99_ms: 4_000,
                    close_max_ms: 9_000,
                    close_samples: 1_120,
                    ..rest_line("Groww", "option chain")
                },
                rest_line("Dhan", "option chain"),
            ],
            false,
        );
        for banned in [
            "spot_1m",
            "chain_1m",
            "contract_1m",
            "rest_fetch_audit",
            "p50",
            "p99",
            ".rs",
            "SQL",
        ] {
            assert!(
                !msg.contains(banned),
                "digest must not carry {banned:?}: {msg}"
            );
        }
        assert!(
            msg.contains("Groww option chain: 1120 pulls OK, 5 failed"),
            "{msg}"
        );
        // Zero-valued extras stay OFF the line (one line = one answer).
        assert!(!msg.contains("0 recovered late"), "{msg}");
        assert!(!msg.contains("0 rate-limit hits"), "{msg}");
        assert!(!msg.contains("0 never recovered"), "{msg}");
    }

    #[test]
    fn test_dual_feed_scorecard_topic_severity_policy() {
        let ev = scorecard(score_line("Dhan"), score_line("Groww"));
        assert_eq!(ev.topic(), "DualFeedDailyScorecard");
        assert_eq!(ev.severity(), Severity::Info);
        // The once-per-day 15:45 digest must arrive AT 15:45 — post-close is
        // off-hours, so the default Info routing would coalesce it
        // (CrossVerify1mSummary precedent).
        assert_eq!(ev.dispatch_policy(), DispatchPolicy::Immediate);
    }

    #[test]
    fn test_dual_feed_scorecard_body_verdict_ladder() {
        // Rung 1: exclusive minutes decide.
        let mut groww = score_line("Groww");
        groww.exclusive_minutes = 63;
        let msg = scorecard(score_line("Dhan"), groww).to_message();
        assert!(
            msg.contains("\u{1f3c6} Verdict: Groww won today — 63 exclusive minutes vs 14."),
            "rung-1 verdict wrong: {msg}"
        );
        // Rung 2: tied exclusive minutes → measured worst-1% delay decides
        // (delta 2160 ms > the 1000 ms clock floor).
        let mut d = score_line("Dhan");
        let mut g = score_line("Groww");
        d.lag_p99_ms = 2900;
        g.lag_p99_ms = 740;
        let msg = scorecard(d, g).to_message();
        assert!(
            msg.contains("Verdict: Groww won today — faster prices beyond the clock floor"),
            "rung-2 verdict wrong: {msg}"
        );
        // Rung-2 clock-floor guard (PR-C review round 1, 2026-07-11): a
        // sub-floor delta is Dhan's whole-second quantization, not speed —
        // 1400 vs 700 (delta 700 ≤ 1000) must NOT declare a lag winner;
        // identical evidence elsewhere falls through to "Even day".
        let mut d = score_line("Dhan");
        let mut g = score_line("Groww");
        d.lag_p99_ms = 1400;
        g.lag_p99_ms = 700;
        let msg = scorecard(d, g).to_message();
        assert!(
            !msg.contains("faster prices"),
            "a sub-floor p99 delta must never decide the delay rung: {msg}"
        );
        assert!(
            msg.contains("\u{1f91d} Verdict: Even day."),
            "sub-floor delta falls through the ladder: {msg}"
        );
        // ... while a beyond-floor delta (5000 vs 700 = 4300 > 1000)
        // decides.
        let mut d = score_line("Dhan");
        let mut g = score_line("Groww");
        d.lag_p99_ms = 5000;
        g.lag_p99_ms = 700;
        let msg = scorecard(d, g).to_message();
        assert!(
            msg.contains("Verdict: Groww won today — faster prices beyond the clock floor"),
            "a beyond-floor delta must decide rung 2: {msg}"
        );
        // The floor const must stay lockstep with the persisted Dhan
        // lag_floor_ms value (LAG_FLOOR_MS_DHAN = 1000 in
        // tickvault-storage — core cannot import it; both pin 1000).
        assert_eq!(VERDICT_LAG_CLOCK_FLOOR_MS, 1000);
        // Rung 2 skip: a −1 sentinel must never decide the delay rung.
        let mut d = score_line("Dhan");
        let g = score_line("Groww"); // lag -1 both sides
        d.blame_broker = 5;
        let msg = scorecard(d, g).to_message();
        assert!(
            msg.contains("Verdict: Groww won today — fewer broker-caused incidents (2 vs 5)."),
            "rung-3 verdict wrong: {msg}"
        );
        // Rung 3 skip (hostile review 2026-07-10): a −1 blame sentinel must
        // never win the drops rung nor render "-1" — falls to Even day.
        let mut d = score_line("Dhan");
        let g = score_line("Groww");
        d.blame_broker = -1; // drops_market still >= 0
        let msg = scorecard(d, g).to_message();
        assert!(
            msg.contains("\u{1f91d} Verdict: Even day."),
            "a -1 blame sentinel must not decide rung 3: {msg}"
        );
        assert!(
            !msg.contains("(-1 vs"),
            "the -1 sentinel must never render in a verdict: {msg}"
        );
        // Rung 4: identical evidence → even day.
        let msg = scorecard(score_line("Dhan"), score_line("Groww")).to_message();
        assert!(msg.contains("\u{1f91d} Verdict: Even day."), "{msg}");
    }

    #[test]
    fn test_dual_feed_scorecard_body_sentinels_and_footnotes() {
        // −1 lag renders honestly, never a fabricated 0 (audit Rule 11).
        let msg = scorecard(score_line("Dhan"), score_line("Groww")).to_message();
        assert!(msg.contains("not measured yet"), "{msg}");
        // Scoreboard PR-C: the retired PR-1 "next upgrade" claim must not
        // render — the unmeasured arm now names the honest cause (backfill
        // re-run / too few samples).
        assert!(
            msg.contains("Delay could not be measured today"),
            "unmeasured-lag footnote missing: {msg}"
        );
        assert!(
            !msg.contains("Delay measurement starts with the next upgrade"),
            "the retired PR-1 delay footnote must not render: {msg}"
        );
        // MEASURED lag swaps in the resolution-asymmetry footnote (Dhan
        // whole-second floor + Groww millisecond one-step-after-the-wire)
        // and renders both feeds at their native precision.
        let mut d = score_line("Dhan");
        let mut g = score_line("Groww");
        d.lag_p50_ms = 1200;
        d.lag_p99_ms = 2900;
        g.lag_p50_ms = 180;
        g.lag_p99_ms = 740;
        let msg = scorecard(d, g).to_message();
        assert!(
            msg.contains("Dhan's price clock ticks in whole seconds"),
            "lag-floor footnote missing: {msg}"
        );
        assert!(
            msg.contains("Groww's delay is millisecond-precise"),
            "Groww receipt-clock semantics footnote missing: {msg}"
        );
        assert!(msg.contains("1.2 s"), "≥1s delays render in seconds: {msg}");
        assert!(
            msg.contains("180 ms"),
            "sub-second Groww delays render in milliseconds: {msg}"
        );
        // Scoreboard PR-B (2026-07-10): stalls are MEASURED — a real count
        // renders numerically and the retired PR-1 footnote never appears
        // (0 = measured 0 from this deploy forward; the runbook keeps the
        // pre-ship-day caveat).
        let mut g = score_line("Groww");
        g.stalls = 1;
        let msg = scorecard(score_line("Dhan"), g).to_message();
        assert!(msg.contains("Stalls: Dhan 0 | Groww 1"), "{msg}");
        assert!(
            !msg.contains("Stall tracking starts with the next upgrade"),
            "the retired PR-1 stall footnote must not render: {msg}"
        );
        // A `-1` still renders as the defensive "?" — without the stale
        // footnote claim.
        let mut d = score_line("Dhan");
        let mut g = score_line("Groww");
        d.stalls = -1;
        g.stalls = -1;
        let msg = scorecard(d, g).to_message();
        assert!(msg.contains("Stalls: Dhan ? | Groww ?"), "{msg}");
        assert!(
            !msg.contains("Stall tracking starts with the next upgrade"),
            "the retired PR-1 stall footnote must not render: {msg}"
        );
        // Partial + degraded days carry loud warnings — the partial wording
        // names the HONEST PR-1 cause (a read failure while building the
        // card), never an unmeasured "app was down" claim.
        let ev = NotificationEvent::DualFeedDailyScorecard {
            trading_date_ist: "2026-07-10".to_string(),
            dhan: score_line("Dhan"),
            groww: score_line("Groww"),
            session_minutes: 375,
            partial_coverage: true,
            degraded: true,
            early_run: false,
            restart_partial: false,
            dhan_feed_off: false,
            groww_feed_off: false,
            rest_legs: vec![],
            rest_legs_read_failed: false,
        };
        let msg = ev.to_message();
        assert!(
            msg.contains("Some of today's records could not be read"),
            "{msg}"
        );
        assert!(
            !msg.contains("the app did not watch the whole session"),
            "the unmeasured restart claim must not render: {msg}"
        );
        assert!(msg.contains("treat the drop counts as a minimum"), "{msg}");
        // An early forced run says so explicitly (the row is stamped
        // partial; the card must not masquerade as end-of-day).
        let ev = NotificationEvent::DualFeedDailyScorecard {
            trading_date_ist: "2026-07-10".to_string(),
            dhan: score_line("Dhan"),
            groww: score_line("Groww"),
            session_minutes: 375,
            partial_coverage: false,
            degraded: false,
            early_run: true,
            restart_partial: false,
            dhan_feed_off: false,
            groww_feed_off: false,
            rest_legs: vec![],
            rest_legs_read_failed: false,
        };
        let msg = ev.to_message();
        assert!(
            msg.contains("produced early on operator request"),
            "early-run footnote missing: {msg}"
        );
    }

    #[test]
    fn test_dual_feed_scorecard_mixed_lag_state_footnote_keys_on_either_feed() {
        // PR-C review round 1 (2026-07-11): the delay footnote branch was
        // keyed on Dhan alone — a Dhan-off / thin-Dhan day with a measured
        // Groww delay rendered "Typical delay: Dhan not measured yet |
        // Groww 180 ms" directly above "Delay could not be measured today"
        // (a Rule-11 self-contradiction on the operator surface). The gate
        // now keys on EITHER feed.
        let d = score_line("Dhan"); // lag -1/-1
        let mut g = score_line("Groww");
        g.lag_p50_ms = 180;
        g.lag_p99_ms = 740;
        let msg = scorecard(d, g).to_message();
        assert!(
            !msg.contains("Delay could not be measured today"),
            "a measured Groww delay must not render under an unmeasured claim: {msg}"
        );
        assert!(
            msg.contains("Dhan's price clock ticks in whole seconds"),
            "the asymmetry footnote must render on a mixed-state day: {msg}"
        );
        assert!(msg.contains("180 ms"), "{msg}");
        // The mirror mixed state (Dhan measured / Groww unmeasured) —
        // reachable on a thin-Groww day — must not claim unmeasured either.
        let mut d = score_line("Dhan");
        d.lag_p50_ms = 1200;
        d.lag_p99_ms = 2900;
        let g = score_line("Groww"); // lag -1/-1
        let msg = scorecard(d, g).to_message();
        assert!(
            !msg.contains("Delay could not be measured today"),
            "a measured Dhan delay must not render under an unmeasured claim: {msg}"
        );
    }

    #[test]
    fn test_dual_feed_scorecard_restart_partial_footnote() {
        // Round-3 hostile review 2026-07-10: a restart day's card must
        // carry the same partial caveat its persisted row does — never a
        // silent row-vs-card honesty mismatch on exactly the day the
        // feature exists for.
        let ev = NotificationEvent::DualFeedDailyScorecard {
            trading_date_ist: "2026-07-10".to_string(),
            dhan: score_line("Dhan"),
            groww: score_line("Groww"),
            session_minutes: 375,
            partial_coverage: false,
            degraded: false,
            early_run: false,
            restart_partial: true,
            dhan_feed_off: false,
            groww_feed_off: false,
            rest_legs: vec![],
            rest_legs_read_failed: false,
        };
        let msg = ev.to_message();
        assert!(
            msg.contains("The app restarted during the day"),
            "restart-partial footnote missing: {msg}"
        );
        // Absent on a clean day.
        let msg = scorecard(score_line("Dhan"), score_line("Groww")).to_message();
        assert!(
            !msg.contains("The app restarted during the day"),
            "restart footnote must not render on a clean day: {msg}"
        );
    }

    #[test]
    fn test_dual_feed_scorecard_groww_drops_sentinel_footnote() {
        // Scoreboard PR-B (2026-07-10): the round-2 Groww drops blind spot
        // closed with the stall rows — the footnote is RETIRED. A `-1`
        // (defensive) still renders as "?" and must never decide a verdict
        // rung, but no stale "not counted yet" claim renders.
        let mut g = score_line("Groww");
        g.drops_market = -1;
        let msg = scorecard(score_line("Dhan"), g).to_message();
        assert!(
            msg.contains("Drops in market hours: Dhan 3 | Groww ?"),
            "{msg}"
        );
        assert!(
            !msg.contains("Groww connection drops are not counted yet"),
            "the retired blind-spot footnote must not render: {msg}"
        );
        // The drops verdict rung must not decide against the sentinel —
        // identical evidence elsewhere → Even day.
        let mut d = score_line("Dhan");
        d.blame_broker = 5;
        let mut g = score_line("Groww");
        g.drops_market = -1;
        let msg = scorecard(d, g).to_message();
        assert!(
            msg.contains("\u{1f91d} Verdict: Even day."),
            "a sentinel drops side must never lose/win the drops rung: {msg}"
        );
        // Both sides measured → still no footnote.
        let msg = scorecard(score_line("Dhan"), score_line("Groww")).to_message();
        assert!(
            !msg.contains("Groww connection drops are not counted yet"),
            "{msg}"
        );
    }

    #[test]
    fn test_dual_feed_scorecard_feed_off_no_contest() {
        // Round-4 hostile review 2026-07-10 (the round-2 finding): a feed
        // switched OFF for the day is a one-horse race — the verdict must
        // say "no contest" (never crown the other feed on exclusive
        // minutes) and the card must carry the switched-off footnote.
        let mut g = score_line("Groww");
        g.ticks = 0;
        g.exclusive_minutes = 0;
        g.streaming_minutes = 0;
        let ev = NotificationEvent::DualFeedDailyScorecard {
            trading_date_ist: "2026-07-10".to_string(),
            dhan: score_line("Dhan"),
            groww: g,
            session_minutes: 375,
            partial_coverage: false,
            degraded: false,
            early_run: false,
            restart_partial: false,
            dhan_feed_off: false,
            groww_feed_off: true,
            rest_legs: vec![],
            rest_legs_read_failed: false,
        };
        let msg = ev.to_message();
        assert!(
            msg.contains(
                "not comparable — Groww was switched off \n             today, no contest"
            ) || msg.contains("not comparable — Groww was switched off today, no contest"),
            "feed-off rung-0 verdict missing: {msg}"
        );
        assert!(
            !msg.contains("won today"),
            "a switched-off feed's day must never declare a winner: {msg}"
        );
        assert!(
            msg.contains("Groww was switched off today — no contest"),
            "feed-off footnote missing: {msg}"
        );
        // Both feeds off (a feeds-disabled test session) — still no winner.
        let mut d = score_line("Dhan");
        d.ticks = 0;
        d.exclusive_minutes = 0;
        d.streaming_minutes = 0;
        let mut g = score_line("Groww");
        g.ticks = 0;
        g.exclusive_minutes = 0;
        g.streaming_minutes = 0;
        let ev = NotificationEvent::DualFeedDailyScorecard {
            trading_date_ist: "2026-07-10".to_string(),
            dhan: d,
            groww: g,
            session_minutes: 375,
            partial_coverage: false,
            degraded: false,
            early_run: false,
            restart_partial: false,
            dhan_feed_off: true,
            groww_feed_off: true,
            rest_legs: vec![],
            rest_legs_read_failed: false,
        };
        let msg = ev.to_message();
        assert!(
            msg.contains("both feeds were switched"),
            "both-off verdict missing: {msg}"
        );
        // A clean comparable day renders NO feed-off wording.
        let msg = scorecard(score_line("Dhan"), score_line("Groww")).to_message();
        assert!(!msg.contains("switched off today"), "{msg}");
    }

    #[test]
    fn test_dual_feed_scorecard_body_obeys_telegram_commandments() {
        // 10-commandments litmus: emoji-first subject, IST 12-hour time,
        // specific numbers, blame split, streaming check, no file paths,
        // no library/infrastructure names.
        let msg = scorecard(score_line("Dhan"), score_line("Groww")).to_message();
        assert!(msg.contains("\u{1f4ca}"), "scorecard leads with 📊: {msg}");
        assert!(msg.contains("3:45 PM IST"), "IST 12-hour time: {msg}");
        assert!(msg.contains("Date: 2026-07-10"), "{msg}");
        // Commandment 6: big counts carry thousands separators.
        assert!(
            msg.contains("Ticks today: Dhan 1,842,551 | Groww 1,842,551"),
            "{msg}"
        );
        assert!(
            msg.contains("Drops in market hours: Dhan 3 | Groww 3"),
            "drops line missing: {msg}"
        );
        // The blame split is its OWN line, decoupled from the drops count
        // (it covers ALL headline incidents — drops + stalls + restarts).
        assert!(
            msg.contains("Who caused today's incidents: Dhan broker 2 / ours 0 / unclear 1"),
            "incident split line missing: {msg}"
        );
        assert!(msg.contains("Streaming: Dhan 373 of 375 min"), "{msg}");
        for banned in ["data/", "QuestDB", "ILP", "DEDUP", ".rs", "SQL"] {
            assert!(
                !msg.contains(banned),
                "operator text must not carry {banned:?}: {msg}"
            );
        }
    }

    #[test]
    fn test_dual_feed_scorecard_aborted_event() {
        let ev = NotificationEvent::DualFeedScorecardAborted {
            detail: "the task crashed: boom".to_string(),
        };
        assert_eq!(ev.topic(), "DualFeedScorecardAborted");
        assert_eq!(ev.severity(), Severity::High);
        let msg = ev.to_message();
        assert!(msg.contains("Daily feed scorecard did NOT run"), "{msg}");
        assert!(msg.contains("3:45 PM IST"), "{msg}");
        assert!(msg.contains("Reason: the task crashed: boom"), "{msg}");
        assert!(msg.contains("What to do RIGHT NOW"), "{msg}");
    }

    // -----------------------------------------------------------------------
    // BrutexCrossverifySummary + BrutexCrossverifyAborted
    // (BRUTEX-XVERIFY, 2026-07-12)
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn brutex_summary(
        outcome: &str,
        diverged: i64,
        noise_p95_paise: i64,
        noise_max_paise: i64,
        top_offenders: &str,
        hint: &str,
    ) -> NotificationEvent {
        NotificationEvent::BrutexCrossverifySummary {
            trading_date_ist: "2026-07-12".to_string(),
            outcome: outcome.to_string(),
            files_read: 5,
            symbols_compared: 742,
            matched: 276_490,
            diverged,
            missing_brutex: 12,
            missing_live: 3,
            tail_unsealed: 742,
            unmapped: 6,
            noise_p95_paise,
            noise_max_paise,
            top_offenders: top_offenders.to_string(),
            hint: hint.to_string(),
        }
    }

    #[test]
    fn test_brutex_crossverify_summary_clean_day() {
        let ev = brutex_summary("clean", 0, 5, 40, "", "");
        assert_eq!(ev.topic(), "BrutexCrossverifySummary");
        assert_eq!(ev.severity(), Severity::Info);
        // The once-per-day 15:50 digest must arrive AT 15:50 — post-close
        // is off-hours, so the default Info routing would coalesce it
        // (CrossVerify1mSummary / DualFeedDailyScorecard precedent).
        assert_eq!(ev.dispatch_policy(), DispatchPolicy::Immediate);
        let msg = ev.to_message();
        assert!(
            msg.starts_with("\u{2705}"),
            "clean day leads with the canonical OK emoji: {msg}"
        );
        assert!(
            msg.contains("BruteX vs live 1-minute check — 2026-07-12: CLEAN"),
            "{msg}"
        );
        assert!(msg.contains("Files read from BruteX: 5"), "{msg}");
        assert!(msg.contains("Symbols compared: 742"), "{msg}");
        // Commandment 6: big counts carry thousands separators.
        assert!(
            msg.contains("Minutes matched: 276,490 | Diverged: 0"),
            "{msg}"
        );
        assert!(
            msg.contains("Typical wiggle: within 5 paise on 95% of compared minutes (max 40)."),
            "noise band line missing: {msg}"
        );
        // The honesty line is ALWAYS present.
        assert!(
            msg.contains(
                "Neither side is the ground truth — small wiggle is expected; \
                 only beyond-tolerance rows are real divergence."
            ),
            "honesty line missing: {msg}"
        );
        // No loud warning line and no offenders block on a clean day.
        assert!(!msg.contains('\u{26a0}'), "{msg}");
        assert!(!msg.contains("Biggest differences today"), "{msg}");
        // 10-commandments litmus: no file paths / library / infra names.
        for banned in ["data/", "QuestDB", "ILP", "DEDUP", ".rs", "SQL", "S3"] {
            assert!(
                !msg.contains(banned),
                "operator text must not carry {banned:?}: {msg}"
            );
        }
    }

    #[test]
    fn test_brutex_crossverify_summary_no_data_is_loud_and_renders_sentinels() {
        let ev = NotificationEvent::BrutexCrossverifySummary {
            trading_date_ist: "2026-07-12".to_string(),
            outcome: "no_data".to_string(),
            files_read: 0,
            symbols_compared: 0,
            matched: 0,
            diverged: 0,
            missing_brutex: 0,
            missing_live: 0,
            tail_unsealed: 0,
            unmapped: 0,
            noise_p95_paise: -1,
            noise_max_paise: -1,
            top_offenders: String::new(),
            hint: String::new(),
        };
        let msg = ev.to_message();
        assert!(msg.contains(": NO DATA"), "{msg}");
        // The LOUD line: an empty compare set must never read quiet
        // (audit Rule 11 — no false-OK on nothing).
        assert!(
            msg.contains(
                "\u{26a0}\u{fe0f} No files arrived from BruteX today — \
                 nothing was compared."
            ),
            "loud no-data line missing: {msg}"
        );
        // -1 sentinels render as "?" — never fabricated zeros.
        assert!(
            msg.contains("Typical wiggle: within ? paise on 95% of compared minutes (max ?)."),
            "sentinel rendering wrong: {msg}"
        );
    }

    #[test]
    fn test_brutex_crossverify_summary_diverged_carries_offenders_and_honesty() {
        let ev = brutex_summary(
            "diverged",
            42,
            5,
            180,
            "RELIANCE 10:15 AM high off by 15 paise\n\
             INFY 11:30 AM close off by 9 paise",
            "Our feed had a stall episode at 10:14 AM — the gap lines up.",
        );
        let msg = ev.to_message();
        assert!(
            msg.starts_with("\u{26a0}\u{fe0f}"),
            "diverged day leads with the canonical warning emoji: {msg}"
        );
        assert!(
            msg.contains("BruteX vs live 1-minute check — 2026-07-12: DIVERGED"),
            "{msg}"
        );
        assert!(
            msg.contains("Minutes matched: 276,490 | Diverged: 42"),
            "{msg}"
        );
        assert!(msg.contains("Biggest differences today:"), "{msg}");
        assert!(
            msg.contains("RELIANCE 10:15 AM high off by 15 paise"),
            "offender line missing: {msg}"
        );
        assert!(
            msg.contains("Our feed had a stall episode at 10:14 AM"),
            "hint line missing: {msg}"
        );
        // The honesty line survives on a diverged day too.
        assert!(
            msg.contains(
                "Neither side is the ground truth — small wiggle is expected; \
                 only beyond-tolerance rows are real divergence."
            ),
            "honesty line missing: {msg}"
        );
    }

    #[test]
    fn test_brutex_crossverify_aborted_event() {
        let ev = NotificationEvent::BrutexCrossverifyAborted {
            trading_date_ist: "2026-07-12".to_string(),
            reason: "task_panicked".to_string(),
        };
        assert_eq!(ev.topic(), "BrutexCrossverifyAborted");
        assert_eq!(ev.severity(), Severity::High);
        let msg = ev.to_message();
        assert!(msg.contains("\u{1f198}"), "leads with 🆘: {msg}");
        assert!(
            msg.contains("BruteX cross-verify run FAILED — 2026-07-12"),
            "{msg}"
        );
        assert!(msg.contains("3:50 PM IST"), "{msg}");
        assert!(msg.contains("Reason: task_panicked"), "{msg}");
        assert!(msg.contains("What to do RIGHT NOW"), "{msg}");
    }

    #[test]
    fn test_scorecard_render_helpers() {
        // render_ms: sentinel / sub-second / ≥1s bands.
        assert_eq!(render_ms(-1), "not measured yet");
        assert_eq!(render_ms(180), "180 ms");
        assert_eq!(render_ms(2900), "2.9 s");
        // render_count: sentinel = honest "?", never a fabricated zero;
        // big measured counts get thousands separators (commandment 6).
        assert_eq!(render_count(-1), "?");
        assert_eq!(render_count(42), "42");
        assert_eq!(render_count(9_999), "9999", "grouping starts at 10,000");
        assert_eq!(render_count(10_000), "10,000");
        assert_eq!(render_count(1_842_551), "1,842,551");
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
    }

    // test_token_forced_remint_triggered_event DELETED 2026-07-14 with the
    // TokenForcedRemintTriggered variant (Dhan noise lock) — the forced
    // re-mint self-heals silently; only a terminal failure pages via the
    // AuthenticationFailed family-(3) Critical.

    // -----------------------------------------------------------------
    // W2 PR#5 (2026-07-10) — HolidayCalendarCoverageLow
    // -----------------------------------------------------------------

    #[test]
    fn test_holiday_calendar_coverage_low_severity_is_high() {
        let ev = NotificationEvent::HolidayCalendarCoverageLow {
            days_remaining: Some(46),
            coverage_end_display: "31 Dec 2026".to_string(),
        };
        assert_eq!(ev.severity(), Severity::High);
        assert_eq!(ev.topic(), "HolidayCalendarCoverageLow");
    }

    #[test]
    fn test_holiday_calendar_coverage_low_body_commandments() {
        let msg = NotificationEvent::HolidayCalendarCoverageLow {
            days_remaining: Some(46),
            coverage_end_display: "31 Dec 2026".to_string(),
        }
        .to_message();
        // Specific numbers + the covered-end date + action steps.
        assert!(msg.contains("46 days"), "got: {msg}");
        assert!(msg.contains("31 Dec 2026"), "got: {msg}");
        assert!(msg.contains("What you need to do"), "got: {msg}");
        assert!(msg.contains("NSE"), "got: {msg}");
        // 10 Telegram commandments: no file paths, no library names.
        assert!(!msg.contains(".rs"), "file paths must never appear: {msg}");
        assert!(
            !msg.contains(".toml"),
            "file paths must never appear: {msg}"
        );
        assert!(
            !msg.contains("HashSet") && !msg.contains("is_holiday"),
            "library/function names must never appear: {msg}"
        );
    }

    #[test]
    fn test_holiday_calendar_coverage_low_body_past_cliff() {
        let msg = NotificationEvent::HolidayCalendarCoverageLow {
            days_remaining: Some(-3),
            coverage_end_display: "31 Dec 2026".to_string(),
        }
        .to_message();
        // Past-cliff wording is HONEST: already ran out + consequence.
        assert!(msg.contains("already ran out 3 days ago"), "got: {msg}");
        assert!(
            msg.contains("treated as trading days"),
            "past-cliff body must state the live consequence: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // Chain1mUnderlyingNotServed + Chain1mUnderlyingServedRecovered
    // (2026-07-14 — the Dhan mirror of the Groww #1537 per-underlying
    // detector; noise-lock family-(2) extension per
    // dhan-rest-only-noise-lock-2026-07-14.md §2.1)
    // -----------------------------------------------------------------------

    #[test]
    fn test_chain_1m_underlying_not_served_is_high_names_dhan_and_underlying() {
        let event = NotificationEvent::Chain1mUnderlyingNotServed {
            underlying: "NIFTY",
            empty_minutes: 10,
        };
        assert_eq!(event.topic(), "Chain1mUnderlyingNotServed");
        assert_eq!(event.severity(), Severity::High);
        let msg = event.to_message();
        // Broker-naming directive: the body names Dhan (never depends on
        // the badge alone), and the badge layer stamps the Dhan badge.
        assert!(
            msg.contains("Dhan is not returning the option chain for NIFTY"),
            "got: {msg}"
        );
        assert!(
            msg.contains("not returning the option chain for"),
            "got: {msg}"
        );
        assert!(msg.contains("other indices are unaffected"), "got: {msg}");
        assert!(msg.contains("10 minutes in a row"), "got: {msg}");
        assert!(msg.contains("What to do RIGHT NOW"), "got: {msg}");
        // Honest scope lines: the live WS pipeline is untouched, the
        // sibling broker is stated as MAY + the explicit IF (independent
        // serving state — no false-OK), and nothing crosses feeds.
        assert!(msg.contains("NOT affected"), "got: {msg}");
        assert!(msg.contains("may still be coming in"), "got: {msg}");
        assert!(msg.contains("IF Groww is serving it"), "got: {msg}");
        assert!(msg.contains("nothing is made up"), "got: {msg}");
        assert!(
            msg.contains("nothing is copied across brokers"),
            "got: {msg}"
        );
        // 10-commandment hygiene: no file paths / config extensions.
        assert!(!msg.contains(".rs"), "no file paths in Telegram: {msg}");
        assert!(!msg.contains(".toml"), "no config paths in Telegram: {msg}");
    }

    #[test]
    fn test_chain_1m_underlying_served_recovered_is_info() {
        let event = NotificationEvent::Chain1mUnderlyingServedRecovered {
            underlying: "NIFTY",
            empty_minutes: 12,
        };
        assert_eq!(event.topic(), "Chain1mUnderlyingServedRecovered");
        assert_eq!(event.severity(), Severity::Info);
        let msg = event.to_message();
        assert!(
            msg.contains("Dhan is serving the option chain for NIFTY again"),
            "got: {msg}"
        );
        assert!(msg.contains("12 empty"), "got: {msg}");
        // No false-OK: recovery never claims the missing minutes came
        // back — the operator is pointed at the Groww copy instead.
        assert!(msg.contains("nothing is made up"), "got: {msg}");
        assert!(msg.contains("check the Groww copy"), "got: {msg}");
    }
}
