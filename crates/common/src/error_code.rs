//! Central structured error-code taxonomy.
//!
//! Every known error/invariant in the tickvault workspace MUST appear as a
//! variant of [`ErrorCode`]. This is the single source of truth that:
//!
//! 1. Lets `error!` sites carry a stable, machine-parseable `code` field —
//!    Claude Code and Loki alert rules pattern-match on this.
//! 2. Maps every code to a [`Severity`] so the notification service can
//!    choose Telegram-only vs Telegram+SNS-SMS automatically.
//! 3. Maps every code to a runbook URL so operators and Claude sessions can
//!    jump directly to the authoritative enforcement rule.
//! 4. Lets the integration test
//!    `tests/error_code_rule_file_crossref.rs` assert that every variant
//!    is mentioned in at least one `.claude/rules/*.md` file AND that every
//!    rule code mentioned there has a matching variant here — the tree
//!    stays in sync mechanically.
//!
//! Phase 1 of `.claude/plans/active-plan.md`.

use std::fmt;
use std::str::FromStr;

/// Operational severity of an error event.
///
/// Order matters: `Info < Low < Medium < High < Critical`. The notification
/// service routes High/Critical events to both Telegram and AWS SNS SMS;
/// lower severities go Telegram-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Informational — no action required, visibility only.
    Info,
    /// Low — log + dashboard; no paging.
    Low,
    /// Medium — Telegram alert, best-effort auto-triage.
    Medium,
    /// High — Telegram + SNS SMS, manual triage likely.
    High,
    /// Critical — Telegram + SNS SMS, halts a sub-system, manual triage required.
    Critical,
}

impl Severity {
    /// Returns the canonical lower-cased label ("info", "low", ...).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every known error/invariant in the tickvault workspace.
///
/// Variants are grouped by domain. New codes MUST be added here AND in the
/// corresponding `.claude/rules/` markdown file — the cross-ref integration
/// test enforces this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    // -----------------------------------------------------------------------
    // Instrument — Priority 0 (data-loss / correctness)
    // -----------------------------------------------------------------------
    // PR #6b (2026-05-19): I-P0-01 / I-P0-02 / I-P0-04 / I-P0-05 / I-P0-06 RETIRED
    // (universe_builder + validation + binary_cache + s3_backup + instrument_loader
    // modules deleted under 4-IDX_I LOCKED_UNIVERSE).
    /// Expired contract reached OMS submit Gate 4.
    InstrumentP0ExpiryAtGate4,

    // -----------------------------------------------------------------------
    // Instrument — Priority 1
    // -----------------------------------------------------------------------
    // PR #6a (2026-05-19): I-P1-01 / I-P1-02 / I-P1-03 retired
    // (daily_scheduler + delta_detector modules deleted under 4-IDX_I scope).
    /// Compound DEDUP key missing underlying_symbol.
    InstrumentP1CompoundDedupKey,
    /// Tick DEDUP key missing segment.
    InstrumentP1SegmentInTickDedup,
    /// Single-row-per-instrument lifecycle violated (row accumulation).
    InstrumentP1SingleRowLifecycle,
    /// Cross-segment security_id collision detected.
    InstrumentP1CrossSegmentCollision,

    // -----------------------------------------------------------------------
    // Instrument — Priority 2
    // -----------------------------------------------------------------------
    /// Trading-day guard triggered on weekend/holiday download.
    InstrumentP2TradingDayGuard,

    // -----------------------------------------------------------------------
    // Networking / API security
    // -----------------------------------------------------------------------
    /// IP monitor detected unexpected public IP change.
    GapNetIpMonitor,
    /// API auth middleware rejected request.
    GapSecApiAuth,

    // -----------------------------------------------------------------------
    // Order Management System
    // -----------------------------------------------------------------------
    /// OMS order lifecycle state-machine invalid transition.
    OmsGapStateMachine,
    /// OMS reconciliation mismatch between local and Dhan.
    OmsGapReconciliation,
    /// OMS circuit breaker tripped.
    OmsGapCircuitBreaker,
    /// OMS rate limit (SEBI 10/sec or daily cap) exceeded.
    OmsGapRateLimit,
    /// OMS idempotency tracking failed.
    OmsGapIdempotency,
    /// OMS dry-run safety gate triggered (unexpected HTTP attempt).
    OmsGapDryRunSafety,

    // -----------------------------------------------------------------------
    // WebSocket
    // -----------------------------------------------------------------------
    /// WebSocket disconnect code outside known set.
    WsGapDisconnectClassification,
    /// WebSocket subscription batching exceeded Dhan limits.
    WsGapSubscriptionBatching,
    /// WebSocket connection state-machine invalid transition.
    WsGapConnectionState,

    // -----------------------------------------------------------------------
    // Risk engine
    // -----------------------------------------------------------------------
    /// Pre-trade risk check rejected an order.
    RiskGapPreTrade,
    /// Position or P&L tracking invariant violated.
    RiskGapPositionPnl,
    /// Tick-gap detector fired above ERROR threshold.
    RiskGapTickGap,

    // -----------------------------------------------------------------------
    // Authentication
    // -----------------------------------------------------------------------
    /// Token expiry validation failed (expired or invalid state).
    AuthGapTokenExpiry,
    /// Only disconnect-code 807 should trigger token refresh; other code did.
    AuthGapDisconnectTokenMap,

    // -----------------------------------------------------------------------
    // Storage
    // -----------------------------------------------------------------------
    /// Tick DEDUP key constant missing segment column.
    StorageGapTickDedupSegment,
    /// f32→f64 conversion used raw widening (precision loss).
    StorageGapF32F64Precision,

    // -----------------------------------------------------------------------
    // Wave 1 hot-path / Phase 2 / prev-close / movers (PR #393)
    // -----------------------------------------------------------------------
    /// HOT-PATH-01: sync filesystem I/O failed inside an async writer task.
    HotPath01SyncFsFailed,
    /// HOT-PATH-02: hot-path writer queue full / closed / uninitialized.
    HotPath02WriterQueueDrop,
    // PR #5 (2026-05-19): PHASE2-01 + PHASE2-02 variants RETIRED.
    // Phase 2 dispatcher chain (phase2_scheduler + phase2_delta +
    // phase2_emit_guard + phase2_readiness_check + phase2_recovery) is
    // deleted under operator-locked 4-IDX_I LOCKED_UNIVERSE.
    /// PREVCLOSE-01: previous_close ILP append or flush failed.
    PrevClose01IlpFailed,
    /// PREVCLOSE-02: first_seen_set inconsistency (reserved for future use).
    PrevClose02FirstSeenInconsistency,
    // Phase 4b cleanup (2026-05-05): MOVERS-01 / MOVERS-02 / MOVERS-03
    // variants RETIRED. Their backing writers (StockMoversWriter,
    // OptionMoversWriter) were deleted in PR #494 along with the
    // `stock_movers` / `option_movers` table-bound persist code.
    // Movers persistence is now exclusively `MoversWriter`
    // (movers_1s + 25 mat views) populated by `movers_pipeline`.
    /// PREVOI-01: prev_oi cache empty at boot — `/api/movers` OI Change
    /// column will display `current_OI - 0 = current_OI` until PR #452
    /// boot orchestrator wires the bhavcopy + Option Chain prev_oi loader.
    /// Severity::Medium — operator-visible WARN per Wave-4 charter Rule 11
    /// (no false-OK signals); the WARN is gated to fire ONCE at boot
    /// rather than per-row. PR #450 commit 8b adversarial-review fix.
    PrevOi01CacheEmptyAtBoot,

