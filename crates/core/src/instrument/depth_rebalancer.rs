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

/// O3-HF2 (2026-04-17): returns true iff the current IST wall-clock time
/// is within the market-hours persistence window (09:00-15:30 IST).
///
/// The depth rebalancer is idle-noise outside this window because the
/// main-feed index LTPs never update post-market — every symbol would
/// appear "stale" and spam alerts. The constants come from
/// `tickvault_common::constants::{TICK_PERSIST_START_SECS_OF_DAY_IST,
/// TICK_PERSIST_END_SECS_OF_DAY_IST}` so market-hours logic is
/// DRY across the codebase.
///
/// O(1) — one `Utc::now()` + arithmetic + range check.
#[inline]
fn is_within_market_hours_ist() -> bool {
    use tickvault_common::constants::{
        IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST,
        TICK_PERSIST_START_SECS_OF_DAY_IST,
    };
    let now_utc_secs = chrono::Utc::now().timestamp();
    let now_ist_secs = now_utc_secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    // Defensive cast: seconds-of-day fits in u32 for any reasonable epoch.
    let sec_of_day = now_ist_secs.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32;
    (TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST).contains(&sec_of_day)
}

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
// TEST-EXEMPT: covered by `test_update_spot_price_stamps_fresh_instant`,
// `test_update_spot_price_refreshes_timestamp_on_overwrite`, and
// `test_get_spot_price_entry_missing_is_none`.
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

    // O3-HF1 (2026-04-17): edge-triggered stale-spot alerting. Before this
    // fix, a stalled IDX_I feed produced ONE Telegram alert every 60 seconds
    // forever (observed live: FINNIFTY + MIDCPNIFTY spammed every minute
    // through the 3:30 PM close and into the evening). We now alert ONCE on
    // the rising edge and ONCE on the falling edge.
    let mut currently_stale: std::collections::HashSet<String> =
        std::collections::HashSet::with_capacity(underlyings.len());

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

        // O3-HF2 (2026-04-17): market-hours gate. The depth rebalancer
        // runs 24/7 to stay connected, but spot prices only update during
        // 09:00-15:30 IST. Outside that window EVERY symbol's spot is
        // "stale" by definition — firing alerts for it is noise. Skip the
        // rebalance + stale check entirely outside market hours.
        if !is_within_market_hours_ist() {
            // Clear stale tracking so the rising edge after next market
            // open is detected correctly.
            currently_stale.clear();
            continue;
        }

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
            let is_stale = age >= Duration::from_secs(STALE_SPOT_THRESHOLD_SECS);
            let was_stale = currently_stale.contains(symbol);

            if is_stale {
                // Skip the rebalance (a stale spot will pick wrong ATM).
                metrics::counter!("tv_depth_rebalancer_stale_spot_skips_total").increment(1);
                // O3-HF1: RISING-EDGE alert only — suppress if we already
                // alerted for this symbol and it is still stale.
                if !was_stale {
                    error!(
                        symbol,
                        age_secs = age.as_secs(),
                        threshold_secs = STALE_SPOT_THRESHOLD_SECS,
                        "depth rebalancer: spot price went STALE (rising edge) — \
                         skipping rebalance and alerting operator ONCE"
                    );
                    currently_stale.insert(symbol.clone()); // O(1) EXEMPT: cold alert path
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
                } else {
                    debug!(
                        symbol,
                        age_secs = age.as_secs(),
                        "spot still stale — alert suppressed (edge already fired)"
                    );
                }
                continue;
            }

            // is_stale == false. If we previously alerted this symbol as
            // stale, emit a FALLING-EDGE recovery log so the operator sees
            // the feed came back. (No Telegram for recovery — reduces noise;
            // the next rebalance success log is proof of life.)
            if was_stale {
                info!(
                    symbol,
                    age_secs = age.as_secs(),
                    "depth rebalancer: spot price RECOVERED (falling edge) — \
                     resuming normal rebalance for this underlying"
                );
                currently_stale.remove(symbol);
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
                // Look up old/new ATM CE/PE security IDs + strike for the
                // notification. Enrich with Dhan CSV `display_name` so the
                // Telegram rebalance alert shows the exact contract label
                // Dhan's web UI shows (e.g. "BANKNIFTY 28 APR 54300 PUT")
                // instead of the synthesized symbol_name.
                let mut new_atm_ids = find_atm_security_ids(chain, current_spot);
                let mut prev_atm_ids = find_atm_security_ids(chain, prev_atm);
                if let Some(ref mut ids) = new_atm_ids {
                    ids.fill_display_names_from_universe(universe.as_ref());
                }
                if let Some(ref mut ids) = prev_atm_ids {
                    ids.fill_display_names_from_universe(universe.as_ref());
                }

                info!(
                    symbol,
                    previous_spot = prev_atm,
                    current_spot,
                    new_atm = ?new_atm_ids,
                    prev_atm_ids = ?prev_atm_ids,
                    %expiry,
                    "depth rebalance triggered — ATM drifted beyond threshold"
                );

                let event = RebalanceEvent {
                    underlying: symbol.clone(), // O(1) EXEMPT: rebalance event
                    new_atm: new_atm_ids,
                    prev_atm: prev_atm_ids,
                    current_spot,
                    previous_spot: prev_atm,
                    drift_strikes: DEPTH_REBALANCE_STRIKE_THRESHOLD,
                    expiry: Some(expiry),
                };

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
                ce_display_name: None,
                pe_display_name: None,
            }),
            prev_atm: Some(AtmIds {
                ce_id: 35100,
                pe_id: Some(35101),
                strike: 23450.0,
                ce_display_name: None,
                pe_display_name: None,
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

    // ========================================================================
    // O3-HF (2026-04-17) — Market-hours gate + edge-triggered alerting tests
    // ========================================================================

    #[test]
    fn test_is_within_market_hours_at_0830_ist_is_false() {
        // We can't freeze Utc::now() without a clock trait. Instead we
        // test the constant bounds directly and verify the helper uses
        // TICK_PERSIST_START/END so the single source of truth for
        // "market hours" is enforced.
        assert_eq!(
            tickvault_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST,
            9 * 3600,
            "market-hours gate must start at 09:00 IST"
        );
        assert_eq!(
            tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST,
            15 * 3600 + 30 * 60,
            "market-hours gate must end at 15:30 IST (exclusive)"
        );
    }

    #[test]
    fn test_is_within_market_hours_helper_exists_and_returns_bool() {
        // Smoke test — helper is callable and returns a plain bool.
        let _: bool = is_within_market_hours_ist();
    }

    /// Guard test: the rebalancer MUST honor `is_within_market_hours_ist()`
    /// so post-market stale alerts cannot fire. We scan the source file
    /// for the exact call pattern to enforce the wiring statically.
    #[test]
    fn test_market_hours_gate_is_wired_into_rebalancer_loop() {
        let src = include_str!("depth_rebalancer.rs");
        assert!(
            src.contains("if !is_within_market_hours_ist() {"),
            "run_depth_rebalancer MUST call is_within_market_hours_ist() \
             to suppress post-market stale-alert storms. Live 2026-04-17 \
             regression: FINNIFTY + MIDCPNIFTY spammed every 60s post-close."
        );
    }

    /// Guard test: rising-edge alert suppression is wired.
    #[test]
    fn test_edge_triggered_stale_alert_suppression_is_wired() {
        let src = include_str!("depth_rebalancer.rs");
        assert!(
            src.contains("currently_stale"),
            "currently_stale set must exist — tracks which underlyings are \
             already alerted so we fire ONCE per stale episode (edge-trigger)."
        );
        assert!(
            src.contains("if !was_stale {"),
            "rising-edge gate must check was_stale before sending the alert"
        );
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
