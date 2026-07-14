//! 🔷 DHAN exit-order execution dispatcher (Cluster B, 2026-07-14).
//!
//! LOCK #2's runtime gate + the S6-G1 call-site hub for EVERY engine exit
//! method (`place_super_order`, `modify_super_order_leg`,
//! `cancel_super_order_leg`, `place_forever_oco`, `place_order_sliced`,
//! `verify_order_execution`). Cluster A constructs [`ExitCommand`]s; ONLY
//! this dispatcher executes them — never the engine methods directly (the
//! frozen seam contract, design §3.5/§4).
//!
//! Gate order (design Ruling 8 / Lock 2a): the `!cfg.enabled` check fires
//! FIRST — before ANY engine touch — so a disabled layer drops every
//! command with a counter (`tv_exit_commands_dropped_total`), never a
//! silent no-op and never a state mutation.
//!
//! The MPP verify retry LADDER lives HERE (tokio sleeps in the app layer,
//! never inside the `&mut`-serial engine — design Ruling 5): pure rungs
//! from `exit_rules::next_verify_backoff_secs` (1, 2, 4, 8, 10s), the
//! engine's `verify_order_execution` is a single probe per rung.
//!
//! Cold path — orders ~1-100/day; allocations are fine. Every failure is
//! coded (`EXIT-ORDER-01`, log-sink-only per the 2026-07-14 Dhan noise
//! lock: zero Telegram emit sites in this module).

use crate::order_observability::{OrderSideDayStats, OrderSideMsg, try_send_order_side};
use tickvault_common::config::ExitOrdersConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::order_types::{OrderType, OrderValidity, ProductType, TransactionType};
use tickvault_common::segment::segment_code_to_str;
use tickvault_trading::oms::exit_rules::{self, ExitCommand};
use tickvault_trading::oms::types::OrderLeg;
use tickvault_trading::oms::{
    ExecutionVerdict, OmsError, OrderManagementSystem, PlaceOrderRequest,
};
use tickvault_trading::risk::engine::RiskEngine;
use tracing::{error, info, warn};

/// Cluster-C order-side observation handle (#1554) — mirror of the
/// `trading_pipeline` alias; `None` in tests. The DISABLED legacy exit
/// body emits the same `order_audit` rows through it that the pre-Cluster-B
/// inline `Signal::Exit` arm did (audit-row-only, no Telegram).
type OrderSideObserver = Option<(
    tokio::sync::mpsc::Sender<OrderSideMsg>,
    std::sync::Arc<OrderSideDayStats>,
)>;

/// Counted, non-blocking order-side capture — no-op without wiring
/// (mirror of `trading_pipeline::order_side_send`).
fn observe_order_side(handle: &OrderSideObserver, msg: OrderSideMsg) {
    if let Some((tx, stats)) = handle {
        try_send_order_side(tx, stats, msg);
    }
}

/// Stable per-variant label for logs + tests (no Debug-format allocation
/// on the gate path).
fn command_label(cmd: &ExitCommand) -> &'static str {
    match cmd {
        ExitCommand::CloseAll { .. } => "close_all",
        ExitCommand::PlaceBracket(_) => "place_bracket",
        ExitCommand::TightenStop { .. } => "tighten_stop",
        ExitCommand::CancelBracket { .. } => "cancel_bracket",
        ExitCommand::PlaceOcoProtection(_) => "place_oco_protection",
        ExitCommand::VerifyExecution { .. } => "verify_execution",
    }
}

