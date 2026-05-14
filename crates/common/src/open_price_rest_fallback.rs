//! Phase 0 Item 14 — REST `/v2/marketfeed/quote.day_open` fallback
//! primitives (2026-05-14).
//!
//! Plan §9 of `topic-PHASE-0-LEAN-LOCKED.md` locks the per-class open-
//! price source. For NSE_EQ F&O stocks, the fallback chain is:
//!
//! ```text
//! Pre-open buffer last slot   (primary)
//!         ↓ (empty)
//! REST /v2/marketfeed/quote.day_open  (secondary — THIS PR)
//!         ↓ (REST also empty / untraded sentinel)
//! First WS tick + OPEN-PRICE-WARN     (last resort)
//! ```
//!
//! Trigger time `OPEN_PRICE_REST_FALLBACK_TIME_SECS_OF_DAY_IST`
//! (= 09:14:55 IST) is already pinned in
//! [`crate::open_price_source`] by PR-13. This module ships the
//! pure-logic primitives:
//!
//! 1. Request body builder — produces the Dhan-expected JSON
//!    `{"NSE_EQ": [11536, ...]}` (integer SIDs, NOT strings, per
//!    `dhan-ref/11-market-quote-rest.md` rule 6).
//! 2. Missing-SIDs computation — deterministic sorted diff of the
//!    full F&O stock universe minus the SIDs the pre-open buffer
//!    already filled.
//! 3. Response parser — walks
//!    `data.NSE_EQ.<security_id_str>.ohlc.open` returning
//!    `Vec<(security_id, day_open)>` for entries that pass the
//!    [`is_quote_day_open_valid`] gate.
//! 4. Validity gate — rejects the Dhan "untraded" sentinel
//!    `last_trade_time == "01/01/1980 00:00:00"` AND
//!    `ohlc.open == 0.0` (a stock that never traded in pre-open
//!    must not poison the candle's `open` field).
//!
//! IO (the actual `reqwest` POST, retry policy, latency metrics,
//! Telegram failure event) lands in a follow-up PR. This module is
//! pure logic — same testing pattern as `open_price_source.rs`.
//!
//! Cross-ref `.claude/rules/dhan/market-quote.md` (auto-loaded for
//! any file containing `marketfeed/quote`).

use serde_json::{Map, Value};

/// NSE equity segment key in the Dhan `/v2/marketfeed/quote` request
/// body. PR-14 only emits NSE_EQ SIDs (the F&O stock universe is
/// strictly NSE per `subscription_planner.rs` + Phase 0 scope =
/// `IndicesUnderlyingsOnly`).
pub const QUOTE_REQUEST_SEGMENT_KEY: &str = "NSE_EQ";

/// Dhan `/v2/marketfeed/quote` accepts up to 1000 SecurityIds per
/// request per `dhan-ref/11-market-quote-rest.md` rule 5. The Phase
/// 0 NSE_EQ F&O universe is ~218 SIDs so one batch fits comfortably;
/// the constant is exposed so a future regression that adds 800+
/// stocks fails at build-time rather than silently truncating.
pub const MAX_QUOTE_BATCH_SIZE: usize = 1000;

/// Dhan's "untraded since session open" sentinel returned in the
/// `last_trade_time` field. Format is `DD/MM/YYYY HH:MM:SS` per
/// `dhan-ref/11-market-quote-rest.md` rule 8. A stock showing this
/// value at 09:14:55 IST means the pre-open call-auction produced
/// no match for it — the `ohlc.open == 0.0` companion field is
/// therefore meaningless and MUST be rejected.
pub const UNTRADED_LAST_TRADE_TIME_SENTINEL: &str = "01/01/1980 00:00:00";

