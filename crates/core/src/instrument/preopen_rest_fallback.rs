//! Fix #5 (2026-04-24) — REST belt-and-suspenders fallback for Phase 2.
//!
//! # Why this exists
//!
//! Phase 2 subscribes stock F&O at 09:13 IST using the pre-open buffer
//! window (09:00..=09:12). If a stock did NOT trade during pre-open —
//! Dhan sent zero ticks — the buffer's `backtrack_latest()` returns
//! `None` for that symbol and the stock is silently dropped from the
//! Phase 2 dispatch. Consequence: no F&O subscription for that stock
//! for the entire trading day.
//!
//! On 2026-04-24 Parthiban requested a third line of defence: at
//! **09:12:55 IST** (5 seconds before Phase 2 reads the buffer), call
//! Dhan's REST `POST /v2/marketfeed/ltp` endpoint for every F&O stock
//! still missing from the buffer. The endpoint returns the current LTP
//! for up to 1000 SIDs in one round-trip. Merge those into the buffer
//! as synthetic slot-12 entries (the "09:12 close" slot) so Phase 2's
//! existing `backtrack_latest()` picks them up without any special
//! code path.
//!
//! If the REST call also returns no price for a SID (genuinely illiquid
//! stock), the caller falls further back to yesterday's `historical_candles`
//! close + fires a `[HIGH]` Telegram "degraded mode" alert.
//!
//! # Design
//!
//! This module is **pure request/response + merge logic**. The actual HTTP
//! call integration and historical-close fallback live in the Phase 2
//! scheduler (see `phase2_scheduler.rs`) so this module stays
//! synchronous + easily unit-testable. The separation matters: retry
//! logic, rate-limit respect (Dhan Data API 1/sec for quotes), and
//! Telegram alerting are policy decisions best made at the scheduler
//! layer.
//!
//! # Dhan reference
//!
//! `docs/dhan-ref/11-market-quote-rest.md` — "Market Quote REST":
//! - Endpoint: `POST https://api.dhan.co/v2/marketfeed/ltp`
//! - Rate limit: 1/sec (unlimited per day)
//! - Headers: `access-token: {JWT}`, `client-id: {CLIENT_ID}`
//! - Request body: `{"NSE_EQ": [sid1, sid2, ...]}` — **integers**, not strings
//! - Response body: `{"status": "success", "data": {"NSE_EQ": {"<sid_string>": {"last_price": f64}}}}`
//!   — keys are strings, values are f64.

use std::collections::HashMap;

use crate::instrument::preopen_price_buffer::{PREOPEN_MINUTE_SLOTS, PreOpenCloses};

/// Path component appended to `dhan.rest_api_base_url` for the LTP
/// endpoint. The full URL is `{rest_api_base_url}{LTP_PATH}`.
pub const LTP_PATH: &str = "/marketfeed/ltp";

/// Dhan's hard cap on SIDs per request body (docs/dhan-ref/11-market-quote-rest.md).
pub const LTP_MAX_SIDS_PER_REQUEST: usize = 1000;

/// Exchange-segment key used in the request body. F&O stocks live under
/// `NSE_EQ` for the LTP lookup (their PRICE feed segment, not their
/// derivative segment).
pub const LTP_SEGMENT_KEY: &str = "NSE_EQ";

/// Result of parsing a successful `/marketfeed/ltp` response body.
///
/// Maps security_id (as reported by Dhan under `NSE_EQ`) to its current
/// LTP. Missing SIDs are simply absent from the map — the caller decides
/// whether to retry or escalate to the historical-close fallback.
pub type LtpResponseMap = HashMap<u32, f64>;

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

