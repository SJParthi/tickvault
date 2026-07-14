//! Pure exit-order rules — slicing math, super-order / Forever-OCO
//! validation, MPP verdict classification, and the verify backoff ladder
//! (🔷 DHAN exit-order execution layer, Cluster B, 2026-07-14).
//!
//! Every function here is PURE (zero I/O, zero engine state) and TOTAL —
//! unknown inputs produce a typed `Err` / `None` / fail-closed verdict,
//! never `unreachable!()` and never a panic. The engine (`oms/engine.rs`)
//! wires these into its exit methods; the app dispatcher
//! (`crates/app/src/exit_execution.rs`) constructs [`ExitCommand`]s —
//! Cluster A goes through the dispatcher, never the engine methods
//! directly.
//!
//! Cold path — allocations are fine (orders ~1-100/day, OMS module docs).

use super::types::{
    ExecutionVerdict, ModifySuperOrderLeg, OmsError, PlaceForeverOcoRequest, PlaceSuperOrderRequest,
};
use tickvault_common::order_types::{OrderStatus, OrderType, ProductType};

/// Cap on the exponential verify backoff rung (seconds).
///
/// Ladder: 1, 2, 4, 8, 10, 10, … — with the default `max_attempts = 5`
/// the cumulative wait is 25s inside the 30s `mpp_verify_deadline_secs`.
pub const VERIFY_BACKOFF_CAP_SECS: u64 = 10;

/// Upper bound on the number of slices one order may split into.
///
/// Defensive totality bound: without it, `compute_slices(i64::MAX, 1)`
/// would attempt a ~9e18-element allocation (abort — a panic-class
/// failure). Real freeze limits are thousands of units and exit
/// quantities a few lots, so any request past this cap is a data error
/// and refuses with a typed `RiskRejected`.
pub const MAX_ORDER_SLICES: i64 = 1_000;

/// The Cluster A seam vocabulary — frozen in this PR. Cluster A constructs
/// commands; ONLY the app dispatcher executes them (never engine methods
/// directly).
#[derive(Debug, Clone)]
pub enum ExitCommand {
    /// What Signal::Exit does today: cancel actives + close net position
    /// (sliced if needed) + verify.
    CloseAll {
        /// Dhan security identifier.
        security_id: u64,
        /// Exchange freeze quantity for the close order(s).
        freeze_limit: i64,
    },
    /// Entry-time 3-leg bracket (Cluster A constructs at entry decision).
    PlaceBracket(PlaceSuperOrderRequest),
    /// Tighten (or otherwise modify) one leg of a tracked super order.
    TightenStop {
        /// Entry-leg order id of the tracked super order.
        order_id: String,
        /// Leg-restricted modify payload.
        modify: ModifySuperOrderLeg,
    },
    /// Cancel a tracked super order (ENTRY_LEG cancel — refused post-fill).
    CancelBracket {
        /// Entry-leg order id of the tracked super order.
        order_id: String,
    },
    /// Forever/OCO protection (CNC/MTF only — NOT the intraday exit vehicle).
    PlaceOcoProtection(PlaceForeverOcoRequest),
    /// Run the MPP verify ladder for one order.
    VerifyExecution {
        /// Order id to verify.
        order_id: String,
    },
}

/// Validates an engine-level super-order request before any gate is spent.
///
/// Rules (Ruling 3 / §3.2 of the 2026-07-14 design):
/// - price / target / stop-loss / trailing must be FINITE (NaN/∞ slip
///   through the engine's ordering validator — Verified);
/// - entry `order_type` ∈ {LIMIT, MARKET} (super orders reject SL/SLM —
///   portal 04:51);
/// - MARKET ⇒ `price == 0.0` (mirror of the plain-order rule; probe U1);
/// - `quantity > 0` and a lot multiple when `lot_size > 1`;
/// - `trailing_jump >= 0.0` and, when non-zero, strictly below
///   `|price - stop_loss_price|` (Assumed A6 sanity bound);
/// - I-P0-03 expiry gate (`expiry_date < today` ⇒ `ExpiredContract`).
///
/// Relative price ordering (BUY target > entry etc.) deliberately stays in
/// `engine.rs::validate_super_order_prices` (wired there, not duplicated).
pub fn validate_super_order_request(r: &PlaceSuperOrderRequest) -> Result<(), OmsError> {
    for (label, value) in [
        ("price", r.price),
        ("targetPrice", r.target_price),
        ("stopLossPrice", r.stop_loss_price),
        ("trailingJump", r.trailing_jump),
    ] {
        if !value.is_finite() {
            return Err(OmsError::RiskRejected {
                reason: format!("super order {label} must be finite, got {value}"),
            });
        }
    }

    if !matches!(r.order_type, OrderType::Limit | OrderType::Market) {
        return Err(OmsError::RiskRejected {
            reason: format!(
                "super order entry leg must be LIMIT or MARKET, got {}",
                r.order_type.as_str()
            ),
        });
    }

    if r.order_type == OrderType::Market && r.price != 0.0 {
        return Err(OmsError::RiskRejected {
            reason: format!("MARKET super order must have price=0.0, got {}", r.price),
        });
    }

    if r.quantity <= 0 {
        return Err(OmsError::RiskRejected {
            reason: format!("super order quantity must be positive, got {}", r.quantity),
        });
    }

    if r.lot_size > 1 && r.quantity % i64::from(r.lot_size) != 0 {
        return Err(OmsError::RiskRejected {
            reason: format!(
                "super order quantity {} is not a multiple of lot size {}",
                r.quantity, r.lot_size
            ),
        });
    }

    if r.trailing_jump < 0.0 {
        return Err(OmsError::RiskRejected {
            reason: format!(
                "super order trailingJump must be >= 0.0, got {}",
                r.trailing_jump
            ),
        });
    }
    // Assumed A6: a trail wider than the entry↔SL distance would cancel or
    // overshoot the stop on the first jump. Hard reject; relax only if a
    // live probe contradicts.
    if r.trailing_jump != 0.0 && r.trailing_jump >= (r.price - r.stop_loss_price).abs() {
        return Err(OmsError::RiskRejected {
            reason: format!(
                "super order trailingJump {} must be below |entry - stopLoss| = {}",
                r.trailing_jump,
                (r.price - r.stop_loss_price).abs()
            ),
        });
    }

    expiry_gate(
        r.security_id,
        r.expiry_date,
        chrono::Utc::now().date_naive(),
    )
}

