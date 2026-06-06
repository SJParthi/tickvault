//! Build-failing ratchet for the §31 NTM subscription contract.
//!
//! The runtime F2 self-test (#1047) catches universe drift at 09:16 IST. This
//! is the COMPILE/CI-time counterpart: it fails the build the instant anyone
//! silently changes the contract the operator cares about — 33 index values,
//! ~748 NTM constituent stocks, NTM-UNION-ONLY subscription, and enough cap
//! headroom to physically hold the live universe.
//!
//! It pins the RELATIONSHIPS between constants (and the previously-unpinned
//! `NTM_CONSTITUENCY_SLUGS` contents) — not the single values that other tests
//! already pin (`MAX_DAILY_UNIVERSE_SIZE == 1200`, allowlist `len() == 32`,
//! the F2 floors).

#![cfg(feature = "daily_universe_fetcher")]

use tickvault_common::constants::{
    INDEX_CONSTITUENCY_SLUGS, MAX_DAILY_UNIVERSE_SIZE, MIN_DAILY_UNIVERSE_SIZE,
    NTM_CONSTITUENCY_SLUGS,
};
use tickvault_core::instrument::index_extractor::NSE_INDEX_ALLOWLIST;
use tickvault_core::instrument::market_open_self_test::{
    INDEX_VALUES_SUBSCRIBED_FLOOR, NTM_CONSTITUENTS_SUBSCRIBED_FLOOR,
};

/// The exact NTM constituent source. Subscribing all ~46 `INDEX_CONSTITUENCY_SLUGS`
/// would be ~the entire NSE (~1,900 stocks) → breach `MAX_DAILY_UNIVERSE_SIZE`
/// and the 2-WebSocket lock. The live subscription must use NTM ONLY.
const NTM_NAME: &str = "Nifty Total Market";
const NTM_SLUG: &str = "ind_niftytotalmarket_list";
/// The IDX_I `SYMBOL_NAME` of the NTM index value in the Dhan master.
const NTM_INDEX_SYMBOL: &str = "NIFTY TOTAL MKT";

/// Documented expectation (operator 2026-06-06). The EXACT live counts come
/// from the Dhan/niftyindices masters at boot; these are the contract floors
/// the cap must physically accommodate.
const EXPECTED_INDEX_VALUES: usize = 33; // 32 NSE allowlist + 1 BSE SENSEX
const EXPECTED_NTM_CONSTITUENTS: usize = 748;

#[test]
fn subscription_slug_is_ntm_only() {
    assert_eq!(
        NTM_CONSTITUENCY_SLUGS.len(),
        1,
        "the live subscription must use NTM ONLY — adding a 2nd slug would blow MAX_DAILY_UNIVERSE_SIZE"
    );
    assert_eq!(NTM_CONSTITUENCY_SLUGS[0], (NTM_NAME, NTM_SLUG));
}

#[test]
fn ntm_subscription_slug_is_subset_of_full_map() {
    for entry in NTM_CONSTITUENCY_SLUGS {
        assert!(
            INDEX_CONSTITUENCY_SLUGS.contains(entry),
            "NTM subscription slug {entry:?} must also exist in the full constituency map"
        );
    }
}

#[test]
fn full_constituency_map_includes_ntm() {
    assert!(
        INDEX_CONSTITUENCY_SLUGS.contains(&(NTM_NAME, NTM_SLUG)),
        "the full per-index map must contain the NIFTY Total Market list"
    );
}

#[test]
fn index_value_allowlist_includes_ntm() {
    assert!(
        NSE_INDEX_ALLOWLIST.contains(&NTM_INDEX_SYMBOL),
        "the IDX_I allowlist must contain the NTM index value symbol {NTM_INDEX_SYMBOL:?}"
    );
}

#[test]
fn f2_index_floor_within_achievable_bounds() {
    // +1 = the single BSE SENSEX index value (not in NSE_INDEX_ALLOWLIST).
    let max_index_values = NSE_INDEX_ALLOWLIST.len() + 1;
    assert!(
        INDEX_VALUES_SUBSCRIBED_FLOOR >= 1,
        "index floor must be positive"
    );
    assert!(
        INDEX_VALUES_SUBSCRIBED_FLOOR <= max_index_values,
        "index floor ({INDEX_VALUES_SUBSCRIBED_FLOOR}) cannot exceed the achievable count \
         ({max_index_values}) — would page every day"
    );
}

#[test]
fn f2_ntm_floor_fits_under_cap() {
    let max_index_values = NSE_INDEX_ALLOWLIST.len() + 1;
    assert!(
        NTM_CONSTITUENTS_SUBSCRIBED_FLOOR >= 1,
        "ntm floor must be positive"
    );
    assert!(
        max_index_values + NTM_CONSTITUENTS_SUBSCRIBED_FLOOR <= MAX_DAILY_UNIVERSE_SIZE,
        "index values + ntm floor ({}) must fit under the cap ({MAX_DAILY_UNIVERSE_SIZE})",
        max_index_values + NTM_CONSTITUENTS_SUBSCRIBED_FLOOR
    );
}

#[test]
fn universe_cap_accommodates_expected_ntm_union() {
    assert!(
        EXPECTED_INDEX_VALUES + EXPECTED_NTM_CONSTITUENTS <= MAX_DAILY_UNIVERSE_SIZE,
        "the cap ({MAX_DAILY_UNIVERSE_SIZE}) must hold the live union ({} = {EXPECTED_INDEX_VALUES} \
         indices + {EXPECTED_NTM_CONSTITUENTS} NTM stocks)",
        EXPECTED_INDEX_VALUES + EXPECTED_NTM_CONSTITUENTS
    );
    assert!(
        MIN_DAILY_UNIVERSE_SIZE <= EXPECTED_INDEX_VALUES + EXPECTED_NTM_CONSTITUENTS,
        "the live union must clear the min-universe gate"
    );
}