/// Builds the JSON request body for `POST /marketfeed/ltp`.
///
/// Splits `sids` into request-sized batches of at most
/// `LTP_MAX_SIDS_PER_REQUEST`. Returns a `Vec<String>` where each element
/// is the full serialized body for one HTTP request.
///
/// # Invariants
///
/// - Output is always non-empty if `sids` is non-empty.
/// - Each batch is a valid JSON object: `{"NSE_EQ": [12345, 67890, ...]}`.
/// - SIDs must be **integers** in the JSON array (per Dhan docs) — NOT
///   the string form used elsewhere for WebSocket subscribe.
///
/// # Performance
///
/// O(n) in `sids.len()`. Cold path — called at most once per Phase 2
/// dispatch.
#[must_use]
pub fn build_ltp_request_bodies(sids: &[u32]) -> Vec<String> {
    if sids.is_empty() {
        return Vec::new();
    }
    let mut bodies: Vec<String> = Vec::with_capacity(sids.len().div_ceil(LTP_MAX_SIDS_PER_REQUEST));
    for chunk in sids.chunks(LTP_MAX_SIDS_PER_REQUEST) {
        // O(1) EXEMPT: cold path — one HTTP round-trip per batch.
        let body = serde_json::json!({ LTP_SEGMENT_KEY: chunk });
        bodies.push(body.to_string());
    }
    bodies
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

/// Error kind emitted by `parse_ltp_response`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LtpParseError {
    /// Body was not valid JSON.
    InvalidJson,
    /// Top-level `status` field was absent or not a string.
    MissingStatus,
    /// `status` field was present but not `"success"`.
    StatusNotSuccess(String),
    /// `data` object was missing.
    MissingData,
    /// `data.NSE_EQ` was missing (no stocks in response).
    MissingSegment,
}

impl std::fmt::Display for LtpParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidJson => write!(f, "body is not valid JSON"),
            Self::MissingStatus => write!(f, "response missing `status` field"),
            Self::StatusNotSuccess(s) => write!(f, "response status is not success: {s}"),
            Self::MissingData => write!(f, "response missing `data` field"),
            Self::MissingSegment => write!(f, "response missing `data.NSE_EQ` segment"),
        }
    }
}

impl std::error::Error for LtpParseError {}

