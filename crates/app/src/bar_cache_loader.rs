//! Wave 7-A4 sub-PR #4 (W7-A4.4) — boot-time rehydration loader for
//! `BarCache`.
//!
//! Pure-logic primitives that read today's + yesterday's sealed-bar
//! rows from a QuestDB `/exec` JSON response and populate
//! [`tickvault_trading::in_mem::BarCache`]. The async wrapper that
//! issues the actual HTTP GET is a follow-up (mirrors the
//! `prev_day_cache_loader::populate_prev_day_cache_at_boot` pattern
//! which shipped the async wrapper alongside the parser in PR #520).
//!
//! ## Why split parse + populate from the network call
//!
//! Following `prev_day_cache_loader.rs`'s pattern: the pure-logic
//! pieces are deterministic, testable without QuestDB, and ratchet
//! cleanly. The HTTP-bearing wrapper is glue — adding it without
//! tests on the parser first leaves the I-P1-11 segment-mapping +
//! TfIndex-parsing invariants un-pinned.
//!
//! ## Schema contract
//!
//! Expected QuestDB dataset shape (`/exec` response `dataset` array):
//!
//! ```json
//! [
//!   ["NSE_FNO", "1m", 12345, 540, 100.0, 105.0, 95.0, 102.0, 1000, 500000, 42],
//!   ...
//! ]
//! ```
//!
//! Columns in order:
//! 1. `exchange_segment` SYMBOL (mapped to `u8` via `parse_segment_code`)
//! 2. `timeframe` SYMBOL (`"1m"`, `"5m"`, ..., `"1d"`) → [`TfIndex`]
//! 3. `security_id` LONG → u32
//! 4. `bucket_start_ist_secs` INT → u32
//! 5. `open` DOUBLE → f64
//! 6. `high` DOUBLE → f64
//! 7. `low` DOUBLE → f64
//! 8. `close` DOUBLE → f64
//! 9. `volume` LONG → i64
//! 10. `oi` LONG → i64
//! 11. `tick_count` INT → u32
//!
//! The shadow tables (`candles_<tf>_shadow`, Wave 6 sub-PR #1 item
//! 1.4a) are union-queried via `UNION ALL` so a single SELECT yields
//! every TF in one dataset.

use serde_json::Value;
use tickvault_trading::candles::TfIndex;
use tickvault_trading::in_mem::{BarCache, CompactBar};

/// One parsed row from the QuestDB dataset response. Carries all the
/// dimensions needed to insert into [`BarCache`] in one shot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarCacheRow {
    pub security_id: u32,
    pub exchange_segment_code: u8,
    pub tf: TfIndex,
    pub bar: CompactBar,
}

/// Outcome of parsing a single QuestDB dataset.
///
/// Mirrors `prev_day_cache_loader::ParseResult` — the counters let
/// the caller distinguish "table empty" (`rows.is_empty()`) from
/// "parse failure" (`skipped_malformed > 0`).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ParseResult {
    pub rows: Vec<BarCacheRow>,
    pub skipped_unknown_segment: u32,
    pub skipped_unknown_tf: u32,
    pub skipped_malformed: u32,
}

/// Parse a QuestDB `dataset` JSON array into [`BarCacheRow`] entries.
///
/// Pure function — no I/O, no allocation beyond the returned `Vec`.
/// O(N) over the input rows.
///
/// Cold path — called once per boot. The follow-up async wrapper
/// will pass `parsed_body.dataset` directly.
#[must_use]
pub fn parse_questdb_bar_dataset(dataset: &Value) -> ParseResult {
    let Some(rows_array) = dataset.as_array() else {
        // Dataset must be a JSON array per QuestDB /exec contract.
        return ParseResult {
            rows: Vec::new(),
            skipped_unknown_segment: 0,
            skipped_unknown_tf: 0,
            skipped_malformed: 1,
        };
    };
    let mut result = ParseResult {
        rows: Vec::with_capacity(rows_array.len()),
        ..Default::default()
    };
    for row in rows_array {
        match parse_single_row(row) {
            ParseOutcome::Ok(bar_row) => result.rows.push(bar_row),
            ParseOutcome::UnknownSegment => {
                result.skipped_unknown_segment = result.skipped_unknown_segment.saturating_add(1);
            }
            ParseOutcome::UnknownTf => {
                result.skipped_unknown_tf = result.skipped_unknown_tf.saturating_add(1);
            }
            ParseOutcome::Malformed => {
                result.skipped_malformed = result.skipped_malformed.saturating_add(1);
            }
        }
    }
    result
}