    // -----------------------------------------------------------------------
    // Wave 2 — resilience (Items 5–9)
    // -----------------------------------------------------------------------
    /// WS-GAP-04: a WebSocket entered post-close sleep-until-next-open mode.
    WsGap04PostCloseSleep,
    /// WS-GAP-05: pool supervisor respawned a dead connection task.
    WsGap05PoolRespawn,
    /// WS-GAP-06: tick-gap detector fired a coalesced summary.
    WsGap06TickGapSummary,
    /// WS-GAP-07: the live-feed frame channel's receiver was dropped
    /// (the tick-processing consumer died), so the WebSocket read loop
    /// stops forwarding frames and returns. Serious mid-market condition —
    /// no ticks reach the pipeline until the consumer/app restarts.
    /// Severity::High.
    WsGap07LiveChannelClosed,
    /// DISK-WATCHER-01: the spill disk-health watcher task exited
    /// (panic/cancel) and the supervisor respawned it so free-space
    /// monitoring — the early-warning for the "disk full + QuestDB down"
    /// zero-loss gap — keeps running instead of vanishing silently.
    /// Mirrors the WS-GAP-05 pool-supervisor pattern. Severity::Low
    /// (respawn self-heals; the `tv_disk_watcher_respawn_total` counter
    /// feeds a CloudWatch alarm that pages on a flapping watcher).
    DiskWatcher01Respawned,
    /// WS-SPILL-01: the WAL frame-spill writer thread died (panic or a
    /// transient disk I/O error) and the supervisor respawned it — mirrors
    /// WS-GAP-05 (pool supervisor) and DISK-WATCHER-01 (disk watcher).
    /// Self-healing: respawning with the same channel preserves the durable
    /// WAL floor so no Dhan frame is silently lost across a writer hiccup.
    /// Severity::High (a flapping WAL writer means the disk is failing).
    WsSpill01WriterRespawn,
    /// WS-SPILL-02: a Dhan frame was dropped on the hot path because the WAL
    /// spill writer thread was dead at that instant (crossbeam channel
    /// reported `Disconnected`). The WS-SPILL-01 respawn makes this
    /// practically unreachable, but it is the honest last-resort
    /// durable-loss signal — previously this arm returned silently.
    /// Severity::Critical.
    WsSpill02FrameDropped,
    /// AUTH-GAP-03: token force-renewed on WebSocket wake.
    AuthGap03TokenForceRenewedOnWake,
    /// BOOT-01: slow-boot QuestDB readiness deadline approaching (>30s).
    Boot01QuestDbSlow,
    /// BOOT-02: boot deadline exceeded (>60s) — HALTING.
    Boot02DeadlineExceeded,
    /// BOOT-03: wall-clock skew vs trusted source exceeded threshold — HALTING.
    Boot03ClockSkewExceeded,
    // PR #5 (2026-05-19): AUDIT-01 variant RETIRED. phase2_audit_persistence.rs
    // deleted; the only ILP writer that could fire this code is gone.
    /// AUDIT-02: depth-rebalance audit row write failed.
    Audit02DepthRebalanceWriteFailed,
    /// AUDIT-03: WS reconnect audit row write failed.
    Audit03WsReconnectWriteFailed,
    /// AUDIT-04: boot audit row write failed.
    Audit04BootWriteFailed,
    /// AUDIT-05: selftest audit row write failed.
    Audit05SelftestWriteFailed,
    /// AUDIT-06: order audit row write failed.
    Audit06OrderWriteFailed,
    /// STORAGE-GAP-03: audit-table write failure (any table).
    StorageGap03AuditWriteFailed,
    /// STORAGE-GAP-04: S3 archive failure (partition upload).
    StorageGap04S3ArchiveFailed,
    /// SELFTEST-01: market-open self-test passed (informational positive ping).
    Selftest01Passed,
    /// SELFTEST-02: market-open self-test detected a Critical or Degraded
    /// failure — operator action required.
    Selftest02Failed,
    /// SLO-01: composite real-time guarantee score recovered to healthy
    /// (≥ 0.95) on the falling edge from a degraded period. Informational.
    Slo01Healthy,
    /// SLO-02: composite real-time guarantee score crossed below 0.95
    /// (Degraded) or 0.80 (Critical). Edge-triggered on the rising edge
    /// into a worse tier. Operator action required for `Critical`.
    Slo02Degraded,

    // -----------------------------------------------------------------------
    // Wave 3 — Telegram dispatcher (Item 11)
    // -----------------------------------------------------------------------
    /// TELEGRAM-01: a Telegram event was dropped (queue full, send-after-retry
    /// failure, or coalescer overflow). Operator alerts MAY be missed.
    Telegram01Dropped,
    /// TELEGRAM-02: coalescer state inconsistency (drain failed mid-window;
    /// next drain self-recovers). Informational.
    Telegram02CoalescerStateInconsistency,

    // -----------------------------------------------------------------------
    // Dhan Trading API (DH-9xx)
    // -----------------------------------------------------------------------
    /// DH-901: Invalid auth — rotate token, retry once.
    Dh901InvalidAuth,
    /// DH-902: No API access — HALT + alert.
    Dh902NoApiAccess,
    /// DH-903: Account issue — HALT + alert.
    Dh903AccountIssue,
    /// DH-904: Rate limit — exponential backoff.
    Dh904RateLimit,
    /// DH-905: Input exception — never retry.
    Dh905InputException,
    /// DH-906: Order error — never retry.
    Dh906OrderError,
    /// DH-907: Data error — check params.
    Dh907DataError,
    /// DH-908: Internal server error — retry with backoff.
    Dh908InternalServerError,
    /// DH-909: Network error — retry with backoff.
    Dh909NetworkError,
    /// DH-910: Other Dhan error — log + alert.
    Dh910Other,

    // -----------------------------------------------------------------------
    // Dhan Data API (8xx)
    // -----------------------------------------------------------------------
    /// 800: Data API internal server error.
    Data800InternalServerError,
    /// 804: Instruments exceed per-connection limit.
    Data804InstrumentsExceedLimit,
    /// 805: Too many requests/connections — STOP ALL 60s.
    Data805TooManyConnections,
    /// 806: Data APIs not subscribed.
    Data806NotSubscribed,
    /// 807: Access token expired — trigger refresh.
    Data807TokenExpired,
    /// 808: Authentication failed.
    Data808AuthFailed,
    /// 809: Access token invalid.
    Data809TokenInvalid,
    /// 810: Client ID invalid.
    Data810ClientIdInvalid,
    /// 811: Invalid expiry date.
    Data811InvalidExpiry,
    /// 812: Invalid date format.
    Data812InvalidDateFormat,
    /// 813: Invalid SecurityId.
    Data813InvalidSecurityId,
    /// 814: Invalid request.
    Data814InvalidRequest,

    /// Wave 5 Item 26 L1 — volume monotonicity breach at runtime. The Dhan
    /// volume field at bytes 22-25 of the Quote/Full packet is cumulative
    /// since session open per Ticket #5525125 (verified via the live Mon
    /// May 4 monotonicity SELECT — see `docs/operator/track-2-monotonicity-select.md`).
    /// If a tick arrives with `volume < last_seen_volume` for the same
    /// `(security_id, exchange_segment)` within a single trading day, that
    /// breaks cumulative-monotonicity invariant — either Dhan changed the
    /// semantic mid-session (escalate to Item 26 L3 ticket) or our parser
    /// regressed on the byte offset. Severity::High.
    Volume01MonotonicityBreach,
    // PR #5 (2026-05-19): PHASE2-READY-01 variant RETIRED. The 09:13:01 IST
    // pre-flight readiness check is deleted alongside the Phase 2 dispatcher
    // chain under operator-locked 4-IDX_I LOCKED_UNIVERSE.
    /// Wave 5 Item 13 — boot-time prev-close routing assertion failed.
    /// The subscription plan contains an instrument whose `(segment,
    /// feed_mode)` pair cannot deliver previous-day close per the
    /// per-segment routing matrix in `live-market-feed.md`:
    /// IDX_I → Ticker (prev close arrives via standalone code 6),
    /// NSE_EQ → Quote/Full (bytes 38-41 / 50-53 of the Quote/Full
    /// packet), NSE_FNO/BSE_FNO → Full (bytes 50-53). Severity::Critical
    /// — halts boot rather than starting a pipeline that loses
    /// prev_close for half the universe.
    PrevClose03BootRoutingAssertion,
    /// PREVCLOSE-04: F2 boot-time `PrevDayCache` loader could not
    /// populate the cache. Either the QuestDB `previous_close` table
    /// is empty (fresh deployment, no prior trading session) or the
    /// SELECT failed outright. The cascade seal-time pct stamping
    /// (PR #520 / F1) falls back to `0.0` for the 3 % fields per the
    /// `compute_*_pct` div-by-zero policy until the next boot
    /// succeeds in reading `previous_close`. Severity::Medium —
    /// degraded but not catastrophic; ratchet for visibility, not
    /// boot-halt.
    PrevClose04CacheEmptyAtBoot,

