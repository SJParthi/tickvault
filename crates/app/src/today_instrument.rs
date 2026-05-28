//! Extract today's full per-instrument detail from the built
//! `DailyUniverse` for the daily reconciler.
//!
//! The reconcile *classification* (`compute_reconcile_plan`,
//! `lifecycle_reconcile_plan`) only needs 6 fields, but the lifecycle-row
//! UPSERT (`apply_reconcile_plan`, follow-up) needs the FULL Dhan detail
//! the parser now produces (#845): underlying ids, display name, lot/tick
//! size, expiry, strike, option type. [`TodayInstrument`] carries that
//! full detail; [`TodayInstrument::to_classification_attrs`] projects the
//! 6-field subset the classifier consumes, so #843's
//! `compute_reconcile_plan` stays untouched.
//!
//! Lives in `app` — it bridges `core` (the `DailyUniverse` + `CsvRow`)
//! and the app-side reconcile types.
//!
//! **Feature-gated** under `daily_universe_fetcher` per rule §21 —
//! dormant in the default build.

#![cfg(feature = "daily_universe_fetcher")]

use tickvault_core::instrument::csv_parser::CsvRow;
use tickvault_core::instrument::daily_universe::DailyUniverse;

use crate::lifecycle_reconcile_plan::TodayInstrumentAttrs;

/// One instrument from today's validated universe, with the FULL detail
/// needed to write a complete `instrument_lifecycle` row.
///
/// `*` numeric ids come from the CSV's string cells parsed to integers
/// (0 on a malformed/blank cell — best-effort, matching the parser's
/// own optional-field policy). `expiry_date` stays the raw `YYYY-MM-DD`
/// string; the lifecycle-row writer converts it to nanos.
#[derive(Debug, Clone, PartialEq)]
pub struct TodayInstrument {
    pub security_id: i64,
    pub exchange_segment: String,
    pub exchange_id: String,
    pub instrument_type: String,
    pub symbol_name: String,
    /// 0 for spot/index (no underlying).
    pub underlying_security_id: i64,
    pub underlying_symbol: String,
    pub display_name: String,
    pub lot_size: i32,
    pub tick_size: f64,
    /// Raw `YYYY-MM-DD`; empty for spot/index.
    pub expiry_date: String,
    pub strike_price: f64,
    /// `CE` / `PE` / empty.
    pub option_type: String,
}

impl TodayInstrument {
    /// Project the 6-field subset `compute_reconcile_plan` classifies on.
    #[must_use]
    pub fn to_classification_attrs(&self) -> TodayInstrumentAttrs {
        TodayInstrumentAttrs {
            security_id: self.security_id,
            exchange_segment: self.exchange_segment.clone(),
            instrument_type: self.instrument_type.clone(),
            lot_size: self.lot_size,
            tick_size: self.tick_size,
            symbol_name: self.symbol_name.clone(),
        }
    }
}

/// Map one parsed `CsvRow` → [`TodayInstrument`]. PURE. String ids parse
/// to i64 best-effort (0 on blank/non-numeric — the daily universe is
/// already validated, so this only guards against a Dhan-side anomaly).
#[must_use]
fn csv_row_to_today_instrument(row: &CsvRow) -> TodayInstrument {
    TodayInstrument {
        security_id: row.security_id.trim().parse::<i64>().unwrap_or(0),
        exchange_segment: row.segment.clone(),
        exchange_id: row.exch_id.clone(),
        instrument_type: row.instrument.clone(),
        symbol_name: row.symbol_name.clone(),
        underlying_security_id: row
            .underlying_security_id
            .trim()
            .parse::<i64>()
            .unwrap_or(0),
        underlying_symbol: row.underlying_symbol.clone(),
        display_name: row.display_name.clone(),
        lot_size: row.lot_size,
        tick_size: row.tick_size,
        expiry_date: row.expiry_date.clone(),
        strike_price: row.strike_price,
        option_type: row.option_type.clone(),
    }
}

