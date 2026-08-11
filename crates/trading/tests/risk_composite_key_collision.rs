//! Regression: the risk engine must key positions and mark prices on the
//! COMPOSITE `(security_id, exchange_segment)` — never on `security_id` alone.
//!
//! # The defect this pins (I-P1-11)
//!
//! `RiskEngine::positions` and `RiskEngine::market_prices` were
//! `HashMap<u64, _>`, keyed on the bare `security_id`. But a numeric
//! `security_id` is NOT unique:
//!
//! * Dhan reuses one id across segments — the live 2026-04-17 case was
//!   FINNIFTY `id = 27` in `IDX_I` colliding with a different `NSE_EQ`
//!   instrument that also carried `id = 27`.
//! * Groww's CASH and FNO `exchange_token` spaces overlap numerically.
//!
//! Two consequences, both of which reach money:
//!
//! 1. **Position limits under-count.** A long future and a short equity that
//!    share one numeric id collapsed into ONE `PositionInfo`, so their lots
//!    NETTED. `check_order` then approved an order whose true per-instrument
//!    position breached `max_position_lots`.
//! 2. **Fabricated P&L.** One leg's mark price overwrote the other's, so
//!    `total_unrealized_pnl` priced a position with a DIFFERENT instrument's
//!    price — and that number feeds the daily-loss auto-halt, which could
//!    then fire spuriously or fail to fire on a real loss.
//!
//! See `.claude/rules/project/security-id-uniqueness.md`.

use tickvault_common::types::ExchangeSegment;
use tickvault_trading::risk::RiskEngine;

/// The colliding numeric id. Any two segments reproduce the class; these two
/// are the realistic live pairing (an index future vs a cash equity).
const COLLIDING_ID: u64 = 27;
const SEG_A: ExchangeSegment = ExchangeSegment::NseFno;
const SEG_B: ExchangeSegment = ExchangeSegment::NseEquity;

fn engine() -> RiskEngine {
    // 2% max daily loss, 10 lots max position, 1,000,000 capital.
    RiskEngine::new(2.0, 10, 1_000_000.0)
}

/// Positions for the same numeric id in different segments must be INDEPENDENT.
///
/// Pre-fix, both fills landed in one row and netted to `+8 + (-8) = 0` lots.
#[test]
fn same_id_different_segments_have_independent_positions() {
    let mut risk = engine();

    // Long 8 lots of the FNO leg, short 8 lots of the EQ leg.
    risk.record_fill_in_segment(COLLIDING_ID, SEG_A, 8, 100.0, 1);
    risk.record_fill_in_segment(COLLIDING_ID, SEG_B, -8, 500.0, 1);

    let fno_lots = risk.net_lots_for_in_segment(COLLIDING_ID, SEG_A);
    let eq_lots = risk.net_lots_for_in_segment(COLLIDING_ID, SEG_B);

    assert_eq!(fno_lots, 8, "FNO leg must keep its own +8 lots");
    assert_eq!(eq_lots, -8, "EQ leg must keep its own -8 lots");

    // The pre-fix behaviour, asserted explicitly so a regression is unmistakable:
    // a single shared row would have netted these to zero.
    assert_ne!(
        fno_lots + eq_lots,
        fno_lots,
        "pre-fix collision signature: the two legs netted into one row"
    );
    assert_eq!(
        risk.open_position_count(),
        2,
        "two distinct instruments must occupy TWO position rows, not one"
    );
}