    // -----------------------------------------------------------------------
    // Wave 6 — Multi-TF aggregator + direct-flush + rehydrate
    // (`.claude/plans/active-plan-aggregator-direct-flush-rehydrate.md` PR1)
    // -----------------------------------------------------------------------
    /// AGGREGATOR-DROP-01: a sealed candle was dropped after the ring
    /// buffer (`SEAL_BUFFER_CAPACITY`), the disk spill, AND the NDJSON
    /// DLQ all refused the row. The only silent-data-loss path for the
    /// aggregator. Severity::Critical — by definition the host is OOM
    /// AND out of disk AND `data/dlq/` unwritable.
    AggregatorDrop01,
    /// AGGREGATOR-LATE-01: a tick arrived after its 1-minute bucket
    /// already sealed. Discarded with `error!` + counter (NOT silently
    /// merged across buckets — that would shift data across
    /// timestamps). Severity::High — typically clock drift or slow
    /// consumer.
    AggregatorLate01,
    /// AGGREGATOR-LAG-01: the candle aggregator's tick-broadcast receiver
    /// returned `Lagged(n)` — it fell so far behind the ~52s
    /// `TICK_BROADCAST_CAPACITY` buffer that the broadcast dropped `n`
    /// ticks from ITS view. The dropped ticks are NOT lost from the
    /// `ticks` table (a separate, lossless+ordered consumer) — only the
    /// derived candles for that window may under-count. Was a silent
    /// counter bump; now `error!` + counter so the operator sees the
    /// (very rare, >52s-stall-class) incident and can rebuild the
    /// affected candle window from the lossless `ticks` table (the
    /// 15:31 IST post-market 1m cross-verify pinpoints the minutes).
    /// Severity::High. Tick ROUTING and ORDER are untouched.
    AggregatorLag01TickLagDropped,
    /// AGGREGATOR-SEAL-01: seal-time ILP write to one of the 9
    /// `candles_*_shadow` tables failed; the row was caught by the
    /// ring buffer. Severity::Medium — data is buffered, not lost.
    /// Also covers the H11 precursor fix at
    /// `crates/storage/src/candle_persistence.rs::flush_buffer`.
    AggregatorSeal01IlpFailed,
    /// AGGREGATOR-HB-01: per-minute seal-burst heartbeat emitted after
    /// each minute boundary completes. Severity::Info — positive
    /// false-OK-avoidance signal per `audit-findings-2026-04-17.md`
    /// Rule 11.
    AggregatorHb01Heartbeat,
    /// BOUNDARY-01: boundary timer detected one or more skipped minute
    /// boundaries (OS scheduler preemption, clock slew, slow consumer)
    /// and walked forward sealing each missed bucket from the in-cell
    /// state. Severity::Medium — late but correct. Repeated firing
    /// within a minute window indicates wall-clock instability;
    /// escalate to BOOT-03 territory.
    Boundary01CatchupSeal,
    /// RESILIENCE-01: another `tickvault` process holds the SSM
    /// dual-instance lock for this client-id. Two processes running
    /// against the same Dhan account fight over static-IP enforcement
    /// and fragment the WebSocket connection budget. Boot HALTS until
    /// the other instance shuts down (or its 90 s TTL expires after
    /// a hard crash). Severity::Critical.
    Resilience01DualInstanceDetected,
    /// ORPHAN-POSITION-01: at 15:25:00 IST the orphan-position
    /// watchdog observed one or more open positions (`net_qty != 0`)
    /// in the account. Strategy is intraday option-buying — NO
    /// overnight positions allowed. Severity::Critical. In Phase 0
    /// (dry-run) the watchdog logs + audits + Telegrams; in Phase 1+
    /// it cancels Super Order legs and places market exits before
    /// the 15:30 IST close.
    OrphanPosition01Detected,
    /// BAR-MISMATCH-01: at 09:16:05 IST the post-open cross-check
    /// fetched Dhan REST `/v2/charts/intraday` for the 09:15 1m bar
    /// across all 222 SIDs and found at least one bar whose OHLCV
    /// disagreed with our `candles_1m_shadow` row outside the
    /// `PRICE_TOLERANCE_RUPEES` (0.01) tolerance. The mismatched bars
    /// were CORRECTED — Dhan's authoritative values written to
    /// `candles_1m_shadow` + `historical_candles` (mirror) + audit
    /// row in `bar_correction_audit`. Severity::Critical so the
    /// operator sees the correction summary before the strategy gate
    /// opens.
    BarMismatch01CorrectedFromHistorical,
    /// BAR-MISMATCH-02: 09:16:05 IST cross-check completed but with
    /// fewer than `MIN_COMPARED_COUNT_FOR_PASS` (200/222) Dhan REST
    /// responses. Cross-check is INCONCLUSIVE — strategy gate stays
    /// CLOSED for the trading day. Operator must inspect Dhan REST
    /// health + manually authorize trading. Severity::Critical.
    BarMismatch02CrossCheckInconclusive,
    /// BAR-MISMATCH-03: 09:16:05 IST cross-check HARD-FAILED (auth
    /// error, network failure, all 222 REST fetches errored). Strategy
    /// gate stays CLOSED. Operator must intervene. Severity::Critical.
    BarMismatch03CrossCheckFailed,

    // -----------------------------------------------------------------------
    // Sub-PR #9 of 2026-05-27 daily-universe expansion: lifecycle-fetch
    // ErrorCodes for the boot-time CSV → universe-build chain. Emit
    // sites land in Sub-PR #10 (boot orchestrator). NO production emit
    // sites in THIS PR — contract stubs only, gated by the cross-ref
    // test against rule-file mentions.
    // -----------------------------------------------------------------------
    /// INSTR-FETCH-01: Detailed CSV fetch hard-failed after retry
    /// exhaustion. The §4 infinite-retry policy on this code is
    /// operator-locked — the boot orchestrator keeps retrying with
    /// escalating Telegram. This variant fires on EVERY attempt past
    /// the §4 escalation table (Info at attempt 1-3, High at 4-10,
    /// Critical at 11+). Severity::Critical.
    InstrFetch01CsvHardFailed,

    /// INSTR-FETCH-02: CSV parse / schema validation failed. The bytes
    /// passed §18 hardening (Sub-PR #3) but failed §26 robustness
    /// (Sub-PR #4) — mandatory-field validation triggered the >0.1%
    /// row failure threshold OR a mandatory header column was missing.
    /// Severity::Critical — operator must inspect raw CSV before
    /// retry can succeed.
    InstrFetch02SchemaValidationFailed,

    /// INSTR-FETCH-03: F&O dangling-reference threshold exceeded.
    /// More than 0.5% of FUTSTK/OPTSTK derivative rows referenced an
    /// `UNDERLYING_SECURITY_ID` that didn't resolve to any NSE_EQ row
    /// in the same CSV. Per Sub-PR #5 + §3 + §26. Severity::Critical
    /// — boot HALTS until the upstream CSV is consistent.
    InstrFetch03DanglingReferences,

    /// INSTR-FETCH-04: Universe-size envelope violated. The built
    /// universe contained `< MIN_DAILY_UNIVERSE_SIZE (100)` OR
    /// `> MAX_DAILY_UNIVERSE_SIZE (1200)` instruments per Sub-PR #7's
    /// envelope check. Either an upstream extractor returned partial
    /// data (too small) or a regression let in extra rows (too large).
    /// Severity::Critical — boot HALTS.
    InstrFetch04UniverseSizeOutOfBounds,

    /// NTM-CONSTITUENCY-01: the NIFTY Total Market constituent source
    /// (niftyindices.com) failed at boot — fetch error, malformed CSV, or
    /// `> 0.5%` dangling constituents vs the Dhan master. Per operator
    /// AskUserQuestion 2026-06-06 the policy is **degrade + alert**: boot
    /// PROCEEDS on the indices + F&O-underlyings core universe (the Dhan CSV
    /// path stays fail-closed §4) and the ~500 cash-only NTM constituents are
    /// absent for the day. Severity::Critical — the operator is paged that
    /// today runs without the full NTM union. NOT a boot halt.
    NtmConstituency01SourceDegraded,

    // -----------------------------------------------------------------------
    // Operator directive 2026-06-02: post-market 1-minute cross-verification.
    // At 15:31 IST we compare every subscribed spot instrument's live
    // candles_1m OHLCV against Dhan intraday 1-minute candles, EXACT match.
    // Narrowed replacement for the deleted cross_verify chain (1m/spot/today).
    // See cross-verify-1m-error-codes.md + live-feed-purity.md rule 11.
    // -----------------------------------------------------------------------
    /// CROSS-VERIFY-1M-01: one or more 1-minute OHLCV cells disagreed between
    /// our `candles_1m` and Dhan intraday. Mismatches written to the
    /// `cross_verify_1m_audit` table + CSV; the per-day count is the quality
    /// signal. Severity::High (operator-visible; expected non-zero due to
    /// sampled-feed vs full-tape — track the trend, not the absolute).
    CrossVerify1m01MismatchFound,
    /// CROSS-VERIFY-1M-02: the Dhan intraday fetch was degraded — REST errored
    /// / rate-limited for a material fraction of spot SIDs, so the
    /// verification could not vouch for the full universe. Severity::High.
    CrossVerify1m02FetchDegraded,

    // -----------------------------------------------------------------------
    // PR #1 (AWS-lifecycle 14-PR sequence): contract stubs for the future
    // option_chain module. Variants exist so downstream PR #8 can wire
    // its emit sites against stable identifiers. NO production emit sites
    // yet — pub-fn-test guards apply when consumers land.
    //
    // (PR-C 2026-05-26: CROSS-VERIFY-01..04 stubs retired along with the
    // entire cross_verify chain.)
    //
    // See:
    // - docs/architecture/option-chain-z-plus-heart-piece.md §8
    // -----------------------------------------------------------------------
    /// OPTION-CHAIN-01: REST fetch failed (network timeout / 5xx). Retried
    /// once with 2s delay; if still failing, the cycle marks that underlying
    /// stale. Severity::High. Strategy fail-closed if cache age > 60s.
    OptionChain01FetchFailed,
    /// OPTION-CHAIN-02: DH-904 backoff ladder exhausted (10s → 80s).
    /// Sustained rate-limit signal from Dhan. Severity::High.
    OptionChain02Dh904Exhausted,
    /// OPTION-CHAIN-03: response parse failure (malformed JSON, missing
    /// `oc` map, wrong types). Retry once; mark underlying stale on repeat.
    /// Severity::Medium.
    OptionChain03ParseFailed,
    /// OPTION-CHAIN-04: option-chain `last_price` of underlying disagrees
    /// with live WebSocket index LTP beyond 0.5% tolerance. Possible parser
    /// bug or Dhan-side data anomaly. Severity::Medium.
    OptionChain04VerifyFailedVsWs,
    /// OPTION-CHAIN-05: cache age > 60s during market hours. Strategy
    /// fail-closes (refuses to emit signals). Severity::Critical.
    /// Operator paged via 4-channel SNS fan-out.
    OptionChain05CacheStaleHaltStrategy,
    /// OPTION-CHAIN-06: previous cycle still running when next 50s tick
    /// fires. Mutex-guarded skip-next-cycle policy. Severity::High.
    OptionChain06CycleOverlapSkip,
    /// OPTION-CHAIN-07: Thursday 15:30 IST expiry rollover detected mid-day;
    /// expiry-list re-fetched and RAM cache rebuilt for new nearest expiry.
    /// Severity::Info — operational signal, no action required.
    OptionChain07ExpiryRollover,
    /// OPTION-CHAIN-08: HTTP 401 mid-cycle — JWT expired between cycles.
    /// Token-manager force-refresh; retry within same cycle. Severity::Critical
    /// because option chain freshness is strategy-blocking.
    OptionChain08TokenExpiredMidCycle,