/// LOCK #2 runtime gate + the S6-G1 call-site hub for every new engine
/// exit pub fn (design §3.5, verbatim contract).
///
/// `!cfg.enabled` ⇒ `warn!` + `tv_exit_commands_dropped_total{reason="disabled"}`
/// + `Ok(())` — NO OMS touch. Engine-call failures ⇒
/// `error!(code = ErrorCode::ExitOrder01ExecutionDegraded...)` + the error
/// propagates to the caller.
///
/// # Errors
/// Propagates the engine's typed [`OmsError`] for the dispatched command
/// (validation refusal, gate denial, Dhan API error, unknown order id).
/// A DISABLED layer never errors — the drop is deliberate and counted.
pub async fn dispatch_exit_command(
    oms: &mut OrderManagementSystem,
    risk_engine: &RiskEngine,
    cmd: ExitCommand,
    cfg: &ExitOrdersConfig,
) -> Result<(), OmsError> {
    // LOCK #2 (design Lock 2a): config gate FIRST — before any engine
    // touch. Ratchet-pinned literal: `!cfg.enabled` + the dropped counter.
    if !cfg.enabled {
        warn!(
            command = command_label(&cmd),
            "exit-order layer disabled — exit command dropped ([exit_orders] enabled = false)"
        );
        metrics::counter!("tv_exit_commands_dropped_total", "reason" => "disabled").increment(1);
        return Ok(());
    }

    let label = command_label(&cmd);
    let result: Result<(), OmsError> = match cmd {
        ExitCommand::CloseAll {
            security_id,
            freeze_limit,
        } => close_all_for_security(oms, risk_engine, security_id, freeze_limit, cfg).await,
        ExitCommand::PlaceBracket(request) => {
            let security_id = request.security_id;
            oms.place_super_order(request, cfg.default_freeze_limit_qty)
                .await
                .map(|placement| {
                    info!(
                        entry_order_id = %placement.entry_order_id,
                        security_id,
                        legs = placement.legs.len(),
                        "exit dispatcher → 3-leg bracket placed"
                    );
                })
        }
        ExitCommand::TightenStop { order_id, modify } => {
            oms.modify_super_order_leg(&order_id, modify).await
        }
        ExitCommand::CancelBracket { order_id } => {
            // ENTRY_LEG cancels the whole bracket — the engine REFUSES it
            // post-fill (naked-position race U4, exit_rules gate).
            oms.cancel_super_order_leg(&order_id, OrderLeg::EntryLeg)
                .await
        }
        ExitCommand::PlaceOcoProtection(request) => {
            let security_id = request.security_id;
            oms.place_forever_oco(request).await.map(|order_id| {
                info!(
                    order_id = %order_id,
                    security_id,
                    "exit dispatcher → forever/OCO protection placed"
                );
            })
        }
        ExitCommand::VerifyExecution { order_id } => {
            run_verify_ladder(oms, &order_id, cfg).await.map(|verdict| {
                info!(
                    order_id = %order_id,
                    verdict = ?verdict,
                    "exit dispatcher → verify ladder finished"
                );
            })
        }
    };

    if let Err(err) = &result {
        error!(
            code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
            command = label,
            error = %err,
            "EXIT-ORDER-01: exit command failed"
        );
    }
    result
}

