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
//!   consumer boundary). SOCKET-FREE shape (2026-07-14 operator Dhan noise
//!   lock): the broadcast has ZERO producers today — no order-update WS is
//!   spawned and no order-event frames are WAL-captured; the gated live
//!   re-arm (fresh dated quote per dhan-rest-only-noise-lock-2026-07-14 §3)
//!   re-attaches the socket + WAL producers to this SAME channel,
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
//! - Spawned ONLY from `dhan_rest_stack` Phase 5b (the dhan-OFF arm) — the
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

use std::collections::{HashMap, HashSet};
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

/// Supervisor respawn backoff BASE after an abnormal inner exit (WS-GAP-05
/// house pattern; unwind builds only — see the module panic-honesty note).
/// E8 (fix-round 2026-07-14): consecutive abnormal exits DOUBLE the backoff
/// up to [`ORDER_RUNTIME_RESPAWN_BACKOFF_CAP_SECS`] so a permanently-closed
/// channel can never become a ~17K-errors/day respawn storm.
const ORDER_RUNTIME_RESPAWN_BACKOFF_SECS: u64 = 5;
/// E8 respawn backoff ceiling (the WS-GAP-05 300s cap).
const ORDER_RUNTIME_RESPAWN_BACKOFF_CAP_SECS: u64 = 300;
/// E8 stability window: an inner run that survives this long resets the
/// backoff ladder (the WS-GAP-10 stability-window precedent).
const ORDER_RUNTIME_RESPAWN_STABILITY_SECS: u64 = 60;
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
/// bridge per-tick path via a lock-free `try_send` (no await; the disarmed
/// and drop arms are strictly zero-alloc — accepted sends carry tokio
/// mpsc's AMORTIZED block-reuse alloc profile, see [`MarkForwarder`]).
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
///
/// # Alloc honesty (HP-1, mirrors the DHAT file's own wording)
/// The DISARMED and ARMED+FULL (drop) arms are strictly zero-alloc
/// (DHAT-proven, `dhat_mark_forward.rs`). ARMED+ACCEPTED sends have an
/// AMORTIZED block-reuse alloc profile — tokio's mpsc grows/reuses its
/// 32-slot block list as queue depth changes — budgeted by the Criterion
/// bench (`order_gate/mark_forward_armed_accept`), never claimed zero.
#[derive(Clone)]
pub struct MarkForwarder {
    /// Armed by the runtime whenever a position / pending paper order /
    /// active self-test needs marks; `false` = the tap is a single load.
    pub marks_wanted: Arc<AtomicBool>,
    /// Bounded channel into the runtime's mark arm.
    pub tx: mpsc::Sender<MarkUpdate>,
}

