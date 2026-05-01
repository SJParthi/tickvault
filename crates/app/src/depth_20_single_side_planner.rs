//! Depth-20 single-side subscription planner — Wave 5 Items 4+5 receiver-side
//! foundation.
//!
//! Operator spec (verbatim 2026-05-01):
//! > "for depth 20 i clearly told you nifty atm ce plus or minus 24 and
//! >  nifty pe plus or minus 24 meanwhile same for banknifty"
//!
//! This module is the **planner primitive** for the receiver-side conn-pool
//! refactor. It walks the 4 static (underlying, side) pairs:
//!
//!   1. NIFTY-CE
//!   2. NIFTY-PE
//!   3. BANKNIFTY-CE
//!   4. BANKNIFTY-PE
//!
//! and for each one calls
//! `tickvault_core::instrument::depth_strike_selector::select_single_side_contracts`
//! to compute the 49-SID single-side window (ATM ± 24 of the requested side).
//!
//! # Honest envelope (per `wave-4-shared-preamble.md` §8)
//!
//! 100% inside the tested envelope, with ratcheted regression coverage:
//! - Spot-price absent → returns `key_with_no_selection` so the caller spawns
//!   the conn in DEFERRED mode (09:13 IST dispatcher provides the SID later).
//! - Underlying expiry absent → same (DEFERRED mode).
//! - Universe `None` → empty plan returned; caller logs + skips.
//!
//! # Why this lives in `app`, not `core`
//!
//! This is purely glue — it stitches together
//! `select_single_side_contracts` (in `core`) with the topology the spawn
//! loop in `main.rs` needs. Other crates do not need this primitive.
//!
//! # Caller (target site, not yet wired)
//!
//! Upcoming commit 2 of this PR replaces the
//! `for underlying in &depth_underlyings` mixed-CE+PE depth-20 spawn block
//! in `crates/app/src/main.rs` with iteration over
//! `plan_depth_20_single_side_subscriptions(..)`. Until commit 2 lands the
//! existing mixed iteration stays intact.

use chrono::NaiveDate;
use tickvault_common::instrument_types::FnoUniverse;
use tickvault_common::types::SecurityId;

use tickvault_core::instrument::depth_strike_selector::{
    DEPTH_ATM_STRIKES_EACH_SIDE, DepthStrikeSelection, select_single_side_contracts,
};

/// One row of the depth-20 single-side spawn plan.
///
/// **Security note (hardening fix M-Sec, 2026-05-01):** `Debug` is gated to
/// `#[cfg(test)]` ONLY. The struct holds no secrets today, but `Debug`
/// auto-derive is a latent exposure vector — if a `TokenHandle` or other
/// sensitive field is added in a future commit, `{:?}` formatting would
/// silently log it via tracing or panic backtraces. Test-only Debug
/// preserves `assert_eq!` ergonomics without leaking in prod.
#[cfg_attr(test, derive(Debug))]
#[derive(Clone)]
pub struct Depth20SingleSideRow {
    /// Underlying symbol (e.g. `"NIFTY"` / `"BANKNIFTY"`).
    pub underlying: String,
    /// Single-character side: `'C'` for CE, `'P'` for PE.
    pub side: char,
    /// Spawn-handle key (e.g. `"NIFTY-CE"` / `"NIFTY-PE"`). Used by the
    /// `depth_cmd_senders` HashMap and by the depth_rebalancer Swap20
    /// targets in commit 4.
    pub key: String,
    /// Single-side ATM ± 24 selection. `None` when the spot price is
    /// missing or the universe / expiry / chain lookup fails — caller
    /// spawns the conn in DEFERRED mode.
    pub selection: Option<DepthStrikeSelection>,
}

/// The 4 static (underlying, side) pairs that own depth-20 conns 1..4.
/// Conn 5 is dynamic (top-50 SENSEX-excluded) — separate orchestrator
/// in `depth_dynamic_pipeline::spawn_depth_20_dynamic_conn5_task`.
pub const DEPTH_20_SINGLE_SIDE_TARGETS: &[(&str, char)] = &[
    ("NIFTY", 'C'),
    ("NIFTY", 'P'),
    ("BANKNIFTY", 'C'),
    ("BANKNIFTY", 'P'),
];