enum ParseOutcome {
    Ok(BarCacheRow),
    UnknownSegment,
    UnknownTf,
    Malformed,
}

fn parse_single_row(row: &Value) -> ParseOutcome {
    let Some(cols) = row.as_array() else {
        return ParseOutcome::Malformed;
    };
    // 11 columns expected per the schema contract above.
    if cols.len() != 11 {
        return ParseOutcome::Malformed;
    }
    let Some(segment_str) = cols[0].as_str() else {
        return ParseOutcome::Malformed;
    };
    let exchange_segment_code = match parse_segment_code(segment_str) {
        Some(code) => code,
        None => return ParseOutcome::UnknownSegment,
    };
    let Some(tf_str) = cols[1].as_str() else {
        return ParseOutcome::Malformed;
    };
    let tf = match parse_tf(tf_str) {
        Some(tf) => tf,
        None => return ParseOutcome::UnknownTf,
    };
    let Some(security_id_i64) = cols[2].as_i64() else {
        return ParseOutcome::Malformed;
    };
    if !(0..=i64::from(u32::MAX)).contains(&security_id_i64) {
        return ParseOutcome::Malformed;
    }
    let security_id = security_id_i64 as u32;
    let Some(bucket_start_i64) = cols[3].as_i64() else {
        return ParseOutcome::Malformed;
    };
    if !(0..=i64::from(u32::MAX)).contains(&bucket_start_i64) {
        return ParseOutcome::Malformed;
    }
    let bucket_start_ist_secs = bucket_start_i64 as u32;
    let Some(open) = cols[4].as_f64() else {
        return ParseOutcome::Malformed;
    };
    let Some(high) = cols[5].as_f64() else {
        return ParseOutcome::Malformed;
    };
    let Some(low) = cols[6].as_f64() else {
        return ParseOutcome::Malformed;
    };
    let Some(close) = cols[7].as_f64() else {
        return ParseOutcome::Malformed;
    };
    let Some(volume) = cols[8].as_i64() else {
        return ParseOutcome::Malformed;
    };
    let Some(oi) = cols[9].as_i64() else {
        return ParseOutcome::Malformed;
    };
    let Some(tick_count_i64) = cols[10].as_i64() else {
        return ParseOutcome::Malformed;
    };
    if !(0..=i64::from(u32::MAX)).contains(&tick_count_i64) {
        return ParseOutcome::Malformed;
    }
    let tick_count = tick_count_i64 as u32;
    ParseOutcome::Ok(BarCacheRow {
        security_id,
        exchange_segment_code,
        tf,
        bar: CompactBar {
            bucket_start_ist_secs,
            open,
            high,
            low,
            close,
            volume,
            oi,
            tick_count,
        },
    })
}

/// Map QuestDB `exchange_segment` SYMBOL to the `u8` code per
/// `dhan-annexure-enums.md` rule 2.
#[must_use]
fn parse_segment_code(s: &str) -> Option<u8> {
    match s {
        "IDX_I" => Some(0),
        "NSE_EQ" => Some(1),
        "NSE_FNO" => Some(2),
        "NSE_CURRENCY" => Some(3),
        "BSE_EQ" => Some(4),
        "MCX_COMM" => Some(5),
        "BSE_CURRENCY" => Some(7),
        "BSE_FNO" => Some(8),
        _ => None,
    }
}

/// Map timeframe SYMBOL to [`TfIndex`]. Mirrors the seal-time emit
/// labels used by the multi-TF aggregator.
#[must_use]
fn parse_tf(s: &str) -> Option<TfIndex> {
    match s {
        "1m" => Some(TfIndex::M1),
        "5m" => Some(TfIndex::M5),
        "15m" => Some(TfIndex::M15),
        "30m" => Some(TfIndex::M30),
        "1h" => Some(TfIndex::H1),
        "2h" => Some(TfIndex::H2),
        "3h" => Some(TfIndex::H3),
        "4h" => Some(TfIndex::H4),
        "1d" => Some(TfIndex::D1),
        _ => None,
    }
}

