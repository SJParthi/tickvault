//! Reverse identity index for one FNO option leg — the vendor-neutral types
//! the leg-P&L persister needs to label a row.
//!
//! These lived in the (now deleted) Groww cadence executor because that
//! module happened to be the first publisher. Nothing here is vendor
//! specific: the index is keyed by the same `(exchange_token, segment_code)`
//! pair the mark tap emits, whichever broker fills it.

use chrono::NaiveDate;

/// Reverse identity for one FNO option leg — everything the leg-P&L
/// persister needs to label a row, keyed by the SAME
/// `(exchange_token, segment_code)` pair the mark tap emits.
/// `underlying` borrows the book's `&'static str` symbol — zero allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionLegIdentity {
    pub underlying: &'static str,
    pub expiry: NaiveDate,
    pub strike_paise: i64,
    pub option_type: tickvault_common::types::OptionType,
}

/// Reverse index `(exchange_token, segment_code) -> identity` for the
/// leg-P&L consumer. O(1) lookup per persisted row.
pub type LegIdentityIndex = std::collections::HashMap<(u64, u8), OptionLegIdentity>;

/// Day-stamped shared handle for the leg identity index. A publisher STOREs
/// a fresh `(day, index)` once per daily master download; the leg-P&L boot
/// consumer LOADs lock-free per row. `None` (pre-publish) means the consumer
/// persists identity sentinels (counted) — later rows self-heal once today's
/// index lands.
pub type SharedLegIdentityIndex =
    std::sync::Arc<arc_swap::ArcSwapOption<(NaiveDate, LegIdentityIndex)>>;

/// Fresh, empty shared leg-identity handle. Created once at boot (main.rs)
/// and cloned into the publisher and the leg-P&L consumer.
#[must_use]
pub fn new_shared_leg_identity_index() -> SharedLegIdentityIndex {
    std::sync::Arc::new(arc_swap::ArcSwapOption::empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_shared_leg_identity_index_starts_empty() {
        // Pre-publish the consumer must see None so it persists a counted
        // sentinel rather than a wrong label.
        let idx = new_shared_leg_identity_index();
        assert!(idx.load().is_none(), "a fresh handle must be un-published");
    }

    #[test]
    fn test_leg_identity_index_lookup_round_trips() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 21).expect("valid date");
        let mut map = LegIdentityIndex::new();
        map.insert(
            (12_345_u64, 2_u8),
            OptionLegIdentity {
                underlying: "NIFTY",
                expiry: day,
                strike_paise: 2_500_000,
                option_type: tickvault_common::types::OptionType::Call,
            },
        );
        let found = map.get(&(12_345_u64, 2_u8)).copied().expect("present");
        assert_eq!(found.underlying, "NIFTY");
        assert_eq!(found.strike_paise, 2_500_000);
        assert!(
            map.get(&(12_345_u64, 8_u8)).is_none(),
            "segment is part of the key"
        );
    }
}