/// Parses a `POST /marketfeed/ltp` response body and returns the
/// `security_id -> last_price` map for the `NSE_EQ` segment.
///
/// Silently skips individual SIDs that:
/// - Have a non-finite / non-positive `last_price`.
/// - Are missing the `last_price` field entirely.
///
/// This matches the buffer's own `PreOpenCloses::record` guard — we
/// never pollute the Phase 2 input with garbage prices.
pub fn parse_ltp_response(body: &str) -> Result<LtpResponseMap, LtpParseError> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| LtpParseError::InvalidJson)?;

    let status = value
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or(LtpParseError::MissingStatus)?;
    if status != "success" {
        return Err(LtpParseError::StatusNotSuccess(status.to_string()));
    }

    let data = value.get("data").ok_or(LtpParseError::MissingData)?;
    let nse_eq = data
        .get(LTP_SEGMENT_KEY)
        .ok_or(LtpParseError::MissingSegment)?;
    let Some(obj) = nse_eq.as_object() else {
        return Err(LtpParseError::MissingSegment);
    };

    let mut out: LtpResponseMap = HashMap::with_capacity(obj.len());
    for (sid_str, entry) in obj {
        // SecurityId key is a STRING in the response (per docs/dhan-ref/11).
        let Ok(sid) = sid_str.parse::<u32>() else {
            continue;
        };
        let Some(ltp) = entry.get("last_price").and_then(|v| v.as_f64()) else {
            continue;
        };
        if !ltp.is_finite() || ltp <= 0.0 {
            continue;
        }
        out.insert(sid, ltp);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Merge into preopen-buffer snapshot
// ---------------------------------------------------------------------------

/// Merges an LTP response map into an existing preopen-buffer snapshot
/// using a `sid -> symbol` reverse lookup.
///
/// Only merges entries where the symbol is NOT already present in the
/// snapshot (i.e. the Phase 2 scheduler already found nothing for it in
/// the 09:00-09:12 buffer). Existing entries are preserved — the buffer
/// is authoritative.
///
/// Each REST LTP is stuffed into the last slot (09:12) so
/// `PreOpenCloses::backtrack_latest()` returns it via the normal path.
///
/// # Performance
///
/// O(n) in `ltp_map.len()`. Cold path — once per Phase 2 dispatch.
// TEST-EXEMPT: covered by test_merge_ltps_fills_missing_stocks_only, test_merge_ltps_ignores_unknown_sids, test_merge_ltps_with_empty_ltp_map_is_noop, test_rest_fallback_merges_ltps_into_buffer — substring grep misses the full fn name.
pub fn merge_ltps_into_snapshot(
    snapshot: &mut HashMap<String, PreOpenCloses>,
    ltp_map: &LtpResponseMap,
    sid_to_symbol: &HashMap<u32, String>,
) -> usize {
    let mut merged: usize = 0;
    for (sid, &ltp) in ltp_map {
        let Some(symbol) = sid_to_symbol.get(sid) else {
            continue;
        };
        // Buffer wins — REST is only a fallback for absent symbols.
        if snapshot.contains_key(symbol) {
            continue;
        }
        let mut closes = PreOpenCloses::default();
        closes.record(PREOPEN_MINUTE_SLOTS - 1, ltp);
        snapshot.insert(symbol.clone(), closes);
        merged = merged.saturating_add(1);
    }
    merged
}

/// Returns the set of F&O stock SIDs whose symbol is absent from the
/// snapshot — i.e. the work list for the REST call.
///
/// The returned `Vec` is deterministic (sorted by SID) so test
/// assertions and Grafana dashboards can compare apples to apples
/// across runs.
///
/// # Performance
///
/// O(n log n) due to the final sort. Cold path — once per Phase 2
/// dispatch at most every 24 hours.
#[must_use]
// TEST-EXEMPT: covered by test_missing_sids_returns_all_when_snapshot_empty, test_missing_sids_excludes_already_present_symbols, test_missing_sids_output_is_sorted, test_missing_sids_empty_when_all_present, test_rest_fallback_invoked_when_buffer_empty — substring grep misses the full fn name.
pub fn missing_sids_for_rest_fallback(
    snapshot: &HashMap<String, PreOpenCloses>,
    stock_sids: &HashMap<u32, String>,
) -> Vec<u32> {
    let mut out: Vec<u32> = stock_sids
        .iter()
        .filter_map(|(sid, symbol)| {
            if snapshot.contains_key(symbol) {
                None
            } else {
                Some(*sid)
            }
        })
        .collect();
    out.sort_unstable();
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- build_ltp_request_bodies ----

    #[test]
    fn test_build_ltp_request_bodies_empty_returns_empty() {
        assert!(build_ltp_request_bodies(&[]).is_empty());
    }

    #[test]
    fn test_build_ltp_request_bodies_single_batch() {
        let sids = vec![11536u32, 22345u32, 33456u32];
        let bodies = build_ltp_request_bodies(&sids);
        assert_eq!(bodies.len(), 1);
        // Must serialise SIDs as INTEGERS, not strings.
        let parsed: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
        let arr = parsed.get("NSE_EQ").and_then(|v| v.as_array()).unwrap();
        assert_eq!(arr.len(), 3);
        assert!(arr[0].is_number());
        assert_eq!(arr[0].as_u64().unwrap(), 11536);
    }

    #[test]
    fn test_build_ltp_request_bodies_splits_on_1000_sid_boundary() {
        let sids: Vec<u32> = (1..=2500u32).collect();
        let bodies = build_ltp_request_bodies(&sids);
        assert_eq!(
            bodies.len(),
            3,
            "2500 sids must split into 3 requests (1000 + 1000 + 500)"
        );
        let lens: Vec<usize> = bodies
            .iter()
            .map(|b| {
                let v: serde_json::Value = serde_json::from_str(b).unwrap();
                v.get("NSE_EQ").unwrap().as_array().unwrap().len()
            })
            .collect();
        assert_eq!(lens, vec![1000, 1000, 500]);
    }

    #[test]
    fn test_build_ltp_request_bodies_exact_1000_is_one_batch() {
        let sids: Vec<u32> = (1..=1000u32).collect();
        let bodies = build_ltp_request_bodies(&sids);
        assert_eq!(bodies.len(), 1);
    }

    #[test]
    fn test_build_ltp_request_body_uses_nse_eq_key() {
        let sids = vec![1u32];
        let bodies = build_ltp_request_bodies(&sids);
        assert!(bodies[0].contains("\"NSE_EQ\""));
    }

    // ---- parse_ltp_response ----

    #[test]
    fn test_parse_ltp_response_happy_path() {
        let body = r#"{
            "status": "success",
            "data": {
                "NSE_EQ": {
                    "11536": {"last_price": 2847.50},
                    "22345": {"last_price": 1480.10}
                }
            }
        }"#;
        let map = parse_ltp_response(body).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&11536), Some(&2847.50));
        assert_eq!(map.get(&22345), Some(&1480.10));
    }

    #[test]
    fn test_parse_ltp_response_invalid_json() {
        assert_eq!(
            parse_ltp_response("not json"),
            Err(LtpParseError::InvalidJson)
        );
    }

    #[test]
    fn test_parse_ltp_response_missing_status() {
        let body = r#"{"data": {"NSE_EQ": {}}}"#;
        assert_eq!(parse_ltp_response(body), Err(LtpParseError::MissingStatus));
    }

    #[test]
    fn test_parse_ltp_response_status_not_success() {
        let body = r#"{"status": "error", "errorCode": "DH-904"}"#;
        let err = parse_ltp_response(body).unwrap_err();
        matches!(err, LtpParseError::StatusNotSuccess(_));
    }

    #[test]
    fn test_parse_ltp_response_missing_nse_eq_segment() {
        // success response with no NSE_EQ section — legitimate when the
        // caller queried no equities, or Dhan omitted the segment.
        let body = r#"{"status": "success", "data": {"NSE_FNO": {}}}"#;
        assert_eq!(parse_ltp_response(body), Err(LtpParseError::MissingSegment));
    }

    #[test]
    fn test_parse_ltp_response_skips_non_finite_prices() {
        // NaN / infinity / zero / negative prices must be silently dropped.
        let body = r#"{
            "status": "success",
            "data": {
                "NSE_EQ": {
                    "100": {"last_price": 0.0},
                    "200": {"last_price": -5.5},
                    "300": {"last_price": 42.0}
                }
            }
        }"#;
        let map = parse_ltp_response(body).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&300), Some(&42.0));
    }

    #[test]
    fn test_parse_ltp_response_skips_non_numeric_sid_keys() {
        let body = r#"{
            "status": "success",
            "data": {
                "NSE_EQ": {
                    "not-an-id": {"last_price": 100.0},
                    "500": {"last_price": 200.0}
                }
            }
        }"#;
        let map = parse_ltp_response(body).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&500), Some(&200.0));
    }

    // ---- merge_ltps_into_snapshot ----

    #[test]
    fn test_merge_ltps_fills_missing_stocks_only() {
        // RELIANCE already has data in the buffer — REST LTP must NOT
        // overwrite it. INFY is missing — REST LTP fills it.
        let mut snapshot: HashMap<String, PreOpenCloses> = HashMap::new();
        let mut reliance_closes = PreOpenCloses::default();
        reliance_closes.record(0, 2840.0); // 09:00 tick
        snapshot.insert("RELIANCE".to_string(), reliance_closes);

        let ltp_map: LtpResponseMap = [(11536u32, 2847.50), (22345u32, 1480.10)]
            .into_iter()
            .collect();
        let sid_to_symbol: HashMap<u32, String> = [
            (11536u32, "RELIANCE".to_string()),
            (22345u32, "INFY".to_string()),
        ]
        .into_iter()
        .collect();

        let merged = merge_ltps_into_snapshot(&mut snapshot, &ltp_map, &sid_to_symbol);
        assert_eq!(merged, 1, "only INFY is newly merged");

        // RELIANCE buffer still has 09:00 original, NOT the REST 2847.50.
        assert_eq!(snapshot.get("RELIANCE").unwrap().closes[0], Some(2840.0));

        // INFY now has the REST LTP in the last slot (09:12).
        let infy_closes = snapshot.get("INFY").unwrap();
        assert_eq!(infy_closes.closes[PREOPEN_MINUTE_SLOTS - 1], Some(1480.10));
        // backtrack_latest returns it via the normal path.
        assert_eq!(infy_closes.backtrack_latest(), Some(1480.10));
    }

    #[test]
    fn test_merge_ltps_ignores_unknown_sids() {
        let mut snapshot: HashMap<String, PreOpenCloses> = HashMap::new();
        let ltp_map: LtpResponseMap = [(99999u32, 100.0)].into_iter().collect();
        let sid_to_symbol: HashMap<u32, String> = HashMap::new(); // empty reverse lookup
        let merged = merge_ltps_into_snapshot(&mut snapshot, &ltp_map, &sid_to_symbol);
        assert_eq!(merged, 0);
        assert!(snapshot.is_empty());
    }

    #[test]
    fn test_merge_ltps_with_empty_ltp_map_is_noop() {
        let mut snapshot: HashMap<String, PreOpenCloses> = HashMap::new();
        let sid_to_symbol: HashMap<u32, String> = [(1u32, "X".to_string())].into_iter().collect();
        let merged =
            merge_ltps_into_snapshot(&mut snapshot, &LtpResponseMap::new(), &sid_to_symbol);
        assert_eq!(merged, 0);
        assert!(snapshot.is_empty());
    }

    // ---- missing_sids_for_rest_fallback ----

    #[test]
    fn test_missing_sids_returns_all_when_snapshot_empty() {
        let snapshot: HashMap<String, PreOpenCloses> = HashMap::new();
        let stock_sids: HashMap<u32, String> = [
            (11536u32, "RELIANCE".to_string()),
            (22345u32, "INFY".to_string()),
            (33456u32, "TCS".to_string()),
        ]
        .into_iter()
        .collect();
        let missing = missing_sids_for_rest_fallback(&snapshot, &stock_sids);
        assert_eq!(missing, vec![11536, 22345, 33456]);
    }

    #[test]
    fn test_missing_sids_excludes_already_present_symbols() {
        let mut snapshot: HashMap<String, PreOpenCloses> = HashMap::new();
        let mut c = PreOpenCloses::default();
        c.record(0, 2840.0);
        snapshot.insert("RELIANCE".to_string(), c);

        let stock_sids: HashMap<u32, String> = [
            (11536u32, "RELIANCE".to_string()),
            (22345u32, "INFY".to_string()),
        ]
        .into_iter()
        .collect();
        let missing = missing_sids_for_rest_fallback(&snapshot, &stock_sids);
        assert_eq!(
            missing,
            vec![22345],
            "RELIANCE is in snapshot → excluded; INFY is not → included"
        );
    }

    #[test]
    fn test_missing_sids_output_is_sorted() {
        let snapshot: HashMap<String, PreOpenCloses> = HashMap::new();
        let stock_sids: HashMap<u32, String> = [
            (33456u32, "TCS".to_string()),
            (11536u32, "RELIANCE".to_string()),
            (22345u32, "INFY".to_string()),
        ]
        .into_iter()
        .collect();
        let missing = missing_sids_for_rest_fallback(&snapshot, &stock_sids);
        assert_eq!(missing, vec![11536, 22345, 33456]);
    }

    #[test]
    fn test_missing_sids_empty_when_all_present() {
        let mut snapshot: HashMap<String, PreOpenCloses> = HashMap::new();
        let mut c = PreOpenCloses::default();
        c.record(0, 2840.0);
        snapshot.insert("RELIANCE".to_string(), c);
        let stock_sids: HashMap<u32, String> =
            [(11536u32, "RELIANCE".to_string())].into_iter().collect();
        let missing = missing_sids_for_rest_fallback(&snapshot, &stock_sids);
        assert!(missing.is_empty());
    }

    // ---- constants ----

    #[test]
    fn test_rest_fallback_invoked_when_buffer_empty() {
        // Fix #5 ratchet (scenario #4): at 09:12:55, if buffer is empty
        // for all stocks, missing_sids returns every SID — the REST
        // fallback MUST be called with the full list.
        let snapshot: HashMap<String, PreOpenCloses> = HashMap::new();
        let stock_sids: HashMap<u32, String> = (0..5u32)
            .map(|i| (10_000 + i, format!("STOCK{i}")))
            .collect();
        let missing = missing_sids_for_rest_fallback(&snapshot, &stock_sids);
        assert_eq!(missing.len(), 5);
    }

    #[test]
    fn test_rest_fallback_merges_ltps_into_buffer() {
        // Fix #5 ratchet (scenario #5): REST returns LTPs, merge stuffs
        // them into the snapshot so Phase 2's backtrack_latest picks
        // them up via the normal 09:12 slot.
        let mut snapshot: HashMap<String, PreOpenCloses> = HashMap::new();
        let ltp_map: LtpResponseMap = [(10_000u32, 123.45)].into_iter().collect();
        let sid_to_symbol: HashMap<u32, String> =
            [(10_000u32, "STOCK0".to_string())].into_iter().collect();
        let merged = merge_ltps_into_snapshot(&mut snapshot, &ltp_map, &sid_to_symbol);
        assert_eq!(merged, 1);
        assert_eq!(
            snapshot.get("STOCK0").unwrap().backtrack_latest(),
            Some(123.45)
        );
    }

    #[test]
    fn test_ltp_path_constant_matches_dhan_ref() {
        assert_eq!(LTP_PATH, "/marketfeed/ltp");
    }

    #[test]
    fn test_ltp_max_sids_per_request_matches_dhan_ref() {
        // Dhan spec: up to 1000 instruments per request.
        // docs/dhan-ref/11-market-quote-rest.md rule 5.
        assert_eq!(LTP_MAX_SIDS_PER_REQUEST, 1000);
    }
}