/// The netting bug let `check_order` approve a real limit breach.
///
/// Max is 10 lots. The FNO leg alone already holds 9. Pre-fix, the EQ leg's
/// -9 lots netted the shared row to 0, so a further +9 on FNO looked like
/// "0 + 9 = 9 <= 10" and was APPROVED — while the true FNO position would
/// become 18 lots, nearly double the limit.
#[test]
fn netting_across_segments_must_not_approve_a_position_limit_breach() {
    let mut risk = engine();

    risk.record_fill_in_segment(COLLIDING_ID, SEG_A, 9, 100.0, 1);
    risk.record_fill_in_segment(COLLIDING_ID, SEG_B, -9, 500.0, 1);

    // Sanity: the shared-row illusion would read zero here.
    assert_eq!(risk.net_lots_for_in_segment(COLLIDING_ID, SEG_A), 9);
    assert_eq!(risk.net_lots_for_in_segment(COLLIDING_ID, SEG_B), -9);

    let check = risk.check_order_in_segment(COLLIDING_ID, SEG_A, 9);
    assert!(
        !check.is_approved(),
        "9 existing + 9 new = 18 lots breaches the 10-lot max and MUST be \
         rejected; approving it is the pre-fix cross-segment netting bug"
    );
}

/// Mark prices must not cross-contaminate: each leg is priced with its own
/// segment's mark, so unrealized P&L (and the daily-loss halt that reads it)
/// is computed from real numbers.
#[test]
fn same_id_different_segments_have_independent_mark_prices() {
    let mut risk = engine();

    // FNO: long 1 lot @ 100. EQ: long 1 lot @ 500.
    risk.record_fill_in_segment(COLLIDING_ID, SEG_A, 1, 100.0, 1);
    risk.record_fill_in_segment(COLLIDING_ID, SEG_B, 1, 500.0, 1);

    // Each leg marked flat at its OWN entry → true unrealized P&L is exactly 0.
    risk.update_market_price_in_segment(COLLIDING_ID, SEG_A, 100.0);
    risk.update_market_price_in_segment(COLLIDING_ID, SEG_B, 500.0);

    let unrealized = risk.total_unrealized_pnl();
    assert!(
        unrealized.abs() < 1e-9,
        "both legs are marked at their own entry price, so unrealized P&L must \
         be 0.0 — got {unrealized}. Pre-fix, the EQ mark (500) overwrote the \
         FNO mark and priced the 100-entry FNO leg at 500, fabricating +400."
    );
}

/// The fabricated-P&L magnitude, pinned directly: with one shared mark row the
/// last writer wins and the other leg is priced with a foreign instrument's
/// price. Here the correct answer is 0.0 and the pre-fix answer was +400.
#[test]
fn cross_segment_mark_contamination_does_not_fabricate_pnl() {
    let mut risk = engine();

    risk.record_fill_in_segment(COLLIDING_ID, SEG_A, 1, 100.0, 1);
    risk.update_market_price_in_segment(COLLIDING_ID, SEG_A, 100.0);

    let before = risk.total_unrealized_pnl();

    // A DIFFERENT instrument that merely shares the numeric id now reports a
    // wildly different price. It must not touch the FNO leg's valuation.
    risk.update_market_price_in_segment(COLLIDING_ID, SEG_B, 500.0);

    let after = risk.total_unrealized_pnl();
    assert!(
        (after - before).abs() < 1e-9,
        "an unrelated same-id instrument's mark changed this leg's P&L \
         ({before} -> {after}); marks must be segment-scoped"
    );
}

/// Realized P&L must also book against the correct leg.
#[test]
fn realized_pnl_books_to_the_correct_segment_leg() {
    let mut risk = engine();

    // FNO: buy 1 @ 100, sell 1 @ 110 → +10 realized on the FNO leg.
    risk.record_fill_in_segment(COLLIDING_ID, SEG_A, 1, 100.0, 1);
    risk.record_fill_in_segment(COLLIDING_ID, SEG_A, -1, 110.0, 1);

    // EQ leg is untouched by the FNO round trip.
    risk.record_fill_in_segment(COLLIDING_ID, SEG_B, 1, 500.0, 1);

    let fno = risk
        .position_in_segment(COLLIDING_ID, SEG_A)
        .expect("FNO leg must exist");
    let eq = risk
        .position_in_segment(COLLIDING_ID, SEG_B)
        .expect("EQ leg must exist");

    assert!(
        (fno.realized_pnl - 10.0).abs() < 1e-9,
        "FNO leg must own its +10 realized P&L, got {}",
        fno.realized_pnl
    );
    assert!(
        eq.realized_pnl.abs() < 1e-9,
        "EQ leg never closed a trade and must have 0 realized P&L, got {}",
        eq.realized_pnl
    );
    assert_eq!(eq.net_lots, 1, "EQ leg must still hold its 1 lot");
    assert_eq!(fno.net_lots, 0, "FNO leg round-tripped back to flat");
}