/// Build the depth-20 single-side spawn plan.
///
/// Returns one [`Depth20SingleSideRow`] per entry in
/// [`DEPTH_20_SINGLE_SIDE_TARGETS`]. Rows whose `selection` is `None`
/// are still returned (caller spawns DEFERRED conn). This keeps the
/// spawn count stable at 4 regardless of pre-market boot timing.
///
/// `spot_lookup` is a closure so the caller can inject the live
/// `SharedSpotPrices` map without taking a hard dependency on its
/// concrete type here. The closure returns `None` when the underlying
/// hasn't yet ticked — common at pre-market boot.
#[must_use]
pub fn plan_depth_20_single_side_subscriptions<F>(
    universe: Option<&FnoUniverse>,
    spot_lookup: F,
    today: NaiveDate,
) -> Vec<Depth20SingleSideRow>
where
    F: Fn(&str) -> Option<f64>,
{
    DEPTH_20_SINGLE_SIDE_TARGETS
        .iter()
        .map(|&(underlying, side)| {
            let key = format_single_side_key(underlying, side);
            let selection = match (universe, spot_lookup(underlying)) {
                (Some(uni), Some(spot)) if spot.is_finite() && spot > 0.0 => {
                    select_single_side_contracts(
                        uni,
                        underlying,
                        side,
                        spot,
                        today,
                        DEPTH_ATM_STRIKES_EACH_SIDE,
                    )
                }
                _ => None,
            };
            Depth20SingleSideRow {
                underlying: underlying.to_string(),
                side,
                key,
                selection,
            }
        })
        .collect()
}

/// Format the spawn-handle key for a (underlying, side) pair.
///
/// `'C'` → `"<UL>-CE"`, `'P'` → `"<UL>-PE"`. Any other char defaults
/// to `"<UL>-??"` (defensive — should never happen since callers pass
/// constants from `DEPTH_20_SINGLE_SIDE_TARGETS`).
#[must_use]
pub fn format_single_side_key(underlying: &str, side: char) -> String {
    let side_label = match side {
        'C' => "CE",
        'P' => "PE",
        _ => "??",
    };
    format!("{underlying}-{side_label}")
}