    // -----------------------------------------------------------------------
    // PR #2.5 (AWS-lifecycle 14-PR sequence): Day OHLC tracker for IDX_I
    // indices. Stays with Ticker mode subscription (Dhan ignores Quote/Full
    // for IDX_I per documented restriction). Uses pre-market finalised close
    // as the 09:15:00 IST open price; tracks day high/low/close from LTP
    // stream. Volume intentionally NOT tracked (Dhan historical has none
    // for indices; BRUTEX doesn't use it).
    //
    // See:
    // - crates/trading/src/in_mem/day_ohlc_tracker.rs
    // - .claude/rules/project/index-day-ohlc-tracker-error-codes.md
    // -----------------------------------------------------------------------
    /// INDEX-OHLC-02: daily reset at IST midnight failed for one or more
    /// SIDs (e.g., parking_lot mutex panic, tracker handle dropped).
    /// Day high/low/close carry over to next trading day — incorrect.
    /// Severity::High. Operator inspects + manually resets via REST or
    /// restart.
    IndexOhlc02DailyResetFailed,
}

impl ErrorCode {
    /// Returns the canonical code string used in logs + rule documentation.
    ///
    /// The strings here must match the codes used in `.claude/rules/*.md`
    /// verbatim — the cross-ref integration test enforces this.
    #[must_use]
    pub const fn code_str(self) -> &'static str {
        match self {
            // Instrument P0 — PR #6b (2026-05-19): I-P0-01/02/04/05/06 retired
            Self::InstrumentP0ExpiryAtGate4 => "I-P0-03",
            // Instrument P1 — PR #6a (2026-05-19): I-P1-01 / I-P1-02 / I-P1-03 retired
            Self::InstrumentP1CompoundDedupKey => "I-P1-05",
            Self::InstrumentP1SegmentInTickDedup => "I-P1-06",
            Self::InstrumentP1SingleRowLifecycle => "I-P1-08",
            Self::InstrumentP1CrossSegmentCollision => "I-P1-11",
            // Instrument P2
            Self::InstrumentP2TradingDayGuard => "I-P2-02",
            // GAP-NET / GAP-SEC
            Self::GapNetIpMonitor => "GAP-NET-01",
            Self::GapSecApiAuth => "GAP-SEC-01",
            // OMS
            Self::OmsGapStateMachine => "OMS-GAP-01",
            Self::OmsGapReconciliation => "OMS-GAP-02",
            Self::OmsGapCircuitBreaker => "OMS-GAP-03",
            Self::OmsGapRateLimit => "OMS-GAP-04",
            Self::OmsGapIdempotency => "OMS-GAP-05",
            Self::OmsGapDryRunSafety => "OMS-GAP-06",
            // WebSocket
            Self::WsGapDisconnectClassification => "WS-GAP-01",
            Self::WsGapSubscriptionBatching => "WS-GAP-02",
            Self::WsGapConnectionState => "WS-GAP-03",
            // Risk
            Self::RiskGapPreTrade => "RISK-GAP-01",
            Self::RiskGapPositionPnl => "RISK-GAP-02",
            Self::RiskGapTickGap => "RISK-GAP-03",
            // Auth
            Self::AuthGapTokenExpiry => "AUTH-GAP-01",
            Self::AuthGapDisconnectTokenMap => "AUTH-GAP-02",
            // Storage
            Self::StorageGapTickDedupSegment => "STORAGE-GAP-01",
            Self::StorageGapF32F64Precision => "STORAGE-GAP-02",
            // Wave 1 (PR #393)
            Self::HotPath01SyncFsFailed => "HOT-PATH-01",
            Self::HotPath02WriterQueueDrop => "HOT-PATH-02",
            // PR #5 (2026-05-19): PHASE2-01 / PHASE2-02 retired.
            Self::PrevClose01IlpFailed => "PREVCLOSE-01",
            Self::PrevClose02FirstSeenInconsistency => "PREVCLOSE-02",
            // Phase 4b cleanup (2026-05-05): MOVERS-01/02/03 retired.
            Self::PrevOi01CacheEmptyAtBoot => "PREVOI-01",
            // Wave 2
            Self::WsGap04PostCloseSleep => "WS-GAP-04",
            Self::WsGap05PoolRespawn => "WS-GAP-05",
            Self::WsGap06TickGapSummary => "WS-GAP-06",
            Self::WsGap07LiveChannelClosed => "WS-GAP-07",
            Self::DiskWatcher01Respawned => "DISK-WATCHER-01",
            Self::WsSpill01WriterRespawn => "WS-SPILL-01",
            Self::WsSpill02FrameDropped => "WS-SPILL-02",
            Self::AuthGap03TokenForceRenewedOnWake => "AUTH-GAP-03",
            Self::Boot01QuestDbSlow => "BOOT-01",
            Self::Boot02DeadlineExceeded => "BOOT-02",
            Self::Boot03ClockSkewExceeded => "BOOT-03",
            // PR #5 (2026-05-19): AUDIT-01 retired with phase2_audit_persistence.rs.
            Self::Audit02DepthRebalanceWriteFailed => "AUDIT-02",
            Self::Audit03WsReconnectWriteFailed => "AUDIT-03",
            Self::Audit04BootWriteFailed => "AUDIT-04",
            Self::Audit05SelftestWriteFailed => "AUDIT-05",
            Self::Audit06OrderWriteFailed => "AUDIT-06",
            Self::StorageGap03AuditWriteFailed => "STORAGE-GAP-03",
            Self::StorageGap04S3ArchiveFailed => "STORAGE-GAP-04",
            // Wave 3 — Telegram dispatcher (Item 11)
            Self::Telegram01Dropped => "TELEGRAM-01",
            Self::Telegram02CoalescerStateInconsistency => "TELEGRAM-02",
            // Wave 3-C — market-open self-test (Item 12)
            Self::Selftest01Passed => "SELFTEST-01",
            Self::Selftest02Failed => "SELFTEST-02",
            // Wave 3-D — composite real-time guarantee score (Item 13)
            Self::Slo01Healthy => "SLO-01",
            Self::Slo02Degraded => "SLO-02",
            // Dhan Trading API
            Self::Dh901InvalidAuth => "DH-901",
            Self::Dh902NoApiAccess => "DH-902",
            Self::Dh903AccountIssue => "DH-903",
            Self::Dh904RateLimit => "DH-904",
            Self::Dh905InputException => "DH-905",
            Self::Dh906OrderError => "DH-906",
            Self::Dh907DataError => "DH-907",
            Self::Dh908InternalServerError => "DH-908",
            Self::Dh909NetworkError => "DH-909",
            Self::Dh910Other => "DH-910",
            // Data API
            Self::Data800InternalServerError => "DATA-800",
            Self::Data804InstrumentsExceedLimit => "DATA-804",
            Self::Data805TooManyConnections => "DATA-805",
            Self::Data806NotSubscribed => "DATA-806",
            Self::Data807TokenExpired => "DATA-807",
            Self::Data808AuthFailed => "DATA-808",
            Self::Data809TokenInvalid => "DATA-809",
            Self::Data810ClientIdInvalid => "DATA-810",
            Self::Data811InvalidExpiry => "DATA-811",
            Self::Data812InvalidDateFormat => "DATA-812",
            Self::Data813InvalidSecurityId => "DATA-813",
            Self::Data814InvalidRequest => "DATA-814",
            // Wave 5 Item 13 — prev-close routing
            Self::PrevClose03BootRoutingAssertion => "PREVCLOSE-03",
            // F2 (Wave-5 #504e follow-up) — PrevDayCache boot loader
            Self::PrevClose04CacheEmptyAtBoot => "PREVCLOSE-04",
            // Wave 5 Item 26 L1 — volume cumulative-monotonicity guard
            Self::Volume01MonotonicityBreach => "VOLUME-MONO-01",
            // PR #5 (2026-05-19): PHASE2-READY-01 retired with phase2_readiness_check.
            // Wave 6 — Multi-TF aggregator
            Self::AggregatorDrop01 => "AGGREGATOR-DROP-01",
            Self::AggregatorLate01 => "AGGREGATOR-LATE-01",
            Self::AggregatorLag01TickLagDropped => "AGGREGATOR-LAG-01",
            Self::AggregatorSeal01IlpFailed => "AGGREGATOR-SEAL-01",
            Self::AggregatorHb01Heartbeat => "AGGREGATOR-HB-01",
            Self::Boundary01CatchupSeal => "BOUNDARY-01",
            Self::Resilience01DualInstanceDetected => "RESILIENCE-01",
            Self::OrphanPosition01Detected => "ORPHAN-POSITION-01",
            Self::BarMismatch01CorrectedFromHistorical => "BAR-MISMATCH-01",
            Self::BarMismatch02CrossCheckInconclusive => "BAR-MISMATCH-02",
            Self::BarMismatch03CrossCheckFailed => "BAR-MISMATCH-03",
            Self::InstrFetch01CsvHardFailed => "INSTR-FETCH-01",
            Self::InstrFetch02SchemaValidationFailed => "INSTR-FETCH-02",
            Self::InstrFetch03DanglingReferences => "INSTR-FETCH-03",
            Self::InstrFetch04UniverseSizeOutOfBounds => "INSTR-FETCH-04",
            Self::NtmConstituency01SourceDegraded => "NTM-CONSTITUENCY-01",
            // Operator 2026-06-02: post-market 1-minute cross-verification
            Self::CrossVerify1m01MismatchFound => "CROSS-VERIFY-1M-01",
            Self::CrossVerify1m02FetchDegraded => "CROSS-VERIFY-1M-02",
            // PR #1 (AWS-lifecycle): option_chain stubs
            Self::OptionChain01FetchFailed => "OPTION-CHAIN-01",
            Self::OptionChain02Dh904Exhausted => "OPTION-CHAIN-02",
            Self::OptionChain03ParseFailed => "OPTION-CHAIN-03",
            Self::OptionChain04VerifyFailedVsWs => "OPTION-CHAIN-04",
            Self::OptionChain05CacheStaleHaltStrategy => "OPTION-CHAIN-05",
            Self::OptionChain06CycleOverlapSkip => "OPTION-CHAIN-06",
            Self::OptionChain07ExpiryRollover => "OPTION-CHAIN-07",
            Self::OptionChain08TokenExpiredMidCycle => "OPTION-CHAIN-08",
            // Day OHLC tracker for IDX_I
            Self::IndexOhlc02DailyResetFailed => "INDEX-OHLC-02",
        }
    }

    /// Returns the severity this code should carry when emitted.
    #[must_use]
    pub const fn severity(self) -> Severity {
        match self {
            // Critical: auth / account / global connection cap
            Self::Dh901InvalidAuth
            | Self::Dh902NoApiAccess
            | Self::Dh903AccountIssue
            | Self::Data805TooManyConnections
            | Self::Data808AuthFailed
            | Self::Data809TokenInvalid
            | Self::Data810ClientIdInvalid
            | Self::AuthGapTokenExpiry
            | Self::AuthGapDisconnectTokenMap
            | Self::OmsGapCircuitBreaker
            | Self::OmsGapDryRunSafety
            | Self::Boot02DeadlineExceeded
            | Self::Boot03ClockSkewExceeded
            | Self::Selftest02Failed
            | Self::PrevClose03BootRoutingAssertion
            | Self::AggregatorDrop01
            | Self::Resilience01DualInstanceDetected
            | Self::OrphanPosition01Detected
            | Self::BarMismatch01CorrectedFromHistorical
            | Self::BarMismatch02CrossCheckInconclusive
            | Self::BarMismatch03CrossCheckFailed
            // PR #1 (AWS-lifecycle) — Critical option-chain
            | Self::OptionChain05CacheStaleHaltStrategy
            | Self::OptionChain08TokenExpiredMidCycle
            // Sub-PR #9 — all 4 INSTR-FETCH codes are HALT-class
            | Self::InstrFetch01CsvHardFailed
            | Self::InstrFetch02SchemaValidationFailed
            | Self::InstrFetch03DanglingReferences
            | Self::InstrFetch04UniverseSizeOutOfBounds
            | Self::NtmConstituency01SourceDegraded
            // WS-SPILL-02 — durable frame dropped (writer dead at append instant)
            | Self::WsSpill02FrameDropped => Severity::Critical,
            // Info: positive-ping / lifecycle confirmations
            Self::Selftest01Passed
            | Self::Slo01Healthy
            | Self::AggregatorHb01Heartbeat
            // PR #1 (AWS-lifecycle) — Info option-chain rollover signal
            | Self::OptionChain07ExpiryRollover => Severity::Info,
            // High: composite SLO degradation summary signal
            Self::Slo02Degraded => Severity::High,
            // High: regulatory / order / risk / rate-limit
            Self::Dh904RateLimit
            | Self::Dh905InputException
            | Self::Dh906OrderError
            | Self::OmsGapStateMachine
            | Self::OmsGapReconciliation
            | Self::OmsGapRateLimit
            | Self::OmsGapIdempotency
            | Self::RiskGapPreTrade
            | Self::RiskGapPositionPnl
            | Self::InstrumentP0ExpiryAtGate4
            | Self::Data807TokenExpired
            | Self::Boot01QuestDbSlow
            | Self::Volume01MonotonicityBreach
            | Self::AggregatorLate01
            | Self::AggregatorLag01TickLagDropped
            // PR #1 (AWS-lifecycle) — High option-chain
            | Self::OptionChain01FetchFailed
            | Self::OptionChain02Dh904Exhausted
            | Self::OptionChain06CycleOverlapSkip
            // PR #2.5 — INDEX-OHLC-02 is High (carry-over wrong but recoverable)
            | Self::IndexOhlc02DailyResetFailed
            // Operator 2026-06-02 — post-market 1m cross-verify (both High)
            | Self::CrossVerify1m01MismatchFound
            | Self::CrossVerify1m02FetchDegraded
            // WS-GAP-07 — live frame channel closed (tick consumer died)
            | Self::WsGap07LiveChannelClosed
            // WS-SPILL-01 — WAL writer respawned (flapping writer = disk dying)
            | Self::WsSpill01WriterRespawn => Severity::High,
            // Medium: data pipeline correctness
            // PR #6b (2026-05-19): I-P0-01/02/04/05 retired with their modules.
            Self::InstrumentP1CrossSegmentCollision
            | Self::InstrumentP1SegmentInTickDedup
            | Self::InstrumentP1CompoundDedupKey
            | Self::InstrumentP1SingleRowLifecycle
            | Self::WsGapDisconnectClassification
            | Self::WsGapSubscriptionBatching
            | Self::WsGapConnectionState
            | Self::RiskGapTickGap
            | Self::StorageGapTickDedupSegment
            | Self::StorageGapF32F64Precision
            | Self::GapNetIpMonitor
            | Self::GapSecApiAuth
            | Self::Dh907DataError
            | Self::Dh908InternalServerError
            | Self::Dh909NetworkError
            | Self::Data800InternalServerError
            | Self::Data804InstrumentsExceedLimit
            | Self::Data806NotSubscribed
            | Self::Data811InvalidExpiry
            | Self::Data812InvalidDateFormat
            | Self::Data813InvalidSecurityId
            | Self::Data814InvalidRequest
            | Self::HotPath01SyncFsFailed
            | Self::PrevClose01IlpFailed
            | Self::PrevClose02FirstSeenInconsistency
            | Self::PrevOi01CacheEmptyAtBoot
            | Self::PrevClose04CacheEmptyAtBoot
            | Self::WsGap06TickGapSummary
            | Self::Audit02DepthRebalanceWriteFailed
            | Self::Audit03WsReconnectWriteFailed
            | Self::Audit04BootWriteFailed
            | Self::Audit05SelftestWriteFailed
            | Self::Audit06OrderWriteFailed
            | Self::StorageGap03AuditWriteFailed
            | Self::StorageGap04S3ArchiveFailed
            | Self::Telegram01Dropped
            | Self::AggregatorSeal01IlpFailed
            | Self::Boundary01CatchupSeal
            // PR #1 (AWS-lifecycle) — Medium option-chain
            | Self::OptionChain03ParseFailed
            | Self::OptionChain04VerifyFailedVsWs => Severity::Medium,
            // Low: trading-day / Dhan other
            // PR #6a (2026-05-19): I-P1-01 (DailyScheduler) + I-P1-02 (DeltaFieldCoverage) retired
            Self::InstrumentP2TradingDayGuard
            | Self::Dh910Other
            | Self::HotPath02WriterQueueDrop
            | Self::WsGap04PostCloseSleep
            | Self::WsGap05PoolRespawn
            | Self::DiskWatcher01Respawned
            | Self::AuthGap03TokenForceRenewedOnWake
            | Self::Telegram02CoalescerStateInconsistency => Severity::Low,
        }
    }

    /// Returns the canonical runbook path inside the repo (relative) that
    /// documents how to triage this error.
    ///
    /// Returning a path rather than a URL makes this usable by Claude Code
    /// sessions that don't necessarily have HTTP egress.
    #[must_use]
    pub const fn runbook_path(self) -> &'static str {
        match self {
            // PR #6b (2026-05-19): I-P0-01/02/04/05/06 retired with their modules.
            Self::InstrumentP0ExpiryAtGate4
            // PR #6a (2026-05-19): I-P1-01 / I-P1-02 / I-P1-03 retired
            | Self::InstrumentP1CompoundDedupKey
            | Self::InstrumentP1SegmentInTickDedup
            | Self::InstrumentP1SingleRowLifecycle
            | Self::InstrumentP1CrossSegmentCollision
            | Self::InstrumentP2TradingDayGuard
            | Self::GapNetIpMonitor
            | Self::GapSecApiAuth
            | Self::OmsGapStateMachine
            | Self::OmsGapReconciliation
            | Self::OmsGapCircuitBreaker
            | Self::OmsGapRateLimit
            | Self::OmsGapIdempotency
            | Self::OmsGapDryRunSafety
            | Self::WsGapDisconnectClassification
            | Self::WsGapSubscriptionBatching
            | Self::WsGapConnectionState
            | Self::RiskGapPreTrade
            | Self::RiskGapPositionPnl
            | Self::RiskGapTickGap
            | Self::AuthGapTokenExpiry
            | Self::AuthGapDisconnectTokenMap
            | Self::StorageGapTickDedupSegment
            | Self::StorageGapF32F64Precision => ".claude/rules/project/gap-enforcement.md",
            Self::HotPath01SyncFsFailed
            | Self::HotPath02WriterQueueDrop
            | Self::PrevClose01IlpFailed
            | Self::PrevClose02FirstSeenInconsistency
            | Self::PrevOi01CacheEmptyAtBoot
            | Self::PrevClose04CacheEmptyAtBoot => ".claude/rules/project/wave-1-error-codes.md",
            Self::WsSpill01WriterRespawn | Self::WsSpill02FrameDropped => {
                ".claude/rules/project/ws-frame-spill-error-codes.md"
            }
            Self::WsGap04PostCloseSleep
            | Self::WsGap05PoolRespawn
            | Self::WsGap06TickGapSummary
            | Self::WsGap07LiveChannelClosed
            | Self::DiskWatcher01Respawned
            | Self::AuthGap03TokenForceRenewedOnWake
            | Self::Boot01QuestDbSlow
            | Self::Boot02DeadlineExceeded
            | Self::Audit02DepthRebalanceWriteFailed
            | Self::Audit03WsReconnectWriteFailed
            | Self::Audit04BootWriteFailed
            | Self::Audit05SelftestWriteFailed
            | Self::Audit06OrderWriteFailed
            | Self::StorageGap03AuditWriteFailed
            | Self::StorageGap04S3ArchiveFailed => ".claude/rules/project/wave-2-error-codes.md",
            Self::Boot03ClockSkewExceeded => ".claude/rules/project/wave-2-c-error-codes.md",
            Self::Telegram01Dropped | Self::Telegram02CoalescerStateInconsistency => {
                ".claude/rules/project/wave-3-error-codes.md"
            }
            Self::Selftest01Passed | Self::Selftest02Failed => {
                ".claude/rules/project/wave-3-c-error-codes.md"
            }
            Self::Slo01Healthy | Self::Slo02Degraded => {
                ".claude/rules/project/wave-3-d-error-codes.md"
            }
            Self::Dh901InvalidAuth
            | Self::Dh902NoApiAccess
            | Self::Dh903AccountIssue
            | Self::Dh904RateLimit
            | Self::Dh905InputException
            | Self::Dh906OrderError
            | Self::Dh907DataError
            | Self::Dh908InternalServerError
            | Self::Dh909NetworkError
            | Self::Dh910Other => ".claude/rules/dhan/annexure-enums.md",
            Self::Data800InternalServerError
            | Self::Data804InstrumentsExceedLimit
            | Self::Data805TooManyConnections
            | Self::Data806NotSubscribed
            | Self::Data807TokenExpired
            | Self::Data808AuthFailed
            | Self::Data809TokenInvalid
            | Self::Data810ClientIdInvalid
            | Self::Data811InvalidExpiry
            | Self::Data812InvalidDateFormat
            | Self::Data813InvalidSecurityId
            | Self::Data814InvalidRequest => ".claude/rules/dhan/annexure-enums.md",
            Self::PrevClose03BootRoutingAssertion
            | Self::Volume01MonotonicityBreach => ".claude/rules/project/wave-5-error-codes.md",
            Self::AggregatorDrop01
            | Self::AggregatorLate01
            | Self::AggregatorLag01TickLagDropped
            | Self::AggregatorSeal01IlpFailed
            | Self::AggregatorHb01Heartbeat
            | Self::Boundary01CatchupSeal => ".claude/rules/project/wave-6-error-codes.md",
            Self::Resilience01DualInstanceDetected => ".claude/rules/project/wave-4-error-codes.md",
            Self::OrphanPosition01Detected => {
                ".claude/rules/project/phase-0-item-20-error-codes.md"
            }
            Self::BarMismatch01CorrectedFromHistorical
            | Self::BarMismatch02CrossCheckInconclusive
            | Self::BarMismatch03CrossCheckFailed => {
                ".claude/rules/project/phase-0-items-15-28-29-error-codes.md"
            }
            // Sub-PR #9 of 2026-05-27 daily-universe expansion
            Self::InstrFetch01CsvHardFailed
            | Self::InstrFetch02SchemaValidationFailed
            | Self::InstrFetch03DanglingReferences
            | Self::InstrFetch04UniverseSizeOutOfBounds => {
                ".claude/rules/project/daily-universe-instr-fetch-error-codes.md"
            }
            // Sub-PR #10 of 2026-06-06 NTM turn-on (§31)
            Self::NtmConstituency01SourceDegraded => {
                ".claude/rules/project/ntm-constituency-error-codes.md"
            }
            // Operator 2026-06-02: post-market 1-minute cross-verification
            Self::CrossVerify1m01MismatchFound | Self::CrossVerify1m02FetchDegraded => {
                ".claude/rules/project/cross-verify-1m-error-codes.md"
            }
            // PR #1 (AWS-lifecycle): option_chain stubs
            Self::OptionChain01FetchFailed
            | Self::OptionChain02Dh904Exhausted
            | Self::OptionChain03ParseFailed
            | Self::OptionChain04VerifyFailedVsWs
            | Self::OptionChain05CacheStaleHaltStrategy
            | Self::OptionChain06CycleOverlapSkip
            | Self::OptionChain07ExpiryRollover
            | Self::OptionChain08TokenExpiredMidCycle => {
                ".claude/rules/project/option-chain-cross-verify-error-codes.md"
            }
            // Day OHLC tracker for IDX_I
            Self::IndexOhlc02DailyResetFailed => {
                ".claude/rules/project/index-day-ohlc-tracker-error-codes.md"
            }
        }
    }

    /// True when Claude Code auto-triage should attempt a fix before
    /// escalating. False means operator action is required — the auto-triage
    /// daemon will log a summary and stop.
    ///
    /// Conservative default: Critical errors are NEVER auto-actioned.
    #[must_use]
    pub const fn is_auto_triage_safe(self) -> bool {
        !matches!(self.severity(), Severity::Critical)
    }

    /// All known variants, in declaration order.
    ///
    /// Used by the cross-ref integration test and by the future error-code
    /// catalogue dashboard. Update this list whenever a new variant is added —
    /// the `test_all_variants_list_is_exhaustive` unit test will fail
    /// otherwise (compile-time via non_exhaustive pattern match).
    #[must_use]
    pub fn all() -> &'static [ErrorCode] {
        &[
            // PR #6b (2026-05-19): I-P0-01/02/04/05/06 retired with their modules.
            Self::InstrumentP0ExpiryAtGate4,
            // PR #6a (2026-05-19): I-P1-01 / I-P1-02 / I-P1-03 retired
            Self::InstrumentP1CompoundDedupKey,
            Self::InstrumentP1SegmentInTickDedup,
            Self::InstrumentP1SingleRowLifecycle,
            Self::InstrumentP1CrossSegmentCollision,
            Self::InstrumentP2TradingDayGuard,
            Self::GapNetIpMonitor,
            Self::GapSecApiAuth,
            Self::OmsGapStateMachine,
            Self::OmsGapReconciliation,
            Self::OmsGapCircuitBreaker,
            Self::OmsGapRateLimit,
            Self::OmsGapIdempotency,
            Self::OmsGapDryRunSafety,
            Self::WsGapDisconnectClassification,
            Self::WsGapSubscriptionBatching,
            Self::WsGapConnectionState,
            Self::RiskGapPreTrade,
            Self::RiskGapPositionPnl,
            Self::RiskGapTickGap,
            Self::AuthGapTokenExpiry,
            Self::AuthGapDisconnectTokenMap,
            Self::StorageGapTickDedupSegment,
            Self::StorageGapF32F64Precision,
            Self::Dh901InvalidAuth,
            Self::Dh902NoApiAccess,
            Self::Dh903AccountIssue,
            Self::Dh904RateLimit,
            Self::Dh905InputException,
            Self::Dh906OrderError,
            Self::Dh907DataError,
            Self::Dh908InternalServerError,
            Self::Dh909NetworkError,
            Self::Dh910Other,
            Self::Data800InternalServerError,
            Self::Data804InstrumentsExceedLimit,
            Self::Data805TooManyConnections,
            Self::Data806NotSubscribed,
            Self::Data807TokenExpired,
            Self::Data808AuthFailed,
            Self::Data809TokenInvalid,
            Self::Data810ClientIdInvalid,
            Self::Data811InvalidExpiry,
            Self::Data812InvalidDateFormat,
            Self::Data813InvalidSecurityId,
            Self::Data814InvalidRequest,
            Self::HotPath01SyncFsFailed,
            Self::HotPath02WriterQueueDrop,
            // PR #5 (2026-05-19): Phase201DispatchFailed + Phase202EmitGuardDropped retired.
            Self::PrevClose01IlpFailed,
            Self::PrevClose02FirstSeenInconsistency,
            Self::PrevOi01CacheEmptyAtBoot,
            Self::PrevClose04CacheEmptyAtBoot,
            Self::WsGap04PostCloseSleep,
            Self::WsGap05PoolRespawn,
            Self::WsGap06TickGapSummary,
            Self::WsGap07LiveChannelClosed,
            Self::DiskWatcher01Respawned,
            Self::WsSpill01WriterRespawn,
            Self::WsSpill02FrameDropped,
            Self::AuthGap03TokenForceRenewedOnWake,
            Self::Boot01QuestDbSlow,
            Self::Boot02DeadlineExceeded,
            Self::Boot03ClockSkewExceeded,
            // PR #5 (2026-05-19): Audit01Phase2WriteFailed retired.
            Self::Audit02DepthRebalanceWriteFailed,
            Self::Audit03WsReconnectWriteFailed,
            Self::Audit04BootWriteFailed,
            Self::Audit05SelftestWriteFailed,
            Self::Audit06OrderWriteFailed,
            Self::StorageGap03AuditWriteFailed,
            Self::StorageGap04S3ArchiveFailed,
            Self::Telegram01Dropped,
            Self::Telegram02CoalescerStateInconsistency,
            Self::Selftest01Passed,
            Self::Selftest02Failed,
            Self::Slo01Healthy,
            Self::Slo02Degraded,
            Self::PrevClose03BootRoutingAssertion,
            Self::Volume01MonotonicityBreach,
            // PR #5 (2026-05-19): Phase2Ready01PreflightFailed retired.
            // Wave 6 — Multi-TF aggregator (Sub-PR #1)
            Self::AggregatorDrop01,
            Self::AggregatorLate01,
            Self::AggregatorLag01TickLagDropped,
            Self::AggregatorSeal01IlpFailed,
            Self::AggregatorHb01Heartbeat,
            Self::Boundary01CatchupSeal,
            // Wave 4 — Item 19 dual-instance lock
            Self::Resilience01DualInstanceDetected,
            // Phase 0 Item 20 — orphan position 15:25 IST watchdog
            Self::OrphanPosition01Detected,
            // Phase 0 Items 15+28+29 — 09:16:05 IST post-open cross-check
            Self::BarMismatch01CorrectedFromHistorical,
            Self::BarMismatch02CrossCheckInconclusive,
            Self::BarMismatch03CrossCheckFailed,
            // Sub-PR #9 of 2026-05-27 daily-universe expansion
            Self::InstrFetch01CsvHardFailed,
            Self::InstrFetch02SchemaValidationFailed,
            Self::InstrFetch03DanglingReferences,
            Self::InstrFetch04UniverseSizeOutOfBounds,
            Self::NtmConstituency01SourceDegraded,
            // Operator 2026-06-02: post-market 1-minute cross-verification
            Self::CrossVerify1m01MismatchFound,
            Self::CrossVerify1m02FetchDegraded,
            // PR #1 (AWS-lifecycle 14-PR sequence) — option_chain stubs
            Self::OptionChain01FetchFailed,
            Self::OptionChain02Dh904Exhausted,
            Self::OptionChain03ParseFailed,
            Self::OptionChain04VerifyFailedVsWs,
            Self::OptionChain05CacheStaleHaltStrategy,
            Self::OptionChain06CycleOverlapSkip,
            Self::OptionChain07ExpiryRollover,
            Self::OptionChain08TokenExpiredMidCycle,
            // Day OHLC tracker for IDX_I (Ticker mode)
            Self::IndexOhlc02DailyResetFailed,
        ]
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code_str())
    }
}

