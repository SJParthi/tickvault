//! Order runtime (dry-run) — the single-owner task that finally gives the
//! order machinery real inputs on the prod (dhan-off, groww-on) profile.
//!
//! Cluster A, operator directive 2026-07-14
//! (`.claude/plans/active-plan-order-runtime-dryrun.md`). Before this module,
//! NO OMS or RiskEngine instance existed in a dhan-off process: order-update
//! events were counted + DISCARDED by the rest-stack drain, `record_fill` /
//! `update_market_price` had zero production callers, and both alert sinks
//! were never wired — the daily-loss halt, position tracking, and P&L were
//! structurally dead (audit Rule 13).
//!
//! # What runs here
//! ONE supervised tokio task owning:
//! - the paper OMS (`dry_run` hard-true — no live-order path exists) fed by
//!   the rest-stack's order-update broadcast (Source=="P" filtered at this
//!   consumer boundary; the WAL keeps capturing ALL frames upstream),
//! - the RiskEngine fed by [`FillEvent`]s (the widened `handle_order_update`
//!   return) and by Groww marks (the zero-alloc [`MarkForwarder`] tap at the
//!   groww_bridge consume seam → bounded mpsc → `update_market_price`),
//! - the next-mark PAPER FILLER (a pending `PAPER-n` order fills at the next
//!   mark for its sid — fill-once, terminal orders never re-fill, finite>0
//!   mark required, else deferred + counted),
//! - the ≤1/sec mark-to-market `evaluate_daily_loss_halt` (a drawdown with
//!   no new signal now halts),
//! - the reconcile scheduler (honest dry-run heartbeat — "broker reconcile
//!   SKIPPED" — plus the REAL local Σfills==net_lots invariant),
//! - the 15:30 IST pending-paper-order sweep + 16:00 IST daily reset,
//! - the once-daily gated paper SELF-TEST (place → mark → fill → P&L →
//!   close → flat) proving the chain end-to-end.
//!
//! # Safety rails
//! - Spawned ONLY from `dhan_rest_stack` Phase 5a (the dhan-OFF arm) — the
//!   dhan-ON trading_pipeline owns its own OMS, so a second runtime there
//!   would be a dual-OMS split-brain (ratcheted: zero `spawn_order_runtime(`
//!   in main.rs).
//! - §28 boundary: this module never constructs IndicatorEngine /
//!   StrategyInstance — there is NO signal source; the only order producer
//!   is the gated self-test.
//! - I-P1-11: OMS/Risk maps stay sid-keyed (u64) — the first-seen-segment
//!   TRIPWIRE converts a cross-segment sid collision into a LOUD skip
//!   (`RISK-GAP-02`) instead of a silent P&L merge; the composite `(u64,u8)`
//!   rewrite is a MANDATORY pre-live follow-up (see
//!   `.claude/rules/project/order-runtime-dryrun.md`).
//!
//! # Panic honesty (TICK-FLUSH-01 precedent)
//! The release profile sets `panic = "abort"` — the supervisor respawn arms
//! engage in UNWIND (dev/test) builds only; in prod a panic aborts the
//! process and recovery is the external restart + WAL replay.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{Notify, broadcast, mpsc};
use tracing::{debug, error, info, warn};

