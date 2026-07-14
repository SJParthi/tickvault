//! Shared Dhan `/v2/charts/intraday` 1-minute request/response primitives.
//!
//! **Relocated from `cross_verify_1m_boot.rs` in Phase C1 of the 2026-07-13
//! Dhan live-WS retirement** (operator directive 2026-07-13; the relocation
//! obligation is recorded in the `cross-verify-1m-error-codes.md` retirement
//! banner + the scope-lock "2026-07-13 Amendment" §B: `spot_1m_rest_boot`
//! imports these primitives, so they MUST outlive `cross_verify_1m_boot.rs`,
//! which the Phase C deletion PRs remove). Pure move — zero behavior change:
//! [`MinuteCandle`], [`intraday_request_body`],
//! [`intraday_utc_secs_to_ist_minute_nanos`] and
//! [`parse_intraday_1m_candles`] are byte-identical to their previous homes.
//!
//! Consumers: `spot_1m_rest_boot.rs` (the §8 per-minute spot leg — the
//! surviving consumer) and `cross_verify_1m_boot.rs` (until Phase C deletes
//! it). COLD PATH — once-per-minute / once-per-day REST parsing only.

use chrono::NaiveDate;
use serde_json::json;

use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;

/// Seconds per minute (local const — mirrors the previous home's literal).
const SECONDS_PER_MINUTE: i64 = 60;
/// Nanoseconds per second (local const — mirrors the previous home's literal).
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// One 1-minute candle, keyed by its IST-minute bucket. `volume` is `i64`
/// (exact integer compare). Prices are `f64` (our `candles_1m` and Dhan REST
/// are both f64 — exact compare per the operator's "exact match").
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MinuteCandle {
    /// IST-minute bucket start, in nanoseconds (the join key).
    pub minute_ts_ist_nanos: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
}

/// Build the `/v2/charts/intraday` request body for ONE spot symbol, interval
/// `"1"`, for a single trading day. `to_date` is the next calendar day so the
/// whole `from_date` session is captured (intraday uses datetime strings).
/// Pure.
#[must_use]
pub fn intraday_request_body(
    security_id: &str,
    exchange_segment: &str,
    instrument: &str,
    from_date: NaiveDate,
    to_date: NaiveDate,
) -> serde_json::Value {
    json!({
        "securityId": security_id,
        "exchangeSegment": exchange_segment,
        "instrument": instrument,
        "interval": "1",
        "oi": false,
        "fromDate": from_date.format("%Y-%m-%d 00:00:00").to_string(),
        "toDate": to_date.format("%Y-%m-%d 00:00:00").to_string(),
    })
}

/// Convert a Dhan intraday UTC-epoch-second timestamp into the IST-minute
/// bucket nanoseconds our `candles_1m` is keyed by. `data-integrity.md`:
/// historical/intraday REST timestamps are UTC epoch seconds → `+IST_UTC_OFFSET`
/// then floor to the minute, then ×1e9. O(1), saturating.
#[must_use]
pub fn intraday_utc_secs_to_ist_minute_nanos(utc_epoch_secs: i64) -> i64 {
    let ist_secs = utc_epoch_secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    let minute_floor = ist_secs - ist_secs.rem_euclid(SECONDS_PER_MINUTE);
    minute_floor.saturating_mul(NANOS_PER_SEC)
}