/// `position_keys` exposes the full composite key so callers can address a
/// specific row; `position_security_ids` (legacy) drops the segment and so
/// reports the same numeric id twice.
#[test]
fn position_keys_expose_both_colliding_rows() {
    let mut risk = engine();

    risk.record_fill_in_segment(COLLIDING_ID, SEG_A, 1, 100.0, 1);
    risk.record_fill_in_segment(COLLIDING_ID, SEG_B, 1, 500.0, 1);

    let mut keys: Vec<_> = risk.position_keys().collect();
    keys.sort_by_key(|(id, seg)| (*id, seg.as_str()));

    assert_eq!(
        keys,
        vec![(COLLIDING_ID, SEG_B), (COLLIDING_ID, SEG_A)],
        "both colliding rows must be addressable by composite key"
    );

    let ids: Vec<u64> = risk.position_security_ids().collect();
    assert_eq!(
        ids.len(),
        2,
        "the legacy id-only iterator yields the shared id once per row"
    );
}

/// `reset_daily` must clear composite-keyed state completely — no row may
/// survive into the next trading day.
#[test]
fn reset_daily_clears_all_composite_rows() {
    let mut risk = engine();

    risk.record_fill_in_segment(COLLIDING_ID, SEG_A, 5, 100.0, 1);
    risk.record_fill_in_segment(COLLIDING_ID, SEG_B, -5, 500.0, 1);
    risk.update_market_price_in_segment(COLLIDING_ID, SEG_A, 120.0);
    risk.update_market_price_in_segment(COLLIDING_ID, SEG_B, 480.0);
    assert_eq!(risk.open_position_count(), 2);

    risk.reset_daily();

    assert_eq!(risk.open_position_count(), 0);
    assert_eq!(risk.net_lots_for_in_segment(COLLIDING_ID, SEG_A), 0);
    assert_eq!(risk.net_lots_for_in_segment(COLLIDING_ID, SEG_B), 0);
    assert!(risk.total_unrealized_pnl().abs() < 1e-9);
    assert_eq!(risk.position_keys().count(), 0);
}

/// The legacy segment-less API must remain behaviour-preserving for callers
/// that have not migrated: every segment-less call lands in ONE bucket, so a
/// legacy caller sees exactly the pre-refactor semantics.
///
/// This documents the migration shim honestly — it is NOT a claim that the
/// collision is fixed for legacy callers. It is not.
#[test]
fn legacy_segmentless_api_is_behaviour_preserving() {
    let mut risk = engine();

    risk.record_fill(COLLIDING_ID, 4, 100.0, 1);
    assert_eq!(risk.net_lots_for(COLLIDING_ID), 4);

    risk.update_market_price(COLLIDING_ID, 110.0);
    let pnl = risk.total_unrealized_pnl();
    assert!(
        (pnl - 40.0).abs() < 1e-9,
        "legacy path must still book 4 lots * (110 - 100) = 40, got {pnl}"
    );

    // And the legacy bucket is reachable through the composite API at the
    // documented default segment — proving the shim's mapping is not a
    // hidden third namespace.
    assert_eq!(
        risk.net_lots_for_in_segment(COLLIDING_ID, ExchangeSegment::IdxI),
        4,
        "legacy calls must land in the documented LEGACY_DEFAULT_SEGMENT bucket"
    );
}

