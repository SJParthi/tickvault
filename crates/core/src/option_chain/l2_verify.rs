//! L2 VERIFY — structural validation of a Dhan option-chain response
//! before it is cached (PR #8d of the heart-piece).
//!
//! Per `docs/architecture/option-chain-z-plus-heart-piece.md` §3 row 2,
//! the Z+ L2 VERIFY layer cross-checks every fetched response before the
//! strategy can read it. L2 has three checks:
//!
//!   (a) option-chain `data.last_price` agrees with the live WS index
//!       LTP within `OPTION_CHAIN_L2_VERIFY_TOLERANCE_PCT` (0.5%)
//!   (b) the `oc` map carries at least `OPTION_CHAIN_MIN_STRIKES` strikes
//!   (c) `status == "success"`
//!
//! **This module ships checks (b) + (c)** — the two STRUCTURAL checks
//! that need only the response itself, no external data. Check (a), the
//! WS-LTP cross-check, needs `SharedSpotPrices` plumbed into the
//! scheduler and lands in a follow-up slice.
//!
//! ## Why structural verify matters
//!
//! The RAM cache (`snapshot_cache.rs`) has no opinion on payload shape —
//! it stores whatever it is handed. If a partial / wrong-expiry /
//! Dhan-degraded response with an almost-empty `oc` map were cached,
//! the strategy's strike-selection would silently run against a hole
//! (no ATM ± N strikes to pick from). Rejecting structurally-bad
//! responses BEFORE the cache insert keeps the cache's contents
//! trustworthy: a slot either holds a real chain or holds the previous
//! (older but valid) chain — never a malformed one.
//!
//! ## Pure function
//!
//! `verify_option_chain_structure` is a pure function of the response.
//! No clock, no I/O, no allocation on the happy path. The scheduler
//! calls it once per fetch (cold path — every 50s).

use tickvault_common::constants::OPTION_CHAIN_MIN_STRIKES;

use super::types::OptionChainResponse;

/// Wire-format value Dhan returns in the `status` field on a healthy
/// option-chain response.
pub const OPTION_CHAIN_SUCCESS_STATUS: &str = "success";

/// Outcome of the L2 structural verification of one option-chain
/// response. `Valid` means the response is safe to cache; every other
/// variant means the scheduler must REJECT it (keep the previous cached
/// snapshot, record a fetch failure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum L2StructuralVerdict {
    /// Response passed both structural checks — safe to cache.
    Valid,
    /// `status` field was not `"success"`. Carries the actual value
    /// (truncated) for the operator log.
    BadStatus { actual: String },
    /// The `oc` strike map was completely empty.
    EmptyOcMap,
    /// The `oc` strike map had fewer than `OPTION_CHAIN_MIN_STRIKES`
    /// entries — structurally suspect (partial body / wrong expiry).
    TooFewStrikes { count: usize },
}

impl L2StructuralVerdict {
    /// `true` iff the response is safe to cache.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        matches!(self, Self::Valid)
    }

    /// Operator-readable reason string for the rejection log. Returns
    /// an empty string for `Valid` (caller should not log on the happy
    /// path).
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Valid => String::new(),
            Self::BadStatus { actual } => {
                format!("L2 verify: response status != \"success\" (got \"{actual}\")")
            }
            Self::EmptyOcMap => "L2 verify: option-chain `oc` strike map is empty".to_string(),
            Self::TooFewStrikes { count } => format!(
                "L2 verify: option-chain has {count} strikes, below the {OPTION_CHAIN_MIN_STRIKES}-strike sanity floor"
            ),
        }
    }
}