/// Extract every instrument in the built universe as a full-detail
/// [`TodayInstrument`]. PURE — reads `DailyUniverse::subscription_targets`.
/// Order matches the universe (indices first, then F&O underlyings).
#[must_use]
pub fn extract_today_instruments(universe: &DailyUniverse) -> Vec<TodayInstrument> {
    universe
        .subscription_targets
        .iter()
        .map(|t| csv_row_to_today_instrument(&t.csv_row))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_core::instrument::daily_universe::{InstrumentRole, SubscriptionTarget};

    fn detailed_row() -> CsvRow {
        CsvRow {
            security_id: "43581".to_string(),
            exch_id: "NSE".to_string(),
            segment: "NSE_FNO".to_string(),
            instrument: "OPTSTK".to_string(),
            symbol_name: "TCS-CE".to_string(),
            underlying_security_id: "11536".to_string(),
            underlying_symbol: "TCS".to_string(),
            display_name: "TCS 4000 CALL".to_string(),
            lot_size: 175,
            tick_size: 0.05,
            expiry_date: "2025-12-25".to_string(),
            strike_price: 4000.5,
            option_type: "CE".to_string(),
        }
    }

    #[test]
    fn test_csv_row_to_today_instrument_carries_full_detail() {
        let ti = csv_row_to_today_instrument(&detailed_row());
        assert_eq!(ti.security_id, 43581);
        assert_eq!(ti.exchange_segment, "NSE_FNO");
        assert_eq!(ti.exchange_id, "NSE");
        assert_eq!(ti.instrument_type, "OPTSTK");
        assert_eq!(ti.underlying_security_id, 11536);
        assert_eq!(ti.underlying_symbol, "TCS");
        assert_eq!(ti.display_name, "TCS 4000 CALL");
        assert_eq!(ti.lot_size, 175);
        assert!((ti.tick_size - 0.05).abs() < f64::EPSILON);
        assert_eq!(ti.expiry_date, "2025-12-25");
        assert!((ti.strike_price - 4000.5).abs() < f64::EPSILON);
        assert_eq!(ti.option_type, "CE");
    }

    #[test]
    fn test_csv_row_to_today_instrument_index_defaults() {
        let row = CsvRow {
            security_id: "13".to_string(),
            exch_id: "NSE".to_string(),
            segment: "IDX_I".to_string(),
            instrument: "INDEX".to_string(),
            symbol_name: "NIFTY".to_string(),
            ..Default::default()
        };
        let ti = csv_row_to_today_instrument(&row);
        assert_eq!(ti.security_id, 13);
        assert_eq!(ti.underlying_security_id, 0, "index has no underlying");
        assert_eq!(ti.underlying_symbol, "");
        assert_eq!(ti.lot_size, 0);
        assert_eq!(ti.option_type, "");
        assert_eq!(ti.expiry_date, "");
    }

    #[test]
    fn test_csv_row_to_today_instrument_non_numeric_id_defaults_zero() {
        let row = CsvRow {
            security_id: "junk".to_string(),
            exch_id: "NSE".to_string(),
            segment: "NSE_EQ".to_string(),
            instrument: "EQUITY".to_string(),
            symbol_name: "X".to_string(),
            ..Default::default()
        };
        let ti = csv_row_to_today_instrument(&row);
        assert_eq!(ti.security_id, 0, "non-numeric security_id → 0");
    }

    #[test]
    fn test_to_classification_attrs_projects_six_fields() {
        let ti = csv_row_to_today_instrument(&detailed_row());
        let attrs = ti.to_classification_attrs();
        assert_eq!(attrs.security_id, 43581);
        assert_eq!(attrs.exchange_segment, "NSE_FNO");
        assert_eq!(attrs.instrument_type, "OPTSTK");
        assert_eq!(attrs.lot_size, 175);
        assert!((attrs.tick_size - 0.05).abs() < f64::EPSILON);
        assert_eq!(attrs.symbol_name, "TCS-CE");
    }

    #[test]
    fn test_extract_today_instruments_maps_all_targets_in_order() {
        let universe = DailyUniverse {
            subscription_targets: vec![
                SubscriptionTarget {
                    role: InstrumentRole::Index,
                    csv_row: CsvRow {
                        security_id: "13".to_string(),
                        exch_id: "NSE".to_string(),
                        segment: "IDX_I".to_string(),
                        instrument: "INDEX".to_string(),
                        symbol_name: "NIFTY".to_string(),
                        ..Default::default()
                    },
                },
                SubscriptionTarget {
                    role: InstrumentRole::FnoUnderlying,
                    csv_row: CsvRow {
                        security_id: "11536".to_string(),
                        exch_id: "NSE".to_string(),
                        segment: "NSE_EQ".to_string(),
                        instrument: "EQUITY".to_string(),
                        symbol_name: "TCS".to_string(),
                        ..Default::default()
                    },
                },
            ],
        };
        let out = extract_today_instruments(&universe);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].security_id, 13);
        assert_eq!(out[0].exchange_segment, "IDX_I");
        assert_eq!(out[1].security_id, 11536);
        assert_eq!(out[1].exchange_segment, "NSE_EQ");
    }

    #[test]
    fn test_extract_today_instruments_empty_universe() {
        let universe = DailyUniverse {
            subscription_targets: vec![],
        };
        assert!(extract_today_instruments(&universe).is_empty());
    }
}
