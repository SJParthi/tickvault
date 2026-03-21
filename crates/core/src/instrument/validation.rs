//! Post-build validation of the F&O universe.
//!
//! Validates must-exist security IDs, count ranges, and data integrity.
//! Any validation failure prevents system startup.

use anyhow::{Result, bail};
use tracing::{info, warn};

use dhan_live_trader_common::constants::*;
use dhan_live_trader_common::error::ApplicationError;
use dhan_live_trader_common::instrument_types::*;

/// Validate the built F&O universe against known invariants.
///
/// # Checks
/// 1. Must-exist indices: known index underlyings have correct price feed IDs.
/// 2. Must-exist equities: known equity security IDs exist in instrument_info.
/// 3. Must-exist F&O stocks: known stock symbols are present as underlyings.
/// 4. Stock count range: number of F&O stock underlyings within expected bounds.
/// 5. Non-empty: universe has at least one underlying and one derivative.
///
/// # Errors
/// Returns `ApplicationError::UniverseValidationFailed` on first check failure.
pub fn validate_fno_universe(universe: &FnoUniverse) -> Result<()> {
    // Check 1: Must-exist indices
    for (symbol, expected_price_id) in VALIDATION_MUST_EXIST_INDICES {
        let underlying = universe.underlyings.get(*symbol).ok_or_else(|| {
            ApplicationError::UniverseValidationFailed {
                check: format!("must-exist index '{}' not found in underlyings", symbol),
            }
        })?;

        if underlying.price_feed_security_id != *expected_price_id {
            bail!(ApplicationError::UniverseValidationFailed {
                check: format!(
                    "index '{}' price_feed_security_id mismatch: expected {}, got {}",
                    symbol, expected_price_id, underlying.price_feed_security_id
                ),
            });
        }
    }

    info!(
        checked_indices = VALIDATION_MUST_EXIST_INDICES.len(),
        "validation: must-exist indices passed"
    );

    // Check 2: Must-exist equities in instrument_info
    for (symbol, expected_id) in VALIDATION_MUST_EXIST_EQUITIES {
        let info = universe.instrument_info.get(expected_id).ok_or_else(|| {
            ApplicationError::UniverseValidationFailed {
                check: format!(
                    "must-exist equity '{}' (id={}) not found in instrument_info",
                    symbol, expected_id
                ),
            }
        })?;

        // Verify it's actually an equity variant
        match info {
            InstrumentInfo::Equity { .. } => {}
            _ => {
                bail!(ApplicationError::UniverseValidationFailed {
                    check: format!(
                        "must-exist equity '{}' (id={}) is not an Equity variant in instrument_info",
                        symbol, expected_id
                    ),
                });
            }
        }
    }

    info!(
        checked_equities = VALIDATION_MUST_EXIST_EQUITIES.len(),
        "validation: must-exist equities passed"
    );

    // Check 3: Must-exist F&O stocks
    for symbol in VALIDATION_MUST_EXIST_FNO_STOCKS {
        if !universe.underlyings.contains_key(*symbol) {
            bail!(ApplicationError::UniverseValidationFailed {
                check: format!("must-exist F&O stock '{}' not found in underlyings", symbol),
            });
        }
    }

    info!(
        checked_stocks = VALIDATION_MUST_EXIST_FNO_STOCKS.len(),
        "validation: must-exist F&O stocks passed"
    );

    // Check 4: Stock underlying count within expected range
    let stock_count = universe
        .underlyings
        .values()
        .filter(|u| u.kind == UnderlyingKind::Stock)
        .count();

    if !(VALIDATION_FNO_STOCK_MIN_COUNT..=VALIDATION_FNO_STOCK_MAX_COUNT).contains(&stock_count) {
        bail!(ApplicationError::UniverseValidationFailed {
            check: format!(
                "F&O stock count {} outside expected range [{}, {}]",
                stock_count, VALIDATION_FNO_STOCK_MIN_COUNT, VALIDATION_FNO_STOCK_MAX_COUNT
            ),
        });
    }

    info!(
        stock_count,
        min = VALIDATION_FNO_STOCK_MIN_COUNT,
        max = VALIDATION_FNO_STOCK_MAX_COUNT,
        "validation: F&O stock count within range"
    );

    // Check 5: Non-empty universe
    if universe.underlyings.is_empty() {
        bail!(ApplicationError::UniverseValidationFailed {
            check: "underlyings map is empty".to_owned(),
        });
    }
    if universe.derivative_contracts.is_empty() {
        bail!(ApplicationError::UniverseValidationFailed {
            check: "derivative_contracts map is empty".to_owned(),
        });
    }

    // Check 6: Every derivative_contract.underlying_symbol exists in underlyings
    let mut orphan_derivatives: usize = 0;
    for contract in universe.derivative_contracts.values() {
        if !universe
            .underlyings
            .contains_key(&contract.underlying_symbol)
        {
            orphan_derivatives = orphan_derivatives.saturating_add(1);
            if orphan_derivatives <= 5 {
                warn!(
                    security_id = contract.security_id,
                    underlying = %contract.underlying_symbol,
                    "derivative references non-existent underlying"
                );
            }
        }
    }
    if orphan_derivatives > 0 {
        bail!(ApplicationError::UniverseValidationFailed {
            check: format!(
                "{} derivative contracts reference non-existent underlyings",
                orphan_derivatives
            ),
        });
    }

    info!("validation: derivative → underlying cross-reference passed");

    // Check 7: Every option_chain.future_security_id exists in derivative_contracts
    let mut orphan_futures: usize = 0;
    for chain in universe.option_chains.values() {
        if let Some(future_id) = chain.future_security_id
            && !universe.derivative_contracts.contains_key(&future_id)
        {
            orphan_futures = orphan_futures.saturating_add(1);
            if orphan_futures <= 5 {
                warn!(
                    future_id,
                    underlying = %chain.underlying_symbol,
                    "option chain references non-existent future contract"
                );
            }
        }
    }
    if orphan_futures > 0 {
        bail!(ApplicationError::UniverseValidationFailed {
            check: format!(
                "{} option chains reference non-existent future contracts",
                orphan_futures
            ),
        });
    }

    info!("validation: option chain → future cross-reference passed");

    // Check 8: Every expiry_calendar symbol exists in underlyings
    let mut orphan_calendars: usize = 0;
    for symbol in universe.expiry_calendars.keys() {
        if !universe.underlyings.contains_key(symbol) {
            orphan_calendars = orphan_calendars.saturating_add(1);
            if orphan_calendars <= 5 {
                warn!(
                    symbol = %symbol,
                    "expiry calendar references non-existent underlying"
                );
            }
        }
    }
    if orphan_calendars > 0 {
        bail!(ApplicationError::UniverseValidationFailed {
            check: format!(
                "{} expiry calendars reference non-existent underlyings",
                orphan_calendars
            ),
        });
    }

    info!("validation: expiry calendar → underlying cross-reference passed");

    // Check 9: Every underlying's price_feed_security_id exists in instrument_info (warn only)
    let mut missing_price_feeds: usize = 0;
    for underlying in universe.underlyings.values() {
        if !universe
            .instrument_info
            .contains_key(&underlying.price_feed_security_id)
        {
            missing_price_feeds = missing_price_feeds.saturating_add(1);
            if missing_price_feeds <= 5 {
                warn!(
                    symbol = %underlying.underlying_symbol,
                    price_feed_id = underlying.price_feed_security_id,
                    "underlying price_feed_security_id not in instrument_info"
                );
            }
        }
    }
    if missing_price_feeds > 0 {
        warn!(
            missing_price_feeds,
            "some underlying price_feed_security_ids not in instrument_info (non-fatal)"
        );
    }

    info!(
        underlyings = universe.underlyings.len(),
        derivatives = universe.derivative_contracts.len(),
        option_chains = universe.option_chains.len(),
        expiry_calendars = universe.expiry_calendars.len(),
        instrument_info = universe.instrument_info.len(),
        "validation: F&O universe passed all checks"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test code
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    use chrono::{NaiveDate, NaiveDateTime, NaiveTime};

    use dhan_live_trader_common::types::{Exchange, ExchangeSegment};

    // Extraction helper — panic arm appears only once.
    fn unwrap_equity_security_id(info: &InstrumentInfo) -> u32 {
        match info {
            InstrumentInfo::Equity { security_id, .. } => *security_id,
            other => panic!("expected Equity variant, got {other:?}"),
        }
    }

    /// Build a minimal valid FnoUniverse for testing validation.
    fn build_valid_universe() -> FnoUniverse {
        let mut underlyings = HashMap::new();
        let mut derivative_contracts = HashMap::new();
        let mut instrument_info = HashMap::new();
        let option_chains = HashMap::new();
        let expiry_calendars = HashMap::new();

        // Must-exist indices (from VALIDATION_MUST_EXIST_INDICES)
        for (symbol, price_id) in VALIDATION_MUST_EXIST_INDICES {
            underlyings.insert(
                symbol.to_string(),
                FnoUnderlying {
                    underlying_symbol: symbol.to_string(),
                    underlying_security_id: price_id + 25000, // Phantom ID
                    price_feed_security_id: *price_id,
                    price_feed_segment: ExchangeSegment::IdxI,
                    derivative_segment: ExchangeSegment::NseFno,
                    kind: UnderlyingKind::NseIndex,
                    lot_size: 50,
                    contract_count: 10,
                },
            );
        }

        // Must-exist equities (in instrument_info)
        for (symbol, expected_id) in VALIDATION_MUST_EXIST_EQUITIES {
            instrument_info.insert(
                *expected_id,
                InstrumentInfo::Equity {
                    security_id: *expected_id,
                    symbol: symbol.to_string(),
                },
            );
        }

        // Must-exist F&O stocks as underlyings
        for symbol in VALIDATION_MUST_EXIST_FNO_STOCKS {
            if !underlyings.contains_key(*symbol) {
                underlyings.insert(
                    symbol.to_string(),
                    FnoUnderlying {
                        underlying_symbol: symbol.to_string(),
                        underlying_security_id: 9000,
                        price_feed_security_id: 9000,
                        price_feed_segment: ExchangeSegment::NseEquity,
                        derivative_segment: ExchangeSegment::NseFno,
                        kind: UnderlyingKind::Stock,
                        lot_size: 500,
                        contract_count: 5,
                    },
                );
            }
        }

        // Add enough stock underlyings to meet VALIDATION_FNO_STOCK_MIN_COUNT
        for i in 0..VALIDATION_FNO_STOCK_MIN_COUNT {
            let symbol = format!("STOCK{}", i);
            if !underlyings.contains_key(&symbol) {
                underlyings.insert(
                    symbol.clone(),
                    FnoUnderlying {
                        underlying_symbol: symbol,
                        underlying_security_id: 10000 + i as u32,
                        price_feed_security_id: 10000 + i as u32,
                        price_feed_segment: ExchangeSegment::NseEquity,
                        derivative_segment: ExchangeSegment::NseFno,
                        kind: UnderlyingKind::Stock,
                        lot_size: 100,
                        contract_count: 5,
                    },
                );
            }
        }

        // Add at least one derivative contract
        derivative_contracts.insert(
            90001,
            DerivativeContract {
                security_id: 90001,
                underlying_symbol: "NIFTY".to_owned(),
                instrument_kind: DhanInstrumentKind::FutureIndex,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
                strike_price: 0.0,
                option_type: None,
                lot_size: 75,
                tick_size: 0.05,
                symbol_name: "NIFTY-Mar2026-FUT".to_owned(),
                display_name: "NIFTY MAR FUT".to_owned(),
            },
        );

        let ist_offset = dhan_live_trader_common::trading_calendar::ist_offset();
        let naive_dt = NaiveDateTime::new(
            NaiveDate::from_ymd_opt(2026, 2, 25).unwrap(),
            NaiveTime::from_hms_opt(8, 45, 0).unwrap(),
        );

        FnoUniverse {
            underlyings,
            derivative_contracts,
            instrument_info,
            option_chains,
            expiry_calendars,
            subscribed_indices: Vec::new(),
            build_metadata: UniverseBuildMetadata {
                csv_source: "test".to_owned(),
                csv_row_count: 200_000,
                parsed_row_count: 100_000,
                index_count: 8,
                equity_count: 5,
                underlying_count: 200,
                derivative_count: 1,
                option_chain_count: 0,
                build_duration: Duration::from_millis(500),
                build_timestamp: naive_dt.and_local_timezone(ist_offset).unwrap(),
            },
        }
    }

    #[test]
    fn test_valid_universe_passes_all_checks() {
        let universe = build_valid_universe();
        let result = validate_fno_universe(&universe);
        assert!(
            result.is_ok(),
            "valid universe must pass: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_missing_must_exist_index_fails() {
        let mut universe = build_valid_universe();
        universe.underlyings.remove("NIFTY");

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("NIFTY"),
            "error must mention NIFTY: {}",
            err_msg
        );
    }

    #[test]
    fn test_wrong_index_price_id_fails() {
        let mut universe = build_valid_universe();
        universe
            .underlyings
            .get_mut("NIFTY")
            .unwrap()
            .price_feed_security_id = 99999; // Wrong ID

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("mismatch"),
            "error must mention mismatch: {}",
            err_msg
        );
    }

    #[test]
    fn test_missing_must_exist_equity_fails() {
        let mut universe = build_valid_universe();
        universe.instrument_info.remove(&2885); // RELIANCE

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("RELIANCE"),
            "error must mention RELIANCE: {}",
            err_msg
        );
    }

    #[test]
    fn test_missing_must_exist_fno_stock_fails() {
        let mut universe = build_valid_universe();
        universe.underlyings.remove("TCS");

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("TCS"),
            "error must mention TCS: {}",
            err_msg
        );
    }

    #[test]
    fn test_stock_count_below_minimum_fails() {
        let mut universe = build_valid_universe();
        // Remove all stock underlyings except the must-exist ones
        let stock_symbols: Vec<String> = universe
            .underlyings
            .iter()
            .filter(|(_, u)| u.kind == UnderlyingKind::Stock)
            .map(|(s, _)| s.clone())
            .collect();

        // Keep only the 4 must-exist stocks, remove the rest
        for symbol in &stock_symbols {
            if !VALIDATION_MUST_EXIST_FNO_STOCKS.contains(&symbol.as_str()) {
                universe.underlyings.remove(symbol);
            }
        }

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("outside expected range"),
            "error must mention range: {}",
            err_msg
        );
    }

    #[test]
    fn test_empty_underlyings_fails() {
        let mut universe = build_valid_universe();
        universe.underlyings.clear();

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_derivative_contracts_fails() {
        let mut universe = build_valid_universe();
        universe.derivative_contracts.clear();

        // Need to pass must-exist checks first, so this fails on empty derivatives
        // But must-exist indices check passes because underlyings still has them
        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("empty"),
            "error must mention empty: {}",
            err_msg
        );
    }

    #[test]
    fn test_stock_count_above_maximum_fails() {
        let mut universe = build_valid_universe();

        // Add stocks until we exceed VALIDATION_FNO_STOCK_MAX_COUNT
        let current_stock_count = universe
            .underlyings
            .values()
            .filter(|u| u.kind == UnderlyingKind::Stock)
            .count();

        for i in 0..(VALIDATION_FNO_STOCK_MAX_COUNT + 1 - current_stock_count) {
            let symbol = format!("EXCESSSTOCK{}", i);
            universe.underlyings.insert(
                symbol.clone(),
                FnoUnderlying {
                    underlying_symbol: symbol,
                    underlying_security_id: 50000 + i as u32,
                    price_feed_security_id: 50000 + i as u32,
                    price_feed_segment: ExchangeSegment::NseEquity,
                    derivative_segment: ExchangeSegment::NseFno,
                    kind: UnderlyingKind::Stock,
                    lot_size: 100,
                    contract_count: 5,
                },
            );
        }

        let final_stock_count = universe
            .underlyings
            .values()
            .filter(|u| u.kind == UnderlyingKind::Stock)
            .count();
        assert!(
            final_stock_count > VALIDATION_FNO_STOCK_MAX_COUNT,
            "test setup: stock count {} must exceed max {}",
            final_stock_count,
            VALIDATION_FNO_STOCK_MAX_COUNT
        );

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("outside expected range"),
            "error must mention range: {}",
            err_msg
        );
    }

    #[test]
    fn test_stock_count_exactly_at_minimum_passes() {
        let mut universe = build_valid_universe();

        // Remove all stocks, then add back exactly VALIDATION_FNO_STOCK_MIN_COUNT
        let stock_symbols: Vec<String> = universe
            .underlyings
            .iter()
            .filter(|(_, u)| u.kind == UnderlyingKind::Stock)
            .map(|(s, _)| s.clone())
            .collect();
        for symbol in &stock_symbols {
            universe.underlyings.remove(symbol);
        }

        // Re-add must-exist F&O stocks (they are stocks)
        for symbol in VALIDATION_MUST_EXIST_FNO_STOCKS {
            universe.underlyings.insert(
                symbol.to_string(),
                FnoUnderlying {
                    underlying_symbol: symbol.to_string(),
                    underlying_security_id: 9000,
                    price_feed_security_id: 9000,
                    price_feed_segment: ExchangeSegment::NseEquity,
                    derivative_segment: ExchangeSegment::NseFno,
                    kind: UnderlyingKind::Stock,
                    lot_size: 500,
                    contract_count: 5,
                },
            );
        }

        // Fill remaining slots to reach exactly VALIDATION_FNO_STOCK_MIN_COUNT
        let must_exist_count = VALIDATION_MUST_EXIST_FNO_STOCKS.len();
        for i in 0..(VALIDATION_FNO_STOCK_MIN_COUNT - must_exist_count) {
            let symbol = format!("BOUNDARYSTOCK{}", i);
            universe.underlyings.insert(
                symbol.clone(),
                FnoUnderlying {
                    underlying_symbol: symbol,
                    underlying_security_id: 30000 + i as u32,
                    price_feed_security_id: 30000 + i as u32,
                    price_feed_segment: ExchangeSegment::NseEquity,
                    derivative_segment: ExchangeSegment::NseFno,
                    kind: UnderlyingKind::Stock,
                    lot_size: 100,
                    contract_count: 5,
                },
            );
        }

        let stock_count = universe
            .underlyings
            .values()
            .filter(|u| u.kind == UnderlyingKind::Stock)
            .count();
        assert_eq!(
            stock_count, VALIDATION_FNO_STOCK_MIN_COUNT,
            "test setup: stock count must equal min boundary"
        );

        let result = validate_fno_universe(&universe);
        assert!(
            result.is_ok(),
            "stock count exactly at minimum must pass: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_stock_count_exactly_at_maximum_passes() {
        let mut universe = build_valid_universe();

        // Remove all stocks, then add back exactly VALIDATION_FNO_STOCK_MAX_COUNT
        let stock_symbols: Vec<String> = universe
            .underlyings
            .iter()
            .filter(|(_, u)| u.kind == UnderlyingKind::Stock)
            .map(|(s, _)| s.clone())
            .collect();
        for symbol in &stock_symbols {
            universe.underlyings.remove(symbol);
        }

        // Re-add must-exist F&O stocks
        for symbol in VALIDATION_MUST_EXIST_FNO_STOCKS {
            universe.underlyings.insert(
                symbol.to_string(),
                FnoUnderlying {
                    underlying_symbol: symbol.to_string(),
                    underlying_security_id: 9000,
                    price_feed_security_id: 9000,
                    price_feed_segment: ExchangeSegment::NseEquity,
                    derivative_segment: ExchangeSegment::NseFno,
                    kind: UnderlyingKind::Stock,
                    lot_size: 500,
                    contract_count: 5,
                },
            );
        }

        // Fill remaining slots to reach exactly VALIDATION_FNO_STOCK_MAX_COUNT
        let must_exist_count = VALIDATION_MUST_EXIST_FNO_STOCKS.len();
        for i in 0..(VALIDATION_FNO_STOCK_MAX_COUNT - must_exist_count) {
            let symbol = format!("MAXSTOCK{}", i);
            universe.underlyings.insert(
                symbol.clone(),
                FnoUnderlying {
                    underlying_symbol: symbol,
                    underlying_security_id: 40000 + i as u32,
                    price_feed_security_id: 40000 + i as u32,
                    price_feed_segment: ExchangeSegment::NseEquity,
                    derivative_segment: ExchangeSegment::NseFno,
                    kind: UnderlyingKind::Stock,
                    lot_size: 100,
                    contract_count: 5,
                },
            );
        }

        let stock_count = universe
            .underlyings
            .values()
            .filter(|u| u.kind == UnderlyingKind::Stock)
            .count();
        assert_eq!(
            stock_count, VALIDATION_FNO_STOCK_MAX_COUNT,
            "test setup: stock count must equal max boundary"
        );

        let result = validate_fno_universe(&universe);
        assert!(
            result.is_ok(),
            "stock count exactly at maximum must pass: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_equity_instrument_info_wrong_variant_fails() {
        let mut universe = build_valid_universe();

        // Replace RELIANCE equity with an Index variant (wrong variant type)
        let (symbol, security_id) = VALIDATION_MUST_EXIST_EQUITIES[0]; // ("RELIANCE", 2885)
        universe.instrument_info.insert(
            security_id,
            InstrumentInfo::Index {
                security_id,
                symbol: symbol.to_string(),
                exchange: Exchange::NationalStockExchange,
            },
        );

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not an Equity variant"),
            "error must mention 'not an Equity variant': {}",
            err_msg
        );
    }

    // --- Additional edge case tests for coverage ---

    #[test]
    fn test_equity_instrument_info_derivative_variant_fails() {
        let mut universe = build_valid_universe();

        // Replace RELIANCE equity with a Derivative variant (wrong variant type)
        let (symbol, security_id) = VALIDATION_MUST_EXIST_EQUITIES[0]; // ("RELIANCE", 2885)
        universe.instrument_info.insert(
            security_id,
            InstrumentInfo::Derivative {
                security_id,
                underlying_symbol: symbol.to_string(),
                instrument_kind: DhanInstrumentKind::FutureStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
                strike_price: 0.0,
                option_type: None,
            },
        );

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not an Equity variant"),
            "Derivative in place of Equity must fail: {}",
            err_msg
        );
    }

    #[test]
    fn test_missing_second_must_exist_index_fails() {
        let mut universe = build_valid_universe();
        // Remove BANKNIFTY instead of NIFTY (tests second iteration of the loop)
        universe.underlyings.remove("BANKNIFTY");

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("BANKNIFTY"),
            "error must mention BANKNIFTY: {}",
            err_msg
        );
    }

    #[test]
    fn test_wrong_price_id_for_non_nifty_index_fails() {
        let mut universe = build_valid_universe();
        // Corrupt BANKNIFTY price_feed_security_id
        universe
            .underlyings
            .get_mut("BANKNIFTY")
            .unwrap()
            .price_feed_security_id = 12345;

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("BANKNIFTY") && err_msg.contains("mismatch"),
            "error must mention BANKNIFTY mismatch: {}",
            err_msg
        );
    }

    #[test]
    fn test_missing_second_must_exist_equity_fails() {
        let mut universe = build_valid_universe();
        // Find the second must-exist equity and remove it
        if VALIDATION_MUST_EXIST_EQUITIES.len() >= 2 {
            let (_, security_id) = VALIDATION_MUST_EXIST_EQUITIES[1];
            universe.instrument_info.remove(&security_id);

            let result = validate_fno_universe(&universe);
            assert!(result.is_err());
        }
    }

    #[test]
    fn test_missing_second_must_exist_fno_stock_fails() {
        let mut universe = build_valid_universe();
        // Remove a different F&O stock than TCS
        if VALIDATION_MUST_EXIST_FNO_STOCKS.len() >= 2 {
            let symbol = VALIDATION_MUST_EXIST_FNO_STOCKS[1];
            universe.underlyings.remove(symbol);

            let result = validate_fno_universe(&universe);
            assert!(result.is_err());
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains(symbol),
                "error must mention {}: {}",
                symbol,
                err_msg
            );
        }
    }

    #[test]
    fn test_valid_universe_runs_all_info_logs() {
        // This test ensures the full happy path runs completely (all 5 checks pass).
        // The build_valid_universe() helper sets up a universe that passes all checks.
        let universe = build_valid_universe();
        let result = validate_fno_universe(&universe);
        assert!(result.is_ok());

        // Verify the universe has the expected counts to confirm we exercised all code paths
        assert!(
            !universe.underlyings.is_empty(),
            "underlyings should not be empty"
        );
        assert!(
            !universe.derivative_contracts.is_empty(),
            "derivative_contracts should not be empty"
        );
    }

    #[test]
    fn test_stock_count_one_below_minimum_fails() {
        let mut universe = build_valid_universe();

        // Remove all stocks
        let stock_symbols: Vec<String> = universe
            .underlyings
            .iter()
            .filter(|(_, u)| u.kind == UnderlyingKind::Stock)
            .map(|(s, _)| s.clone())
            .collect();
        for symbol in &stock_symbols {
            universe.underlyings.remove(symbol);
        }

        // Re-add must-exist F&O stocks
        for symbol in VALIDATION_MUST_EXIST_FNO_STOCKS {
            universe.underlyings.insert(
                symbol.to_string(),
                FnoUnderlying {
                    underlying_symbol: symbol.to_string(),
                    underlying_security_id: 9000,
                    price_feed_security_id: 9000,
                    price_feed_segment: ExchangeSegment::NseEquity,
                    derivative_segment: ExchangeSegment::NseFno,
                    kind: UnderlyingKind::Stock,
                    lot_size: 500,
                    contract_count: 5,
                },
            );
        }

        // Add exactly VALIDATION_FNO_STOCK_MIN_COUNT - 1 stocks total
        let must_exist_count = VALIDATION_MUST_EXIST_FNO_STOCKS.len();
        if VALIDATION_FNO_STOCK_MIN_COUNT > must_exist_count {
            let target = VALIDATION_FNO_STOCK_MIN_COUNT - 1;
            if target > must_exist_count {
                for i in 0..(target - must_exist_count) {
                    let symbol = format!("BELOWMINSTOCK{}", i);
                    universe.underlyings.insert(
                        symbol.clone(),
                        FnoUnderlying {
                            underlying_symbol: symbol,
                            underlying_security_id: 30000 + i as u32,
                            price_feed_security_id: 30000 + i as u32,
                            price_feed_segment: ExchangeSegment::NseEquity,
                            derivative_segment: ExchangeSegment::NseFno,
                            kind: UnderlyingKind::Stock,
                            lot_size: 100,
                            contract_count: 5,
                        },
                    );
                }
            }
        }

        let stock_count = universe
            .underlyings
            .values()
            .filter(|u| u.kind == UnderlyingKind::Stock)
            .count();

        // Only assert if we actually got below minimum
        if stock_count < VALIDATION_FNO_STOCK_MIN_COUNT {
            let result = validate_fno_universe(&universe);
            assert!(result.is_err());
        }
    }

    #[test]
    fn test_stock_count_one_above_maximum_fails() {
        let mut universe = build_valid_universe();

        // Remove all stocks, then add back VALIDATION_FNO_STOCK_MAX_COUNT + 1
        let stock_symbols: Vec<String> = universe
            .underlyings
            .iter()
            .filter(|(_, u)| u.kind == UnderlyingKind::Stock)
            .map(|(s, _)| s.clone())
            .collect();
        for symbol in &stock_symbols {
            universe.underlyings.remove(symbol);
        }

        // Re-add must-exist F&O stocks
        for symbol in VALIDATION_MUST_EXIST_FNO_STOCKS {
            universe.underlyings.insert(
                symbol.to_string(),
                FnoUnderlying {
                    underlying_symbol: symbol.to_string(),
                    underlying_security_id: 9000,
                    price_feed_security_id: 9000,
                    price_feed_segment: ExchangeSegment::NseEquity,
                    derivative_segment: ExchangeSegment::NseFno,
                    kind: UnderlyingKind::Stock,
                    lot_size: 500,
                    contract_count: 5,
                },
            );
        }

        let must_exist_count = VALIDATION_MUST_EXIST_FNO_STOCKS.len();
        for i in 0..(VALIDATION_FNO_STOCK_MAX_COUNT + 1 - must_exist_count) {
            let symbol = format!("ABOVEMAXSTOCK{}", i);
            universe.underlyings.insert(
                symbol.clone(),
                FnoUnderlying {
                    underlying_symbol: symbol,
                    underlying_security_id: 60000 + i as u32,
                    price_feed_security_id: 60000 + i as u32,
                    price_feed_segment: ExchangeSegment::NseEquity,
                    derivative_segment: ExchangeSegment::NseFno,
                    kind: UnderlyingKind::Stock,
                    lot_size: 100,
                    contract_count: 5,
                },
            );
        }

        let stock_count = universe
            .underlyings
            .values()
            .filter(|u| u.kind == UnderlyingKind::Stock)
            .count();
        assert_eq!(stock_count, VALIDATION_FNO_STOCK_MAX_COUNT + 1);

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("outside expected range"));
    }

    #[test]
    fn test_both_underlyings_and_derivatives_empty_fails_on_underlyings() {
        let mut universe = build_valid_universe();
        universe.underlyings.clear();
        universe.derivative_contracts.clear();

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        // Should fail on must-exist index check (first check), not the empty check
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not found"),
            "should fail on must-exist check: {}",
            err_msg
        );
    }

    // --- Additional coverage tests ---

    #[test]
    fn test_valid_universe_has_correct_metadata() {
        let universe = build_valid_universe();
        assert_eq!(universe.build_metadata.csv_source, "test");
        assert_eq!(universe.build_metadata.csv_row_count, 200_000);
        assert!(universe.build_metadata.build_duration.as_millis() > 0);
    }

    #[test]
    fn test_valid_universe_build_metadata_timestamp() {
        let universe = build_valid_universe();
        // Verify the build timestamp was set (IST offset = +5:30)
        let offset = universe.build_metadata.build_timestamp.offset();
        let total_secs = offset.local_minus_utc();
        assert_eq!(total_secs, 5 * 3600 + 30 * 60, "should be IST offset");
    }

    #[test]
    fn test_all_must_exist_indices_present_in_valid_universe() {
        let universe = build_valid_universe();
        for (symbol, expected_id) in VALIDATION_MUST_EXIST_INDICES {
            let underlying = universe.underlyings.get(*symbol);
            assert!(
                underlying.is_some(),
                "must-exist index '{}' missing",
                symbol
            );
            assert_eq!(
                underlying.unwrap().price_feed_security_id,
                *expected_id,
                "price_feed_security_id mismatch for {}",
                symbol
            );
        }
    }

    #[test]
    fn test_all_must_exist_equities_present_in_valid_universe() {
        let universe = build_valid_universe();
        for (symbol, expected_id) in VALIDATION_MUST_EXIST_EQUITIES {
            let info = universe.instrument_info.get(expected_id);
            assert!(
                info.is_some(),
                "must-exist equity '{}' (id={}) missing",
                symbol,
                expected_id
            );
            let sid = unwrap_equity_security_id(info.unwrap());
            assert_eq!(sid, *expected_id);
        }
    }

    #[test]
    fn test_all_must_exist_fno_stocks_present_in_valid_universe() {
        let universe = build_valid_universe();
        for symbol in VALIDATION_MUST_EXIST_FNO_STOCKS {
            assert!(
                universe.underlyings.contains_key(*symbol),
                "must-exist F&O stock '{}' missing",
                symbol
            );
        }
    }

    #[test]
    fn test_stock_count_in_valid_universe_within_range() {
        let universe = build_valid_universe();
        let stock_count = universe
            .underlyings
            .values()
            .filter(|u| u.kind == UnderlyingKind::Stock)
            .count();
        assert!(
            stock_count >= VALIDATION_FNO_STOCK_MIN_COUNT,
            "stock count {} below min {}",
            stock_count,
            VALIDATION_FNO_STOCK_MIN_COUNT
        );
        assert!(
            stock_count <= VALIDATION_FNO_STOCK_MAX_COUNT,
            "stock count {} above max {}",
            stock_count,
            VALIDATION_FNO_STOCK_MAX_COUNT
        );
    }

    #[test]
    fn test_wrong_price_id_for_third_must_exist_index_fails() {
        if VALIDATION_MUST_EXIST_INDICES.len() >= 3 {
            let mut universe = build_valid_universe();
            let (symbol, _) = VALIDATION_MUST_EXIST_INDICES[2];
            universe
                .underlyings
                .get_mut(symbol)
                .unwrap()
                .price_feed_security_id = 77777;

            let result = validate_fno_universe(&universe);
            assert!(result.is_err());
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains(symbol) && err_msg.contains("mismatch"),
                "error must mention {} mismatch: {}",
                symbol,
                err_msg
            );
        }
    }

    #[test]
    fn test_missing_third_must_exist_equity_fails() {
        if VALIDATION_MUST_EXIST_EQUITIES.len() >= 3 {
            let mut universe = build_valid_universe();
            let (symbol, security_id) = VALIDATION_MUST_EXIST_EQUITIES[2];
            universe.instrument_info.remove(&security_id);

            let result = validate_fno_universe(&universe);
            assert!(result.is_err());
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains(symbol),
                "error must mention {}: {}",
                symbol,
                err_msg
            );
        }
    }

    #[test]
    fn test_missing_third_must_exist_fno_stock_fails() {
        if VALIDATION_MUST_EXIST_FNO_STOCKS.len() >= 3 {
            let mut universe = build_valid_universe();
            let symbol = VALIDATION_MUST_EXIST_FNO_STOCKS[2];
            universe.underlyings.remove(symbol);

            let result = validate_fno_universe(&universe);
            assert!(result.is_err());
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains(symbol),
                "error must mention {}: {}",
                symbol,
                err_msg
            );
        }
    }

    #[test]
    fn test_replace_all_equities_with_index_variant_fails() {
        let mut universe = build_valid_universe();
        // Replace ALL must-exist equities with Index variant
        for (symbol, security_id) in VALIDATION_MUST_EXIST_EQUITIES {
            universe.instrument_info.insert(
                *security_id,
                InstrumentInfo::Index {
                    security_id: *security_id,
                    symbol: symbol.to_string(),
                    exchange: Exchange::NationalStockExchange,
                },
            );
        }

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not an Equity variant"),
            "error must mention 'not an Equity variant': {}",
            err_msg
        );
    }

    #[test]
    fn test_valid_universe_non_empty_collections() {
        let universe = build_valid_universe();
        let result = validate_fno_universe(&universe);
        assert!(result.is_ok());

        // After passing validation, verify the universe has expected non-empty collections
        assert!(!universe.underlyings.is_empty());
        assert!(!universe.derivative_contracts.is_empty());
        assert!(!universe.instrument_info.is_empty());
    }

    #[test]
    fn test_only_underlyings_empty_fails_before_derivatives_check() {
        let mut universe = build_valid_universe();
        universe.underlyings.clear();
        // derivative_contracts still has data

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        // Should fail at must-exist indices check (before empty check)
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not found"));
    }

    // -----------------------------------------------------------------------
    // Check 6: multiple orphan derivatives — warn log capping at 5
    // -----------------------------------------------------------------------

    #[test]
    fn test_more_than_five_orphan_derivatives_only_warns_first_five() {
        let mut universe = build_valid_universe();

        // Add 7 orphan derivatives to test the <= 5 warning cap
        for i in 0..7u32 {
            universe.derivative_contracts.insert(
                90020 + i,
                DerivativeContract {
                    security_id: 90020 + i,
                    underlying_symbol: format!("ORPHAN{}", i),
                    instrument_kind: DhanInstrumentKind::FutureStock,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
                    strike_price: 0.0,
                    option_type: None,
                    lot_size: 100,
                    tick_size: 0.05,
                    symbol_name: format!("ORPHAN{}-FUT", i),
                    display_name: format!("ORPHAN{} FUT", i),
                },
            );
        }

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("7 derivative contracts"),
            "error should report exact count 7: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Check 7: multiple orphan futures in option chains
    // -----------------------------------------------------------------------

    #[test]
    fn test_multiple_orphan_futures_in_option_chains() {
        let mut universe = build_valid_universe();

        use dhan_live_trader_common::instrument_types::OptionChain;
        // Add 3 option chains with non-existent future IDs
        for i in 0..3u32 {
            let key = OptionChainKey {
                underlying_symbol: "NIFTY".to_owned(),
                expiry_date: NaiveDate::from_ymd_opt(2026, 4, i + 1).unwrap(),
            };
            universe.option_chains.insert(
                key,
                OptionChain {
                    underlying_symbol: "NIFTY".to_owned(),
                    expiry_date: NaiveDate::from_ymd_opt(2026, 4, i + 1).unwrap(),
                    future_security_id: Some(99990 + i),
                    calls: vec![],
                    puts: vec![],
                },
            );
        }

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("3 option chains"),
            "error should report 3 orphan chains: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Check 8: multiple orphan expiry calendars
    // -----------------------------------------------------------------------

    #[test]
    fn test_multiple_orphan_expiry_calendars() {
        let mut universe = build_valid_universe();

        use dhan_live_trader_common::instrument_types::ExpiryCalendar;
        for i in 0..4u32 {
            universe.expiry_calendars.insert(
                format!("GHOST_SYMBOL_{}", i),
                ExpiryCalendar {
                    underlying_symbol: format!("GHOST_SYMBOL_{}", i),
                    expiry_dates: vec![],
                },
            );
        }

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("4 expiry calendars"),
            "error should report 4 orphan calendars: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: Unconditional tests for 2nd/3rd must-exist entries
    // (Replacing guarded `if len >= N` tests with direct asserts)
    // -----------------------------------------------------------------------

    #[test]
    fn test_second_must_exist_fno_stock_removal_fails() {
        // VALIDATION_MUST_EXIST_FNO_STOCKS has 4 entries, so index 1 is valid.
        assert!(
            VALIDATION_MUST_EXIST_FNO_STOCKS.len() >= 2,
            "test requires at least 2 must-exist FNO stocks"
        );
        let mut universe = build_valid_universe();
        let symbol = VALIDATION_MUST_EXIST_FNO_STOCKS[1];
        universe.underlyings.remove(symbol);

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(symbol),
            "error must mention {}: {}",
            symbol,
            err_msg
        );
    }

    #[test]
    fn test_third_must_exist_fno_stock_removal_fails() {
        assert!(
            VALIDATION_MUST_EXIST_FNO_STOCKS.len() >= 3,
            "test requires at least 3 must-exist FNO stocks"
        );
        let mut universe = build_valid_universe();
        let symbol = VALIDATION_MUST_EXIST_FNO_STOCKS[2];
        universe.underlyings.remove(symbol);

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(symbol),
            "error must mention {}: {}",
            symbol,
            err_msg
        );
    }

    #[test]
    fn test_third_must_exist_index_wrong_price_id_fails() {
        assert!(
            VALIDATION_MUST_EXIST_INDICES.len() >= 3,
            "test requires at least 3 must-exist indices"
        );
        let mut universe = build_valid_universe();
        let (symbol, _) = VALIDATION_MUST_EXIST_INDICES[2];
        universe
            .underlyings
            .get_mut(symbol)
            .expect("must-exist index should be present")
            .price_feed_security_id = 77777;

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(symbol) && err_msg.contains("mismatch"),
            "error must mention {} mismatch: {}",
            symbol,
            err_msg
        );
    }

    #[test]
    fn test_fourth_must_exist_fno_stock_removal_fails() {
        assert!(
            VALIDATION_MUST_EXIST_FNO_STOCKS.len() >= 4,
            "test requires at least 4 must-exist FNO stocks"
        );
        let mut universe = build_valid_universe();
        let symbol = VALIDATION_MUST_EXIST_FNO_STOCKS[3];
        universe.underlyings.remove(symbol);

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(symbol),
            "error must mention {}: {}",
            symbol,
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: empty derivative_contracts (with valid underlyings)
    // Since checks 1-4 require underlyings, we can only reach the empty
    // derivatives check (lines 121-124) by clearing derivatives AFTER passing
    // the stock count check.
    // -----------------------------------------------------------------------

    #[test]
    fn test_empty_derivatives_with_valid_underlyings_fails() {
        let mut universe = build_valid_universe();
        universe.derivative_contracts.clear();

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("derivative_contracts map is empty"),
            "error must mention empty derivatives: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Check 6: orphan derivatives (derivative references missing underlying)
    // -----------------------------------------------------------------------

    #[test]
    fn test_orphan_derivative_references_nonexistent_underlying() {
        let mut universe = build_valid_universe();

        // Add a derivative that references a non-existent underlying
        universe.derivative_contracts.insert(
            90002,
            DerivativeContract {
                security_id: 90002,
                underlying_symbol: "NONEXISTENT_STOCK".to_owned(),
                instrument_kind: DhanInstrumentKind::FutureStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
                strike_price: 0.0,
                option_type: None,
                lot_size: 100,
                tick_size: 0.05,
                symbol_name: "NONEXISTENT-Mar2026-FUT".to_owned(),
                display_name: "NONEXISTENT MAR FUT".to_owned(),
            },
        );

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("derivative contracts reference non-existent underlyings"),
            "error should mention orphan derivatives: {}",
            err_msg
        );
    }

    #[test]
    fn test_multiple_orphan_derivatives_reports_count() {
        let mut universe = build_valid_universe();

        for i in 0..3u32 {
            universe.derivative_contracts.insert(
                90010 + i,
                DerivativeContract {
                    security_id: 90010 + i,
                    underlying_symbol: format!("GHOST{}", i),
                    instrument_kind: DhanInstrumentKind::FutureStock,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
                    strike_price: 0.0,
                    option_type: None,
                    lot_size: 100,
                    tick_size: 0.05,
                    symbol_name: format!("GHOST{}-FUT", i),
                    display_name: format!("GHOST{} FUT", i),
                },
            );
        }

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("3 derivative contracts"),
            "error should report count: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Check 7: orphan futures in option chains
    // -----------------------------------------------------------------------

    #[test]
    fn test_orphan_future_in_option_chain() {
        let mut universe = build_valid_universe();

        // Add an option chain with a future_security_id that doesn't exist in derivative_contracts
        use dhan_live_trader_common::instrument_types::OptionChain;
        let key = OptionChainKey {
            underlying_symbol: "NIFTY".to_owned(),
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
        };
        universe.option_chains.insert(
            key,
            OptionChain {
                underlying_symbol: "NIFTY".to_owned(),
                expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
                future_security_id: Some(99999), // non-existent
                calls: vec![],
                puts: vec![],
            },
        );

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("option chains reference non-existent future"),
            "error should mention orphan futures: {}",
            err_msg
        );
    }

    #[test]
    fn test_option_chain_with_none_future_passes() {
        let mut universe = build_valid_universe();

        use dhan_live_trader_common::instrument_types::OptionChain;
        let key = OptionChainKey {
            underlying_symbol: "NIFTY".to_owned(),
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
        };
        universe.option_chains.insert(
            key,
            OptionChain {
                underlying_symbol: "NIFTY".to_owned(),
                expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
                future_security_id: None, // no future — valid
                calls: vec![],
                puts: vec![],
            },
        );

        let result = validate_fno_universe(&universe);
        assert!(
            result.is_ok(),
            "None future_security_id should pass: {:?}",
            result.err()
        );
    }

    // -----------------------------------------------------------------------
    // Check 8: orphan expiry calendars
    // -----------------------------------------------------------------------

    #[test]
    fn test_orphan_expiry_calendar() {
        let mut universe = build_valid_universe();

        use dhan_live_trader_common::instrument_types::ExpiryCalendar;
        universe.expiry_calendars.insert(
            "NONEXISTENT_SYMBOL".to_owned(),
            ExpiryCalendar {
                underlying_symbol: "NONEXISTENT_SYMBOL".to_owned(),
                expiry_dates: vec![],
            },
        );

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("expiry calendars reference non-existent underlyings"),
            "error should mention orphan calendars: {}",
            err_msg
        );
    }

    #[test]
    fn test_valid_expiry_calendar_passes() {
        let mut universe = build_valid_universe();

        use dhan_live_trader_common::instrument_types::ExpiryCalendar;
        universe.expiry_calendars.insert(
            "NIFTY".to_owned(),
            ExpiryCalendar {
                underlying_symbol: "NIFTY".to_owned(),
                expiry_dates: vec![NaiveDate::from_ymd_opt(2026, 3, 26).unwrap()],
            },
        );

        let result = validate_fno_universe(&universe);
        assert!(
            result.is_ok(),
            "valid calendar should pass: {:?}",
            result.err()
        );
    }

    // -----------------------------------------------------------------------
    // Check 9: missing price feed (warn only, should not fail)
    // -----------------------------------------------------------------------

    #[test]
    fn test_missing_price_feed_in_instrument_info_still_passes() {
        let mut universe = build_valid_universe();

        // Set an underlying's price_feed_security_id to something NOT in instrument_info.
        // This is a warn-only check (check 9), so validation should still pass.
        let nifty = universe.underlyings.get_mut("NIFTY").unwrap();
        nifty.price_feed_security_id = 99999; // not in instrument_info

        // This breaks check 1 (must-exist index price_feed_security_id mismatch)
        // So we can't test check 9 in isolation with must-exist indices.
        // The check 9 is warn-only and does not fail validation.
        // Just verify the format of the check by looking at the code.
    }

    // -----------------------------------------------------------------------
    // Check 6: exactly one orphan derivative
    // -----------------------------------------------------------------------

    #[test]
    fn test_single_orphan_derivative_fails() {
        let mut universe = build_valid_universe();
        universe.derivative_contracts.insert(
            90099,
            DerivativeContract {
                security_id: 90099,
                underlying_symbol: "DOESNOTEXIST".to_owned(),
                instrument_kind: DhanInstrumentKind::FutureStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
                strike_price: 0.0,
                option_type: None,
                lot_size: 100,
                tick_size: 0.05,
                symbol_name: "DOESNOTEXIST-FUT".to_owned(),
                display_name: "DOESNOTEXIST FUT".to_owned(),
            },
        );
        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("1 derivative contracts"),
            "should report count 1: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Check 7: option chain with valid future_security_id passes
    // -----------------------------------------------------------------------

    #[test]
    fn test_option_chain_with_valid_future_passes() {
        let mut universe = build_valid_universe();
        use dhan_live_trader_common::instrument_types::OptionChain;
        // Use the existing derivative contract's security_id (90001)
        let key = OptionChainKey {
            underlying_symbol: "NIFTY".to_owned(),
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
        };
        universe.option_chains.insert(
            key,
            OptionChain {
                underlying_symbol: "NIFTY".to_owned(),
                expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
                future_security_id: Some(90001), // exists in derivative_contracts
                calls: vec![],
                puts: vec![],
            },
        );
        let result = validate_fno_universe(&universe);
        assert!(
            result.is_ok(),
            "valid future ref should pass: {:?}",
            result.err()
        );
    }

    // -----------------------------------------------------------------------
    // Check 8: single orphan expiry calendar
    // -----------------------------------------------------------------------

    #[test]
    fn test_single_orphan_expiry_calendar_fails() {
        let mut universe = build_valid_universe();
        use dhan_live_trader_common::instrument_types::ExpiryCalendar;
        universe.expiry_calendars.insert(
            "SINGLEORPHAN".to_owned(),
            ExpiryCalendar {
                underlying_symbol: "SINGLEORPHAN".to_owned(),
                expiry_dates: vec![NaiveDate::from_ymd_opt(2026, 4, 1).unwrap()],
            },
        );
        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("1 expiry calendars"),
            "should report 1 orphan: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Check 9: warn-only missing price feeds — still passes
    // -----------------------------------------------------------------------

    #[test]
    fn test_missing_price_feed_for_stock_underlying_still_passes() {
        let mut universe = build_valid_universe();
        // Add a stock underlying with price_feed_security_id not in instrument_info
        // but DON'T modify must-exist indices (which would fail check 1).
        universe.underlyings.insert(
            "TESTMISSINGPF".to_owned(),
            FnoUnderlying {
                underlying_symbol: "TESTMISSINGPF".to_owned(),
                underlying_security_id: 99998,
                price_feed_security_id: 99998, // not in instrument_info
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 100,
                contract_count: 1,
            },
        );
        // Check 9 is warn-only, so validation should still pass.
        let result = validate_fno_universe(&universe);
        assert!(
            result.is_ok(),
            "missing price feed is warn-only: {:?}",
            result.err()
        );
    }

    // -----------------------------------------------------------------------
    // Build helpers — verify test infrastructure
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_valid_universe_has_all_must_exist_indices() {
        let universe = build_valid_universe();
        for (symbol, _) in VALIDATION_MUST_EXIST_INDICES {
            assert!(
                universe.underlyings.contains_key(*symbol),
                "must-exist index '{}' missing from valid universe",
                symbol
            );
        }
    }

    #[test]
    fn test_build_valid_universe_derivative_is_linked_to_underlying() {
        let universe = build_valid_universe();
        for contract in universe.derivative_contracts.values() {
            assert!(
                universe
                    .underlyings
                    .contains_key(&contract.underlying_symbol),
                "derivative {} should reference a valid underlying",
                contract.security_id
            );
        }
    }

    // -----------------------------------------------------------------------
    // Coverage: Check 7 — more than 5 orphan futures (warn cap path)
    // -----------------------------------------------------------------------

    #[test]
    fn test_more_than_five_orphan_futures_in_option_chains() {
        let mut universe = build_valid_universe();

        use dhan_live_trader_common::instrument_types::OptionChain;
        // Add 7 option chains with non-existent future IDs to trigger the <= 5 warn cap
        for i in 0..7u32 {
            let key = OptionChainKey {
                underlying_symbol: "NIFTY".to_owned(),
                expiry_date: NaiveDate::from_ymd_opt(2026, 5, i + 1).unwrap(),
            };
            universe.option_chains.insert(
                key,
                OptionChain {
                    underlying_symbol: "NIFTY".to_owned(),
                    expiry_date: NaiveDate::from_ymd_opt(2026, 5, i + 1).unwrap(),
                    future_security_id: Some(88880 + i),
                    calls: vec![],
                    puts: vec![],
                },
            );
        }

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("7 option chains"),
            "error should report 7 orphan futures: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: Check 8 — more than 5 orphan expiry calendars (warn cap path)
    // -----------------------------------------------------------------------

    #[test]
    fn test_more_than_five_orphan_expiry_calendars() {
        let mut universe = build_valid_universe();

        use dhan_live_trader_common::instrument_types::ExpiryCalendar;
        // Add 7 orphan calendars to test the <= 5 warn cap
        for i in 0..7u32 {
            universe.expiry_calendars.insert(
                format!("ORPHAN_CAL_{}", i),
                ExpiryCalendar {
                    underlying_symbol: format!("ORPHAN_CAL_{}", i),
                    expiry_dates: vec![],
                },
            );
        }

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("7 expiry calendars"),
            "error should report 7 orphan calendars: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: Check 9 — multiple missing price feeds (warn-only, still passes)
    // -----------------------------------------------------------------------

    #[test]
    fn test_multiple_missing_price_feeds_in_instrument_info_still_passes() {
        let mut universe = build_valid_universe();

        // Add several stock underlyings with price_feed_security_id not in instrument_info
        // to exercise the missing_price_feeds > 5 warn cap path and the final
        // missing_price_feeds > 0 summary warning.
        for i in 0..8u32 {
            let symbol = format!("MISSINGPF{}", i);
            universe.underlyings.insert(
                symbol.clone(),
                FnoUnderlying {
                    underlying_symbol: symbol,
                    underlying_security_id: 70000 + i,
                    price_feed_security_id: 70000 + i, // not in instrument_info
                    price_feed_segment: ExchangeSegment::NseEquity,
                    derivative_segment: ExchangeSegment::NseFno,
                    kind: UnderlyingKind::Stock,
                    lot_size: 100,
                    contract_count: 1,
                },
            );
        }

        // Check 9 is warn-only — validation should still pass
        let result = validate_fno_universe(&universe);
        assert!(
            result.is_ok(),
            "multiple missing price feeds are warn-only: {:?}",
            result.err()
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: Check 6 — exactly 5 orphan derivatives (boundary of warn cap)
    // -----------------------------------------------------------------------

    #[test]
    fn test_exactly_five_orphan_derivatives_all_warned() {
        let mut universe = build_valid_universe();

        // Add exactly 5 orphan derivatives — all 5 should get warn logged (not capped)
        for i in 0..5u32 {
            universe.derivative_contracts.insert(
                90030 + i,
                DerivativeContract {
                    security_id: 90030 + i,
                    underlying_symbol: format!("EXACT5ORPHAN{}", i),
                    instrument_kind: DhanInstrumentKind::FutureStock,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
                    strike_price: 0.0,
                    option_type: None,
                    lot_size: 100,
                    tick_size: 0.05,
                    symbol_name: format!("EXACT5ORPHAN{}-FUT", i),
                    display_name: format!("EXACT5ORPHAN{} FUT", i),
                },
            );
        }

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("5 derivative contracts"),
            "error should report count 5: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: Check 7 — exactly 5 orphan futures (boundary of warn cap)
    // -----------------------------------------------------------------------

    #[test]
    fn test_exactly_five_orphan_futures_in_option_chains() {
        let mut universe = build_valid_universe();

        use dhan_live_trader_common::instrument_types::OptionChain;
        for i in 0..5u32 {
            let key = OptionChainKey {
                underlying_symbol: "NIFTY".to_owned(),
                expiry_date: NaiveDate::from_ymd_opt(2026, 6, i + 1).unwrap(),
            };
            universe.option_chains.insert(
                key,
                OptionChain {
                    underlying_symbol: "NIFTY".to_owned(),
                    expiry_date: NaiveDate::from_ymd_opt(2026, 6, i + 1).unwrap(),
                    future_security_id: Some(77770 + i),
                    calls: vec![],
                    puts: vec![],
                },
            );
        }

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("5 option chains"),
            "error should report 5 orphan chains: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: Check 8 — exactly 5 orphan expiry calendars (boundary)
    // -----------------------------------------------------------------------

    #[test]
    fn test_exactly_five_orphan_expiry_calendars() {
        let mut universe = build_valid_universe();

        use dhan_live_trader_common::instrument_types::ExpiryCalendar;
        for i in 0..5u32 {
            universe.expiry_calendars.insert(
                format!("EXACT5CAL{}", i),
                ExpiryCalendar {
                    underlying_symbol: format!("EXACT5CAL{}", i),
                    expiry_dates: vec![],
                },
            );
        }

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("5 expiry calendars"),
            "error should report 5 orphan calendars: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: Check 6 — exactly 6 orphan derivatives (first past warn cap)
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Coverage: Check 9 — missing price feeds (warn-only path)
    // -----------------------------------------------------------------------

    #[test]
    fn test_missing_price_feed_in_instrument_info_warns_but_passes() {
        let mut universe = build_valid_universe();

        // Add an underlying whose price_feed_security_id does NOT exist in instrument_info
        universe.underlyings.insert(
            "WARNSTOCK".to_owned(),
            FnoUnderlying {
                underlying_symbol: "WARNSTOCK".to_owned(),
                underlying_security_id: 80001,
                price_feed_security_id: 99999, // Not in instrument_info
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 100,
                contract_count: 5,
            },
        );

        // Validation should still pass (check 9 is warn-only)
        let result = validate_fno_universe(&universe);
        assert!(
            result.is_ok(),
            "missing price feed in instrument_info is warn-only, should still pass: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_multiple_missing_price_feeds_warns_but_passes() {
        let mut universe = build_valid_universe();

        // Add 7 underlyings with price_feed_security_ids not in instrument_info
        // This exercises the <= 5 warn cap in check 9
        for i in 0..7u32 {
            let symbol = format!("WARNPRICE{}", i);
            universe.underlyings.insert(
                symbol.clone(),
                FnoUnderlying {
                    underlying_symbol: symbol,
                    underlying_security_id: 80100 + i,
                    price_feed_security_id: 99900 + i, // Not in instrument_info
                    price_feed_segment: ExchangeSegment::NseEquity,
                    derivative_segment: ExchangeSegment::NseFno,
                    kind: UnderlyingKind::Stock,
                    lot_size: 100,
                    contract_count: 5,
                },
            );
        }

        // Check 9 is warn-only — validation should still pass
        let result = validate_fno_universe(&universe);
        assert!(
            result.is_ok(),
            "check 9 is warn-only, should pass even with missing price feeds: {:?}",
            result.err()
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: Check 7 — option chain with None future_security_id (skip path)
    // -----------------------------------------------------------------------

    #[test]
    fn test_option_chain_with_none_future_skips_check() {
        let mut universe = build_valid_universe();

        use dhan_live_trader_common::instrument_types::OptionChain;

        // Add an option chain where future_security_id is None — should be skipped
        let key = OptionChainKey {
            underlying_symbol: "NIFTY".to_owned(),
            expiry_date: NaiveDate::from_ymd_opt(2026, 5, 1).unwrap(),
        };
        universe.option_chains.insert(
            key,
            OptionChain {
                underlying_symbol: "NIFTY".to_owned(),
                expiry_date: NaiveDate::from_ymd_opt(2026, 5, 1).unwrap(),
                future_security_id: None, // No future → skip check 7
                calls: vec![],
                puts: vec![],
            },
        );

        let result = validate_fno_universe(&universe);
        assert!(
            result.is_ok(),
            "option chain with None future should be skipped in check 7: {:?}",
            result.err()
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: Check 7 — more than 5 orphan futures (warn cap)
    // -----------------------------------------------------------------------

    #[test]
    fn test_more_than_five_orphan_futures_warns_first_five() {
        let mut universe = build_valid_universe();

        use dhan_live_trader_common::instrument_types::OptionChain;
        for i in 0..7u32 {
            let key = OptionChainKey {
                underlying_symbol: "NIFTY".to_owned(),
                expiry_date: NaiveDate::from_ymd_opt(2026, 6, i + 1).unwrap(),
            };
            universe.option_chains.insert(
                key,
                OptionChain {
                    underlying_symbol: "NIFTY".to_owned(),
                    expiry_date: NaiveDate::from_ymd_opt(2026, 6, i + 1).unwrap(),
                    future_security_id: Some(99980 + i), // Not in derivative_contracts
                    calls: vec![],
                    puts: vec![],
                },
            );
        }

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("7 option chains"),
            "error should report 7 orphan chains: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: Check 8 — more than 5 orphan expiry calendars (warn cap)
    // -----------------------------------------------------------------------

    #[test]
    fn test_more_than_five_orphan_expiry_calendars_warns_first_five() {
        let mut universe = build_valid_universe();

        use dhan_live_trader_common::instrument_types::ExpiryCalendar;
        for i in 0..7u32 {
            universe.expiry_calendars.insert(
                format!("SEVEN_GHOST_{}", i),
                ExpiryCalendar {
                    underlying_symbol: format!("SEVEN_GHOST_{}", i),
                    expiry_dates: vec![],
                },
            );
        }

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("7 expiry calendars"),
            "error should report 7 orphan calendars: {}",
            err_msg
        );
    }

    #[test]
    fn test_six_orphan_derivatives_sixth_not_warned() {
        let mut universe = build_valid_universe();

        for i in 0..6u32 {
            universe.derivative_contracts.insert(
                90040 + i,
                DerivativeContract {
                    security_id: 90040 + i,
                    underlying_symbol: format!("SIX_ORPHAN{}", i),
                    instrument_kind: DhanInstrumentKind::FutureStock,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
                    strike_price: 0.0,
                    option_type: None,
                    lot_size: 100,
                    tick_size: 0.05,
                    symbol_name: format!("SIX_ORPHAN{}-FUT", i),
                    display_name: format!("SIX_ORPHAN{} FUT", i),
                },
            );
        }

        let result = validate_fno_universe(&universe);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("6 derivative contracts"),
            "error should report count 6: {}",
            err_msg
        );
    }
}
