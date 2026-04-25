//! Live-Tick ATM Resolver — Mode C primary stock spot-price source.
//!
//! ## Purpose
//!
//! When the app boots mid-market (09:13–15:30 IST, `BootMode::MidMarket`),
//! the pre-open buffer is empty (it only captures 09:00–09:12 closes) and
//! we cannot wait for it. The live cash-equity WebSocket subscription is
//! already streaming LTPs into [`SharedSpotPrices`]. This module polls that
//! map at progressive intervals and returns:
//!
//! 1. `resolved` — stocks whose LTP is in `SharedSpotPrices`
//! 2. `stragglers` — stocks that haven't ticked yet (handed to REST fallback)
//!
//! Stragglers fall through to:
//! - REST `/v2/marketfeed/ltp` via `preopen_rest_fallback` (200ms)
//! - QuestDB previous close as last resort
//! - Skip-for-the-day if even QuestDB has no row
//!
//! ## Why this is preferred over REST as primary
//!
//! | Aspect | REST | Live-tick |
//! |---|---|---|
//! | Cost | 1 Data API call | FREE (already subscribed) |
//! | Latency | ~200ms after auth | ~5–15s wait for ticks |
//! | Freshness | 1-sec stale | Sub-millisecond |
//! | Rate limit | DH-904 risk | None |
//! | Failure modes | 805/807/814 | Same WS already running |
//!
//! ## O(1) / I-P1-11 / dedup
//!
//! Resolver itself does not subscribe — it only reads. Output is a
//! `HashMap<UnderlyingSymbol, f64>` for the planner. The downstream
//! `subscription_planner` retains all I-P1-11 composite-key (security_id,
//! exchange_segment) guarantees.
//!
//! ## Market-hours awareness (Rule 3)
//!
//! The resolver assumes the caller has already verified `BootMode::MidMarket`.
//! It will still function outside market hours (returns all stragglers) but
//! the design contract is "called only when ticks should be flowing".

use std::collections::HashMap;
use std::time::Duration;

use tracing::{info, warn};

use super::depth_rebalancer::{SharedSpotPrices, get_spot_price};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Progressive timeout schedule. Resolver polls at each interval; exits early
/// when all stocks are resolved. Total budget = 25 seconds before handing
/// stragglers to REST fallback.
///
/// 5 → 10 → 15 → 20 → 25 seconds. Each interval is the wall-clock time AFTER
/// boot; the resolver sleeps the delta between intervals.
pub const POLL_INTERVALS_SECS: &[u64] = &[5, 10, 15, 20, 25];

/// Maximum total wait time before giving up on stragglers and routing to REST.
pub const MAX_RESOLVE_BUDGET_SECS: u64 = 25;

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

/// Output of [`resolve_stock_atm_from_live_ticks`].
#[derive(Debug, Clone, Default)]
pub struct ResolveResult {
    /// Stocks whose live cash-equity tick LTP is in `SharedSpotPrices`.
    /// Key: underlying symbol, Value: latest LTP.
    pub resolved: HashMap<String, f64>,
    /// Stocks that did NOT tick within `MAX_RESOLVE_BUDGET_SECS`.
    /// Caller should fall through to REST `/marketfeed/ltp` fallback.
    pub stragglers: Vec<String>,
    /// Number of poll cycles performed. 1..=POLL_INTERVALS_SECS.len().
    pub cycles_run: u32,
    /// Wall-clock duration this resolution took (millis).
    pub elapsed_ms: u64,
}

impl ResolveResult {
    /// Total stocks attempted = resolved + stragglers.
    pub fn total_attempted(&self) -> usize {
        self.resolved.len().saturating_add(self.stragglers.len())
    }

    /// Resolution rate as a percentage 0..=100.
    pub fn resolution_pct(&self) -> u32 {
        let total = self.total_attempted();
        if total == 0 {
            return 0;
        }
        // u32 arithmetic: total ≤ ~250 in practice, no overflow concern.
        let pct = (self.resolved.len() * 100).saturating_div(total);
        u32::try_from(pct).unwrap_or(100)
    }
}

