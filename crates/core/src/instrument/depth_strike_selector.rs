//! Depth strike selector — selects ATM ± N strikes for depth subscriptions.
//!
//! Pure functions that take (spot_price, option_chain) → Vec<SecurityId>.
//! Used by the depth connection to dynamically select which strikes to subscribe.
//!
//! # Dynamic Rebalancing
//! When spot price moves beyond a threshold (e.g., ±3 strikes from previous ATM),
//! the depth connection unsubscribes old strikes and subscribes new ATM ± N.

use tickvault_common::instrument_types::{
    FnoUniverse, OptionChain, OptionChainEntry, OptionChainKey,
};
use tickvault_common::types::SecurityId;

use chrono::NaiveDate;

/// Number of strikes above and below ATM to subscribe for 20-level depth.
/// 24 CE above + ATM + 24 PE below = 49 instruments per underlying.
pub const DEPTH_ATM_STRIKES_EACH_SIDE: usize = 24;

/// Spot price movement threshold (in strikes) before triggering rebalance.
/// If spot moves ±3 strikes from the previously selected ATM, re-subscribe.
pub const DEPTH_REBALANCE_STRIKE_THRESHOLD: usize = 3;

/// Result of ATM strike selection for depth subscription.
#[derive(Debug, Clone)]
pub struct DepthStrikeSelection {
    /// The ATM strike price selected.
    pub atm_strike: f64,
    /// Security IDs of selected call options (ATM ± N).
    pub call_security_ids: Vec<SecurityId>,
    /// Security IDs of selected put options (ATM ± N).
    pub put_security_ids: Vec<SecurityId>,
    /// All security IDs combined (calls + puts), ready for subscription.
    pub all_security_ids: Vec<SecurityId>,
    /// The underlying symbol these belong to.
    pub underlying_symbol: String,
    /// The expiry date of the selected chain.
    pub expiry_date: NaiveDate,
}

/// Selects ATM ± N strikes from an option chain based on spot price.
///
/// # Arguments
/// * `chain` — The option chain (sorted calls and puts by strike).
/// * `spot_price` — Current spot/LTP of the underlying.
/// * `strikes_each_side` — Number of strikes above and below ATM.
///
/// # Returns
/// `Some(DepthStrikeSelection)` with the selected security IDs, or `None` if
/// the chain is empty or spot price is invalid.
///
/// # Precision Guarantee
/// - Finds the strike closest to spot_price using binary search on sorted strikes.
/// - Selects exactly `strikes_each_side` above and below (or fewer at chain edges).
/// - Both CE and PE for each strike.
pub fn select_atm_strikes(
    chain: &OptionChain,
    spot_price: f64,
    strikes_each_side: usize,
) -> Option<DepthStrikeSelection> {
    if chain.calls.is_empty() || !spot_price.is_finite() || spot_price <= 0.0 {
        return None;
    }

    // Find ATM index using binary search on sorted call strikes.
    let atm_idx = find_atm_index(&chain.calls, spot_price);
    let atm_strike = chain.calls[atm_idx].strike_price;

    // Select range: [atm_idx - N, atm_idx + N] clamped to chain bounds.
    let start = atm_idx.saturating_sub(strikes_each_side);
    let end = (atm_idx + strikes_each_side + 1).min(chain.calls.len());

    let mut call_ids = Vec::with_capacity(end - start);
    let mut put_ids = Vec::with_capacity(end - start);

    for i in start..end {
        call_ids.push(chain.calls[i].security_id);

        // Find matching put at same strike price.
        if let Some(put_entry) = find_put_at_strike(&chain.puts, chain.calls[i].strike_price) {
            put_ids.push(put_entry.security_id);
        }
    }

    let mut all_ids = Vec::with_capacity(call_ids.len() + put_ids.len());
    all_ids.extend_from_slice(&call_ids);
    all_ids.extend_from_slice(&put_ids);

    Some(DepthStrikeSelection {
        atm_strike,
        call_security_ids: call_ids,
        put_security_ids: put_ids,
        all_security_ids: all_ids,
        underlying_symbol: chain.underlying_symbol.clone(),
        expiry_date: chain.expiry_date,
    })
}

