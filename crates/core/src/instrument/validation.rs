//! Post-build validation of the F&O universe.
//!
//! Validates must-exist security IDs, count ranges, and data integrity.
//! Any validation failure prevents system startup.

use anyhow::{Result, bail};
use tracing::info;

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
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    use chrono::{FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};

    use dhan_live_trader_common::types::ExchangeSegment;

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

        let ist_offset = FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap();
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
}
