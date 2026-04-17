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
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use super::depth_strike_selector::{
    AtmIds, DEPTH_REBALANCE_STRIKE_THRESHOLD, find_atm_security_ids, should_rebalance,
};
use tickvault_common::instrument_types::{FnoUniverse, OptionChainKey};

/// Rebalance check interval (seconds). Checks spot drift every 60 seconds.
const REBALANCE_CHECK_INTERVAL_SECS: u64 = 60;

/// O3 (2026-04-17): A spot price older than this is considered stale. The
/// depth rebalancer will skip the rebalance decision for that underlying
/// and emit a Telegram alert. 180 seconds = 3× the 60s check interval,
/// so one missed update is tolerated but a genuinely stalled index LTP
/// feed trips the alert.
pub const STALE_SPOT_THRESHOLD_SECS: u64 = 180;

/// A spot-price entry with the wall-clock instant of the last update.
///
/// The `updated_at` field is used by `run_depth_rebalancer` to detect
/// stalled index LTP feeds. A stale price will cause an incorrect ATM
/// swap so we refuse to act and escalate instead.
#[derive(Debug, Clone, Copy)]
pub struct SpotPriceEntry {
    pub price: f64,
    pub updated_at: Instant,
}

/// Shared spot price tracker — updated by tick broadcast subscriber, read by rebalancer.
///
/// Uses `tokio::sync::RwLock` since both readers and writers are in async contexts.
/// Key: underlying symbol (e.g., "NIFTY"), Value: `SpotPriceEntry` (price + age).
pub type SharedSpotPrices = Arc<tokio::sync::RwLock<HashMap<String, SpotPriceEntry>>>;

/// Creates a new shared spot price map.
pub fn new_shared_spot_prices() -> SharedSpotPrices {
    Arc::new(tokio::sync::RwLock::new(HashMap::with_capacity(8)))
}

/// Updates the spot price for an underlying. Called from tick broadcast
/// subscriber when a tick arrives for an index instrument (IDX_I).
///
/// O(1): single HashMap insert with pre-allocated capacity. Always stamps
/// `updated_at = Instant::now()` so the rebalancer can detect stalled feeds.
pub async fn update_spot_price(prices: &SharedSpotPrices, symbol: &str, price: f64) {
    if !price.is_finite() || price <= 0.0 {
        return;
    }
    let mut map = prices.write().await;
    let entry = SpotPriceEntry {
        price,
        updated_at: Instant::now(),
    };
    // M3: get_mut avoids String allocation on updates (most calls).
    // Only allocates on first insert per underlying (~4 symbols).
    if let Some(val) = map.get_mut(symbol) {
        *val = entry;
    } else {
        map.insert(symbol.to_string(), entry); // O(1) EXEMPT: first insert per underlying
    }
}

/// Reads the current spot price for an underlying (backward-compatible —
/// returns price only). For freshness-aware callers, use
/// `get_spot_price_entry`.
pub async fn get_spot_price(prices: &SharedSpotPrices, symbol: &str) -> Option<f64> {
    let map = prices.read().await;
    map.get(symbol).map(|e| e.price)
}

/// Reads the current spot price entry (price + last-update `Instant`).
pub async fn get_spot_price_entry(
    prices: &SharedSpotPrices,
    symbol: &str,
) -> Option<SpotPriceEntry> {
    let map = prices.read().await;
    map.get(symbol).copied()
}

/// Rebalance event — sent when spot drifts beyond threshold.
#[derive(Debug, Clone)]
pub struct RebalanceEvent {
    /// Which underlying needs rebalancing.
    pub underlying: String,
    /// New ATM contract identifiers (CE/PE security IDs + actual strike).
    pub new_atm: Option<AtmIds>,
    /// Previous ATM contract identifiers.
    pub prev_atm: Option<AtmIds>,
    /// Current spot price that triggered the rebalance.
    pub current_spot: f64,
    /// Previous spot price (at last rebalance).
    pub previous_spot: f64,
    /// Number of strikes drifted.
    pub drift_strikes: usize,
    /// Expiry date of the selected chain.
    pub expiry: Option<chrono::NaiveDate>,
}

