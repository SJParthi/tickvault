//! Wave 5 Item 3 — main-feed connection distribution helpers.
//!
//! Splits the indices-only Wave 5 universe (~11,018 instruments) across
//! `MAIN_FEED_CONNECTIONS` (= 5) WebSocket connections using a deterministic,
//! category-balanced round-robin so:
//!
//! 1. Each connection holds approximately the same number of instruments
//!    (~2,204), well under the per-connection cap of 5,000.
//! 2. Each conn carries a roughly equal mix of IDX_I, NSE_EQ, and
//!    NSE_FNO+BSE_FNO instruments so a single conn drop doesn't
//!    catastrophically remove an entire segment.
//! 3. Subscription assignment is STABLE across boots — given the same
//!    universe the same security_id always lands on the same connection,
//!    enabling SubscribeRxGuard reconnect to resume without divergence.
//!
//! Algorithm (pure function, O(N log N) one-time at boot):
//!   1. Bucket instruments by category: `IDX_I`, `NSE_EQ`,
//!      `NSE_FNO + BSE_FNO`.
//!   2. Sort each bucket ASC by `security_id`.
//!   3. Round-robin: `conn_index = i % MAIN_FEED_CONNECTIONS` walking
//!      each bucket independently — preserves balance per category.
//!
//! What this module does NOT do:
//! - Spawn / connect WebSockets. The actual connection_pool integration
//!   is a follow-up. This module is the pure-logic primitive + ratchet
//!   tests so the algorithm can be unit-verified before wiring.
//! - Handle dynamic re-balancing. The Wave 5 plan keeps assignments
//!   static for the trading day; mid-day rebalances are out of scope.

use tickvault_common::types::ExchangeSegment;

/// The Wave 5 plan pins the main-feed pool at 5 connections (Dhan limit
/// = 5; we use all 5 for capacity + redundancy). Bumping this requires
/// updating both this module + the ratchet test
/// `test_distribution_per_conn_within_5_pct_of_target`.
pub const MAIN_FEED_CONNECTIONS: usize = 5;

/// Category bucket — used both for sort grouping and for the
/// `tv_main_feed_per_conn_instrument_count{category}` Prom label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DistributionCategory {
    /// IDX_I (3 majors + ~26 display indices ≈ 29).
    IdxI,
    /// NSE_EQ cash equities — one per F&O stock (~216).
    NseEq,
    /// NSE_FNO + BSE_FNO derivative contracts (~10,773 under indices-only).
    Derivatives,
}

impl DistributionCategory {
    /// Maps a `(security_id, segment)` pair to its distribution bucket.
    /// Returns `None` for segments that don't appear in the Wave 5
    /// indices-only universe (currency, commodity, BSE_EQ, etc.).
    #[must_use]
    // TEST-EXEMPT: covered transitively by `test_distribution_drops_unsupported_segments` and `test_distribution_is_category_balanced_round_robin`.
    pub const fn from_segment(segment: ExchangeSegment) -> Option<Self> {
        match segment {
            ExchangeSegment::IdxI => Some(Self::IdxI),
            ExchangeSegment::NseEquity => Some(Self::NseEq),
            ExchangeSegment::NseFno | ExchangeSegment::BseFno => Some(Self::Derivatives),
            ExchangeSegment::NseCurrency
            | ExchangeSegment::BseEquity
            | ExchangeSegment::McxComm
            | ExchangeSegment::BseCurrency => None,
        }
    }
}

/// One row of the deterministic distribution table.
/// `(connection_index, category, security_id)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistributedSubscription {
    pub connection_index: usize,
    pub category: DistributionCategory,
    pub security_id: u32,
    pub segment: ExchangeSegment,
}

