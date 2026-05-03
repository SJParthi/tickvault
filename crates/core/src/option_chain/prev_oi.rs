//! PR #450 commit 3 (2026-05-03) — `previous_oi` extraction from
//! Dhan Option Chain REST responses for the unified `/api/movers`
//! Dhan-parity OI Change calculations.
//!
//! # Why this exists
//!
//! Dhan's UI columns "OI Change" + "OI Change %" (visible on EVERY tab
//! of Markets > Options view) require `current_OI − prev_session_OI`.
//! Per `.claude/rules/dhan/live-market-feed.md` rule 7 (Dhan support
//! Ticket #5525125), the WS PrevClose packet (code 6) emits prev-day-OI
//! ONLY for IDX_I — NOT for NSE_FNO derivatives. Per
//! `docs/dhan-ref/11-market-quote-rest.md:144-169`, the
//! `/v2/marketfeed/quote` REST endpoint also doesn't carry it.
//!
//! The Dhan-canonical source for NSE_FNO option `previous_oi` is the
//! **Option Chain REST `/v2/optionchain`** response — every `OptionData`
//! row carries an explicit `previous_oi: i64` field
//! (`docs/dhan-ref/06-option-chain.md:90`).
//!
//! # What this module provides
//!
//! Pure functions to extract a `prev_oi` cache
//! (`HashMap<(security_id, exchange_segment_code), i64>`) from a parsed
//! `OptionChainResponse`. The caller is responsible for the actual REST
//! invocation + rate-limiting + audit-logging — those concerns live
//! upstream in `bhavcopy_pipeline.rs` (commit-3 follow-up wiring).
//!
//! # Hot-path
//!
//! Cold path. Per Agent 3 hot-path review (PR #450 deep research),
//! this runs ONCE per trading day at boot — the resulting `Arc<HashMap>`
//! is consumed by `spawn_movers_pipeline` as the `prev_oi_cache`
//! argument and read O(1) lock-free per 1s drain row.

use std::collections::HashMap;

use crate::option_chain::types::OptionChainResponse;

/// Builds the prev_oi cache from a single `OptionChainResponse`. Each
/// (CE / PE) leg with `previous_oi > 0` produces ONE entry keyed by
/// `(security_id_u32, exchange_segment_code)` per I-P1-11.
///
/// # `exchange_segment_code` source
///
/// Option Chain responses do NOT carry the exchange segment (the
/// caller queries by `(UnderlyingScrip, UnderlyingSeg)` so segment
/// is known at request time). Pass it in via `exchange_segment_code`
/// — typically `2` (NseFno) for NIFTY/BANKNIFTY/most stocks, or `8`
/// (BseFno) for SENSEX.
///
/// # Why we filter `previous_oi > 0`
///
/// Dhan returns `previous_oi: 0` for newly-listed contracts that have
/// never traded. Inserting `0` into the cache would make the
/// `oi_change_session` SQL view (`current_OI - prev_oi`) compute as
/// `current_OI - 0 = current_OI` for these contracts — misleading.
/// The consumer treats absence as "no baseline available" → renders
/// OI Change as `0` (correct semantic).
///
/// Defensive against negative values (Dhan should never emit them but
/// the schema is `i64` not `u64` per
/// `crates/core/src/option_chain/types.rs:90`): we silently skip
/// negative values.
///
/// # Hot path
///
/// O(N) where N = strike count × 2 (CE + PE) — typically ~150 strikes
/// per response = ~300 inserts per call. Cold path. Zero allocations
/// beyond the returned HashMap.
#[must_use]
pub fn extract_prev_oi_from_option_chain(
    response: &OptionChainResponse,
    exchange_segment_code: u8,
) -> HashMap<(u32, u8), i64> {
    let mut out: HashMap<(u32, u8), i64> = HashMap::with_capacity(response.data.oc.len() * 2);
    for strike in response.data.oc.values() {
        if let Some(ce) = &strike.ce {
            insert_if_positive(
                &mut out,
                ce.security_id,
                exchange_segment_code,
                ce.previous_oi,
            );
        }
        if let Some(pe) = &strike.pe {
            insert_if_positive(
                &mut out,
                pe.security_id,
                exchange_segment_code,
                pe.previous_oi,
            );
        }
    }
    out
}