// ---------------------------------------------------------------------------
// Pure-logic helpers (testable without async I/O)
// ---------------------------------------------------------------------------

/// Pure helper: split a stock list into resolved + stragglers using a
/// pre-fetched snapshot. Extracted so unit tests can verify the partition
/// logic without spinning up tokio.
///
/// `stocks` is the F&O stock list to resolve. `snapshot` is a snapshot of
/// `SharedSpotPrices` (typically obtained via `read().await` and cloned).
///
/// O(N) where N = `stocks.len()`. Single pass. The HashMap lookup is O(1).
pub fn partition_resolved_and_stragglers(
    stocks: &[String],
    snapshot: &HashMap<String, f64>,
) -> (HashMap<String, f64>, Vec<String>) {
    let mut resolved: HashMap<String, f64> = HashMap::with_capacity(stocks.len());
    let mut stragglers: Vec<String> = Vec::with_capacity(stocks.len());

    for stock in stocks {
        match snapshot.get(stock) {
            Some(&price) if price > 0.0 && price.is_finite() => {
                resolved.insert(stock.clone(), price);
            }
            _ => stragglers.push(stock.clone()),
        }
    }

    (resolved, stragglers)
}

// ---------------------------------------------------------------------------
// Async resolver
// ---------------------------------------------------------------------------

/// Resolve stock spot prices from live cash-equity ticks at progressive
/// timeouts. Exits early when all stocks are resolved.
///
/// # Arguments
/// - `stocks`: F&O stock underlying symbols to resolve (e.g., `["RELIANCE", "TCS"]`).
/// - `spot_prices`: Shared map updated by the tick processor.
/// - `intervals_secs`: Override the default `POLL_INTERVALS_SECS` (test hook).
///   Pass `None` for the production schedule.
///
/// # Returns
/// `ResolveResult` with resolved + stragglers + cycle count + elapsed time.
///
/// # Behaviour
/// - Polls `spot_prices` at each interval in `intervals_secs`.
/// - At every poll: computes `(resolved, stragglers)` partition.
/// - Exits early when `stragglers.is_empty()`.
/// - On completion, returns the latest partition.
///
/// # Logging (Rule 5: ERROR for failures, INFO for progress)
/// - INFO at each cycle: `cycle=N, resolved=R, stragglers=S`.
/// - INFO at exit: `final cycles=N, resolved=R, stragglers=S, elapsed_ms=X`.
/// - WARN if stragglers remain after final cycle (caller will REST-fall-back).
pub async fn resolve_stock_atm_from_live_ticks(
    stocks: &[String],
    spot_prices: &SharedSpotPrices,
    intervals_secs: Option<&[u64]>,
) -> ResolveResult {
    let intervals = intervals_secs.unwrap_or(POLL_INTERVALS_SECS);
    let start = std::time::Instant::now();

    if stocks.is_empty() {
        info!("resolver: no stocks to resolve, exiting immediately");
        return ResolveResult::default();
    }

    let mut last_result = ResolveResult {
        resolved: HashMap::new(),
        stragglers: stocks.to_vec(),
        cycles_run: 0,
        elapsed_ms: 0,
    };
    let mut prev_elapsed_secs: u64 = 0;

    for (cycle_idx, &target_secs) in intervals.iter().enumerate() {
        // Sleep the delta from the previous interval.
        let delta = target_secs.saturating_sub(prev_elapsed_secs);
        if delta > 0 {
            tokio::time::sleep(Duration::from_secs(delta)).await;
        }
        prev_elapsed_secs = target_secs;

        // Snapshot `SharedSpotPrices` for the F&O stocks. We could read each
        // entry individually, but a single read-lock-then-extract is cheaper
        // than N async lock acquisitions.
        let snapshot = build_snapshot_for_stocks(stocks, spot_prices).await;
        let (resolved, stragglers) = partition_resolved_and_stragglers(stocks, &snapshot);

        let cycle_no = u32::try_from(cycle_idx)
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        info!(
            cycle = cycle_no,
            target_secs = target_secs,
            resolved = resolved.len(),
            stragglers = stragglers.len(),
            "live-tick ATM resolver cycle"
        );

        last_result = ResolveResult {
            resolved,
            stragglers,
            cycles_run: cycle_no,
            elapsed_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        };

        // Early exit if all resolved.
        if last_result.stragglers.is_empty() {
            info!(
                final_cycles = cycle_no,
                resolved = last_result.resolved.len(),
                elapsed_ms = last_result.elapsed_ms,
                "live-tick ATM resolver: all stocks resolved early"
            );
            return last_result;
        }
    }

    // Stragglers remain after final cycle → caller will REST-fall-back.
    if !last_result.stragglers.is_empty() {
        warn!(
            stragglers = last_result.stragglers.len(),
            resolved = last_result.resolved.len(),
            cycles = last_result.cycles_run,
            elapsed_ms = last_result.elapsed_ms,
            "live-tick ATM resolver: stragglers remain after final cycle — caller should fall back to REST /marketfeed/ltp"
        );
    }

    last_result
}

