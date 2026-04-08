//! Dynamic depth rebalancer — monitors spot price and re-subscribes depth
//! connections when ATM drifts beyond threshold.
//!
//! Runs as a background task during market hours. Every `check_interval_secs`
//! seconds, reads the latest spot price for each underlying from the tick
//! broadcast and checks if ATM has drifted. If drift exceeds `strike_threshold`,
//! sends a rebalance signal.
//!
//! # Architecture
//! The rebalancer does NOT directly manage WebSocket connections.
//! It publishes rebalance events via a channel. The boot sequence
//! can use these events to restart depth connections with new instruments.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use tracing::{debug, info, warn};

use super::depth_strike_selector::{DEPTH_REBALANCE_STRIKE_THRESHOLD, should_rebalance};
use dhan_live_trader_common::instrument_types::{FnoUniverse, OptionChainKey};

/// Rebalance check interval (seconds). Checks spot drift every 60 seconds.
const REBALANCE_CHECK_INTERVAL_SECS: u64 = 60;

/// Shared spot price tracker — updated by tick broadcast subscriber, read by rebalancer.
///
/// Uses `tokio::sync::RwLock` since both readers and writers are in async contexts.
/// Key: underlying symbol (e.g., "NIFTY"), Value: latest spot price.
pub type SharedSpotPrices = Arc<tokio::sync::RwLock<HashMap<String, f64>>>;

/// Creates a new shared spot price map.
pub fn new_shared_spot_prices() -> SharedSpotPrices {
    Arc::new(tokio::sync::RwLock::new(HashMap::with_capacity(8)))
}

/// Updates the spot price for an underlying. Called from tick broadcast
/// subscriber when a tick arrives for an index instrument (IDX_I).
///
/// O(1): single HashMap insert with pre-allocated capacity.
pub async fn update_spot_price(prices: &SharedSpotPrices, symbol: &str, price: f64) {
    if !price.is_finite() || price <= 0.0 {
        return;
    }
    let mut map = prices.write().await;
    // M3: get_mut avoids String allocation on updates (most calls).
    // Only allocates on first insert per underlying (~4 symbols).
    if let Some(val) = map.get_mut(symbol) {
        *val = price;
    } else {
        map.insert(symbol.to_string(), price); // O(1) EXEMPT: first insert per underlying
    }
}

/// Reads the current spot price for an underlying.
pub async fn get_spot_price(prices: &SharedSpotPrices, symbol: &str) -> Option<f64> {
    let map = prices.read().await;
    map.get(symbol).copied()
}

/// Rebalance event — sent when spot drifts beyond threshold.
#[derive(Debug, Clone)]
pub struct RebalanceEvent {
    /// Which underlying needs rebalancing.
    pub underlying: String,
    /// New ATM strike price.
    pub new_atm_strike: f64,
    /// Previous ATM strike price.
    pub previous_atm_strike: f64,
    /// Number of strikes drifted.
    pub drift_strikes: usize,
}