/// Selects depth strikes for multiple underlyings from the FnoUniverse.
///
/// For each underlying, finds the nearest expiry option chain and selects
/// ATM ± N strikes based on spot price.
///
/// # Arguments
/// * `universe` — The FnoUniverse with all option chains.
/// * `underlyings` — Underlying symbols to select (e.g., ["NIFTY", "BANKNIFTY"]).
/// * `spot_prices` — Spot prices keyed by underlying symbol.
/// * `today` — Current date for finding nearest expiry.
/// * `strikes_each_side` — Number of strikes above/below ATM.
// TEST-EXEMPT: orchestrator — delegates to select_atm_strikes (fully tested), requires full FnoUniverse fixture
pub fn select_depth_instruments(
    universe: &FnoUniverse,
    underlyings: &[&str],
    spot_prices: &std::collections::HashMap<String, f64>,
    today: NaiveDate,
    strikes_each_side: usize,
) -> Vec<DepthStrikeSelection> {
    let mut selections = Vec::new();

    for &symbol in underlyings {
        // Find nearest expiry for this underlying
        let nearest_expiry = universe
            .expiry_calendars
            .get(symbol)
            .and_then(|cal| cal.expiry_dates.iter().find(|&&e| e >= today).copied());

        let expiry = match nearest_expiry {
            Some(e) => e,
            None => {
                tracing::warn!(symbol, "no valid expiry found for depth subscription");
                continue;
            }
        };

        // Look up the option chain
        let chain_key = OptionChainKey {
            underlying_symbol: symbol.to_string(),
            expiry_date: expiry,
        };

        let chain = match universe.option_chains.get(&chain_key) {
            Some(c) => c,
            None => {
                tracing::warn!(
                    symbol,
                    %expiry,
                    "no option chain found for depth subscription"
                );
                continue;
            }
        };

        // Get spot price for this underlying
        let spot = match spot_prices.get(symbol) {
            Some(&p) if p > 0.0 && p.is_finite() => p,
            _ => {
                tracing::warn!(symbol, "no valid spot price for depth ATM selection");
                continue;
            }
        };

        // Select ATM ± N strikes
        if let Some(selection) = select_atm_strikes(chain, spot, strikes_each_side) {
            tracing::info!(
                symbol,
                atm_strike = selection.atm_strike,
                calls = selection.call_security_ids.len(),
                puts = selection.put_security_ids.len(),
                total = selection.all_security_ids.len(),
                %expiry,
                "depth strikes selected"
            );
            selections.push(selection);
        }
    }

    selections
}

/// Returns true if spot has moved enough from previous ATM to warrant rebalancing.
///
/// Checks if the new ATM strike index differs by more than `threshold` strikes
/// from the previous ATM index in the sorted chain.
pub fn should_rebalance(
    chain: &OptionChain,
    previous_atm_strike: f64,
    current_spot: f64,
    threshold: usize,
) -> bool {
    if chain.calls.is_empty() || !current_spot.is_finite() || current_spot <= 0.0 {
        return false;
    }

    let prev_idx = find_atm_index(&chain.calls, previous_atm_strike);
    let curr_idx = find_atm_index(&chain.calls, current_spot);

    curr_idx.abs_diff(prev_idx) >= threshold
}

/// ATM contract identifiers at a given spot price — CE + PE security IDs
/// and the actual ATM strike price from the chain (not the raw spot price).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AtmIds {
    /// CE security ID at ATM strike.
    pub ce_id: u32,
    /// PE security ID at ATM strike (None if no matching put).
    pub pe_id: Option<u32>,
    /// The actual strike price in the chain closest to spot.
    pub strike: f64,
}