/// Validates an engine-level Forever/OCO request before any gate is spent.
///
/// Rules (07b §1): `product_type` ∈ {CNC, MTF} ONLY (Forever orders are
/// structurally NOT the intraday F&O exit vehicle — wired for completeness);
/// `order_type` ∈ {LIMIT, MARKET}; price + trigger positive AND finite;
/// `quantity > 0`; when the OCO leg is present, all three of its fields
/// positive/finite; I-P0-03 expiry gate.
pub fn validate_forever_oco(r: &PlaceForeverOcoRequest) -> Result<(), OmsError> {
    if !matches!(r.product_type, ProductType::Cnc | ProductType::Mtf) {
        return Err(OmsError::RiskRejected {
            reason: format!(
                "forever/OCO orders accept CNC or MTF only, got {}",
                r.product_type.as_str()
            ),
        });
    }

    if !matches!(r.order_type, OrderType::Limit | OrderType::Market) {
        return Err(OmsError::RiskRejected {
            reason: format!(
                "forever/OCO order type must be LIMIT or MARKET, got {}",
                r.order_type.as_str()
            ),
        });
    }

    for (label, value) in [("price", r.price), ("triggerPrice", r.trigger_price)] {
        if !(value.is_finite() && value > 0.0) {
            return Err(OmsError::RiskRejected {
                reason: format!("forever/OCO {label} must be positive and finite, got {value}"),
            });
        }
    }

    if r.quantity <= 0 {
        return Err(OmsError::RiskRejected {
            reason: format!("forever/OCO quantity must be positive, got {}", r.quantity),
        });
    }

    if let Some(leg) = &r.oco_leg {
        for (label, value) in [("price1", leg.price), ("triggerPrice1", leg.trigger_price)] {
            if !(value.is_finite() && value > 0.0) {
                return Err(OmsError::RiskRejected {
                    reason: format!(
                        "OCO second leg {label} must be positive and finite, got {value}"
                    ),
                });
            }
        }
        if leg.quantity <= 0 {
            return Err(OmsError::RiskRejected {
                reason: format!(
                    "OCO second leg quantity must be positive, got {}",
                    leg.quantity
                ),
            });
        }
    }

    expiry_gate(
        r.security_id,
        r.expiry_date,
        chrono::Utc::now().date_naive(),
    )
}

/// Splits `quantity` into slices of at most `freeze_limit` each.
///
/// Used by the dry-run slicing simulation (N paper orders) and by
/// reconciliation math — the LIVE split is Dhan's server-side
/// `POST /orders/slicing` (one POST, never client-side loops).
///
/// Totality: `freeze_limit <= 0` or `quantity <= 0` ⇒ `RiskRejected`;
/// a split that would exceed [`MAX_ORDER_SLICES`] ⇒ `RiskRejected`
/// (defensive bound — see the constant's doc). Checked math throughout —
/// no overflow at `i64::MAX`.
///
/// Invariants (property-tested): `sum(slices) == quantity` and every slice
/// is in `1..=freeze_limit`.
pub fn compute_slices(quantity: i64, freeze_limit: i64) -> Result<Vec<i64>, OmsError> {
    if freeze_limit <= 0 {
        return Err(OmsError::RiskRejected {
            reason: format!("freeze limit must be positive, got {freeze_limit}"),
        });
    }
    if quantity <= 0 {
        return Err(OmsError::RiskRejected {
            reason: format!("slice quantity must be positive, got {quantity}"),
        });
    }

    let full_slices = quantity / freeze_limit;
    let remainder = quantity % freeze_limit;
    // full_slices <= quantity and the +1 cannot overflow (full_slices <
    // i64::MAX because freeze_limit >= 1 implies full_slices <= quantity).
    let slice_count = full_slices.saturating_add(i64::from(remainder > 0));
    if slice_count > MAX_ORDER_SLICES {
        return Err(OmsError::RiskRejected {
            reason: format!(
                "quantity {quantity} at freeze limit {freeze_limit} needs {slice_count} slices \
                 (max {MAX_ORDER_SLICES}) — refusing runaway split"
            ),
        });
    }

    let capacity = usize::try_from(slice_count).unwrap_or(0);
    let mut slices = Vec::with_capacity(capacity);
    for _ in 0..full_slices {
        slices.push(freeze_limit);
    }
    if remainder > 0 {
        slices.push(remainder);
    }
    Ok(slices)
}