/// The `Signal::Exit` arm delegate (design §3.5).
///
/// `enabled == false` ⇒ the byte-equivalent LEGACY body (cancel actives +
/// plain MARKET close via `oms.place_order` — the exact pre-Cluster-B
/// `trading_pipeline.rs` behavior, same log lines AND the cluster-C
/// order-side observability (#1554): every exit cancel/close still lands
/// in `order_audit` via `order_side` — audit-row-only, no Telegram).
/// `enabled == true` ⇒ [`ExitCommand::CloseAll`] through
/// [`dispatch_exit_command`] (cancel actives super-order-aware +
/// `place_order_sliced` + the verify ladder).
pub async fn execute_exit_for_security(
    oms: &mut OrderManagementSystem,
    risk_engine: &RiskEngine,
    security_id: u64,
    cfg: &ExitOrdersConfig,
    order_side: &OrderSideObserver,
    exchange_segment_code: u8,
) {
    if cfg.enabled {
        let cmd = ExitCommand::CloseAll {
            security_id,
            freeze_limit: cfg.default_freeze_limit_qty,
        };
        if let Err(err) = dispatch_exit_command(oms, risk_engine, cmd, cfg).await {
            // Already EXIT-ORDER-01-coded inside the dispatcher; this is
            // the pipeline-facing summary line (legacy warn semantics).
            warn!(
                ?err,
                security_id, "EXIT signal → exit-command dispatch failed"
            );
        }
        return;
    }

    // ---- LEGACY body (disabled path) — byte-equivalent to the
    // pre-Cluster-B Signal::Exit arm, including the cluster-C order-side
    // observability (#1554). ----
    // Step 1: Cancel active (unfilled/pending) orders for this security
    let active: Vec<String> = oms
        .active_orders()
        .iter()
        .filter(|o| o.security_id == security_id)
        .map(|o| o.order_id.clone())
        .collect();
    for order_id in active {
        match oms.cancel_order(&order_id).await {
            Ok(()) => {
                observe_order_side(order_side, OrderSideMsg::Cancelled { order_id });
            }
            Err(err) => {
                warn!(
                    ?err,
                    order_id = %order_id,
                    "EXIT signal → cancel failed"
                );
                observe_order_side(
                    order_side,
                    OrderSideMsg::CancelFailed {
                        order_id,
                        detail: format!("exit cancel: {err}"),
                    },
                );
            }
        }
    }
    // Step 2: Close open position (if any filled lots exist)
    let net_lots = risk_engine.net_lots_for(security_id);
    if net_lots != 0 {
        let close_type = if net_lots > 0 {
            TransactionType::Sell
        } else {
            TransactionType::Buy
        };
        let close_qty = i64::from(net_lots.unsigned_abs());
        info!(
            security_id,
            net_lots,
            close_type = ?close_type,
            "EXIT signal → placing closing order for open position"
        );
        let close_request = PlaceOrderRequest {
            security_id,
            transaction_type: close_type,
            order_type: OrderType::Market,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: close_qty,
            price: 0.0,
            trigger_price: 0.0,
            lot_size: 1,
            expiry_date: None,
        };
        match oms.place_order(close_request).await {
            Ok(order_id) => {
                info!(
                    order_id = %order_id,
                    security_id,
                    net_lots,
                    "EXIT signal → closing order placed"
                );
                observe_order_side(
                    order_side,
                    OrderSideMsg::Placed {
                        order_id,
                        correlation_id: String::new(),
                        security_id,
                        exchange_segment: segment_code_to_str(exchange_segment_code),
                        transaction_type: if net_lots > 0 { "SELL" } else { "BUY" },
                        quantity: close_qty,
                        price: 0.0,
                    },
                );
            }
            Err(err) => {
                warn!(
                    ?err,
                    security_id, "EXIT signal → closing order placement failed"
                );
                observe_order_side(
                    order_side,
                    OrderSideMsg::PlaceFailed {
                        correlation_id: String::new(),
                        security_id,
                        detail: format!("exit close: {err}"),
                    },
                );
            }
        }
    }
}

/// `CloseAll` execution (enabled path): cancel actives SUPER-ORDER-AWARE
/// (a tracked bracket cancels via its ENTRY_LEG — post-fill refusal
/// intact), then close the net position through `place_order_sliced`
/// (freeze-limit escape hatch), then run the MPP verify ladder for every
/// placed close-order id.
///
/// Cancel failures and verify-ladder failures are coded-logged
/// (`EXIT-ORDER-01`) but do NOT abort the close — the position flatten is
/// the protective op. A close-order PLACEMENT failure IS the error.
async fn close_all_for_security(
    oms: &mut OrderManagementSystem,
    risk_engine: &RiskEngine,
    security_id: u64,
    freeze_limit: i64,
    cfg: &ExitOrdersConfig,
) -> Result<(), OmsError> {
    // Step 1: cancel actives for this security — super-order-aware.
    let active: Vec<String> = oms
        .active_orders()
        .iter()
        .filter(|o| o.security_id == security_id)
        .map(|o| o.order_id.clone())
        .collect();
    for order_id in &active {
        let cancel_result = if oms.super_order(order_id).is_some() {
            oms.cancel_super_order_leg(order_id, OrderLeg::EntryLeg)
                .await
        } else {
            oms.cancel_order(order_id).await
        };
        if let Err(err) = cancel_result {
            // Non-fatal: keep flattening — the refusal case (post-fill
            // ENTRY_LEG) means the exchange-resident exits stay live,
            // which is the SAFE outcome.
            error!(
                code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                order_id = %order_id,
                security_id,
                error = %err,
                "EXIT-ORDER-01: CloseAll → cancel failed"
            );
        }
    }

    // Step 2: close the net position (sliced when it exceeds the freeze).
    let net_lots = risk_engine.net_lots_for(security_id);
    if net_lots == 0 {
        return Ok(());
    }
    let close_type = if net_lots > 0 {
        TransactionType::Sell
    } else {
        TransactionType::Buy
    };
    let close_qty = i64::from(net_lots.unsigned_abs());
    info!(
        security_id,
        net_lots,
        close_type = ?close_type,
        freeze_limit,
        "CloseAll → placing closing order(s) for open position"
    );
    let close_request = PlaceOrderRequest {
        security_id,
        transaction_type: close_type,
        order_type: OrderType::Market,
        product_type: ProductType::Intraday,
        validity: OrderValidity::Day,
        quantity: close_qty,
        price: 0.0,
        trigger_price: 0.0,
        lot_size: 1,
        expiry_date: None,
    };
    let order_ids = oms.place_order_sliced(close_request, freeze_limit).await?;
    info!(
        security_id,
        orders = order_ids.len(),
        "CloseAll → closing order(s) placed"
    );

    // Step 3: MPP verify-after-place ladder per close-order id
    // (orders.md rule 18 — never assume a MARKET order filled).
    for order_id in &order_ids {
        if let Err(err) = run_verify_ladder(oms, order_id, cfg).await {
            error!(
                code = ErrorCode::ExitOrder01ExecutionDegraded.code_str(),
                order_id = %order_id,
                security_id,
                error = %err,
                "EXIT-ORDER-01: CloseAll → verify ladder failed"
            );
        }
    }
    Ok(())
}

