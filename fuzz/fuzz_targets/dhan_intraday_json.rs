//! Fuzz target: the Dhan intraday REST body (`POST /v2/charts/intraday`,
//! interval "1"), the only outside record the daily 1-minute cross-check
//! compares our candles against.
//!
//! Properties beyond "no panic":
//!
//! * kept candles plus skipped rows never exceed the shortest column, and a
//!   body that is not six equal-length arrays gives nothing and no faults;
//! * every kept candle's minute stamp is a whole IST minute.
//!
//! The full row-by-row reference lives in
//! `crates/app/tests/adversarial_intraday_properties.rs`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tickvault_app::dhan_intraday_parse::{IntradayCellFaults, parse_intraday_1m_candles};

const MAX_INPUT: usize = 256 * 1024;
const COLUMNS: [&str; 6] = ["open", "high", "low", "close", "volume", "timestamp"];
const NANOS_PER_MINUTE: i64 = 60_000_000_000;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let (candles, faults) = parse_intraday_1m_candles(text);
    let lengths: Option<Vec<usize>> = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            COLUMNS
                .iter()
                .map(|k| v.get(*k).and_then(|c| c.as_array()).map(Vec::len))
                .collect()
        });
    match lengths {
        Some(lens) if lens.iter().all(|l| *l == lens[0]) && lens[0] > 0 => {
            assert_eq!(candles.len() + faults.skipped_rows, lens[0]);
            assert!(faults.coerced_volumes <= candles.len());
        }
        _ => {
            assert!(candles.is_empty(), "candles from a non-columnar body");
            assert_eq!(faults, IntradayCellFaults::default());
        }
    }
    for c in &candles {
        // Saturated extremes (i64::MIN/MAX) are not whole minutes; every
        // other stamp is.
        if c.minute_ts_ist_nanos != i64::MIN && c.minute_ts_ist_nanos != i64::MAX {
            assert_eq!(c.minute_ts_ist_nanos.rem_euclid(NANOS_PER_MINUTE), 0);
        }
    }
});