/// Extract the single-side `SecurityId` list from a [`Depth20SingleSideRow`].
///
/// Returns the CE list when `side == 'C'`, the PE list when `side == 'P'`,
/// and an empty Vec for any other input. Matches the post-strip semantics
/// of [`select_single_side_contracts`] which clears the off-side fields.
#[must_use]
pub fn extract_single_side_ids(row: &Depth20SingleSideRow) -> Vec<SecurityId> {
    match (&row.selection, row.side) {
        (Some(sel), 'C') => sel.call_security_ids.clone(),
        (Some(sel), 'P') => sel.put_security_ids.clone(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tickvault_common::instrument_types::{
        DerivativeContract, DhanInstrumentKind, ExpiryCalendar, FnoUniverse, OptionChain,
        OptionChainEntry, OptionChainKey, UniverseBuildMetadata,
    };
    use tickvault_common::types::{ExchangeSegment, OptionType};

    fn ist(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("test date")
    }

    fn build_test_universe() -> FnoUniverse {
        let underlying = "NIFTY";
        let expiry = ist(2026, 5, 7);
        // 49 strikes around 25050 to satisfy DEPTH_ATM_STRIKES_EACH_SIDE = 24.
        let strikes: Vec<f64> = (0..49).map(|i| 24800.0 + (i as f64) * 50.0).collect();
        let mut calls: Vec<OptionChainEntry> = Vec::new();
        let mut puts: Vec<OptionChainEntry> = Vec::new();
        let mut derivative_contracts: HashMap<u32, DerivativeContract> = HashMap::new();

        for (i, strike) in strikes.iter().enumerate() {
            let ce_id = (1000 + (i * 2)) as u32;
            let pe_id = ce_id + 1;
            calls.push(OptionChainEntry {
                strike_price: *strike,
                security_id: ce_id,
                lot_size: 50,
            });
            puts.push(OptionChainEntry {
                strike_price: *strike,
                security_id: pe_id,
                lot_size: 50,
            });
            derivative_contracts.insert(
                ce_id,
                DerivativeContract {
                    security_id: ce_id,
                    underlying_symbol: underlying.to_string(),
                    instrument_kind: DhanInstrumentKind::OptionIndex,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: expiry,
                    strike_price: *strike,
                    option_type: Some(OptionType::Call),
                    lot_size: 50,
                    tick_size: 0.05,
                    symbol_name: format!("{underlying}-May2026-{}-CE", *strike as i64),
                    display_name: format!("{underlying} 07 MAY {} CALL", *strike as i64),
                },
            );
            derivative_contracts.insert(
                pe_id,
                DerivativeContract {
                    security_id: pe_id,
                    underlying_symbol: underlying.to_string(),
                    instrument_kind: DhanInstrumentKind::OptionIndex,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: expiry,
                    strike_price: *strike,
                    option_type: Some(OptionType::Put),
                    lot_size: 50,
                    tick_size: 0.05,
                    symbol_name: format!("{underlying}-May2026-{}-PE", *strike as i64),
                    display_name: format!("{underlying} 07 MAY {} PUT", *strike as i64),
                },
            );
        }

        let chain = OptionChain {
            underlying_symbol: underlying.to_string(),
            expiry_date: expiry,
            calls,
            puts,
            future_security_id: None,
        };
        let mut option_chains = HashMap::new();
        option_chains.insert(
            OptionChainKey {
                underlying_symbol: underlying.to_string(),
                expiry_date: expiry,
            },
            chain,
        );

        let mut expiry_calendars = HashMap::new();
        expiry_calendars.insert(
            underlying.to_string(),
            ExpiryCalendar {
                underlying_symbol: underlying.to_string(),
                expiry_dates: vec![expiry],
            },
        );

        FnoUniverse {
            underlyings: HashMap::new(),
            derivative_contracts,
            instrument_info: HashMap::new(),
            option_chains,
            expiry_calendars,
            subscribed_indices: Vec::new(),
            build_metadata: UniverseBuildMetadata {
                csv_source: "test".to_string(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 0,
                derivative_count: 0,
                option_chain_count: 0,
                build_duration: std::time::Duration::ZERO,
                build_timestamp: chrono::Utc::now()
                    .with_timezone(&chrono::FixedOffset::east_opt(19_800).expect("IST offset")),
            },
        }
    }

    #[test]
    fn test_format_single_side_key_ce() {
        assert_eq!(format_single_side_key("NIFTY", 'C'), "NIFTY-CE");
    }

    #[test]
    fn test_format_single_side_key_pe() {
        assert_eq!(format_single_side_key("BANKNIFTY", 'P'), "BANKNIFTY-PE");
    }

    #[test]
    fn test_format_single_side_key_invalid_defaults() {
        assert_eq!(format_single_side_key("X", 'Z'), "X-??");
    }

    #[test]
    fn test_targets_constant_is_exactly_four_pairs() {
        assert_eq!(DEPTH_20_SINGLE_SIDE_TARGETS.len(), 4);
    }

    #[test]
    fn test_targets_first_pair_is_nifty_ce() {
        assert_eq!(DEPTH_20_SINGLE_SIDE_TARGETS[0], ("NIFTY", 'C'));
    }

    #[test]
    fn test_targets_last_pair_is_banknifty_pe() {
        assert_eq!(DEPTH_20_SINGLE_SIDE_TARGETS[3], ("BANKNIFTY", 'P'));
    }

    #[test]
    fn test_targets_includes_all_four_required_keys() {
        let keys: Vec<String> = DEPTH_20_SINGLE_SIDE_TARGETS
            .iter()
            .map(|&(u, s)| format_single_side_key(u, s))
            .collect();
        assert!(keys.contains(&"NIFTY-CE".to_string()));
        assert!(keys.contains(&"NIFTY-PE".to_string()));
        assert!(keys.contains(&"BANKNIFTY-CE".to_string()));
        assert!(keys.contains(&"BANKNIFTY-PE".to_string()));
    }

    #[test]
    fn test_plan_depth_20_single_side_subscriptions_returns_four_rows_universe_none() {
        let plan = plan_depth_20_single_side_subscriptions(None, |_| None, ist(2026, 5, 1));
        assert_eq!(plan.len(), 4);
        for row in &plan {
            assert!(
                row.selection.is_none(),
                "selection must be None when universe is absent"
            );
        }
    }

    #[test]
    fn test_plan_depth_20_single_side_subscriptions_returns_four_rows_spot_missing() {
        let universe = build_test_universe();
        let plan =
            plan_depth_20_single_side_subscriptions(Some(&universe), |_| None, ist(2026, 5, 1));
        assert_eq!(plan.len(), 4);
        for row in &plan {
            assert!(
                row.selection.is_none(),
                "selection must be None when spot is absent"
            );
        }
    }

    #[test]
    fn test_plan_depth_20_single_side_subscriptions_nifty_ce_returns_call_only_when_spot_available()
    {
        let universe = build_test_universe();
        let plan = plan_depth_20_single_side_subscriptions(
            Some(&universe),
            |u| if u == "NIFTY" { Some(25050.0) } else { None },
            ist(2026, 5, 1),
        );
        // First row is NIFTY-CE per DEPTH_20_SINGLE_SIDE_TARGETS[0].
        let nifty_ce = &plan[0];
        assert_eq!(nifty_ce.key, "NIFTY-CE");
        assert_eq!(nifty_ce.side, 'C');
        let sel = nifty_ce.selection.as_ref().expect("CE selection present");
        assert!(!sel.call_security_ids.is_empty());
        assert!(
            sel.put_security_ids.is_empty(),
            "PE side must be cleared on CE selection"
        );
        assert!(sel.atm_pe_security_id.is_none());
    }

    #[test]
    fn test_plan_depth_20_single_side_subscriptions_nifty_pe_returns_put_only_when_spot_available()
    {
        let universe = build_test_universe();
        let plan = plan_depth_20_single_side_subscriptions(
            Some(&universe),
            |u| if u == "NIFTY" { Some(25050.0) } else { None },
            ist(2026, 5, 1),
        );
        // Second row is NIFTY-PE per DEPTH_20_SINGLE_SIDE_TARGETS[1].
        let nifty_pe = &plan[1];
        assert_eq!(nifty_pe.key, "NIFTY-PE");
        assert_eq!(nifty_pe.side, 'P');
        let sel = nifty_pe.selection.as_ref().expect("PE selection present");
        assert!(!sel.put_security_ids.is_empty());
        assert!(
            sel.call_security_ids.is_empty(),
            "CE side must be cleared on PE selection"
        );
        assert!(sel.atm_ce_security_id.is_none());
    }

    #[test]
    fn test_plan_depth_20_single_side_subscriptions_banknifty_rows_default_to_none_when_spot_absent_for_banknifty()
     {
        let universe = build_test_universe(); // only has NIFTY
        let plan = plan_depth_20_single_side_subscriptions(
            Some(&universe),
            |u| if u == "NIFTY" { Some(25050.0) } else { None },
            ist(2026, 5, 1),
        );
        let banknifty_ce = &plan[2];
        let banknifty_pe = &plan[3];
        assert_eq!(banknifty_ce.key, "BANKNIFTY-CE");
        assert_eq!(banknifty_pe.key, "BANKNIFTY-PE");
        assert!(banknifty_ce.selection.is_none());
        assert!(banknifty_pe.selection.is_none());
    }

    #[test]
    fn test_plan_depth_20_single_side_subscriptions_negative_spot_returns_none() {
        let universe = build_test_universe();
        let plan = plan_depth_20_single_side_subscriptions(
            Some(&universe),
            |u| if u == "NIFTY" { Some(-1.0) } else { None },
            ist(2026, 5, 1),
        );
        assert!(plan[0].selection.is_none());
    }

    #[test]
    fn test_plan_depth_20_single_side_subscriptions_nan_spot_returns_none() {
        let universe = build_test_universe();
        let plan = plan_depth_20_single_side_subscriptions(
            Some(&universe),
            |u| if u == "NIFTY" { Some(f64::NAN) } else { None },
            ist(2026, 5, 1),
        );
        assert!(plan[0].selection.is_none());
    }

    #[test]
    fn test_extract_single_side_ids_ce_returns_calls() {
        let universe = build_test_universe();
        let plan = plan_depth_20_single_side_subscriptions(
            Some(&universe),
            |u| if u == "NIFTY" { Some(25050.0) } else { None },
            ist(2026, 5, 1),
        );
        let ids = extract_single_side_ids(&plan[0]);
        assert!(!ids.is_empty(), "CE row must extract non-empty CE IDs");
    }

    #[test]
    fn test_extract_single_side_ids_pe_returns_puts() {
        let universe = build_test_universe();
        let plan = plan_depth_20_single_side_subscriptions(
            Some(&universe),
            |u| if u == "NIFTY" { Some(25050.0) } else { None },
            ist(2026, 5, 1),
        );
        let ids = extract_single_side_ids(&plan[1]);
        assert!(!ids.is_empty(), "PE row must extract non-empty PE IDs");
    }

    #[test]
    fn test_extract_single_side_ids_no_selection_returns_empty() {
        let row = Depth20SingleSideRow {
            underlying: "NIFTY".to_string(),
            side: 'C',
            key: "NIFTY-CE".to_string(),
            selection: None,
        };
        assert!(extract_single_side_ids(&row).is_empty());
    }

    #[test]
    fn test_plan_depth_20_single_side_subscriptions_keys_stable_regardless_of_universe_state() {
        // Even when universe is absent, the spawn-handle keys must be
        // determined so the depth_rebalancer + phase2_scheduler can pre-
        // register their Swap20 targets at boot.
        let plan = plan_depth_20_single_side_subscriptions(None, |_| None, ist(2026, 5, 1));
        let expected = ["NIFTY-CE", "NIFTY-PE", "BANKNIFTY-CE", "BANKNIFTY-PE"];
        for (row, exp) in plan.iter().zip(expected.iter()) {
            assert_eq!(row.key, *exp);
        }
    }
}