/// Insert all parsed rows into the [`BarCache`]. Returns the count
/// of inserts.
///
/// Cold path — called once per boot. Bounded by total bar count
/// (~5.5M today + 5.5M yesterday = ~11M) → ~5.5 seconds at ~2M/sec
/// papaya insert throughput. Well within the boot budget.
pub fn merge_into_cache(rows: &[BarCacheRow], cache: &BarCache) -> usize {
    let mut inserted: usize = 0;
    for row in rows {
        cache.insert(row.security_id, row.exchange_segment_code, row.tf, row.bar);
        inserted = inserted.saturating_add(1);
    }
    inserted
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn well_formed_row() -> Value {
        json!([
            "NSE_FNO", "1m", 12345, 540, 100.0, 105.0, 95.0, 102.0, 1000, 500000, 42
        ])
    }

    #[test]
    fn test_parse_well_formed_row() {
        let dataset = json!([well_formed_row()]);
        let result = parse_questdb_bar_dataset(&dataset);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.skipped_unknown_segment, 0);
        assert_eq!(result.skipped_unknown_tf, 0);
        assert_eq!(result.skipped_malformed, 0);
        let r = result.rows[0];
        assert_eq!(r.security_id, 12345);
        assert_eq!(r.exchange_segment_code, 2); // NSE_FNO
        assert_eq!(r.tf, TfIndex::M1);
        assert_eq!(r.bar.bucket_start_ist_secs, 540);
        assert_eq!(r.bar.close, 102.0);
        assert_eq!(r.bar.tick_count, 42);
    }

    #[test]
    fn test_parse_empty_dataset() {
        let dataset = json!([]);
        let result = parse_questdb_bar_dataset(&dataset);
        assert!(result.rows.is_empty());
        assert_eq!(result.skipped_malformed, 0);
    }

    #[test]
    fn test_parse_non_array_dataset_is_malformed() {
        let dataset = json!({"not": "an array"});
        let result = parse_questdb_bar_dataset(&dataset);
        assert!(result.rows.is_empty());
        assert_eq!(result.skipped_malformed, 1);
    }

    #[test]
    fn test_parse_skips_unknown_segment() {
        let row = json!([
            "FAKE_SEGMENT",
            "1m",
            1,
            540,
            100.0,
            105.0,
            95.0,
            102.0,
            1000,
            0,
            42
        ]);
        let result = parse_questdb_bar_dataset(&json!([row]));
        assert!(result.rows.is_empty());
        assert_eq!(result.skipped_unknown_segment, 1);
    }

    #[test]
    fn test_parse_skips_unknown_tf() {
        let row = json!([
            "NSE_FNO", "47s", 1, 540, 100.0, 105.0, 95.0, 102.0, 1000, 0, 42
        ]);
        let result = parse_questdb_bar_dataset(&json!([row]));
        assert!(result.rows.is_empty());
        assert_eq!(result.skipped_unknown_tf, 1);
    }

    #[test]
    fn test_parse_skips_wrong_column_count() {
        // 10 cols instead of 11
        let row = json!(["NSE_FNO", "1m", 1, 540, 100.0, 105.0, 95.0, 102.0, 1000, 0]);
        let result = parse_questdb_bar_dataset(&json!([row]));
        assert!(result.rows.is_empty());
        assert_eq!(result.skipped_malformed, 1);
    }

    #[test]
    fn test_parse_skips_security_id_overflow() {
        let row = json!([
            "NSE_FNO",
            "1m",
            5_000_000_000i64,
            540,
            100.0,
            105.0,
            95.0,
            102.0,
            1000,
            0,
            42
        ]);
        let result = parse_questdb_bar_dataset(&json!([row]));
        assert!(result.rows.is_empty());
        assert_eq!(result.skipped_malformed, 1);
    }

    #[test]
    fn test_parse_skips_negative_security_id() {
        let row = json!([
            "NSE_FNO", "1m", -1, 540, 100.0, 105.0, 95.0, 102.0, 1000, 0, 42
        ]);
        let result = parse_questdb_bar_dataset(&json!([row]));
        assert!(result.rows.is_empty());
        assert_eq!(result.skipped_malformed, 1);
    }

    #[test]
    fn test_parse_handles_mixed_rows() {
        let dataset = json!([
            well_formed_row(),
            json!(["BAD", "1m", 1, 540, 100.0, 105.0, 95.0, 102.0, 1000, 0, 42]),
            well_formed_row(),
        ]);
        let result = parse_questdb_bar_dataset(&dataset);
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.skipped_unknown_segment, 1);
    }

    #[test]
    fn test_parse_segment_code_covers_all_known() {
        assert_eq!(parse_segment_code("IDX_I"), Some(0));
        assert_eq!(parse_segment_code("NSE_EQ"), Some(1));
        assert_eq!(parse_segment_code("NSE_FNO"), Some(2));
        assert_eq!(parse_segment_code("NSE_CURRENCY"), Some(3));
        assert_eq!(parse_segment_code("BSE_EQ"), Some(4));
        assert_eq!(parse_segment_code("MCX_COMM"), Some(5));
        assert_eq!(parse_segment_code("BSE_CURRENCY"), Some(7));
        assert_eq!(parse_segment_code("BSE_FNO"), Some(8));
        assert_eq!(parse_segment_code("FAKE"), None);
        // I-P1-11: enum 6 has no mapping per dhan-annexure-enums.md
        // rule 2. Future-proofed.
    }

    #[test]
    fn test_parse_tf_covers_all_known() {
        assert_eq!(parse_tf("1m"), Some(TfIndex::M1));
        assert_eq!(parse_tf("5m"), Some(TfIndex::M5));
        assert_eq!(parse_tf("15m"), Some(TfIndex::M15));
        assert_eq!(parse_tf("30m"), Some(TfIndex::M30));
        assert_eq!(parse_tf("1h"), Some(TfIndex::H1));
        assert_eq!(parse_tf("2h"), Some(TfIndex::H2));
        assert_eq!(parse_tf("3h"), Some(TfIndex::H3));
        assert_eq!(parse_tf("4h"), Some(TfIndex::H4));
        assert_eq!(parse_tf("1d"), Some(TfIndex::D1));
        assert_eq!(parse_tf("1y"), None);
    }

    #[test]
    fn test_merge_into_cache_inserts_all_rows() {
        let cache = BarCache::new();
        let rows = vec![
            BarCacheRow {
                security_id: 12345,
                exchange_segment_code: 2,
                tf: TfIndex::M1,
                bar: CompactBar {
                    bucket_start_ist_secs: 540,
                    open: 100.0,
                    high: 105.0,
                    low: 95.0,
                    close: 102.0,
                    volume: 1000,
                    oi: 500_000,
                    tick_count: 42,
                },
            },
            BarCacheRow {
                security_id: 67890,
                exchange_segment_code: 1,
                tf: TfIndex::M5,
                bar: CompactBar {
                    bucket_start_ist_secs: 540,
                    open: 200.0,
                    high: 210.0,
                    low: 195.0,
                    close: 205.0,
                    volume: 2000,
                    oi: 0,
                    tick_count: 100,
                },
            },
        ];
        let inserted = merge_into_cache(&rows, &cache);
        assert_eq!(inserted, 2);
        assert_eq!(cache.len_bars(), 2);
        assert!(cache.lookup(12345, 2, TfIndex::M1, 540).is_some());
        assert!(cache.lookup(67890, 1, TfIndex::M5, 540).is_some());
    }

    /// I-P1-11 ratchet: same security_id, different segment must
    /// land as distinct cache entries.
    #[test]
    fn test_merge_into_cache_isolates_cross_segment_per_i_p1_11() {
        let cache = BarCache::new();
        let bar = CompactBar {
            bucket_start_ist_secs: 540,
            open: 100.0,
            high: 105.0,
            low: 95.0,
            close: 102.0,
            volume: 1000,
            oi: 0,
            tick_count: 42,
        };
        let rows = vec![
            BarCacheRow {
                security_id: 27,
                exchange_segment_code: 0,
                tf: TfIndex::M1,
                bar,
            }, // IDX_I
            BarCacheRow {
                security_id: 27,
                exchange_segment_code: 1,
                tf: TfIndex::M1,
                bar,
            }, // NSE_EQ
        ];
        let inserted = merge_into_cache(&rows, &cache);
        assert_eq!(inserted, 2);
        assert_eq!(cache.len_bars(), 2);
        assert!(cache.lookup(27, 0, TfIndex::M1, 540).is_some());
        assert!(cache.lookup(27, 1, TfIndex::M1, 540).is_some());
    }

    #[test]
    fn test_parse_and_merge_end_to_end() {
        let dataset = json!([
            [
                "NSE_FNO", "1m", 12345, 540, 100.0, 105.0, 95.0, 102.0, 1000, 500000, 42
            ],
            [
                "NSE_EQ", "5m", 67890, 600, 200.0, 210.0, 195.0, 205.0, 2000, 0, 100
            ],
            ["FAKE", "1m", 1, 540, 100.0, 105.0, 95.0, 102.0, 1000, 0, 42], // skipped
        ]);
        let cache = BarCache::new();
        let result = parse_questdb_bar_dataset(&dataset);
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.skipped_unknown_segment, 1);
        let inserted = merge_into_cache(&result.rows, &cache);
        assert_eq!(inserted, 2);
        assert_eq!(cache.len_bars(), 2);
    }
}