/// Error returned when parsing an unknown code string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownErrorCode(pub String);

impl fmt::Display for UnknownErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown error code: {}", self.0)
    }
}

impl std::error::Error for UnknownErrorCode {}

impl FromStr for ErrorCode {
    type Err = UnknownErrorCode;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for code in Self::all() {
            if code.code_str() == s {
                return Ok(*code);
            }
        }
        Err(UnknownErrorCode(s.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Unit tests — invariants that must hold at all times
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_all_variants_have_unique_code_str() {
        let mut seen: HashSet<&'static str> = HashSet::new();
        for code in ErrorCode::all() {
            assert!(
                seen.insert(code.code_str()),
                "duplicate code_str: {}",
                code.code_str()
            );
        }
    }

    #[test]
    fn test_code_str_roundtrip_via_from_str() {
        for code in ErrorCode::all() {
            let parsed: ErrorCode = code.code_str().parse().unwrap_or_else(|e| {
                panic!("FromStr failed to roundtrip for {}: {e:?}", code.code_str())
            });
            assert_eq!(parsed, *code);
        }
    }

    #[test]
    fn test_from_str_rejects_unknown_code() {
        let result: Result<ErrorCode, _> = "BOGUS-999".parse();
        assert!(result.is_err());
    }

    #[test]
    fn test_every_variant_has_non_empty_runbook_path() {
        for code in ErrorCode::all() {
            let path = code.runbook_path();
            assert!(
                !path.is_empty(),
                "runbook_path empty for {}",
                code.code_str()
            );
            assert!(
                path.starts_with(".claude/"),
                "runbook_path for {} should point at .claude/: got {}",
                code.code_str(),
                path
            );
        }
    }

    #[test]
    fn test_severity_ordering() {
        assert!(Severity::Info < Severity::Low);
        assert!(Severity::Low < Severity::Medium);
        assert!(Severity::Medium < Severity::High);
        assert!(Severity::High < Severity::Critical);
    }

    #[test]
    fn test_severity_as_str_is_stable() {
        assert_eq!(Severity::Info.as_str(), "info");
        assert_eq!(Severity::Low.as_str(), "low");
        assert_eq!(Severity::Medium.as_str(), "medium");
        assert_eq!(Severity::High.as_str(), "high");
        assert_eq!(Severity::Critical.as_str(), "critical");
    }

    #[test]
    fn test_critical_codes_never_auto_triage() {
        for code in ErrorCode::all() {
            if code.severity() == Severity::Critical {
                assert!(
                    !code.is_auto_triage_safe(),
                    "{} is Critical but is_auto_triage_safe returned true",
                    code.code_str()
                );
            }
        }
    }

    #[test]
    fn test_every_severity_is_assigned_to_at_least_one_code() {
        let mut seen = HashSet::new();
        for code in ErrorCode::all() {
            seen.insert(code.severity());
        }
        // Info is allowed to be unused (no code is purely informational in
        // the current catalogue); the remaining four must all appear.
        assert!(seen.contains(&Severity::Low));
        assert!(seen.contains(&Severity::Medium));
        assert!(seen.contains(&Severity::High));
        assert!(seen.contains(&Severity::Critical));
    }

    #[test]
    fn test_display_matches_code_str() {
        for code in ErrorCode::all() {
            assert_eq!(code.to_string(), code.code_str());
        }
    }

    #[test]
    fn test_all_list_length_matches_catalogue_size() {
        // If this fails, the `all()` list was not updated when a new variant
        // was added. Keep this count in sync with the enum.
        // 2026-04-27 (Wave 1): bumped 54 -> 62 for 8 new variants
        // (HOT-PATH-01/02, PHASE2-01/02, PREVCLOSE-01/02, MOVERS-01/02).
        // 2026-04-27 (Wave 2): bumped 62 -> 76 for 14 new variants
        // (WS-GAP-04/05/06, AUTH-GAP-03, BOOT-01/02, AUDIT-01..06,
        // STORAGE-GAP-03/04).
        // 2026-04-27 (Wave 2-C Item 7.3): bumped 76 -> 77 for BOOT-03
        // (clock-skew exceeded — HALTING).
        // 2026-04-28 (Wave 3-A Item 10): bumped 77 -> 78 for MOVERS-03
        // (pre-open movers persistence failed).
        // 2026-04-28 (Wave 3-B Item 11): bumped 78 -> 80 for TELEGRAM-01/02
        // (Telegram bucket-coalescer hardening).
        // 2026-04-28 (Wave 3-C Item 12): bumped 80 -> 82 for SELFTEST-01
        // (passed) + SELFTEST-02 (failed) — market-open self-test.
        // 2026-04-28 (Wave 3-D Item 13): bumped 82 -> 84 for SLO-01
        // (healthy recovery) + SLO-02 (degraded/critical) — composite
        // real-time guarantee score.
        // 2026-04-28 (depth-200 SELF token): bumped 84 -> 87 for
        // 2026-04-28 (Phase 7 of v3 plan): bumped 87 -> 89 for
        // DEPTH-DYN-01/02 — depth-20 dynamic top-150 selector
        // promoted from RESERVED to defined.
        // 2026-04-28 (Phase 11 of v3 plan): bumped 89 -> 92 for
        // MOVERS-22TF-01/02/03 — movers 22-timeframe persistence,
        // scheduler, and universe-drift codes.
        // 2026-05-01 (Wave 5 Item 9): bumped 92 -> 96 for
        // CORE-PIN-01/02 (Tokio worker pinning) +
        // DEPTH-20-DYN-03 (top-50 depth-20 selector) +
        // DEPTH-200-DYN-01 (top-5 depth-200 selector).
        // 2026-05-01 (Wave 5 Item 13): bumped 96 -> 97 for
        // PREVCLOSE-03 (boot-time prev-close routing assertion).
        // 2026-05-01 (Wave 5 Item 26 L1): bumped 97 -> 98 for
        // VOLUME-MONO-01 (cumulative-monotonicity breach).
        // 2026-05-01 (movers cleanup): bumped 98 -> 95 — removed
        // MOVERS-22TF-01/02/03 along with the dead 22-tf pipeline.
        // 2026-05-02 (depth-200 SELF token retired per Dhan Ticket
        // #5610706): bumped 95 -> 92 — removed DEPTH200-AUTH-01/02/03
        // along with the SELF-token manager.
        // 2026-05-02 (PR-B): bumped 92 -> 93 for DEPTH200-SMOKE-01
        // (boot-time depth-200 smoke test no-frames Critical signal).
        // 2026-05-02 (PR-G): bumped 93 -> 94 for PHASE2-READY-01
        // (09:13:01 IST forward-looking pre-flight readiness check).
        // 2026-05-03 (PR #450 commit 8b adversarial-review HIGH H1 fix):
        // bumped 94 -> 95 for PREVOI-01 (prev_oi cache empty at boot
        // WARN — typed enum replaces ad-hoc `code = "PREVOI-01"` string).
        // 2026-05-05 (Phase 4b cleanup): bumped 95 -> 92 — retired
        // MOVERS-01/02/03 alongside StockMoversWriter +
        // OptionMoversWriter deletion in PR #494.
        // 2026-05-08 (F2 / Wave-5 #504e follow-up): bumped 92 -> 93
        // for PREVCLOSE-04 (PrevDayCache boot loader empty / failed
        // — degraded cascade pct-stamping fallback signal).
        // 2026-05-10 (Wave 6 Sub-PR #1 — multi-TF aggregator): bumped
        // 93 -> 99 for AGGREGATOR-DROP-01, AGGREGATOR-LATE-01,
        // AGGREGATOR-SEAL-01, AGGREGATOR-HB-01, BOUNDARY-01,
        // AGGREGATOR-AUDIT-01.
        // 2026-05-17 (Phase 0 Items 8+9 — gap-fill scheduler): bumped
        // 100 -> 104 for GAP-FILL-01/02/03/04.
        // 2026-05-18 (Phase 0 Item 20 — orphan position watchdog):
        // bumped 104 -> 105 for ORPHAN-POSITION-01.
        // 2026-05-18 (Phase 0 Items 15+28+29 — post-open cross-check):
        // bumped 105 -> 108 for BAR-MISMATCH-01/02/03.
        // 2026-05-18 (PR #1 of AWS-lifecycle 14-PR sequence — contract stubs):
        // bumped 108 -> 120 for OPTION-CHAIN-01..08 + CROSS-VERIFY-01..04
        // (CROSS-VERIFY-* retired in PR-C 2026-05-26).
        // 2026-05-18 (PR #2.5 of AWS-lifecycle — Day OHLC tracker for IDX_I):
        // bumped 120 -> 122 for INDEX-OHLC-01 + INDEX-OHLC-02.
        // 2026-05-19 (PR #4 of AWS-lifecycle — depth pipelines retirement):
        // bumped 122 -> 117 by removing DEPTH-DYN-01/02, DEPTH-20-DYN-03,
        // DEPTH-200-DYN-01, DEPTH200-SMOKE-01 (depth feeds retired
        // entirely; only main-feed + order-update WSes remain).
        // 2026-05-19 (PR #5 of AWS-lifecycle — Phase 2 dispatcher retirement):
        // bumped 117 -> 113 by removing PHASE2-01, PHASE2-02, PHASE2-READY-01,
        // AUDIT-01 (Phase 2 stock-F&O dispatcher chain retired alongside
        // phase2_audit_persistence under operator-locked 4-IDX_I scope).
        // 2026-05-19 (PR #6a of AWS-lifecycle — universe support files retirement):
        // bumped 113 -> 110 by removing I-P1-01 (DailyScheduler), I-P1-02
        // (DeltaFieldCoverage), I-P1-03 (SecurityIdReuse) — daily_scheduler
        // and delta_detector modules deleted under 4-IDX_I LOCKED_UNIVERSE.
        // 2026-05-19 (PR #6b of AWS-lifecycle — universe machinery deletion):
        // bumped 110 -> 105 by removing I-P0-01 (DuplicateSecurityId),
        // I-P0-02 (CountConsistency), I-P0-04 (CachePersistence),
        // I-P0-05 (S3Backup), I-P0-06 (EmergencyDownload) — universe_builder
        // + validation + binary_cache + s3_backup + instrument_loader modules
        // deleted under 4-IDX_I LOCKED_UNIVERSE.
        // 2026-05-20 (#T2a — QuestDB table cleanup): bumped 105 -> 104 by
        // removing AGGREGATOR-AUDIT-01 (aggregator_seal_audit table dropped).
        // 2026-05-25 (Phase B1 deletion): bumped 104 -> 102 by removing
        // CORE-PIN-01 (CorePin01PinningFailedAtBoot) + CORE-PIN-02
        // (CorePin02WorkerDrifted) — core_pinning.rs module deleted under
        // LOCKED 4-SID / t4g.medium 2-vCPU scope (no 4-core pinning to do).
        // 2026-05-26 (PR-A — pre-open buffer + Dhan historical removal):
        // bumped 102 -> 101 by removing INDEX-OHLC-01 (preopen buffer
        // empty at 09:15:00 IST) — pre-open buffer module deleted; day_open
        // is now the first observed live WebSocket tick after midnight reset.
        // 2026-05-26 (PR-B — gap_fill scheduler removal): bumped 101 -> 97
        // by removing GAP-FILL-01/02/03/04 — gap_fill_scheduler + planner +
        // disconnect_event + last_seen_ltt_cache modules deleted alongside
        // Dhan historical fetch chain.
        // 2026-05-26 (PR-C — cross_verify chain removal): bumped 97 -> 93
        // by removing CROSS-VERIFY-01/02/03/04 — cross_verify + post_open_cross_check
        // + post_market_fetch_window + cross_verify_scheduler modules deleted
        // alongside Dhan historical fetch chain.
        // 2026-06-02 (operator post-market 1-minute cross-verification):
        // bumped 97 -> 99 by adding CROSS-VERIFY-1M-01 (mismatch found) +
        // CROSS-VERIFY-1M-02 (intraday fetch degraded).
        // 2026-06-03 (zero-tick-loss PR-2 — G2): bumped 99 -> 100 for
        // WS-GAP-07 (live frame channel closed — tick consumer died).
        // 2026-06-03 (zero-tick-loss PR-5 — G3): bumped 100 -> 101 for
        // DISK-WATCHER-01 (spill disk-health watcher respawned by supervisor).
        // 2026-06-03 (zero-tick-loss PR-8b — H2-lite): bumped 101 -> 102 for
        // AGGREGATOR-LAG-01 (candle aggregator broadcast Lagged — now loud).
        // 2026-06-06 (NTM Sub-PR #10a, §31): bumped 102 -> 103 for
        // NTM-CONSTITUENCY-01 (niftyindices source degraded — core universe continues).
        // 2026-06-09 (zero-tick-loss WAL writer hardening): bumped 103 -> 105 for
        // WS-SPILL-01 (writer respawned) + WS-SPILL-02 (durable frame dropped — now loud).
        assert_eq!(ErrorCode::all().len(), 105);
    }

    #[test]
    fn test_code_str_follows_expected_prefix_pattern() {
        for code in ErrorCode::all() {
            let s = code.code_str();
            let has_known_prefix = s.starts_with("I-P")
                || s.starts_with("GAP-")
                || s.starts_with("OMS-GAP-")
                || s.starts_with("WS-GAP-")
                || s.starts_with("RISK-GAP-")
                || s.starts_with("AUTH-GAP-")
                || s.starts_with("STORAGE-GAP-")
                || s.starts_with("DH-")
                || s.starts_with("DATA-")
                // Wave 1 (PR #393): hot-path / phase2 / prev-close / movers prefixes
                || s.starts_with("HOT-PATH-")
                || s.starts_with("PHASE2-")
                || s.starts_with("PREVCLOSE-")
                || s.starts_with("MOVERS-")
                // Wave 2: boot / audit prefixes
                || s.starts_with("BOOT-")
                || s.starts_with("AUDIT-")
                // Wave 3-B: Telegram dispatcher
                || s.starts_with("TELEGRAM-")
                // Wave 3-C: market-open self-test prefix
                || s.starts_with("SELFTEST-")
                // Wave 3-D: composite real-time guarantee score
                || s.starts_with("SLO-")
                // Wave 5 (2026-05-01): core_affinity pinning.
                // (Depth-20/200 dynamic selector prefixes retired by PR #4.)
                || s.starts_with("CORE-PIN-")
                // Wave 5 Item 26 L1: volume cumulative-monotonicity guard.
                || s.starts_with("VOLUME-")
                // PR #450 commit 8b (2026-05-03): prev_oi cache state.
                || s.starts_with("PREVOI-")
                // Wave 6 Sub-PR #1: multi-TF aggregator + boundary timer.
                || s.starts_with("AGGREGATOR-")
                || s.starts_with("BOUNDARY-")
                // Wave 4 Item 19 (Phase 0): dual-instance lock.
                || s.starts_with("RESILIENCE-")
                // Phase 0 Item 20: orphan position 15:25 IST watchdog.
                || s.starts_with("ORPHAN-POSITION-")
                // Phase 0 Items 15+28+29: 09:16:05 IST post-open cross-check.
                || s.starts_with("BAR-MISMATCH-")
                // PR #1 (AWS-lifecycle 14-PR sequence): option_chain stubs
                || s.starts_with("OPTION-CHAIN-")
                // PR #2.5 (AWS-lifecycle): Day OHLC tracker for IDX_I
                || s.starts_with("INDEX-OHLC-")
                // Sub-PR #9 of 2026-05-27 daily-universe expansion
                || s.starts_with("INSTR-FETCH-")
                // Operator 2026-06-02: post-market 1-minute cross-verification
                || s.starts_with("CROSS-VERIFY-1M-")
                // zero-tick-loss PR-5 (G3): supervised disk-health watcher
                || s.starts_with("DISK-WATCHER-")
                // zero-tick-loss 2026-06-09: WAL frame-spill writer hardening
                || s.starts_with("WS-SPILL-")
                // NTM Sub-PR #10a (§31): niftyindices constituent source degrade
                || s.starts_with("NTM-CONSTITUENCY-");
            assert!(has_known_prefix, "unexpected code prefix: {s}");
        }
    }

    #[test]
    fn test_severity_display_impl_matches_as_str() {
        // Covers `impl fmt::Display for Severity` (error_code.rs:56-60).
        for sev in [
            Severity::Info,
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ] {
            assert_eq!(format!("{sev}"), sev.as_str());
        }
    }

    #[test]
    fn test_unknown_error_code_display_and_error_trait() {
        // Covers `impl fmt::Display for UnknownErrorCode`
        // (error_code.rs:516-520) and the std::error::Error impl blanket use.
        let err = UnknownErrorCode("BOGUS-999".to_string());
        let rendered = format!("{err}");
        assert_eq!(rendered, "unknown error code: BOGUS-999");
        // Exercises std::error::Error path via the trait object.
        let as_err: &dyn std::error::Error = &err;
        assert_eq!(as_err.to_string(), "unknown error code: BOGUS-999");
    }

    #[test]
    fn test_unknown_error_code_equality_and_clone() {
        // Covers PartialEq + Clone auto-derives so they are exercised.
        let a = UnknownErrorCode("X".to_string());
        let b = a.clone();
        assert_eq!(a, b);
    }
}
