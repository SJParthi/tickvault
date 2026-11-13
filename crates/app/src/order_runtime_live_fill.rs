//! The order runtime's arm for the live paper fill chain (plan item LF-6b).
//!
//! The chain sends a paper order only once its worst-case fill is worked out
//! (owner rules of 2026-11-12). This arm then, in one go, on the runtime's
//! single-owner books:
//!
//! 1. refuses unless the OMS is dry-run (it always is: `dry_run` is
//!    hard-wired true; this is the belt to that brace);
//! 2. runs the risk engine's pre-trade check for the instrument;
//! 3. places a LIMIT order on the dry-run OMS (no HTTP; a `PAPER-n` id);
//! 4. applies the worked-out fill through the OMS state machine and the risk
//!    engine (`TRADED`, or `PART_TRADED` for part of it);
//! 5. cancels any remainder (owner rule: a partial fill's remainder is
//!    cancelled after the 60 s window); if the OMS cancel is refused by its
//!    rate, budget or circuit gate, the dry-run order is closed with a local
//!    CANCELLED update through the state machine, so no paper remainder stays
//!    open for a mark-driven filler;
//!
//! and writes a `live_order_events` row for each step (Sent, Ack, Fill,
//! Cancel, or Reject with the reason). A 1 s close for an instrument with a
//! paper position marks it to market (owner rule).
//!
//! Cold path: per paper order and per marked second, never per tick.

use tickvault_common::order_types::{
    OrderType, OrderUpdate, OrderValidity, ProductType, TransactionType,
};
use tickvault_core::websocket::frame_ring::FrameRingReceiver;
use tickvault_storage::live_order_events_persistence::{LiveOrderEvent, LiveOrderEventKind};
use tickvault_trading::brutex_host::engine::Side;
use tickvault_trading::oms::{ManagedOrder, OmsError, OrderManagementSystem, PlaceOrderRequest};
use tickvault_trading::risk::engine::RiskEngine;
use tickvault_trading::risk::types::RiskCheck;
use tokio::sync::MutexGuard;
use tracing::{error, info, warn};

use super::{BookState, RuntimeCtx, apply_fill, synthesize_paper_fill};
use crate::live_fill::chain::{PaperCmd, PaperExecution};
use crate::live_fill::paper_rows::{self, clamp_exit};
use tickvault_common::error_code::ErrorCode;