/// Structurally verify a fetched option-chain response BEFORE it is
/// cached. Pure function — no clock, no I/O.
///
/// Checks, in order:
///   1. `status == "success"` (check c)
///   2. `oc` map is non-empty (check b, part 1)
///   3. `oc` map has ≥ `OPTION_CHAIN_MIN_STRIKES` strikes (check b, part 2)
///
/// The first failing check wins — the verdict names exactly one problem
/// so the operator log is unambiguous.
#[must_use]
pub fn verify_option_chain_structure(response: &OptionChainResponse) -> L2StructuralVerdict {
    // Check (c): status must be the literal "success".
    if response.status != OPTION_CHAIN_SUCCESS_STATUS {
        let actual: String = response.status.chars().take(64).collect();
        return L2StructuralVerdict::BadStatus { actual };
    }

    // Check (b) part 1: the strike map must not be empty.
    let strike_count = response.data.oc.len();
    if strike_count == 0 {
        return L2StructuralVerdict::EmptyOcMap;
    }

    // Check (b) part 2: enough strikes to strike-select against.
    if strike_count < OPTION_CHAIN_MIN_STRIKES {
        return L2StructuralVerdict::TooFewStrikes {
            count: strike_count,
        };
    }

    L2StructuralVerdict::Valid
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::option_chain::types::{OptionChainData, StrikeData};
    use std::collections::HashMap;

    /// Builds a response with `strike_count` synthetic strikes + the
    /// given status string.
    fn make_response(status: &str, strike_count: usize) -> OptionChainResponse {
        let mut oc: HashMap<String, StrikeData> = HashMap::new();
        for i in 0..strike_count {
            // Strike keys are decimal strings per dhan/option-chain.md rule 6.
            let strike = format!("{}.000000", 25_000 + i * 50);
            oc.insert(strike, StrikeData { ce: None, pe: None });
        }
        OptionChainResponse {
            data: OptionChainData {
                last_price: 25_650.0,
                oc,
            },
            status: status.to_string(),
        }
    }

    #[test]
    fn test_verify_option_chain_structure_valid_with_200_strikes() {
        let resp = make_response("success", 200);
        assert_eq!(
            verify_option_chain_structure(&resp),
            L2StructuralVerdict::Valid
        );
        assert!(verify_option_chain_structure(&resp).is_valid());
    }

    #[test]
    fn test_verify_valid_at_exactly_min_strikes() {
        // Boundary: exactly OPTION_CHAIN_MIN_STRIKES is VALID (>= floor).
        let resp = make_response("success", OPTION_CHAIN_MIN_STRIKES);
        assert_eq!(
            verify_option_chain_structure(&resp),
            L2StructuralVerdict::Valid
        );
    }

    #[test]
    fn test_verify_rejects_one_below_min_strikes() {
        let resp = make_response("success", OPTION_CHAIN_MIN_STRIKES - 1);
        assert_eq!(
            verify_option_chain_structure(&resp),
            L2StructuralVerdict::TooFewStrikes {
                count: OPTION_CHAIN_MIN_STRIKES - 1
            }
        );
    }

    #[test]
    fn test_verify_rejects_empty_oc_map() {
        let resp = make_response("success", 0);
        assert_eq!(
            verify_option_chain_structure(&resp),
            L2StructuralVerdict::EmptyOcMap
        );
    }

    #[test]
    fn test_verify_rejects_bad_status() {
        let resp = make_response("failure", 200);
        match verify_option_chain_structure(&resp) {
            L2StructuralVerdict::BadStatus { actual } => assert_eq!(actual, "failure"),
            other => panic!("expected BadStatus, got {other:?}"),
        }
    }

    #[test]
    fn test_verify_status_checked_before_strike_count() {
        // A bad-status response with zero strikes must report BadStatus
        // (the FIRST failing check wins — unambiguous operator log).
        let resp = make_response("error", 0);
        assert!(matches!(
            verify_option_chain_structure(&resp),
            L2StructuralVerdict::BadStatus { .. }
        ));
    }

    #[test]
    fn test_verify_truncates_pathological_status_string() {
        // A 500-char status string must not blow up the operator log.
        let long_status = "x".repeat(500);
        let resp = make_response(&long_status, 200);
        match verify_option_chain_structure(&resp) {
            L2StructuralVerdict::BadStatus { actual } => {
                assert!(actual.len() <= 64, "status must be truncated to 64 chars");
            }
            other => panic!("expected BadStatus, got {other:?}"),
        }
    }

    #[test]
    fn test_reason_string_is_empty_for_valid() {
        assert_eq!(L2StructuralVerdict::Valid.reason(), "");
    }

    #[test]
    fn test_reason_string_non_empty_for_every_failure() {
        for verdict in [
            L2StructuralVerdict::BadStatus {
                actual: "x".to_string(),
            },
            L2StructuralVerdict::EmptyOcMap,
            L2StructuralVerdict::TooFewStrikes { count: 5 },
        ] {
            assert!(
                !verdict.reason().is_empty(),
                "{verdict:?} must produce a non-empty operator reason"
            );
        }
    }

    #[test]
    fn test_is_valid_only_true_for_valid_variant() {
        assert!(L2StructuralVerdict::Valid.is_valid());
        assert!(!L2StructuralVerdict::EmptyOcMap.is_valid());
        assert!(!L2StructuralVerdict::TooFewStrikes { count: 0 }.is_valid());
        assert!(
            !L2StructuralVerdict::BadStatus {
                actual: String::new()
            }
            .is_valid()
        );
    }
}