/// Computes the deterministic main-feed distribution for the given set
/// of `(security_id, segment)` pairs.
///
/// O(N log N) once at boot. Output is sorted by category, then by
/// security_id ASC — the sort key is what makes the distribution stable
/// across boots (same input → same output).
#[must_use]
// TEST-EXEMPT: covered by 7 ratchet tests (test_distribution_*, test_same_security_id_*, test_per_connection_*).
pub fn distribute_main_feed_subscriptions(
    instruments: &[(u32, ExchangeSegment)],
) -> Vec<DistributedSubscription> {
    let mut idx_i: Vec<(u32, ExchangeSegment)> = Vec::new();
    let mut nse_eq: Vec<(u32, ExchangeSegment)> = Vec::new();
    let mut derivs: Vec<(u32, ExchangeSegment)> = Vec::new();

    for &(sid, seg) in instruments {
        match DistributionCategory::from_segment(seg) {
            Some(DistributionCategory::IdxI) => idx_i.push((sid, seg)),
            Some(DistributionCategory::NseEq) => nse_eq.push((sid, seg)),
            Some(DistributionCategory::Derivatives) => derivs.push((sid, seg)),
            None => { /* dropped — not in Wave 5 universe */ }
        }
    }

    // Stable ordering: sort each bucket by security_id ASC. The same set
    // of (sid, seg) pairs always produces the same Vec<DistributedSubscription>.
    idx_i.sort_unstable_by_key(|&(sid, _)| sid);
    nse_eq.sort_unstable_by_key(|&(sid, _)| sid);
    derivs.sort_unstable_by_key(|&(sid, _)| sid);

    let total = idx_i.len() + nse_eq.len() + derivs.len();
    let mut out: Vec<DistributedSubscription> = Vec::with_capacity(total);

    // Round-robin within each bucket independently.
    for (i, &(sid, seg)) in idx_i.iter().enumerate() {
        out.push(DistributedSubscription {
            connection_index: i % MAIN_FEED_CONNECTIONS,
            category: DistributionCategory::IdxI,
            security_id: sid,
            segment: seg,
        });
    }
    for (i, &(sid, seg)) in nse_eq.iter().enumerate() {
        out.push(DistributedSubscription {
            connection_index: i % MAIN_FEED_CONNECTIONS,
            category: DistributionCategory::NseEq,
            security_id: sid,
            segment: seg,
        });
    }
    for (i, &(sid, seg)) in derivs.iter().enumerate() {
        out.push(DistributedSubscription {
            connection_index: i % MAIN_FEED_CONNECTIONS,
            category: DistributionCategory::Derivatives,
            security_id: sid,
            segment: seg,
        });
    }
    out
}