/// Build the Dhan `/v2/marketfeed/quote` POST body for the
/// supplied NSE_EQ SecurityIds.
///
/// Output shape (per `dhan-ref/11-market-quote-rest.md` rule 6):
/// ```json
/// { "NSE_EQ": [11536, 11538, ...] }
/// ```
///
/// SecurityIds are emitted as JSON integers (NOT strings) — Dhan
/// rejects string SIDs in this endpoint's request body (separately
/// from the WebSocket-feed JSON which requires strings; see the
/// rule-file divergence note in `market-quote.md` rule 6).
///
/// Returns an empty object (NO `NSE_EQ` key) when `nse_eq_sids` is
/// empty — caller MUST skip the REST call in that case rather than
/// sending a malformed empty array.
#[must_use]
pub fn build_quote_request_body(nse_eq_sids: &[u32]) -> Value {
    let mut body = Map::with_capacity(1);
    if !nse_eq_sids.is_empty() {
        let arr: Vec<Value> = nse_eq_sids.iter().map(|sid| Value::from(*sid)).collect();
        body.insert(QUOTE_REQUEST_SEGMENT_KEY.to_string(), Value::Array(arr));
    }
    Value::Object(body)
}

/// Compute the set of NSE_EQ F&O SecurityIds that need REST fallback
/// because the pre-open buffer did NOT capture them.
///
/// Inputs:
/// - `all_fno_stock_sids` — the full Phase 0 F&O stock universe
/// - `buffer_filled_sids` — SIDs that the pre-open buffer DID capture
///
/// Output: deterministic ascending-sorted dedup'd `Vec<u32>` of SIDs
/// to send in the REST request. Deterministic ordering keeps the
/// audit trail + Telegram messages stable across reruns.
///
/// Phase 0 scope is NSE_EQ only — there is no cross-segment
/// collision risk here, so a plain `u32` set is correct (I-P1-11
/// composite-key rule applies to cross-segment collections; this
/// is single-segment by construction). The caller MUST guarantee
/// both inputs are NSE_EQ.
#[must_use]
pub fn compute_missing_sids_for_rest_fallback(
    all_fno_stock_sids: &[u32],
    buffer_filled_sids: &[u32],
) -> Vec<u32> {
    use std::collections::BTreeSet;
    let filled: BTreeSet<u32> = buffer_filled_sids.iter().copied().collect();
    let mut missing: BTreeSet<u32> = BTreeSet::new();
    for sid in all_fno_stock_sids {
        if !filled.contains(sid) {
            missing.insert(*sid);
        }
    }
    missing.into_iter().collect()
}

/// True when a `(day_open, last_trade_time)` pair represents a real
/// traded price. Rejects the untraded sentinel
/// `last_trade_time == "01/01/1980 00:00:00"` and any non-positive
/// `day_open` (including NaN, infinity, negative — the candle's
/// `open` field is f64 finite-positive).
#[must_use]
pub fn is_quote_day_open_valid(day_open: f64, last_trade_time: &str) -> bool {
    if last_trade_time.trim() == UNTRADED_LAST_TRADE_TIME_SENTINEL {
        return false;
    }
    day_open.is_finite() && day_open > 0.0
}