/// Returns the CE and PE security IDs + actual strike at the ATM nearest to `spot_price`.
///
/// Used by the depth rebalancer to include full contract labels in Telegram
/// notifications (e.g. `NIFTY-24Apr2026-24400-CE (SID 35246)`).
pub fn find_atm_security_ids(chain: &OptionChain, spot_price: f64) -> Option<AtmIds> {
    if chain.calls.is_empty() || !spot_price.is_finite() || spot_price <= 0.0 {
        return None;
    }
    let atm_idx = find_atm_index(&chain.calls, spot_price);
    let strike = chain.calls[atm_idx].strike_price;
    let ce_id = chain.calls[atm_idx].security_id;
    let pe_id = find_put_at_strike(&chain.puts, strike).map(|p| p.security_id);
    Some(AtmIds {
        ce_id,
        pe_id,
        strike,
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Binary search for the closest strike to spot_price in a sorted slice.
fn find_atm_index(entries: &[OptionChainEntry], spot_price: f64) -> usize {
    if entries.is_empty() {
        return 0;
    }

    // Binary search for the closest strike
    let mut best_idx = 0;
    let mut best_diff = f64::MAX;

    // Use partition_point for O(log N) search
    let partition = entries.partition_point(|e| e.strike_price < spot_price);

    // Check the two candidates around the partition point
    let candidates = [
        partition.saturating_sub(1),
        partition.min(entries.len() - 1),
    ];

    for &idx in &candidates {
        let diff = (entries[idx].strike_price - spot_price).abs();
        if diff < best_diff {
            best_diff = diff;
            best_idx = idx;
        }
    }

    best_idx
}

/// Finds a put option at the exact strike price using binary search.
fn find_put_at_strike(puts: &[OptionChainEntry], strike_price: f64) -> Option<&OptionChainEntry> {
    // Puts are sorted by strike_price — use partition_point
    let idx = puts.partition_point(|p| p.strike_price < strike_price);
    if idx < puts.len() && (puts[idx].strike_price - strike_price).abs() < 0.01 {
        Some(&puts[idx])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::instrument_types::OptionChainEntry;

    fn make_chain(strikes: &[f64], underlying: &str, expiry: NaiveDate) -> OptionChain {
        let calls: Vec<OptionChainEntry> = strikes
            .iter()
            .enumerate()
            .map(|(i, &s)| OptionChainEntry {
                security_id: (10000 + i) as u32,
                strike_price: s,
                lot_size: 50,
            })
            .collect();

        let puts: Vec<OptionChainEntry> = strikes
            .iter()
            .enumerate()
            .map(|(i, &s)| OptionChainEntry {
                security_id: (20000 + i) as u32,
                strike_price: s,
                lot_size: 50,
            })
            .collect();

        OptionChain {
            underlying_symbol: underlying.to_string(),
            expiry_date: expiry,
            calls,
            puts,
            future_security_id: None,
        }
    }

    #[test]
    fn test_select_atm_strikes_nifty() {
        // NIFTY strikes: 23000, 23050, ..., 24000 (21 strikes, 50-point gap)
        let strikes: Vec<f64> = (0..21).map(|i| 23000.0 + (i as f64) * 50.0).collect();
        let chain = make_chain(
            &strikes,
            "NIFTY",
            NaiveDate::from_ymd_opt(2026, 4, 9).unwrap(),
        );

        let selection = select_atm_strikes(&chain, 23500.0, 10).unwrap();

        // ATM should be 23500 (index 10)
        assert!((selection.atm_strike - 23500.0).abs() < 1.0);
        // Should have calls and puts
        assert!(!selection.call_security_ids.is_empty());
        assert!(!selection.put_security_ids.is_empty());
        // ATM ± 10 = 21 strikes (entire chain in this case)
        assert_eq!(selection.call_security_ids.len(), 21);
        assert_eq!(selection.put_security_ids.len(), 21);
    }

    #[test]
    fn test_select_atm_strikes_asymmetric() {
        // 30 strikes, ATM near start — should get fewer below, more above
        let strikes: Vec<f64> = (0..30).map(|i| 23000.0 + (i as f64) * 50.0).collect();
        let chain = make_chain(
            &strikes,
            "NIFTY",
            NaiveDate::from_ymd_opt(2026, 4, 9).unwrap(),
        );

        let selection = select_atm_strikes(&chain, 23100.0, 10).unwrap();

        // ATM should be 23100 (index 2)
        assert!((selection.atm_strike - 23100.0).abs() < 1.0);
        // Below ATM: only 2 strikes available (23000, 23050)
        // Above ATM: 10 strikes available
        // Total: 2 + 1 (ATM) + 10 = 13
        assert!(selection.call_security_ids.len() <= 21);
        assert!(selection.call_security_ids.len() >= 11);
    }

    #[test]
    fn test_select_atm_strikes_empty_chain() {
        let chain = OptionChain {
            underlying_symbol: "NIFTY".to_string(),
            expiry_date: NaiveDate::from_ymd_opt(2026, 4, 9).unwrap(),
            calls: vec![],
            puts: vec![],
            future_security_id: None,
        };
        assert!(select_atm_strikes(&chain, 23500.0, 10).is_none());
    }

    #[test]
    fn test_select_atm_strikes_invalid_spot() {
        let strikes: Vec<f64> = (0..5).map(|i| 23000.0 + (i as f64) * 50.0).collect();
        let chain = make_chain(
            &strikes,
            "NIFTY",
            NaiveDate::from_ymd_opt(2026, 4, 9).unwrap(),
        );

        assert!(select_atm_strikes(&chain, 0.0, 10).is_none());
        assert!(select_atm_strikes(&chain, -100.0, 10).is_none());
        assert!(select_atm_strikes(&chain, f64::NAN, 10).is_none());
        assert!(select_atm_strikes(&chain, f64::INFINITY, 10).is_none());
    }

    #[test]
    fn test_should_rebalance_no_move() {
        let strikes: Vec<f64> = (0..30).map(|i| 23000.0 + (i as f64) * 50.0).collect();
        let chain = make_chain(
            &strikes,
            "NIFTY",
            NaiveDate::from_ymd_opt(2026, 4, 9).unwrap(),
        );

        // Same spot → no rebalance
        assert!(!should_rebalance(&chain, 23500.0, 23500.0, 3));
        // Small move (1 strike) → no rebalance
        assert!(!should_rebalance(&chain, 23500.0, 23550.0, 3));
        // 2 strikes → no rebalance (threshold is 3)
        assert!(!should_rebalance(&chain, 23500.0, 23600.0, 3));
    }

    #[test]
    fn test_should_rebalance_significant_move() {
        let strikes: Vec<f64> = (0..30).map(|i| 23000.0 + (i as f64) * 50.0).collect();
        let chain = make_chain(
            &strikes,
            "NIFTY",
            NaiveDate::from_ymd_opt(2026, 4, 9).unwrap(),
        );

        // 3 strikes move → rebalance
        assert!(should_rebalance(&chain, 23500.0, 23650.0, 3));
        // 5 strikes move → definitely rebalance
        assert!(should_rebalance(&chain, 23500.0, 23750.0, 3));
        // Downward move → rebalance
        assert!(should_rebalance(&chain, 23500.0, 23350.0, 3));
    }

    #[test]
    fn test_find_atm_index_exact_match() {
        let entries: Vec<OptionChainEntry> = vec![
            OptionChainEntry {
                security_id: 1,
                strike_price: 23000.0,
                lot_size: 50,
            },
            OptionChainEntry {
                security_id: 2,
                strike_price: 23050.0,
                lot_size: 50,
            },
            OptionChainEntry {
                security_id: 3,
                strike_price: 23100.0,
                lot_size: 50,
            },
        ];
        assert_eq!(find_atm_index(&entries, 23050.0), 1);
    }

    #[test]
    fn test_find_atm_index_between_strikes() {
        let entries: Vec<OptionChainEntry> = vec![
            OptionChainEntry {
                security_id: 1,
                strike_price: 23000.0,
                lot_size: 50,
            },
            OptionChainEntry {
                security_id: 2,
                strike_price: 23100.0,
                lot_size: 50,
            },
        ];
        // 23040 is closer to 23000 (diff=40) than 23100 (diff=60)
        assert_eq!(find_atm_index(&entries, 23040.0), 0);
        // 23060 is closer to 23100 (diff=40) than 23000 (diff=60)
        assert_eq!(find_atm_index(&entries, 23060.0), 1);
    }

    #[test]
    fn test_find_put_at_strike_found() {
        let puts = vec![
            OptionChainEntry {
                security_id: 100,
                strike_price: 23000.0,
                lot_size: 50,
            },
            OptionChainEntry {
                security_id: 101,
                strike_price: 23050.0,
                lot_size: 50,
            },
        ];
        let found = find_put_at_strike(&puts, 23050.0);
        assert!(found.is_some());
        assert_eq!(found.unwrap().security_id, 101);
    }

    #[test]
    fn test_find_put_at_strike_not_found() {
        let puts = vec![OptionChainEntry {
            security_id: 100,
            strike_price: 23000.0,
            lot_size: 50,
        }];
        assert!(find_put_at_strike(&puts, 23050.0).is_none());
    }

    #[test]
    fn test_all_security_ids_contains_both_ce_pe() {
        let strikes: Vec<f64> = (0..5).map(|i| 23000.0 + (i as f64) * 100.0).collect();
        let chain = make_chain(
            &strikes,
            "NIFTY",
            NaiveDate::from_ymd_opt(2026, 4, 9).unwrap(),
        );

        let selection = select_atm_strikes(&chain, 23200.0, 2).unwrap();

        // All IDs should include both CE (10000+) and PE (20000+)
        let has_ce = selection
            .all_security_ids
            .iter()
            .any(|&id| id >= 10000 && id < 20000);
        let has_pe = selection.all_security_ids.iter().any(|&id| id >= 20000);
        assert!(has_ce, "must have CE security IDs");
        assert!(has_pe, "must have PE security IDs");
    }

    #[test]
    fn test_find_atm_security_ids_returns_ce_pe_and_strike() {
        let strikes: Vec<f64> = (0..5).map(|i| 23000.0 + (i as f64) * 100.0).collect();
        let chain = make_chain(
            &strikes,
            "NIFTY",
            NaiveDate::from_ymd_opt(2026, 4, 9).unwrap(),
        );
        let atm = find_atm_security_ids(&chain, 23200.0).unwrap();
        // ATM at 23200 → index 2 → CE=10002, PE=20002, strike=23200.0
        assert_eq!(atm.ce_id, 10002);
        assert_eq!(atm.pe_id, Some(20002));
        assert!((atm.strike - 23200.0).abs() < 0.01);
    }

    #[test]
    fn test_find_atm_security_ids_between_strikes() {
        let strikes: Vec<f64> = (0..5).map(|i| 23000.0 + (i as f64) * 100.0).collect();
        let chain = make_chain(
            &strikes,
            "NIFTY",
            NaiveDate::from_ymd_opt(2026, 4, 9).unwrap(),
        );
        // Spot 23170 → nearest strike is 23200 (not 23100)
        let atm = find_atm_security_ids(&chain, 23170.0).unwrap();
        assert!((atm.strike - 23200.0).abs() < 0.01);
    }

    #[test]
    fn test_find_atm_security_ids_empty_chain() {
        let chain = OptionChain {
            underlying_symbol: "NIFTY".to_string(),
            expiry_date: NaiveDate::from_ymd_opt(2026, 4, 9).unwrap(),
            calls: vec![],
            puts: vec![],
            future_security_id: None,
        };
        assert!(find_atm_security_ids(&chain, 23200.0).is_none());
    }

    #[test]
    fn test_find_atm_security_ids_invalid_spot() {
        let strikes: Vec<f64> = (0..5).map(|i| 23000.0 + (i as f64) * 100.0).collect();
        let chain = make_chain(
            &strikes,
            "NIFTY",
            NaiveDate::from_ymd_opt(2026, 4, 9).unwrap(),
        );
        assert!(find_atm_security_ids(&chain, 0.0).is_none());
    }
}