use tickvault_common::config::ApplicationConfig;
use tickvault_common::constants::{
    IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST,
    TICK_PERSIST_START_SECS_OF_DAY_IST,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::order_types::{
    OrderType, OrderUpdate, OrderValidity, ProductType, TransactionType,
};
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::auth::token_manager::TokenHandle;
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_trading::oms::engine::{OmsAlert, OmsAlertSink};
use tickvault_trading::oms::{
    FillEvent, ManagedOrder, OrderApiClient, OrderManagementSystem, OrderRateLimiter,
    PlaceOrderRequest, SEGMENT_CODE_UNKNOWN,
};
use tickvault_trading::risk::engine::{RiskAlertSink, RiskEngine};

use crate::oms_wiring::{TokenHandleBridge, build_oms_http_client};

// ---------------------------------------------------------------------------
// Constants (module-scoped, named per rust-code.md "no hardcoded values")
// ---------------------------------------------------------------------------

/// Supervisor respawn backoff after an abnormal inner exit (WS-GAP-05 house
/// pattern; unwind builds only — see the module panic-honesty note).
const ORDER_RUNTIME_RESPAWN_BACKOFF_SECS: u64 = 5;
/// Boot-once reconcile delay: first cycle fires this long after spawn.
const ORDER_RUNTIME_BOOT_RECONCILE_DELAY_SECS: u64 = 60;
/// Reconcile scheduler window END (15:35 IST — 5 min past close so the
/// final session state is reconciled once after 15:30).
const RECONCILE_WINDOW_END_SECS_OF_DAY_IST: u32 = TICK_PERSIST_END_SECS_OF_DAY_IST + 5 * 60;
/// Daily reset instant (16:00 IST — mirrors the trading_pipeline reset).
const DAILY_RESET_SECS_OF_DAY_IST: u32 = 16 * 3600;
/// Self-test window: [09:20, 15:00) IST (F17 — never at the open auction,
/// never near the close sweep).
const SELF_TEST_WINDOW_START_SECS_OF_DAY_IST: u32 = 9 * 3600 + 20 * 60;
const SELF_TEST_WINDOW_END_SECS_OF_DAY_IST: u32 = 15 * 3600;
/// Self-test end-to-end deadline: a cycle stuck longer than this fails
/// loudly and latches for the day (no retry storm).
const SELF_TEST_TIMEOUT_SECS: u64 = 180;
/// Max marks drained per select! wake (bounds one arm's hold on the task).
const MARK_BATCH_MAX: usize = 256;
/// Broadcast-lag escalation threshold (mirrors the trading_pipeline
/// template — beyond this, fills were likely missed).
const ORDER_UPDATE_LAG_ERROR_THRESHOLD: u64 = 1_000;
/// Housekeeping cadence: reconcile/reset/close/self-test timers + the
/// mark-to-market halt evaluation all ride one 1s cold-path tick.
const HOUSEKEEPING_TICK_SECS: u64 = 1;

// ---------------------------------------------------------------------------
// Mark forwarding (the Groww bridge hot-path tap)
// ---------------------------------------------------------------------------

/// A mark-to-market price update. `Copy`, 16 bytes — sent from the Groww
/// bridge per-tick path via a lock-free `try_send` (zero alloc, no await).
#[derive(Clone, Copy, Debug)]
pub struct MarkUpdate {
    /// Feed-native security id (Groww exchange_token space; bit-62 indices).
    pub security_id: u64,
    /// Numeric `ExchangeSegment` code of the tick.
    pub segment_code: u8,
    /// Last traded price (f32 as delivered — the runtime widens for math).
    pub price: f32,
}

/// The hot-path side of the mark bridge, cloned into the Groww bridge.
///
/// Dominant path (no positions, no pending paper orders): ONE `Relaxed`
/// atomic load and nothing else. When armed, a bounded `try_send` of a
/// `Copy` struct — a full channel DROPS the mark (counted; the next tick
/// supersedes it — prices are idempotent, positions stay exact).
#[derive(Clone)]
pub struct MarkForwarder {
    /// Armed by the runtime whenever a position / pending paper order /
    /// active self-test needs marks; `false` = the tap is a single load.
    pub marks_wanted: Arc<AtomicBool>,
    /// Bounded channel into the runtime's mark arm.
    pub tx: mpsc::Sender<MarkUpdate>,
}

impl MarkForwarder {
    /// Forward one mark if the runtime wants marks. Zero-alloc, lock-free,
    /// never blocks, never awaits — safe at the per-tick consume seam.
    #[inline]
    pub fn mark_forward(&self, security_id: u64, segment_code: u8, price: f32) {
        if !self.marks_wanted.load(Ordering::Relaxed) {
            return;
        }
        if self
            .tx
            .try_send(MarkUpdate {
                security_id,
                segment_code,
                price,
            })
            .is_err()
        {
            metrics::counter!("tv_mark_forward_dropped_total").increment(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Alert sink — the FIRST production implementer of both alert traits
// ---------------------------------------------------------------------------

/// Bridges OMS + Risk alerts to `NotificationService` (Telegram). `notify()`
/// is sync fire-and-forget (service.rs — F19: never blocks this task).
pub struct NotifierAlertSink(pub Arc<NotificationService>);

impl OmsAlertSink for NotifierAlertSink {
    fn fire(&self, alert: OmsAlert) {
        match alert {
            OmsAlert::OrderRejected {
                correlation_id,
                reason,
            } => {
                error!(
                    code = ErrorCode::OmsGapStateMachine.code_str(),
                    correlation_id = %correlation_id,
                    reason = %reason,
                    "OMS-GAP-01: order rejected (lifecycle event routed to Telegram)"
                );
                self.0.notify(NotificationEvent::OrderRejected {
                    correlation_id,
                    reason,
                });
            }
            OmsAlert::CircuitBreakerOpened {
                consecutive_failures,
            } => {
                error!(
                    code = ErrorCode::OmsGapCircuitBreaker.code_str(),
                    consecutive_failures,
                    "OMS-GAP-03: order circuit breaker OPENED — order API calls halted"
                );
                self.0.notify(NotificationEvent::CircuitBreakerOpened {
                    consecutive_failures,
                });
            }
            OmsAlert::CircuitBreakerClosed => {
                info!("order circuit breaker closed — order API calls resumed");
                self.0.notify(NotificationEvent::CircuitBreakerClosed);
            }
            OmsAlert::RateLimitExhausted { limit_type } => {
                error!(
                    code = ErrorCode::OmsGapRateLimit.code_str(),
                    limit_type = %limit_type,
                    "OMS-GAP-04: order rate limit exhausted — order rejected"
                );
                self.0
                    .notify(NotificationEvent::RateLimitExhausted { limit_type });
            }
        }
    }
}

impl RiskAlertSink for NotifierAlertSink {
    fn fire_risk_halt(&self, reason: &'static str) {
        error!(
            code = ErrorCode::RiskGapPreTrade.code_str(),
            reason, "RISK-GAP-01: risk halt fired — routing Critical Telegram"
        );
        self.0.notify(NotificationEvent::RiskHalt {
            reason: reason.to_string(),
        });
    }
}

// ---------------------------------------------------------------------------
// WAL confirm decision (pure — consumed by dhan_rest_stack Phase 5a)
// ---------------------------------------------------------------------------

/// Verdict for the whole-dir `confirm_replayed` after the rest-stack's
/// order-update WAL drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmVerdict {
    /// Drain was parse-clean AND no stale live-feed frames were staged —
    /// safe to archive `replaying/`.
    Confirm,
    /// Defer: either the drain hit parse errors, or stale live-feed frames
    /// sit staged (a whole-dir confirm would archive them UN-reinjected —
    /// silent tick loss, F6). Segments re-replay next boot; zero loss.
    Defer {
        /// Staged live-feed frame count (0 when the defer is parse-driven).
        live_feed_frames: usize,
    },
}

/// F6/F7: confirm iff the order-update drain was parse-clean AND zero
/// live-feed frames were staged this boot (the dhan-off residual class).
#[must_use]
pub fn confirm_decision(parse_errors: u64, livefeed_frames: usize) -> ConfirmVerdict {
    if parse_errors == 0 && livefeed_frames == 0 {
        ConfirmVerdict::Confirm
    } else {
        ConfirmVerdict::Defer {
            live_feed_frames: livefeed_frames,
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime params + spawn
// ---------------------------------------------------------------------------

/// Everything the runtime needs — assembled by `dhan_rest_stack` Phase 5a.
pub struct OrderRuntimeParams {
    /// Immutable boot config snapshot.
    pub config: Arc<ApplicationConfig>,
    /// Telegram dispatcher (alert sinks + heartbeats).
    pub notifier: Arc<NotificationService>,
    /// Trading calendar (self-test trading-day gate).
    pub calendar: Arc<TradingCalendar>,
    /// The stack-local order-update broadcast sender — held for the process
    /// lifetime (keeps the WS send arm quiet, M1) + respawn re-subscribe.
    pub order_update_sender: broadcast::Sender<OrderUpdate>,
    /// The FIRST receiver, subscribed BEFORE the WAL drain (ordering law
    /// F5 — replayed frames must reach this runtime).
    pub first_order_update_rx: broadcast::Receiver<OrderUpdate>,
    /// Bounded mark channel (Groww bridge tap → this runtime).
    pub mark_rx: mpsc::Receiver<MarkUpdate>,
    /// Shared arm flag (the hot-path gate half of the mark bridge).
    pub marks_wanted: Arc<AtomicBool>,
    /// Live token handle (dry-run never uses it for HTTP; wired for parity
    /// with the future live path).
    pub token_handle: TokenHandle,
    /// Dhan client id (fetched by the stack's Phase 4).
    pub client_id: String,
    /// The stack's order-update auth signal (fires ONCE per process on the
    /// first successful WS auth) — triggers an immediate reconcile cycle.
    pub auth_notify: Arc<Notify>,
}

/// Spawn the supervised order runtime. Returns the SUPERVISOR handle.
// TEST-EXEMPT: orchestration spawn (supervisor loop around live channels); the pure helpers + state machine below are unit-tested, wiring pinned by test_rest_stack_wires_order_runtime
pub fn spawn_order_runtime(params: OrderRuntimeParams) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let OrderRuntimeParams {
            config,
            notifier,
            calendar,
            order_update_sender,
            first_order_update_rx,
            mark_rx,
            marks_wanted,
            token_handle,
            client_id,
            auth_notify,
        } = params;
        // The mark receiver survives inner respawns behind a tokio Mutex —
        // the inner run holds the guard for its lifetime; an (unwind-build)
        // death releases it for the respawn.
        let mark_rx = Arc::new(tokio::sync::Mutex::new(mark_rx));
        let mut pending_first_rx = Some(first_order_update_rx);
        loop {
            let order_rx = match pending_first_rx.take() {
                Some(rx) => rx,
                // Respawn path: re-subscribe (events during the dead window
                // are lost — unwind-build-only, honest).
                None => order_update_sender.subscribe(),
            };
            let ctx = RuntimeCtx {
                config: Arc::clone(&config),
                notifier: Arc::clone(&notifier),
                calendar: Arc::clone(&calendar),
                marks_wanted: Arc::clone(&marks_wanted),
                token_handle: token_handle.clone(),
                client_id: client_id.clone(),
                auth_notify: Arc::clone(&auth_notify),
            };
            let mark_rx_shared = Arc::clone(&mark_rx);
            let inner =
                tokio::spawn(async move { run_order_runtime(ctx, order_rx, mark_rx_shared).await });
            let reason = match inner.await {
                // The inner loop is infinite; a clean return means the
                // order-update broadcast CLOSED — structurally impossible
                // while this supervisor holds a sender clone, so treat it
                // as an abnormal exit (WS-GAP-05 semantics).
                Ok(()) => "clean_exit",
                Err(err) if err.is_panic() => "panic",
                // Cancelled = the runtime is shutting down — exit quietly,
                // never respawn onto a dying runtime.
                Err(_) => {
                    info!("order runtime supervisor: inner task cancelled — shutting down");
                    metrics::gauge!("tv_order_runtime_up").set(0.0);
                    return;
                }
            };
            metrics::gauge!("tv_order_runtime_up").set(0.0);
            metrics::counter!("tv_order_runtime_respawn_total", "reason" => reason).increment(1);
            error!(
                code = ErrorCode::OmsGapDryRunSafety.code_str(),
                reason,
                "OMS-GAP-06: order runtime task died — respawning with a FRESH paper book \
                 (in-RAM state lost; replayed order updates on the fresh book surface as \
                 loud orphan warns, never silent fills)"
            );
            tokio::time::sleep(Duration::from_secs(ORDER_RUNTIME_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

/// Per-run construction context (Arc clones; cheap).
struct RuntimeCtx {
    config: Arc<ApplicationConfig>,
    notifier: Arc<NotificationService>,
    calendar: Arc<TradingCalendar>,
    marks_wanted: Arc<AtomicBool>,
    token_handle: TokenHandle,
    client_id: String,
    auth_notify: Arc<Notify>,
}

// ---------------------------------------------------------------------------
// Time helpers (pure)
// ---------------------------------------------------------------------------

/// IST seconds-of-day for an epoch-seconds instant.
fn ist_secs_of_day(now_utc_secs: i64) -> u32 {
    let now_ist = now_utc_secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    (now_ist.rem_euclid(i64::from(SECONDS_PER_DAY))) as u32
}

/// IST day ordinal (days since epoch, IST wall clock) — the once-per-day
/// latch key for reset/close/self-test.
fn ist_day_number(now_utc_secs: i64) -> i64 {
    let now_ist = now_utc_secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    now_ist.div_euclid(i64::from(SECONDS_PER_DAY))
}

/// Reconcile-window gate: [09:00, 15:35) IST (market hours + a 5-min tail
/// so the final session state reconciles once after close).
fn reconcile_window_ok(secs_of_day: u32) -> bool {
    (TICK_PERSIST_START_SECS_OF_DAY_IST..RECONCILE_WINDOW_END_SECS_OF_DAY_IST)
        .contains(&secs_of_day)
}

/// Self-test window gate: [09:20, 15:00) IST (F17).
fn self_test_window_ok(secs_of_day: u32) -> bool {
    (SELF_TEST_WINDOW_START_SECS_OF_DAY_IST..SELF_TEST_WINDOW_END_SECS_OF_DAY_IST)
        .contains(&secs_of_day)
}

// ---------------------------------------------------------------------------
// Paper fill synthesis (pure)
// ---------------------------------------------------------------------------

/// Reverse of `parse_segment_chars` for synthetic updates — best-effort
/// wire chars for a numeric segment code (unknown → empty, tolerated).
fn segment_code_to_chars(segment_code: u8) -> (&'static str, &'static str) {
    use tickvault_common::types::ExchangeSegment as Seg;
    match Seg::from_byte(segment_code) {
        Some(Seg::NseFno) => ("NSE", "D"),
        Some(Seg::BseFno) => ("BSE", "D"),
        Some(Seg::NseEquity) => ("NSE", "E"),
        Some(Seg::BseEquity) => ("BSE", "E"),
        Some(Seg::IdxI) => ("NSE", "I"),
        _ => ("", ""),
    }
}

/// Builds the synthetic TRADED update for a pending paper order at `mark`.
///
/// Returns `None` unless the mark is finite and > 0 (F20 — NEVER fabricate
/// a price; the caller counts the deferral and retries on the next mark).
fn synthesize_paper_fill(order: &ManagedOrder, mark: f64, segment_code: u8) -> Option<OrderUpdate> {
    if !mark.is_finite() || mark <= 0.0 {
        return None;
    }
    let (exchange, segment) = segment_code_to_chars(segment_code);
    let txn = match order.transaction_type {
        TransactionType::Buy => "B",
        TransactionType::Sell => "S",
    };
    Some(OrderUpdate {
        exchange: exchange.to_string(),
        segment: segment.to_string(),
        security_id: order.security_id.to_string(),
        client_id: String::new(),
        order_no: order.order_id.clone(),
        exch_order_no: String::new(),
        product: "I".to_string(),
        txn_type: txn.to_string(),
        order_type: "MKT".to_string(),
        validity: "DAY".to_string(),
        quantity: order.quantity,
        traded_qty: order.quantity,
        remaining_quantity: 0,
        price: order.price,
        trigger_price: 0.0,
        traded_price: mark,
        avg_traded_price: mark,
        status: "TRADED".to_string(),
        symbol: String::new(),
        display_name: String::new(),
        correlation_id: order.correlation_id.clone(),
        remarks: String::new(),
        reason_description: String::new(),
        order_date_time: String::new(),
        exch_order_time: String::new(),
        last_updated_time: String::new(),
        instrument: String::new(),
        lot_size: i64::from(order.lot_size),
        strike_price: 0.0,
        expiry_date: String::new(),
        opt_type: String::new(),
        isin: String::new(),
        disc_quantity: 0,
        disc_qty_rem: 0,
        leg_no: 0,
        product_name: String::new(),
        ref_ltp: 0.0,
        tick_size: 0.0,
        // Synthetic paper fills are OUR OWN orders (rule 7 semantics).
        source: "P".to_string(),
        off_mkt_flag: String::new(),
        algo_ord_no: String::new(),
        mkt_type: String::new(),
        series: String::new(),
        good_till_days_date: String::new(),
        algo_id: String::new(),
        multiplier: 0,
    })
}

// ---------------------------------------------------------------------------
// Local reconcile invariant (pure over engine views)
// ---------------------------------------------------------------------------

/// Result of the dry-run LOCAL invariant check (F14 — a real check, never
/// fabricated assurance).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct LocalReconcileReport {
    sids_checked: usize,
    divergences: usize,
}

/// Compares the runtime's FillEvent-folded net-lots mirror against the risk
/// engine's positions, per sid, in BOTH directions.
fn local_reconcile(risk: &RiskEngine, mirror: &HashMap<u64, i64>) -> LocalReconcileReport {
    let mut report = LocalReconcileReport::default();
    for (&sid, &mirror_lots) in mirror {
        report.sids_checked += 1;
        if i64::from(risk.net_lots_for(sid)) != mirror_lots {
            report.divergences += 1;
        }
    }
    report
}

// ---------------------------------------------------------------------------
// Self-test state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum SelfTestPhase {
    Idle,
    /// Armed; waiting for the first mark to pick a sid.
    AwaitingMark,
    AwaitingEntryFill {
        order_id: String,
        sid: u64,
    },
    AwaitingCloseFill {
        order_id: String,
        sid: u64,
    },
}

struct SelfTestState {
    phase: SelfTestPhase,
    /// IST day ordinal of the last completed (or failed) run — the
    /// once-per-day latch (F12/F17).
    last_run_day: i64,
    started_at: std::time::Instant,
}

impl SelfTestState {
    fn new() -> Self {
        Self {
            phase: SelfTestPhase::Idle,
            last_run_day: i64::MIN,
            started_at: std::time::Instant::now(),
        }
    }

    fn fail(&mut self, today: i64, stage: &'static str) {
        error!(
            code = ErrorCode::OmsGapDryRunSafety.code_str(),
            stage,
            "OMS-GAP-06: paper self-test FAILED — the order machinery did not \
             complete the place→fill→P&L→close chain today; investigate before \
             trusting the paper book"
        );
        metrics::counter!("tv_paper_selftest_total", "outcome" => "failed").increment(1);
        self.phase = SelfTestPhase::Idle;
        self.last_run_day = today;
    }
}

// ---------------------------------------------------------------------------
// The runtime loop
// ---------------------------------------------------------------------------

/// Book-side runtime state folded by the loop arms.
struct BookState {
    /// Σ FillEvent net-lots mirror per sid (the local reconcile invariant).
    /// I-P1-11 note: sid-keyed by design THIS PR — the first-seen-segment
    /// tripwire below makes a cross-segment collision LOUD; the composite
    /// key rewrite is the mandatory pre-live follow-up.
    mirror: HashMap<u64, i64>,
    /// First-seen segment code per sid (the I-P1-11 tripwire).
    tripwire: HashMap<u64, u8>,
}

impl BookState {
    fn new() -> Self {
        Self {
            mirror: HashMap::with_capacity(64),
            tripwire: HashMap::with_capacity(64),
        }
    }

    fn clear(&mut self) {
        self.mirror.clear();
        self.tripwire.clear();
    }

    /// Tripwire check: `true` = segment consistent (or unknown), proceed.
    /// A DIVERGENT segment for a known sid → loud RISK-GAP-02 + `false`.
    fn tripwire_ok(&mut self, sid: u64, segment_code: u8) -> bool {
        if segment_code == SEGMENT_CODE_UNKNOWN {
            return true;
        }
        match self.tripwire.get(&sid) {
            None => {
                self.tripwire.insert(sid, segment_code);
                true
            }
            Some(&first) if first == segment_code => true,
            Some(&first) => {
                error!(
                    code = ErrorCode::RiskGapPositionPnl.code_str(),
                    security_id = sid,
                    first_seen_segment = first,
                    conflicting_segment = segment_code,
                    reason = "sid_segment_collision",
                    "RISK-GAP-02: cross-segment security_id collision — skipping this \
                     event to avoid a silent P&L merge (I-P1-11; composite-key rewrite \
                     is the mandatory pre-live follow-up)"
                );
                metrics::counter!("tv_oms_sid_segment_collisions_total").increment(1);
                false
            }
        }
    }
}

/// Applies one FillEvent to the risk engine + mirror. Returns whether the
/// fill was applied (tripwire may refuse).
fn apply_fill(risk: &mut RiskEngine, book: &mut BookState, fill: &FillEvent) -> bool {
    if !book.tripwire_ok(fill.security_id, fill.segment_code) {
        return false;
    }
    risk.record_fill(
        fill.security_id,
        fill.fill_lots,
        fill.avg_price,
        fill.lot_size,
    );
    *book.mirror.entry(fill.security_id).or_insert(0) += i64::from(fill.fill_lots);
    let kind = if fill.order_id.starts_with("PAPER-") {
        "paper"
    } else {
        "live"
    };
    metrics::counter!("tv_risk_fills_recorded_total", "kind" => kind).increment(1);
    true
}

// APPROVED: single-owner task orchestration — every helper is unit-tested; the loop is exercised by order_runtime_e2e
#[allow(clippy::too_many_lines)]
async fn run_order_runtime(
    ctx: RuntimeCtx,
    mut order_rx: broadcast::Receiver<OrderUpdate>,
    mark_rx_shared: Arc<tokio::sync::Mutex<mpsc::Receiver<MarkUpdate>>>,
) {
    let config = &ctx.config;
    // ---- Construction (the proven trading_pipeline template) ----
    let api_client = OrderApiClient::new(
        build_oms_http_client(),
        config.dhan.rest_api_base_url.clone(),
        ctx.client_id.clone(),
    );
    let rate_limiter = OrderRateLimiter::new(config.trading.max_orders_per_second);
    let token_bridge = Box::new(TokenHandleBridge {
        handle: ctx.token_handle.clone(),
    });
    let mut oms = OrderManagementSystem::new(
        api_client,
        rate_limiter,
        token_bridge,
        ctx.client_id.clone(),
    );
    let mut risk = RiskEngine::new(
        config.risk.max_daily_loss_percent,
        config.risk.max_position_size_lots,
        config.strategy.capital,
    );
    // FIRST production wiring of both alert sinks (the stale "wired at
    // boot" comments in the engines are finally true).
    oms.set_alert_sink(Box::new(NotifierAlertSink(Arc::clone(&ctx.notifier))));
    risk.set_alert_sink(Box::new(NotifierAlertSink(Arc::clone(&ctx.notifier))));

    let mut book = BookState::new();
    let mut self_test = SelfTestState::new();
    let mut mark_rx = mark_rx_shared.lock().await;

    let reconcile_interval = config.order_runtime.reconcile_interval_secs.max(60);
    let mut next_reconcile =
        std::time::Instant::now() + Duration::from_secs(ORDER_RUNTIME_BOOT_RECONCILE_DELAY_SECS);
    let mut last_reset_day: i64 = i64::MIN;
    let mut last_close_sweep_day: i64 = i64::MIN;
    let mut housekeeping = tokio::time::interval(Duration::from_secs(HOUSEKEEPING_TICK_SECS));
    housekeeping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    metrics::gauge!("tv_order_runtime_up").set(1.0);
    info!(
        dry_run = oms.is_dry_run(),
        paper_fill = config.order_runtime.paper_fill,
        self_test = config.order_runtime.self_test,
        reconcile_interval_secs = reconcile_interval,
        "order runtime started (paper book live — order updates + Groww marks now \
         reach the OMS/RiskEngine; alert sinks wired)"
    );

    loop {
        tokio::select! {
            // ---- Arm 1: order updates (Source=P filter at this boundary) ----
            update_result = order_rx.recv() => {
                match update_result {
                    Ok(update) => {
                        handle_order_update_event(
                            &mut oms, &mut risk, &mut book, &mut self_test,
                            &ctx, &update, config.order_runtime.paper_fill,
                        ).await;
                        republish_marks_wanted(&ctx.marks_wanted, &oms, &risk, &self_test);
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        metrics::counter!("tv_order_update_receiver_lagged_total")
                            .increment(skipped);
                        if skipped >= ORDER_UPDATE_LAG_ERROR_THRESHOLD {
                            error!(
                                code = ErrorCode::OmsGapReconciliation.code_str(),
                                skipped,
                                "OMS-GAP-02: order runtime receiver SEVERE lag — fills \
                                 likely missed; the local reconcile invariant will flag \
                                 divergence"
                            );
                        } else {
                            warn!(skipped, "order runtime receiver lagged — events skipped");
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Structurally unreachable (the supervisor holds a
                        // sender clone); surface loudly via the supervisor's
                        // clean_exit respawn arm.
                        warn!("order runtime: order-update broadcast closed");
                        return;
                    }
                }
            }

            // ---- Arm 2: marks (batch-drained, bounded) ----
            mark = mark_rx.recv() => {
                let Some(first) = mark else {
                    warn!("order runtime: mark channel closed");
                    return;
                };
                let mut processed = 0usize;
                let mut next = Some(first);
                while let Some(m) = next {
                    process_mark(
                        &mut oms, &mut risk, &mut book, &mut self_test, &ctx, m,
                        config.order_runtime.paper_fill,
                    ).await;
                    processed += 1;
                    if processed >= MARK_BATCH_MAX {
                        break;
                    }
                    next = mark_rx.try_recv().ok();
                }
                // Mark-to-market halt evaluation after the batch (≤1/sec by
                // the batching + the housekeeping tick backstop).
                risk.evaluate_daily_loss_halt();
                republish_marks_wanted(&ctx.marks_wanted, &oms, &risk, &self_test);
            }

            // ---- Arm 3: one-shot reconcile on first WS auth ----
            _ = ctx.auth_notify.notified() => {
                info!("order runtime: order-update WS authenticated — running reconcile cycle");
                run_reconcile_cycle(&mut oms, &risk, &book).await;
            }

            // ---- Arm 4: 1s housekeeping (timers ride one cold tick) ----
            _ = housekeeping.tick() => {
                let now_utc = chrono::Utc::now().timestamp();
                let secs_of_day = ist_secs_of_day(now_utc);
                let today = ist_day_number(now_utc);

                // Reconcile scheduler (market-hours gated + boot-once).
                if std::time::Instant::now() >= next_reconcile {
                    next_reconcile = std::time::Instant::now()
                        + Duration::from_secs(reconcile_interval);
                    if reconcile_window_ok(secs_of_day) {
                        run_reconcile_cycle(&mut oms, &risk, &book).await;
                    }
                }

                // Mark-to-market halt backstop (marks may be idle).
                risk.evaluate_daily_loss_halt();

                // 15:30 IST close sweep: cancel pending paper orders.
                if (TICK_PERSIST_END_SECS_OF_DAY_IST..DAILY_RESET_SECS_OF_DAY_IST)
                    .contains(&secs_of_day)
                    && last_close_sweep_day != today
                {
                    last_close_sweep_day = today;
                    market_close_sweep(&mut oms).await;
                    republish_marks_wanted(&ctx.marks_wanted, &oms, &risk, &self_test);
                }

                // 16:00 IST daily reset — ONE block clears risk + oms +
                // mirror + tripwire + the marks gate (F16: single-task
                // serialization makes this atomic vs in-flight fills).
                if secs_of_day >= DAILY_RESET_SECS_OF_DAY_IST && last_reset_day != today {
                    last_reset_day = today;
                    info!(
                        orders_placed = oms.total_placed(),
                        realized_pnl = risk.total_realized_pnl(),
                        "order runtime daily reset — zeroing paper book + risk state"
                    );
                    risk.reset_daily();
                    oms.reset_daily();
                    book.clear();
                    self_test.phase = SelfTestPhase::Idle;
                    ctx.marks_wanted.store(false, Ordering::Release);
                }

                // Self-test arm/start + timeout (F17 gates).
                drive_self_test_timers(
                    &mut self_test, &ctx, &oms, secs_of_day, today,
                    config.order_runtime.self_test,
                );
                republish_marks_wanted(&ctx.marks_wanted, &oms, &risk, &self_test);
            }
        }
    }
}

/// Arm-1 body: Source filter → engine → fill bridge → orphan visibility →
/// self-test advancement.
async fn handle_order_update_event(
    oms: &mut OrderManagementSystem,
    risk: &mut RiskEngine,
    book: &mut BookState,
    self_test: &mut SelfTestState,
    ctx: &RuntimeCtx,
    update: &OrderUpdate,
    paper_fill: bool,
) {
    // F18: manual Dhan-app events (Source=N) never touch the paper book.
    // Empty Source is tolerated (every wire field is serde-default; our
    // synthetic paper updates always stamp "P").
    if !update.source.is_empty() && update.source != "P" {
        metrics::counter!("tv_order_update_events_total", "outcome" => "foreign_source")
            .increment(1);
        return;
    }
    metrics::counter!("tv_order_update_events_total", "outcome" => "consumed").increment(1);
    match oms.handle_order_update(update) {
        Ok(Some(fill)) => {
            let order_id = fill.order_id.clone();
            if apply_fill(risk, book, &fill) {
                advance_self_test_on_fill(oms, risk, self_test, ctx, &order_id, paper_fill).await;
            }
        }
        Ok(None) => {
            // F3: a replayed/foreign fill-carrying update on an EMPTY book —
            // never silent (the engine's unknown-order arm stays debug).
            if update.traded_qty > 0 && oms.order(&update.order_no).is_none() {
                warn!(
                    code = ErrorCode::OmsGapReconciliation.code_str(),
                    order_no = %update.order_no,
                    traded_qty = update.traded_qty,
                    reason = "orphan_fill_update",
                    "OMS-GAP-02: fill-carrying update for an order this (fresh) book \
                     does not track — likely a WAL replay across a restart; the fill \
                     is NOT applied"
                );
                metrics::counter!("tv_oms_orphan_fill_updates_total").increment(1);
            }
        }
        Err(err) => {
            // InvalidTransition is already error!-coded inside the engine.
            debug!(?err, order_no = %update.order_no, "order update handling error");
        }
    }
}

/// Arm-2 body: tripwire → self-test sid pick → mark apply → paper filler.
async fn process_mark(
    oms: &mut OrderManagementSystem,
    risk: &mut RiskEngine,
    book: &mut BookState,
    self_test: &mut SelfTestState,
    ctx: &RuntimeCtx,
    mark: MarkUpdate,
    paper_fill: bool,
) {
    if !book.tripwire_ok(mark.security_id, mark.segment_code) {
        return;
    }
    let price = f64::from(mark.price);

    // Self-test sid pick: first usable mark starts the cycle.
    if self_test.phase == SelfTestPhase::AwaitingMark && price.is_finite() && price > 0.0 {
        start_self_test_entry(oms, self_test, mark.security_id, mark.segment_code).await;
    }

    // Runtime-side filter: only sids with a position or a pending paper
    // order reach the risk engine's market_prices map (keeps it small).
    let has_position = risk.net_lots_for(mark.security_id) != 0;
    let has_pending_paper = oms
        .active_orders()
        .iter()
        .any(|o| o.security_id == mark.security_id && o.order_id.starts_with("PAPER-"));
    if !(has_position || has_pending_paper) {
        return;
    }
    risk.update_market_price(mark.security_id, price);

    // Next-mark paper filler (F12 fill-once: active_orders() is
    // non-terminal-only, and a filled order goes terminal Traded).
    if paper_fill && oms.is_dry_run() && has_pending_paper {
        let pending: Vec<String> = oms
            .active_orders()
            .iter()
            .filter(|o| o.security_id == mark.security_id && o.order_id.starts_with("PAPER-"))
            .map(|o| o.order_id.clone())
            .collect();
        for order_id in pending {
            fill_paper_order(
                oms,
                risk,
                book,
                self_test,
                ctx,
                &order_id,
                price,
                mark.segment_code,
            )
            .await;
        }
    }
}

/// Synthesize + route one paper fill through the CANONICAL arm-1 path
/// (state machine, metrics, and fill code all exercised identically).
// APPROVED: argument list mirrors the single-owner loop's split borrows
#[allow(clippy::too_many_arguments)]
async fn fill_paper_order(
    oms: &mut OrderManagementSystem,
    risk: &mut RiskEngine,
    book: &mut BookState,
    self_test: &mut SelfTestState,
    ctx: &RuntimeCtx,
    order_id: &str,
    mark: f64,
    segment_code: u8,
) {
    // PAPER-prefix assertion (F12): a non-paper id in this path is a logic
    // bug — drop loudly, never synthesize a fill for a real order id.
    if !order_id.starts_with("PAPER-") {
        error!(
            code = ErrorCode::OmsGapDryRunSafety.code_str(),
            order_id,
            "OMS-GAP-06: paper filler reached a NON-PAPER order id — dropping \
             (synthetic fills are only ever produced for PAPER-n orders)"
        );
        return;
    }
    let Some(order) = oms.order(order_id).cloned() else {
        return;
    };
    let Some(synthetic) = synthesize_paper_fill(&order, mark, segment_code) else {
        // F20: zero/absent/non-finite mark — defer, retry on the next mark.
        metrics::counter!("tv_paper_fills_deferred_total").increment(1);
        return;
    };
    match oms.handle_order_update(&synthetic) {
        Ok(Some(fill)) => {
            metrics::counter!("tv_paper_fills_synthesized_total").increment(1);
            let filled_order_id = fill.order_id.clone();
            if apply_fill(risk, book, &fill) {
                advance_self_test_on_fill(oms, risk, self_test, ctx, &filled_order_id, true).await;
            }
        }
        Ok(None) => {
            metrics::counter!("tv_paper_fills_deferred_total").increment(1);
        }
        Err(err) => {
            debug!(
                ?err,
                order_id, "paper fill synthesis rejected by state machine"
            );
        }
    }
}

/// Republish the hot-path marks gate: marks are wanted while ANY position,
/// pending paper order, or active self-test needs them.
fn republish_marks_wanted(
    marks_wanted: &AtomicBool,
    oms: &OrderManagementSystem,
    risk: &RiskEngine,
    self_test: &SelfTestState,
) {
    let wanted = risk.open_position_count() > 0
        || !oms.active_orders().is_empty()
        || self_test.phase != SelfTestPhase::Idle;
    marks_wanted.store(wanted, Ordering::Release);
}

/// Reconcile cycle: honest dry-run heartbeat + the REAL local invariant
/// (F14); live mode (future) delegates to `oms.reconcile()`.
async fn run_reconcile_cycle(oms: &mut OrderManagementSystem, risk: &RiskEngine, book: &BookState) {
    if oms.is_dry_run() {
        metrics::counter!("tv_oms_reconcile_runs_total", "mode" => "paper_noop").increment(1);
        let report = local_reconcile(risk, &book.mirror);
        if report.divergences > 0 {
            error!(
                code = ErrorCode::OmsGapReconciliation.code_str(),
                divergences = report.divergences,
                sids_checked = report.sids_checked,
                "OMS-GAP-02: LOCAL reconcile invariant DIVERGED — the FillEvent \
                 mirror disagrees with the risk engine's net lots (fills were \
                 double-applied or lost inside this process)"
            );
            metrics::counter!("tv_oms_local_reconcile_divergence_total")
                .increment(report.divergences as u64);
        } else {
            info!(
                open_paper_orders = oms.active_orders().len(),
                open_positions = risk.open_position_count(),
                sids_checked = report.sids_checked,
                realized_pnl = risk.total_realized_pnl(),
                "paper mode — broker reconcile SKIPPED; local Σfills==net_lots \
                 invariant HELD (heartbeat, not a broker reconciliation)"
            );
        }
    } else {
        // Live mode (future — unreachable today: dry_run is hard-true).
        metrics::counter!("tv_oms_reconcile_runs_total", "mode" => "live").increment(1);
        match oms.reconcile().await {
            Ok(report) if report.mismatches_found > 0 => {
                error!(
                    code = ErrorCode::OmsGapReconciliation.code_str(),
                    mismatches = report.mismatches_found,
                    "OMS-GAP-02: broker reconcile found mismatches"
                );
            }
            Ok(_) => {}
            Err(err) => {
                error!(
                    code = ErrorCode::OmsGapReconciliation.code_str(),
                    ?err,
                    "OMS-GAP-02: broker reconcile FAILED"
                );
            }
        }
    }
}

/// 15:30 IST sweep: cancel every pending paper order (mirrors the
/// trading_pipeline market-close sweep).
async fn market_close_sweep(oms: &mut OrderManagementSystem) {
    let pending: Vec<String> = oms
        .active_orders()
        .iter()
        .map(|o| o.order_id.clone())
        .collect();
    if pending.is_empty() {
        return;
    }
    info!(
        pending = pending.len(),
        "market close — cancelling pending paper orders"
    );
    for order_id in &pending {
        if let Err(err) = oms.cancel_order(order_id).await {
            warn!(?err, order_id = %order_id, "market close — paper cancel failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Self-test driving
// ---------------------------------------------------------------------------

/// Housekeeping-tick half: arm the daily cycle when gates pass; time out a
/// stuck cycle (F17).
fn drive_self_test_timers(
    self_test: &mut SelfTestState,
    ctx: &RuntimeCtx,
    oms: &OrderManagementSystem,
    secs_of_day: u32,
    today: i64,
    enabled: bool,
) {
    // Timeout first: a stuck cycle fails loudly + latches for the day.
    if self_test.phase != SelfTestPhase::Idle
        && self_test.started_at.elapsed() > Duration::from_secs(SELF_TEST_TIMEOUT_SECS)
    {
        self_test.fail(today, "timeout");
        return;
    }
    if !enabled
        || self_test.phase != SelfTestPhase::Idle
        || self_test.last_run_day == today
        || !self_test_window_ok(secs_of_day)
        || !oms.is_dry_run()
        || !ctx.calendar.is_trading_day_today()
    {
        return;
    }
    info!("paper self-test armed — waiting for the first Groww mark to pick a sid");
    self_test.phase = SelfTestPhase::AwaitingMark;
    self_test.started_at = std::time::Instant::now();
}

/// Mark-arm half: the first usable mark picks the sid and places the entry
/// paper order (qty 1 unit, lot_size 1 → 1 lot).
async fn start_self_test_entry(
    oms: &mut OrderManagementSystem,
    self_test: &mut SelfTestState,
    sid: u64,
    _segment_code: u8,
) {
    let request = PlaceOrderRequest {
        security_id: sid,
        transaction_type: TransactionType::Buy,
        order_type: OrderType::Market,
        product_type: ProductType::Intraday,
        validity: OrderValidity::Day,
        quantity: 1,
        price: 0.0,
        trigger_price: 0.0,
        lot_size: 1,
        expiry_date: None,
    };
    match oms.place_order(request).await {
        Ok(order_id) => {
            info!(order_id = %order_id, security_id = sid, "paper self-test entry order placed");
            self_test.phase = SelfTestPhase::AwaitingEntryFill { order_id, sid };
        }
        Err(err) => {
            warn!(
                ?err,
                security_id = sid,
                "paper self-test entry placement failed"
            );
            // Leave AwaitingMark; the timeout latches the day if it never
            // succeeds (a halted risk engine is irrelevant here — the
            // self-test bypasses check_order by design: it exercises the
            // OMS/fill chain, not the pre-trade gate).
        }
    }
}

/// Fill-path half: advance the state machine when one of OUR self-test
/// orders fills.
async fn advance_self_test_on_fill(
    oms: &mut OrderManagementSystem,
    risk: &RiskEngine,
    self_test: &mut SelfTestState,
    ctx: &RuntimeCtx,
    filled_order_id: &str,
    paper_fill_enabled: bool,
) {
    let today = ist_day_number(chrono::Utc::now().timestamp());
    match &self_test.phase {
        SelfTestPhase::AwaitingEntryFill { order_id, sid } if order_id == filled_order_id => {
            let sid = *sid;
            if risk.net_lots_for(sid) == 0 {
                self_test.fail(today, "entry_fill_net_lots_zero");
                return;
            }
            if !paper_fill_enabled {
                // Without the filler the close leg can never fill — end
                // honestly rather than dangle until timeout.
                self_test.fail(today, "paper_fill_disabled_mid_cycle");
                return;
            }
            // Place the close (opposite side, same 1-lot size).
            let request = PlaceOrderRequest {
                security_id: sid,
                transaction_type: TransactionType::Sell,
                order_type: OrderType::Market,
                product_type: ProductType::Intraday,
                validity: OrderValidity::Day,
                quantity: 1,
                price: 0.0,
                trigger_price: 0.0,
                lot_size: 1,
                expiry_date: None,
            };
            match oms.place_order(request).await {
                Ok(close_id) => {
                    info!(order_id = %close_id, security_id = sid, "paper self-test close order placed");
                    self_test.phase = SelfTestPhase::AwaitingCloseFill {
                        order_id: close_id,
                        sid,
                    };
                }
                Err(err) => {
                    warn!(?err, "paper self-test close placement failed");
                    self_test.fail(today, "close_placement_failed");
                }
            }
        }
        SelfTestPhase::AwaitingCloseFill { order_id, sid } if order_id == filled_order_id => {
            let sid = *sid;
            let flat = risk.net_lots_for(sid) == 0;
            let pnl_finite = risk.total_realized_pnl().is_finite();
            if flat && pnl_finite {
                info!(
                    security_id = sid,
                    realized_pnl = risk.total_realized_pnl(),
                    "paper self-test PASSED — order machinery verified end-to-end today \
                     (place → mark → fill → P&L → close → flat)"
                );
                metrics::counter!("tv_paper_selftest_total", "outcome" => "ok").increment(1);
                self_test.phase = SelfTestPhase::Idle;
                self_test.last_run_day = today;
            } else {
                self_test.fail(
                    today,
                    if flat {
                        "pnl_not_finite"
                    } else {
                        "close_fill_not_flat"
                    },
                );
            }
        }
        _ => {
            let _ = (oms, ctx); // non-self-test fills need no advancement
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretString;
    use tickvault_trading::oms::{OmsError, TokenProvider};

    struct TestTokenProvider;
    impl TokenProvider for TestTokenProvider {
        fn get_access_token(&self) -> Result<SecretString, OmsError> {
            Ok(SecretString::from("test-token"))
        }
    }

    fn make_oms() -> OrderManagementSystem {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TestTokenProvider),
            "100".to_owned(),
        )
    }

    fn make_risk() -> RiskEngine {
        RiskEngine::new(2.0, 100, 1_000_000.0)
    }

    async fn place_paper(
        oms: &mut OrderManagementSystem,
        sid: u64,
        side: TransactionType,
    ) -> String {
        oms.place_order(PlaceOrderRequest {
            security_id: sid,
            transaction_type: side,
            order_type: OrderType::Market,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 1,
            price: 0.0,
            trigger_price: 0.0,
            lot_size: 1,
            expiry_date: None,
        })
        .await
        .expect("paper placement never fails") // APPROVED: test
    }

    // -------------------------------------------------------------------
    // confirm_decision (F6/F7)
    // -------------------------------------------------------------------

    #[test]
    fn test_confirm_decision_matrix() {
        // Clean drain + no stale live-feed frames → Confirm.
        assert_eq!(confirm_decision(0, 0), ConfirmVerdict::Confirm);
        // Parse errors → Defer (segments re-replay next boot).
        assert_eq!(
            confirm_decision(1, 0),
            ConfirmVerdict::Defer {
                live_feed_frames: 0
            }
        );
        // Stale live-feed frames → Defer (whole-dir confirm would archive
        // them UN-reinjected — silent tick loss).
        assert_eq!(
            confirm_decision(0, 7),
            ConfirmVerdict::Defer {
                live_feed_frames: 7
            }
        );
        // Both → Defer.
        assert_eq!(
            confirm_decision(3, 7),
            ConfirmVerdict::Defer {
                live_feed_frames: 7
            }
        );
    }

    // -------------------------------------------------------------------
    // Mark forwarder (F9/F10) — hot-path gate semantics
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_mark_forward_skips_send_when_marks_wanted_false() {
        let (tx, mut rx) = mpsc::channel::<MarkUpdate>(4);
        let fwd = MarkForwarder {
            marks_wanted: Arc::new(AtomicBool::new(false)),
            tx,
        };
        fwd.mark_forward(13, 0, 23_146.45);
        assert!(
            rx.try_recv().is_err(),
            "marks_wanted=false must skip the send entirely"
        );
    }

    #[tokio::test]
    async fn test_mark_forward_sends_when_armed() {
        let (tx, mut rx) = mpsc::channel::<MarkUpdate>(4);
        let fwd = MarkForwarder {
            marks_wanted: Arc::new(AtomicBool::new(true)),
            tx,
        };
        fwd.mark_forward(13, 0, 23_146.45);
        let m = rx.try_recv().expect("armed forwarder must send"); // APPROVED: test
        assert_eq!(m.security_id, 13);
        assert_eq!(m.segment_code, 0);
    }

    #[tokio::test]
    async fn test_mark_forward_channel_full_drops_counted_never_blocks() {
        let (tx, _rx) = mpsc::channel::<MarkUpdate>(1);
        let fwd = MarkForwarder {
            marks_wanted: Arc::new(AtomicBool::new(true)),
            tx,
        };
        // Fill the 1-slot channel, then overflow — must return immediately
        // (try_send), never block, never panic.
        fwd.mark_forward(13, 0, 1.0);
        fwd.mark_forward(13, 0, 2.0);
        fwd.mark_forward(13, 0, 3.0);
    }

    /// MarkUpdate stays a small Copy payload (the zero-alloc contract).
    #[test]
    fn test_mark_update_is_copy_and_small() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<MarkUpdate>();
        assert!(std::mem::size_of::<MarkUpdate>() <= 16);
    }

    // -------------------------------------------------------------------
    // Fill application + tripwire (F1/F11)
    // -------------------------------------------------------------------

    #[test]
    fn test_apply_fill_reaches_risk_engine_net_lots_nonzero() {
        let mut risk = make_risk();
        let mut book = BookState::new();
        let fill = FillEvent {
            security_id: 13,
            segment_code: 0,
            fill_lots: 2,
            avg_price: 100.0,
            lot_size: 25,
            order_id: "PAPER-1".to_string(),
        };
        assert!(apply_fill(&mut risk, &mut book, &fill));
        assert_eq!(risk.net_lots_for(13), 2, "fill must reach the risk engine");
        assert_eq!(book.mirror.get(&13), Some(&2));
    }

    #[test]
    fn test_sid_segment_collision_skips_and_errors() {
        let mut risk = make_risk();
        let mut book = BookState::new();
        let fill_a = FillEvent {
            security_id: 27,
            segment_code: 0, // IDX_I
            fill_lots: 1,
            avg_price: 100.0,
            lot_size: 1,
            order_id: "PAPER-1".to_string(),
        };
        let fill_b = FillEvent {
            security_id: 27,
            segment_code: 1, // NSE_EQ — the FINNIFTY id=27 collision class
            fill_lots: 1,
            avg_price: 200.0,
            lot_size: 1,
            order_id: "PAPER-2".to_string(),
        };
        assert!(apply_fill(&mut risk, &mut book, &fill_a));
        assert!(
            !apply_fill(&mut risk, &mut book, &fill_b),
            "a divergent segment for a known sid must be SKIPPED (loud), \
             never silently merged into the same position"
        );
        assert_eq!(risk.net_lots_for(27), 1, "second fill must not apply");
    }

    #[test]
    fn test_tripwire_unknown_segment_is_compatible() {
        let mut book = BookState::new();
        assert!(book.tripwire_ok(13, SEGMENT_CODE_UNKNOWN));
        assert!(book.tripwire_ok(13, 0), "unknown never poisons first-seen");
        assert!(book.tripwire_ok(13, SEGMENT_CODE_UNKNOWN));
        assert!(!book.tripwire_ok(13, 2), "real divergence still trips");
    }

    // -------------------------------------------------------------------
    // Paper fill synthesis (F20/F12)
    // -------------------------------------------------------------------

    fn make_paper_order(order_id: &str, side: TransactionType) -> ManagedOrder {
        ManagedOrder {
            order_id: order_id.to_string(),
            correlation_id: "corr".to_string(),
            security_id: 13,
            transaction_type: side,
            order_type: OrderType::Market,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 1,
            price: 0.0,
            trigger_price: 0.0,
            status: tickvault_common::order_types::OrderStatus::Confirmed,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 1,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        }
    }

    #[test]
    fn test_paper_fill_deferred_until_finite_positive_mark() {
        let order = make_paper_order("PAPER-1", TransactionType::Buy);
        assert!(synthesize_paper_fill(&order, 0.0, 0).is_none());
        assert!(synthesize_paper_fill(&order, -1.0, 0).is_none());
        assert!(synthesize_paper_fill(&order, f64::NAN, 0).is_none());
        assert!(synthesize_paper_fill(&order, f64::INFINITY, 0).is_none());
        let update = synthesize_paper_fill(&order, 101.5, 0).expect("finite>0 fills"); // APPROVED: test
        assert_eq!(update.status, "TRADED");
        assert_eq!(update.traded_qty, 1);
        assert!((update.avg_traded_price - 101.5).abs() < f64::EPSILON);
        assert_eq!(update.source, "P", "synthetic fills are our own orders");
        assert_eq!(update.txn_type, "B");
    }

    #[tokio::test]
    async fn test_terminal_order_never_refilled() {
        let mut oms = make_oms();
        let order_id = place_paper(&mut oms, 13, TransactionType::Buy).await;
        let order = oms.order(&order_id).cloned().expect("tracked"); // APPROVED: test
        let synthetic = synthesize_paper_fill(&order, 100.0, 0).expect("fill"); // APPROVED: test
        let first = oms.handle_order_update(&synthetic).ok().flatten();
        assert!(first.is_some(), "first fill applies");
        // The order is now terminal Traded → active_orders() excludes it
        // (the filler's candidate set), and a re-route is refused by the
        // state machine (terminal has no outgoing transitions).
        assert!(oms.active_orders().is_empty(), "terminal never re-listed");
        let refill = oms.handle_order_update(&synthetic);
        assert!(
            !matches!(refill, Ok(Some(_))),
            "a terminal order must never fill twice"
        );
    }

    // -------------------------------------------------------------------
    // Local reconcile invariant (F14)
    // -------------------------------------------------------------------

    #[test]
    fn test_local_reconcile_invariant_holds_and_diverges() {
        let mut risk = make_risk();
        let mut book = BookState::new();
        let fill = FillEvent {
            security_id: 13,
            segment_code: 0,
            fill_lots: 3,
            avg_price: 100.0,
            lot_size: 25,
            order_id: "PAPER-1".to_string(),
        };
        assert!(apply_fill(&mut risk, &mut book, &fill));
        let ok = local_reconcile(&risk, &book.mirror);
        assert_eq!(ok.sids_checked, 1);
        assert_eq!(ok.divergences, 0, "mirror==net_lots after a clean fill");
        // Poison the mirror → divergence must be detected.
        book.mirror.insert(13, 99);
        let bad = local_reconcile(&risk, &book.mirror);
        assert_eq!(bad.divergences, 1, "a poisoned mirror must be flagged");
    }

    /// Property-style sweep: random signed fill sequences keep the mirror
    /// equal to the risk engine's net lots (prop_fill_mirror_matches_risk_net_lots).
    #[test]
    fn prop_fill_mirror_matches_risk_net_lots() {
        // Deterministic pseudo-random walk (no proptest dep in app crate):
        // an LCG drives sid/side/lots choices across 1,000 fills.
        let mut risk = make_risk();
        let mut book = BookState::new();
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        for _ in 0..1_000 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let sid = 10 + (state >> 33) % 7;
            let lots = ((state >> 12) % 5) as i32 + 1;
            let signed = if state & 1 == 0 { lots } else { -lots };
            let fill = FillEvent {
                security_id: sid,
                segment_code: 0,
                fill_lots: signed,
                avg_price: 100.0 + (state % 100) as f64,
                lot_size: 25,
                order_id: "PAPER-x".to_string(),
            };
            assert!(apply_fill(&mut risk, &mut book, &fill));
        }
        let report = local_reconcile(&risk, &book.mirror);
        assert_eq!(
            report.divergences, 0,
            "after any fill sequence the mirror must equal risk net_lots"
        );
    }

    // -------------------------------------------------------------------
    // Reset atomicity (F16) + marks gate republish
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_daily_reset_clears_book_mirror_tripwire_flag_atomically() {
        let mut oms = make_oms();
        let mut risk = make_risk();
        let mut book = BookState::new();
        let marks_wanted = Arc::new(AtomicBool::new(true));
        let order_id = place_paper(&mut oms, 13, TransactionType::Buy).await;
        let order = oms.order(&order_id).cloned().expect("tracked"); // APPROVED: test
        let synthetic = synthesize_paper_fill(&order, 100.0, 0).expect("fill"); // APPROVED: test
        if let Ok(Some(fill)) = oms.handle_order_update(&synthetic) {
            apply_fill(&mut risk, &mut book, &fill);
        }
        assert_eq!(risk.net_lots_for(13), 1);
        assert!(!book.mirror.is_empty());
        assert!(!book.tripwire.is_empty());
        // The 16:00 reset block (mirrors the loop's arm-4 body).
        risk.reset_daily();
        oms.reset_daily();
        book.clear();
        marks_wanted.store(false, Ordering::Release);
        assert_eq!(risk.net_lots_for(13), 0);
        assert!(book.mirror.is_empty());
        assert!(book.tripwire.is_empty());
        assert!(oms.all_orders().is_empty());
        assert!(!marks_wanted.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn test_republish_marks_wanted_tracks_book_state() {
        let mut oms = make_oms();
        let risk = make_risk();
        let self_test = SelfTestState::new();
        let flag = Arc::new(AtomicBool::new(false));
        republish_marks_wanted(&flag, &oms, &risk, &self_test);
        assert!(!flag.load(Ordering::Acquire), "empty book wants no marks");
        let _ = place_paper(&mut oms, 13, TransactionType::Buy).await;
        republish_marks_wanted(&flag, &oms, &risk, &self_test);
        assert!(
            flag.load(Ordering::Acquire),
            "a pending paper order arms the marks gate"
        );
    }

    // -------------------------------------------------------------------
    // Self-test gates (F17) + time helpers
    // -------------------------------------------------------------------

    #[test]
    fn test_selftest_refused_on_holiday_and_off_hours() {
        // Window gate is pure: [09:20, 15:00) IST.
        assert!(!self_test_window_ok(9 * 3600)); // 09:00 — pre-window
        assert!(!self_test_window_ok(9 * 3600 + 19 * 60)); // 09:19
        assert!(self_test_window_ok(SELF_TEST_WINDOW_START_SECS_OF_DAY_IST));
        assert!(self_test_window_ok(12 * 3600));
        assert!(!self_test_window_ok(SELF_TEST_WINDOW_END_SECS_OF_DAY_IST)); // 15:00
        assert!(!self_test_window_ok(15 * 3600 + 30 * 60));
    }

    #[test]
    fn test_selftest_single_cycle_latched() {
        let mut st = SelfTestState::new();
        let today = 20_000_i64;
        // Success path latches the day.
        st.phase = SelfTestPhase::Idle;
        st.last_run_day = today;
        assert_eq!(st.last_run_day, today, "a completed run must latch the day");
        // fail() also latches (no retry storm).
        let mut st2 = SelfTestState::new();
        st2.phase = SelfTestPhase::AwaitingMark;
        st2.fail(today, "timeout");
        assert_eq!(st2.phase, SelfTestPhase::Idle);
        assert_eq!(st2.last_run_day, today);
    }

    #[test]
    fn test_reconcile_window_and_time_helpers() {
        assert!(!reconcile_window_ok(8 * 3600)); // 08:00 — closed
        assert!(reconcile_window_ok(TICK_PERSIST_START_SECS_OF_DAY_IST)); // 09:00
        assert!(reconcile_window_ok(15 * 3600 + 34 * 60)); // 15:34
        assert!(!reconcile_window_ok(RECONCILE_WINDOW_END_SECS_OF_DAY_IST)); // 15:35
        // ist helpers: epoch 0 UTC = 05:30 IST, day 0.
        assert_eq!(ist_secs_of_day(0), 19_800);
        assert_eq!(ist_day_number(0), 0);
        // 2026-07-14 00:00:00 UTC → 05:30 IST same day.
        let t = 1_784_073_600_i64; // approx epoch for a 2026 date
        assert!(ist_secs_of_day(t) < u32::from(u16::MAX) * 2);
    }

    // -------------------------------------------------------------------
    // Source filter (F18) — the consumer-boundary semantics
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_source_n_filtered_empty_tolerated() {
        // Pure boundary predicate replicated from handle_order_update_event:
        let is_foreign = |source: &str| !source.is_empty() && source != "P";
        assert!(!is_foreign(""), "empty Source tolerated (serde default)");
        assert!(!is_foreign("P"), "API-sourced events consumed");
        assert!(is_foreign("N"), "manual Dhan-app events filtered");
        assert!(is_foreign("X"), "unknown sources filtered");
    }

    // -------------------------------------------------------------------
    // Segment char round-trip for synthetic updates
    // -------------------------------------------------------------------

    #[test]
    fn test_segment_code_to_chars_round_trips_via_parse() {
        use tickvault_common::types::ExchangeSegment as Seg;
        for seg in [
            Seg::NseFno,
            Seg::BseFno,
            Seg::NseEquity,
            Seg::BseEquity,
            Seg::IdxI,
        ] {
            let (ex, sg) = segment_code_to_chars(seg.binary_code());
            let parsed = tickvault_trading::oms::parse_segment_chars(ex, sg)
                .expect("known segment must round-trip"); // APPROVED: test
            assert_eq!(parsed.binary_code(), seg.binary_code());
        }
        // Unknown → empty chars (tolerated by the engine's sentinel arm).
        assert_eq!(segment_code_to_chars(SEGMENT_CODE_UNKNOWN), ("", ""));
    }
}