/// Runs the depth rebalancer as a background task.
///
/// Checks spot prices every 5 minutes during market hours.
/// When drift exceeds threshold, sends rebalance event.
///
/// # Arguments
/// * `spot_prices` — Shared spot price map (updated by tick processor).
/// * `universe` — FnoUniverse with option chains for ATM lookup.
/// * `underlyings` — Symbols to monitor (e.g., ["NIFTY", "BANKNIFTY"]).
/// * `rebalance_tx` — Channel to send rebalance events.
/// * `shutdown` — Atomic flag to stop the rebalancer.
// TEST-EXEMPT: Background task — requires live spot price feed
pub async fn run_depth_rebalancer(
    spot_prices: SharedSpotPrices,
    universe: Arc<FnoUniverse>,
    underlyings: Vec<String>,
    rebalance_tx: watch::Sender<Option<RebalanceEvent>>,
    shutdown: Arc<AtomicBool>,
) {
    info!(
        underlyings = ?underlyings,
        check_interval_secs = REBALANCE_CHECK_INTERVAL_SECS,
        strike_threshold = DEPTH_REBALANCE_STRIKE_THRESHOLD,
        "depth rebalancer started"
    );

    // Track previous ATM strikes per underlying
    let mut previous_atm: HashMap<String, f64> = HashMap::with_capacity(underlyings.len());

    // Initialize with current spot prices
    for symbol in &underlyings {
        if let Some(spot) = get_spot_price(&spot_prices, symbol).await {
            previous_atm.insert(symbol.clone(), spot); // O(1) EXEMPT: init
            info!(
                symbol,
                spot, "depth rebalancer initialized ATM for underlying"
            );
        }
    }

    loop {
        if shutdown.load(Ordering::Relaxed) {
            info!("depth rebalancer shutting down");
            break;
        }

        tokio::time::sleep(Duration::from_secs(REBALANCE_CHECK_INTERVAL_SECS)).await;

        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // O2: Heartbeat — proves the rebalancer loop is alive.
        metrics::gauge!("dlt_depth_rebalancer_active").set(1.0);

        // Recompute today each iteration to handle midnight crossover correctly
        let today = {
            let now_utc = chrono::Utc::now().timestamp();
            let now_ist = now_utc.saturating_add(i64::from(
                dhan_live_trader_common::constants::IST_UTC_OFFSET_SECONDS,
            ));
            chrono::DateTime::from_timestamp(now_ist, 0)
                .map(|dt| dt.date_naive())
                .unwrap_or_else(|| chrono::Utc::now().date_naive())
        };

        for symbol in &underlyings {
            let current_spot = match get_spot_price(&spot_prices, symbol).await {
                Some(p) => p,
                None => {
                    debug!(symbol, "no spot price available — skipping rebalance check");
                    continue;
                }
            };

            let prev_atm = match previous_atm.get(symbol) {
                Some(&p) => p,
                None => {
                    // First time seeing this underlying — initialize
                    previous_atm.insert(symbol.clone(), current_spot); // O(1) EXEMPT: init
                    continue;
                }
            };

            // Find the nearest expiry option chain for this underlying
            let nearest_expiry = universe
                .expiry_calendars
                .get(symbol.as_str())
                .and_then(|cal| cal.expiry_dates.iter().find(|&&e| e >= today).copied());

            let expiry = match nearest_expiry {
                Some(e) => e,
                None => continue,
            };

            let chain_key = OptionChainKey {
                underlying_symbol: symbol.clone(), // O(1) EXEMPT: rebalance check
                expiry_date: expiry,
            };

            let chain = match universe.option_chains.get(&chain_key) {
                Some(c) => c,
                None => continue,
            };

            if should_rebalance(
                chain,
                prev_atm,
                current_spot,
                DEPTH_REBALANCE_STRIKE_THRESHOLD,
            ) {
                let event = RebalanceEvent {
                    underlying: symbol.clone(), // O(1) EXEMPT: rebalance event
                    new_atm_strike: current_spot,
                    previous_atm_strike: prev_atm,
                    drift_strikes: DEPTH_REBALANCE_STRIKE_THRESHOLD,
                };

                info!(
                    symbol,
                    previous_atm = prev_atm,
                    current_spot,
                    "depth rebalance triggered — ATM drifted beyond threshold"
                );

                // Update tracked ATM
                previous_atm.insert(symbol.clone(), current_spot); // O(1) EXEMPT: rebalance

                // Send rebalance event
                if rebalance_tx.send(Some(event)).is_err() {
                    warn!(symbol, "rebalance receiver dropped — stopping rebalancer");
                    return;
                }
            } else {
                debug!(
                    symbol,
                    previous_atm = prev_atm,
                    current_spot,
                    "spot within threshold — no rebalance needed"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_new_shared_spot_prices() {
        let prices = new_shared_spot_prices();
        assert!(prices.read().await.is_empty());
    }

    #[tokio::test]
    async fn test_update_and_get_spot_price() {
        let prices = new_shared_spot_prices();
        update_spot_price(&prices, "NIFTY", 23500.0).await;
        assert_eq!(get_spot_price(&prices, "NIFTY").await, Some(23500.0));
    }

    #[tokio::test]
    async fn test_update_spot_price_rejects_invalid() {
        let prices = new_shared_spot_prices();
        update_spot_price(&prices, "NIFTY", f64::NAN).await;
        assert_eq!(get_spot_price(&prices, "NIFTY").await, None);

        update_spot_price(&prices, "NIFTY", 0.0).await;
        assert_eq!(get_spot_price(&prices, "NIFTY").await, None);

        update_spot_price(&prices, "NIFTY", -100.0).await;
        assert_eq!(get_spot_price(&prices, "NIFTY").await, None);
    }

    #[tokio::test]
    async fn test_get_spot_price_missing() {
        let prices = new_shared_spot_prices();
        assert_eq!(get_spot_price(&prices, "UNKNOWN").await, None);
    }

    #[tokio::test]
    async fn test_update_spot_price_overwrites() {
        let prices = new_shared_spot_prices();
        update_spot_price(&prices, "NIFTY", 23500.0).await;
        update_spot_price(&prices, "NIFTY", 23600.0).await;
        assert_eq!(get_spot_price(&prices, "NIFTY").await, Some(23600.0));
    }

    #[tokio::test]
    async fn test_multiple_underlyings() {
        let prices = new_shared_spot_prices();
        update_spot_price(&prices, "NIFTY", 23500.0).await;
        update_spot_price(&prices, "BANKNIFTY", 51000.0).await;
        update_spot_price(&prices, "FINNIFTY", 24500.0).await;
        update_spot_price(&prices, "MIDCPNIFTY", 12500.0).await;

        assert_eq!(get_spot_price(&prices, "NIFTY").await, Some(23500.0));
        assert_eq!(get_spot_price(&prices, "BANKNIFTY").await, Some(51000.0));
        assert_eq!(get_spot_price(&prices, "FINNIFTY").await, Some(24500.0));
        assert_eq!(get_spot_price(&prices, "MIDCPNIFTY").await, Some(12500.0));
    }

    #[test]
    fn test_rebalance_check_interval() {
        assert_eq!(REBALANCE_CHECK_INTERVAL_SECS, 60);
    }

    #[test]
    fn test_rebalance_event_debug() {
        let event = RebalanceEvent {
            underlying: "NIFTY".to_string(),
            new_atm_strike: 23600.0,
            previous_atm_strike: 23450.0,
            drift_strikes: 3,
        };
        let debug = format!("{event:?}");
        assert!(debug.contains("NIFTY"));
        assert!(debug.contains("23600"));
    }
}
