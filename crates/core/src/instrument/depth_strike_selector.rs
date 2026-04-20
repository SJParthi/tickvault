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
    /// Security IDs of selected call options (ATM ± N), sorted by strike
    /// ascending. `call_security_ids.first()` is the LOWEST strike in the
    /// range (ATM − N), NOT the ATM strike. For the exact ATM pair use
    /// `atm_ce_security_id` / `atm_pe_security_id` below.
    pub call_security_ids: Vec<SecurityId>,
    /// Security IDs of selected put options (ATM ± N), sorted by strike
    /// ascending. See caveat above on `call_security_ids.first()`.
    pub put_security_ids: Vec<SecurityId>,
    /// All security IDs combined (calls + puts), ready for subscription.
    pub all_security_ids: Vec<SecurityId>,
    /// Security ID of the CE contract AT the ATM strike (not the range-start).
    /// `None` only if the chain is empty at the ATM index (should never happen
    /// for a non-empty chain returned by `select_atm_strikes`).
    ///
    /// **Use this — NOT `call_security_ids.first()` — when building the
    /// Telegram / log label for the ATM contract.** Getting these mixed up
    /// produces a label/security_id mismatch (e.g. label says
    /// `BANKNIFTY-Apr2026-56700-PE` while the security_id actually points
    /// at `BANKNIFTY-Apr2026-54300-PE`), which poisons operator triage.
    pub atm_ce_security_id: Option<SecurityId>,
    /// Security ID of the PE contract AT the ATM strike (not the range-start).
    /// `None` if no put exists at the exact ATM strike.
    pub atm_pe_security_id: Option<SecurityId>,
    /// Dhan CSV `display_name` for the ATM CE contract (e.g.
    /// `"BANKNIFTY 28 APR 54300 CALL"`). Populated by
    /// [`select_depth_instruments`] from
    /// `FnoUniverse::derivative_contracts`. `None` if the registry lookup
    /// fails (degrade gracefully — caller should fall back to the
    /// synthesized `UNDERLYING-MmmYYYY-STRIKE-SIDE` label).
    ///
    /// Prefer this over the synthesized label in operator-facing surfaces
    /// (Telegram, logs) because it matches exactly what Dhan's web UI and
    /// order book show.
    pub atm_ce_display_name: Option<String>,
    /// Dhan CSV `display_name` for the ATM PE contract (e.g.
    /// `"BANKNIFTY 28 APR 54300 PUT"`). Same semantics as
    /// `atm_ce_display_name` above.
    pub atm_pe_display_name: Option<String>,
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

    // Capture the EXACT ATM CE + PE security IDs BEFORE the range expansion.
    // Using `call_security_ids.first()` downstream would return the lowest
    // strike in the range (ATM − N), not the ATM itself — a real bug that
    // previously produced Telegram alerts like
    // `BANKNIFTY-Apr2026-56700-PE (SID 67481)` where the SID actually
    // pointed at strike 54300. The two identities are kept here so every
    // caller can reliably grab the ATM pair without re-deriving it.
    let atm_ce_security_id = Some(chain.calls[atm_idx].security_id);
    let atm_pe_security_id = find_put_at_strike(&chain.puts, atm_strike).map(|p| p.security_id);

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
        atm_ce_security_id,
        atm_pe_security_id,
        // Filled in by `select_depth_instruments` (which has registry access)
        // or by `fill_display_names_from_universe` for direct callers. Left
        // as None here because `OptionChain` does not carry display_name.
        atm_ce_display_name: None,
        atm_pe_display_name: None,
        underlying_symbol: chain.underlying_symbol.clone(),
        expiry_date: chain.expiry_date,
    })
}