/// Receives one command while the arm is armed; pends forever without a
/// receiver (the `select!` arm is also disabled then).
pub(super) async fn recv_live_fill(
    rx: &mut Option<MutexGuard<'_, FrameRingReceiver<PaperCmd>>>,
) -> Option<PaperCmd> {
    match rx.as_mut() {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// The paper fill update for `traded_qty` shares at `avg`: `TRADED` when it
/// covers the order, `PART_TRADED` otherwise. `None` for a non-positive
/// quantity or price (never fabricated). O(1).
pub(super) fn synthesize_paper_fill_qty(
    order: &ManagedOrder,
    avg: f64,
    segment_code: u8,
    traded_qty: i64,
) -> Option<OrderUpdate> {
    if traded_qty <= 0 {
        return None;
    }
    let mut u = synthesize_paper_fill(order, avg, segment_code)?;
    let traded = traded_qty.min(order.quantity);
    u.traded_qty = traded;
    u.remaining_quantity = order.quantity.saturating_sub(traded);
    u.order_type = "LIMIT".to_owned();
    if traded < order.quantity {
        u.status = "PART_TRADED".to_owned();
    }
    Some(u)
}

/// Closes a DRY-RUN order's open remainder with a local CANCELLED update
/// through the OMS state machine (the same path a broker cancel ack takes),
/// keeping the traded quantity and average already booked. No rate, budget
/// or circuit gate, no HTTP: used only after the OMS cancel was refused, and
/// only on a dry-run OMS. `Ok` when the order is terminal afterwards. O(1).
///
/// # Errors
/// The OMS is not dry-run, the order is unknown, or the state machine refused
/// the transition (the reason text).
fn close_paper_remainder(
    oms: &mut OrderManagementSystem,
    order_id: &str,
    segment_code: u8,
) -> Result<(), String> {
    if !oms.is_dry_run() {
        return Err("not_dry_run".to_owned());
    }
    let Some(order) = oms.order(order_id).cloned() else {
        return Err("order_not_found".to_owned());
    };
    if order.is_terminal() {
        return Ok(());
    }
    let seed = if order.price.is_finite() && order.price > 0.0 {
        order.price
    } else {
        1.0
    };
    let Some(mut u) = synthesize_paper_fill(&order, seed, segment_code) else {
        return Err("no_update".to_owned());
    };
    u.status = "CANCELLED".to_owned();
    u.order_type = "LIMIT".to_owned();
    u.traded_qty = order.traded_qty;
    u.remaining_quantity = order.quantity.saturating_sub(order.traded_qty);
    u.traded_price = order.avg_traded_price;
    u.avg_traded_price = order.avg_traded_price;
    match oms.handle_order_update(&u) {
        Ok(None) => {}
        Ok(Some(_)) => return Err("unexpected_fill".to_owned()),
        Err(e) => return Err(e.to_string()),
    }
    if oms.order(order_id).is_some_and(ManagedOrder::is_terminal) {
        Ok(())
    } else {
        Err("still_open".to_owned())
    }
}

/// Writes one row derived from the signal's template. Never waits; a full
/// channel is counted (the writer's channel is the same bound the chain uses).
fn emit(
    ctx: &RuntimeCtx,
    base: &LiveOrderEvent,
    kind: LiveOrderEventKind,
    seq: &mut i32,
    fill: impl FnOnce(&mut LiveOrderEvent),
) {
    let Some(tx) = ctx.live_fill_events.as_ref() else {
        return;
    };
    // The row and each step's fields are built by `live_fill::paper_rows`
    // (plan item LF-8), the same builder the offline replay uses.
    let mut row = paper_rows::order_row(base, crate::live_fill::wall_ist_ns(), kind, seq);
    fill(&mut row);
    if tx.try_send(row).is_err() {
        metrics::counter!(crate::live_fill::EVENTS_DROPPED_COUNTER).increment(1);
    }
}

/// One command from the chain (see the module docs). Returns whether it
/// can have changed the order set or a position, so the caller must republish
/// whether marks are wanted (an O(orders) scan): `false` for a `Mark`, which
/// only moves a price and re-runs the loss halt (LF-9 finding 4; it used to
/// pay the scan once per marked second per traded instrument).
pub(super) async fn handle_live_fill_cmd(
    oms: &mut OrderManagementSystem,
    risk: &mut RiskEngine,
    book: &mut BookState,
    ctx: &RuntimeCtx,
    cmd: PaperCmd,
) -> bool {
    match cmd {
        PaperCmd::Mark {
            security_id,
            segment,
            price,
        } => {
            if price.is_finite()
                && price > 0.0
                && risk.net_lots_for_in_segment(security_id, segment) != 0
            {
                risk.update_market_price_in_segment(security_id, segment, price);
                risk.evaluate_daily_loss_halt();
            }
            false
        }
        PaperCmd::Execute(x) => {
            // An exit arriving while earlier exits wait for the order gates
            // goes to the back of their queue without a place attempt: the
            // gates that refused them are still closed, and every refused
            // attempt costs an order-budget unit and an alert (LF-6b review
            // round 3, findings 3 and 4). Exits keep their order.
            if x.exit && !book.live_fill_exit_retry.is_empty() {
                queue_exit_retry(book, risk, ctx, ExitRetry::waiting(x));
                return true;
            }
            match execute(oms, risk, book, ctx, &x).await {
                Placed::RetryExit(err) => {
                    queue_exit_retry(book, risk, ctx, ExitRetry::new(x, err));
                }
                Placed::Done => report_position(risk, ctx, &x),
            }
            true
        }
    }
}

/// Exits a time-rolling OMS gate refused, kept for a resend: four times the
/// orders the chain can hold at once, so a burst of finishing windows plus a
/// minute of refusals fits.
pub(super) const LIVE_FILL_EXIT_RETRY_CAPACITY: usize =
    crate::live_fill::fill_window::MAX_PENDING_ORDERS * 4;
/// Resends (one per 1 s housekeeping tick) before an exit is given up with a
/// Reject row: one hour, the per-hour order budget's window.
pub(super) const LIVE_FILL_EXIT_RETRY_MAX_ATTEMPTS: u32 = 3_600;
/// Counter of exit resends (local `/metrics` only).
pub(super) const LIVE_FILL_EXIT_RETRIES_COUNTER: &str = "tv_live_fill_exit_retries_total";

/// What one place attempt came to.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Placed {
    /// Placed, or refused for good with a row.
    Done,
    /// An exit a rate, budget, order-book or circuit gate refused: no row
    /// yet, to be sent again (the reason text).
    RetryExit(String),
}

/// One exit waiting for a resend.
#[derive(Debug)]
pub(super) struct ExitRetry {
    x: Box<PaperExecution>,
    attempts: u32,
    last_err: String,
}

impl ExitRetry {
    fn new(x: Box<PaperExecution>, err: String) -> Self {
        Self {
            x,
            attempts: 1,
            last_err: err,
        }
    }

    /// An exit queued behind earlier ones without a place attempt.
    fn waiting(x: Box<PaperExecution>) -> Self {
        Self {
            x,
            attempts: 0,
            last_err: "queued_behind_earlier_exits".to_owned(),
        }
    }
}

/// A refusal that clears as time passes: the per-second rate limit, the
/// per-minute/hour/day budget, the order-book ceiling (reset daily) and the
/// circuit breaker. A field-validation refusal never clears. O(1).
fn place_refusal_rolls_over(err: &OmsError) -> bool {
    matches!(
        err,
        OmsError::RateLimited
            | OmsError::OrderBudgetExhausted { .. }
            | OmsError::OrderBookFull { .. }
            | OmsError::CircuitBreakerOpen
    )
}

/// Counter: position reports the chain's channel had no room for (local
/// `/metrics` only; LF-9 finding 2).
pub(super) const POSITION_REPORTS_DROPPED: &str = "tv_live_fill_position_reports_dropped_total";

/// Tells the chain what the paper book holds for `x`'s instrument now that
/// `x` (as the chain sent it) is booked, refused or given up (LF-9 finding
/// 2). Never waits: a full channel is counted, and the chain then keeps the
/// shares it counted for `x` until the daily reset. O(1), one risk probe.
fn report_position(risk: &RiskEngine, ctx: &RuntimeCtx, x: &PaperExecution) {
    let Some(tx) = ctx.live_fill_positions.as_ref() else {
        return;
    };
    let net = risk.net_lots_for_in_segment(x.security_id, x.segment);
    if tx.try_send(paper_rows::position_report(x, net)).is_err() {
        // Rare: the channel is sized to every Execute that can be in flight.
        // One line per drop, per finished paper order, never per tick.
        warn!(
            code = ErrorCode::OrderReady01GateRefused.code_str(),
            source = "live_fill_position_report_dropped",
            security_id = x.security_id,
            "live paper fill chain: a position report found its channel full; the \
             chain keeps the shares it counted for this instrument until the daily \
             reset (paper only, no order is placed)"
        );
        metrics::counter!(POSITION_REPORTS_DROPPED).increment(1);
    }
}

/// Keeps `r` for the next housekeeping tick, or gives it up with one Reject
/// row when the queue is full or its attempts are spent (the chain is told
/// nothing was booked). Cold.
fn queue_exit_retry(book: &mut BookState, risk: &RiskEngine, ctx: &RuntimeCtx, r: ExitRetry) {
    if r.attempts < LIVE_FILL_EXIT_RETRY_MAX_ATTEMPTS
        && book.live_fill_exit_retry.len() < LIVE_FILL_EXIT_RETRY_CAPACITY
    {
        book.live_fill_exit_retry.push(r);
        return;
    }
    let why = if r.attempts >= LIVE_FILL_EXIT_RETRY_MAX_ATTEMPTS {
        "retries_exhausted"
    } else {
        "retry_queue_full"
    };
    error!(
        code = ErrorCode::OrderReady01GateRefused.code_str(),
        source = "live_fill_exit_not_sent",
        security_id = r.x.security_id,
        attempts = r.attempts,
        why,
        "live paper fill: a paper exit the order gates kept refusing was given up; the \
         paper position stays open until the daily reset (paper only, no broker order)"
    );
    let mut seq = r.x.next_seq;
    emit(
        ctx,
        &r.x.base,
        LiveOrderEventKind::Reject,
        &mut seq,
        |row| paper_rows::fill_reject(row, &r.x, format!("{};{why}", r.last_err)),
    );
    report_position(risk, ctx, &r.x);
}

/// Sends every waiting exit again (the 1 s housekeeping tick). Each one is
/// clamped afresh against the position held now, so an exit whose position
/// another order already closed is refused `exit_without_position`. O(1) when
/// nothing waits; else O(waiting), at most
/// [`LIVE_FILL_EXIT_RETRY_CAPACITY`]. Cold.
pub(super) async fn retry_live_fill_exits(
    oms: &mut OrderManagementSystem,
    risk: &mut RiskEngine,
    book: &mut BookState,
    ctx: &RuntimeCtx,
) {
    if book.live_fill_exit_retry.is_empty() {
        return;
    }
    let waiting = std::mem::take(&mut book.live_fill_exit_retry);
    // The first refusal ends the round (LF-6b review round 3, findings 3 and
    // 4): every refusal that rolls over is a gate shared by all orders (rate,
    // budget, order book, circuit), so the exits after it would only be
    // refused too, each costing an order-budget unit (the OMS takes it before
    // the rate check) and a High alert. They go back in order, untouched.
    let mut gate_closed = false;
    let mut sent: u64 = 0;
    for mut r in waiting {
        if gate_closed {
            book.live_fill_exit_retry.push(r);
            continue;
        }
        sent = sent.saturating_add(1);
        match execute(oms, risk, book, ctx, &r.x).await {
            Placed::RetryExit(err) => {
                r.attempts = r.attempts.saturating_add(1);
                r.last_err = err;
                gate_closed = true;
                queue_exit_retry(book, risk, ctx, r);
            }
            Placed::Done => report_position(risk, ctx, &r.x),
        }
    }
    metrics::counter!(LIVE_FILL_EXIT_RETRIES_COUNTER).increment(sent);
}

// The exit clamp (`clamp_exit`) moved to `live_fill::paper_rows` unchanged
// (plan item LF-8), shared with the offline replay.

async fn execute(
    oms: &mut OrderManagementSystem,
    risk: &mut RiskEngine,
    book: &mut BookState,
    ctx: &RuntimeCtx,
    x: &PaperExecution,
) -> Placed {
    let mut seq = x.next_seq;
    // An exit is reduce-only against the paper position the risk engine
    // actually holds (LF-6b review finding 6): the chain cannot see a
    // refused or part-booked entry, so it may ask for more than is held.
    let clamped;
    let x = if x.exit {
        let Some(c) = clamp_exit(risk.net_lots_for_in_segment(x.security_id, x.segment), x) else {
            emit(ctx, &x.base, LiveOrderEventKind::Reject, &mut seq, |r| {
                paper_rows::fill_reject(r, x, "exit_without_position".to_owned());
            });
            return Placed::Done;
        };
        clamped = c;
        &clamped
    } else {
        x
    };
    let reject = |seq: &mut i32, reason: String| {
        emit(ctx, &x.base, LiveOrderEventKind::Reject, seq, |r| {
            paper_rows::fill_reject(r, x, reason);
        });
    };
    if !oms.is_dry_run() {
        error!(
            code = ErrorCode::OmsGapDryRunSafety.code_str(),
            source = "live_fill_not_dry_run",
            security_id = x.security_id,
            "OMS-GAP-06: the live paper fill chain reached an OMS that is NOT dry-run — \
             refused; the chain only ever places paper orders"
        );
        reject(&mut seq, "refused:not_dry_run".to_owned());
        return Placed::Done;
    }
    let lot = i64::from(x.lot_size.max(1));
    let lots = i32::try_from(x.qty / lot).unwrap_or(i32::MAX);
    let signed = match x.side {
        Side::Buy => lots,
        Side::Sell => lots.saturating_neg(),
    };
    // A clamped exit can only bring the position toward flat, so the halt and
    // the pre-trade limits do not apply to it (LF-6b review round 2, findings
    // 6 and 8): a daily-loss halt stops NEW risk, and refusing the order that
    // removes risk left a losing paper position open and marked until the
    // 16:00 reset. The OMS treats a cancel the same way (no halt gate).
    if !x.exit
        && let RiskCheck::Rejected { breach, .. } =
            risk.check_order_in_segment(x.security_id, x.segment, signed)
    {
        reject(&mut seq, format!("risk:{breach:?}"));
        return Placed::Done;
    }
    let request = PlaceOrderRequest {
        security_id: x.security_id,
        exchange_segment: x.segment,
        transaction_type: match x.side {
            Side::Buy => TransactionType::Buy,
            Side::Sell => TransactionType::Sell,
        },
        order_type: OrderType::Limit,
        product_type: ProductType::Intraday,
        validity: OrderValidity::Day,
        quantity: x.qty,
        price: x.limit_price,
        trigger_price: 0.0,
        lot_size: x.lot_size,
        expiry_date: None,
    };
    let started = std::time::Instant::now();
    let order_id = match oms.place_order(request).await {
        Ok(id) => id,
        Err(err) => {
            // An exit a gate that rolls over time refused is kept and sent
            // again on the 1 s housekeeping tick (finding 7): the chain has
            // already counted it, so dropping it would leave the paper
            // position open with nothing to close it.
            if x.exit && place_refusal_rolls_over(&err) {
                return Placed::RetryExit(format!("place_refused:{err}"));
            }
            reject(&mut seq, format!("place_refused:{err}"));
            return Placed::Done;
        }
    };
    let rtt_us = i64::try_from(started.elapsed().as_micros()).unwrap_or(i64::MAX);
    if !order_id.starts_with("PAPER-") {
        error!(
            code = ErrorCode::OmsGapDryRunSafety.code_str(),
            source = "live_fill_non_paper_id",
            "OMS-GAP-06: the live paper fill chain's order came back with a NON-PAPER id — \
             no fill is applied"
        );
        reject(&mut seq, "refused:non_paper_id".to_owned());
        return Placed::Done;
    }
    emit(ctx, &x.base, LiveOrderEventKind::Sent, &mut seq, |r| {
        paper_rows::fill_sent(r, &order_id, Some(rtt_us));
    });
    emit(ctx, &x.base, LiveOrderEventKind::Ack, &mut seq, |r| {
        paper_rows::fill_ack(r, &order_id);
    });
    let segment_code = x.segment.binary_code();
    let mut filled = 0i64;
    if x.filled_qty > 0
        && let Some(order) = oms.order(&order_id).cloned()
        && let Some(update) =
            synthesize_paper_fill_qty(&order, x.avg_price, segment_code, x.filled_qty)
    {
        match oms.handle_order_update(&update) {
            Ok(Some(fill)) => {
                apply_fill(risk, book, &fill, ctx.leg_pnl_tx.as_ref());
                filled = update.traded_qty;
                emit(ctx, &x.base, LiveOrderEventKind::Fill, &mut seq, |r| {
                    paper_rows::fill_fill(r, &order_id, x, filled);
                });
            }
            Ok(None) | Err(_) => {
                reject(&mut seq, "fill_not_applied".to_owned());
            }
        }
    }
    if filled < x.qty {
        let remainder = x.qty.saturating_sub(filled);
        let why = paper_rows::remainder_reason(filled);
        // The OMS cancel runs the order book, budget, rate and circuit gates
        // before its dry-run arm, so several paper orders finishing together
        // can have the cancel refused. The remainder must never stay open (a
        // mark-driven paper filler would fill it at the next mark): on a
        // refusal, the dry-run order is closed through the OMS state machine
        // with a local CANCELLED update (no gate, no HTTP; paper only).
        let closed = match oms.cancel_order(&order_id).await {
            Ok(()) => Ok(why.to_owned()),
            Err(err) => close_paper_remainder(oms, &order_id, segment_code)
                .map(|()| format!("{why};oms_cancel_refused:{err}"))
                .map_err(|local| format!("cancel_refused:{err};local_close_failed:{local}")),
        };
        match closed {
            Ok(reason) => emit(ctx, &x.base, LiveOrderEventKind::Cancel, &mut seq, |r| {
                paper_rows::fill_cancel(r, &order_id, remainder, reason);
            }),
            Err(reason) => {
                error!(
                    code = ErrorCode::OmsGapStateMachine.code_str(),
                    source = "live_fill_remainder_not_closed",
                    order_id = %order_id,
                    "OMS-GAP-01: the live paper fill chain could not close a paper order's \
                     remainder; it stays open in the paper book (no broker order exists)"
                );
                reject(&mut seq, reason);
            }
        }
    }
    book.rebuild_pending_paper(oms);
    info!(
        order_id = %order_id,
        security_id = x.security_id,
        qty = x.qty,
        filled,
        "live paper fill: paper order placed, filled and closed (no broker call)"
    );
    Placed::Done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_fill::chain::PaperExecution;
    use secrecy::SecretString;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use tickvault_common::config::ApplicationConfig;
    use tickvault_common::trading_calendar::TradingCalendar;
    use tickvault_common::types::ExchangeSegment;
    use tickvault_core::notification::NotificationService;
    use tickvault_storage::live_order_events_persistence::{LiveOrderMode, LiveOrderSide};
    use tickvault_trading::oms::{OrderApiClient, OrderRateLimiter, TokenProvider};
    use tokio::sync::Notify;
    use tokio::sync::mpsc;

    struct NoToken;
    impl TokenProvider for NoToken {
        fn get_access_token(&self) -> Result<SecretString, OmsError> {
            Ok(SecretString::from("paper"))
        }
    }

    fn oms() -> OrderManagementSystem {
        oms_with_rate(10)
    }

    fn oms_with_rate(per_second: u32) -> OrderManagementSystem {
        OrderManagementSystem::new(
            OrderApiClient::new(
                reqwest::Client::new(),
                "https://api.dhan.co/v2".to_owned(),
                "100".to_owned(),
            ),
            OrderRateLimiter::new(per_second),
            Box::new(NoToken),
            "100".to_owned(),
        )
    }

    fn ctx(events: mpsc::Sender<LiveOrderEvent>) -> RuntimeCtx {
        use figment::Figment;
        use figment::providers::{Format, Toml};
        let path = if std::path::Path::new("config/base.toml").exists() {
            "config/base.toml"
        } else {
            "../../config/base.toml"
        };
        let config: Arc<ApplicationConfig> = Arc::new(
            Figment::new()
                .merge(Toml::file(path))
                .extract()
                .expect("config/base.toml must parse"), // APPROVED: test
        );
        let calendar = Arc::new(
            TradingCalendar::from_config(&config.trading).expect("calendar builds"), // APPROVED: test
        );
        RuntimeCtx {
            config,
            notifier: NotificationService::disabled(),
            calendar,
            marks_wanted: Arc::new(AtomicBool::new(false)),
            marks_dropped: Arc::new(AtomicU64::new(0)),
            token_handle: Arc::new(arc_swap::ArcSwap::from_pointee(None)),
            client_id: "100".to_string(),
            auth_notify: Arc::new(Notify::new()),
            leg_pnl_tx: None,
            live_fill_events: Some(events),
            live_fill_positions: None,
        }
    }

    /// LF-9 finding 4: a Mark changes no order and no position, so the
    /// runtime is told not to run its O(orders) republish scan after it; an
    /// Execute (booked or refused) is. The runtime's arm republishes only on
    /// `true`.
    #[tokio::test]
    async fn test_handle_live_fill_cmd_asks_for_the_republish_only_after_an_execute() {
        let (tx, _rx) = mpsc::channel(64);
        let ctx = ctx(tx);
        let mut oms = oms();
        let mut risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        let mut book = BookState::new();
        let mark = PaperCmd::Mark {
            security_id: 7,
            segment: ExchangeSegment::NseFno,
            price: 101.5,
        };
        assert!(!handle_live_fill_cmd(&mut oms, &mut risk, &mut book, &ctx, mark).await);
        let exec = PaperCmd::Execute(Box::new(execution(10, 10)));
        assert!(handle_live_fill_cmd(&mut oms, &mut risk, &mut book, &ctx, exec).await);
        // A Mark for a held position still marks it, and still asks for
        // nothing.
        let x = execution(10, 10);
        let mark = PaperCmd::Mark {
            security_id: x.security_id,
            segment: x.segment,
            price: 99.0,
        };
        assert!(!handle_live_fill_cmd(&mut oms, &mut risk, &mut book, &ctx, mark).await);
    }

    /// LF-9 review, finding 11: the gate itself, in the runtime's live-fill
    /// arm. The test above checks only the returned flag; an arm that ignored
    /// it and republished after every command (every 1 s Mark of every traded
    /// instrument paying the O(orders) scan again) passed every test. The
    /// arm's only republish must sit inside `if changes_orders {`.
    #[test]
    fn test_regression_live_fill_arm_republishes_only_when_orders_changed() {
        let src = include_str!("order_runtime.rs");
        let prod = src.split("\n#[cfg(test)]").next().unwrap_or(src);
        let start = prod
            .find("// ---- Arm 2b (LF-6b): live paper fill commands")
            .expect("the live-fill arm");
        let end = prod[start..].find("// ---- Arm 3:").expect("the next arm") + start;
        let arm = &prod[start..end];
        let call = "let changes_orders = live_fill_arm::handle_live_fill_cmd(";
        assert_eq!(arm.matches(call).count(), 1, "one command handler call");
        assert_eq!(
            arm.matches("republish_marks_wanted(").count(),
            1,
            "exactly one republish in the arm:\n{arm}"
        );
        let gate = arm.find("if changes_orders {").expect("the gate");
        let republish = arm.find("republish_marks_wanted(").expect("republish");
        let gate_end = gate + arm[gate..].find('}').expect("gate end");
        assert!(
            arm.find(call).expect("call") < gate && gate < republish && republish < gate_end,
            "the republish must sit inside `if changes_orders {{`:\n{arm}"
        );
    }

    fn execution(qty: i64, filled: i64) -> PaperExecution {
        PaperExecution {
            base: crate::live_fill::test_support::event(),
            next_seq: 2,
            security_id: 2885,
            segment: ExchangeSegment::NseEquity,
            side: Side::Buy,
            qty,
            lot_size: 1,
            limit_price: 101.0,
            filled_qty: filled,
            avg_price: 100.25,
            avg_price_paise: Some(10_025),
            exit: false,
        }
    }

    fn drain(
        rx: &mut mpsc::Receiver<LiveOrderEvent>,
    ) -> Vec<(LiveOrderEventKind, i32, Option<String>)> {
        let mut v = Vec::new();
        while let Ok(r) = rx.try_recv() {
            assert_eq!(r.mode, LiveOrderMode::Paper);
            assert_eq!(r.side, LiveOrderSide::Buy);
            v.push((r.event, r.seq, r.reason));
        }
        v
    }

    #[tokio::test]
    async fn test_handle_live_fill_cmd_full_fill_places_paper_and_books_the_position() {
        let (tx, mut rx) = mpsc::channel(64);
        let ctx = ctx(tx);
        let mut oms = oms();
        let mut risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        let mut book = BookState::new();
        assert!(oms.is_dry_run());
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Execute(Box::new(execution(10, 10))),
        )
        .await;
        let rows = drain(&mut rx);
        assert_eq!(
            rows.iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(),
            vec![
                (LiveOrderEventKind::Sent, 2),
                (LiveOrderEventKind::Ack, 3),
                (LiveOrderEventKind::Fill, 4)
            ]
        );
        assert_eq!(
            risk.net_lots_for_in_segment(2885, ExchangeSegment::NseEquity),
            10
        );
        // A mark for the open position is applied; one for a flat
        // instrument is ignored.
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Mark {
                security_id: 2885,
                segment: ExchangeSegment::NseEquity,
                price: 101.0,
            },
        )
        .await;
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Mark {
                security_id: 9,
                segment: ExchangeSegment::NseEquity,
                price: f64::NAN,
            },
        )
        .await;
    }

    #[tokio::test]
    async fn test_handle_live_fill_cmd_partial_fill_cancels_the_rest() {
        let (tx, mut rx) = mpsc::channel(64);
        let ctx = ctx(tx);
        let mut oms = oms();
        let mut risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        let mut book = BookState::new();
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Execute(Box::new(execution(10, 4))),
        )
        .await;
        let rows = drain(&mut rx);
        assert_eq!(
            rows,
            vec![
                (LiveOrderEventKind::Sent, 2, None),
                (LiveOrderEventKind::Ack, 3, None),
                (LiveOrderEventKind::Fill, 4, None),
                (
                    LiveOrderEventKind::Cancel,
                    5,
                    Some("remainder_after_window".to_owned())
                )
            ]
        );
        assert_eq!(
            risk.net_lots_for_in_segment(2885, ExchangeSegment::NseEquity),
            4
        );
        // Nothing filled: the whole order is cancelled.
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Execute(Box::new(execution(10, 0))),
        )
        .await;
        let rows = drain(&mut rx);
        assert_eq!(
            rows.last().cloned(),
            Some((
                LiveOrderEventKind::Cancel,
                4,
                Some("no_fill_in_window".to_owned())
            ))
        );
    }

    /// LF-6b review finding 6: an entry of 10 lots that filled only 4, then
    /// the strategy's full-size exit. The exit is cut to the 4 lots actually
    /// held, leaving the paper book flat, never short 6; an exit with nothing
    /// held is refused.
    #[tokio::test]
    async fn test_exit_is_reduce_only_against_the_booked_paper_position_clamp_exit() {
        let (tx, mut rx) = mpsc::channel(64);
        let ctx = ctx(tx);
        let mut oms = oms();
        let mut risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        let mut book = BookState::new();
        let seg = ExchangeSegment::NseEquity;
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Execute(Box::new(execution(10, 4))),
        )
        .await;
        assert_eq!(risk.net_lots_for_in_segment(2885, seg), 4);
        let mut exit = execution(10, 10);
        exit.side = Side::Sell;
        exit.exit = true;
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Execute(Box::new(exit.clone())),
        )
        .await;
        assert_eq!(
            risk.net_lots_for_in_segment(2885, seg),
            0,
            "the exit closes the 4 lots held, never opens a short"
        );
        // Flat now: a further exit is refused before anything is placed.
        let orders = oms.active_order_count();
        while rx.try_recv().is_ok() {}
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Execute(Box::new(exit.clone())),
        )
        .await;
        let last = rx.try_recv().expect("a reject row");
        assert_eq!(last.event, LiveOrderEventKind::Reject);
        assert_eq!(last.reason.as_deref(), Some("exit_without_position"));
        assert_eq!(risk.net_lots_for_in_segment(2885, seg), 0);
        assert_eq!(oms.active_order_count(), orders);
        // The pure clamp: a Buy exit covers a short only.
        let mut cover = execution(10, 10);
        cover.exit = true;
        assert!(clamp_exit(3, &cover).is_none(), "a buy exit on a long");
        let c = clamp_exit(-3, &cover).expect("covers the short");
        assert_eq!((c.qty, c.filled_qty, c.base.qty), (3, 3, 3));
        assert!(clamp_exit(0, &exit).is_none());
    }

    #[tokio::test]
    async fn test_handle_live_fill_cmd_risk_refusal_writes_a_reject_and_places_nothing() {
        let (tx, mut rx) = mpsc::channel(64);
        let ctx = ctx(tx);
        let mut oms = oms();
        // Max position 5 lots: a 10-lot order is refused.
        let mut risk = RiskEngine::new(2.0, 5, 1_000_000.0);
        let mut book = BookState::new();
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Execute(Box::new(execution(10, 10))),
        )
        .await;
        let rows = drain(&mut rx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, LiveOrderEventKind::Reject);
        assert!(rows[0].2.as_deref().is_some_and(|r| r.starts_with("risk:")));
        assert_eq!(oms.active_order_count(), 0);
    }

    /// LF-9 finding 2: every Execute the arm finishes, booked or refused,
    /// sends the chain one position report: the shares the chain counted and
    /// what the paper book now holds. A risk-refused entry reports 0 held; a
    /// booked one its lots. Mark commands report nothing.
    #[tokio::test]
    async fn test_handle_live_fill_cmd_reports_the_booked_position_to_the_chain() {
        let (tx, _rx) = mpsc::channel(64);
        let mut ctx = ctx(tx);
        let (pos_tx, mut pos_rx) = tickvault_core::websocket::frame_ring::frame_ring::<
            crate::live_fill::chain::PositionReport,
        >(8);
        pos_rx.set_busy_polling(true);
        ctx.live_fill_positions = Some(pos_tx);
        let mut oms = oms();
        // Refused by the risk engine (max position 5 lots).
        let mut risk = RiskEngine::new(2.0, 5, 1_000_000.0);
        let mut book = BookState::new();
        let x = execution(10, 10);
        let lot = i64::from(x.lot_size.max(1));
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Execute(Box::new(x)),
        )
        .await;
        let r = pos_rx.try_recv().expect("a report for the refused entry");
        assert_eq!(r.runtime_net_shares, 0, "nothing booked");
        assert_ne!(
            r.expected_shares, 0,
            "the chain's count comes back to be taken off"
        );
        // Booked.
        let mut risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Execute(Box::new(execution(10, 10))),
        )
        .await;
        let r = pos_rx.try_recv().expect("a report for the booked entry");
        let held = i64::from(risk.net_lots_for_in_segment(
            r.security_id,
            ExchangeSegment::from_byte(r.segment).expect("segment"),
        ));
        assert_eq!(r.runtime_net_shares, held * lot);
        assert_ne!(r.runtime_net_shares, 0);
        // A mark reports nothing.
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Mark {
                security_id: r.security_id,
                segment: ExchangeSegment::from_byte(r.segment).expect("segment"),
                price: 100.0,
            },
        )
        .await;
        assert!(pos_rx.try_recv().is_none());
    }

    /// Findings 8 and 10 (LF-6b review): a remainder cancel the OMS refuses
    /// (here its rate limiter, 1 a second, already spent by the place) still
    /// closes the paper order through a local CANCELLED update: nothing stays
    /// open for a mark-driven filler, and the Cancel row says why.
    #[tokio::test]
    async fn test_refused_remainder_cancel_still_closes_the_paper_order_close_paper_remainder() {
        let (tx, mut rx) = mpsc::channel(64);
        let ctx = ctx(tx);
        let mut oms = oms_with_rate(1);
        let mut risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        let mut book = BookState::new();
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Execute(Box::new(execution(10, 4))),
        )
        .await;
        let rows = drain(&mut rx);
        let cancel = rows.last().cloned().expect("rows");
        assert_eq!(cancel.0, LiveOrderEventKind::Cancel);
        assert!(
            cancel
                .2
                .as_deref()
                .is_some_and(|r| r.starts_with("remainder_after_window;oms_cancel_refused:")),
            "{cancel:?}"
        );
        assert_eq!(oms.active_order_count(), 0, "the remainder is closed");
        assert!(!book.has_pending_paper(2885, ExchangeSegment::NseEquity));
        let order = oms.all_orders().values().next().cloned().expect("tracked");
        assert_eq!(order.traded_qty, 4, "the booked fill is kept");
        assert!((order.avg_traded_price - 100.25).abs() < 1e-9);
        assert_eq!(
            risk.net_lots_for_in_segment(2885, ExchangeSegment::NseEquity),
            4
        );
        // Unknown and already-terminal orders.
        assert_eq!(
            close_paper_remainder(&mut oms, "PAPER-none", 1),
            Err("order_not_found".to_owned())
        );
        let id = order.order_id.clone();
        assert_eq!(close_paper_remainder(&mut oms, &id, 1), Ok(()));
    }

    /// Ten executions in one go on the default 10-a-second limiter: every
    /// order the OMS placed ends terminal, none keeps an open remainder.
    #[tokio::test]
    async fn test_ten_executions_in_one_tick_leave_no_open_paper_order() {
        let (tx, mut rx) = mpsc::channel(256);
        let ctx = ctx(tx);
        let mut oms = oms();
        let mut risk = RiskEngine::new(2.0, 1_000, 1_000_000.0);
        let mut book = BookState::new();
        for _ in 0..10 {
            handle_live_fill_cmd(
                &mut oms,
                &mut risk,
                &mut book,
                &ctx,
                PaperCmd::Execute(Box::new(execution(10, 4))),
            )
            .await;
        }
        let _rows = drain(&mut rx);
        assert!(oms.all_orders().len() >= 5);
        assert_eq!(oms.active_order_count(), 0, "no open paper remainder");
        assert!(!book.has_pending_paper(2885, ExchangeSegment::NseEquity));
    }

    /// LF-6b review round 2, findings 6 and 8: the paper position's own marks
    /// trip the daily-loss halt; the strategy's exit for that position still
    /// goes through (a halt stops new risk, never the order that removes it),
    /// and a NEW entry is still refused while halted.
    #[tokio::test]
    async fn test_regression_exit_closes_the_position_after_the_daily_loss_halt() {
        let (tx, mut rx) = mpsc::channel(64);
        let ctx = ctx(tx);
        let mut oms = oms();
        // 2% of 10,000 rupees: a 200-rupee loss halts.
        let mut risk = RiskEngine::new(2.0, 100, 10_000.0);
        let mut book = BookState::new();
        let seg = ExchangeSegment::NseEquity;
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Execute(Box::new(execution(10, 10))),
        )
        .await;
        assert_eq!(risk.net_lots_for_in_segment(2885, seg), 10);
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Mark {
                security_id: 2885,
                segment: seg,
                price: 50.0,
            },
        )
        .await;
        assert!(risk.is_halted(), "a 500-rupee paper loss halts the book");
        while rx.try_recv().is_ok() {}
        let mut exit = execution(10, 10);
        exit.side = Side::Sell;
        exit.exit = true;
        exit.avg_price = 50.0;
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Execute(Box::new(exit)),
        )
        .await;
        assert_eq!(
            risk.net_lots_for_in_segment(2885, seg),
            0,
            "the exit closes the losing position although the book is halted"
        );
        let rows = drain(&mut rx);
        assert!(
            rows.iter().all(|r| r.0 != LiveOrderEventKind::Reject),
            "{rows:?}"
        );
        // A new entry is still refused by the halt.
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Execute(Box::new(execution(10, 10))),
        )
        .await;
        let rows = drain(&mut rx);
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].2.as_deref().is_some_and(|r| r.starts_with("risk:")),
            "{rows:?}"
        );
        assert_eq!(risk.net_lots_for_in_segment(2885, seg), 0);
    }

    fn execution_for(security_id: u64, side: Side, exit: bool) -> PaperExecution {
        let mut x = execution(1, 1);
        x.security_id = security_id;
        x.side = side;
        x.exit = exit;
        x
    }

    /// LF-6b review round 2, finding 7: twenty paper exits finishing in the
    /// same second against the 10-a-second order limiter. The refused ones
    /// are kept (no row yet) and sent again on the housekeeping tick until
    /// every position is flat; none is lost.
    #[tokio::test]
    async fn test_regression_exits_refused_by_the_rate_limit_are_resent_retry_live_fill_exits() {
        let (tx, mut rx) = mpsc::channel(1_024);
        let ctx = ctx(tx);
        let mut oms = oms();
        let mut risk = RiskEngine::new(2.0, 1_000, 1_000_000.0);
        let mut book = BookState::new();
        let seg = ExchangeSegment::NseEquity;
        let sids: Vec<u64> = (5_000..5_020).collect();
        for batch in sids.chunks(10) {
            for &sid in batch {
                handle_live_fill_cmd(
                    &mut oms,
                    &mut risk,
                    &mut book,
                    &ctx,
                    PaperCmd::Execute(Box::new(execution_for(sid, Side::Buy, false))),
                )
                .await;
            }
            std::thread::sleep(std::time::Duration::from_millis(1_100));
        }
        for &sid in &sids {
            assert_eq!(risk.net_lots_for_in_segment(sid, seg), 1, "sid {sid} open");
        }
        while rx.try_recv().is_ok() {}
        for &sid in &sids {
            handle_live_fill_cmd(
                &mut oms,
                &mut risk,
                &mut book,
                &ctx,
                PaperCmd::Execute(Box::new(execution_for(sid, Side::Sell, true))),
            )
            .await;
        }
        assert!(
            !book.live_fill_exit_retry.is_empty(),
            "the burst outran the limiter, so some exits wait"
        );
        let mut rounds = 0;
        while !book.live_fill_exit_retry.is_empty() && rounds < 5 {
            std::thread::sleep(std::time::Duration::from_millis(1_100));
            retry_live_fill_exits(&mut oms, &mut risk, &mut book, &ctx).await;
            rounds += 1;
        }
        for &sid in &sids {
            assert_eq!(risk.net_lots_for_in_segment(sid, seg), 0, "sid {sid} flat");
        }
        let rows = drain_any(&mut rx);
        assert!(
            rows.iter().all(|r| r.0 != LiveOrderEventKind::Reject),
            "{rows:?}"
        );
        // An exit whose position closed meanwhile is refused when resent.
        let mut gone = ExitRetry::new(
            Box::new(execution_for(sids[0], Side::Sell, true)),
            "place_refused:x".to_owned(),
        );
        gone.attempts = 2;
        book.live_fill_exit_retry.push(gone);
        retry_live_fill_exits(&mut oms, &mut risk, &mut book, &ctx).await;
        assert!(book.live_fill_exit_retry.is_empty());
        let last = rx.try_recv().expect("a reject row");
        assert_eq!(last.reason.as_deref(), Some("exit_without_position"));
        // Spent attempts or a full queue: one Reject row, nothing kept.
        let mut spent = ExitRetry::new(
            Box::new(execution_for(1, Side::Sell, true)),
            "place_refused:rate".to_owned(),
        );
        spent.attempts = LIVE_FILL_EXIT_RETRY_MAX_ATTEMPTS;
        queue_exit_retry(&mut book, &risk, &ctx, spent);
        assert!(book.live_fill_exit_retry.is_empty());
        let last = rx.try_recv().expect("a reject row");
        assert_eq!(
            last.reason.as_deref(),
            Some("place_refused:rate;retries_exhausted")
        );
        assert!(place_refusal_rolls_over(&OmsError::RateLimited));
        assert!(!place_refusal_rolls_over(&OmsError::OrderNotFound {
            order_id: "x".to_owned()
        }));
        assert_eq!(
            Placed::RetryExit("a".to_owned()),
            Placed::RetryExit("a".to_owned())
        );
    }

    /// LF-9 review: an exit the runtime gives up (attempts spent) and an exit
    /// it books late from the retry queue each send the chain exactly one
    /// position report: the shares the chain counted for that exit and what
    /// the paper book holds now.
    #[tokio::test]
    async fn test_regression_given_up_and_late_booked_exits_report_their_position() {
        let (tx, _rx) = mpsc::channel(1_024);
        let mut ctx = ctx(tx);
        let (pos_tx, mut pos_rx) = tickvault_core::websocket::frame_ring::frame_ring::<
            crate::live_fill::chain::PositionReport,
        >(8);
        pos_rx.set_busy_polling(true);
        ctx.live_fill_positions = Some(pos_tx);
        let mut oms = oms();
        let mut risk = RiskEngine::new(2.0, 1_000, 1_000_000.0);
        let mut book = BookState::new();
        let sid = 6_100;
        handle_live_fill_cmd(
            &mut oms,
            &mut risk,
            &mut book,
            &ctx,
            PaperCmd::Execute(Box::new(execution_for(sid, Side::Buy, false))),
        )
        .await;
        let entry = pos_rx.try_recv().expect("the entry's report");
        assert_eq!(entry.runtime_net_shares, 1);
        // Given up: attempts spent. The chain counted -1; the book still
        // holds the long.
        let mut spent = ExitRetry::new(
            Box::new(execution_for(sid, Side::Sell, true)),
            "place_refused:rate".to_owned(),
        );
        spent.attempts = LIVE_FILL_EXIT_RETRY_MAX_ATTEMPTS;
        queue_exit_retry(&mut book, &risk, &ctx, spent);
        let r = pos_rx.try_recv().expect("one report for the given-up exit");
        assert_eq!((r.security_id, r.expected_shares), (sid, -1));
        assert_eq!(r.runtime_net_shares, 1, "still held");
        assert!(pos_rx.try_recv().is_none(), "exactly one");
        // Booked late from the retry queue: one report, flat.
        book.live_fill_exit_retry.push(ExitRetry::new(
            Box::new(execution_for(sid, Side::Sell, true)),
            "place_refused:rate".to_owned(),
        ));
        retry_live_fill_exits(&mut oms, &mut risk, &mut book, &ctx).await;
        assert!(book.live_fill_exit_retry.is_empty());
        let r = pos_rx.try_recv().expect("one report for the booked exit");
        assert_eq!((r.security_id, r.expected_shares), (sid, -1));
        assert_eq!(r.runtime_net_shares, 0, "flat");
        assert!(pos_rx.try_recv().is_none(), "exactly one");
    }

    /// Counts the OMS's rate-limit and budget alerts. TEST DOUBLE.
    #[derive(Clone, Default)]
    struct CountingSink(Arc<AtomicU64>);
    impl tickvault_trading::oms::engine::OmsAlertSink for CountingSink {
        fn fire(&self, alert: tickvault_trading::oms::engine::OmsAlert) {
            if matches!(
                alert,
                tickvault_trading::oms::engine::OmsAlert::RateLimitExhausted { .. }
            ) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// LF-6b review round 3, findings 3 and 4: thirty paper exits finishing
    /// together against the 10-a-second limiter. An exit arriving while
    /// others wait joins the back of the queue with no place attempt, and a
    /// retry round stops at its first refusal, so the refused attempts (each
    /// one a High alert and an order-budget unit) are one per round, not one
    /// per waiting exit; every position still ends flat and in order.
    #[tokio::test]
    async fn test_regression_exit_burst_makes_one_refused_attempt_per_round() {
        let (tx, mut rx) = mpsc::channel(4_096);
        let ctx = ctx(tx);
        let mut oms = oms();
        let alerts = CountingSink::default();
        let mut risk = RiskEngine::new(2.0, 1_000, 1_000_000.0);
        let mut book = BookState::new();
        let seg = ExchangeSegment::NseEquity;
        let sids: Vec<u64> = (7_000..7_030).collect();
        for batch in sids.chunks(10) {
            for &sid in batch {
                handle_live_fill_cmd(
                    &mut oms,
                    &mut risk,
                    &mut book,
                    &ctx,
                    PaperCmd::Execute(Box::new(execution_for(sid, Side::Buy, false))),
                )
                .await;
            }
            std::thread::sleep(std::time::Duration::from_millis(1_100));
        }
        while rx.try_recv().is_ok() {}
        oms.set_alert_sink(Box::new(alerts.clone()));
        for &sid in &sids {
            let mut x = execution_for(sid, Side::Sell, true);
            x.base.security_id = i64::try_from(sid).expect("fits");
            handle_live_fill_cmd(
                &mut oms,
                &mut risk,
                &mut book,
                &ctx,
                PaperCmd::Execute(Box::new(x)),
            )
            .await;
        }
        assert_eq!(
            alerts.0.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "one refused attempt for the burst, the rest wait untouched"
        );
        assert_eq!(book.live_fill_exit_retry.len(), 20);
        assert_eq!(book.live_fill_exit_retry[1].attempts, 0);
        let mut rounds = 0u64;
        while !book.live_fill_exit_retry.is_empty() && rounds < 6 {
            std::thread::sleep(std::time::Duration::from_millis(1_100));
            retry_live_fill_exits(&mut oms, &mut risk, &mut book, &ctx).await;
            rounds += 1;
        }
        for &sid in &sids {
            assert_eq!(risk.net_lots_for_in_segment(sid, seg), 0, "sid {sid} flat");
        }
        assert!(
            alerts.0.load(std::sync::atomic::Ordering::Relaxed) <= 1 + rounds,
            "at most one refused attempt per round: {} alerts in {rounds} rounds",
            alerts.0.load(std::sync::atomic::Ordering::Relaxed)
        );
        // Exits close in the order they finished.
        let mut order = Vec::new();
        while let Ok(r) = rx.try_recv() {
            if r.event == LiveOrderEventKind::Fill {
                order.push(r.security_id);
            }
            assert_ne!(r.event, LiveOrderEventKind::Reject, "{r:?}");
        }
        let want: Vec<i64> = sids
            .iter()
            .map(|s| i64::try_from(*s).expect("fits"))
            .collect();
        assert_eq!(order, want);
    }

    fn drain_any(
        rx: &mut mpsc::Receiver<LiveOrderEvent>,
    ) -> Vec<(LiveOrderEventKind, i32, Option<String>)> {
        let mut v = Vec::new();
        while let Ok(r) = rx.try_recv() {
            v.push((r.event, r.seq, r.reason));
        }
        v
    }

    #[test]
    fn test_synthesize_paper_fill_qty_partial_and_full() {
        let mut oms = oms();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt");
        let id = rt
            .block_on(oms.place_order(PlaceOrderRequest {
                security_id: 1,
                exchange_segment: ExchangeSegment::NseEquity,
                transaction_type: TransactionType::Buy,
                order_type: OrderType::Limit,
                product_type: ProductType::Intraday,
                validity: OrderValidity::Day,
                quantity: 10,
                price: 100.0,
                trigger_price: 0.0,
                lot_size: 1,
                expiry_date: None,
            }))
            .expect("paper place");
        let order = oms.order(&id).cloned().expect("tracked");
        let part = synthesize_paper_fill_qty(&order, 100.0, 1, 4).expect("part");
        assert_eq!(part.status, "PART_TRADED");
        assert_eq!(part.traded_qty, 4);
        assert_eq!(part.remaining_quantity, 6);
        let full = synthesize_paper_fill_qty(&order, 100.0, 1, 99).expect("full");
        assert_eq!(full.status, "TRADED");
        assert_eq!(full.traded_qty, 10);
        assert!(synthesize_paper_fill_qty(&order, 100.0, 1, 0).is_none());
        assert!(synthesize_paper_fill_qty(&order, 0.0, 1, 4).is_none());
    }

    #[tokio::test]
    async fn test_recv_live_fill_reads_a_command() {
        let (tx, rx) = tickvault_core::websocket::frame_ring::frame_ring::<PaperCmd>(1);
        let m = tokio::sync::Mutex::new(rx);
        let mut g = Some(m.lock().await);
        tx.try_send(PaperCmd::Mark {
            security_id: 1,
            segment: ExchangeSegment::NseEquity,
            price: 1.0,
        })
        .expect("room");
        assert!(matches!(
            recv_live_fill(&mut g).await,
            Some(PaperCmd::Mark { .. })
        ));
        drop(tx);
        assert!(recv_live_fill(&mut g).await.is_none());
    }
}