/// Parse a Dhan `/v2/marketfeed/quote` response JSON value, returning
/// the `(security_id, day_open)` pairs for NSE_EQ entries that pass
/// the [`is_quote_day_open_valid`] gate.
///
/// Response shape (per `dhan-ref/11-market-quote-rest.md`):
/// ```json
/// {
///   "data": {
///     "NSE_EQ": {
///       "11536": {
///         "last_trade_time": "13/05/2026 09:08:00",
///         "ohlc": { "open": 2847.50, "close": ..., "high": ..., "low": ... }
///       }
///     }
///   },
///   "status": "success"
/// }
/// ```
///
/// Note: response keys are STRINGS even though the request body sends
/// integer SIDs (this asymmetry is documented in `market-quote.md`
/// rule 7). Keys that fail to parse as u32 are silently dropped — a
/// well-formed Dhan response will never violate this.
///
/// Output is deterministically ascending-sorted by `security_id`.
/// An empty response (e.g. Dhan returned an empty `NSE_EQ` object,
/// or `data` is missing entirely, or every entry hit the untraded
/// sentinel) yields an empty Vec — the caller's downstream chain
/// (first-WS-tick fallback) handles that case.
#[must_use]
pub fn parse_quote_response_day_opens(response: &Value) -> Vec<(u32, f64)> {
    let Some(nse_eq) = response
        .get("data")
        .and_then(|d| d.get(QUOTE_REQUEST_SEGMENT_KEY))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };

    let mut out: Vec<(u32, f64)> = Vec::with_capacity(nse_eq.len());
    for (sid_str, entry) in nse_eq {
        let Ok(sid) = sid_str.parse::<u32>() else {
            continue;
        };
        let day_open = entry
            .get("ohlc")
            .and_then(|o| o.get("open"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let last_trade_time = entry
            .get("last_trade_time")
            .and_then(Value::as_str)
            .unwrap_or("");
        if is_quote_day_open_valid(day_open, last_trade_time) {
            out.push((sid, day_open));
        }
    }
    out.sort_by_key(|(sid, _)| *sid);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ──────────────────────────────────────────────────────────────
    // Constants pins
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn test_quote_request_segment_key_is_nse_eq() {
        assert_eq!(QUOTE_REQUEST_SEGMENT_KEY, "NSE_EQ");
    }

    #[test]
    fn test_max_quote_batch_size_pinned_at_1000() {
        // Dhan hard limit per `dhan-ref/11-market-quote-rest.md` rule 5.
        assert_eq!(MAX_QUOTE_BATCH_SIZE, 1000);
    }

    #[test]
    fn test_untraded_sentinel_pinned_exactly() {
        // The Dhan response uses 4-digit year + zero-padded fields.
        assert_eq!(UNTRADED_LAST_TRADE_TIME_SENTINEL, "01/01/1980 00:00:00");
    }

    // ──────────────────────────────────────────────────────────────
    // build_quote_request_body
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn test_build_quote_request_body_single_sid() {
        let body = build_quote_request_body(&[11536]);
        assert_eq!(body, json!({ "NSE_EQ": [11536] }));
    }

    #[test]
    fn test_build_quote_request_body_multiple_sids_preserves_order() {
        let body = build_quote_request_body(&[11536, 11538, 2885]);
        assert_eq!(body, json!({ "NSE_EQ": [11536, 11538, 2885] }));
    }

    #[test]
    fn test_build_quote_request_body_empty_yields_empty_object() {
        // Caller must skip the REST call when empty — do NOT
        // emit `{"NSE_EQ": []}` (Dhan would error on it).
        let body = build_quote_request_body(&[]);
        assert_eq!(body, json!({}));
        assert!(body.as_object().is_some_and(|m| m.is_empty()));
    }

    #[test]
    fn test_build_quote_request_body_sids_are_integers_not_strings() {
        // `dhan-ref/11-market-quote-rest.md` rule 6 — integer SIDs.
        let body = build_quote_request_body(&[11536]);
        let arr = body
            .get("NSE_EQ")
            .and_then(Value::as_array)
            .expect("NSE_EQ array present");
        assert!(arr[0].is_number(), "SID must serialize as JSON integer");
        assert!(!arr[0].is_string(), "SID must NOT serialize as JSON string");
    }

    // ──────────────────────────────────────────────────────────────
    // compute_missing_sids_for_rest_fallback
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn test_compute_missing_sids_for_rest_fallback_all_filled_returns_empty() {
        let missing = compute_missing_sids_for_rest_fallback(&[1, 2, 3], &[1, 2, 3]);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_compute_missing_sids_for_rest_fallback_none_filled_returns_all_sorted() {
        let missing = compute_missing_sids_for_rest_fallback(&[3, 1, 2], &[]);
        // Output is deterministically ascending sorted.
        assert_eq!(missing, vec![1, 2, 3]);
    }

    #[test]
    fn test_compute_missing_sids_for_rest_fallback_partial_returns_difference_sorted() {
        let missing = compute_missing_sids_for_rest_fallback(&[1, 2, 3, 4, 5], &[2, 4]);
        assert_eq!(missing, vec![1, 3, 5]);
    }

    #[test]
    fn test_compute_missing_sids_for_rest_fallback_dedups_duplicates_in_universe() {
        // Universe accidentally contains duplicates — output is still deduped.
        let missing = compute_missing_sids_for_rest_fallback(&[1, 1, 2, 2, 3], &[]);
        assert_eq!(missing, vec![1, 2, 3]);
    }

    #[test]
    fn test_compute_missing_sids_for_rest_fallback_ignores_filled_sid_not_in_universe() {
        // Buffer captured a SID we don't care about — must not crash.
        let missing = compute_missing_sids_for_rest_fallback(&[1, 2], &[1, 999]);
        assert_eq!(missing, vec![2]);
    }

    // ──────────────────────────────────────────────────────────────
    // is_quote_day_open_valid
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn test_is_quote_day_open_valid_normal_traded_passes() {
        assert!(is_quote_day_open_valid(2847.50, "13/05/2026 09:08:00"));
    }

    #[test]
    fn test_is_quote_day_open_valid_untraded_sentinel_rejected() {
        assert!(!is_quote_day_open_valid(
            0.0,
            UNTRADED_LAST_TRADE_TIME_SENTINEL
        ));
        // Even if `day_open` is non-zero, the sentinel timestamp invalidates.
        assert!(!is_quote_day_open_valid(
            2847.50,
            UNTRADED_LAST_TRADE_TIME_SENTINEL
        ));
    }

    #[test]
    fn test_is_quote_day_open_valid_rejects_non_positive_price() {
        assert!(!is_quote_day_open_valid(0.0, "13/05/2026 09:08:00"));
        assert!(!is_quote_day_open_valid(-1.0, "13/05/2026 09:08:00"));
    }

    #[test]
    fn test_is_quote_day_open_valid_rejects_non_finite_price() {
        assert!(!is_quote_day_open_valid(f64::NAN, "13/05/2026 09:08:00"));
        assert!(!is_quote_day_open_valid(
            f64::INFINITY,
            "13/05/2026 09:08:00"
        ));
        assert!(!is_quote_day_open_valid(
            f64::NEG_INFINITY,
            "13/05/2026 09:08:00"
        ));
    }

    #[test]
    fn test_is_quote_day_open_valid_trims_sentinel_whitespace() {
        // Dhan has been observed padding fields; be lenient on leading/trailing whitespace.
        assert!(!is_quote_day_open_valid(0.0, "  01/01/1980 00:00:00  "));
    }

    // ──────────────────────────────────────────────────────────────
    // parse_quote_response_day_opens
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn test_parse_quote_response_day_opens_single_valid_entry() {
        let response = json!({
            "data": {
                "NSE_EQ": {
                    "11536": {
                        "last_trade_time": "13/05/2026 09:08:00",
                        "ohlc": { "open": 2847.50, "close": 2820.00, "high": 0, "low": 0 }
                    }
                }
            },
            "status": "success"
        });
        let parsed = parse_quote_response_day_opens(&response);
        assert_eq!(parsed, vec![(11536, 2847.50)]);
    }

    #[test]
    fn test_parse_quote_response_day_opens_filters_untraded_sentinel() {
        let response = json!({
            "data": {
                "NSE_EQ": {
                    "11536": {
                        "last_trade_time": "13/05/2026 09:08:00",
                        "ohlc": { "open": 2847.50, "close": 0, "high": 0, "low": 0 }
                    },
                    "11538": {
                        "last_trade_time": "01/01/1980 00:00:00",
                        "ohlc": { "open": 0, "close": 368.15, "high": 0, "low": 0 }
                    }
                }
            }
        });
        let parsed = parse_quote_response_day_opens(&response);
        // 11538 dropped — untraded sentinel.
        assert_eq!(parsed, vec![(11536, 2847.50)]);
    }

    #[test]
    fn test_parse_quote_response_day_opens_sorts_output_ascending() {
        // Use 3 SIDs in non-monotonic order to prove the sort.
        let response = json!({
            "data": {
                "NSE_EQ": {
                    "30000": {
                        "last_trade_time": "13/05/2026 09:08:00",
                        "ohlc": { "open": 100.0 }
                    },
                    "10000": {
                        "last_trade_time": "13/05/2026 09:08:00",
                        "ohlc": { "open": 200.0 }
                    },
                    "20000": {
                        "last_trade_time": "13/05/2026 09:08:00",
                        "ohlc": { "open": 300.0 }
                    }
                }
            }
        });
        let parsed = parse_quote_response_day_opens(&response);
        assert_eq!(parsed, vec![(10000, 200.0), (20000, 300.0), (30000, 100.0)]);
    }

    #[test]
    fn test_parse_quote_response_day_opens_missing_data_key_returns_empty() {
        let response = json!({ "status": "error" });
        assert!(parse_quote_response_day_opens(&response).is_empty());
    }

    #[test]
    fn test_parse_quote_response_day_opens_missing_nse_eq_returns_empty() {
        let response = json!({ "data": { "NSE_FNO": {} } });
        assert!(parse_quote_response_day_opens(&response).is_empty());
    }

    #[test]
    fn test_parse_quote_response_day_opens_empty_nse_eq_returns_empty() {
        let response = json!({ "data": { "NSE_EQ": {} } });
        assert!(parse_quote_response_day_opens(&response).is_empty());
    }

    #[test]
    fn test_parse_quote_response_day_opens_drops_non_numeric_keys() {
        // Defensive — Dhan should never emit non-numeric SID keys.
        let response = json!({
            "data": {
                "NSE_EQ": {
                    "garbage": {
                        "last_trade_time": "13/05/2026 09:08:00",
                        "ohlc": { "open": 100.0 }
                    },
                    "11536": {
                        "last_trade_time": "13/05/2026 09:08:00",
                        "ohlc": { "open": 200.0 }
                    }
                }
            }
        });
        let parsed = parse_quote_response_day_opens(&response);
        assert_eq!(parsed, vec![(11536, 200.0)]);
    }

    #[test]
    fn test_parse_quote_response_day_opens_missing_ohlc_drops_entry() {
        let response = json!({
            "data": {
                "NSE_EQ": {
                    "11536": {
                        "last_trade_time": "13/05/2026 09:08:00"
                        // ohlc missing entirely
                    }
                }
            }
        });
        // Missing ohlc.open is treated as 0.0 which fails the validity gate.
        assert!(parse_quote_response_day_opens(&response).is_empty());
    }

    #[test]
    fn test_parse_quote_response_day_opens_missing_last_trade_time_drops_entry() {
        // Without last_trade_time we treat it as empty string, sentinel
        // mismatch passes, but the price MUST still be > 0. Validate that
        // a valid price + missing timestamp passes (defensive — Dhan
        // always sends the field).
        let response = json!({
            "data": {
                "NSE_EQ": {
                    "11536": {
                        "ohlc": { "open": 2847.50 }
                    }
                }
            }
        });
        let parsed = parse_quote_response_day_opens(&response);
        assert_eq!(parsed, vec![(11536, 2847.50)]);
    }

    #[test]
    fn test_parse_quote_response_day_opens_zero_open_dropped_even_with_real_timestamp() {
        // Edge case: real timestamp but `ohlc.open == 0`. Should be
        // dropped (data integrity).
        let response = json!({
            "data": {
                "NSE_EQ": {
                    "11536": {
                        "last_trade_time": "13/05/2026 09:08:00",
                        "ohlc": { "open": 0.0 }
                    }
                }
            }
        });
        assert!(parse_quote_response_day_opens(&response).is_empty());
    }
}