impl DepthStrikeSelection {
    /// Populates `atm_ce_display_name` / `atm_pe_display_name` by looking
    /// up the ATM security IDs in the universe's `derivative_contracts`
    /// map. Idempotent — calling twice is safe.
    ///
    /// Used so that Telegram alerts and operator logs show the exact
    /// Dhan-UI label (`"BANKNIFTY 28 APR 54300 PUT"`) instead of the
    /// synthesized `symbol_name` (`"BANKNIFTY-Apr2026-54300-PE"`). The
    /// two can be misleading to an operator scanning the web terminal in
    /// parallel with Telegram.
    pub fn fill_display_names_from_universe(&mut self, universe: &FnoUniverse) {
        if let Some(sid) = self.atm_ce_security_id
            && let Some(contract) = universe.derivative_contracts.get(&sid)
        {
            self.atm_ce_display_name = Some(contract.display_name.clone());
        }
        if let Some(sid) = self.atm_pe_security_id
            && let Some(contract) = universe.derivative_contracts.get(&sid)
        {
            self.atm_pe_display_name = Some(contract.display_name.clone());
        }
    }
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
        if let Some(mut selection) = select_atm_strikes(chain, spot, strikes_each_side) {
            // Enrich with Dhan CSV `display_name` so every downstream
            // operator surface (Telegram alert, info log, web panel) can
            // show the exact string Dhan's UI shows for the same contract.
            selection.fill_display_names_from_universe(universe);
            tracing::info!(
                symbol,
                atm_strike = selection.atm_strike,
                atm_ce_sid = ?selection.atm_ce_security_id,
                atm_pe_sid = ?selection.atm_pe_security_id,
                atm_ce_display = ?selection.atm_ce_display_name,
                atm_pe_display = ?selection.atm_pe_display_name,
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
#[derive(Debug, Clone, PartialEq)]
pub struct AtmIds {
    /// CE security ID at ATM strike.
    pub ce_id: u32,
    /// PE security ID at ATM strike (None if no matching put).
    pub pe_id: Option<u32>,
    /// The actual strike price in the chain closest to spot.
    pub strike: f64,
    /// Dhan CSV `display_name` for the CE contract, if the caller enriches
    /// via [`AtmIds::fill_display_names_from_universe`]. `None` from bare
    /// [`find_atm_security_ids`] which has no registry access.
    pub ce_display_name: Option<String>,
    /// Dhan CSV `display_name` for the PE contract. Same semantics as above.
    pub pe_display_name: Option<String>,
}

impl AtmIds {
    /// Populates `ce_display_name` / `pe_display_name` from the universe's
    /// `derivative_contracts` map. Idempotent. Used so that rebalancer
    /// Telegram messages can show the exact Dhan web-UI label
    /// (e.g. `"BANKNIFTY 28 APR 54300 PUT"`) instead of the synthesized
    /// `UNDERLYING-MmmYYYY-STRIKE-SIDE` format.
    pub fn fill_display_names_from_universe(&mut self, universe: &FnoUniverse) {
        if let Some(contract) = universe.derivative_contracts.get(&self.ce_id) {
            self.ce_display_name = Some(contract.display_name.clone());
        }
        if let Some(pe_id) = self.pe_id
            && let Some(contract) = universe.derivative_contracts.get(&pe_id)
        {
            self.pe_display_name = Some(contract.display_name.clone());
        }
    }
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
        ce_display_name: None,
        pe_display_name: None,
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

    // -----------------------------------------------------------------------
    // Regression: ATM CE/PE security_id fields must point at the ACTUAL ATM
    // strike, not the lowest strike in the ATM ± N range. This is the bug
    // that produced Telegram alerts like "BANKNIFTY-Apr2026-56700-PE (SID
    // 67481)" where the SID actually pointed at strike 54300 (range-first,
    // not ATM). The fix moved off `.first()` onto dedicated fields.
    // -----------------------------------------------------------------------

    #[test]
    fn atm_ce_security_id_is_exact_atm_not_range_first() {
        // 49 strikes (ATM ± 24) — mirrors production DEPTH_ATM_STRIKES_EACH_SIDE.
        // ATM is at index 24 of the 49-element range.
        let strikes: Vec<f64> = (0..49).map(|i| 54000.0 + (i as f64) * 100.0).collect();
        let chain = make_chain(
            &strikes,
            "BANKNIFTY",
            NaiveDate::from_ymd_opt(2026, 4, 28).unwrap(),
        );

        // Spot near strike 56400 → ATM should be 56400 (index 24).
        let selection = select_atm_strikes(&chain, 56400.0, 24).unwrap();

        // ATM strike is 56400, and the CE security_id from make_chain is 10000 + 24 = 10024.
        assert!((selection.atm_strike - 56400.0).abs() < 0.01);
        assert_eq!(
            selection.atm_ce_security_id,
            Some(10024),
            "atm_ce_security_id must point at the ATM strike (10024), \
             NOT the range-first strike (10000)"
        );
        assert_eq!(
            selection.atm_pe_security_id,
            Some(20024),
            "atm_pe_security_id must point at the ATM strike (20024), \
             NOT the range-first strike (20000)"
        );

        // And prove the historical bug pattern — `.first()` returns the
        // LOWEST strike in the range, which is NOT the ATM.
        assert_eq!(
            selection.call_security_ids.first().copied(),
            Some(10000),
            "call_security_ids.first() returns range-start (10000 = strike 54000), \
             which is why it was wrong to use for the ATM label"
        );
        assert_ne!(
            selection.call_security_ids.first().copied(),
            selection.atm_ce_security_id,
            "this assertion documents the exact bug: .first() != atm_ce_security_id"
        );
    }

    #[test]
    fn atm_ce_matches_find_atm_security_ids() {
        // Cross-check: the new field and the standalone helper must agree.
        let strikes: Vec<f64> = (0..49).map(|i| 54000.0 + (i as f64) * 100.0).collect();
        let chain = make_chain(
            &strikes,
            "BANKNIFTY",
            NaiveDate::from_ymd_opt(2026, 4, 28).unwrap(),
        );
        let sel = select_atm_strikes(&chain, 56400.0, 24).unwrap();
        let via_helper = find_atm_security_ids(&chain, 56400.0).unwrap();
        assert_eq!(sel.atm_ce_security_id, Some(via_helper.ce_id));
        assert_eq!(sel.atm_pe_security_id, via_helper.pe_id);
    }

    #[test]
    fn atmids_fill_display_names_from_universe_populates_both_sides() {
        use std::collections::HashMap;
        use tickvault_common::instrument_types::{
            DerivativeContract, DhanInstrumentKind, FnoUniverse, UniverseBuildMetadata,
        };
        use tickvault_common::types::{ExchangeSegment, OptionType};

        let expiry = NaiveDate::from_ymd_opt(2026, 4, 28).unwrap();
        let mut derivative_contracts = HashMap::new();
        let mk = |sid: u32, strike: f64, ot: OptionType, dn: &str, sn: &str| DerivativeContract {
            security_id: sid,
            underlying_symbol: "BANKNIFTY".to_string(),
            instrument_kind: DhanInstrumentKind::OptionIndex,
            exchange_segment: ExchangeSegment::NseFno,
            expiry_date: expiry,
            strike_price: strike,
            option_type: Some(ot),
            lot_size: 15,
            tick_size: 0.05,
            symbol_name: sn.to_string(),
            display_name: dn.to_string(),
        };
        derivative_contracts.insert(
            67480_u32,
            mk(
                67480,
                54300.0,
                OptionType::Call,
                "BANKNIFTY 28 APR 54300 CALL",
                "BANKNIFTY-Apr2026-54300-CE",
            ),
        );
        derivative_contracts.insert(
            67481_u32,
            mk(
                67481,
                54300.0,
                OptionType::Put,
                "BANKNIFTY 28 APR 54300 PUT",
                "BANKNIFTY-Apr2026-54300-PE",
            ),
        );
        let universe = FnoUniverse {
            underlyings: HashMap::new(),
            derivative_contracts,
            instrument_info: HashMap::new(),
            option_chains: HashMap::new(),
            expiry_calendars: HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: UniverseBuildMetadata {
                csv_source: "test".to_string(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 0,
                derivative_count: 0,
                option_chain_count: 0,
                build_duration: std::time::Duration::ZERO,
                build_timestamp: chrono::Utc::now()
                    .with_timezone(&chrono::FixedOffset::east_opt(19_800).unwrap()),
            },
        };

        let mut atm = AtmIds {
            ce_id: 67480,
            pe_id: Some(67481),
            strike: 54300.0,
            ce_display_name: None,
            pe_display_name: None,
        };
        atm.fill_display_names_from_universe(&universe);
        assert_eq!(
            atm.ce_display_name.as_deref(),
            Some("BANKNIFTY 28 APR 54300 CALL")
        );
        assert_eq!(
            atm.pe_display_name.as_deref(),
            Some("BANKNIFTY 28 APR 54300 PUT")
        );

        // No registry entry → display_name stays None (no panic).
        let mut missing = AtmIds {
            ce_id: 99999,
            pe_id: Some(99998),
            strike: 99999.0,
            ce_display_name: None,
            pe_display_name: None,
        };
        missing.fill_display_names_from_universe(&universe);
        assert_eq!(missing.ce_display_name, None);
        assert_eq!(missing.pe_display_name, None);
    }

    #[test]
    fn fill_display_names_uses_registry_display_name() {
        use std::collections::HashMap;
        use tickvault_common::instrument_types::{
            DerivativeContract, DhanInstrumentKind, FnoUniverse, UniverseBuildMetadata,
        };
        use tickvault_common::types::{ExchangeSegment, OptionType};

        let strikes: Vec<f64> = (0..5).map(|i| 23000.0 + (i as f64) * 100.0).collect();
        let chain = make_chain(
            &strikes,
            "NIFTY",
            NaiveDate::from_ymd_opt(2026, 4, 28).unwrap(),
        );
        // ATM @ 23200 → CE sid 10002, PE sid 20002 (from make_chain).
        let mut sel = select_atm_strikes(&chain, 23200.0, 2).unwrap();

        // Build a minimal universe with display_names for the ATM pair.
        let mut derivative_contracts = HashMap::new();
        let expiry = NaiveDate::from_ymd_opt(2026, 4, 28).unwrap();
        derivative_contracts.insert(
            10002_u32,
            DerivativeContract {
                security_id: 10002,
                underlying_symbol: "NIFTY".to_string(),
                instrument_kind: DhanInstrumentKind::OptionIndex,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: expiry,
                strike_price: 23200.0,
                option_type: Some(OptionType::Call),
                lot_size: 50,
                tick_size: 0.05,
                symbol_name: "NIFTY-Apr2026-23200-CE".to_string(),
                display_name: "NIFTY 28 APR 23200 CALL".to_string(),
            },
        );
        derivative_contracts.insert(
            20002_u32,
            DerivativeContract {
                security_id: 20002,
                underlying_symbol: "NIFTY".to_string(),
                instrument_kind: DhanInstrumentKind::OptionIndex,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: expiry,
                strike_price: 23200.0,
                option_type: Some(OptionType::Put),
                lot_size: 50,
                tick_size: 0.05,
                symbol_name: "NIFTY-Apr2026-23200-PE".to_string(),
                display_name: "NIFTY 28 APR 23200 PUT".to_string(),
            },
        );
        let universe = FnoUniverse {
            underlyings: HashMap::new(),
            derivative_contracts,
            instrument_info: HashMap::new(),
            option_chains: HashMap::new(),
            expiry_calendars: HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: UniverseBuildMetadata {
                csv_source: "test".to_string(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 0,
                derivative_count: 0,
                option_chain_count: 0,
                build_duration: std::time::Duration::ZERO,
                build_timestamp: chrono::Utc::now()
                    .with_timezone(&chrono::FixedOffset::east_opt(19_800).unwrap()),
            },
        };

        sel.fill_display_names_from_universe(&universe);
        assert_eq!(
            sel.atm_ce_display_name.as_deref(),
            Some("NIFTY 28 APR 23200 CALL"),
            "CE display_name must come from the registry, not be synthesized"
        );
        assert_eq!(
            sel.atm_pe_display_name.as_deref(),
            Some("NIFTY 28 APR 23200 PUT")
        );
    }

    #[test]
    fn atm_security_id_correct_near_chain_edge() {
        // If ATM is near the start of the chain, the range gets clamped.
        // `.first()` = lowest strike in clamped range.
        // Dedicated field must still point at the true ATM strike.
        let strikes: Vec<f64> = (0..20).map(|i| 23000.0 + (i as f64) * 100.0).collect();
        let chain = make_chain(
            &strikes,
            "NIFTY",
            NaiveDate::from_ymd_opt(2026, 4, 28).unwrap(),
        );

        // Spot near strike 23200 → ATM index = 2. Range-start clamps at 0.
        let sel = select_atm_strikes(&chain, 23200.0, 10).unwrap();
        assert!((sel.atm_strike - 23200.0).abs() < 0.01);
        assert_eq!(
            sel.atm_ce_security_id,
            Some(10002),
            "ATM CE at index 2 → security_id 10002 (make_chain baseline)"
        );
        assert_eq!(
            sel.call_security_ids.first().copied(),
            Some(10000),
            "range-first when ATM is near edge is still 10000 (clamped)"
        );
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
