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
    /// KEPT in the C4 sweep (2026-07-15): still emitted by the
    /// retained-dormant `order_update_connection.rs` (scope-lock §A.1).
    WsGap04PostCloseSleep,
    // C4 sweep (2026-07-15, Dhan live-WS retirement — operator 2026-07-13,
    // websocket-connection-scope-lock.md "2026-07-13 Amendment"):
    // WS-GAP-05 (pool respawn), WS-GAP-06 (tick-gap summary), WS-GAP-07
    // (live frame channel closed), WS-GAP-08 (429 cooldown) and WS-GAP-09
    // (watchdog reconnect-in-place) variants RETIRED — their only emit
    // sites (the main-feed pool / tick-gap detector) were deleted in
    // Phases C2/C3 (#1522/#1569). Rule-file history retained per the house
    // banner convention (reverse-crossref allowlist entries carry the
    // dated justifications).
    /// WS-GAP-10: the order-update WebSocket is in an in-market outage.
    /// Tags three events (see `reason` field): `in_market_outage` — the
    /// once-per-episode \[HIGH\] `OrderUpdateDisconnected` page fired from
    /// INSIDE the never-give-up reconnect loop when consecutive in-market
    /// failures reach 3 (the old task-exit emit in main.rs was dead code
    /// since the WS-GAP-04 rewrite; verified live 2026-07-06: 39+ in-market
    /// failures, zero HIGH pages); `threshold_streak` — the every-10th-failure
    /// persist error; `task_exited_unreachable` — the defensive log at the
    /// main.rs spawn site. Latch re-arms ONLY after a reconnect survives the
    /// 60s stability window. Severity::High.
    WsGap10OrderUpdateOutage,
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
    /// WS-REINJECT-01: the boot-time STAGE-C.2b WAL re-injection ABORTED —
    /// a backpressured `send().await` into the pool frame channel either
    /// returned `SendError` (tick-processor consumer dropped) or exceeded
    /// the bounded per-send timeout (`WAL_REINJECT_SEND_TIMEOUT_SECS` —
    /// consumer wedged). The injector STOPS, counts the remaining frames
    /// as aborted, and the re-injection is NOT marked clean, so
    /// `confirm_replayed()` is skipped and the staged WAL segments
    /// re-replay next boot — no frame is silently lost (durable WAL
    /// floor). Replaces the pre-2026-07-03 silent `try_send` drop storm
    /// (dropped=1,127,801 at 10:35 IST — everything past
    /// `FRAME_CHANNEL_CAPACITY` dropped, the WAL never confirmed, and the
    /// replay re-grew every restart). Severity::High — the abort
    /// self-heals via next boot's replay; a dead consumer usually means
    /// the process is restarting anyway.
    WsReinject01Aborted,
    /// TICK-FLUSH-01: the off-thread tick ILP flush worker died (panic or a
    /// fatal return) and the supervisor respawned it — mirrors WS-SPILL-01
    /// (WAL writer) and WS-GAP-05 (pool supervisor). Self-healing: the
    /// respawn re-enters the loop with the SAME channels, so queued flush
    /// batches survive and the writer's `append` never sees `Disconnected`.
    /// B6 (2026-07-03): this worker is what keeps the blocking questdb ILP
    /// TCP flush OFF the tick-consumer thread. Severity::High (a flapping
    /// flush worker means QuestDB ILP or the host is degrading).
    TickFlush01WorkerRespawn,
    /// WAL-SUSPEND-01: a QuestDB table's WAL apply is SUSPENDED — the 60s
    /// `wal_tables()` probe (W2 PR#6, 2026-07-10, audit follow-up row 10)
    /// got a SUCCESSFUL response showing `suspended=true` for the table.
    /// ILP ingestion keeps ACKing rows into the table's WAL, but they
    /// silently stop becoming visible/durable-applied — the
    /// silent-data-visibility-loss class (post disk-full episode or WAL
    /// apply error). Edge-latched: ONE emit per (table, suspension
    /// episode); a merely-DOWN QuestDB never fires this (down ≠ suspended
    /// — BOOT-01/02 own that page). Severity::High; NEVER auto-triaged —
    /// the recovery action `ALTER TABLE <t> RESUME WAL` is an operator
    /// decision (auto-resume can replay into a still-broken disk).
    WalSuspend01TableSuspended,
    /// PROC-01: an OOM kill was detected — the cgroup-v2 `memory.events`
    /// `oom_kill` counter rose above the boot-time baseline. Some process in
    /// this cgroup (tickvault itself or a sidecar) was killed by the kernel
    /// OOM killer. Before this signal an OOM was only caught indirectly
    /// (process dies → systemd → market-hours-liveness page on a missing SLO),
    /// so an OOM-loop was indistinguishable from a panic-loop with zero OOM
    /// attribution. Severity::Critical — the host is out of memory.
    Proc01OomKillDetected,
    /// AUTH-GAP-03: token force-renewed on WebSocket wake.
    AuthGap03TokenForceRenewedOnWake,
    /// AUTH-GAP-04: the login TOTP/2FA secret was rotated externally
    /// (e.g. operator regenerated it via the dhan.co web UI without
    /// updating SSM). Token generation exhausts `TOTP_MAX_RETRIES` and
    /// returns a distinctly-typed failure instead of a generic auth
    /// error, so the operator is pointed at the exact SSM parameter to
    /// reconcile. Severity::Critical — auth is dead until the secret is
    /// fixed. AUTH-P11 (audit 2026-07-01).
    AuthGap04TotpRotatedExternally,
    /// AUTH-GAP-05: sustained mid-session token-invalid — the mid-session
    /// profile watchdog observed the consecutive-REAL-`/v2/profile`-failure
    /// threshold during market hours and forced exactly ONE token re-mint
    /// per failing episode via the existing renewal machinery
    /// (lock-before-mint honored fail-closed; ~125s Dhan mint cooldown
    /// honored; retry-once latch). Severity::High — the re-mint IS the
    /// self-remediation; the standing CRITICAL profile page covers the
    /// unrecovered case. (2026-07-06.)
    AuthGap05ForcedRemintTriggered,
    // C4 sweep (2026-07-15): AUTH-GAP-06 variant RETIRED — the fast-boot
    // cached-token validation module was deleted 2026-07-14 (operator Dhan
    // noise lock, dhan-rest-only-noise-lock-2026-07-14.md; "variant
    // retained until the C4 variant sweep" — this is that sweep).
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
    /// AUDIT-WS-01: `ws_event_audit` row write failed — a WebSocket lifecycle
    /// event (connect/disconnect/reconnect/sleep) could not be persisted to the
    /// forensic table. Supersedes the never-shipped AUDIT-03 ws_reconnect concept
    /// (this table covers ALL 6 WS event kinds, not just reconnect).
    AuditWs01EventWriteFailed,
    /// STORAGE-GAP-03: audit-table write failure (any table).
    StorageGap03AuditWriteFailed,
    /// STORAGE-GAP-04: S3 archive failure (partition upload).
    StorageGap04S3ArchiveFailed,
    /// SELFTEST-01: market-open self-test passed (informational positive ping).
    Selftest01Passed,
    /// SELFTEST-02: market-open self-test detected a Critical or Degraded
    /// failure — operator action required.
    Selftest02Failed,
    // C4 sweep (2026-07-15): SLO-01 / SLO-02 / SLO-03 variants RETIRED —
    // the 10s SLO evaluator/publisher was PARKED in PR-C2 (wave-3-d
    // banner: "pending either a Groww-scoped SLO re-design with a fresh
    // dated operator quote, or Phase C variant cleanup" — this is that
    // cleanup; the caller-less slo_score.rs contract stub + its bench
    // retired with them).

    // -----------------------------------------------------------------------
    // Wave 3 — Telegram dispatcher (Item 11)
    // -----------------------------------------------------------------------
    /// TELEGRAM-01: a Telegram event was dropped (queue full, send-after-retry
    /// failure, or coalescer overflow). Operator alerts MAY be missed.
    Telegram01Dropped,
    /// TELEGRAM-02: coalescer state inconsistency (drain failed mid-window;
    /// next drain self-recovers). Informational.
    Telegram02CoalescerStateInconsistency,
    /// TELEGRAM-03: episode live-edit machinery degraded (2026-07-07 UX
    /// overhaul) — snapshot store write failed, rehydrate hit corrupt JSON,
    /// or the edit-fallback ladder is storming. Delivery itself is
    /// unaffected (duplicate-over-drop ladder terminates at TELEGRAM-01);
    /// only the one-bubble-per-incident UX degrades.
    Telegram03EpisodeDegraded,

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
    /// RESILIENCE-03: a Dhan `generateAccessToken` mint was REFUSED
    /// because this process no longer holds the SSM dual-instance lock
    /// (the heartbeat observed a peer taking ownership, or shutdown
    /// released it). Dhan enforces one active token at a time — minting
    /// while a peer owns the session would invalidate the peer's JWT
    /// and start a cross-host mint ping-pong (coexistence audit
    /// 2026-07-04). Fail-closed refusal, loud, never silent.
    /// Severity::Critical.
    Resilience03MintRefusedLockNotHeld,
    /// RESOURCE-01: open file-descriptor count crossed the early-warning
    /// threshold (default 80% of `LimitNOFILE`). A leaked WS / QuestDB
    /// socket can exhaust the fd table with zero signal until `connect()`
    /// starts failing; this monitor pages BEFORE that. Severity::High.
    /// BP-08 (audit 2026-07-01).
    Resource01FdCountHigh,
    /// RESOURCE-02: process resident memory (VmRSS) crossed the
    /// early-warning threshold (default 80% of the cgroup memory limit).
    /// Distinct from the host-aggregate `mem_used_high` alarm — this is
    /// the tickvault process itself approaching its cgroup ceiling before
    /// the OOM killer fires. Severity::High. BP-08 (audit 2026-07-01).
    Resource02ResidentMemoryHigh,
    /// RESOURCE-03: spill-directory free space dropped below the
    /// early-warning percent-of-total threshold (default 20% free). A
    /// process-level percent view distinct from the host `disk_used_high`
    /// aggregate, so a fast-filling spill dir is caught before the zero-
    /// loss chain is at risk. Severity::High. BP-08 (audit 2026-07-01).
    Resource03SpillFreeLow,
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

    // C4 sweep (2026-07-15, Dhan live-WS retirement): INSTR-FETCH-01..04 +
    // NTM-CONSTITUENCY-01 variants RETIRED — the daily-universe fetch chain
    // (CSV downloader/parser, universe build, NTM constituency) was deleted
    // in Phase C3 (#1569; operator Q3 2026-07-13: "hereafter no Dhan
    // instrument download/parsing"). CROSS-VERIFY-1M-01/-02 variants
    // RETIRED — the 15:31 IST cross-verify module was deleted in C3 (no
    // Dhan live side to compare); its `cross_verify_1m_audit` table + CSVs
    // stay (forensic). Rule-file retirement banners carry the history.

    // -----------------------------------------------------------------------
    /// TICK-CONSERVE-01: the daily 15:40 IST end-to-end tick-conservation
    /// audit found a positive residual — ticks Dhan delivered (counted in
    /// the WAL disk log) did not all reach a known outcome
    /// (DB row / filter counter). The audit row in `tick_conservation_audit`
    /// carries the exact per-stage numbers. Severity::High (operator
    /// directive 2026-06-10 "Go ahead to achieve zero tick loss").
    TickConserve01DailyResidual,

    // C4 sweep (2026-07-15): REST-CANARY-01 variant RETIRED — the canary
    // module + both spawn sites + its CloudWatch entry were deleted
    // 2026-07-14 (operator Dhan noise lock,
    // dhan-rest-only-noise-lock-2026-07-14.md; "variant retained until the
    // C4 variant sweep" — this is that sweep). The retained spot-1m +
    // option-chain legs self-detect a dead Dhan REST surface via their own
    // escalation edges.

    // -----------------------------------------------------------------------
    // OPTION-CHAIN-01..08 REMOVED 2026-06-28 along with the entire
    // option_chain REST subsystem (operator directive 2026-06-28 — "drop the
    // option chain entire implementations and its table also"). The subsystem
    // was disabled since 2026-06-02 with no live consumer. (PR-C 2026-05-26:
    // CROSS-VERIFY-01..04 stubs were retired earlier.)
    // -----------------------------------------------------------------------

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

    // C4 sweep (2026-07-15): PREVDAY-01 variant RETIRED — the boot-time
    // prev-day OHLCV fetch died with the Dhan lane (C3; the `prev_day_ohlcv`
    // table stays, forensic). DHAN-LANE-01..04 variants RETIRED — the
    // runtime lane FSM (`start_dhan_lane`/`stop_dhan_lane`/`LaneState`) was
    // deleted in Phase C2 (#1522); auth-failure visibility for the retained
    // Dhan REST surface lives in the `dhan_rest_stack` backoff arms + the
    // AUTH-GAP-* codes (dhan-lane-error-codes.md retirement banner).
    /// GROWW-MASTER-01 (PR-A 2026-06-28) — the best-effort cold-path write of
    /// the Groww instrument set into the SHARED `instrument_lifecycle` /
    /// `index_constituency` master tables (tagged `feed='groww'`) failed
    /// (QuestDB ILP unreachable / flush error). The Groww feed activation and
    /// the live tick path are UNAFFECTED — this is a forensic master/metadata
    /// write, never on the data-correctness or recovery path. Idempotent DEDUP
    /// UPSERT, so the next boot re-runs. Severity::Medium (auto-triage-safe).
    GrowwMaster01PersistFailed,
    /// FEED-STALL-01 (2026-06-30) — a live-feed sidecar was ALIVE but streaming
    /// NOTHING across its whole subscribed universe for longer than the stall
    /// threshold during market hours (the silently-closed NATS socket that left
    /// the Groww feed dead at 10:31 IST and never recovering). The FEED-AGNOSTIC
    /// supervisor stall-watchdog killed + relaunched the sidecar so it re-auths +
    /// re-subscribes. NOT auto-triage-safe: a stall restart is a real recovery
    /// action the operator must see (and a flapping restart STORM, escalated with
    /// the rapid-restart count, points at a persistent provider-side reject).
    /// Severity::High.
    FeedStall01SidecarRestarted,
    /// FEED-SUPERVISOR-01 (2026-06-30) — a feed sidecar SUPERVISOR task itself
    /// died (panic / unexpected return) and the respawning wrapper (WS-GAP-05
    /// pattern) re-spawned it so the stall-watchdog can never die silently. The
    /// feed is briefly unsupervised between death and respawn; the relaunch loop
    /// resumes on respawn. Severity::High (a supervisor that keeps dying points at
    /// a real bug), auto-triage-safe (the respawn already self-healed).
    FeedSupervisor01Respawned,
    /// FEED-REJECT-01 (2026-07-09) — a live-feed sidecar opened an
    /// operator-actionable reject/error episode (the once-per-child alert edge
    /// that fires the fixed-wording Telegram), and the supervisor captured a
    /// BOUNDED (≤160 chars), secret-redacted SIGNATURE of the triggering
    /// sidecar diagnostic line into the coded error stream. Closes the
    /// 2026-07-09 all-day reject-loop blindness: the Telegram wording is fixed
    /// per class (10-commandments correct), so without this code the operator
    /// (and triage) could not query errors.jsonl / CloudWatch for WHY the loop
    /// repeats. Severity::High, auto-triage-safe (visibility only — the
    /// restart/backoff machinery already owns recovery).
    FeedReject01SidecarErrorDetail,
    /// HTTP-CLIENT-01 (C2 2026-07-03) — `reqwest::ClientBuilder::build()`
    /// failed (TLS backend init / resolver init / fd exhaustion). Previously
    /// 8 storage sites fell back to `Client::new()`, which PANICS on the
    /// exact same failure conditions — a silent tokio-task death (the
    /// suspected mechanism behind the 2026-07-03 10:35 IST SLO-publisher
    /// death during a 1.13M-frame storm). Now a typed error degrades the
    /// single probe/write and the site logs + counts
    /// (`tv_http_client_build_failed_total{site}`), never the process.
    /// Severity::High, auto-triage-safe (the degrade already happened; the
    /// operator inspects the fd/resolver pressure — cross-check
    /// RESOURCE-01).
    HttpClient01BuildFailed,
    /// GROWW-SCALE-01 (§34 auto-scale, 2026-07-03) — the Groww auto-scale
    /// ladder ROLLED BACK a failed rung advance: the newly added connections
    /// were killed (verified older connections untouched), the last
    /// KNOWN-healthy connection count was restored, and an exponential hold
    /// (`min(10m × 2^k, 4h)`) was armed before the next attempt. This IS the
    /// operator-demanded auto-correction ("if there is any failure how that
    /// can be auto corrected") — Severity::High so the operator sees every
    /// rollback; auto-triage-safe (the correction already applied).
    GrowwScale01RollbackFired,
    /// GROWW-SCALE-02 (§34 auto-scale, 2026-07-03) — ALL active Groww
    /// connections failed within a 60s window: classified as an ACCOUNT-LEVEL
    /// throttle (provider rejecting us), NOT a single-rung failure. The
    /// ladder applied a 5-minute global cooldown, HALVED the connection count
    /// (`N → max(ceil(N/2), 1)`), and resumed PROBING with staggered
    /// reconnects. Severity::High, auto-triage-safe (self-applied); a
    /// REPEATING halve down to 1 conn is the honest "Groww refuses
    /// multi-connection" signal per §34.3.
    GrowwScale02GlobalHalve,
    /// GROWW-SCALE-03 (§34 auto-scale, 2026-07-03) — the fail-closed shard
    /// invariant was violated: two shards claimed the same
    /// `(exchange, segment, security_id)` OR the shard union did not cover
    /// the watch-set. The ladder step HALTs; the younger conflicting
    /// connection is killed. Should be unreachable (the `cut_shards` ratchet
    /// tests pin disjointness + coverage) — a firing means a real cutter/
    /// ladder bug. Severity::Critical (never auto-triaged).
    ///
    /// CORRECTED 2026-07-04 (Session-B verdict): the `ticks` DEDUP key
    /// `(ts, security_id, segment, capture_seq, feed)` does NOT collapse
    /// cross-connection overlap — `capture_seq` is globally unique and IN
    /// the key, so two connections streaming the SAME instrument produce
    /// DISTINCT `capture_seq` values → TWO rows per tick (silent duplicate
    /// rows). The cut-time fail-closed cutter is the actual protection;
    /// until the PR-2 runtime duplicate-SID detector ships, an overlap that
    /// escapes the cutter IS silent data duplication — which is exactly why
    /// this code is Critical, not Medium.
    GrowwScale03ShardOverlap,
    /// GROWW-SCALE-04 (§34 auto-scale, 2026-07-03) — a `groww_scale_audit`
    /// rung-transition row could not be persisted (ILP down / QuestDB
    /// unreachable / disk full). Best-effort forensic side-record (mirror of
    /// AUDIT-WS-01): ladder decisions continue on in-memory state; a restart
    /// during the outage rehydrates from the last PERSISTED (always
    /// previously-verified) rung. Severity::Medium, auto-triage-safe.
    GrowwScale04AuditWriteFailed,
    /// GROWW-SCALE-05 (Session-B fix, operator go 2026-07-04) — the Groww
    /// scale-FLEET spawn was REFUSED by the fleet dual-instance SSM lock
    /// (`/tickvault/<env>/instance-lock-groww-scale`, reusing the
    /// RESILIENCE-01 `instance_lock` machinery via the named-lock knob).
    /// Fires when (a) another tickvault instance ALREADY holds the fleet
    /// lock (Mac + AWS, or two Macs, scaling the SAME Groww account — a
    /// failure that previously masqueraded as provider throttle), (b) SSM
    /// was unavailable after the bounded 3-attempt / 2s-4s retry budget
    /// (fail-closed: we cannot prove there is no peer), or (c) the fleet
    /// lock was lost mid-run to a foreign takeover. The boot degrades to
    /// the SINGLE-CONNECTION Groww path (same fallback as a failed scale
    /// PREFLIGHT — capture continues; only the multi-conn fleet is
    /// refused). Severity::Critical (never auto-triaged — the operator
    /// must decide which host runs the scale test).
    GrowwScale05DualFleetDetected,
    /// GROWW-NATIVE-01 (PR-R1 shadow client, 2026-07-04) — the native-Rust
    /// Groww NATS-over-WebSocket shadow client failed to connect / lost the
    /// connection / had its supervised task respawned. Bounded expo backoff
    /// reconnect (`GROWW_NATIVE_RECONNECT_*_SECS`); shadow-only — the Python
    /// sidecar capture chain is UNAFFECTED. Severity::Medium,
    /// auto-triage-safe (self-healing reconnect; a sustained rate is the
    /// live-probe answer the shadow run exists to collect).
    GrowwNative01ConnectFailed,
    /// GROWW-NATIVE-02 (PR-R1 shadow client, 2026-07-04) — the per-session
    /// socket-token mint (`GROWW_SOCKET_TOKEN_URL`, live-feed-AUTH KEEP
    /// class) failed, or the NATS `CONNECT` was rejected with an auth-class
    /// `-ERR`. Recovery: fresh ed25519 keypair + fresh socket-token mint;
    /// the ACCESS token is only ever RE-READ from SSM at
    /// `GROWW_NATIVE_AUTH_RETRY_FLOOR_SECS` pacing — NEVER minted
    /// (token-minter lock 2026-07-02). Severity::Medium, auto-triage-safe.
    GrowwNative02AuthFailed,
    /// GROWW-NATIVE-03 (PR-R1 shadow client, 2026-07-04) — a NATS frame or
    /// protobuf tick payload failed to decode (typed error, never a panic).
    /// Proto-level failures are counted + skipped; framing-level failures
    /// close + reconnect the socket. Severity::Medium, auto-triage-safe.
    GrowwNative03DecodeFailed,
    /// GROWW-NATIVE-04 (PR-R1 shadow client, 2026-07-04) — the shadow NDJSON
    /// writer failed a write / flush / IST-midnight rotation, or its bounded
    /// channel overflowed. Capture continues (rotation failures retry next
    /// midnight, mirroring the sidecar); the loss is shadow comparison data
    /// only — the production capture chain is untouched. Severity::Medium,
    /// auto-triage-safe.
    GrowwNative04WriterFailed,
    /// FUTIDX-01 (§36 2026-07-08; §36.7 all monthly expiries 2026-07-10) —
    /// the index-future selection degraded on one feed: a whole underlying
    /// (no FUT rows / all expiries past / unparsable expiry / monthly serial
    /// flood) or a single month (ambiguous duplicate expiry / candidate
    /// flood at that expiry — the payload's `expiry` field names it). That
    /// feed runs WITHOUT that future (or that month) for the day — degrade,
    /// never HALT; the spot universe is unaffected. Severity::High,
    /// auto-triage-safe (the degrade already happened; operator inspects the
    /// day's master CSV + the alias-drift evidence payload at leisure).
    Futidx01SelectionDegraded,
    /// FUTIDX-02 (§36 2026-07-08; §36.7 expiry-SET parity 2026-07-10) — the
    /// boot-time cross-feed comparator found the Dhan and Groww builds chose
    /// DIVERGENT expiry sets in a comparable month (nearest differs, a hole,
    /// or one-sided presence) for an index-future underlying (a pure
    /// far-suffix depth difference is an info-level note, never this code).
    /// Both feeds STAY LIVE
    /// (visibility, never a halt); cross-feed rows for that underlying are
    /// not comparable that day. One vendor's master is stale/divergent —
    /// operator compares the two masters' FUT rows and records a dated note.
    /// Severity::High.
    Futidx02CrossFeedExpiryMismatch,
    /// SCOREBOARD-01 (dual-feed scoreboard PR-A, 2026-07-10) — the daily
    /// 15:45 IST Dhan-vs-Groww scoreboard aggregation was DEGRADED: a
    /// QuestDB `/exec` read failed (sentinels recorded, never fabricated
    /// zeros), the `feed_scoreboard_daily` / `feed_episode_audit` ILP write
    /// was rejected, the boot-time process-death reconciliation could not
    /// read/write, or the same-day `ws_event_audit` drop counter shows the
    /// episode source under-counted. Best-effort forensic aggregate
    /// (AUDIT-WS-01 / GROWW-MASTER-01 class) — the live feeds, tick capture
    /// and trading are NEVER affected; DEDUP-idempotent re-runs
    /// (`TICKVAULT_SCOREBOARD_NOW`) backfill the day. Severity::Medium,
    /// auto-triage-safe.
    Scoreboard01AggregationDegraded,
    /// BRUTEX-XVERIFY-01 (BruteX↔TickVault daily cross-verify, 2026-07-12) —
    /// the 15:50 IST run found ≥1 divergent cell (or missing-live /
    /// missing-brutex minute) between the BruteX-produced Groww 1-minute
    /// OHLCV CSVs (S3 `crossverify/groww/<date>/`) and the live
    /// `candles_1m` (`feed='groww'`) — paise-integer compare, inclusive
    /// tolerance. Each cell is a `brutex_crossverify_cell_audit` row; the
    /// daily verdict row + Telegram summary carry the counts. Severity::High;
    /// NOT auto-triage-safe (severity-independent override — a cross-system
    /// data-comparability verdict is an OPERATOR judgment: which side's
    /// pipeline drifted; the FUTIDX-02 precedent).
    BrutexXverify01DivergenceFound,
    /// BRUTEX-XVERIFY-02 (BruteX↔TickVault daily cross-verify, 2026-07-12) —
    /// the 15:50 IST run itself DEGRADED: S3 list/get failed after bounded
    /// retries, no objects appeared by the 16:05 IST wall-clock cap
    /// (NO_DATA), CSV parse rejected, the symbol→security_id mapping read
    /// failed, the live `candles_1m` read failed, or the forensic ILP write
    /// was rejected. The day is stamped `no_data` / `blind` / `degraded` —
    /// never a fabricated clean verdict (Rule 11). Best-effort cold path:
    /// the live feeds, tick capture and trading are NEVER affected; the
    /// DEDUP-idempotent tables let a healthy re-run backfill the day.
    /// Severity::High, auto-triage-safe (the degrade already happened —
    /// the operator inspects; the next trading day re-runs).
    BrutexXverify02RunDegraded,
    /// SPOT1M-01 (per-minute REST pipeline PR-2, operator grant 2026-07-12)
    /// — the per-minute spot 1m REST fetch degraded: a whole minute failed
    /// for one/all of the 3 IDX_I spot indices (transport error, non-2xx,
    /// DH-904/429 after the bounded in-minute re-poll ladder, no token, or
    /// a 200 whose body never carried the just-closed minute's candle —
    /// `outcome="empty"`, counted, never silent). The ESCALATION emission
    /// (the one that also pages the typed Telegram event) fires
    /// edge-triggered after 3 consecutive fully-failed minutes; sub-edge
    /// per-minute emissions are coalesced once per fire. Severity::High,
    /// auto-triage-safe (the fetch already degraded; the next minute
    /// re-attempts; the WS candle pipeline is untouched).
    Spot1m01FetchDegraded,
    /// SPOT1M-02 (per-minute REST pipeline PR-2, 2026-07-12) — the
    /// `spot_1m_rest` QuestDB persist leg failed (ensure-DDL non-2xx /
    /// unreachable, ILP append rejected, or the ILP-over-HTTP flush was
    /// refused by the server ACK). Best-effort forensic write: the fetch
    /// loop continues, rows stay buffered where possible, and re-appends
    /// are DEDUP-idempotent (`ts, security_id, exchange_segment, feed`).
    /// Severity::High, auto-triage-safe.
    Spot1m02PersistFailed,
    /// CHAIN-01 (per-minute REST pipeline PR-3, operator grant 2026-07-12)
    /// — the option-chain Data-API ENTITLEMENT is absent: Dhan rejected an
    /// expirylist / option-chain call with the DH-902 / DATA 806 class
    /// (or a 401/403 whose body names the missing subscription). Fires
    /// ONCE per day (edge-triggered); the chain pipeline stays DOWN for
    /// the day — never a per-minute 401 storm. Severity::High,
    /// auto-triage NO (severity-independent override: restoring the
    /// entitlement is an operator/broker account decision, never a code
    /// fix).
    Chain01EntitlementAbsent,
    /// CHAIN-02 (per-minute REST pipeline PR-3, 2026-07-12) — the
    /// per-minute option-chain fetch degraded: a whole minute failed for
    /// one/all of the 3 underlyings (transport error, non-2xx, budget
    /// overrun, malformed body, or a 200 whose chain carried zero strikes
    /// — `outcome="empty"`, counted, never silent). The ESCALATION
    /// emission (the one that also pages the typed Telegram event) fires
    /// edge-triggered after 3 consecutive fully-failed minutes (persist
    /// failures count — a fetched-but-never-persisted minute is NOT ok).
    /// Severity::High, auto-triage-safe (the next minute re-attempts;
    /// the WS pipeline is untouched).
    Chain02FetchDegraded,
    /// CHAIN-03 (per-minute REST pipeline PR-3, 2026-07-12) — the
    /// `option_chain_1m` QuestDB persist leg failed (ensure-DDL non-2xx /
    /// unreachable, ILP append rejected, or the ILP-over-HTTP flush was
    /// refused by the server ACK; failed flushes DISCARD pending rows —
    /// the poisoned-buffer defense). Best-effort forensic write;
    /// re-appends are DEDUP-idempotent. Severity::High, auto-triage-safe.
    Chain03PersistFailed,
    /// CHAIN-04 (per-minute REST pipeline PR-3, 2026-07-12) — the
    /// day-start expirylist warmup failed after bounded retries (3
    /// attempts, 3s/6s backoff): the chain pipeline degrades to
    /// DISABLED-FOR-THE-DAY — expiry dates come ONLY from the API
    /// (option-chain.md rule 9), NEVER guessed. One page per day.
    /// Severity::High, auto-triage-safe (the next trading-day boot
    /// re-attempts automatically).
    Chain04ExpirylistFailed,

    // -----------------------------------------------------------------------
    // Operator 2026-07-13: daily timeframe-consistency verifier.
    // "how will you guarantee that all our defined timeframes internally are
    // correct — how do you identify whether any miscalculation or data
    // issues" → at 15:40 IST every trading day, recompute every sealed
    // higher-TF candle (2m..4h, both feeds) from its stored candles_1m
    // constituents and compare exactly (integer-paise OHLC, exact i64
    // volume). Dhan verifies TODAY; Groww verifies the PREVIOUS trading
    // day (no close-time force-seal — its tails seal at IST midnight).
    // See tf-consistency-error-codes.md.
    // -----------------------------------------------------------------------
    /// TF-VERIFY-01: the daily timeframe-consistency verifier found ≥1
    /// paging finding for a (feed, date) pass — a higher-TF candle
    /// disagrees with its recomputed-from-1m value (`mismatch`), a stored
    /// TF row is missing where 1m data exists (`missing_tf_row`), a TF row
    /// exists over zero 1m rows (`no_1m_coverage`), a stored TF ts is off
    /// the 09:15-anchored grid (`off_grid_ts`), or duplicate rows share a
    /// DEDUP key (`duplicate_key`). ONE coalesced emission per (feed, date)
    /// pass, never per-row. Severity::High, auto-triage NO
    /// (severity-independent override — a TF-definition verdict is an
    /// operator data-integrity judgment, the Futidx02 precedent).
    TfVerify01MismatchFound,
    /// TF-VERIFY-02: the daily timeframe-consistency run DEGRADED — HTTP
    /// client build failure, QuestDB unreachable, discovery failure, a
    /// per-SID query failure, a LIMIT-hit truncation tripwire, an audit
    /// flush failure (pending rows discarded — poisoned-buffer defense),
    /// or the 900s wall-clock budget exceeded (`stage` names the leg).
    /// Severity::High, auto-triage-safe (the next trading day re-runs
    /// automatically; forced reruns are DEDUP-idempotent).
    TfVerify02RunDegraded,
    /// FEED-GAP-01 (Groww hardening PR-3, 2026-07-14) — the feed GAP-EPISODE
    /// forensics machinery DEGRADED: the tracker could not persist an
    /// OPEN/CLOSE row to the `feed_gap_audit` table (ensure-DDL / ILP append /
    /// flush), or the 15:45 IST scoreboard dangling-close sweep failed
    /// (`stage` names the leg). ANNOTATION ONLY — never on the feed recovery
    /// path or the tick hot path; the Telegram episode bubble still fires and
    /// re-appends are DEDUP-idempotent (`ts, trading_date_ist, feed, start_ts,
    /// outcome`). Telegram-episode-only delivery per the operator-approved
    /// 2026-07-14 default (no CloudWatch filter). Severity::Medium,
    /// auto-triage-safe.
    FeedGap01EpisodeDegraded,
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
            Self::WsGap10OrderUpdateOutage => "WS-GAP-10",
            Self::DiskWatcher01Respawned => "DISK-WATCHER-01",
            Self::WsSpill01WriterRespawn => "WS-SPILL-01",
            Self::WsSpill02FrameDropped => "WS-SPILL-02",
            Self::WsReinject01Aborted => "WS-REINJECT-01",
            Self::TickFlush01WorkerRespawn => "TICK-FLUSH-01",
            Self::WalSuspend01TableSuspended => "WAL-SUSPEND-01",
            Self::Proc01OomKillDetected => "PROC-01",
            Self::AuthGap03TokenForceRenewedOnWake => "AUTH-GAP-03",
            Self::AuthGap04TotpRotatedExternally => "AUTH-GAP-04",
            Self::AuthGap05ForcedRemintTriggered => "AUTH-GAP-05",
            Self::Boot01QuestDbSlow => "BOOT-01",
            Self::Boot02DeadlineExceeded => "BOOT-02",
            Self::Boot03ClockSkewExceeded => "BOOT-03",
            // PR #5 (2026-05-19): AUDIT-01 retired with phase2_audit_persistence.rs.
            Self::Audit02DepthRebalanceWriteFailed => "AUDIT-02",
            Self::Audit03WsReconnectWriteFailed => "AUDIT-03",
            Self::Audit04BootWriteFailed => "AUDIT-04",
            Self::Audit05SelftestWriteFailed => "AUDIT-05",
            Self::Audit06OrderWriteFailed => "AUDIT-06",
            Self::AuditWs01EventWriteFailed => "AUDIT-WS-01",
            Self::StorageGap03AuditWriteFailed => "STORAGE-GAP-03",
            Self::StorageGap04S3ArchiveFailed => "STORAGE-GAP-04",
            // Wave 3 — Telegram dispatcher (Item 11)
            Self::Telegram01Dropped => "TELEGRAM-01",
            Self::Telegram02CoalescerStateInconsistency => "TELEGRAM-02",
            Self::Telegram03EpisodeDegraded => "TELEGRAM-03",
            // Wave 3-C — market-open self-test (Item 12)
            Self::Selftest01Passed => "SELFTEST-01",
            Self::Selftest02Failed => "SELFTEST-02",
            // Wave 3-D — composite real-time guarantee score (Item 13)
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
            Self::Resilience03MintRefusedLockNotHeld => "RESILIENCE-03",
            Self::Resource01FdCountHigh => "RESOURCE-01",
            Self::Resource02ResidentMemoryHigh => "RESOURCE-02",
            Self::Resource03SpillFreeLow => "RESOURCE-03",
            Self::OrphanPosition01Detected => "ORPHAN-POSITION-01",
            Self::BarMismatch01CorrectedFromHistorical => "BAR-MISMATCH-01",
            Self::BarMismatch02CrossCheckInconclusive => "BAR-MISMATCH-02",
            Self::BarMismatch03CrossCheckFailed => "BAR-MISMATCH-03",
            // Operator 2026-06-02: post-market 1-minute cross-verification
            Self::TickConserve01DailyResidual => "TICK-CONSERVE-01",
            // DHAN-REST-400 (2026-06-10): scheduled REST-health canary
            // Day OHLC tracker for IDX_I
            Self::IndexOhlc02DailyResetFailed => "INDEX-OHLC-02",
            // Boot-time previous-day OHLCV fetch (PR4 2026-06-01)
            // D2b — runtime Dhan-lane cold-start FSM (2026-06-26)
            // PR-A (2026-06-28): Groww shared-master persist
            Self::GrowwMaster01PersistFailed => "GROWW-MASTER-01",
            Self::FeedStall01SidecarRestarted => "FEED-STALL-01",
            Self::FeedSupervisor01Respawned => "FEED-SUPERVISOR-01",
            // 2026-07-09: bounded sidecar reject-cause signature surfacing
            Self::FeedReject01SidecarErrorDetail => "FEED-REJECT-01",
            // C2 (2026-07-03): panic-free reqwest client construction
            Self::HttpClient01BuildFailed => "HTTP-CLIENT-01",
            // §34 (2026-07-03): Groww multi-connection auto-scale ladder
            Self::GrowwScale01RollbackFired => "GROWW-SCALE-01",
            Self::GrowwScale02GlobalHalve => "GROWW-SCALE-02",
            Self::GrowwScale03ShardOverlap => "GROWW-SCALE-03",
            Self::GrowwScale04AuditWriteFailed => "GROWW-SCALE-04",
            Self::GrowwScale05DualFleetDetected => "GROWW-SCALE-05",
            // PR-R1 (2026-07-04): Groww native-Rust shadow client
            Self::GrowwNative01ConnectFailed => "GROWW-NATIVE-01",
            Self::GrowwNative02AuthFailed => "GROWW-NATIVE-02",
            Self::GrowwNative03DecodeFailed => "GROWW-NATIVE-03",
            Self::GrowwNative04WriterFailed => "GROWW-NATIVE-04",
            Self::Futidx01SelectionDegraded => "FUTIDX-01",
            Self::Futidx02CrossFeedExpiryMismatch => "FUTIDX-02",
            // Dual-feed scoreboard PR-A (2026-07-10)
            Self::Scoreboard01AggregationDegraded => "SCOREBOARD-01",
            // BruteX↔TickVault daily cross-verify (2026-07-12)
            Self::BrutexXverify01DivergenceFound => "BRUTEX-XVERIFY-01",
            Self::BrutexXverify02RunDegraded => "BRUTEX-XVERIFY-02",
            // Per-minute spot 1m REST pipeline (operator grant 2026-07-12)
            Self::Spot1m01FetchDegraded => "SPOT1M-01",
            Self::Spot1m02PersistFailed => "SPOT1M-02",
            // Per-minute option-chain REST pipeline (PR-3, 2026-07-12)
            Self::Chain01EntitlementAbsent => "CHAIN-01",
            Self::Chain02FetchDegraded => "CHAIN-02",
            Self::Chain03PersistFailed => "CHAIN-03",
            Self::Chain04ExpirylistFailed => "CHAIN-04",
            // Daily timeframe-consistency verifier (operator 2026-07-13)
            Self::TfVerify01MismatchFound => "TF-VERIFY-01",
            Self::TfVerify02RunDegraded => "TF-VERIFY-02",
            // Feed gap-episode forensics (Groww hardening PR-3, 2026-07-14)
            Self::FeedGap01EpisodeDegraded => "FEED-GAP-01",
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
            | Self::Resilience03MintRefusedLockNotHeld
            | Self::OrphanPosition01Detected
            | Self::BarMismatch01CorrectedFromHistorical
            | Self::BarMismatch02CrossCheckInconclusive
            | Self::BarMismatch03CrossCheckFailed
            // AUTH-P11 (2026-07-01) — TOTP secret rotated externally; auth is
            // dead until the SSM secret is reconciled (Critical, not auto-triage).
            | Self::AuthGap04TotpRotatedExternally
            // WS-SPILL-02 — durable frame dropped (writer dead at append instant)
            | Self::WsSpill02FrameDropped
            // PROC-01 — OOM kill detected (host out of memory)
            | Self::Proc01OomKillDetected
            // GROWW-SCALE-03 — shard disjointness/coverage contract violated
            // (a cutter/ladder bug; ladder step HALTs fail-closed)
            | Self::GrowwScale03ShardOverlap
            // GROWW-SCALE-05 (Session-B 2026-07-04) — dual scale-fleet
            // instance detected / lock unprovable: fleet spawn refused
            // fail-closed; operator must pick the winning host.
            | Self::GrowwScale05DualFleetDetected => Severity::Critical,
            // Info: positive-ping / lifecycle confirmations
            Self::Selftest01Passed
            | Self::AggregatorHb01Heartbeat => Severity::Info,
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
            // PR #2.5 — INDEX-OHLC-02 is High (carry-over wrong but recoverable)
            | Self::IndexOhlc02DailyResetFailed
            // Operator 2026-06-10 — daily tick-conservation residual (High:
            // delivered-but-unaccounted ticks need operator eyes; the WAL
            // still holds them durably, so this is not Critical data loss)
            | Self::TickConserve01DailyResidual
            // WS-GAP-10 — order confirmations feed down in-market; the loop
            // self-retries but the operator must see it (2026-07-06 incident)
            | Self::WsGap10OrderUpdateOutage
            // WS-SPILL-01 — WAL writer respawned (flapping writer = disk dying)
            | Self::WsSpill01WriterRespawn
            // WS-REINJECT-01 — boot WAL re-injection aborted (consumer dead/
            // wedged); frames stay staged in replaying/ — self-heals on the
            // next boot's replay, but the operator must see a dead consumer
            | Self::WsReinject01Aborted
            // TICK-FLUSH-01 — off-thread tick flush worker respawned (B6);
            // flapping = QuestDB ILP / host degrading
            | Self::TickFlush01WorkerRespawn
            // WAL-SUSPEND-01 — a table's WAL apply is suspended (W2 PR#6):
            // silent data-visibility loss until the operator RESUMEs WAL.
            // High, not Critical: the rows are durably in the table's WAL
            // (apply resumes them) — staleness + disk growth, not
            // permanent loss; but operator action IS required.
            | Self::WalSuspend01TableSuspended
            // FEED-STALL-01 / FEED-SUPERVISOR-01 (2026-06-30) — the feed-agnostic
            // stall-watchdog killed+relaunched a silently-stalled sidecar, or the
            // supervisor task itself was respawned. High (a real recovery action
            // the operator must see; a flapping restart STORM points at a
            // persistent provider-side reject).
            | Self::FeedStall01SidecarRestarted
            | Self::FeedSupervisor01Respawned
            // FEED-REJECT-01 (2026-07-09) — a sidecar reject/error episode
            // opened; the coded line carries the bounded cause signature the
            // operator needs to triage a recurring reject loop. High: it
            // accompanies the HIGH GrowwSidecarRejected Telegram page.
            | Self::FeedReject01SidecarErrorDetail
            // BP-08 (2026-07-01) — fd / RSS / spill-free early-warning
            // monitors: page at 80% so the operator acts before exhaustion.
            | Self::Resource01FdCountHigh
            | Self::Resource02ResidentMemoryHigh
            | Self::Resource03SpillFreeLow
            // HTTP-CLIENT-01 (C2 2026-07-03) — a failed client build means the
            // host is under TLS/resolver/fd pressure; the site already
            // degraded gracefully, but the operator must see it (High).
            | Self::HttpClient01BuildFailed
            // AUTH-GAP-05 (2026-07-06) — sustained mid-session token-invalid
            // forced a re-mint. High: the re-mint IS the self-remediation
            // (auto-triage safe); the existing CRITICAL profile page covers
            // the unrecovered case.
            | Self::AuthGap05ForcedRemintTriggered
            // GROWW-SCALE-01/02 (§34 2026-07-03) — the auto-scale ladder
            // rolled back a failed rung / halved on fleet-wide failure. The
            // auto-correction already applied; the operator must see every
            // rollback (a repeat at the same rung = the discovered
            // server-side cap).
            | Self::GrowwScale01RollbackFired
            | Self::GrowwScale02GlobalHalve
            // SPOT1M-01/02 (operator grant 2026-07-12) — the per-minute
            // spot 1m REST fetch/persist degraded. High: the operator must
            // see a failing exchange-record pull (the escalation is
            // edge-triggered at 3 consecutive fully-failed minutes); never
            // a halt — the WS candle pipeline is untouched and re-appends
            // are DEDUP-idempotent.
            | Self::Spot1m01FetchDegraded
            | Self::Spot1m02PersistFailed
            // CHAIN-01..04 (PR-3, 2026-07-12) — the option-chain half of
            // the per-minute REST pipeline. High: entitlement absence /
            // sustained fetch degrade / persist failure / expirylist
            // failure all need operator eyes; never a halt — the WS
            // pipeline is untouched and re-appends are DEDUP-idempotent.
            | Self::Chain01EntitlementAbsent
            | Self::Chain02FetchDegraded
            | Self::Chain03PersistFailed
            | Self::Chain04ExpirylistFailed => Severity::High,
            // FUTIDX-01/02 (§36 2026-07-08) — per-underlying selection degrade
            // / cross-feed expiry divergence. Loud (Telegram High), never a
            // halt; the spot universe + both live feeds are unaffected.
            Self::Futidx01SelectionDegraded | Self::Futidx02CrossFeedExpiryMismatch => {
                Severity::High
            }
            // BRUTEX-XVERIFY-01/02 (2026-07-12) — daily cross-verify
            // divergence / degraded run. Loud (Telegram High), never a
            // halt; the live feeds + tick capture are unaffected.
            Self::BrutexXverify01DivergenceFound | Self::BrutexXverify02RunDegraded => {
                Severity::High
            }
            // TF-VERIFY-01/02 (operator 2026-07-13) — the daily
            // timeframe-consistency verifier found a TF-vs-1m divergence /
            // ran degraded. High: operator eyes required on every occurrence
            // (a divergence questions the higher-TF candle store; a degraded
            // run could not vouch for it); never a halt — the live candle
            // pipeline is untouched and reruns are DEDUP-idempotent.
            Self::TfVerify01MismatchFound | Self::TfVerify02RunDegraded => Severity::High,
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
            | Self::Audit02DepthRebalanceWriteFailed
            | Self::Audit03WsReconnectWriteFailed
            | Self::Audit04BootWriteFailed
            | Self::Audit05SelftestWriteFailed
            | Self::Audit06OrderWriteFailed
            | Self::AuditWs01EventWriteFailed
            | Self::StorageGap03AuditWriteFailed
            | Self::StorageGap04S3ArchiveFailed
            | Self::Telegram01Dropped
            | Self::AggregatorSeal01IlpFailed
            | Self::Boundary01CatchupSeal
            // GROWW-MASTER-01 (PR-A 2026-06-28): best-effort cold-path master
            // write failed; feed + ticks unaffected, next boot re-runs. Medium.
            | Self::GrowwMaster01PersistFailed
            // GROWW-SCALE-04 (§34 2026-07-03): best-effort groww_scale_audit
            // row write failed; ladder continues on in-memory state. Medium.
            | Self::GrowwScale04AuditWriteFailed
            // GROWW-NATIVE-01..04 (PR-R1 2026-07-04): shadow-only validation
            // client — failures lose comparison data for the window, never
            // production capture. Medium so the operator sees sustained
            // rates without paging on every live-probe reject.
            | Self::GrowwNative01ConnectFailed
            | Self::GrowwNative02AuthFailed
            | Self::GrowwNative03DecodeFailed
            | Self::GrowwNative04WriterFailed
            // SCOREBOARD-01 (2026-07-10): best-effort daily forensic
            // aggregate degraded; feeds/capture/trading unaffected, the
            // DEDUP-idempotent re-run backfills. Medium.
            | Self::Scoreboard01AggregationDegraded => Severity::Medium,
            // FEED-GAP-01 (2026-07-14): gap-episode forensics degraded —
            // annotation-only side record; capture/recovery unaffected. Medium.
            Self::FeedGap01EpisodeDegraded => Severity::Medium,
            // Low: trading-day / Dhan other
            // PR #6a (2026-05-19): I-P1-01 (DailyScheduler) + I-P1-02 (DeltaFieldCoverage) retired
            Self::InstrumentP2TradingDayGuard
            | Self::Dh910Other
            | Self::HotPath02WriterQueueDrop
            | Self::WsGap04PostCloseSleep
            | Self::DiskWatcher01Respawned
            | Self::AuthGap03TokenForceRenewedOnWake
            | Self::Telegram02CoalescerStateInconsistency
            // TELEGRAM-03 (2026-07-07): episode UX degradation only — the
            // never-drop delivery ladder is untouched, so Low.
            | Self::Telegram03EpisodeDegraded => Severity::Low,
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
            // C3 (2026-07-03): bounded chunked backpressure WAL re-injection
            Self::WsReinject01Aborted => ".claude/rules/project/ws-reinject-error-codes.md",
            // B6 (2026-07-03): off-thread tick ILP flush worker
            Self::TickFlush01WorkerRespawn => {
                ".claude/rules/project/tick-flush-worker-error-codes.md"
            }
            // W2 PR#6 (2026-07-10): per-table WAL-suspension probe
            Self::WalSuspend01TableSuspended => {
                ".claude/rules/project/wal-suspension-error-codes.md"
            }
            Self::WsGap04PostCloseSleep
            | Self::WsGap10OrderUpdateOutage
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
            Self::AuditWs01EventWriteFailed => {
                ".claude/rules/project/ws-event-audit-error-codes.md"
            }
            Self::Boot03ClockSkewExceeded => ".claude/rules/project/wave-2-c-error-codes.md",
            Self::Telegram01Dropped
            | Self::Telegram02CoalescerStateInconsistency
            | Self::Telegram03EpisodeDegraded => ".claude/rules/project/wave-3-error-codes.md",
            Self::Selftest01Passed | Self::Selftest02Failed => {
                ".claude/rules/project/wave-3-c-error-codes.md"
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
            Self::Resilience01DualInstanceDetected
            | Self::Proc01OomKillDetected
            // AUTH-P11 (2026-07-01) — TOTP secret rotated externally (promotes
            // the RESERVED AUTH-GAP-04 stub in wave-4-error-codes.md).
            | Self::AuthGap04TotpRotatedExternally
            // AUTH-GAP-05 (2026-07-06) — sustained mid-session token-invalid:
            // forced re-mint triggered (runbook §AUTH-GAP-05).
            | Self::AuthGap05ForcedRemintTriggered
            // BP-08 (2026-07-01) — fd / RSS / spill-free early-warning monitors
            // (promotes the RESERVED RESOURCE-01/02/03 stubs).
            | Self::Resource01FdCountHigh
            | Self::Resource02ResidentMemoryHigh
            | Self::Resource03SpillFreeLow => ".claude/rules/project/wave-4-error-codes.md",
            // 2026-07-04 dual-instance lock hardening (lock-before-mint)
            Self::Resilience03MintRefusedLockNotHeld => {
                ".claude/rules/project/dual-instance-lock-2026-07-04.md"
            }
            Self::OrphanPosition01Detected => {
                ".claude/rules/project/phase-0-item-20-error-codes.md"
            }
            Self::BarMismatch01CorrectedFromHistorical
            | Self::BarMismatch02CrossCheckInconclusive
            | Self::BarMismatch03CrossCheckFailed => {
                ".claude/rules/project/phase-0-items-15-28-29-error-codes.md"
            }
            // Operator 2026-06-10: daily tick-conservation audit
            Self::TickConserve01DailyResidual => {
                ".claude/rules/project/tick-conservation-audit-error-codes.md"
            }
            // Day OHLC tracker for IDX_I
            Self::IndexOhlc02DailyResetFailed => {
                ".claude/rules/project/index-day-ohlc-tracker-error-codes.md"
            }
            // PR-A (2026-06-28): Groww shared-master persist
            Self::GrowwMaster01PersistFailed => {
                ".claude/rules/project/groww-shared-master-error-codes.md"
            }
            // 2026-06-30: feed-agnostic sidecar stall-watchdog + supervisor respawn
            Self::FeedStall01SidecarRestarted
            | Self::FeedSupervisor01Respawned
            | Self::FeedReject01SidecarErrorDetail => {
                ".claude/rules/project/feed-stall-watchdog-error-codes.md"
            }
            // C2 (2026-07-03): panic-free reqwest client construction
            Self::HttpClient01BuildFailed => {
                ".claude/rules/project/http-client-error-codes.md"
            }
            // §34 (2026-07-03): Groww multi-connection auto-scale ladder
            Self::GrowwScale01RollbackFired
            | Self::GrowwScale02GlobalHalve
            | Self::GrowwScale03ShardOverlap
            | Self::GrowwScale04AuditWriteFailed
            | Self::GrowwScale05DualFleetDetected => {
                ".claude/rules/project/groww-scale-error-codes.md"
            }
            // PR-R1 (2026-07-04): Groww native-Rust shadow client
            Self::GrowwNative01ConnectFailed
            | Self::GrowwNative02AuthFailed
            | Self::GrowwNative03DecodeFailed
            | Self::GrowwNative04WriterFailed => {
                ".claude/rules/project/groww-native-rust-error-codes.md"
            }
            Self::Futidx01SelectionDegraded | Self::Futidx02CrossFeedExpiryMismatch => {
                ".claude/rules/project/futidx-4-error-codes.md"
            }
            // Dual-feed scoreboard PR-A (2026-07-10)
            Self::Scoreboard01AggregationDegraded => {
                ".claude/rules/project/dual-feed-scoreboard-error-codes.md"
            }
            // BruteX↔TickVault daily cross-verify (2026-07-12)
            Self::BrutexXverify01DivergenceFound | Self::BrutexXverify02RunDegraded => {
                ".claude/rules/project/brutex-crossverify-error-codes.md"
            }
            // Per-minute spot 1m REST pipeline (operator grant 2026-07-12)
            Self::Spot1m01FetchDegraded | Self::Spot1m02PersistFailed => {
                ".claude/rules/project/rest-1m-pipeline-error-codes.md"
            }
            // Per-minute option-chain REST pipeline (PR-3, 2026-07-12) —
            // one runbook for the whole per-minute REST pipeline family.
            Self::Chain01EntitlementAbsent
            | Self::Chain02FetchDegraded
            | Self::Chain03PersistFailed
            | Self::Chain04ExpirylistFailed => {
                ".claude/rules/project/rest-1m-pipeline-error-codes.md"
            }
            // Daily timeframe-consistency verifier (operator 2026-07-13)
            Self::TfVerify01MismatchFound | Self::TfVerify02RunDegraded => {
                ".claude/rules/project/tf-consistency-error-codes.md"
            }
            // Feed gap-episode forensics (Groww hardening PR-3, 2026-07-14)
            Self::FeedGap01EpisodeDegraded => {
                ".claude/rules/project/feed-gap-error-codes.md"
            }
        }
    }

    /// True when Claude Code auto-triage should attempt a fix before
    /// escalating. False means operator action is required — the auto-triage
    /// daemon will log a summary and stop.
    ///
    /// Conservative default: Critical errors are NEVER auto-actioned.
    ///
    /// Severity-independent overrides (design contracts pinning MANUAL
    /// triage on a non-Critical code):
    /// - `FUTIDX-02` (hostile-review round 3, 2026-07-08): a cross-feed
    ///   data-comparability verdict must never be auto-actioned — the
    ///   operator judges WHICH vendor master is stale (design contract
    ///   `is_auto_triage_safe() == false`; previously drifted to the
    ///   blanket severity derivation and papered over in runbook prose).
    /// - `WAL-SUSPEND-01` (W2 PR#6, 2026-07-10): the recovery action
    ///   `ALTER TABLE <t> RESUME WAL` is an OPERATOR decision — resuming
    ///   into a still-broken disk replays the failure; auto-triage must
    ///   never execute it.
    /// - `BRUTEX-XVERIFY-01` (2026-07-12): a cross-system (BruteX vs
    ///   TickVault) data-comparability verdict is an operator judgment —
    ///   the operator decides which capture chain is at fault (the
    ///   FUTIDX-02 precedent); auto-triage must never act on it.
    #[must_use]
    pub const fn is_auto_triage_safe(self) -> bool {
        if matches!(
            self,
            Self::Futidx02CrossFeedExpiryMismatch
                | Self::WalSuspend01TableSuspended
                | Self::BrutexXverify01DivergenceFound
                // CHAIN-01 (PR-3, 2026-07-12): restoring the option-chain
                // Data-API entitlement is an operator/broker ACCOUNT
                // decision — never auto-actioned despite High severity.
                | Self::Chain01EntitlementAbsent
                // TF-VERIFY-01 (operator 2026-07-13): a TF-vs-1m divergence
                // is a data-comparability VERDICT over the audit rows — the
                // operator judges whether it is a restart window, a real
                // aggregator bug, or a dead seal leg. An auto-triage rerun
                // would re-stamp rows and could mask the evidence (the
                // Futidx02 precedent).
                | Self::TfVerify01MismatchFound
        ) {
            return false;
        }
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
            Self::WsGap10OrderUpdateOutage,
            Self::DiskWatcher01Respawned,
            Self::WsSpill01WriterRespawn,
            Self::WsSpill02FrameDropped,
            Self::WsReinject01Aborted,
            Self::TickFlush01WorkerRespawn,
            Self::WalSuspend01TableSuspended,
            Self::Proc01OomKillDetected,
            Self::AuthGap03TokenForceRenewedOnWake,
            Self::AuthGap04TotpRotatedExternally,
            Self::AuthGap05ForcedRemintTriggered,
            Self::Boot01QuestDbSlow,
            Self::Boot02DeadlineExceeded,
            Self::Boot03ClockSkewExceeded,
            // PR #5 (2026-05-19): Audit01Phase2WriteFailed retired.
            Self::Audit02DepthRebalanceWriteFailed,
            Self::Audit03WsReconnectWriteFailed,
            Self::Audit04BootWriteFailed,
            Self::Audit05SelftestWriteFailed,
            Self::Audit06OrderWriteFailed,
            Self::AuditWs01EventWriteFailed,
            Self::StorageGap03AuditWriteFailed,
            Self::StorageGap04S3ArchiveFailed,
            Self::Telegram01Dropped,
            Self::Telegram02CoalescerStateInconsistency,
            Self::Telegram03EpisodeDegraded,
            Self::Selftest01Passed,
            Self::Selftest02Failed,
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
            // 2026-07-04 dual-instance lock hardening (lock-before-mint tripwire)
            Self::Resilience03MintRefusedLockNotHeld,
            // BP-08 (2026-07-01) — fd / RSS / spill-free early-warning monitors
            Self::Resource01FdCountHigh,
            Self::Resource02ResidentMemoryHigh,
            Self::Resource03SpillFreeLow,
            // Phase 0 Item 20 — orphan position 15:25 IST watchdog
            Self::OrphanPosition01Detected,
            // Phase 0 Items 15+28+29 — 09:16:05 IST post-open cross-check
            Self::BarMismatch01CorrectedFromHistorical,
            Self::BarMismatch02CrossCheckInconclusive,
            Self::BarMismatch03CrossCheckFailed,
            Self::TickConserve01DailyResidual,
            // Day OHLC tracker for IDX_I (Ticker mode)
            Self::IndexOhlc02DailyResetFailed,
            // PR-A (2026-06-28): Groww shared-master persist
            Self::GrowwMaster01PersistFailed,
            // 2026-06-30: feed-agnostic sidecar stall-watchdog + supervisor respawn
            Self::FeedStall01SidecarRestarted,
            Self::FeedSupervisor01Respawned,
            Self::FeedReject01SidecarErrorDetail,
            // C2 (2026-07-03): panic-free reqwest client construction
            Self::HttpClient01BuildFailed,
            // §34 (2026-07-03): Groww multi-connection auto-scale ladder
            Self::GrowwScale01RollbackFired,
            Self::GrowwScale02GlobalHalve,
            Self::GrowwScale03ShardOverlap,
            Self::GrowwScale04AuditWriteFailed,
            // Session-B fix (2026-07-04): Groww fleet dual-instance lock
            Self::GrowwScale05DualFleetDetected,
            // PR-R1 (2026-07-04): Groww native-Rust shadow client
            Self::GrowwNative01ConnectFailed,
            Self::GrowwNative02AuthFailed,
            Self::GrowwNative03DecodeFailed,
            Self::GrowwNative04WriterFailed,
            Self::Futidx01SelectionDegraded,
            Self::Futidx02CrossFeedExpiryMismatch,
            // Dual-feed scoreboard PR-A (2026-07-10)
            Self::Scoreboard01AggregationDegraded,
            // BruteX↔TickVault daily cross-verify (2026-07-12)
            Self::BrutexXverify01DivergenceFound,
            Self::BrutexXverify02RunDegraded,
            // Per-minute spot 1m REST pipeline (operator grant 2026-07-12)
            Self::Spot1m01FetchDegraded,
            Self::Spot1m02PersistFailed,
            // Per-minute option-chain REST pipeline (PR-3, 2026-07-12)
            Self::Chain01EntitlementAbsent,
            Self::Chain02FetchDegraded,
            Self::Chain03PersistFailed,
            Self::Chain04ExpirylistFailed,
            // Daily timeframe-consistency verifier (operator 2026-07-13)
            Self::TfVerify01MismatchFound,
            Self::TfVerify02RunDegraded,
            // Feed gap-episode forensics (Groww hardening PR-3, 2026-07-14)
            Self::FeedGap01EpisodeDegraded,
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
            let cs = code.code_str();
            assert!(seen.insert(cs), "duplicate code_str: {cs}");
        }
    }

    #[test]
    fn test_code_str_roundtrip_via_from_str() {
        for code in ErrorCode::all() {
            let cs = code.code_str();
            let parsed: ErrorCode = cs.parse().unwrap_or_else(|e| panic!("{cs}: {e:?}"));
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
            let cs = code.code_str();
            let path = code.runbook_path();
            assert!(!path.is_empty(), "runbook_path empty for {cs}");
            assert!(
                path.starts_with(".claude/"),
                "{cs} must point at .claude/: {path}"
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
                let cs = code.code_str();
                assert!(
                    !code.is_auto_triage_safe(),
                    "{cs} is Critical but auto-triage-safe"
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
        // 2026-06-10 (operator "Go ahead to achieve zero tick loss"): bumped
        // 105 -> 106 for TICK-CONSERVE-01 (daily WAL-vs-DB conservation audit).
        // 2026-06-10 (DHAN-REST-400): bumped 106 -> 107 for REST-CANARY-01
        // (scheduled REST-health probe failed).
        // 2026-06-12 (WS lifecycle audit table): bumped 107 -> 108 for
        // AUDIT-WS-01 (ws_event_audit row write failed — covers all 6 WS
        // lifecycle event kinds, future-proof for 5+5+5+1 connections).
        // 2026-06-26 (log-driven fixes): bumped 108 -> 109 for PREVDAY-01
        // (boot-time previous-day OHLCV fetch coverage EMPTY — typed +
        // per-empty observability for the 774-silent-empties signature).
        // 2026-06-26 (D2b — runtime Dhan-lane cold-start FSM): bumped 109 -> 113
        // for DHAN-LANE-01..04 (universe-build / ws-pool-spawn / auth-gate
        // failures + teardown-timeout on the runtime cold-start path).
        // 2026-06-28 (option_chain subsystem removal): bumped 113 -> 105 by
        // removing OPTION-CHAIN-01..08 (the entire option_chain REST subsystem
        // was deleted per operator directive — disabled since 2026-06-02 with
        // no live consumer; its QuestDB table was dropped 2026-06-23).
        // 2026-06-28 (PR-A Groww shared-master): bumped 105 -> 106 for
        // GROWW-MASTER-01 (Groww instrument persist into the shared
        // instrument_lifecycle + index_constituency tables, feed='groww').
        // 2026-06-30 (WS-429-cooldown): bumped 106 -> 107 for WS-GAP-08
        // (persisted Dhan 429 rate-limit cooldown — survives process restart).
        // 2026-06-30 (feed-agnostic self-heal): bumped 107 -> 109 for
        // FEED-STALL-01 (silently-stalled sidecar killed+relaunched) +
        // FEED-SUPERVISOR-01 (supervisor task respawned).
        // 2026-06-30 (Dhan reconnect hardening Fix A): bumped 109 -> 110 for
        // WS-GAP-09 (watchdog reconnect-in-place on the bare-Dhan-reset class
        // instead of process::exit + 775-SID re-subscribe → 429).
        // 2026-07-01 (BP-07 / Wave-4-E1): bumped 110 -> 111 for PROC-01
        // (OOM-kill monitor — cgroup-v2 memory.events oom_kill vs boot baseline).
        // 2026-07-01 (audit sweep): bumped 111 -> 115 for AUTH-GAP-04 (AUTH-P11)
        // + RESOURCE-01/02/03 (BP-08 fd / RSS / spill-free monitors).
        // 2026-07-03 (SLO publisher supervisor): bumped 115 -> 116 for SLO-03
        // (the 10s tv_realtime_guarantee_score publisher died silently at
        // 10:35 IST mid-market; now supervised + respawned).
        // 2026-07-03 (B6 latency-histogram split): bumped 116 -> 117 for
        // TICK-FLUSH-01 (off-thread tick ILP flush worker respawned).
        // 2026-07-03 (C3 re-injection storm fix): bumped 117 -> 118 for
        // WS-REINJECT-01 (boot WAL re-injection aborted — chunked
        // backpressure replaces the silent try_send drop storm).
        // 2026-07-03 (C2 panic-free reqwest client): bumped 118 -> 119 for
        // HTTP-CLIENT-01 (ClientBuilder::build failed — typed degrade
        // replaces the Client::new() panic fallback at 8 storage sites).
        // 2026-07-03 (§34 Groww multi-connection auto-scale): bumped 119 -> 123
        // for GROWW-SCALE-01 (ladder rollback fired) + GROWW-SCALE-02
        // (fleet-wide failure → global cooldown + halve) + GROWW-SCALE-03
        // (shard disjointness/coverage contract violated) + GROWW-SCALE-04
        // (groww_scale_audit row write failed, best-effort).
        // 2026-07-04 (dual-instance lock hardening): bumped 123 -> 124 for
        // RESILIENCE-03 (generateAccessToken mint refused — instance lock
        // not held; lock-before-mint tripwire, operator "go" 2026-07-04).
        // 2026-07-04 (PR-R1 Groww native-Rust shadow client): bumped 124 -> 128
        // for GROWW-NATIVE-01 (connect/reconnect/respawn) + GROWW-NATIVE-02
        // (socket-token mint / CONNECT auth rejected) + GROWW-NATIVE-03
        // (NATS/proto decode failure) + GROWW-NATIVE-04 (shadow NDJSON
        // writer/rotation/channel failure).
        // 2026-07-04 (Session-B fix — Groww fleet dual-instance lock):
        // bumped 128 -> 129 for GROWW-SCALE-05 (dual scale-fleet instance
        // detected / SSM lock unprovable — fleet spawn refused fail-closed,
        // single-connection fallback).
        // 2026-07-06 (order-update outage paging PR-1): bumped 129 -> 130 for
        // WS-GAP-10 (order-update in-market outage — the reachable in-loop
        // [HIGH] page; the old task-exit emit was dead code since WS-GAP-04).
        // 2026-07-07 (Telegram UX overhaul — episode live-edit coalescing):
        // bumped 130 -> 131 for TELEGRAM-03 (episode machinery degraded:
        // store_write_failed / rehydrate_corrupt / edit_fallback_storm —
        // delivery unaffected, UX-only degrade, Severity::Low).
        // 2026-07-06 (AUTH-GAP-05 token self-heal): bumped 131 -> 132 for
        // AUTH-GAP-05 (sustained mid-session token-invalid — forced re-mint
        // triggered via the existing renewal machinery; lock-before-mint +
        // ~125s cooldown + retry-once latch honored).
        // 2026-07-08 (§36 FUTIDX-4): bumped 132 -> 134 for FUTIDX-01
        // (per-underlying nearest-expiry selection degraded, per feed) +
        // FUTIDX-02 (cross-feed expiry mismatch) — both Severity::High.
        // 2026-07-08 (AUTH-GAP-06 fast-boot cached-token validation):
        // bumped 134 -> 135 — one GET /v2/profile validates the cached JWT
        // before any WebSocket spawn on the fast crash-recovery arm; a
        // prefix-anchored 401/403 forces a re-mint via the existing
        // TokenManager machinery (2026-07-07 third morning outage).
        // 2026-07-09 (Groww reject-loop hardening): bumped 135 -> 136 for
        // FEED-REJECT-01 — bounded, secret-redacted sidecar reject-cause
        // signature surfaced at the once-per-child alert edge (the all-day
        // 09:22/14:17 IST reject loop was invisible in the coded stream).
        // 2026-07-10 (W2 PR#6, audit follow-up row 10): bumped 136 -> 137
        // for WAL-SUSPEND-01 — per-table QuestDB WAL-apply suspension probe
        // (a suspended table keeps ACKing ILP rows while they silently stop
        // becoming visible; previously zero signal).
        // 2026-07-10 (dual-feed scoreboard PR-A): bumped 137 -> 138 for
        // SCOREBOARD-01 — the daily 15:45 IST Dhan-vs-Groww scoreboard
        // aggregation degraded (best-effort forensic aggregate; sentinels,
        // never fabricated zeros; DEDUP-idempotent re-run backfills).
        // 2026-07-12 (BruteX crossverify Commit 1): bumped 138 -> 140 for
        // BRUTEX-XVERIFY-01 (daily BruteX-vs-live 1m divergence found —
        // High, NOT auto-triage-safe: a data-comparability signal is never
        // auto-actioned) + BRUTEX-XVERIFY-02 (run degraded — S3/CSV/QuestDB
        // leg failed; keep-better guard + DEDUP-idempotent re-run backfills).
        // 2026-07-12 (per-minute spot 1m REST pipeline PR-2): bumped
        // 140 -> 142 for SPOT1M-01 (per-minute spot fetch degraded — edge-
        // triggered escalation) + SPOT1M-02 (spot_1m_rest persist failed —
        // best-effort, DEDUP-idempotent re-append).
        // 2026-07-12 (per-minute option-chain REST pipeline PR-3): bumped
        // for CHAIN-01 (entitlement absent — once-per-day edge,
        // manual triage) + CHAIN-02 (per-minute chain fetch degraded —
        // edge-triggered escalation) + CHAIN-03 (option_chain_1m persist
        // failed — best-effort, DEDUP-idempotent) + CHAIN-04 (day-start
        // expirylist warmup failed — pipeline disabled-for-the-day, never
        // a guessed expiry).
        // 2026-07-12 merge note: BRUTEX-XVERIFY (2) + SPOT1M (2) landed on
        // this branch at 142; main's CHAIN-01..04 (4) merge in => 146.
        // 2026-07-13 (daily timeframe-consistency verifier, merged from
        // main): TF-VERIFY-01 (higher-TF candle disagrees with its
        // recomputed-from-1m value — coalesced per (feed, date) pass,
        // manual triage) + TF-VERIFY-02 (the daily run degraded —
        // client/query/truncation/flush/budget stage taxonomy) => 148.
        // 2026-07-14 (Groww hardening PR-3): FEED-GAP-01 (gap-episode
        // forensics degraded — annotation-only side record) => 149.
        // 2026-07-15 (C4 sweep — Dhan live-WS retirement, operator
        // 2026-07-13): DROPPED 149 -> 127. The 22 zero-emit-site variants
        // retained through Phases A/B/C1/C2/C3 were deleted:
        // INSTR-FETCH-01..04, NTM-CONSTITUENCY-01, PREVDAY-01,
        // CROSS-VERIFY-1M-01/-02, DHAN-LANE-01..04, WS-GAP-05..09,
        // AUTH-GAP-06, REST-CANARY-01, SLO-01/02/03 (WS-GAP-04 +
        // WS-GAP-10 survive — emitted by the retained-dormant
        // order_update_connection.rs; main's FEED-GAP-01 landed
        // mid-flight, hence 149 -> 127, not 148 -> 126).
        assert_eq!(ErrorCode::all().len(), 127);
    }

    #[test]
    fn test_telegram_03_episode_degraded_contract() {
        // Telegram UX overhaul (2026-07-07): episode live-edit machinery
        // degrade signal. Low + auto-triage-safe — delivery is never at
        // risk (the fallback ladder terminates at TELEGRAM-01 loudness).
        let code = ErrorCode::Telegram03EpisodeDegraded;
        assert_eq!(code.code_str(), "TELEGRAM-03");
        assert_eq!("TELEGRAM-03".parse::<ErrorCode>(), Ok(code));
        assert_eq!(code.severity(), Severity::Low);
        assert!(code.is_auto_triage_safe());
        assert_eq!(
            code.runbook_path(),
            ".claude/rules/project/wave-3-error-codes.md"
        );
    }

    #[test]
    fn test_groww_scale_05_dual_fleet_contract() {
        // Session-B fix (operator go 2026-07-04): the Groww scale fleet's
        // dual-instance lock refusal signal. Critical + never auto-triaged —
        // the operator must pick which host runs the scale test.
        let code = ErrorCode::GrowwScale05DualFleetDetected;
        assert_eq!(code.code_str(), "GROWW-SCALE-05");
        assert_eq!("GROWW-SCALE-05".parse::<ErrorCode>(), Ok(code));
        assert_eq!(code.severity(), Severity::Critical);
        assert!(!code.is_auto_triage_safe());
        assert_eq!(
            code.runbook_path(),
            ".claude/rules/project/groww-scale-error-codes.md"
        );
        let abs = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .map(|root| root.join(code.runbook_path()))
            .expect("workspace root");
        let shown = abs.display().to_string();
        assert!(
            abs.exists(),
            "GROWW-SCALE-05 runbook missing on disk: {shown}"
        );
        assert!(ErrorCode::all().contains(&code));
    }

    #[test]
    fn test_futidx_codes_contract() {
        // §36 FUTIDX-4 (hostile-review round 3, 2026-07-08).
        let f1 = ErrorCode::Futidx01SelectionDegraded;
        assert_eq!(f1.code_str(), "FUTIDX-01");
        assert_eq!("FUTIDX-01".parse::<ErrorCode>(), Ok(f1));
        assert_eq!(f1.severity(), Severity::High);
        // Per-feed degrade self-heals next boot — auto-triage may inspect.
        assert!(f1.is_auto_triage_safe());

        let f2 = ErrorCode::Futidx02CrossFeedExpiryMismatch;
        assert_eq!(f2.code_str(), "FUTIDX-02");
        assert_eq!("FUTIDX-02".parse::<ErrorCode>(), Ok(f2));
        assert_eq!(f2.severity(), Severity::High);
        // Design contract (futidx-4-error-codes.md §2 + the R3-4 record in
        // .claude/plans/archive/2026-07-08-futidx-4.md — the in-repo
        // authority (archived from active-plan-futidx-4.md per
        // plan-enforcement rule 7; path synced 2026-07-13, deferred from
        // the Groww REST-1m PR-1);
        // round 4 replaced a dangling scratchpad "FINAL.md" citation that
        // never landed in the tree): a cross-feed comparability
        // verdict is NEVER auto-actioned despite being non-Critical — the
        // severity-independent override arm, not the blanket derivation.
        assert!(!f2.is_auto_triage_safe());
        assert_eq!(
            f2.runbook_path(),
            ".claude/rules/project/futidx-4-error-codes.md"
        );
    }

    #[test]
    fn test_chain_codes_contract() {
        // Per-minute option-chain REST pipeline (PR-3, 2026-07-12).
        let c1 = ErrorCode::Chain01EntitlementAbsent;
        assert_eq!(c1.code_str(), "CHAIN-01");
        assert_eq!("CHAIN-01".parse::<ErrorCode>(), Ok(c1));
        assert_eq!(c1.severity(), Severity::High);
        // Design contract: the entitlement is an operator/broker ACCOUNT
        // decision — NEVER auto-actioned despite being non-Critical (the
        // FUTIDX-02 severity-independent override precedent).
        assert!(!c1.is_auto_triage_safe());

        for (code, s) in [
            (ErrorCode::Chain02FetchDegraded, "CHAIN-02"),
            (ErrorCode::Chain03PersistFailed, "CHAIN-03"),
            (ErrorCode::Chain04ExpirylistFailed, "CHAIN-04"),
        ] {
            assert_eq!(code.code_str(), s);
            assert_eq!(s.parse::<ErrorCode>(), Ok(code));
            assert_eq!(code.severity(), Severity::High);
            // Degrades self-heal (next minute / next trading-day boot) —
            // auto-triage may inspect.
            assert!(code.is_auto_triage_safe());
            assert!(ErrorCode::all().contains(&code));
        }
        // The whole family shares the per-minute REST pipeline runbook.
        assert_eq!(
            c1.runbook_path(),
            ".claude/rules/project/rest-1m-pipeline-error-codes.md"
        );
    }

    #[test]
    fn test_tf_verify_codes_contract() {
        // Daily timeframe-consistency verifier (operator 2026-07-13).
        let v1 = ErrorCode::TfVerify01MismatchFound;
        assert_eq!(v1.code_str(), "TF-VERIFY-01");
        assert_eq!("TF-VERIFY-01".parse::<ErrorCode>(), Ok(v1));
        assert_eq!(v1.severity(), Severity::High);
        // Design contract: a TF-vs-1m divergence is a data-comparability
        // VERDICT — NEVER auto-actioned despite being non-Critical (the
        // FUTIDX-02 severity-independent override precedent).
        assert!(!v1.is_auto_triage_safe());

        let v2 = ErrorCode::TfVerify02RunDegraded;
        assert_eq!(v2.code_str(), "TF-VERIFY-02");
        assert_eq!("TF-VERIFY-02".parse::<ErrorCode>(), Ok(v2));
        assert_eq!(v2.severity(), Severity::High);
        // The degrade already happened; the next trading day re-runs and
        // forced reruns are DEDUP-idempotent — auto-triage may inspect.
        assert!(v2.is_auto_triage_safe());

        for code in [v1, v2] {
            assert!(ErrorCode::all().contains(&code));
            assert_eq!(
                code.runbook_path(),
                ".claude/rules/project/tf-consistency-error-codes.md"
            );
        }
    }

    // C4 sweep (2026-07-15): test_prev_day_01_coverage_empty_contract died
    // with its PrevDay01CoverageEmpty variant (Dhan live-WS retirement).

    #[test]
    fn test_ws_reinject_01_aborted_contract() {
        let code = ErrorCode::WsReinject01Aborted;
        // Wire-format string + roundtrip via FromStr.
        assert_eq!(code.code_str(), "WS-REINJECT-01");
        assert_eq!("WS-REINJECT-01".parse::<ErrorCode>(), Ok(code));
        // High (abort self-heals via next boot's WAL re-replay) and
        // therefore auto-triage-safe.
        assert_eq!(code.severity(), Severity::High);
        assert!(code.is_auto_triage_safe());
        // Runbook points at the dedicated rule file and exists on disk.
        assert_eq!(
            code.runbook_path(),
            ".claude/rules/project/ws-reinject-error-codes.md"
        );
        let abs = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .map(|root| root.join(code.runbook_path()))
            .expect("workspace root");
        let shown = abs.display().to_string();
        assert!(
            abs.exists(),
            "WS-REINJECT-01 runbook missing on disk: {shown}"
        );
        // Listed in the catalogue.
        assert!(ErrorCode::all().contains(&code));
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
                // DHAN-REST-400 (2026-06-10): scheduled REST-health canary.
                || s.starts_with("REST-CANARY-")
                // B6 (2026-07-03): off-thread tick ILP flush worker.
                || s.starts_with("TICK-FLUSH-")
                // W2 PR#6 (2026-07-10): per-table WAL-suspension probe.
                || s.starts_with("WAL-SUSPEND-")
                // PR #450 commit 8b (2026-05-03): prev_oi cache state.
                || s.starts_with("PREVOI-")
                // Wave 6 Sub-PR #1: multi-TF aggregator + boundary timer.
                || s.starts_with("AGGREGATOR-")
                || s.starts_with("BOUNDARY-")
                // Wave 4 Item 19 (Phase 0): dual-instance lock.
                || s.starts_with("RESILIENCE-")
                // BP-08 (audit 2026-07-01): fd / RSS / spill-free monitors.
                || s.starts_with("RESOURCE-")
                // Phase 0 Item 20: orphan position 15:25 IST watchdog.
                || s.starts_with("ORPHAN-POSITION-")
                // Phase 0 Items 15+28+29: 09:16:05 IST post-open cross-check.
                || s.starts_with("BAR-MISMATCH-")
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
                // C3 2026-07-03: bounded chunked-backpressure WAL re-injection
                || s.starts_with("WS-REINJECT-")
                // NTM Sub-PR #10a (§31): niftyindices constituent source degrade
                || s.starts_with("NTM-CONSTITUENCY-")
                // Operator 2026-06-10: daily end-to-end tick-conservation audit
                || s.starts_with("TICK-CONSERVE-")
                // PR4 2026-06-01: boot-time previous-day OHLCV fetch coverage
                || s.starts_with("PREVDAY-")
                // D2b 2026-06-26: runtime Dhan-lane cold-start FSM
                || s.starts_with("DHAN-LANE-")
                // PR-A 2026-06-28: Groww shared-master persist
                || s.starts_with("GROWW-MASTER-")
                // §34 (2026-07-03): Groww multi-connection auto-scale ladder
                || s.starts_with("GROWW-SCALE-")
                // PR-R1 (2026-07-04): Groww native-Rust shadow client
                || s.starts_with("GROWW-NATIVE-")
                // 2026-06-30: feed-agnostic sidecar stall-watchdog + respawn
                || s.starts_with("FEED-STALL-")
                || s.starts_with("FEED-SUPERVISOR-")
                // 2026-07-09: bounded sidecar reject-cause signature surfacing
                || s.starts_with("FEED-REJECT-")
                // Wave-4-E1 / BP-07 (2026-07-01): OOM-kill monitor.
                || s.starts_with("PROC-")
                // C2 (2026-07-03): panic-free reqwest client construction.
                || s.starts_with("HTTP-CLIENT-")
                // §36 (2026-07-08; §36.7 all-months 2026-07-10): FUTIDX
                // index futures.
                || s.starts_with("FUTIDX-")
                // Dual-feed scoreboard PR-A (2026-07-10).
                || s.starts_with("SCOREBOARD-")
                // BruteX↔TickVault daily cross-verify (2026-07-12).
                || s.starts_with("BRUTEX-XVERIFY-")
                // Per-minute spot 1m REST pipeline (operator grant 2026-07-12).
                || s.starts_with("SPOT1M-")
                // Per-minute option-chain REST pipeline (PR-3, 2026-07-12).
                || s.starts_with("CHAIN-")
                // Operator 2026-07-13: daily timeframe-consistency verifier
                || s.starts_with("TF-VERIFY-")
                // Groww hardening PR-3 (2026-07-14): feed gap-episode
                // forensics.
                || s.starts_with("FEED-GAP-");
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
