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

/// Plausible epoch-seconds window for the float-timestamp fallback sanity
/// check (2026-07-15 wire drift — see [`timestamp_epoch_secs`]): 1e9 =
/// 2001-09-09, 4e9 = 2096-10-02. A float outside this window (incl. NaN /
/// ±inf / negative / zero) is garbage, never a Dhan candle timestamp.
const EPOCH_SECS_FLOAT_MIN: f64 = 1.0e9;
/// Upper bound of the float-timestamp sanity window (see above).
const EPOCH_SECS_FLOAT_MAX: f64 = 4.0e9;

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

/// Tolerant epoch-seconds read for the `timestamp` array: JSON int OR float.
///
/// **2026-07-15 wire drift (probe-verified):** Dhan `/v2/charts/intraday`
/// started serializing `timestamp` as scientific-notation JSON floats
/// (verbatim wire: `"timestamp":[1.7840871E9,1.78408716E9]`). `as_i64()`
/// returns `None` for ANY float number, so every candle was skipped —
/// 14 days of 0-row spot-1m capture (`empty_no_rows` on all SIDs every
/// minute). Mirror of the pre-existing `volume` float fallback, PLUS a
/// sanity window: a float timestamp must be finite and inside
/// [`EPOCH_SECS_FLOAT_MIN`, `EPOCH_SECS_FLOAT_MAX`] (rejects NaN / inf /
/// negative / zero / absurd), then rounds to whole seconds (f64 represents
/// these epoch magnitudes exactly). A failing value returns `None` → the
/// caller's existing per-candle skip arm (callers count parsed rows and
/// classify empty — never silent). Integer timestamps are unchanged
/// (back-compat). Pure — never panics.
fn timestamp_epoch_secs(v: &serde_json::Value) -> Option<i64> {
    if let Some(i) = v.as_i64() {
        return Some(i);
    }
    let f = v.as_f64()?;
    if !f.is_finite() || !(EPOCH_SECS_FLOAT_MIN..=EPOCH_SECS_FLOAT_MAX).contains(&f) {
        return None;
    }
    // Bounded by the range check above (≤ 4e9 ≪ i64::MAX) — safe cast.
    Some(f.round() as i64)
}

/// Parse Dhan's columnar intraday response into `MinuteCandle`s. Parallel
/// arrays `open/high/low/close/volume/timestamp`; all must be the same
/// non-zero length. Returns empty on malformed/empty. Pure — never panics.
/// `timestamp` entries accept int OR finite ranged float per
/// [`timestamp_epoch_secs`] (the 2026-07-15 Dhan wire drift).
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
            timestamp_epoch_secs(&ts[i]),
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

