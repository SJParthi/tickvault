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
use tickvault_storage::live_order_events_persistence::{LiveOrderEvent, LiveOrderEventKind};
use tickvault_trading::brutex_host::engine::Side;
use tickvault_trading::oms::{ManagedOrder, OrderManagementSystem, PlaceOrderRequest};
use tickvault_trading::risk::engine::RiskEngine;
use tickvault_trading::risk::types::RiskCheck;
use tokio::sync::{MutexGuard, mpsc};
use tracing::{error, info};

use super::{BookState, RuntimeCtx, apply_fill, synthesize_paper_fill};
use crate::live_fill::chain::{PaperCmd, PaperExecution};
use tickvault_common::error_code::ErrorCode;

/// Receives one command while the arm is armed; pends forever without a
/// receiver (the `select!` arm is also disabled then).
pub(super) async fn recv_live_fill(
    rx: &mut Option<MutexGuard<'_, mpsc::Receiver<PaperCmd>>>,
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
    let mut row = base.clone();
    row.ts_ist_nanos = crate::live_fill::wall_ist_ns();
    row.event = kind;
    row.seq = *seq;
    *seq = seq.saturating_add(1);
    fill(&mut row);
    if tx.try_send(row).is_err() {
        metrics::counter!(crate::live_fill::EVENTS_DROPPED_COUNTER).increment(1);
    }
}

/// One command from the chain (see the module docs).
pub(super) async fn handle_live_fill_cmd(
    oms: &mut OrderManagementSystem,
    risk: &mut RiskEngine,
    book: &mut BookState,
    ctx: &RuntimeCtx,
    cmd: PaperCmd,
) {
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
        }
        PaperCmd::Execute(x) => execute(oms, risk, book, ctx, &x).await,
    }
}

async fn execute(
    oms: &mut OrderManagementSystem,
    risk: &mut RiskEngine,
    book: &mut BookState,
    ctx: &RuntimeCtx,
    x: &PaperExecution,
) {
    let mut seq = x.next_seq;
    let reject = |seq: &mut i32, reason: String| {
        emit(ctx, &x.base, LiveOrderEventKind::Reject, seq, |r| {
            r.remainder_qty = Some(x.qty.max(0));
            r.reason = Some(reason);
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
        return;
    }
    let lot = i64::from(x.lot_size.max(1));
    let lots = i32::try_from(x.qty / lot).unwrap_or(i32::MAX);
    let signed = match x.side {
        Side::Buy => lots,
        Side::Sell => lots.saturating_neg(),
    };
    if let RiskCheck::Rejected { breach, .. } =
        risk.check_order_in_segment(x.security_id, x.segment, signed)
    {
        reject(&mut seq, format!("risk:{breach:?}"));
        return;
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
            reject(&mut seq, format!("place_refused:{err}"));
            return;
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
        return;
    }
    emit(ctx, &x.base, LiveOrderEventKind::Sent, &mut seq, |r| {
        r.order_id = Some(order_id.clone());
        r.api_rtt_us = Some(rtt_us);
    });
    emit(ctx, &x.base, LiveOrderEventKind::Ack, &mut seq, |r| {
        r.order_id = Some(order_id.clone());
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
                    r.order_id = Some(order_id.clone());
                    r.fill_qty = Some(filled);
                    r.fill_price_paise = x.avg_price_paise;
                    r.avg_price_paise = x.avg_price_paise;
                    r.remainder_qty = Some(x.qty.saturating_sub(filled));
                    // The paper fill IS the worst-case model's price.
                    r.worse_than_model = Some(false);
                });
            }
            Ok(None) | Err(_) => {
                reject(&mut seq, "fill_not_applied".to_owned());
            }
        }
    }
    if filled < x.qty {
        let remainder = x.qty.saturating_sub(filled);
        let why = if filled == 0 {
            "no_fill_in_window"
        } else {
            "remainder_after_window"
        };
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
                r.order_id = Some(order_id.clone());
                r.remainder_qty = Some(remainder);
                r.reason = Some(reason);
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
    use tickvault_trading::oms::{OmsError, OrderApiClient, OrderRateLimiter, TokenProvider};
    use tokio::sync::Notify;

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
        }
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
        let (tx, rx) = mpsc::channel::<PaperCmd>(1);
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