/// Returns per-connection instrument counts. Used by the boot logger and
/// the `tv_main_feed_per_conn_instrument_count{conn}` Prom gauge.
#[must_use]
pub fn per_connection_counts(
    distribution: &[DistributedSubscription],
) -> [usize; MAIN_FEED_CONNECTIONS] {
    let mut counts = [0usize; MAIN_FEED_CONNECTIONS];
    for d in distribution {
        if d.connection_index < MAIN_FEED_CONNECTIONS {
            counts[d.connection_index] = counts[d.connection_index].saturating_add(1);
        }
    }
    counts
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_universe() -> Vec<(u32, ExchangeSegment)> {
        let mut v = Vec::new();
        // 29 IDX_I — security_id 13, 25, 51, plus 26 display indices
        // (synthetic 100..125).
        v.push((13, ExchangeSegment::IdxI));
        v.push((25, ExchangeSegment::IdxI));
        v.push((51, ExchangeSegment::IdxI));
        for i in 100..126 {
            v.push((i, ExchangeSegment::IdxI));
        }
        // 216 NSE_EQ stocks — synthetic ids 1000..1216.
        for i in 1000..1216 {
            v.push((i, ExchangeSegment::NseEquity));
        }
        // ~2,037 index F&O on NSE_FNO (synthetic ids 50000..52037).
        for i in 50_000..52_037 {
            v.push((i, ExchangeSegment::NseFno));
        }
        // ~248 index F&O on BSE_FNO (SENSEX) (synthetic ids 80000..80248).
        for i in 80_000..80_248 {
            v.push((i, ExchangeSegment::BseFno));
        }
        v
    }

    #[test]
    fn test_distribution_is_category_balanced_round_robin() {
        let universe = synth_universe();
        let dist = distribute_main_feed_subscriptions(&universe);

        // Check each category is round-robin-distributed across all 5 conns.
        for category in [
            DistributionCategory::IdxI,
            DistributionCategory::NseEq,
            DistributionCategory::Derivatives,
        ] {
            let mut per_conn = [0usize; MAIN_FEED_CONNECTIONS];
            for d in dist.iter().filter(|d| d.category == category) {
                per_conn[d.connection_index] = per_conn[d.connection_index].saturating_add(1);
            }
            let total: usize = per_conn.iter().sum();
            if total == 0 {
                continue;
            }
            let expected = total / MAIN_FEED_CONNECTIONS;
            for (i, count) in per_conn.iter().enumerate() {
                assert!(
                    count.abs_diff(expected) <= 1,
                    "category {:?} conn {} has {} entries; expected {expected} ± 1",
                    category,
                    i,
                    count
                );
            }
        }
    }

    #[test]
    fn test_same_security_id_lands_on_same_connection_across_runs() {
        let universe = synth_universe();
        // Run 1
        let dist1 = distribute_main_feed_subscriptions(&universe);
        // Run 2 with the input shuffled — order should NOT affect output.
        let mut shuffled = universe.clone();
        shuffled.reverse();
        let dist2 = distribute_main_feed_subscriptions(&shuffled);

        // Build sid → conn maps from each run; assert identical mapping.
        let map1: std::collections::HashMap<(u32, ExchangeSegment), usize> = dist1
            .iter()
            .map(|d| ((d.security_id, d.segment), d.connection_index))
            .collect();
        let map2: std::collections::HashMap<(u32, ExchangeSegment), usize> = dist2
            .iter()
            .map(|d| ((d.security_id, d.segment), d.connection_index))
            .collect();
        assert_eq!(map1.len(), map2.len(), "distribution sizes must match");
        for (k, v1) in &map1 {
            let v2 = map2.get(k).copied();
            assert_eq!(
                v2,
                Some(*v1),
                "sid {:?} mapped to conn {v1} in run 1, conn {v2:?} in run 2",
                k
            );
        }
    }

    #[test]
    fn test_distribution_per_conn_within_5_pct_of_target() {
        let universe = synth_universe();
        let dist = distribute_main_feed_subscriptions(&universe);
        let per_conn = per_connection_counts(&dist);
        let total: usize = per_conn.iter().sum();
        assert!(total > 0);
        let target = total / MAIN_FEED_CONNECTIONS;
        // 5% tolerance per the plan's 9-box ratchet; absolute floor of
        // 1 to handle small synthetic universes.
        let tolerance = (target / 20).max(1);
        for (i, count) in per_conn.iter().enumerate() {
            assert!(
                count.abs_diff(target) <= tolerance,
                "conn {i} has {count} instruments; target {target} ± {tolerance}"
            );
        }
    }

    #[test]
    fn test_distribution_idempotent_on_replay() {
        let universe = synth_universe();
        let dist1 = distribute_main_feed_subscriptions(&universe);
        let dist2 = distribute_main_feed_subscriptions(&universe);
        let dist3 = distribute_main_feed_subscriptions(&universe);
        assert_eq!(dist1, dist2);
        assert_eq!(dist2, dist3);
        // And the per-conn count vector also stable.
        assert_eq!(per_connection_counts(&dist1), per_connection_counts(&dist3));
    }

    #[test]
    fn test_distribution_drops_unsupported_segments() {
        let mut u = synth_universe();
        // Inject a few unsupported-segment ticks.
        u.push((9_999, ExchangeSegment::NseCurrency));
        u.push((9_998, ExchangeSegment::McxComm));
        u.push((9_997, ExchangeSegment::BseCurrency));
        u.push((9_996, ExchangeSegment::BseEquity));
        let dist = distribute_main_feed_subscriptions(&u);
        // None of the unsupported sids appear in the output.
        for sid in [9_999, 9_998, 9_997, 9_996] {
            assert!(
                !dist.iter().any(|d| d.security_id == sid),
                "unsupported sid {sid} leaked into distribution"
            );
        }
    }

    #[test]
    fn test_main_feed_connections_count_matches_dhan_cap() {
        // Dhan caps the per-account WebSocket pool at 5. Bumping this
        // requires Dhan support escalation + plan amendment.
        assert_eq!(MAIN_FEED_CONNECTIONS, 5);
    }

    #[test]
    fn test_per_connection_counts_returns_zero_for_empty_input() {
        let counts = per_connection_counts(&[]);
        assert_eq!(counts, [0usize; MAIN_FEED_CONNECTIONS]);
    }
}