/// Merges a per-underlying prev_oi cache into a global accumulator.
///
/// **Semantics: LAST-WINS** — `HashMap::insert` overwrites on collision,
/// so the LAST `merge_prev_oi_cache` call's values shadow earlier ones.
///
/// **Required call order for the PR #450 boot tier hierarchy:**
/// 1. Tier 3 (lowest priority): bhavcopy bulk merge first
/// 2. Tier 2 (medium): Historical Data REST merge (if used)
/// 3. Tier 1 (highest priority — Dhan-canonical): Option Chain REST
///    `previous_oi` merge LAST so its values win on collision.
///
/// Hostile-bug-hunt MEDIUM M1 (commit 8 review) fix: the prior
/// docstring said "iterate most-watched FIRST" — opposite of the
/// actual `HashMap::insert` last-wins semantic. The corrected
/// guidance is documented above and pinned by
/// `test_merge_prev_oi_cache_last_wins_on_collision`.
pub fn merge_prev_oi_cache(
    accumulator: &mut HashMap<(u32, u8), i64>,
    incremental: HashMap<(u32, u8), i64>,
) {
    accumulator.reserve(incremental.len());
    for (key, value) in incremental {
        accumulator.insert(key, value);
    }
}

#[inline]
fn insert_if_positive(
    out: &mut HashMap<(u32, u8), i64>,
    security_id_u64: u64,
    exchange_segment_code: u8,
    previous_oi: i64,
) {
    if previous_oi <= 0 {
        return;
    }
    // SecurityId in OptionData is u64 (per types.rs:94) but the rest of
    // the system uses u32. SecurityIds always fit u32 per Dhan's
    // protocol (binary header field is u32 LE). Defensive try_from
    // skips silently on overflow rather than panicking.
    let Ok(security_id_u32) = u32::try_from(security_id_u64) else {
        return;
    };
    out.insert((security_id_u32, exchange_segment_code), previous_oi);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::option_chain::types::{DhanGreeks, OptionChainData, OptionData, StrikeData};
    use std::collections::HashMap;

    fn make_option_data(security_id: u64, previous_oi: i64) -> OptionData {
        OptionData {
            average_price: 0.0,
            greeks: DhanGreeks {
                delta: 0.0,
                gamma: 0.0,
                theta: 0.0,
                vega: 0.0,
            },
            implied_volatility: 0.0,
            last_price: 0.0,
            oi: 0,
            previous_close_price: 0.0,
            previous_oi,
            previous_volume: 0,
            security_id,
            top_ask_price: 0.0,
            top_ask_quantity: 0,
            top_bid_price: 0.0,
            top_bid_quantity: 0,
            volume: 0,
        }
    }

    fn make_response_with_strikes(
        strikes: Vec<(String, Option<u64>, Option<u64>, i64, i64)>,
    ) -> OptionChainResponse {
        let mut oc: HashMap<String, StrikeData> = HashMap::new();
        for (strike_str, ce_sid, pe_sid, ce_prev, pe_prev) in strikes {
            oc.insert(
                strike_str,
                StrikeData {
                    ce: ce_sid.map(|sid| make_option_data(sid, ce_prev)),
                    pe: pe_sid.map(|sid| make_option_data(sid, pe_prev)),
                },
            );
        }
        OptionChainResponse {
            status: "success".to_string(),
            data: OptionChainData {
                last_price: 25_000.0,
                oc,
            },
        }
    }

    /// PR #450 commit 3 ratchet: extracts every CE + PE leg with
    /// positive previous_oi, keyed by composite (security_id,
    /// exchange_segment_code) per I-P1-11.
    #[test]
    fn test_extract_prev_oi_keys_by_composite_security_id_and_exchange_segment() {
        let resp = make_response_with_strikes(vec![
            ("25000.0".to_string(), Some(101), Some(102), 50_000, 60_000),
            ("25100.0".to_string(), Some(103), Some(104), 70_000, 80_000),
        ]);
        let cache = extract_prev_oi_from_option_chain(&resp, 2 /* NSE_FNO */);
        assert_eq!(cache.len(), 4);
        assert_eq!(cache.get(&(101, 2)), Some(&50_000));
        assert_eq!(cache.get(&(102, 2)), Some(&60_000));
        assert_eq!(cache.get(&(103, 2)), Some(&70_000));
        assert_eq!(cache.get(&(104, 2)), Some(&80_000));
    }

    /// PR #450 commit 3 ratchet: BSE options (e.g. SENSEX) get segment
    /// code 8 — distinct from NSE F&O code 2 — preserving I-P1-11.
    #[test]
    fn test_extract_prev_oi_segment_code_param_propagates_to_cache_key() {
        let resp =
            make_response_with_strikes(vec![("80000.0".to_string(), Some(500), None, 90_000, 0)]);
        let cache = extract_prev_oi_from_option_chain(&resp, 8 /* BSE_FNO */);
        assert_eq!(cache.get(&(500, 8)), Some(&90_000));
        // Same security_id under NSE_FNO segment is a DIFFERENT key.
        assert_eq!(cache.get(&(500, 2)), None);
    }

    /// PR #450 commit 3 ratchet: skip newly-listed contracts (previous_oi == 0)
    /// to avoid polluting the cache with sentinel-zero baselines.
    #[test]
    fn test_extract_prev_oi_skips_zero_baseline_for_newly_listed_contracts() {
        let resp = make_response_with_strikes(vec![
            ("25000.0".to_string(), Some(101), Some(102), 50_000, 0), // PE has no baseline
            ("25100.0".to_string(), Some(103), None, 0, 0),           // CE has no baseline
        ]);
        let cache = extract_prev_oi_from_option_chain(&resp, 2);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&(101, 2)), Some(&50_000));
        assert_eq!(cache.get(&(102, 2)), None);
        assert_eq!(cache.get(&(103, 2)), None);
    }

    /// PR #450 commit 3 ratchet: defensive against negative baselines
    /// (Dhan should never emit them but the schema is i64).
    #[test]
    fn test_extract_prev_oi_skips_negative_baselines() {
        let resp =
            make_response_with_strikes(vec![("25000.0".to_string(), Some(101), None, -1, 0)]);
        let cache = extract_prev_oi_from_option_chain(&resp, 2);
        assert!(cache.is_empty());
    }

    /// PR #450 commit 3 ratchet: deep-OTM strikes returning None for
    /// CE or PE must be skipped silently — they're not erroneous,
    /// just absent.
    #[test]
    fn test_extract_prev_oi_skips_none_legs_silently() {
        let resp = make_response_with_strikes(vec![
            ("25000.0".to_string(), None, Some(102), 0, 60_000), // no CE
            ("30000.0".to_string(), Some(103), None, 70_000, 0), // no PE (deep OTM)
        ]);
        let cache = extract_prev_oi_from_option_chain(&resp, 2);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&(102, 2)), Some(&60_000));
        assert_eq!(cache.get(&(103, 2)), Some(&70_000));
    }

    /// PR #450 commit 3 ratchet: empty option chain (no strikes
    /// matched the underlying) returns empty cache, not an error.
    #[test]
    fn test_extract_prev_oi_empty_chain_returns_empty_cache() {
        let resp = make_response_with_strikes(vec![]);
        let cache = extract_prev_oi_from_option_chain(&resp, 2);
        assert!(cache.is_empty());
    }

    /// PR #450 commit 3 ratchet: merging two per-underlying caches
    /// produces the union; later inserts override earlier (last-wins).
    #[test]
    fn test_merge_prev_oi_cache_last_wins_on_collision() {
        let mut acc: HashMap<(u32, u8), i64> = HashMap::new();
        acc.insert((101, 2), 50_000);
        let mut incr: HashMap<(u32, u8), i64> = HashMap::new();
        incr.insert((101, 2), 55_000); // newer value
        incr.insert((201, 2), 90_000); // new entry
        merge_prev_oi_cache(&mut acc, incr);
        assert_eq!(acc.len(), 2);
        assert_eq!(acc.get(&(101, 2)), Some(&55_000)); // overwritten
        assert_eq!(acc.get(&(201, 2)), Some(&90_000)); // added
    }
}