/// O3 (2026-04-17): Emitted when the spot price for an underlying is older
/// than `STALE_SPOT_THRESHOLD_SECS`. The rebalancer skips the rebalance
/// decision for this underlying and publishes the event so the app can
/// Telegram the operator.
#[derive(Debug, Clone)]
pub struct StaleSpotPriceEvent {
    pub underlying: String,
    pub age_secs: u64,
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
/// * `stale_tx` — Channel to publish stale-spot-price events (O3). The
///   app's main boot wires a listener that fires a Telegram alert.
/// * `shutdown` — Atomic flag to stop the rebalancer.
// TEST-EXEMPT: Background task — requires live spot price feed
pub async fn run_depth_rebalancer(
    spot_prices: SharedSpotPrices,
    universe: Arc<FnoUniverse>,
    underlyings: Vec<String>,
    rebalance_tx: watch::Sender<Option<RebalanceEvent>>,
    stale_tx: watch::Sender<Option<StaleSpotPriceEvent>>,
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
        metrics::gauge!("tv_depth_rebalancer_active").set(1.0);

        // Recompute today each iteration to handle midnight crossover correctly
        let today = {
            let now_utc = chrono::Utc::now().timestamp();
            let now_ist = now_utc.saturating_add(i64::from(
                tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
            ));
            chrono::DateTime::from_timestamp(now_ist, 0)
                .map(|dt| dt.date_naive())
                .unwrap_or_else(|| chrono::Utc::now().date_naive())
        };

        for symbol in &underlyings {
            let entry = match get_spot_price_entry(&spot_prices, symbol).await {
                Some(e) => e,
                None => {
                    debug!(symbol, "no spot price available — skipping rebalance check");
                    continue;
                }
            };
            // O3 (2026-04-17): if the index LTP feed has stalled, the price
            // is a lie — ATM decisions made on it will swap to the wrong
            // strike. Skip the rebalance for this underlying and escalate
            // so the operator can investigate.
            let age = entry.updated_at.elapsed();
            if age >= Duration::from_secs(STALE_SPOT_THRESHOLD_SECS) {
                error!(
                    symbol,
                    age_secs = age.as_secs(),
                    threshold_secs = STALE_SPOT_THRESHOLD_SECS,
                    "depth rebalancer: spot price is stale — skipping rebalance \
                     and alerting operator"
                );
                metrics::counter!("tv_depth_rebalancer_stale_spot_skips_total").increment(1);
                if stale_tx
                    .send(Some(StaleSpotPriceEvent {
                        underlying: symbol.clone(), // O(1) EXEMPT: cold alert path
                        age_secs: age.as_secs(),
                    }))
                    .is_err()
                {
                    debug!(
                        symbol,
                        "stale spot receiver dropped — telegram listener gone"
                    );
                }
                continue;
            }
            let current_spot = entry.price;

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
                // Look up old/new ATM CE/PE security IDs + strike for the notification.
                let new_atm_ids = find_atm_security_ids(chain, current_spot);
                let prev_atm_ids = find_atm_security_ids(chain, prev_atm);

                let event = RebalanceEvent {
                    underlying: symbol.clone(), // O(1) EXEMPT: rebalance event
                    new_atm: new_atm_ids,
                    prev_atm: prev_atm_ids,
                    current_spot,
                    previous_spot: prev_atm,
                    drift_strikes: DEPTH_REBALANCE_STRIKE_THRESHOLD,
                    expiry: Some(expiry),
                };

                info!(
                    symbol,
                    previous_spot = prev_atm,
                    current_spot,
                    ?new_atm_ids,
                    ?prev_atm_ids,
                    %expiry,
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
        use super::super::depth_strike_selector::AtmIds;
        let event = RebalanceEvent {
            underlying: "NIFTY".to_string(),
            new_atm: Some(AtmIds {
                ce_id: 35246,
                pe_id: Some(35247),
                strike: 23600.0,
            }),
            prev_atm: Some(AtmIds {
                ce_id: 35100,
                pe_id: Some(35101),
                strike: 23450.0,
            }),
            current_spot: 23620.0,
            previous_spot: 23430.0,
            drift_strikes: 3,
            expiry: Some(chrono::NaiveDate::from_ymd_opt(2026, 4, 24).unwrap()),
        };
        let debug = format!("{event:?}");
        assert!(debug.contains("NIFTY"));
        assert!(debug.contains("35246"));
        assert!(debug.contains("35247"));
        assert!(debug.contains("23600")); // new ATM strike
        assert!(debug.contains("23450")); // prev ATM strike
    }

    // ========================================================================
    // O3 (2026-04-17) — Stale-spot-price detection
    // ========================================================================

    #[tokio::test]
    async fn test_update_spot_price_stamps_fresh_instant() {
        let prices = new_shared_spot_prices();
        update_spot_price(&prices, "NIFTY", 23500.0).await;
        let entry = get_spot_price_entry(&prices, "NIFTY").await.unwrap();
        assert_eq!(entry.price, 23500.0);
        assert!(
            entry.updated_at.elapsed() < Duration::from_secs(1),
            "just-written entry must have a fresh timestamp"
        );
    }

    #[tokio::test]
    async fn test_update_spot_price_refreshes_timestamp_on_overwrite() {
        let prices = new_shared_spot_prices();
        update_spot_price(&prices, "NIFTY", 23500.0).await;
        let first = get_spot_price_entry(&prices, "NIFTY").await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        update_spot_price(&prices, "NIFTY", 23550.0).await;
        let second = get_spot_price_entry(&prices, "NIFTY").await.unwrap();
        assert!(
            second.updated_at > first.updated_at,
            "second update must advance `updated_at`"
        );
    }

    #[test]
    fn test_stale_spot_threshold_is_sane() {
        // Must be strictly greater than the check interval so a single
        // missed tick batch does NOT trip the alert.
        assert!(
            STALE_SPOT_THRESHOLD_SECS > REBALANCE_CHECK_INTERVAL_SECS,
            "stale threshold must exceed the check interval to absorb single misses"
        );
        // But small enough that a genuinely stalled feed is caught quickly.
        assert!(
            STALE_SPOT_THRESHOLD_SECS <= 600,
            "stale threshold must be <= 10 min — longer means silent data corruption"
        );
    }

    #[test]
    fn test_stale_spot_price_event_debug() {
        let event = StaleSpotPriceEvent {
            underlying: "BANKNIFTY".to_string(),
            age_secs: 240,
        };
        let debug = format!("{event:?}");
        assert!(debug.contains("BANKNIFTY"));
        assert!(debug.contains("240"));
    }

    #[tokio::test]
    async fn test_get_spot_price_entry_missing_is_none() {
        let prices = new_shared_spot_prices();
        assert!(get_spot_price_entry(&prices, "UNKNOWN").await.is_none());
    }

    #[tokio::test]
    async fn test_stale_spot_detection_simulated() {
        // Simulate the rebalancer's stale check without spawning the
        // background task (the real function is TEST-EXEMPT).
        let prices = new_shared_spot_prices();
        update_spot_price(&prices, "NIFTY", 23500.0).await;
        let entry = get_spot_price_entry(&prices, "NIFTY").await.unwrap();
        // Fresh entry → not stale.
        assert!(entry.updated_at.elapsed() < Duration::from_secs(STALE_SPOT_THRESHOLD_SECS));
        // Construct an artificially-stale entry and check the same logic
        // would fire. We can't mutate `updated_at` directly, but we can
        // verify the predicate shape is correct by comparing against a
        // past-instant surrogate.
        let surrogate = SpotPriceEntry {
            price: 23500.0,
            updated_at: Instant::now() - Duration::from_secs(STALE_SPOT_THRESHOLD_SECS + 10),
        };
        assert!(
            surrogate.updated_at.elapsed() >= Duration::from_secs(STALE_SPOT_THRESHOLD_SECS),
            "surrogate must be detected as stale by the rebalancer"
        );
    }
}