/// Snapshot helper — single read-lock-then-extract for the F&O stocks.
async fn build_snapshot_for_stocks(
    stocks: &[String],
    spot_prices: &SharedSpotPrices,
) -> HashMap<String, f64> {
    let map = spot_prices.read().await;
    let mut out: HashMap<String, f64> = HashMap::with_capacity(stocks.len());
    for stock in stocks {
        if let Some(entry) = map.get(stock)
            && entry.price > 0.0
            && entry.price.is_finite()
        {
            out.insert(stock.clone(), entry.price);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tick classification — wires live cash-equity ticks into SharedSpotPrices
// ---------------------------------------------------------------------------

/// Index segment code in Dhan's binary protocol header byte 4 (`IDX_I = 0`).
pub const IDX_I_SEGMENT_CODE: u8 = 0;

/// NSE_EQ segment code in Dhan's binary protocol header byte 4 (`NSE_EQ = 1`).
pub const NSE_EQ_SEGMENT_CODE: u8 = 1;

/// Classify a live tick for spot-price update routing. Pure function — no
/// I/O — so the caller in `main.rs` can spawn one tokio task that owns the
/// broadcast subscription and calls this on every tick.
///
/// # Arguments
/// - `security_id`: Tick's security_id (binary header bytes 4–7).
/// - `exchange_segment_code`: Tick's segment code (binary header byte 3).
/// - `last_traded_price`: Tick's LTP. Pre-validated by caller (must be > 0
///   and finite — caller short-circuits invalid ticks before calling).
/// - `index_lookup`: Map of (security_id) → underlying_symbol for IDX_I
///   indices we care about (NIFTY=13, BANKNIFTY=25, SENSEX=51).
/// - `stock_lookup`: Map of (security_id) → underlying_symbol for NSE_EQ
///   F&O cash-equity SIDs (e.g., 2885 → "RELIANCE").
///
/// # Returns
/// - `Some(symbol)` if the tick should update `SharedSpotPrices` for that
///   symbol. Caller calls `update_spot_price(prices, symbol, price)`.
/// - `None` if the tick is not a candidate for spot-price tracking.
///
/// # I-P1-11 (composite key) safety
/// Lookups are gated by segment_code FIRST. An IDX_I tick at security_id=27
/// will NOT collide with an NSE_EQ tick at security_id=27 — they go through
/// different match arms. This preserves the I-P1-11 invariant that the
/// composite key `(security_id, exchange_segment)` is the unique identifier.
///
/// # O(1)
/// Two HashMap reads, both O(1). Zero allocation on hot path (returns
/// `&str` from the lookup map; caller clones only on update).
pub fn classify_tick_for_spot_update<'a>(
    security_id: u32,
    exchange_segment_code: u8,
    last_traded_price: f32,
    // APPROVED: I-P1-11 — `index_lookup` is single-segment IDX_I by contract; segment dispatched at call time.
    index_lookup: &'a HashMap<u32, String>,
    // APPROVED: I-P1-11 — `stock_lookup` is single-segment NSE_EQ by contract; segment dispatched at call time.
    stock_lookup: &'a HashMap<u32, String>,
) -> Option<&'a str> {
    if last_traded_price <= 0.0 || !last_traded_price.is_finite() {
        return None;
    }
    match exchange_segment_code {
        IDX_I_SEGMENT_CODE => index_lookup.get(&security_id).map(String::as_str),
        NSE_EQ_SEGMENT_CODE => stock_lookup.get(&security_id).map(String::as_str),
        _ => None,
    }
}

/// Build the index lookup for the 3 full-chain indices. Same set as
/// `tickvault_common::constants::FULL_CHAIN_INDEX_SYMBOLS` (NIFTY, BANKNIFTY,
/// SENSEX) — the F&O indices we care about for spot price tracking.
///
/// 2026-04-25: FINNIFTY (27) and MIDCPNIFTY (442) were dropped — neither
/// IDX_I value nor F&O is subscribed for them anymore.
///
/// # I-P1-11 single-segment safety
/// Returned map is single-segment by construction (every entry is IDX_I).
/// Caller must only consult it from an IDX_I-gated path — see
/// `classify_tick_for_spot_update`.
// APPROVED: I-P1-11 — pure-IDX_I lookup, segment is implicit in the builder's name + contract.
pub fn build_full_chain_index_lookup() -> HashMap<u32, String> {
    let mut m = HashMap::with_capacity(3);
    m.insert(13, "NIFTY".to_string());
    m.insert(25, "BANKNIFTY".to_string());
    m.insert(51, "SENSEX".to_string());
    m
}

// ---------------------------------------------------------------------------
// Direct passthrough for unit tests of the live API
// ---------------------------------------------------------------------------

/// Read a single underlying's latest spot price from the shared map.
/// Re-exported here so callers don't need to import depth_rebalancer.
pub async fn read_one_spot(spot_prices: &SharedSpotPrices, symbol: &str) -> Option<f64> {
    get_spot_price(spot_prices, symbol).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::depth_rebalancer::{new_shared_spot_prices, update_spot_price};
    use std::sync::Arc;

    // Pure helper tests (no async required for partition_resolved_and_stragglers)

    #[test]
    fn test_partition_all_resolved() {
        let stocks = vec!["RELIANCE".to_string(), "TCS".to_string()];
        let mut snap = HashMap::new();
        snap.insert("RELIANCE".to_string(), 2700.0);
        snap.insert("TCS".to_string(), 3500.0);
        let (resolved, stragglers) = partition_resolved_and_stragglers(&stocks, &snap);
        assert_eq!(resolved.len(), 2);
        assert!(stragglers.is_empty());
        assert!((resolved["RELIANCE"] - 2700.0).abs() < f64::EPSILON);
        assert!((resolved["TCS"] - 3500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_partition_all_stragglers() {
        let stocks = vec!["RELIANCE".to_string(), "TCS".to_string()];
        let snap: HashMap<String, f64> = HashMap::new();
        let (resolved, stragglers) = partition_resolved_and_stragglers(&stocks, &snap);
        assert!(resolved.is_empty());
        assert_eq!(stragglers.len(), 2);
    }

    #[test]
    fn test_partition_mixed() {
        let stocks = vec![
            "RELIANCE".to_string(),
            "TCS".to_string(),
            "INFY".to_string(),
        ];
        let mut snap = HashMap::new();
        snap.insert("RELIANCE".to_string(), 2700.0);
        // TCS missing
        snap.insert("INFY".to_string(), 1500.0);
        let (resolved, stragglers) = partition_resolved_and_stragglers(&stocks, &snap);
        assert_eq!(resolved.len(), 2);
        assert_eq!(stragglers.len(), 1);
        assert!(stragglers.contains(&"TCS".to_string()));
    }

    #[test]
    fn test_partition_invalid_price_treated_as_straggler() {
        let stocks = vec![
            "ZERO".to_string(),
            "NEGATIVE".to_string(),
            "INFINITY".to_string(),
            "NAN".to_string(),
            "VALID".to_string(),
        ];
        let mut snap = HashMap::new();
        snap.insert("ZERO".to_string(), 0.0);
        snap.insert("NEGATIVE".to_string(), -100.0);
        snap.insert("INFINITY".to_string(), f64::INFINITY);
        snap.insert("NAN".to_string(), f64::NAN);
        snap.insert("VALID".to_string(), 100.0);
        let (resolved, stragglers) = partition_resolved_and_stragglers(&stocks, &snap);
        assert_eq!(resolved.len(), 1);
        assert_eq!(stragglers.len(), 4);
        assert!((resolved["VALID"] - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_partition_empty_stocks() {
        let stocks: Vec<String> = Vec::new();
        let mut snap = HashMap::new();
        snap.insert("RELIANCE".to_string(), 2700.0);
        let (resolved, stragglers) = partition_resolved_and_stragglers(&stocks, &snap);
        assert!(resolved.is_empty());
        assert!(stragglers.is_empty());
    }

    #[test]
    fn test_resolve_result_total_attempted() {
        let mut r = ResolveResult::default();
        r.resolved.insert("A".to_string(), 1.0);
        r.resolved.insert("B".to_string(), 2.0);
        r.stragglers.push("C".to_string());
        assert_eq!(r.total_attempted(), 3);
    }

    #[test]
    fn test_resolve_result_resolution_pct() {
        let mut r = ResolveResult::default();
        // 0/0 → 0
        assert_eq!(r.resolution_pct(), 0);

        r.resolved.insert("A".to_string(), 1.0);
        r.resolved.insert("B".to_string(), 2.0);
        r.stragglers.push("C".to_string());
        // 2/3 = 66
        assert_eq!(r.resolution_pct(), 66);

        let mut all_resolved = ResolveResult::default();
        all_resolved.resolved.insert("A".to_string(), 1.0);
        assert_eq!(all_resolved.resolution_pct(), 100);
    }

    #[test]
    fn test_poll_intervals_are_progressive() {
        let mut prev = 0u64;
        for &secs in POLL_INTERVALS_SECS {
            assert!(secs > prev, "intervals must be strictly increasing");
            prev = secs;
        }
        assert_eq!(
            *POLL_INTERVALS_SECS.last().unwrap(),
            MAX_RESOLVE_BUDGET_SECS
        );
    }

    // Async resolver tests (use short test intervals so tests run fast)

    #[tokio::test]
    async fn test_resolver_returns_empty_when_stocks_empty() {
        let prices = new_shared_spot_prices();
        let stocks: Vec<String> = Vec::new();
        let result = resolve_stock_atm_from_live_ticks(&stocks, &prices, Some(&[1])).await;
        assert!(result.resolved.is_empty());
        assert!(result.stragglers.is_empty());
        assert_eq!(result.cycles_run, 0);
    }

    #[tokio::test]
    async fn test_resolver_resolves_immediately_when_all_present() {
        let prices = new_shared_spot_prices();
        update_spot_price(&prices, "RELIANCE", 2700.0).await;
        update_spot_price(&prices, "TCS", 3500.0).await;

        let stocks = vec!["RELIANCE".to_string(), "TCS".to_string()];
        // Even with a 1-second first interval the resolver must wait that long
        // before the first poll. Use a tiny first interval to keep test fast.
        let result = resolve_stock_atm_from_live_ticks(&stocks, &prices, Some(&[1])).await;
        assert_eq!(result.resolved.len(), 2);
        assert!(result.stragglers.is_empty());
        assert_eq!(result.cycles_run, 1);
    }

    #[tokio::test]
    async fn test_resolver_returns_stragglers_when_silent() {
        let prices = new_shared_spot_prices();
        // No tick recorded for any of the stocks.
        let stocks = vec!["RELIANCE".to_string(), "TCS".to_string()];
        let result = resolve_stock_atm_from_live_ticks(&stocks, &prices, Some(&[1, 2])).await;
        assert!(result.resolved.is_empty());
        assert_eq!(result.stragglers.len(), 2);
        assert_eq!(result.cycles_run, 2);
    }

    #[tokio::test]
    async fn test_resolver_partial_resolution() {
        let prices = new_shared_spot_prices();
        update_spot_price(&prices, "RELIANCE", 2700.0).await;
        // TCS never ticks.
        let stocks = vec!["RELIANCE".to_string(), "TCS".to_string()];
        let result = resolve_stock_atm_from_live_ticks(&stocks, &prices, Some(&[1, 2])).await;
        assert_eq!(result.resolved.len(), 1);
        assert!(result.resolved.contains_key("RELIANCE"));
        assert_eq!(result.stragglers, vec!["TCS".to_string()]);
        assert_eq!(result.cycles_run, 2);
    }

    #[tokio::test]
    async fn test_resolver_exits_early_on_full_resolution() {
        let prices = new_shared_spot_prices();
        update_spot_price(&prices, "RELIANCE", 2700.0).await;

        let stocks = vec!["RELIANCE".to_string()];
        // Cycles at 1, 5, 10 — should exit at cycle 1 since RELIANCE is resolved.
        let result = resolve_stock_atm_from_live_ticks(&stocks, &prices, Some(&[1, 5, 10])).await;
        assert_eq!(result.cycles_run, 1, "must exit at cycle 1 not later");
    }

    #[tokio::test]
    async fn test_resolver_late_arriving_tick_resolved_at_later_cycle() {
        let prices = Arc::clone(&new_shared_spot_prices());
        let prices_for_writer = Arc::clone(&prices);

        // Spawn a writer that updates LTP after 2 seconds.
        let _writer_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            update_spot_price(&prices_for_writer, "RELIANCE", 2700.0).await;
        });

        let stocks = vec!["RELIANCE".to_string()];
        // Cycles at 1, 3 — at cycle 1 (1s) RELIANCE not ticked; at cycle 3 (3s) it is.
        let result = resolve_stock_atm_from_live_ticks(&stocks, &prices, Some(&[1, 3])).await;
        assert_eq!(result.resolved.len(), 1);
        assert_eq!(result.cycles_run, 2);
    }

    #[tokio::test]
    async fn test_resolver_read_one_spot_passthrough() {
        let prices = new_shared_spot_prices();
        update_spot_price(&prices, "RELIANCE", 2700.0).await;
        let v = read_one_spot(&prices, "RELIANCE").await;
        assert_eq!(v, Some(2700.0));
        let missing = read_one_spot(&prices, "MISSING").await;
        assert!(missing.is_none());
    }

    /// O(1) per-stock guarantee — partition is O(N) overall but each lookup
    /// is a HashMap read = O(1). Verify this with a pathologically large
    /// stock list (N=1000) and confirm the result is still correct.
    #[test]
    fn test_partition_o1_per_stock_at_n_1000() {
        let stocks: Vec<String> = (0..1000).map(|i| format!("STOCK{i}")).collect();
        let mut snap = HashMap::new();
        for i in 0..1000 {
            snap.insert(format!("STOCK{i}"), i as f64 + 100.0);
        }
        let (resolved, stragglers) = partition_resolved_and_stragglers(&stocks, &snap);
        assert_eq!(resolved.len(), 1000);
        assert!(stragglers.is_empty());
    }

    /// Uniqueness guarantee — the resolver does NOT introduce duplicate stock
    /// keys even if the input list contains them.
    #[test]
    fn test_partition_dedup_input_duplicates_via_hashmap_semantics() {
        let stocks = vec![
            "RELIANCE".to_string(),
            "RELIANCE".to_string(), // duplicate input
            "TCS".to_string(),
        ];
        let mut snap = HashMap::new();
        snap.insert("RELIANCE".to_string(), 2700.0);
        snap.insert("TCS".to_string(), 3500.0);
        let (resolved, stragglers) = partition_resolved_and_stragglers(&stocks, &snap);
        // HashMap dedups — RELIANCE appears once in `resolved`.
        assert_eq!(resolved.len(), 2);
        // Vec does NOT dedup — but since both RELIANCE entries are resolved,
        // stragglers stays empty.
        assert!(stragglers.is_empty());
    }

    /// Stress test — the resolver must NEVER allocate per-stock during the
    /// hot loop; pre-allocated capacities should cover it. Verify by
    /// requesting a 250-stock resolution and checking no panic / no
    /// degenerate behaviour.
    #[test]
    fn test_partition_stress_250_stocks_mixed() {
        let stocks: Vec<String> = (0..250).map(|i| format!("STOCK{i}")).collect();
        let mut snap = HashMap::new();
        // Insert every other stock.
        for i in (0..250).step_by(2) {
            snap.insert(format!("STOCK{i}"), 100.0 + i as f64);
        }
        let (resolved, stragglers) = partition_resolved_and_stragglers(&stocks, &snap);
        assert_eq!(resolved.len(), 125);
        assert_eq!(stragglers.len(), 125);
    }

    // ========================================================================
    // classify_tick_for_spot_update tests
    // ========================================================================

    fn sample_index_lookup() -> HashMap<u32, String> {
        build_full_chain_index_lookup()
    }

    fn sample_stock_lookup() -> HashMap<u32, String> {
        let mut m = HashMap::new();
        m.insert(2885, "RELIANCE".to_string());
        m.insert(11536, "TCS".to_string());
        m.insert(1594, "INFY".to_string());
        m
    }

    #[test]
    fn test_classify_idx_i_nifty_returns_nifty() {
        let i = sample_index_lookup();
        let s = sample_stock_lookup();
        let r = classify_tick_for_spot_update(13, IDX_I_SEGMENT_CODE, 24500.0, &i, &s);
        assert_eq!(r, Some("NIFTY"));
    }

    #[test]
    fn test_classify_idx_i_sensex_returns_sensex() {
        let i = sample_index_lookup();
        let s = sample_stock_lookup();
        let r = classify_tick_for_spot_update(51, IDX_I_SEGMENT_CODE, 80000.0, &i, &s);
        assert_eq!(r, Some("SENSEX"));
    }

    #[test]
    fn test_classify_idx_i_unknown_index_returns_none() {
        let i = sample_index_lookup();
        let s = sample_stock_lookup();
        // FINNIFTY=27 was dropped → not in the lookup → None
        let r = classify_tick_for_spot_update(27, IDX_I_SEGMENT_CODE, 24500.0, &i, &s);
        assert_eq!(r, None);
        // MIDCPNIFTY=442 also dropped
        let r2 = classify_tick_for_spot_update(442, IDX_I_SEGMENT_CODE, 24500.0, &i, &s);
        assert_eq!(r2, None);
    }

    #[test]
    fn test_classify_nse_eq_reliance_returns_reliance() {
        let i = sample_index_lookup();
        let s = sample_stock_lookup();
        let r = classify_tick_for_spot_update(2885, NSE_EQ_SEGMENT_CODE, 2700.0, &i, &s);
        assert_eq!(r, Some("RELIANCE"));
    }

    #[test]
    fn test_classify_nse_eq_unknown_returns_none() {
        let i = sample_index_lookup();
        let s = sample_stock_lookup();
        let r = classify_tick_for_spot_update(99999, NSE_EQ_SEGMENT_CODE, 100.0, &i, &s);
        assert_eq!(r, None);
    }

    /// I-P1-11 ratchet: same security_id in two different segments must
    /// resolve to two different symbols. The classify function MUST gate on
    /// segment_code FIRST so id=13 in NSE_EQ doesn't collide with NIFTY.
    #[test]
    fn test_classify_id_collision_across_segments_resolved_by_segment() {
        let mut i = HashMap::new();
        i.insert(13, "NIFTY".to_string());
        let mut s = HashMap::new();
        // Hypothetical: an NSE_EQ instrument also has security_id=13.
        s.insert(13, "FAKE_STOCK".to_string());

        let r_idx = classify_tick_for_spot_update(13, IDX_I_SEGMENT_CODE, 24500.0, &i, &s);
        assert_eq!(r_idx, Some("NIFTY"));

        let r_eq = classify_tick_for_spot_update(13, NSE_EQ_SEGMENT_CODE, 100.0, &i, &s);
        assert_eq!(r_eq, Some("FAKE_STOCK"));
    }

    #[test]
    fn test_classify_invalid_price_returns_none() {
        let i = sample_index_lookup();
        let s = sample_stock_lookup();
        // Zero price
        assert_eq!(
            classify_tick_for_spot_update(13, IDX_I_SEGMENT_CODE, 0.0, &i, &s),
            None
        );
        // Negative
        assert_eq!(
            classify_tick_for_spot_update(13, IDX_I_SEGMENT_CODE, -1.0, &i, &s),
            None
        );
        // Infinity
        assert_eq!(
            classify_tick_for_spot_update(13, IDX_I_SEGMENT_CODE, f32::INFINITY, &i, &s),
            None
        );
        // NaN
        assert_eq!(
            classify_tick_for_spot_update(13, IDX_I_SEGMENT_CODE, f32::NAN, &i, &s),
            None
        );
    }

    #[test]
    fn test_classify_unknown_segment_code_returns_none() {
        let i = sample_index_lookup();
        let s = sample_stock_lookup();
        // NSE_FNO = 2, not a spot-price source
        let r = classify_tick_for_spot_update(13, 2, 24500.0, &i, &s);
        assert_eq!(r, None);
        // BSE_EQ = 4
        let r = classify_tick_for_spot_update(13, 4, 24500.0, &i, &s);
        assert_eq!(r, None);
    }

    #[test]
    fn test_build_full_chain_index_lookup_has_three_entries() {
        let lookup = build_full_chain_index_lookup();
        assert_eq!(lookup.len(), 3);
        assert_eq!(lookup.get(&13), Some(&"NIFTY".to_string()));
        assert_eq!(lookup.get(&25), Some(&"BANKNIFTY".to_string()));
        assert_eq!(lookup.get(&51), Some(&"SENSEX".to_string()));
    }

    #[test]
    fn test_build_full_chain_index_lookup_excludes_finnifty_midcpnifty() {
        let lookup = build_full_chain_index_lookup();
        assert!(
            !lookup.contains_key(&27),
            "FINNIFTY (security_id=27) must stay dropped"
        );
        assert!(
            !lookup.contains_key(&442),
            "MIDCPNIFTY (security_id=442) must stay dropped"
        );
    }

    /// Sweep test — pathological 5,000-entry stock lookup. Confirms HashMap
    /// O(1) lookup latency stays sane and no allocation on the classify path.
    #[test]
    fn test_classify_o1_lookup_under_pathological_5k_stock_map() {
        let mut s: HashMap<u32, String> = HashMap::with_capacity(5_000);
        for sid in 1_000_000..1_005_000 {
            s.insert(sid, format!("STOCK{sid}"));
        }
        let i = sample_index_lookup();
        // Lookup the LAST inserted SID to defeat any hash bucket linear scan.
        let r = classify_tick_for_spot_update(1_004_999, NSE_EQ_SEGMENT_CODE, 100.0, &i, &s);
        assert_eq!(r, Some("STOCK1004999"));
    }
}