/// Drives the MPP verify ladder for ONE order id: sleep the pure rung
/// delay (`exit_rules::next_verify_backoff_secs` — 1, 2, 4, 8, 10s), then
/// one engine probe per rung.
///
/// - A DECISIVE verdict (`Filled` / `SimulatedFilled` / `Terminal` /
///   `PendingAtLimit` / `Unknown`) stops the ladder (the engine already
///   emitted `EXIT-VERIFY-01` for the degraded terminals).
/// - `Pending` / `PartiallyFilled` keep polling until the rungs run out.
/// - A transport/HTTP/429 probe failure is INCONCLUSIVE — retried on the
///   next rung, NEVER treated as order-absent (design §5). Only
///   `OrderNotFound` (structural — we never tracked the id) aborts.
///
/// # Errors
/// `OrderNotFound` immediately; otherwise the LAST probe error when every
/// rung failed without a single verdict.
async fn run_verify_ladder(
    oms: &mut OrderManagementSystem,
    order_id: &str,
    cfg: &ExitOrdersConfig,
) -> Result<ExecutionVerdict, OmsError> {
    let mut elapsed_secs: u64 = 0;
    let mut last_verdict: Option<ExecutionVerdict> = None;
    let mut last_probe_error: Option<OmsError> = None;

    for attempt in 1u32.. {
        let Some(delay_secs) =
            exit_rules::next_verify_backoff_secs(attempt, cfg.mpp_verify_max_attempts)
        else {
            break; // ladder exhausted
        };
        tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
        elapsed_secs = elapsed_secs.saturating_add(delay_secs);

        match oms
            .verify_order_execution(order_id, elapsed_secs, cfg.mpp_verify_deadline_secs)
            .await
        {
            Ok(verdict) => {
                let decisive = matches!(
                    verdict,
                    ExecutionVerdict::Filled { .. }
                        | ExecutionVerdict::SimulatedFilled
                        | ExecutionVerdict::Terminal { .. }
                        | ExecutionVerdict::PendingAtLimit { .. }
                        | ExecutionVerdict::Unknown { .. }
                );
                last_probe_error = None;
                last_verdict = Some(verdict);
                if decisive {
                    break;
                }
            }
            // Structural: we never tracked this id — no rung can fix it.
            Err(err @ OmsError::OrderNotFound { .. }) => return Err(err),
            // Transport / HTTP / 429 / gate denial: inconclusive — retry
            // the next rung; a failed probe NEVER marks an order absent.
            Err(err) => {
                warn!(
                    order_id = %order_id,
                    attempt,
                    error = %err,
                    "verify probe inconclusive — retrying next rung"
                );
                last_probe_error = Some(err);
            }
        }
    }

    match (last_verdict, last_probe_error) {
        (Some(verdict), _) => Ok(verdict),
        (None, Some(err)) => Err(err),
        // Unreachable with validate()'s attempts >= 1 bound; fail-closed
        // Unknown rather than a panic-class arm (never assumed filled).
        (None, None) => Ok(ExecutionVerdict::Unknown {
            raw_status: "VERIFY_LADDER_EMPTY".to_owned(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretString;
    use tickvault_trading::oms::{
        ModifySuperOrderLeg, OcoSecondLeg, OrderApiClient, OrderRateLimiter,
        PlaceForeverOcoRequest, PlaceSuperOrderRequest, TokenProvider,
    };

    struct TestToken;
    impl TokenProvider for TestToken {
        fn get_access_token(&self) -> Result<SecretString, OmsError> {
            Ok(SecretString::from("test-jwt".to_owned()))
        }
    }

    /// Dry-run OMS (the constructor default — LOCK #3): no HTTP call can
    /// ever fire from these tests.
    fn make_dry_run_oms() -> OrderManagementSystem {
        let api_client = OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "test_client".to_owned(),
        );
        let rate_limiter = OrderRateLimiter::new(10);
        OrderManagementSystem::new(
            api_client,
            rate_limiter,
            Box::new(TestToken),
            "test_client".to_owned(),
        )
    }

    fn enabled_cfg(freeze_limit: i64) -> ExitOrdersConfig {
        let mut cfg = ExitOrdersConfig::default();
        cfg.enabled = true;
        cfg.default_freeze_limit_qty = freeze_limit;
        cfg.freeze_limits_reviewed_on = "2026-07-14".to_string();
        cfg
    }

    fn bracket_request(security_id: u64) -> PlaceSuperOrderRequest {
        PlaceSuperOrderRequest {
            security_id,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            quantity: 75,
            price: 100.0,
            target_price: 110.0,
            stop_loss_price: 95.0,
            trailing_jump: 0.0,
            lot_size: 75,
            expiry_date: None,
        }
    }

    fn oco_request() -> PlaceForeverOcoRequest {
        PlaceForeverOcoRequest {
            security_id: 1333,
            transaction_type: TransactionType::Sell,
            product_type: ProductType::Cnc,
            order_type: OrderType::Limit,
            validity: OrderValidity::Day,
            quantity: 5,
            price: 1428.0,
            trigger_price: 1427.0,
            oco_leg: Some(OcoSecondLeg {
                price: 1420.0,
                trigger_price: 1419.0,
                quantity: 5,
            }),
            expiry_date: None,
        }
    }

    /// LOCK #2 behavioral pin: a disabled layer drops EVERY command
    /// variant without touching the OMS — `total_placed` stays 0 and no
    /// order state appears.
    #[tokio::test]
    async fn test_dispatch_gate_off_drops_every_variant_without_touching_oms() {
        let mut oms = make_dry_run_oms();
        let risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        let cfg = ExitOrdersConfig::default();
        assert!(!cfg.enabled, "default must be OFF (LOCK #1)");

        let commands = vec![
            ExitCommand::CloseAll {
                security_id: 13,
                freeze_limit: 1800,
            },
            ExitCommand::PlaceBracket(bracket_request(49081)),
            ExitCommand::TightenStop {
                order_id: "PAPER-SO-1".to_string(),
                modify: ModifySuperOrderLeg::StopLoss {
                    stop_loss_price: 96.0,
                    trailing_jump: 0.0,
                },
            },
            ExitCommand::CancelBracket {
                order_id: "PAPER-SO-1".to_string(),
            },
            ExitCommand::PlaceOcoProtection(oco_request()),
            ExitCommand::VerifyExecution {
                order_id: "PAPER-1".to_string(),
            },
        ];
        for cmd in commands {
            let result = dispatch_exit_command(&mut oms, &risk, cmd, &cfg).await;
            assert!(result.is_ok(), "disabled gate must drop with Ok, not Err");
        }
        assert_eq!(
            oms.total_placed(),
            0,
            "disabled dispatcher must NEVER touch the OMS"
        );
        assert!(oms.active_orders().is_empty());
    }

    /// CloseAll (enabled, dry-run): cancels the pre-existing active order,
    /// slices the 5-lot close at freeze 2 into 3 paper orders, and runs
    /// the verify ladder (SimulatedFilled stops it after one probe).
    #[tokio::test]
    async fn test_dispatch_close_all_cancels_slices_and_verifies() {
        let mut oms = make_dry_run_oms();
        let mut risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        risk.record_fill(13, 5, 100.0, 1); // net +5 lots to flatten
        let cfg = enabled_cfg(2);

        // Pre-existing active paper order for the same security.
        let pending = PlaceOrderRequest {
            security_id: 13,
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
        let pending_id = oms.place_order(pending).await.expect("paper place");

        let cmd = ExitCommand::CloseAll {
            security_id: 13,
            freeze_limit: cfg.default_freeze_limit_qty,
        };
        dispatch_exit_command(&mut oms, &risk, cmd, &cfg)
            .await
            .expect("CloseAll must succeed in dry-run");

        // 1 pre-existing + 3 slices (2 + 2 + 1) placed.
        assert_eq!(oms.total_placed(), 4, "5 lots at freeze 2 = 3 slices");
        // The pre-existing active order was cancelled.
        assert!(
            !oms.active_orders().iter().any(|o| o.order_id == pending_id),
            "CloseAll must cancel the pre-existing active order"
        );
    }

    /// Per-variant dispatch smoke: PlaceBracket → TightenStop →
    /// CancelBracket through the dispatcher (the S6-G1 call sites).
    #[tokio::test]
    async fn test_dispatch_bracket_tighten_and_cancel_smoke() {
        let mut oms = make_dry_run_oms();
        let risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        let cfg = enabled_cfg(1800);

        dispatch_exit_command(
            &mut oms,
            &risk,
            ExitCommand::PlaceBracket(bracket_request(49081)),
            &cfg,
        )
        .await
        .expect("bracket must place in dry-run");
        assert_eq!(oms.total_placed(), 1);

        let entry_id = oms
            .active_orders()
            .iter()
            .find(|o| o.order_id.starts_with("PAPER-SO-"))
            .map(|o| o.order_id.clone())
            .expect("bracket entry must be tracked as a ManagedOrder");
        assert!(
            oms.super_order(&entry_id).is_some(),
            "bracket must land in the super-order registry"
        );

        dispatch_exit_command(
            &mut oms,
            &risk,
            ExitCommand::TightenStop {
                order_id: entry_id.clone(),
                modify: ModifySuperOrderLeg::StopLoss {
                    stop_loss_price: 97.0,
                    trailing_jump: 0.0,
                },
            },
            &cfg,
        )
        .await
        .expect("stop-loss tighten must succeed in dry-run");

        dispatch_exit_command(
            &mut oms,
            &risk,
            ExitCommand::CancelBracket {
                order_id: entry_id.clone(),
            },
            &cfg,
        )
        .await
        .expect("ENTRY_LEG cancel must succeed pre-fill in dry-run");
    }

    /// PlaceOcoProtection dispatch smoke (CNC forever/OCO — honestly NOT
    /// the intraday exit vehicle; wired for completeness).
    #[tokio::test]
    async fn test_dispatch_place_oco_protection_smoke() {
        let mut oms = make_dry_run_oms();
        let risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        let cfg = enabled_cfg(1800);

        dispatch_exit_command(
            &mut oms,
            &risk,
            ExitCommand::PlaceOcoProtection(oco_request()),
            &cfg,
        )
        .await
        .expect("CNC forever/OCO must place in dry-run");
        assert_eq!(oms.total_placed(), 1);
    }

    /// VerifyExecution: a tracked paper order returns `SimulatedFilled`
    /// deterministically and the ladder stops after ONE probe (decisive
    /// verdict) — with paused time the whole ladder is instant.
    #[tokio::test]
    async fn test_dispatch_verify_execution_paper_simulated_filled() {
        let mut oms = make_dry_run_oms();
        let risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        let cfg = enabled_cfg(1800);

        let request = PlaceOrderRequest {
            security_id: 13,
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
        let order_id = oms.place_order(request).await.expect("paper place");

        let verdict = run_verify_ladder(&mut oms, &order_id, &cfg)
            .await
            .expect("paper verify must succeed");
        assert_eq!(
            verdict,
            ExecutionVerdict::SimulatedFilled,
            "dry-run verify must be the paper-only verdict (never Filled)"
        );

        dispatch_exit_command(
            &mut oms,
            &risk,
            ExitCommand::VerifyExecution { order_id },
            &cfg,
        )
        .await
        .expect("verify dispatch must succeed");
    }

    /// An engine-call failure propagates as a typed Err (and is
    /// EXIT-ORDER-01-coded inside the dispatcher): TightenStop on an
    /// unknown order id.
    #[tokio::test]
    async fn test_dispatch_engine_failure_propagates_typed_error() {
        let mut oms = make_dry_run_oms();
        let risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        let cfg = enabled_cfg(1800);

        let result = dispatch_exit_command(
            &mut oms,
            &risk,
            ExitCommand::TightenStop {
                order_id: "NO-SUCH-ORDER".to_string(),
                modify: ModifySuperOrderLeg::Target {
                    target_price: 111.0,
                },
            },
            &cfg,
        )
        .await;
        assert!(
            matches!(result, Err(OmsError::OrderNotFound { .. })),
            "unknown order id must be a typed OrderNotFound, got {result:?}"
        );

        // Verify ladder on an untracked id: structural OrderNotFound too.
        let verify = dispatch_exit_command(
            &mut oms,
            &risk,
            ExitCommand::VerifyExecution {
                order_id: "NO-SUCH-ORDER".to_string(),
            },
            &cfg,
        )
        .await;
        assert!(matches!(verify, Err(OmsError::OrderNotFound { .. })));
    }

    /// Disabled `execute_exit_for_security` runs the LEGACY body:
    /// cancel actives + ONE plain MARKET close order (no slicing, no
    /// dispatcher) — byte-equivalent to the pre-Cluster-B Exit arm.
    #[tokio::test]
    async fn test_execute_exit_disabled_runs_legacy_cancel_and_close() {
        let mut oms = make_dry_run_oms();
        let mut risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        risk.record_fill(13, 3, 100.0, 1); // net +3 lots
        let cfg = ExitOrdersConfig::default(); // disabled

        let pending = PlaceOrderRequest {
            security_id: 13,
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
        let pending_id = oms.place_order(pending).await.expect("paper place");

        execute_exit_for_security(&mut oms, &risk, 13, &cfg, &None, 0).await;

        // Legacy path: the active order cancelled + exactly ONE close
        // order placed (freeze scalar 0 is IGNORED — no slicing path).
        assert_eq!(oms.total_placed(), 2, "1 pre-existing + 1 plain close");
        assert!(
            !oms.active_orders().iter().any(|o| o.order_id == pending_id),
            "legacy exit must cancel the pre-existing active order"
        );
    }

    /// Disabled exit with a FLAT position cancels actives only — no close
    /// order is placed (net_lots == 0 guard, legacy-equivalent).
    #[tokio::test]
    async fn test_execute_exit_disabled_flat_position_places_no_close() {
        let mut oms = make_dry_run_oms();
        let risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        let cfg = ExitOrdersConfig::default();

        execute_exit_for_security(&mut oms, &risk, 13, &cfg, &None, 0).await;
        assert_eq!(oms.total_placed(), 0, "flat + no actives = no orders");
    }

    /// Enabled `execute_exit_for_security` routes through the dispatcher:
    /// a 2-lot net position at freeze 1 slices into TWO paper close
    /// orders (the CloseAll path, not the legacy single-order path).
    #[tokio::test]
    async fn test_execute_exit_enabled_routes_through_dispatcher_and_slices() {
        let mut oms = make_dry_run_oms();
        let mut risk = RiskEngine::new(2.0, 100, 1_000_000.0);
        risk.record_fill(21, -2, 50.0, 1); // net -2 lots → BUY 2 to close
        let cfg = enabled_cfg(1);

        execute_exit_for_security(&mut oms, &risk, 21, &cfg, &None, 0).await;
        assert_eq!(
            oms.total_placed(),
            2,
            "2 lots at freeze 1 must slice into 2 paper close orders"
        );
    }

    /// `command_label` covers every ExitCommand variant with a stable
    /// snake_case label (log/test vocabulary).
    #[test]
    fn test_command_label_covers_all_variants() {
        let labels = [
            command_label(&ExitCommand::CloseAll {
                security_id: 13,
                freeze_limit: 1,
            }),
            command_label(&ExitCommand::PlaceBracket(bracket_request(13))),
            command_label(&ExitCommand::TightenStop {
                order_id: "x".to_string(),
                modify: ModifySuperOrderLeg::Target { target_price: 1.0 },
            }),
            command_label(&ExitCommand::CancelBracket {
                order_id: "x".to_string(),
            }),
            command_label(&ExitCommand::PlaceOcoProtection(oco_request())),
            command_label(&ExitCommand::VerifyExecution {
                order_id: "x".to_string(),
            }),
        ];
        assert_eq!(
            labels,
            [
                "close_all",
                "place_bracket",
                "tighten_stop",
                "cancel_bracket",
                "place_oco_protection",
                "verify_execution",
            ]
        );
    }
}