impl MarkForwarder {
    /// Forward one mark if the runtime wants marks. Lock-free, never
    /// blocks, never awaits — safe at the per-tick consume seam (alloc
    /// profile per the struct-level honesty note).
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
            // Cluster-F alert classes (#1558: readiness refusal / OMS halt /
            // DH-904 ladder exhaustion / DATA-805 stop-all) are
            // LOG-SINK-ONLY by contract (order-readiness-error-codes.md §3 +
            // the Dhan noise lock §2 — no Telegram family row exists for
            // them). The engine already emits the coded, edge-latched
            // `error!` at each fire site, so the sink deliberately routes
            // NOTHING here — a notify() would be an unauthorized new Dhan
            // page class; a second error! would double-log the same episode.
            alert @ (OmsAlert::OrderReadinessRefused { .. }
            | OmsAlert::OmsHalted { .. }
            | OmsAlert::Dh904LadderExhausted { .. }
            | OmsAlert::OrderApiStopAll { .. }) => {
                debug!(
                    message = %alert.operator_message(),
                    "cluster-F OMS alert observed by the sink — log-sink-only \
                     by contract (engine already emitted the coded error)"
                );
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
// Runtime params + spawn
// ---------------------------------------------------------------------------

/// Everything the runtime needs — assembled by `dhan_rest_stack` Phase 5b.
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
    /// The FIRST receiver (the channel-construction receiver — no earlier
    /// producer exists in the socket-free shape; the gated live re-arm's
    /// socket/WAL drain must keep subscribing BEFORE any producer starts).
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
        // E8: consecutive-abnormal-exit counter drives an escalating backoff
        // (5s → 300s cap) so a permanently-broken channel cannot storm.
        let mut consecutive_abnormal_exits: u32 = 0;
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
            let run_started = std::time::Instant::now();
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
            // E8: a run that survived ≥60s resets the ladder (the WS-GAP-10
            // stability-window precedent) — only rapid flapping escalates.
            if run_started.elapsed() >= Duration::from_secs(ORDER_RUNTIME_RESPAWN_STABILITY_SECS) {
                consecutive_abnormal_exits = 0;
            }
            // E8: escalating backoff — doubles per consecutive abnormal exit,
            // capped at 300s (WS-GAP-05 pattern).
            consecutive_abnormal_exits = consecutive_abnormal_exits.saturating_add(1);
            let backoff_secs = ORDER_RUNTIME_RESPAWN_BACKOFF_SECS
                .saturating_mul(1_u64 << consecutive_abnormal_exits.saturating_sub(1).min(8))
                .min(ORDER_RUNTIME_RESPAWN_BACKOFF_CAP_SECS);
            // E2 honesty: PAPER fills / self-test orders never traverse the
            // broadcast — a respawn (or process restart) SILENTLY zeroes the
            // paper book + day P&L. In the socket-free shape NOTHING replays
            // (no WAL capture); once the gated live re-arm lands, replayed
            // REAL-account fill frames surface as loud orphan warns.
            error!(
                code = ErrorCode::OmsGapDryRunSafety.code_str(),
                reason,
                backoff_secs,
                consecutive_abnormal_exits,
                "OMS-GAP-06: order runtime task died — respawning with a FRESH paper book. \
                 PAPER positions + day P&L are in-RAM only and are silently ZEROED by this \
                 respawn; replayed REAL-account fill frames on the fresh book surface as \
                 loud orphan warns"
            );
            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
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

/// 15:30 IST close-sweep gate: inside [15:30, 16:00) IST, once per IST day,
/// TRADING DAYS ONLY — mirrors the reconcile scheduler's trading-day gate
/// (E5 sibling): a weekend/holiday housekeeping tick must never fire the
/// close sweep.
fn close_sweep_due(
    secs_of_day: u32,
    today: i64,
    last_sweep_day: i64,
    is_trading_day: bool,
) -> bool {
    is_trading_day
        && (TICK_PERSIST_END_SECS_OF_DAY_IST..DAILY_RESET_SECS_OF_DAY_IST).contains(&secs_of_day)
        && last_sweep_day != today
}

/// Self-test window gate: [09:20, 15:00) IST (F17).
fn self_test_window_ok(secs_of_day: u32) -> bool {
    (SELF_TEST_WINDOW_START_SECS_OF_DAY_IST..SELF_TEST_WINDOW_END_SECS_OF_DAY_IST)
        .contains(&secs_of_day)
}

/// E1 (fix-round 2026-07-14): the 16:00-IST RESET EPOCH — the IST day
/// ordinal of the most recent 16:00 boundary already passed. The reset
/// fires whenever the observed epoch ADVANCES past the last-reset epoch,
/// so a process suspended across [16:00, midnight) still resets on its
/// first tick of the new day (the old same-day-keyed gate carried day-1
/// positions + a latched HALT through day-2's entire session).
fn reset_epoch(secs_of_day: u32, today: i64) -> i64 {
    if secs_of_day >= DAILY_RESET_SECS_OF_DAY_IST {
        today
    } else {
        today.saturating_sub(1)
    }
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
    /// C7 second leg: per-sid Σ signed order `traded_qty` lots vs the risk
    /// engine — catches a fill LOST between the engine and `record_fill`
    /// (e.g. a broadcast-lag drop), which the mirror leg structurally
    /// cannot (mirror + risk are mutated together in `apply_fill`).
    order_fold_divergences: usize,
}

/// Compares (leg 1) the runtime's FillEvent-folded net-lots mirror against
/// the risk engine's positions over the UNION of both key sets, and
/// (leg 2, C7) the per-sid signed fold of every tracked order's cumulative
/// `traded_qty` lots against the risk engine's net lots.
///
/// Leg-2 honesty: valid for the DRY-RUN book (PAPER order ids never
/// re-index, so `all_orders()` holds one entry per order); the live-mode
/// correlation re-index keeps a stale clone under the old order_no and
/// would double-count — the caller only runs this in dry-run.
fn local_reconcile(
    oms: &OrderManagementSystem,
    risk: &RiskEngine,
    mirror: &HashMap<u64, i64>,
) -> LocalReconcileReport {
    let mut report = LocalReconcileReport::default();
    // Leg 1: mirror vs risk over the UNION of keys (C7 — a risk-side
    // position absent from the mirror is a divergence too).
    let mut sids: HashSet<u64> = mirror.keys().copied().collect();
    sids.extend(risk.position_security_ids());
    for &sid in &sids {
        report.sids_checked += 1;
        let mirror_lots = mirror.get(&sid).copied().unwrap_or(0);
        if i64::from(risk.net_lots_for(sid)) != mirror_lots {
            report.divergences += 1;
        }
    }
    // Leg 2: per-sid signed order fold (floor of cumulatives — the same
    // math the engine's fill delta uses) vs risk net lots.
    let mut folded: HashMap<u64, i64> = HashMap::with_capacity(mirror.len());
    for order in oms.all_orders().values() {
        let lot = i64::from(order.lot_size.max(1));
        let lots = order.traded_qty.max(0) / lot;
        let signed = match order.transaction_type {
            TransactionType::Buy => lots,
            TransactionType::Sell => -lots,
        };
        *folded.entry(order.security_id).or_insert(0) += signed;
    }
    let mut fold_sids: HashSet<u64> = folded.keys().copied().collect();
    fold_sids.extend(risk.position_security_ids());
    for &sid in &fold_sids {
        let fold_lots = folded.get(&sid).copied().unwrap_or(0);
        if i64::from(risk.net_lots_for(sid)) != fold_lots {
            report.order_fold_divergences += 1;
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
    /// First-seen segment code per sid (the I-P1-11 tripwire). Footprint
    /// (HP-6): while armed, EVERY sid the tap forwards gets an entry — the
    /// map is bounded by the Groww watch universe (~770 sids ≈ tens of KB)
    /// and cleared at the daily reset.
    tripwire: HashMap<u64, u8>,
    /// HP-3/E3 edge latch: sids whose divergence has already fired the
    /// coded `error!` this episode (cleared at the daily reset). The
    /// counter keeps counting per event; the LOG fires once per sid per
    /// day (audit-findings Rule 4 — edge-triggered alerts only).
    tripwire_reported: HashSet<u64>,
    /// HP-2: sid → pending PAPER order ids — the O(1) per-mark filler
    /// index. REBUILT (O(N_all_orders), cold — order events are low-rate)
    /// via [`Self::rebuild_pending_paper`] after every order-mutating
    /// event; per-mark reads are one HashMap lookup, zero alloc on the
    /// no-pending path.
    pending_paper: HashMap<u64, Vec<String>>,
}

impl BookState {
    fn new() -> Self {
        Self {
            mirror: HashMap::with_capacity(64),
            tripwire: HashMap::with_capacity(64),
            tripwire_reported: HashSet::with_capacity(8),
            pending_paper: HashMap::with_capacity(8),
        }
    }

    fn clear(&mut self) {
        self.mirror.clear();
        self.tripwire.clear();
        self.tripwire_reported.clear();
        self.pending_paper.clear();
    }

    /// Rebuilds the sid → pending-PAPER-order index from the OMS book.
    /// O(N_all_orders) + per-pending-id clones — COLD path (called on
    /// order-mutating events only, never per mark). Honest trade-off: a
    /// full rebuild per order event beats maintaining incremental deltas
    /// across 6 mutation sites (drift there would silently kill fills).
    fn rebuild_pending_paper(&mut self, oms: &OrderManagementSystem) {
        self.pending_paper.clear();
        for order in oms.all_orders().values() {
            if !order.is_terminal() && order.order_id.starts_with("PAPER-") {
                self.pending_paper
                    .entry(order.security_id)
                    .or_default()
                    .push(order.order_id.clone());
            }
        }
    }

    /// O(1) per-mark check: does this sid have a pending paper order?
    fn has_pending_paper(&self, sid: u64) -> bool {
        // Entries are inserted non-empty by construction (rebuild).
        self.pending_paper.contains_key(&sid)
    }

    /// Tripwire check: `true` = segment consistent (or unknown), proceed.
    /// A DIVERGENT segment for a known sid → RISK-GAP-02 (edge-latched
    /// `error!` once per sid per episode; counter per event) + `false`.
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
                // HP-3/E3: counter EVERY event; error! on the RISING EDGE
                // only — a colliding sid ticking 1-4/s for hours must never
                // become an error-per-event flood (audit Rule 4).
                metrics::counter!("tv_oms_sid_segment_collisions_total").increment(1);
                if self.tripwire_reported.insert(sid) {
                    error!(
                        code = ErrorCode::RiskGapPositionPnl.code_str(),
                        security_id = sid,
                        first_seen_segment = first,
                        conflicting_segment = segment_code,
                        reason = "sid_segment_collision",
                        "RISK-GAP-02: cross-segment security_id collision — skipping \
                         this and every further divergent event for this sid today \
                         (counted per event; logged once per episode). I-P1-11; the \
                         composite-key rewrite is the mandatory pre-live follow-up"
                    );
                }
                false
            }
        }
    }
}

/// M3: the arm-1 consumer-boundary Source filter (F18) — a PRODUCTION pure
/// fn so the test exercises the real predicate, never a replica. Manual
/// Dhan-app events (Source=N) and unknown sources never touch the paper
/// book; empty Source is tolerated (every wire field is serde-default; our
/// synthetic paper updates always stamp "P").
fn is_foreign_source(source: &str) -> bool {
    !source.is_empty() && source != "P"
}

/// S3: bounded + sanitized broker string for log interpolation — strips
/// control/BiDi chars and caps the length so a crafted multi-KB `order_no`
/// cannot amplify `errors.log` (cold error-path alloc is fine).
fn log_safe_id(raw: &str) -> String {
    tickvault_common::sanitize::sanitize_audit_string(raw)
        .chars()
        .take(64)
        .collect()
}

/// Applies one FillEvent to the risk engine + mirror. Returns whether the
/// fill was applied (tripwire / unknown-segment guard may refuse).
fn apply_fill(risk: &mut RiskEngine, book: &mut BookState, fill: &FillEvent) -> bool {
    // S2 (fix-round 2026-07-14): a fill whose segment chars were UNMAPPABLE
    // cannot be partition-checked — treating UNKNOWN as compatible-with-
    // everything was the exact tripwire bypass. Refuse the fill (loud);
    // every legitimate dry-run fill round-trips a known segment.
    if fill.segment_code == SEGMENT_CODE_UNKNOWN {
        error!(
            code = ErrorCode::RiskGapPositionPnl.code_str(),
            security_id = fill.security_id,
            order_id = %log_safe_id(&fill.order_id),
            reason = "unknown_segment_fill",
            "RISK-GAP-02: fill with an UNMAPPABLE exchange/segment refused — \
             it would bypass the I-P1-11 tripwire partition check"
        );
        metrics::counter!("tv_oms_unknown_segment_fills_refused_total").increment(1);
        return false;
    }
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
    let mut last_reset_epoch: i64 = i64::MIN;
    let mut last_close_sweep_day: i64 = i64::MIN;
    // Fix F (2026-07-17 respawn flap): the mark producers are DAY-SCOPED —
    // the Groww per-minute REST legs' supervisors exit at day completion
    // ("day complete — supervisor exiting", ~15:31 IST after the
    // post-session sweep) and drop the last MarkForwarder clones, CLOSING
    // the mark mpsc for the rest of the process lifetime. That is a
    // legitimate steady state, not a death — this flag disarms Arm 2 once
    // the close is observed so the loop keeps running (pre-fix the arm
    // `return`ed, and the supervisor's clean_exit respawn re-read the same
    // closed channel instantly: a permanent flap that reset the paper book
    // at every backoff step from 15:31 through post-close).
    let mut mark_channel_open = true;
    let mut housekeeping = tokio::time::interval(Duration::from_secs(HOUSEKEEPING_TICK_SECS));
    housekeeping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    metrics::gauge!("tv_order_runtime_up").set(1.0);
    info!(
        dry_run = oms.is_dry_run(),
        paper_fill = config.order_runtime.paper_fill,
        self_test = config.order_runtime.self_test,
        reconcile_interval_secs = reconcile_interval,
        "order runtime started (paper book live — order updates + Groww marks now \
         reach the OMS/RiskEngine; alert sinks wired). Paper book starts EMPTY: \
         paper fills are in-RAM only, so a restart zeroes paper positions + day \
         P&L (socket-free shape: no order-event frames are captured or replayed)"
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
                            // C7 honesty: the MIRROR leg cannot see a lagged
                            // drop (mirror + risk are mutated together); the
                            // ORDER-FOLD leg of the local reconcile catches
                            // it for TRACKED orders only — fills for orders
                            // this book never tracked are lost silently.
                            error!(
                                code = ErrorCode::OmsGapReconciliation.code_str(),
                                skipped,
                                "OMS-GAP-02: order runtime receiver SEVERE lag — fills \
                                 likely missed; the local reconcile's order-fold leg \
                                 flags divergence for TRACKED orders (untracked fills \
                                 are lost)"
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

            // ---- Arm 2: marks (batch-drained, bounded; disarmed after the
            // day-scoped producers close the channel — Fix F) ----
            mark = mark_rx.recv(), if mark_channel_open => {
                let Some(first) = mark else {
                    warn!(
                        "order runtime: mark channel closed (day-scoped mark \
                         producers finished) — mark arm disarmed; the runtime \
                         stays live (order updates, reconcile, 15:30 sweep and \
                         16:00 reset keep running; marks resume at the next \
                         process boot)"
                    );
                    mark_channel_open = false;
                    continue;
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
                // E5: trading-day gated too — no weekend/holiday heartbeats.
                if std::time::Instant::now() >= next_reconcile {
                    next_reconcile = std::time::Instant::now()
                        + Duration::from_secs(reconcile_interval);
                    if reconcile_window_ok(secs_of_day)
                        && ctx.calendar.is_trading_day_today()
                    {
                        run_reconcile_cycle(&mut oms, &risk, &book).await;
                    }
                }

                // Mark-to-market halt backstop (marks may be idle).
                risk.evaluate_daily_loss_halt();

                // 15:30 IST close sweep: cancel pending paper orders.
                // Trading-day gated like the reconcile scheduler above —
                // no weekend/holiday sweep firings (2026-07-17 audit, LOW).
                if close_sweep_due(
                    secs_of_day,
                    today,
                    last_close_sweep_day,
                    ctx.calendar.is_trading_day_today(),
                ) {
                    last_close_sweep_day = today;
                    market_close_sweep(&mut oms, &mut book).await;
                    republish_marks_wanted(&ctx.marks_wanted, &oms, &risk, &self_test);
                }

                // 16:00 IST daily reset — ONE block clears risk + oms +
                // mirror + tripwire + the marks gate (F16: single-task
                // serialization makes this atomic vs in-flight fills).
                // E1: EPOCH-keyed — a process suspended across [16:00,
                // midnight) catches up on its first tick of the new day
                // instead of carrying day-1 positions + a latched halt
                // through day-2's whole session.
                let epoch = reset_epoch(secs_of_day, today);
                if last_reset_epoch == i64::MIN {
                    // Boot: initialize the epoch only (empty book — nothing
                    // to reset; today's 16:00 boundary still fires).
                    last_reset_epoch = epoch;
                } else if epoch > last_reset_epoch {
                    last_reset_epoch = epoch;
                    perform_daily_reset(
                        &mut risk, &mut oms, &mut book, &mut self_test, &ctx.marks_wanted,
                    );
                }

                // Self-test arm/start + timeout (F17 gates; C6 timeout
                // cleanup cancels the outstanding self-test order).
                drive_self_test_timers(
                    &mut self_test, &ctx, &mut oms, &risk, &mut book, secs_of_day, today,
                    config.order_runtime.self_test,
                ).await;
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
    // synthetic paper updates always stamp "P"). M3: the predicate is the
    // production `is_foreign_source` (unit-tested directly, no replica).
    if is_foreign_source(&update.source) {
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
                    order_no = %log_safe_id(&update.order_no),
                    traded_qty = update.traded_qty,
                    reason = "orphan_fill_update",
                    "OMS-GAP-02: fill-carrying update for an order this (fresh) book \
                     does not track — an externally injected event (e.g. a future \
                     live re-arm's WAL replay across a restart); the fill is NOT \
                     applied"
                );
                metrics::counter!("tv_oms_orphan_fill_updates_total").increment(1);
            }
        }
        Err(err) => {
            // InvalidTransition is already error!-coded inside the engine.
            debug!(?err, order_no = %log_safe_id(&update.order_no), "order update handling error");
        }
    }
    // HP-2: the update may have created/advanced/terminated an order (and
    // advance_self_test_on_fill may have placed the close) — refresh the
    // per-sid pending index (cold path; order events are low-rate).
    book.rebuild_pending_paper(oms);
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
    // C14: shortest-decimal widening — plain `f64::from(f32)` carries the
    // IEEE 754 artifact (10.2f32 → 10.19999980926514) into every paper
    // fill price and P&L log line. Cold path (the runtime side, not the
    // bridge tap).
    let price = tickvault_common::price_precision::f32_to_f64_clean(mark.price);

    // Self-test sid pick: the first usable INDEX-SPOT mark starts the
    // cycle. E4: restricted to IDX_I (segment 0) — index spots tick ~1/s,
    // so the close leg's second mark always lands inside the 180s timeout;
    // an arbitrary sid could be a far-month future with MINUTES of
    // legitimate silence (daily-universe §36.4) → a daily false FAIL.
    if self_test.phase == SelfTestPhase::AwaitingMark
        && mark.segment_code == tickvault_common::types::ExchangeSegment::IdxI.binary_code()
        && price.is_finite()
        && price > 0.0
    {
        start_self_test_entry(oms, self_test, mark.security_id, mark.segment_code).await;
        // HP-2: the entry placement changed the pending set.
        book.rebuild_pending_paper(oms);
    }

    // Runtime-side filter: only sids with a position or a pending paper
    // order reach the risk engine's market_prices map (keeps it small).
    // HP-2: both checks are O(1) lookups — NO per-mark scan/alloc over the
    // whole order book (the pending index is rebuilt on order events).
    let has_position = risk.net_lots_for(mark.security_id) != 0;
    let has_pending_paper = book.has_pending_paper(mark.security_id);
    if !(has_position || has_pending_paper) {
        return;
    }
    risk.update_market_price(mark.security_id, price);

    // Next-mark paper filler (F12 fill-once: the pending index holds
    // non-terminal PAPER orders only, and a filled order goes terminal
    // Traded + drops out on the rebuild).
    if paper_fill && oms.is_dry_run() && has_pending_paper {
        // Cold branch by construction (only reached when THIS sid has a
        // pending paper order); the id Vec clone is bounded by that sid's
        // pending count (1-2 in practice — self-test legs).
        let pending: Vec<String> = book
            .pending_paper
            .get(&mark.security_id)
            .cloned()
            .unwrap_or_default();
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
    // HP-2: the fill (and any self-test close placement above) changed the
    // pending set — refresh the per-sid index.
    book.rebuild_pending_paper(oms);
}

/// Republish the hot-path marks gate: marks are wanted while ANY position,
/// pending paper order, or active self-test needs them.
fn republish_marks_wanted(
    marks_wanted: &AtomicBool,
    oms: &OrderManagementSystem,
    risk: &RiskEngine,
    self_test: &SelfTestState,
) {
    // HP-2: active_order_count() is alloc-free (per-event cold path).
    let wanted = risk.open_position_count() > 0
        || oms.active_order_count() > 0
        || self_test.phase != SelfTestPhase::Idle;
    marks_wanted.store(wanted, Ordering::Release);
}

/// Reconcile cycle: honest dry-run heartbeat + the REAL local invariant
/// (F14); live mode (future) delegates to `oms.reconcile()`.
async fn run_reconcile_cycle(oms: &mut OrderManagementSystem, risk: &RiskEngine, book: &BookState) {
    if oms.is_dry_run() {
        metrics::counter!("tv_oms_reconcile_runs_total", "mode" => "paper_noop").increment(1);
        let report = local_reconcile(oms, risk, &book.mirror);
        if report.divergences > 0 || report.order_fold_divergences > 0 {
            error!(
                code = ErrorCode::OmsGapReconciliation.code_str(),
                divergences = report.divergences,
                order_fold_divergences = report.order_fold_divergences,
                sids_checked = report.sids_checked,
                "OMS-GAP-02: LOCAL reconcile invariant DIVERGED — the FillEvent \
                 mirror and/or the per-order traded_qty fold disagree with the \
                 risk engine's net lots (fills were double-applied, lost, or \
                 refused inside this process)"
            );
            metrics::counter!("tv_oms_local_reconcile_divergence_total")
                .increment((report.divergences + report.order_fold_divergences) as u64);
        } else {
            info!(
                open_paper_orders = oms.active_order_count(),
                open_positions = risk.open_position_count(),
                sids_checked = report.sids_checked,
                realized_pnl = risk.total_realized_pnl(),
                "paper mode — broker reconcile SKIPPED; local Σfills==net_lots \
                 invariant HELD on both legs (heartbeat, not a broker \
                 reconciliation)"
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
async fn market_close_sweep(oms: &mut OrderManagementSystem, book: &mut BookState) {
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
    // HP-2: cancels emptied the pending set.
    book.rebuild_pending_paper(oms);
}

/// The 16:00 IST daily reset body — ONE block clears risk + oms + mirror +
/// tripwire (+ latch + pending index) + the marks gate. Extracted (M7,
/// fix-round 2026-07-14) so the unit test drives THIS production fn instead
/// of re-enacting the loop arm; F16 atomicity comes from the single-owner
/// task serialization at the call site.
fn perform_daily_reset(
    risk: &mut RiskEngine,
    oms: &mut OrderManagementSystem,
    book: &mut BookState,
    self_test: &mut SelfTestState,
    marks_wanted: &AtomicBool,
) {
    info!(
        orders_placed = oms.total_placed(),
        realized_pnl = risk.total_realized_pnl(),
        "order runtime daily reset — zeroing paper book + risk state"
    );
    risk.reset_daily();
    oms.reset_daily();
    book.clear();
    self_test.phase = SelfTestPhase::Idle;
    marks_wanted.store(false, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Self-test driving
// ---------------------------------------------------------------------------

/// C6 (fix-round 2026-07-14): a failed/timed-out self-test can leave the
/// entry position OPEN with nothing watching it (`marks_wanted` stays armed
/// all day for the ghost). Report it loudly — the position still shows in
/// every reconcile heartbeat and is cleared by the 16:00 reset.
fn report_dangling_self_test_position(risk: &RiskEngine, sid: u64, stage: &'static str) {
    let net = risk.net_lots_for(sid);
    if net != 0 {
        error!(
            code = ErrorCode::OmsGapDryRunSafety.code_str(),
            security_id = sid,
            net_lots = net,
            stage,
            "OMS-GAP-06: self-test failure left a DANGLING paper position — no \
             close leg is watching it; it persists until the 16:00 IST reset \
             (visible in every reconcile heartbeat)"
        );
    }
}

/// Housekeeping-tick half: arm the daily cycle when gates pass; time out a
/// stuck cycle (F17). C6 (fix-round 2026-07-14): the timeout path CANCELS
/// the outstanding self-test order (previously a still-active entry order
/// could be filled by the paper filler LATER, opening a ghost position no
/// state machine watched) and loudly reports any dangling position.
async fn drive_self_test_timers(
    self_test: &mut SelfTestState,
    ctx: &RuntimeCtx,
    oms: &mut OrderManagementSystem,
    risk: &RiskEngine,
    book: &mut BookState,
    secs_of_day: u32,
    today: i64,
    enabled: bool,
) {
    // Timeout first: a stuck cycle fails loudly + latches for the day.
    if self_test.phase != SelfTestPhase::Idle
        && self_test.started_at.elapsed() > Duration::from_secs(SELF_TEST_TIMEOUT_SECS)
    {
        // C6 cleanup: cancel the outstanding self-test order so the filler
        // can never fill it after the state machine stopped watching.
        let outstanding = match &self_test.phase {
            SelfTestPhase::AwaitingEntryFill { order_id, sid }
            | SelfTestPhase::AwaitingCloseFill { order_id, sid } => Some((order_id.clone(), *sid)),
            _ => None,
        };
        if let Some((order_id, sid)) = outstanding {
            if let Err(err) = oms.cancel_order(&order_id).await {
                // Already terminal (e.g. filled just before the timeout) —
                // benign; the dangling-position check below still fires.
                debug!(?err, order_id = %order_id, "self-test timeout cancel refused");
            }
            book.rebuild_pending_paper(oms);
            report_dangling_self_test_position(risk, sid, "timeout");
        }
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
                    // C6: the entry already FILLED (+1 lot) — a failed close
                    // placement leaves that position dangling for the rest
                    // of the day. Loud, never silent.
                    report_dangling_self_test_position(risk, sid, "close_placement_failed");
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

    /// Real boot config (unit tests run with cwd = the crate dir).
    fn load_test_config() -> Arc<ApplicationConfig> {
        use figment::Figment;
        use figment::providers::{Format, Toml};
        let path = if std::path::Path::new("config/base.toml").exists() {
            "config/base.toml"
        } else {
            "../../config/base.toml"
        };
        Arc::new(
            Figment::new()
                .merge(Toml::file(path))
                .extract()
                .expect("config/base.toml must parse"), // APPROVED: test
        )
    }

    /// A REAL RuntimeCtx (disabled notifier, real calendar, empty token
    /// handle) so tests drive the actual production arm bodies —
    /// `process_mark` / `handle_order_update_event` — never replicas.
    fn make_ctx() -> RuntimeCtx {
        let config = load_test_config();
        let calendar = Arc::new(
            TradingCalendar::from_config(&config.trading).expect("calendar builds"), // APPROVED: test
        );
        RuntimeCtx {
            config,
            notifier: NotificationService::disabled(),
            calendar,
            marks_wanted: Arc::new(AtomicBool::new(false)),
            token_handle: Arc::new(arc_swap::ArcSwap::from_pointee(None)),
            client_id: "100".to_string(),
            auth_notify: Arc::new(Notify::new()),
        }
    }

    /// Orphan / wire-shaped OrderUpdate via serde defaults.
    fn wire_update(order_no: &str, traded_qty: i64, source: &str) -> OrderUpdate {
        let mut update: OrderUpdate =
            serde_json::from_str("{}").expect("OrderUpdate fields are serde-default"); // APPROVED: test
        update.exchange = "NSE".to_string();
        update.segment = "D".to_string();
        update.order_no = order_no.to_string();
        update.status = "TRADED".to_string();
        update.traded_qty = traded_qty;
        update.avg_traded_price = 123.45;
        update.source = source.to_string();
        update
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

    /// L1 (fix-round 2026-07-14): renamed honestly — the drop-arm COUNTER
    /// increment is not assertable without a metrics recorder in this test
    /// process; what this pins is the no-block / no-panic overflow contract.
    #[tokio::test]
    async fn test_mark_forward_channel_full_never_blocks_or_panics() {
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

    #[tokio::test]
    async fn test_local_reconcile_invariant_holds_and_diverges() {
        let mut oms = make_oms();
        let mut risk = make_risk();
        let mut book = BookState::new();
        // A real tracked order + its fill so BOTH legs (mirror + order fold)
        // hold on the clean path.
        let order_id = place_paper(&mut oms, 13, TransactionType::Buy).await;
        let order = oms.order(&order_id).cloned().expect("tracked"); // APPROVED: test
        let synthetic = synthesize_paper_fill(&order, 100.0, 0).expect("fill"); // APPROVED: test
        let fill = oms
            .handle_order_update(&synthetic)
            .ok()
            .flatten()
            .expect("fill event"); // APPROVED: test
        assert!(apply_fill(&mut risk, &mut book, &fill));
        let ok = local_reconcile(&oms, &risk, &book.mirror);
        assert_eq!(ok.sids_checked, 1);
        assert_eq!(ok.divergences, 0, "mirror==net_lots after a clean fill");
        assert_eq!(ok.order_fold_divergences, 0, "order fold matches too");
        // Poison the mirror → the mirror leg must flag it.
        book.mirror.insert(13, 99);
        let bad = local_reconcile(&oms, &risk, &book.mirror);
        assert_eq!(bad.divergences, 1, "a poisoned mirror must be flagged");
    }

    /// C7 second leg: a fill LOST between the engine and `record_fill` (the
    /// broadcast-lag drop shape — mirror and risk stay consistent at 0) is
    /// caught by the per-order traded_qty fold, which the mirror leg
    /// structurally cannot see.
    #[tokio::test]
    async fn test_local_reconcile_order_fold_catches_lost_fill() {
        let mut oms = make_oms();
        let risk = make_risk();
        let book = BookState::new();
        let order_id = place_paper(&mut oms, 13, TransactionType::Buy).await;
        let order = oms.order(&order_id).cloned().expect("tracked"); // APPROVED: test
        let synthetic = synthesize_paper_fill(&order, 100.0, 0).expect("fill"); // APPROVED: test
        // The engine records the fill on the ORDER — but the FillEvent is
        // "lost" (never applied to risk/mirror), as in a lagged drop.
        let _lost = oms
            .handle_order_update(&synthetic)
            .ok()
            .flatten()
            .expect("fill event produced then dropped"); // APPROVED: test
        let report = local_reconcile(&oms, &risk, &book.mirror);
        assert_eq!(
            report.divergences, 0,
            "mirror leg CANNOT see a lost fill (both sides 0)"
        );
        assert_eq!(
            report.order_fold_divergences, 1,
            "the order-fold leg MUST flag the lost fill for a tracked order"
        );
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
        // Leg 1 only: this synthetic walk applies fills WITHOUT tracked
        // orders, so the order-fold leg legitimately diverges (empty book
        // vs positions) — it is asserted by its own dedicated test above.
        let report = local_reconcile(&make_oms(), &risk, &book.mirror);
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
        // M7 (fix-round 2026-07-14): drive the PRODUCTION reset fn — the
        // same body arm-4 calls — never a re-enactment.
        let mut self_test = SelfTestState::new();
        self_test.phase = SelfTestPhase::AwaitingMark;
        perform_daily_reset(
            &mut risk,
            &mut oms,
            &mut book,
            &mut self_test,
            &marks_wanted,
        );
        assert_eq!(risk.net_lots_for(13), 0);
        assert!(book.mirror.is_empty());
        assert!(book.tripwire.is_empty());
        assert!(book.pending_paper.is_empty());
        assert!(book.tripwire_reported.is_empty());
        assert!(oms.all_orders().is_empty());
        assert_eq!(self_test.phase, SelfTestPhase::Idle);
        assert!(!marks_wanted.load(Ordering::Acquire));
    }

    /// E1: the reset-epoch gate — the same-day 16:00 fire AND the new-day
    /// catch-up (a process suspended across [16:00, midnight) resets on its
    /// first tick of the new day; day-1 halt/positions never carry).
    #[test]
    fn test_reset_epoch_same_day_fire_and_new_day_catch_up() {
        // Before 16:00 the epoch is YESTERDAY's boundary; at/after, today's.
        assert_eq!(reset_epoch(DAILY_RESET_SECS_OF_DAY_IST - 1, 100), 99);
        assert_eq!(reset_epoch(DAILY_RESET_SECS_OF_DAY_IST, 100), 100);
        assert_eq!(reset_epoch(23 * 3600, 100), 100);
        // Normal day flow: reset at day-100 16:00 (epoch 100); overnight +
        // next morning stay epoch 100 (no double reset); day-101 16:00 → 101.
        assert_eq!(reset_epoch(6 * 3600, 101), 100);
        assert_eq!(reset_epoch(DAILY_RESET_SECS_OF_DAY_IST, 101), 101);
        // E1 suspend shape: last reset epoch 99 (day-99 16:00); the process
        // sleeps through day-100's 16:00; first tick day-101 06:00 → epoch
        // 100 > 99 → the catch-up arm fires BEFORE the session, never after.
        assert!(reset_epoch(6 * 3600, 101) > 99);
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

    /// L6 (fix-round 2026-07-14): renamed honestly — this pins the PURE
    /// time-of-day window only; the calendar/holiday gate lives in
    /// drive_self_test_timers (`ctx.calendar.is_trading_day_today()`,
    /// source-visible) and cannot be driven deterministically from a test
    /// running on an arbitrary real day.
    #[test]
    fn test_selftest_window_gate_boundaries() {
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
        // fail() latches (no retry storm); the SUCCESS-path latch is driven
        // through the production advance path by
        // test_selftest_full_cycle_via_production_advance_latches_day (M6).
        let mut st = SelfTestState::new();
        let today = 20_000_i64;
        st.phase = SelfTestPhase::AwaitingMark;
        st.fail(today, "timeout");
        assert_eq!(st.phase, SelfTestPhase::Idle);
        assert_eq!(st.last_run_day, today);
    }

    /// M6 (fix-round 2026-07-14): the full self-test cycle through the
    /// PRODUCTION advance path — arm → index-spot mark picks the sid +
    /// places entry → entry fills → close placed → next mark fills the
    /// close → flat + PASSED + the day latches; a second arming attempt is
    /// refused by the latch (deterministic: the latch gate precedes the
    /// calendar gate in drive_self_test_timers).
    #[tokio::test]
    async fn test_selftest_full_cycle_via_production_advance_latches_day() {
        let ctx = make_ctx();
        let mut oms = make_oms();
        let mut risk = make_risk();
        let mut book = BookState::new();
        let mut self_test = SelfTestState::new();
        self_test.phase = SelfTestPhase::AwaitingMark; // the arm step
        self_test.started_at = std::time::Instant::now();
        // First INDEX-SPOT mark: picks the sid, places + fills the entry,
        // places the close (all through the real arm-2 body).
        process_mark(
            &mut oms,
            &mut risk,
            &mut book,
            &mut self_test,
            &ctx,
            MarkUpdate {
                security_id: 13,
                segment_code: 0,
                price: 100.0,
            },
            true,
        )
        .await;
        assert!(
            matches!(self_test.phase, SelfTestPhase::AwaitingCloseFill { .. }),
            "entry must fill and the close must be placed, got {:?}",
            self_test.phase
        );
        assert_eq!(risk.net_lots_for(13), 1, "entry fill opened 1 lot");
        // Second mark fills the close → flat → PASSED → day latched.
        process_mark(
            &mut oms,
            &mut risk,
            &mut book,
            &mut self_test,
            &ctx,
            MarkUpdate {
                security_id: 13,
                segment_code: 0,
                price: 101.0,
            },
            true,
        )
        .await;
        let today = ist_day_number(chrono::Utc::now().timestamp());
        assert_eq!(self_test.phase, SelfTestPhase::Idle, "cycle completed");
        assert_eq!(
            self_test.last_run_day, today,
            "the SUCCESS path must latch the day"
        );
        assert_eq!(risk.net_lots_for(13), 0, "flat after the close fill");
        assert!(risk.total_realized_pnl().is_finite());
        // The once-per-day latch refuses a second run (latch gate fires
        // before the calendar gate — deterministic on any test day).
        drive_self_test_timers(
            &mut self_test,
            &ctx,
            &mut oms,
            &risk,
            &mut book,
            SELF_TEST_WINDOW_START_SECS_OF_DAY_IST,
            today,
            true,
        )
        .await;
        assert_eq!(
            self_test.phase,
            SelfTestPhase::Idle,
            "a second same-day run must be refused by the latch"
        );
    }

    /// E4: the self-test sid pick ignores NON-index marks (a far-month
    /// future with minutes of legitimate silence would false-FAIL the day).
    #[tokio::test]
    async fn test_selftest_sid_pick_requires_index_spot_mark() {
        let ctx = make_ctx();
        let mut oms = make_oms();
        let mut risk = make_risk();
        let mut book = BookState::new();
        let mut self_test = SelfTestState::new();
        self_test.phase = SelfTestPhase::AwaitingMark;
        self_test.started_at = std::time::Instant::now();
        // An NSE_FNO mark (segment 2) must NOT start the cycle.
        process_mark(
            &mut oms,
            &mut risk,
            &mut book,
            &mut self_test,
            &ctx,
            MarkUpdate {
                security_id: 49_081,
                segment_code: 2,
                price: 400.0,
            },
            true,
        )
        .await;
        assert_eq!(
            self_test.phase,
            SelfTestPhase::AwaitingMark,
            "a non-index mark must not pick the self-test sid (E4)"
        );
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
        // L2 (fix-round 2026-07-14): EXACT values — t is a UTC-midnight
        // instant (1_784_073_600 = 20_649 × 86_400), so IST secs-of-day is
        // exactly the 05:30 offset and the IST day ordinal is 20_649.
        let t = 1_784_073_600_i64;
        assert_eq!(ist_secs_of_day(t), 19_800);
        assert_eq!(ist_day_number(t), 20_649);
    }

    #[test]
    fn test_close_sweep_due_skips_non_trading_day() {
        let in_window = TICK_PERSIST_END_SECS_OF_DAY_IST; // 15:30:00 IST
        // In-window, un-latched, trading day → the sweep fires.
        assert!(close_sweep_due(in_window, 100, i64::MIN, true));
        // NON-trading day (weekend/holiday) → NEVER fires, even in-window
        // with the once-per-day latch un-armed (the 2026-07-17 audit gap).
        assert!(!close_sweep_due(in_window, 100, i64::MIN, false));
        assert!(!close_sweep_due(in_window + 60, 100, 99, false));
        // Already swept today → latched off.
        assert!(!close_sweep_due(in_window, 100, 100, true));
        // Outside [15:30, 16:00) IST → not due.
        assert!(!close_sweep_due(in_window - 1, 100, i64::MIN, true));
        assert!(!close_sweep_due(
            DAILY_RESET_SECS_OF_DAY_IST,
            100,
            i64::MIN,
            true
        ));
    }

    // -------------------------------------------------------------------
    // Source filter (F18) — the consumer-boundary semantics
    // -------------------------------------------------------------------

    #[test]
    fn test_source_n_filtered_empty_tolerated() {
        // M3 (fix-round 2026-07-14): the PRODUCTION predicate — the exact fn
        // handle_order_update_event calls — never a replica.
        assert!(
            !is_foreign_source(""),
            "empty Source tolerated (serde default)"
        );
        assert!(!is_foreign_source("P"), "API-sourced events consumed");
        assert!(is_foreign_source("N"), "manual Dhan-app events filtered");
        assert!(is_foreign_source("X"), "unknown sources filtered");
    }

    /// M3 discriminating half: a Source=N fill for a TRACKED order must
    /// leave the book untouched THROUGH the real arm-1 body; the identical
    /// P-sourced update fills — deleting the production filter fails this.
    #[tokio::test]
    async fn test_source_n_update_for_tracked_order_never_fills() {
        let ctx = make_ctx();
        let mut oms = make_oms();
        let mut risk = make_risk();
        let mut book = BookState::new();
        let mut self_test = SelfTestState::new();
        let order_id = place_paper(&mut oms, 13, TransactionType::Buy).await;
        book.rebuild_pending_paper(&oms);
        let order = oms.order(&order_id).cloned().expect("tracked"); // APPROVED: test
        let mut update = synthesize_paper_fill(&order, 100.0, 0).expect("fill"); // APPROVED: test
        update.source = "N".to_string();
        handle_order_update_event(
            &mut oms,
            &mut risk,
            &mut book,
            &mut self_test,
            &ctx,
            &update,
            true,
        )
        .await;
        assert_eq!(risk.net_lots_for(13), 0, "Source=N must never fill");
        assert_eq!(oms.order(&order_id).map(|o| o.traded_qty), Some(0));
        // The identical event with Source=P DOES fill (discriminating arm).
        update.source = "P".to_string();
        handle_order_update_event(
            &mut oms,
            &mut risk,
            &mut book,
            &mut self_test,
            &ctx,
            &update,
            true,
        )
        .await;
        assert_eq!(risk.net_lots_for(13), 1, "Source=P must fill");
    }

    /// M5 (F3 loudness path): an orphan fill-carrying update through the
    /// real arm-1 body is tolerated — no panic, no position, book unchanged
    /// (the warn + counter emission sits on this exact branch; asserting
    /// the counter needs a metrics recorder the test process does not
    /// install — state assertions + the S3-sanitized log id cover the rest).
    #[tokio::test]
    async fn test_orphan_fill_update_tolerated_book_unchanged() {
        let ctx = make_ctx();
        let mut oms = make_oms();
        let mut risk = make_risk();
        let mut book = BookState::new();
        let mut self_test = SelfTestState::new();
        // Multi-KB hostile order_no exercises the S3 log_safe_id path too.
        let hostile_id = format!("GHOST-{}\u{202e}{}", "A".repeat(4096), "\u{0007}");
        let update = wire_update(&hostile_id, 75, "P");
        handle_order_update_event(
            &mut oms,
            &mut risk,
            &mut book,
            &mut self_test,
            &ctx,
            &update,
            true,
        )
        .await;
        assert_eq!(risk.position_security_ids().count(), 0, "no position");
        assert!(book.mirror.is_empty(), "mirror untouched");
        assert!(oms.all_orders().is_empty(), "orphan never tracked");
    }

    /// M4 (partial — honest scope): exercises every NotifierAlertSink arm
    /// (all 4 OmsAlert variants + the risk halt) through the PRODUCTION sink
    /// against a disabled NotificationService — the mapping code executes;
    /// asserting the emitted NotificationEvent payloads would need a capture
    /// seam NotificationService does not expose (flagged in the plan).
    #[test]
    fn test_alert_sink_event_mapping() {
        let sink = NotifierAlertSink(NotificationService::disabled());
        sink.fire(OmsAlert::OrderRejected {
            correlation_id: "corr-1".to_string(),
            reason: "test".to_string(),
        });
        sink.fire(OmsAlert::CircuitBreakerOpened {
            consecutive_failures: 3,
        });
        sink.fire(OmsAlert::CircuitBreakerClosed);
        sink.fire(OmsAlert::RateLimitExhausted {
            limit_type: "per_second".to_string(),
        });
        RiskAlertSink::fire_risk_halt(&sink, "MaxDailyLossExceeded");
    }

    /// S3: bounded + sanitized log ids — control/BiDi stripped, length capped.
    #[test]
    fn test_log_safe_id_truncates_and_strips() {
        let hostile = format!("evil\u{202e}{}\u{0007}", "B".repeat(500));
        let safe = log_safe_id(&hostile);
        assert!(safe.chars().count() <= 64, "must cap at 64 chars");
        assert!(!safe.contains('\u{202e}'), "BiDi override stripped");
        assert!(!safe.contains('\u{0007}'), "control char stripped");
        assert_eq!(log_safe_id("PAPER-1"), "PAPER-1", "normal ids untouched");
    }

    // -------------------------------------------------------------------
    // Segment char round-trip for synthetic updates
    // -------------------------------------------------------------------

    // -------------------------------------------------------------------
    // H3 (fix-round 2026-07-14): the promised end-to-end chain through the
    // ACTUAL production arm bodies — place → paper fill via a mark →
    // net_lots ≠ 0 → adverse mark → daily-loss halt → RiskHalt reaches a
    // test alert sink (the production trigger_halt fires it) → check_order
    // rejects.
    // -------------------------------------------------------------------

    struct CapturingRiskSink {
        fires: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl tickvault_trading::risk::engine::RiskAlertSink for CapturingRiskSink {
        fn fire_risk_halt(&self, reason: &'static str) {
            self.fires
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(reason.to_string());
        }
    }

    #[tokio::test]
    async fn test_e2e_place_fill_mark_halt_fires_risk_halt_sink() {
        let ctx = make_ctx();
        let mut oms = make_oms();
        // 2% of 10L capital → 20,000 daily-loss threshold.
        let mut risk = make_risk();
        let fires = Arc::new(std::sync::Mutex::new(Vec::new()));
        risk.set_alert_sink(Box::new(CapturingRiskSink {
            fires: Arc::clone(&fires),
        }));
        let mut book = BookState::new();
        let mut self_test = SelfTestState::new();

        // 1. PLACE: one paper order — 1 lot of 75 units (lot_size 75).
        let order_id = oms
            .place_order(PlaceOrderRequest {
                security_id: 13,
                transaction_type: TransactionType::Buy,
                order_type: OrderType::Market,
                product_type: ProductType::Intraday,
                validity: OrderValidity::Day,
                quantity: 75,
                price: 0.0,
                trigger_price: 0.0,
                lot_size: 75,
                expiry_date: None,
            })
            .await
            .expect("paper placement never fails"); // APPROVED: test
        book.rebuild_pending_paper(&oms);
        assert!(book.has_pending_paper(13));

        // 2. FILL: a mark @400 through the REAL arm-2 body — the paper
        //    filler synthesizes the fill through the canonical arm-1 path.
        process_mark(
            &mut oms,
            &mut risk,
            &mut book,
            &mut self_test,
            &ctx,
            MarkUpdate {
                security_id: 13,
                segment_code: 0,
                price: 400.0,
            },
            true,
        )
        .await;
        assert_eq!(risk.net_lots_for(13), 1, "paper fill must open 1 lot");
        assert_eq!(
            oms.order(&order_id).map(|o| o.traded_qty),
            Some(75),
            "the order filled through the production state machine"
        );

        // 3. ADVERSE MARK: 400 → 100 with lot_size 75 = -22,500 unrealized
        //    (≥ the 20,000 threshold) via the same production arm body.
        process_mark(
            &mut oms,
            &mut risk,
            &mut book,
            &mut self_test,
            &ctx,
            MarkUpdate {
                security_id: 13,
                segment_code: 0,
                price: 100.0,
            },
            true,
        )
        .await;

        // 4. HALT: the ≤1/sec evaluation the loop runs after mark batches.
        assert!(
            risk.evaluate_daily_loss_halt(),
            "the mark-to-market drawdown must halt"
        );
        assert!(risk.is_halted());
        // Idempotent second evaluation — the sink must NOT re-fire.
        assert!(risk.evaluate_daily_loss_halt());
        {
            let fired = fires
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(
                fired.as_slice(),
                ["MaxDailyLossExceeded".to_string()],
                "RiskHalt must reach the alert sink EXACTLY once per episode"
            );
        }

        // 5. REJECT: the pre-trade gate refuses everything while halted.
        assert!(
            !risk.check_order(13, 1).is_approved(),
            "a halted book must reject new orders"
        );
    }

    // -------------------------------------------------------------------
    // S2 + HP-2 + HP-3 fix-round coverage
    // -------------------------------------------------------------------

    /// S2: a fill with an UNMAPPABLE segment is refused — it can no longer
    /// bypass the tripwire partition check.
    #[test]
    fn test_apply_fill_refuses_unknown_segment() {
        let mut risk = make_risk();
        let mut book = BookState::new();
        let fill = FillEvent {
            security_id: 27,
            segment_code: SEGMENT_CODE_UNKNOWN,
            fill_lots: 1,
            avg_price: 100.0,
            lot_size: 1,
            order_id: "PAPER-1".to_string(),
        };
        assert!(
            !apply_fill(&mut risk, &mut book, &fill),
            "an unknown-segment fill must be REFUSED (tripwire bypass — S2)"
        );
        assert_eq!(risk.net_lots_for(27), 0);
        assert!(book.mirror.is_empty());
    }

    /// HP-3/E3: the tripwire error! is edge-latched per sid — the reported
    /// set records the sid on the first divergence and the skip semantics
    /// persist per event (the counter keeps counting; the LOG fires once).
    #[test]
    fn test_tripwire_divergence_error_latched_per_sid() {
        let mut book = BookState::new();
        assert!(book.tripwire_ok(27, 0));
        assert!(!book.tripwire_ok(27, 2), "first divergence skips");
        assert!(
            book.tripwire_reported.contains(&27),
            "first divergence must latch the reported set (error! fired)"
        );
        assert!(!book.tripwire_ok(27, 2), "later divergences still skip");
        assert_eq!(
            book.tripwire_reported.len(),
            1,
            "one latch entry per sid per episode"
        );
        // The daily reset re-arms the latch.
        book.clear();
        assert!(book.tripwire_reported.is_empty());
    }

    /// HP-2: the per-sid pending-paper index tracks placements and fills.
    #[tokio::test]
    async fn test_pending_paper_index_tracks_placements_and_fills() {
        let mut oms = make_oms();
        let mut book = BookState::new();
        assert!(!book.has_pending_paper(13));
        let order_id = place_paper(&mut oms, 13, TransactionType::Buy).await;
        book.rebuild_pending_paper(&oms);
        assert!(book.has_pending_paper(13));
        assert!(!book.has_pending_paper(14), "index is per-sid");
        // Fill → terminal → drops out on rebuild.
        let order = oms.order(&order_id).cloned().expect("tracked"); // APPROVED: test
        let synthetic = synthesize_paper_fill(&order, 100.0, 0).expect("fill"); // APPROVED: test
        let _ = oms.handle_order_update(&synthetic);
        book.rebuild_pending_paper(&oms);
        assert!(
            !book.has_pending_paper(13),
            "a terminal order must leave the pending index"
        );
    }

    /// C6: a self-test timeout cancels the outstanding order so the filler
    /// can never fill it afterwards (the ghost-position hole).
    #[tokio::test]
    async fn test_selftest_timeout_cancels_outstanding_order() {
        let ctx = make_ctx();
        let mut oms = make_oms();
        let risk = make_risk();
        let mut book = BookState::new();
        let mut self_test = SelfTestState::new();
        let order_id = place_paper(&mut oms, 13, TransactionType::Buy).await;
        book.rebuild_pending_paper(&oms);
        self_test.phase = SelfTestPhase::AwaitingEntryFill {
            order_id: order_id.clone(),
            sid: 13,
        };
        // A started_at far in the past forces the timeout arm.
        self_test.started_at = std::time::Instant::now()
            .checked_sub(Duration::from_secs(SELF_TEST_TIMEOUT_SECS + 5))
            .expect("monotonic clock has > timeout of history"); // APPROVED: test
        drive_self_test_timers(
            &mut self_test,
            &ctx,
            &mut oms,
            &risk,
            &mut book,
            12 * 3600,
            ist_day_number(chrono::Utc::now().timestamp()),
            true,
        )
        .await;
        assert_eq!(self_test.phase, SelfTestPhase::Idle, "timeout latches");
        assert!(
            oms.order(&order_id)
                .is_some_and(tickvault_trading::oms::ManagedOrder::is_terminal),
            "the outstanding self-test order must be CANCELLED on timeout \
             (a later paper fill would open a ghost position — C6)"
        );
        assert!(
            !book.has_pending_paper(13),
            "the pending index must drop the cancelled order"
        );
    }

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