/// True when `quantity` exceeds the exchange freeze limit and the order
/// must go through `POST /orders/slicing` (never call the slicing endpoint
/// under-freeze — behavior undocumented, live-probe U5).
pub fn requires_slicing(quantity: i64, freeze_limit: i64) -> bool {
    quantity > freeze_limit
}

/// True when an ENTRY_LEG cancel is allowed for the given raw entry status.
///
/// FALSE for `PART_TRADED` and `TRADED`: a post-fill ENTRY_LEG cancel's
/// effect on the live exit legs is UNVERIFIED-LIVE (naked-position risk
/// U4) — it is NEVER exercised by design.
pub fn entry_leg_cancel_allowed(entry_status_raw: &str) -> bool {
    !matches!(entry_status_raw, "PART_TRADED" | "TRADED")
}

/// True when an ENTRY_LEG modify is allowed for the given raw entry status
/// (07a §3: only PENDING / PART_TRADED; TRANSIT tolerated as pre-PENDING).
pub fn entry_leg_modify_allowed(entry_status_raw: &str) -> bool {
    matches!(entry_status_raw, "PENDING" | "PART_TRADED" | "TRANSIT")
}

/// Classifies an MPP verify probe result into an [`ExecutionVerdict`].
///
/// TOTAL over all inputs — never panics (property-tested incl. NaN
/// `avg_price`). Classifies on STATUS + fill quantities ONLY, never on the
/// order-type echo (the book may show LIMIT for a request we sent as
/// MARKET — MPP conversion, orders.md rule 18).
///
/// - `None` status (unparsable) ⇒ [`ExecutionVerdict::Unknown`] —
///   fail-closed, NEVER treated as filled;
/// - `Traded` ⇒ [`ExecutionVerdict::Filled`];
/// - `PartTraded` ⇒ [`ExecutionVerdict::PartiallyFilled`] (in-budget or at
///   budget — the caller reads elapsed/deadline for the at-budget flag);
/// - `Rejected` / `Cancelled` / `Expired` / `Closed` ⇒
///   [`ExecutionVerdict::Terminal`] (`Closed` is the super-order all-legs-
///   done state — the engine applies the Closed transition separately);
/// - every other (pending-class) status ⇒ [`ExecutionVerdict::Pending`]
///   in-budget, [`ExecutionVerdict::PendingAtLimit`] once
///   `elapsed_secs >= deadline_secs` (the MPP never-fills case).
pub fn classify_mpp_verdict(
    status: Option<OrderStatus>,
    raw_status: &str,
    traded_qty: i64,
    quantity: i64,
    avg_price: f64,
    elapsed_secs: u64,
    deadline_secs: u64,
) -> ExecutionVerdict {
    let Some(status) = status else {
        return ExecutionVerdict::Unknown {
            raw_status: raw_status.to_string(),
        };
    };

    match status {
        OrderStatus::Traded => ExecutionVerdict::Filled {
            traded_qty,
            avg_price,
        },
        OrderStatus::PartTraded => ExecutionVerdict::PartiallyFilled {
            traded_qty,
            remaining: quantity.saturating_sub(traded_qty).max(0),
        },
        OrderStatus::Rejected
        | OrderStatus::Cancelled
        | OrderStatus::Expired
        | OrderStatus::Closed => ExecutionVerdict::Terminal { status },
        OrderStatus::Transit
        | OrderStatus::Pending
        | OrderStatus::Confirmed
        | OrderStatus::Triggered => {
            if elapsed_secs >= deadline_secs {
                ExecutionVerdict::PendingAtLimit { elapsed_secs }
            } else {
                ExecutionVerdict::Pending { elapsed_secs }
            }
        }
    }
}