// ---------------------------------------------------------------------------
// Per-function coverage for the segment-aware API.
//
// The tests above prove the COLLISION behaviour, which is what actually
// matters. These add one test NAMED for each new `*_in_segment` entry point.
// That is not ceremony: the pub-fn ratchet matches test names against function
// names, so a function covered only under a differently-named scenario test
// reads to the guard — and to a human grepping for it — as untested. Naming
// them makes the coverage discoverable instead of merely present.
// ---------------------------------------------------------------------------

#[test]
fn test_check_order_in_segment_isolates_the_two_colliding_segments() {
    let mut risk = RiskEngine::new(1_000_000.0, 5, 1_000_000.0);
    // Fill 5 lots on the FNO leg — exactly at the limit for THAT instrument.
    risk.record_fill_in_segment(COLLIDING_ID, ExchangeSegment::NseFno, 5, 100.0, 1);
    // The EQ leg shares the numeric id but must have its own empty book, so a
    // fresh 5-lot order there is approved on its own merits.
    assert!(
        risk.check_order_in_segment(COLLIDING_ID, ExchangeSegment::NseEquity, 5)
            .is_approved(),
        "the EQ leg must be judged on its own position, not the FNO leg's"
    );
    // …while the FNO leg itself is already full.
    assert!(
        !risk
            .check_order_in_segment(COLLIDING_ID, ExchangeSegment::NseFno, 5)
            .is_approved(),
        "the FNO leg is at max_position_lots and must refuse"
    );
}

#[test]
fn test_record_fill_in_segment_and_net_lots_for_in_segment_do_not_net() {
    let mut risk = RiskEngine::new(1_000_000.0, 100, 1_000_000.0);
    risk.record_fill_in_segment(COLLIDING_ID, ExchangeSegment::NseFno, 9, 100.0, 1);
    risk.record_fill_in_segment(COLLIDING_ID, ExchangeSegment::NseEquity, -9, 100.0, 1);
    // Pre-fix these netted to 0 in one bucket. They must stay separate.
    assert_eq!(
        risk.net_lots_for_in_segment(COLLIDING_ID, ExchangeSegment::NseFno),
        9
    );
    assert_eq!(
        risk.net_lots_for_in_segment(COLLIDING_ID, ExchangeSegment::NseEquity),
        -9
    );
}

#[test]
fn test_update_market_price_in_segment_does_not_cross_price_the_other_leg() {
    let mut risk = RiskEngine::new(1_000_000.0, 100, 1_000_000.0);
    risk.record_fill_in_segment(COLLIDING_ID, ExchangeSegment::NseFno, 1, 100.0, 1);
    // Mark ONLY the EQ leg, wildly away from the FNO entry.
    risk.update_market_price_in_segment(COLLIDING_ID, ExchangeSegment::NseEquity, 10_000.0);
    // The FNO leg has no mark of its own, so a conservative engine books
    // nothing for it — and must NOT borrow the EQ leg's price.
    let pnl = risk.total_unrealized_pnl();
    assert!(
        pnl.abs() < 1e-9,
        "an unmarked leg must contribute 0, not be priced from a colliding \
         segment's mark (got {pnl})"
    );
}

#[test]
fn test_position_in_segment_returns_the_right_leg() {
    let mut risk = RiskEngine::new(1_000_000.0, 100, 1_000_000.0);
    risk.record_fill_in_segment(COLLIDING_ID, ExchangeSegment::NseFno, 3, 100.0, 1);
    assert!(
        risk.position_in_segment(COLLIDING_ID, ExchangeSegment::NseFno)
            .is_some(),
        "the filled leg must be present"
    );
    assert!(
        risk.position_in_segment(COLLIDING_ID, ExchangeSegment::NseEquity)
            .is_none(),
        "the untouched leg must be absent, not aliased to the filled one"
    );
}