/// Parse Dhan's columnar intraday response into `MinuteCandle`s. Parallel
/// arrays `open/high/low/close/volume/timestamp`; all must be the same
/// non-zero length. Returns empty on malformed/empty. Pure — never panics.
#[must_use]
pub fn parse_intraday_1m_candles(body: &str) -> Vec<MinuteCandle> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let arr = |k: &str| v.get(k).and_then(|x| x.as_array());
    let (Some(open), Some(high), Some(low), Some(close), Some(vol), Some(ts)) = (
        arr("open"),
        arr("high"),
        arr("low"),
        arr("close"),
        arr("volume"),
        arr("timestamp"),
    ) else {
        return Vec::new();
    };
    let n = ts.len();
    if n == 0
        || open.len() != n
        || high.len() != n
        || low.len() != n
        || close.len() != n
        || vol.len() != n
    {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let (Some(o), Some(h), Some(l), Some(c), Some(t)) = (
            open[i].as_f64(),
            high[i].as_f64(),
            low[i].as_f64(),
            close[i].as_f64(),
            ts[i].as_i64(),
        ) else {
            continue;
        };
        let volume = vol[i]
            .as_i64()
            .or_else(|| vol[i].as_f64().map(|f| f as i64))
            .unwrap_or(0);
        out.push(MinuteCandle {
            minute_ts_ist_nanos: intraday_utc_secs_to_ist_minute_nanos(t),
            open: o,
            high: h,
            low: l,
            close: c,
            volume,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intraday_request_body_has_interval_1_and_string_sid() {
        let from = NaiveDate::from_ymd_opt(2026, 6, 2).expect("from");
        let to = NaiveDate::from_ymd_opt(2026, 6, 3).expect("to");
        let b = intraday_request_body("1333", "NSE_EQ", "EQUITY", from, to);
        assert_eq!(b["securityId"], "1333", "string sid");
        assert_eq!(b["interval"], "1", "1-minute interval as STRING");
        assert_eq!(b["exchangeSegment"], "NSE_EQ");
        assert_eq!(b["fromDate"], "2026-06-02 00:00:00");
        assert_eq!(b["toDate"], "2026-06-03 00:00:00");
    }

    #[test]
    fn intraday_utc_secs_to_ist_minute_nanos_floors_to_minute() {
        // 2026-06-02 06:02:07 UTC = 11:32:07 IST. Floor to 11:32:00 IST.
        let utc = 1_780_380_127_i64; // arbitrary epoch second
        let got = intraday_utc_secs_to_ist_minute_nanos(utc);
        // It must be a whole minute in IST nanos.
        assert_eq!(
            got % (SECONDS_PER_MINUTE * NANOS_PER_SEC),
            0,
            "floored to minute"
        );
        // And exactly IST offset + minute floor.
        let ist = utc + i64::from(IST_UTC_OFFSET_SECONDS);
        let expected = (ist - ist.rem_euclid(60)) * NANOS_PER_SEC;
        assert_eq!(got, expected);
    }

    #[test]
    fn parse_intraday_1m_candles_happy_path() {
        let body = r#"{"open":[100.0,101.0],"high":[100.5,101.5],"low":[99.5,100.5],"close":[100.2,101.2],"volume":[10,20],"timestamp":[1780380120,1780380180]}"#;
        let c = parse_intraday_1m_candles(body);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].open, 100.0);
        assert_eq!(c[1].volume, 20);
        // minute buckets differ by exactly 60s in nanos.
        assert_eq!(
            c[1].minute_ts_ist_nanos - c[0].minute_ts_ist_nanos,
            SECONDS_PER_MINUTE * NANOS_PER_SEC
        );
    }

    #[test]
    fn parse_intraday_1m_candles_rejects_length_mismatch_and_malformed() {
        assert!(parse_intraday_1m_candles("not json").is_empty());
        assert!(parse_intraday_1m_candles("{}").is_empty());
        let mismatch = r#"{"open":[1.0],"high":[1.0],"low":[1.0],"close":[1.0],"volume":[1],"timestamp":[1,2]}"#;
        assert!(parse_intraday_1m_candles(mismatch).is_empty());
    }

    #[test]
    fn parse_intraday_1m_candles_volume_as_float_truncates() {
        let body = r#"{"open":[1.0],"high":[1.0],"low":[1.0],"close":[1.0],"volume":[12345.0],"timestamp":[1780380120]}"#;
        let c = parse_intraday_1m_candles(body);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].volume, 12345);
    }
}