/// Delay (seconds) BEFORE verify probe `attempt` (1-indexed).
///
/// `Some(min(2^(attempt-1), 10))` — the 1, 2, 4, 8, 10 ladder; `None` when
/// `attempt == 0` (not a valid 1-indexed rung) or `attempt > max_attempts`
/// (ladder exhausted — the caller stops probing and records the terminal
/// verdict). Total: no shift overflow at any `u32` input.
pub fn next_verify_backoff_secs(attempt: u32, max_attempts: u32) -> Option<u64> {
    if attempt == 0 || attempt > max_attempts {
        return None;
    }
    let exponent = attempt - 1;
    // 2^4 = 16 already exceeds the 10s cap, so any exponent >= 4 is capped
    // (also avoids shift-overflow for large attempts).
    let delay = if exponent >= 4 {
        VERIFY_BACKOFF_CAP_SECS
    } else {
        1u64 << exponent
    };
    Some(delay.min(VERIFY_BACKOFF_CAP_SECS))
}

/// I-P0-03 expiry gate shared by the exit-order validators (the engine
/// place-order gate-4 pattern: reject when the derivative contract expired
/// before `today`).
fn expiry_gate(
    security_id: u64,
    expiry_date: Option<chrono::NaiveDate>,
    today: chrono::NaiveDate,
) -> Result<(), OmsError> {
    if let Some(expiry) = expiry_date
        && expiry < today
    {
        return Err(OmsError::ExpiredContract {
            security_id,
            expiry_date: expiry.to_string(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oms::types::OcoSecondLeg;
    use chrono::NaiveDate;
    use proptest::prelude::*;
    use tickvault_common::order_types::{OrderValidity, TransactionType};

    fn super_req() -> PlaceSuperOrderRequest {
        PlaceSuperOrderRequest {
            security_id: 49081,
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

    fn forever_req() -> PlaceForeverOcoRequest {
        PlaceForeverOcoRequest {
            security_id: 1333,
            transaction_type: TransactionType::Sell,
            product_type: ProductType::Cnc,
            order_type: OrderType::Limit,
            validity: OrderValidity::Day,
            quantity: 5,
            price: 1428.0,
            trigger_price: 1427.0,
            oco_leg: None,
            expiry_date: None,
        }
    }

    // --- validate_super_order_request ---

    #[test]
    fn test_validate_super_order_request_accepts_valid_limit_bracket() {
        assert!(validate_super_order_request(&super_req()).is_ok());
    }

    #[test]
    fn test_validate_super_order_request_accepts_market_with_zero_price() {
        let mut r = super_req();
        r.order_type = OrderType::Market;
        r.price = 0.0;
        assert!(validate_super_order_request(&r).is_ok());
    }

    #[test]
    fn test_validate_super_order_request_rejects_market_with_nonzero_price() {
        let mut r = super_req();
        r.order_type = OrderType::Market;
        r.price = 100.0;
        assert!(matches!(
            validate_super_order_request(&r),
            Err(OmsError::RiskRejected { .. })
        ));
    }

    #[test]
    fn test_validate_super_order_request_rejects_sl_and_slm_order_types() {
        // Portal 04:51 — super orders accept LIMIT/MARKET only; the old
        // doc-comment claiming SL/SLM was WRONG (Verified V6).
        for ot in [OrderType::StopLoss, OrderType::StopLossMarket] {
            let mut r = super_req();
            r.order_type = ot;
            assert!(
                matches!(
                    validate_super_order_request(&r),
                    Err(OmsError::RiskRejected { .. })
                ),
                "{ot:?} must be rejected"
            );
        }
    }

    #[test]
    fn test_validate_super_order_request_boundary_zero_negative_quantity() {
        for qty in [0_i64, -1, i64::MIN] {
            let mut r = super_req();
            r.quantity = qty;
            assert!(
                matches!(
                    validate_super_order_request(&r),
                    Err(OmsError::RiskRejected { .. })
                ),
                "quantity {qty} must be rejected"
            );
        }
    }

    #[test]
    fn test_validate_super_order_request_boundary_nan_and_infinite_prices() {
        // The engine's ordering validator lets NaN slip (Verified V17) —
        // the finite gate here is the L4 defense.
        for (field, value) in [
            ("price", f64::NAN),
            ("price", f64::INFINITY),
            ("target", f64::NAN),
            ("sl", f64::NEG_INFINITY),
            ("trail", f64::NAN),
        ] {
            let mut r = super_req();
            match field {
                "price" => r.price = value,
                "target" => r.target_price = value,
                "sl" => r.stop_loss_price = value,
                _ => r.trailing_jump = value,
            }
            assert!(
                matches!(
                    validate_super_order_request(&r),
                    Err(OmsError::RiskRejected { .. })
                ),
                "{field}={value} must be rejected"
            );
        }
    }

    #[test]
    fn test_validate_super_order_request_rejects_lot_multiple_violation() {
        let mut r = super_req();
        r.quantity = 80; // not a multiple of lot_size 75
        assert!(matches!(
            validate_super_order_request(&r),
            Err(OmsError::RiskRejected { .. })
        ));
        // lot_size <= 1 disables the multiple check.
        r.lot_size = 1;
        assert!(validate_super_order_request(&r).is_ok());
    }

    #[test]
    fn test_validate_super_order_request_boundary_negative_trailing_jump() {
        let mut r = super_req();
        r.trailing_jump = -0.05;
        assert!(matches!(
            validate_super_order_request(&r),
            Err(OmsError::RiskRejected { .. })
        ));
    }

    #[test]
    fn test_validate_super_order_request_trailing_bound_limit_at_entry_sl_gap() {
        // |entry - SL| = 5.0 — a trail at/above the gap is rejected,
        // strictly below passes (Assumed A6).
        let mut r = super_req();
        r.trailing_jump = 5.0;
        assert!(matches!(
            validate_super_order_request(&r),
            Err(OmsError::RiskRejected { .. })
        ));
        r.trailing_jump = 4.99;
        assert!(validate_super_order_request(&r).is_ok());
    }

    #[test]
    fn test_validate_super_order_request_rejects_expired_contract() {
        let mut r = super_req();
        r.expiry_date = NaiveDate::from_ymd_opt(2020, 1, 1);
        assert!(matches!(
            validate_super_order_request(&r),
            Err(OmsError::ExpiredContract { .. })
        ));
    }

    proptest! {
        #[test]
        fn prop_validate_super_order_request_total_never_panics(
            price in proptest::num::f64::ANY,
            target in proptest::num::f64::ANY,
            sl in proptest::num::f64::ANY,
            trail in proptest::num::f64::ANY,
            quantity in proptest::num::i64::ANY,
            lot_size in proptest::num::u32::ANY,
        ) {
            let mut r = super_req();
            r.price = price;
            r.target_price = target;
            r.stop_loss_price = sl;
            r.trailing_jump = trail;
            r.quantity = quantity;
            r.lot_size = lot_size;
            // Total: any input yields Ok or a typed Err — never a panic.
            let _ = validate_super_order_request(&r);
        }
    }

    // --- validate_forever_oco ---

    #[test]
    fn test_validate_forever_oco_accepts_cnc_single_and_mtf_oco() {
        assert!(validate_forever_oco(&forever_req()).is_ok());

        let mut oco = forever_req();
        oco.product_type = ProductType::Mtf;
        oco.oco_leg = Some(OcoSecondLeg {
            price: 1420.0,
            trigger_price: 1419.0,
            quantity: 5,
        });
        assert!(validate_forever_oco(&oco).is_ok());
    }

    #[test]
    fn test_validate_forever_oco_rejects_intraday_and_margin_products() {
        // 07b §1: CNC/MTF ONLY — the reason Forever/OCO structurally
        // cannot serve intraday F&O exits.
        for pt in [ProductType::Intraday, ProductType::Margin] {
            let mut r = forever_req();
            r.product_type = pt;
            assert!(
                matches!(validate_forever_oco(&r), Err(OmsError::RiskRejected { .. })),
                "{pt:?} must be rejected"
            );
        }
    }

    #[test]
    fn test_validate_forever_oco_rejects_sl_order_types() {
        for ot in [OrderType::StopLoss, OrderType::StopLossMarket] {
            let mut r = forever_req();
            r.order_type = ot;
            assert!(matches!(
                validate_forever_oco(&r),
                Err(OmsError::RiskRejected { .. })
            ));
        }
    }

    #[test]
    fn test_validate_forever_oco_boundary_zero_negative_nan_prices() {
        for price in [0.0_f64, -1.0, f64::NAN, f64::INFINITY] {
            let mut r = forever_req();
            r.price = price;
            assert!(
                matches!(validate_forever_oco(&r), Err(OmsError::RiskRejected { .. })),
                "price {price} must be rejected"
            );
        }
        let mut r = forever_req();
        r.trigger_price = 0.0;
        assert!(matches!(
            validate_forever_oco(&r),
            Err(OmsError::RiskRejected { .. })
        ));
    }

    #[test]
    fn test_validate_forever_oco_boundary_zero_quantity_and_bad_second_leg() {
        let mut r = forever_req();
        r.quantity = 0;
        assert!(matches!(
            validate_forever_oco(&r),
            Err(OmsError::RiskRejected { .. })
        ));

        // Second leg completeness is BY CONSTRUCTION; its values still
        // have to be positive + finite.
        let mut oco = forever_req();
        oco.oco_leg = Some(OcoSecondLeg {
            price: 0.0,
            trigger_price: 1419.0,
            quantity: 5,
        });
        assert!(matches!(
            validate_forever_oco(&oco),
            Err(OmsError::RiskRejected { .. })
        ));

        let mut oco2 = forever_req();
        oco2.oco_leg = Some(OcoSecondLeg {
            price: 1420.0,
            trigger_price: 1419.0,
            quantity: -5,
        });
        assert!(matches!(
            validate_forever_oco(&oco2),
            Err(OmsError::RiskRejected { .. })
        ));
    }

    #[test]
    fn test_validate_forever_oco_rejects_expired_contract() {
        let mut r = forever_req();
        r.expiry_date = NaiveDate::from_ymd_opt(2020, 1, 1);
        assert!(matches!(
            validate_forever_oco(&r),
            Err(OmsError::ExpiredContract { .. })
        ));
    }

    proptest! {
        #[test]
        fn prop_validate_forever_oco_total_never_panics(
            price in proptest::num::f64::ANY,
            trigger in proptest::num::f64::ANY,
            quantity in proptest::num::i64::ANY,
            leg_price in proptest::num::f64::ANY,
            leg_trigger in proptest::num::f64::ANY,
            leg_qty in proptest::num::i64::ANY,
            with_leg in proptest::bool::ANY,
        ) {
            let mut r = forever_req();
            r.price = price;
            r.trigger_price = trigger;
            r.quantity = quantity;
            r.oco_leg = with_leg.then_some(OcoSecondLeg {
                price: leg_price,
                trigger_price: leg_trigger,
                quantity: leg_qty,
            });
            let _ = validate_forever_oco(&r);
        }
    }

    // --- compute_slices / requires_slicing ---

    #[test]
    fn test_compute_slices_exact_and_remainder_splits() {
        assert_eq!(compute_slices(250, 100).ok(), Some(vec![100, 100, 50]));
        assert_eq!(compute_slices(200, 100).ok(), Some(vec![100, 100]));
        assert_eq!(compute_slices(99, 100).ok(), Some(vec![99]));
        assert_eq!(compute_slices(100, 100).ok(), Some(vec![100]));
        assert_eq!(compute_slices(1, 1).ok(), Some(vec![1]));
    }

    #[test]
    fn test_compute_slices_boundary_zero_negative_inputs() {
        assert!(matches!(
            compute_slices(0, 100),
            Err(OmsError::RiskRejected { .. })
        ));
        assert!(matches!(
            compute_slices(-50, 100),
            Err(OmsError::RiskRejected { .. })
        ));
        assert!(matches!(
            compute_slices(100, 0),
            Err(OmsError::RiskRejected { .. })
        ));
        assert!(matches!(
            compute_slices(100, -1),
            Err(OmsError::RiskRejected { .. })
        ));
    }

    #[test]
    fn test_compute_slices_boundary_i64_max_no_overflow_no_runaway_alloc() {
        // i64::MAX at freeze 1 would need ~9e18 slices — typed refusal,
        // never an allocation abort or an overflow.
        assert!(matches!(
            compute_slices(i64::MAX, 1),
            Err(OmsError::RiskRejected { .. })
        ));
        // i64::MAX at a huge freeze still splits cleanly within the cap.
        let slices = compute_slices(i64::MAX, i64::MAX).ok();
        assert_eq!(slices, Some(vec![i64::MAX]));
        // Exactly at the slice cap: MAX_ORDER_SLICES slices of 1.
        let at_cap = compute_slices(MAX_ORDER_SLICES, 1).ok();
        assert_eq!(
            at_cap.map(|v| v.len()),
            Some(usize::try_from(MAX_ORDER_SLICES).unwrap_or(0))
        );
        // One past the cap refuses.
        assert!(matches!(
            compute_slices(MAX_ORDER_SLICES + 1, 1),
            Err(OmsError::RiskRejected { .. })
        ));
    }

    proptest! {
        #[test]
        fn prop_compute_slices_sum_and_bounds_invariants(
            quantity in 1_i64..=2_000_000,
            freeze in 1_i64..=10_000,
        ) {
            match compute_slices(quantity, freeze) {
                Ok(slices) => {
                    let sum: i64 = slices.iter().sum();
                    prop_assert_eq!(sum, quantity);
                    prop_assert!(slices.iter().all(|s| (1..=freeze).contains(s)));
                    prop_assert!(!slices.is_empty());
                }
                Err(OmsError::RiskRejected { .. }) => {
                    // Only the slice-count cap can refuse in this range.
                    let count = quantity / freeze + i64::from(quantity % freeze > 0);
                    prop_assert!(count > MAX_ORDER_SLICES);
                }
                Err(other) => prop_assert!(false, "unexpected error: {}", other),
            }
        }

        #[test]
        fn prop_compute_slices_total_never_panics(
            quantity in proptest::num::i64::ANY,
            freeze in proptest::num::i64::ANY,
        ) {
            let _ = compute_slices(quantity, freeze);
        }
    }

    #[test]
    fn test_requires_slicing_boundary_at_freeze_limit() {
        assert!(!requires_slicing(100, 100)); // at freeze = no slicing
        assert!(!requires_slicing(1, 100));
        assert!(requires_slicing(101, 100));
        assert!(requires_slicing(1, 0)); // degenerate freeze → slicing path → typed refusal downstream
        assert!(!requires_slicing(-5, 0));
    }

    // --- entry-leg gating tables ---

    #[test]
    fn test_entry_leg_cancel_allowed_table() {
        // Post-fill ENTRY_LEG cancel = UNVERIFIED-LIVE naked-position risk
        // U4 — NEVER exercised.
        assert!(!entry_leg_cancel_allowed("PART_TRADED"));
        assert!(!entry_leg_cancel_allowed("TRADED"));
        assert!(entry_leg_cancel_allowed("PENDING"));
        assert!(entry_leg_cancel_allowed("TRANSIT"));
        assert!(entry_leg_cancel_allowed("CONFIRMED"));
        // Unknown strings stay cancellable (fail-open here is safe: the
        // cancel is itself a protective op; the refusal targets fills only).
        assert!(entry_leg_cancel_allowed("SOMETHING_NEW"));
    }

    #[test]
    fn test_entry_leg_modify_allowed_table() {
        assert!(entry_leg_modify_allowed("PENDING"));
        assert!(entry_leg_modify_allowed("PART_TRADED"));
        assert!(entry_leg_modify_allowed("TRANSIT"));
        assert!(!entry_leg_modify_allowed("TRADED"));
        assert!(!entry_leg_modify_allowed("REJECTED"));
        assert!(!entry_leg_modify_allowed("CANCELLED"));
        assert!(!entry_leg_modify_allowed(""));
        assert!(!entry_leg_modify_allowed("SOMETHING_NEW"));
    }

    // --- classify_mpp_verdict ---

    #[test]
    fn test_classify_mpp_verdict_traded_is_filled() {
        let v = classify_mpp_verdict(Some(OrderStatus::Traded), "TRADED", 75, 75, 101.5, 3, 30);
        assert_eq!(
            v,
            ExecutionVerdict::Filled {
                traded_qty: 75,
                avg_price: 101.5
            }
        );
    }

    #[test]
    fn test_classify_mpp_verdict_part_traded_in_and_at_budget() {
        // In-budget and at-budget both classify PartiallyFilled — the
        // at-budget flags (needs_reconciliation + EXIT-VERIFY-01) are the
        // engine's, keyed on the same elapsed/deadline the caller holds.
        for elapsed in [3_u64, 30, 31] {
            let v = classify_mpp_verdict(
                Some(OrderStatus::PartTraded),
                "PART_TRADED",
                30,
                75,
                100.0,
                elapsed,
                30,
            );
            assert_eq!(
                v,
                ExecutionVerdict::PartiallyFilled {
                    traded_qty: 30,
                    remaining: 45
                }
            );
        }
    }

    #[test]
    fn test_classify_mpp_verdict_pending_in_budget_and_at_limit() {
        let pending =
            classify_mpp_verdict(Some(OrderStatus::Pending), "PENDING", 0, 75, 0.0, 10, 30);
        assert_eq!(pending, ExecutionVerdict::Pending { elapsed_secs: 10 });

        // The MPP never-fills case: MARKET→LIMIT resting on the book past
        // the deadline is NEVER assumed filled.
        let at_limit =
            classify_mpp_verdict(Some(OrderStatus::Pending), "PENDING", 0, 75, 0.0, 30, 30);
        assert_eq!(
            at_limit,
            ExecutionVerdict::PendingAtLimit { elapsed_secs: 30 }
        );
    }

    #[test]
    fn test_classify_mpp_verdict_terminal_statuses() {
        for status in [
            OrderStatus::Rejected,
            OrderStatus::Cancelled,
            OrderStatus::Expired,
            OrderStatus::Closed,
        ] {
            let v = classify_mpp_verdict(Some(status), "X", 0, 75, 0.0, 5, 30);
            assert_eq!(v, ExecutionVerdict::Terminal { status });
        }
    }

    #[test]
    fn test_classify_mpp_verdict_unknown_status_fails_closed() {
        let v = classify_mpp_verdict(None, "MPP_WEIRD_STATE", 0, 75, 0.0, 5, 30);
        assert_eq!(
            v,
            ExecutionVerdict::Unknown {
                raw_status: "MPP_WEIRD_STATE".to_string()
            }
        );
    }

    #[test]
    fn test_classify_mpp_verdict_boundary_overfill_and_zero_deadline() {
        // traded_qty > quantity (broker over-report) clamps remaining to 0.
        let v = classify_mpp_verdict(
            Some(OrderStatus::PartTraded),
            "PART_TRADED",
            100,
            75,
            100.0,
            5,
            30,
        );
        assert_eq!(
            v,
            ExecutionVerdict::PartiallyFilled {
                traded_qty: 100,
                remaining: 0
            }
        );
        // deadline 0 → everything pending-class is instantly at-limit.
        let v2 = classify_mpp_verdict(Some(OrderStatus::Pending), "PENDING", 0, 75, 0.0, 0, 0);
        assert_eq!(v2, ExecutionVerdict::PendingAtLimit { elapsed_secs: 0 });
        // NaN avg_price passes through the payload without panicking.
        let v3 = classify_mpp_verdict(Some(OrderStatus::Traded), "TRADED", 75, 75, f64::NAN, 5, 30);
        assert!(matches!(v3, ExecutionVerdict::Filled { .. }));
    }

    proptest! {
        #[test]
        fn prop_classify_mpp_verdict_total_never_panics(
            status_idx in 0_usize..=10,
            raw in ".{0,40}",
            traded_qty in proptest::num::i64::ANY,
            quantity in proptest::num::i64::ANY,
            avg_price in proptest::num::f64::ANY,
            elapsed in proptest::num::u64::ANY,
            deadline in proptest::num::u64::ANY,
        ) {
            let all = [
                OrderStatus::Transit,
                OrderStatus::Pending,
                OrderStatus::Confirmed,
                OrderStatus::PartTraded,
                OrderStatus::Traded,
                OrderStatus::Cancelled,
                OrderStatus::Rejected,
                OrderStatus::Expired,
                OrderStatus::Closed,
                OrderStatus::Triggered,
            ];
            let status = all.get(status_idx).copied();
            let verdict = classify_mpp_verdict(
                status, &raw, traded_qty, quantity, avg_price, elapsed, deadline,
            );
            // Fail-closed: an unparsable status can never classify Filled.
            if status.is_none() {
                let is_unknown = matches!(verdict, ExecutionVerdict::Unknown { .. });
                prop_assert!(is_unknown, "None status must classify Unknown");
            }
            // A PartiallyFilled remaining is never negative.
            if let ExecutionVerdict::PartiallyFilled { remaining, .. } = verdict {
                prop_assert!(remaining >= 0);
            }
        }
    }

    // --- next_verify_backoff_secs ---

    #[test]
    fn test_next_verify_backoff_secs_ladder_shape() {
        // The 1, 2, 4, 8, 10 ladder — 25s cumulative inside the 30s budget.
        assert_eq!(next_verify_backoff_secs(1, 5), Some(1));
        assert_eq!(next_verify_backoff_secs(2, 5), Some(2));
        assert_eq!(next_verify_backoff_secs(3, 5), Some(4));
        assert_eq!(next_verify_backoff_secs(4, 5), Some(8));
        assert_eq!(next_verify_backoff_secs(5, 5), Some(10));
        assert_eq!(next_verify_backoff_secs(6, 5), None);
    }

    #[test]
    fn test_next_verify_backoff_secs_boundary_zero_and_u32_max_no_overflow() {
        assert_eq!(next_verify_backoff_secs(0, 5), None);
        assert_eq!(next_verify_backoff_secs(0, 0), None);
        assert_eq!(next_verify_backoff_secs(1, 0), None);
        // No shift overflow at extreme attempts; still capped at 10s.
        assert_eq!(
            next_verify_backoff_secs(u32::MAX, u32::MAX),
            Some(VERIFY_BACKOFF_CAP_SECS)
        );
        assert_eq!(next_verify_backoff_secs(64, 64), Some(10));
    }

    proptest! {
        #[test]
        fn prop_next_verify_backoff_secs_total_and_bounded(
            attempt in proptest::num::u32::ANY,
            max_attempts in proptest::num::u32::ANY,
        ) {
            match next_verify_backoff_secs(attempt, max_attempts) {
                Some(delay) => {
                    prop_assert!(attempt >= 1 && attempt <= max_attempts);
                    prop_assert!((1..=VERIFY_BACKOFF_CAP_SECS).contains(&delay));
                }
                None => prop_assert!(attempt == 0 || attempt > max_attempts),
            }
        }
    }

    // --- expiry gate ---

    #[test]
    fn test_expiry_gate_boundary_today_passes_yesterday_rejects() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 14).unwrap_or_default();
        // Expiring TODAY still trades through the session (>= today passes).
        assert!(expiry_gate(13, NaiveDate::from_ymd_opt(2026, 7, 14), today).is_ok());
        assert!(expiry_gate(13, NaiveDate::from_ymd_opt(2026, 7, 15), today).is_ok());
        assert!(expiry_gate(13, None, today).is_ok());
        assert!(matches!(
            expiry_gate(13, NaiveDate::from_ymd_opt(2026, 7, 13), today),
            Err(OmsError::ExpiredContract { .. })
        ));
    }

    // --- ExitCommand seam ---

    #[test]
    fn test_exit_command_all_variants_construct_and_clone() {
        let commands = [
            ExitCommand::CloseAll {
                security_id: 13,
                freeze_limit: 1800,
            },
            ExitCommand::PlaceBracket(super_req()),
            ExitCommand::TightenStop {
                order_id: "SO-1".to_string(),
                modify: ModifySuperOrderLeg::StopLoss {
                    stop_loss_price: 96.0,
                    trailing_jump: 0.0,
                },
            },
            ExitCommand::CancelBracket {
                order_id: "SO-1".to_string(),
            },
            ExitCommand::PlaceOcoProtection(forever_req()),
            ExitCommand::VerifyExecution {
                order_id: "O-1".to_string(),
            },
        ];
        assert_eq!(commands.len(), 6);
        for cmd in &commands {
            let cloned = cmd.clone();
            assert!(!format!("{cloned:?}").is_empty());
        }
    }
}