/// H1 (audit 2026-07-20, Dim A): discriminate a ZERO-candle parse of a
/// Dhan intraday body into "well-formed empty day" vs "malformed/wrong
/// shape". Only the strict well-formed-empty shape — valid JSON carrying
/// ALL six columnar arrays at equal length ZERO — is a benign vendor
/// empty. Everything else that parses to zero candles is MALFORMED:
/// non-JSON, missing arrays, length mismatch, AND a rows>0 body whose
/// EVERY row fails value parsing (the exact 2026-07-15 float-timestamp
/// wire-drift class that produced 14 silent 0-row days as
/// `empty_no_rows`). Callers use this ONLY when
/// [`parse_intraday_1m_candles`] returned zero candles. Pure — never
/// panics.
#[must_use]
pub fn zero_candle_body_is_malformed(body: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return true;
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
        return true;
    };
    let n = ts.len();
    // Well-formed empty: every parallel array present at equal length 0.
    // Any non-zero length here means the caller's zero-candle parse
    // skipped EVERY row (value drift) — malformed, never a benign empty.
    !(n == 0
        && open.is_empty()
        && high.is_empty()
        && low.is_empty()
        && close.is_empty()
        && vol.is_empty())
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

    /// H1 (audit 2026-07-20): the zero-candle discriminator — only the
    /// strict well-formed six-empty-arrays shape is a benign vendor
    /// empty; every other zero-candle body is malformed, INCLUDING a
    /// rows>0 body whose every row fails value parsing (the 2026-07-15
    /// float-timestamp incident class).
    #[test]
    fn zero_candle_body_discriminates_malformed_vs_empty() {
        // Well-formed empty day: NOT malformed.
        let empty = r#"{"open":[],"high":[],"low":[],"close":[],"volume":[],"timestamp":[]}"#;
        assert!(parse_intraday_1m_candles(empty).is_empty());
        assert!(!zero_candle_body_is_malformed(empty));
        // Garbage / wrong shape / missing arrays / length mismatch: malformed.
        assert!(zero_candle_body_is_malformed("not json"));
        assert!(zero_candle_body_is_malformed("{}"));
        assert!(zero_candle_body_is_malformed(
            r#"{"open":[],"high":[],"low":[],"close":[],"volume":[]}"#
        ));
        assert!(zero_candle_body_is_malformed(
            r#"{"open":[1.0],"high":[1.0],"low":[1.0],"close":[1.0],"volume":[1],"timestamp":[1,2]}"#
        ));
        // Rows present but EVERY row value-unparseable (all-null OHLC):
        // parses to zero candles AND classifies malformed — value drift,
        // never a benign empty.
        let value_drift = r#"{"open":[null],"high":[null],"low":[null],"close":[null],"volume":[null],"timestamp":[null]}"#;
        assert!(parse_intraday_1m_candles(value_drift).is_empty());
        assert!(zero_candle_body_is_malformed(value_drift));
        // A HEALTHY body is out of the discriminator's domain (callers
        // consult it only on a zero-candle parse) — but it still answers
        // honestly: rows>0 ⇒ "not the well-formed-empty shape".
        let healthy = r#"{"open":[1.0],"high":[1.0],"low":[1.0],"close":[1.0],"volume":[1],"timestamp":[1780380120]}"#;
        assert_eq!(parse_intraday_1m_candles(healthy).len(), 1);
    }

    #[test]
    fn parse_intraday_1m_candles_volume_as_float_truncates() {
        let body = r#"{"open":[1.0],"high":[1.0],"low":[1.0],"close":[1.0],"volume":[12345.0],"timestamp":[1780380120]}"#;
        let c = parse_intraday_1m_candles(body);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].volume, 12345);
    }

    /// 2026-07-15 wire drift: the VERBATIM captured Dhan body (SENSEX,
    /// raw_body_sample 09:16:01 IST) — `timestamp` as scientific-notation
    /// floats. Both candles MUST parse, with exact epoch seconds
    /// 1784087100 (09:15 IST) and 1784087160 (09:16 IST).
    #[test]
    fn parse_intraday_1m_candles_float_scientific_timestamps_verbatim_wire() {
        let body = r#"{"open":[57643.75,57694.9],"high":[57751.65,57699.85],"low":[57616.95,57691.05],"close":[57695.15,57691.05],"volume":[4340422.0,19095.0],"timestamp":[1.7840871E9,1.78408716E9]}"#;
        let c = parse_intraday_1m_candles(body);
        assert_eq!(c.len(), 2, "both float-timestamp candles parse");
        assert_eq!(
            c[0].minute_ts_ist_nanos,
            intraday_utc_secs_to_ist_minute_nanos(1_784_087_100),
            "1.7840871E9 == exactly 1784087100 epoch seconds"
        );
        assert_eq!(
            c[1].minute_ts_ist_nanos,
            intraday_utc_secs_to_ist_minute_nanos(1_784_087_160),
            "1.78408716E9 == exactly 1784087160 epoch seconds"
        );
        assert_eq!(c[0].open, 57643.75);
        assert_eq!(c[0].volume, 4_340_422);
        assert_eq!(c[1].close, 57691.05);
        assert_eq!(c[1].volume, 19_095);
    }

    /// Back-compat: an INTEGER timestamp and its float form yield the
    /// identical minute bucket (the pre-drift wire shape keeps working).
    #[test]
    fn parse_intraday_1m_candles_int_and_float_timestamps_agree() {
        let int_body = r#"{"open":[1.0],"high":[1.0],"low":[1.0],"close":[1.0],"volume":[1],"timestamp":[1784087100]}"#;
        let float_body = r#"{"open":[1.0],"high":[1.0],"low":[1.0],"close":[1.0],"volume":[1],"timestamp":[1.7840871E9]}"#;
        let i = parse_intraday_1m_candles(int_body);
        let f = parse_intraday_1m_candles(float_body);
        assert_eq!(i.len(), 1);
        assert_eq!(f.len(), 1);
        assert_eq!(i[0].minute_ts_ist_nanos, f[0].minute_ts_ist_nanos);
    }

    /// Hostile float timestamps: negative, zero, and out-of-range (5e9)
    /// floats fail the sanity window and skip THAT candle only — the good
    /// candle in the same body still parses (per-candle skip, never a
    /// whole-body reject).
    #[test]
    fn parse_intraday_1m_candles_float_timestamp_sanity_window() {
        let body = r#"{"open":[1.0,2.0,3.0,4.0],"high":[1.0,2.0,3.0,4.0],"low":[1.0,2.0,3.0,4.0],"close":[1.0,2.0,3.0,4.0],"volume":[1,2,3,4],"timestamp":[-1.7840871E9,0.0,5.0E9,1.7840871E9]}"#;
        let c = parse_intraday_1m_candles(body);
        assert_eq!(c.len(), 1, "only the in-range float survives");
        assert_eq!(c[0].open, 4.0, "the surviving candle is the 4th column");
        assert_eq!(
            c[0].minute_ts_ist_nanos,
            intraday_utc_secs_to_ist_minute_nanos(1_784_087_100)
        );
    }

    /// Hostile overflow: `1e999` overflows f64. serde_json rejects the
    /// number at parse time (non-finite Numbers are unrepresentable), so
    /// the whole body degrades to empty; if a future serde ever yielded
    /// inf instead, the `is_finite()` gate skips the candle. Either way:
    /// zero bogus rows.
    #[test]
    fn parse_intraday_1m_candles_float_timestamp_overflow_yields_no_rows() {
        let body = r#"{"open":[1.0],"high":[1.0],"low":[1.0],"close":[1.0],"volume":[1],"timestamp":[1e999]}"#;
        assert!(parse_intraday_1m_candles(body).is_empty());
    }

    /// A fractional float timestamp rounds to the nearest whole second
    /// (defensive — Dhan epochs are whole seconds; f64 is exact at these
    /// magnitudes).
    #[test]
    fn parse_intraday_1m_candles_float_timestamp_fractional_rounds() {
        let body = r#"{"open":[1.0],"high":[1.0],"low":[1.0],"close":[1.0],"volume":[1],"timestamp":[1784087099.9]}"#;
        let c = parse_intraday_1m_candles(body);
        assert_eq!(c.len(), 1);
        assert_eq!(
            c[0].minute_ts_ist_nanos,
            intraday_utc_secs_to_ist_minute_nanos(1_784_087_100),
            "rounds to 1784087100, not truncates to ...099"
        );
    }
}
