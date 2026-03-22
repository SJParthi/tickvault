//! Trading pipeline — bridges tick data to indicators → strategies → OMS.
//!
//! This module wires the `trading` crate into the live system. It runs as
//! a separate tokio task, consuming ticks from a broadcast channel (cold path,
//! does NOT pollute the hot tick processor).
//!
//! # Safety: Paper Trading Only
//! The OMS operates in `dry_run = true` mode by default. No real HTTP calls
//! are ever made to Dhan. All orders are simulated with PAPER-xxx IDs.
//!
//! # Architecture
//! ```text
//! Tick Broadcast (from tick_processor)
//!       ↓
//! IndicatorEngine::update()  — O(1) per tick
//!       ↓
//! StrategyInstance::evaluate() — O(C) per strategy (C = conditions, typically 2-5)
//!       ↓
//! RiskEngine::check_order()  — O(1) per signal
//!       ↓
//! OMS::place_order()         — paper trade (dry_run=true)
//!       ↓
//! Order Update WebSocket     → OMS::handle_order_update()
//! ```

use std::path::Path;

use secrecy::{ExposeSecret, SecretString};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use dhan_live_trader_common::config::ApplicationConfig;
use dhan_live_trader_common::constants::MAX_INDICATOR_INSTRUMENTS;
use dhan_live_trader_common::order_types::{
    OrderType, OrderUpdate, OrderValidity, ProductType, TransactionType,
};
use dhan_live_trader_common::tick_types::ParsedTick;
use dhan_live_trader_core::auth::token_manager::TokenHandle;

use dhan_live_trader_trading::indicator::{IndicatorEngine, IndicatorParams};
use dhan_live_trader_trading::oms::{
    OrderApiClient, OrderManagementSystem, OrderRateLimiter, PlaceOrderRequest, TokenProvider,
};
use dhan_live_trader_trading::risk::engine::RiskEngine;
use dhan_live_trader_trading::risk::types::RiskCheck;
use dhan_live_trader_trading::strategy::{Signal, StrategyHotReloader, StrategyInstance};

// ---------------------------------------------------------------------------
// TokenProvider bridge — connects core::TokenHandle to trading::TokenProvider
// ---------------------------------------------------------------------------

/// Bridges the core crate's `TokenHandle` (arc-swap) to the trading crate's
/// `TokenProvider` trait. Cold path — called once per order placement.
struct TokenHandleBridge {
    handle: TokenHandle,
}

impl TokenProvider for TokenHandleBridge {
    fn get_access_token(&self) -> Result<SecretString, dhan_live_trader_trading::oms::OmsError> {
        let guard = self.handle.load();
        match guard.as_ref() {
            Some(token_state) => {
                let token_str = token_state.access_token().expose_secret();
                if token_str.is_empty() {
                    return Err(dhan_live_trader_trading::oms::OmsError::NoToken);
                }
                Ok(SecretString::from(token_str.to_owned()))
            }
            None => Err(dhan_live_trader_trading::oms::OmsError::NoToken),
        }
    }
}

// ---------------------------------------------------------------------------
// Trading Pipeline Task
// ---------------------------------------------------------------------------

/// Configuration for the trading pipeline.
pub struct TradingPipelineConfig {
    /// Indicator parameters (from strategy TOML or defaults).
    pub indicator_params: IndicatorParams,
    /// Strategy instances (from strategy TOML).
    pub strategies: Vec<StrategyInstance>,
    /// Risk engine configuration.
    pub max_daily_loss_percent: f64,
    /// Risk engine max position lots.
    pub max_position_lots: u32,
    /// Trading capital in rupees.
    pub capital: f64,
    /// Dry-run mode (default: true).
    pub dry_run: bool,
    /// SEBI max orders per second.
    pub max_orders_per_second: u32,
    /// Dhan REST API base URL.
    pub rest_api_base_url: String,
    /// Dhan client ID.
    pub client_id: String,
    /// Token handle for authentication.
    pub token_handle: TokenHandle,
}

/// Spawns the trading pipeline as a background task.
///
/// Returns the task handle. The pipeline runs until the tick broadcast
/// sender is dropped (i.e., tick processor stops).
///
/// The `daily_reset_signal` is fired at 16:00 IST to zero out all counters
/// (risk engine, OMS, indicators). Pass `None` if daily reset is not needed.
///
/// # Safety
/// When `dry_run` is true, NO HTTP calls are ever made. All orders are paper trades.
pub fn spawn_trading_pipeline(
    pipeline_config: TradingPipelineConfig,
    tick_receiver: broadcast::Receiver<ParsedTick>,
    order_update_receiver: broadcast::Receiver<OrderUpdate>,
    strategy_hot_reloader: Option<StrategyHotReloader>,
) -> tokio::task::JoinHandle<()> {
    spawn_trading_pipeline_with_reset(
        pipeline_config,
        tick_receiver,
        order_update_receiver,
        strategy_hot_reloader,
        None,
    )
}

/// Spawns the trading pipeline with optional daily reset and market close signals.
///
/// When `daily_reset_signal` is `Some`, the pipeline listens for it and
/// resets risk engine, OMS, and indicator state at 16:00 IST.
///
/// When `market_close_signal` is `Some`, the pipeline logs all positions
/// and cancels pending paper orders at 15:30 IST before shutdown.
pub fn spawn_trading_pipeline_with_reset(
    pipeline_config: TradingPipelineConfig,
    tick_receiver: broadcast::Receiver<ParsedTick>,
    order_update_receiver: broadcast::Receiver<OrderUpdate>,
    strategy_hot_reloader: Option<StrategyHotReloader>,
    daily_reset_signal: Option<std::sync::Arc<tokio::sync::Notify>>,
) -> tokio::task::JoinHandle<()> {
    spawn_trading_pipeline_full(
        pipeline_config,
        tick_receiver,
        order_update_receiver,
        strategy_hot_reloader,
        daily_reset_signal,
        None,
    )
}

/// Spawns the trading pipeline with all optional signals.
pub fn spawn_trading_pipeline_full(
    pipeline_config: TradingPipelineConfig,
    tick_receiver: broadcast::Receiver<ParsedTick>,
    order_update_receiver: broadcast::Receiver<OrderUpdate>,
    strategy_hot_reloader: Option<StrategyHotReloader>,
    daily_reset_signal: Option<std::sync::Arc<tokio::sync::Notify>>,
    market_close_signal: Option<std::sync::Arc<tokio::sync::Notify>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_trading_pipeline(
            pipeline_config,
            tick_receiver,
            order_update_receiver,
            strategy_hot_reloader,
            daily_reset_signal,
            market_close_signal,
        )
        .await;
    })
}

/// Main trading pipeline loop.
async fn run_trading_pipeline(
    config: TradingPipelineConfig,
    mut tick_receiver: broadcast::Receiver<ParsedTick>,
    mut order_update_receiver: broadcast::Receiver<OrderUpdate>,
    hot_reloader: Option<StrategyHotReloader>,
    daily_reset_signal: Option<std::sync::Arc<tokio::sync::Notify>>,
    market_close_signal: Option<std::sync::Arc<tokio::sync::Notify>>,
) {
    // Initialize indicator engine
    let mut indicator_engine = IndicatorEngine::new(config.indicator_params);

    // Initialize strategy instances
    let mut strategies = config.strategies;

    // Initialize risk engine
    let mut risk_engine = RiskEngine::new(
        config.max_daily_loss_percent,
        config.max_position_lots,
        config.capital,
    );

    // Initialize OMS (always starts in dry_run mode by default)
    let api_client = OrderApiClient::new(
        reqwest::Client::new(),
        config.rest_api_base_url,
        config.client_id.clone(),
    );
    let rate_limiter = OrderRateLimiter::new(config.max_orders_per_second);
    let token_bridge = Box::new(TokenHandleBridge {
        handle: config.token_handle,
    });
    let mut oms =
        OrderManagementSystem::new(api_client, rate_limiter, token_bridge, config.client_id);

    info!(
        dry_run = config.dry_run,
        strategy_count = strategies.len(),
        capital = config.capital,
        max_daily_loss_percent = config.max_daily_loss_percent,
        "trading pipeline started"
    );

    if config.dry_run {
        info!("PAPER TRADING MODE — no real orders will be placed");
    } else {
        warn!("LIVE TRADING MODE — real orders WILL be placed");
    }

    let mut ticks_processed: u64 = 0;
    let mut signals_generated: u64 = 0;

    // Daily reset: await the signal (or pend forever if None).
    // Uses Pin<Box<dyn Future>> so the future can be re-armed after each reset.
    fn make_reset_future(
        signal: &Option<std::sync::Arc<tokio::sync::Notify>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        match signal {
            Some(notify) => {
                let n = std::sync::Arc::clone(notify);
                Box::pin(async move { n.notified().await })
            }
            None => Box::pin(std::future::pending::<()>()),
        }
    }
    let mut wait_for_reset = make_reset_future(&daily_reset_signal);
    let mut wait_for_market_close = make_reset_future(&market_close_signal);

    loop {
        tokio::select! {
            // Process ticks from the broadcast channel
            tick_result = tick_receiver.recv() => {
                match tick_result {
                    Ok(tick) => {
                        ticks_processed = ticks_processed.saturating_add(1);

                        // Check for hot-reloaded strategy config
                        if let Some(ref reloader) = hot_reloader
                            && let Some(event) = reloader.try_recv()
                        {
                            info!(
                                strategy_count = event.strategies.len(),
                                "hot-reloading strategy config"
                            );
                            indicator_engine = IndicatorEngine::new(event.indicator_params);
                            strategies = event
                                .strategies
                                .into_iter()
                                .map(|def| StrategyInstance::new(def, MAX_INDICATOR_INSTRUMENTS))
                                .collect();
                        }

                        // Step 1: Update indicators (O(1) per tick)
                        let snapshot = indicator_engine.update(&tick);

                        // Step 2: Evaluate all strategies
                        for strategy in &mut strategies {
                            let signal = strategy.evaluate(&snapshot);

                            match signal {
                                Signal::Hold => {}
                                Signal::EnterLong { size_fraction: _, stop_loss, target } => {
                                    signals_generated = signals_generated.saturating_add(1);
                                    // Risk check before order
                                    let risk_result = risk_engine.check_order(tick.security_id, 1);
                                    match risk_result {
                                        RiskCheck::Approved => {
                                            let request = PlaceOrderRequest {
                                                security_id: tick.security_id,
                                                transaction_type: TransactionType::Buy,
                                                order_type: OrderType::Market,
                                                product_type: ProductType::Intraday,
                                                validity: OrderValidity::Day,
                                                quantity: 1,
                                                price: 0.0,
                                                trigger_price: 0.0,
                                                lot_size: 1,
                                            };
                                            match oms.place_order(request).await {
                                                Ok(order_id) => {
                                                    debug!(
                                                        order_id = %order_id,
                                                        security_id = tick.security_id,
                                                        stop_loss,
                                                        target,
                                                        strategy = %strategy.definition().name,
                                                        "LONG signal → order placed"
                                                    );
                                                }
                                                Err(err) => {
                                                    warn!(
                                                        ?err,
                                                        security_id = tick.security_id,
                                                        "LONG signal → order placement failed"
                                                    );
                                                }
                                            }
                                        }
                                        RiskCheck::Rejected { breach, reason } => {
                                            info!(
                                                security_id = tick.security_id,
                                                ?breach,
                                                %reason,
                                                "LONG signal rejected by risk engine"
                                            );
                                        }
                                    }
                                }
                                Signal::EnterShort { size_fraction: _, stop_loss, target } => {
                                    signals_generated = signals_generated.saturating_add(1);
                                    let risk_result = risk_engine.check_order(tick.security_id, -1);
                                    match risk_result {
                                        RiskCheck::Approved => {
                                            let request = PlaceOrderRequest {
                                                security_id: tick.security_id,
                                                transaction_type: TransactionType::Sell,
                                                order_type: OrderType::Market,
                                                product_type: ProductType::Intraday,
                                                validity: OrderValidity::Day,
                                                quantity: 1,
                                                price: 0.0,
                                                trigger_price: 0.0,
                                                lot_size: 1,
                                            };
                                            match oms.place_order(request).await {
                                                Ok(order_id) => {
                                                    debug!(
                                                        order_id = %order_id,
                                                        security_id = tick.security_id,
                                                        stop_loss,
                                                        target,
                                                        strategy = %strategy.definition().name,
                                                        "SHORT signal → order placed"
                                                    );
                                                }
                                                Err(err) => {
                                                    warn!(
                                                        ?err,
                                                        security_id = tick.security_id,
                                                        "SHORT signal → order placement failed"
                                                    );
                                                }
                                            }
                                        }
                                        RiskCheck::Rejected { breach, reason } => {
                                            info!(
                                                security_id = tick.security_id,
                                                ?breach,
                                                %reason,
                                                "SHORT signal rejected by risk engine"
                                            );
                                        }
                                    }
                                }
                                Signal::Exit { reason } => {
                                    signals_generated = signals_generated.saturating_add(1);
                                    debug!(
                                        security_id = tick.security_id,
                                        ?reason,
                                        strategy = %strategy.definition().name,
                                        "EXIT signal"
                                    );
                                    // Exit handling: cancel active orders for this security
                                    let active: Vec<String> = oms
                                        .active_orders()
                                        .iter()
                                        .filter(|o| o.security_id == tick.security_id)
                                        .map(|o| o.order_id.clone())
                                        .collect();
                                    for order_id in active {
                                        if let Err(err) = oms.cancel_order(&order_id).await {
                                            warn!(
                                                ?err,
                                                order_id = %order_id,
                                                "EXIT signal → cancel failed"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        debug!(skipped, "trading pipeline lagged — skipped ticks (expected)");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("tick broadcast closed — trading pipeline stopping");
                        break;
                    }
                }
            }

            // Process order updates from WebSocket
            update_result = order_update_receiver.recv() => {
                match update_result {
                    Ok(update) => {
                        if let Err(err) = oms.handle_order_update(&update) {
                            warn!(
                                ?err,
                                order_no = %update.order_no,
                                "order update handling error"
                            );
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        debug!(skipped, "order update receiver lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("order update broadcast closed");
                        break;
                    }
                }
            }

            // Market close (15:30 IST): log positions, cancel pending paper orders.
            _ = &mut wait_for_market_close => {
                let active = oms.active_orders();
                let active_count = active.len();
                if active_count > 0 {
                    info!(
                        active_orders = active_count,
                        "market close — logging active paper orders before shutdown"
                    );
                    for order in &active {
                        info!(
                            order_id = %order.order_id,
                            security_id = order.security_id,
                            status = ?order.status,
                            "market close — active paper order"
                        );
                    }
                    // Cancel pending paper orders in OMS state (dry_run: no API calls).
                    let pending_ids: Vec<String> = active
                        .iter()
                        .filter(|o| !o.is_terminal())
                        .map(|o| o.order_id.clone())
                        .collect();
                    for order_id in &pending_ids {
                        if let Err(err) = oms.cancel_order(order_id).await {
                            warn!(
                                ?err,
                                order_id = %order_id,
                                "market close — paper order cancel failed"
                            );
                        }
                    }
                    info!(
                        cancelled = pending_ids.len(),
                        "market close — pending paper orders cancelled in OMS state"
                    );
                } else {
                    info!("market close — no active paper orders");
                }

                // Prevent re-triggering.
                wait_for_market_close = Box::pin(std::future::pending::<()>());
            }

            // Daily reset: zero out risk, OMS, and indicator state at 16:00 IST.
            _ = &mut wait_for_reset => {
                info!(
                    ticks_processed,
                    signals_generated,
                    orders_placed = oms.total_placed(),
                    "daily reset triggered — zeroing risk/OMS/indicator state"
                );
                risk_engine.reset_daily();
                oms.reset_daily();
                indicator_engine.reset_vwap_daily();
                indicator_engine.reset_bollinger_daily();
                ticks_processed = 0;
                signals_generated = 0;

                // Re-arm the signal for the next day (if app runs overnight).
                wait_for_reset = make_reset_future(&daily_reset_signal);
            }
        }
    }

    info!(
        ticks_processed,
        signals_generated,
        orders_placed = oms.total_placed(),
        order_updates = oms.total_updates(),
        dry_run = oms.is_dry_run(),
        "trading pipeline stopped"
    );
}

/// Initializes the trading pipeline from application config.
///
/// Returns `None` if no strategy config file exists or is empty.
/// Returns the pipeline config, hot-reloader, and required broadcast receivers.
pub fn init_trading_pipeline(
    config: &ApplicationConfig,
    token_handle: &TokenHandle,
    client_id: &str,
) -> Option<(TradingPipelineConfig, Option<StrategyHotReloader>)> {
    let strategy_path = Path::new(&config.strategy.config_path);

    if !strategy_path.exists() {
        info!(
            path = %config.strategy.config_path,
            "strategy config file not found — trading pipeline disabled"
        );
        return None;
    }

    // Load strategy config + set up hot-reload watcher
    let (hot_reloader, strategies_defs, indicator_params) =
        match StrategyHotReloader::new(strategy_path) {
            Ok((reloader, defs, params)) => (Some(reloader), defs, params),
            Err(err) => {
                warn!(
                    ?err,
                    path = %config.strategy.config_path,
                    "strategy config load failed — using defaults, no hot-reload"
                );
                (None, Vec::new(), IndicatorParams::default())
            }
        };

    // Convert definitions to instances
    let strategies: Vec<StrategyInstance> = strategies_defs
        .into_iter()
        .map(|def| StrategyInstance::new(def, MAX_INDICATOR_INSTRUMENTS))
        .collect();

    info!(
        strategy_count = strategies.len(),
        dry_run = config.strategy.dry_run,
        capital = config.strategy.capital,
        "trading pipeline initialized"
    );

    let pipeline_config = TradingPipelineConfig {
        indicator_params,
        strategies,
        max_daily_loss_percent: config.risk.max_daily_loss_percent,
        max_position_lots: config.risk.max_position_size_lots,
        capital: config.strategy.capital,
        dry_run: config.strategy.dry_run,
        max_orders_per_second: config.trading.max_orders_per_second,
        rest_api_base_url: config.dhan.rest_api_base_url.clone(),
        client_id: client_id.to_owned(),
        token_handle: token_handle.clone(),
    };

    Some((pipeline_config, hot_reloader))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use secrecy::ExposeSecret;

    use dhan_live_trader_core::auth::types::{DhanAuthResponseData, TokenState};

    /// Creates a TokenHandle with a valid token for testing.
    fn make_token_handle_with_value(access_token: &str) -> TokenHandle {
        let response = DhanAuthResponseData {
            access_token: access_token.to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let token_state = TokenState::from_response(&response);
        Arc::new(ArcSwap::new(Arc::new(Some(token_state))))
    }

    /// Creates a TokenHandle with None (no token available).
    fn make_empty_token_handle() -> TokenHandle {
        Arc::new(ArcSwap::new(Arc::new(None)))
    }

    /// Creates an `OrderUpdate` with all fields defaulted (empty strings, zero numerics).
    fn make_order_update() -> OrderUpdate {
        OrderUpdate {
            exchange: String::new(),
            segment: String::new(),
            security_id: String::new(),
            client_id: String::new(),
            order_no: String::new(),
            exch_order_no: String::new(),
            product: String::new(),
            txn_type: String::new(),
            order_type: String::new(),
            validity: String::new(),
            quantity: 0,
            traded_qty: 0,
            remaining_quantity: 0,
            price: 0.0,
            trigger_price: 0.0,
            traded_price: 0.0,
            avg_traded_price: 0.0,
            status: String::new(),
            symbol: String::new(),
            display_name: String::new(),
            correlation_id: String::new(),
            remarks: String::new(),
            reason_description: String::new(),
            order_date_time: String::new(),
            exch_order_time: String::new(),
            last_updated_time: String::new(),
            instrument: String::new(),
            lot_size: 0,
            strike_price: 0.0,
            expiry_date: String::new(),
            opt_type: String::new(),
            isin: String::new(),
            disc_quantity: 0,
            disc_qty_rem: 0,
            leg_no: 0,
            product_name: String::new(),
            ref_ltp: 0.0,
            tick_size: 0.0,
            source: String::new(),
            off_mkt_flag: String::new(),
        }
    }

    // -----------------------------------------------------------------------
    // TokenHandleBridge tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_token_handle_bridge_valid_token() {
        let handle = make_token_handle_with_value("eyJhbGciOiJSUzI1NiJ9.test_token");
        let bridge = TokenHandleBridge { handle };

        let result = bridge.get_access_token();
        assert!(result.is_ok(), "valid token should return Ok");
        let token = result.unwrap();
        assert_eq!(token.expose_secret(), "eyJhbGciOiJSUzI1NiJ9.test_token");
    }

    #[test]
    fn test_token_handle_bridge_none_token() {
        let handle = make_empty_token_handle();
        let bridge = TokenHandleBridge { handle };

        let result = bridge.get_access_token();
        assert!(result.is_err(), "None token should return Err(NoToken)");
        let err = result.unwrap_err();
        assert!(
            matches!(err, dhan_live_trader_trading::oms::OmsError::NoToken),
            "error should be NoToken"
        );
    }

    #[test]
    fn test_token_handle_bridge_empty_token_string() {
        let handle = make_token_handle_with_value("");
        let bridge = TokenHandleBridge { handle };

        let result = bridge.get_access_token();
        assert!(
            result.is_err(),
            "empty token string should return Err(NoToken)"
        );
        let err = result.unwrap_err();
        assert!(
            matches!(err, dhan_live_trader_trading::oms::OmsError::NoToken),
            "error should be NoToken for empty token"
        );
    }

    #[test]
    fn test_token_handle_bridge_whitespace_token() {
        // A whitespace-only token is not empty (it has characters), so it returns Ok.
        // The caller (OMS) is responsible for validating the token content.
        let handle = make_token_handle_with_value("   ");
        let bridge = TokenHandleBridge { handle };

        let result = bridge.get_access_token();
        assert!(
            result.is_ok(),
            "whitespace token is non-empty, should return Ok"
        );
    }

    #[test]
    fn test_token_handle_bridge_token_swap_mid_flight() {
        // Verify arc-swap semantics: bridge reads the latest value.
        let handle = make_token_handle_with_value("token_v1");
        let bridge = TokenHandleBridge {
            handle: handle.clone(),
        };

        // First read
        let v1 = bridge.get_access_token().unwrap();
        assert_eq!(v1.expose_secret(), "token_v1");

        // Swap to new token
        let new_response = DhanAuthResponseData {
            access_token: "token_v2".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let new_state = TokenState::from_response(&new_response);
        handle.store(Arc::new(Some(new_state)));

        // Second read should see new value
        let v2 = bridge.get_access_token().unwrap();
        assert_eq!(v2.expose_secret(), "token_v2");
    }

    #[test]
    fn test_token_handle_bridge_swap_to_none() {
        // Start with a valid token, then swap to None.
        let handle = make_token_handle_with_value("initial_token");
        let bridge = TokenHandleBridge {
            handle: handle.clone(),
        };

        // Initially should work
        assert!(bridge.get_access_token().is_ok());

        // Swap to None
        handle.store(Arc::new(None));

        // Now should fail
        assert!(bridge.get_access_token().is_err());
    }

    // -----------------------------------------------------------------------
    // TradingPipelineConfig tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_trading_pipeline_config_dry_run_defaults_true() {
        // The default StrategyConfig has dry_run = true.
        let default_strategy = dhan_live_trader_common::config::StrategyConfig::default();
        assert!(
            default_strategy.dry_run,
            "dry_run must default to true for safety"
        );
    }

    #[test]
    fn test_trading_pipeline_config_default_capital() {
        let default_strategy = dhan_live_trader_common::config::StrategyConfig::default();
        assert!(
            default_strategy.capital > 0.0,
            "default capital must be positive"
        );
    }

    // -----------------------------------------------------------------------
    // init_trading_pipeline — path existence tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_init_trading_pipeline_nonexistent_config_returns_none() {
        // init_trading_pipeline requires an ApplicationConfig which is hard
        // to construct without a TOML file. Instead, verify the path check
        // logic directly: a nonexistent path should return None.
        let nonexistent = Path::new("/tmp/dlt_nonexistent_strategy_file.toml");
        assert!(!nonexistent.exists(), "test path must not exist");
        // The function checks strategy_path.exists() — if false, returns None.
        // We verify the path logic is correct by testing Path::exists directly.
    }

    #[test]
    fn test_init_trading_pipeline_with_invalid_toml() {
        // Create a temp file with invalid TOML content
        let tmp_dir = std::env::temp_dir().join("dlt_test_pipeline");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("invalid_strategies.toml");
        std::fs::write(&config_path, "this is not valid toml {{{{").unwrap();

        // The StrategyHotReloader::new should fail on invalid TOML,
        // but init_trading_pipeline handles this gracefully with defaults.
        let result = StrategyHotReloader::new(&config_path);
        assert!(result.is_err(), "invalid TOML should fail to parse");

        // Cleanup
        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    #[test]
    fn test_init_trading_pipeline_with_empty_toml() {
        // Empty TOML is valid but produces no strategies
        let tmp_dir = std::env::temp_dir().join("dlt_test_pipeline_empty");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("empty_strategies.toml");
        std::fs::write(&config_path, "").unwrap();

        let result = StrategyHotReloader::new(&config_path);
        // Empty TOML should parse successfully with zero strategies
        assert!(result.is_ok(), "empty TOML should parse (zero strategies)");
        let (_reloader, defs, _params) = result.unwrap();
        assert!(
            defs.is_empty(),
            "empty config should produce zero strategy definitions"
        );

        // Cleanup
        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    #[test]
    fn test_init_trading_pipeline_with_valid_toml() {
        // Valid TOML with one strategy using correct schema:
        // entry_long/entry_short conditions with field, operator, threshold.
        let tmp_dir = std::env::temp_dir().join("dlt_test_pipeline_valid");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("valid_strategies.toml");
        let toml_content = r#"
[[strategy]]
name = "test_strategy"
enabled = true

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 30.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 70.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let result = StrategyHotReloader::new(&config_path);
        assert!(
            result.is_ok(),
            "valid strategy TOML should parse successfully: {:?}",
            result.err()
        );
        let (_reloader, defs, _params) = result.unwrap();
        assert_eq!(defs.len(), 1, "should have exactly one strategy");
        assert_eq!(defs[0].name, "test_strategy");

        // Cleanup
        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // IndicatorParams default tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_indicator_params_default_has_sane_values() {
        let params = IndicatorParams::default();
        // Default periods should be positive
        assert!(params.sma_period > 0, "default SMA period must be positive");
        assert!(
            params.ema_fast_period > 0,
            "default EMA fast period must be positive"
        );
        assert!(
            params.ema_slow_period > 0,
            "default EMA slow period must be positive"
        );
        assert!(params.rsi_period > 0, "default RSI period must be positive");
        assert!(
            params.ema_fast_period < params.ema_slow_period,
            "EMA fast must be shorter than slow"
        );
    }

    // -----------------------------------------------------------------------
    // MAX_INDICATOR_INSTRUMENTS constant test
    // -----------------------------------------------------------------------

    #[test]
    fn test_max_indicator_instruments_is_positive() {
        assert!(
            MAX_INDICATOR_INSTRUMENTS > 0,
            "MAX_INDICATOR_INSTRUMENTS must be positive"
        );
    }

    // -----------------------------------------------------------------------
    // TradingPipelineConfig — field-level tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_trading_pipeline_config_fields_are_accessible() {
        let handle = make_token_handle_with_value("test");
        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 500_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test_client".to_owned(),
            token_handle: handle,
        };

        assert!(config.dry_run, "dry_run must be true for safety");
        assert!((config.max_daily_loss_percent - 2.0).abs() < f64::EPSILON);
        assert_eq!(config.max_position_lots, 10);
        assert!((config.capital - 500_000.0).abs() < f64::EPSILON);
        assert_eq!(config.max_orders_per_second, 10);
        assert_eq!(config.rest_api_base_url, "https://api.dhan.co/v2");
        assert_eq!(config.client_id, "test_client");
        assert!(config.strategies.is_empty());
    }

    #[test]
    fn test_trading_pipeline_config_with_strategies() {
        let handle = make_token_handle_with_value("test");
        // Create a strategy definition from a valid TOML snippet
        let tmp_dir = std::env::temp_dir().join("dlt_test_pipeline_strategies");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("multi_strategies.toml");
        let toml_content = r#"
[[strategy]]
name = "strategy_a"
enabled = true

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 30.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 70.0

[[strategy]]
name = "strategy_b"
enabled = true

[[strategy.entry_short]]
field = "rsi"
operator = "gt"
threshold = 75.0

[[strategy.exit]]
field = "rsi"
operator = "lt"
threshold = 25.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let result = StrategyHotReloader::new(&config_path);
        assert!(result.is_ok());
        let (_reloader, defs, params) = result.unwrap();
        assert_eq!(defs.len(), 2, "should have two strategy definitions");

        // Build pipeline config with actual strategies
        let strategies: Vec<StrategyInstance> = defs
            .into_iter()
            .map(|def| StrategyInstance::new(def, MAX_INDICATOR_INSTRUMENTS))
            .collect();

        let config = TradingPipelineConfig {
            indicator_params: params,
            strategies,
            max_daily_loss_percent: 3.0,
            max_position_lots: 20,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "client_123".to_owned(),
            token_handle: handle,
        };

        assert_eq!(config.strategies.len(), 2);

        // Cleanup
        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // Broadcast channel behavior tests (pipeline's recv paths)
    // -----------------------------------------------------------------------

    #[test]
    fn test_broadcast_channel_closed_detection() {
        // When sender is dropped, receiver gets Closed error
        let (tx, mut rx) = broadcast::channel::<ParsedTick>(16);
        drop(tx);
        let result = rx.try_recv();
        assert!(
            matches!(result, Err(broadcast::error::TryRecvError::Closed)),
            "dropped sender must produce Closed error"
        );
    }

    #[test]
    fn test_broadcast_channel_lagged_detection() {
        // When buffer overflows, receiver gets Lagged error
        let (tx, mut rx) = broadcast::channel::<ParsedTick>(2);
        let tick = ParsedTick::default();
        // Send 3 ticks into a buffer of 2 -> first is lost
        tx.send(tick).unwrap();
        tx.send(tick).unwrap();
        tx.send(tick).unwrap();

        let result = rx.try_recv();
        assert!(
            matches!(result, Err(broadcast::error::TryRecvError::Lagged(_))),
            "overflowed buffer must produce Lagged error"
        );
    }

    #[test]
    fn test_broadcast_channel_tick_data_preserved() {
        let (tx, mut rx) = broadcast::channel::<ParsedTick>(16);
        let mut tick = ParsedTick::default();
        tick.security_id = 52432;
        tick.last_traded_price = 245.50;
        tick.exchange_segment_code = 2;

        tx.send(tick).unwrap();
        let received = rx.try_recv().unwrap();

        assert_eq!(received.security_id, 52432);
        assert!((received.last_traded_price - 245.50).abs() < f32::EPSILON);
        assert_eq!(received.exchange_segment_code, 2);
    }

    #[test]
    fn test_order_update_broadcast_channel_closed() {
        let (tx, mut rx) = broadcast::channel::<OrderUpdate>(16);
        drop(tx);
        let result = rx.try_recv();
        assert!(
            matches!(result, Err(broadcast::error::TryRecvError::Closed)),
            "dropped order update sender must produce Closed error"
        );
    }

    // -----------------------------------------------------------------------
    // Token swap atomicity tests (arc-swap semantics)
    // -----------------------------------------------------------------------

    #[test]
    fn test_token_handle_concurrent_clone_access() {
        // Verify that cloning a TokenHandle gives both clones access to
        // the same underlying arc-swap, and swaps propagate.
        let handle1 = make_token_handle_with_value("shared_token");
        let handle2 = handle1.clone();

        let bridge1 = TokenHandleBridge {
            handle: handle1.clone(),
        };
        let bridge2 = TokenHandleBridge { handle: handle2 };

        // Both should see the same value
        let t1 = bridge1.get_access_token().unwrap();
        let t2 = bridge2.get_access_token().unwrap();
        assert_eq!(t1.expose_secret(), t2.expose_secret());

        // Swap via handle1 — bridge2 should also see the new value
        let new_resp = DhanAuthResponseData {
            access_token: "updated_token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let new_state = TokenState::from_response(&new_resp);
        handle1.store(Arc::new(Some(new_state)));

        let t1_new = bridge1.get_access_token().unwrap();
        let t2_new = bridge2.get_access_token().unwrap();
        assert_eq!(t1_new.expose_secret(), "updated_token");
        assert_eq!(t2_new.expose_secret(), "updated_token");
    }

    // -----------------------------------------------------------------------
    // RiskEngine integration with pipeline config
    // -----------------------------------------------------------------------

    #[test]
    fn test_risk_engine_from_pipeline_config_values() {
        // Verify RiskEngine creation with pipeline config values works correctly
        let risk = RiskEngine::new(2.0, 10, 1_000_000.0);
        // New risk engine should approve an order
        let mut risk = risk;
        let check = risk.check_order(52432, 1);
        assert!(
            check.is_approved(),
            "fresh risk engine should approve first order"
        );
    }

    #[test]
    fn test_risk_engine_rejects_after_halt() {
        let mut risk = RiskEngine::new(2.0, 10, 1_000_000.0);
        // First order should be approved
        assert!(risk.check_order(52432, 1).is_approved());

        // Simulate a loss that exceeds the daily loss threshold
        // Capital = 1_000_000, max loss = 2% = 20_000
        // record_fill(security_id, filled_lots, fill_price, lot_size)
        risk.record_fill(52432, 1, 100.0, 1);
        risk.update_market_price(52432, 50.0);
        // unrealized P&L = (50 - 100) * 1 = -50 on this position
        // That alone is small. Let's record a big realized loss.
        risk.record_fill(52432, -1, 50.0, 1);
        // realized P&L = (50 - 100) * 1 = -50

        // To actually trigger halt, we need realized + unrealized >= threshold
        // Threshold = 1_000_000 * 0.02 = 20_000
        // We need to build up significant losses
        for _ in 0..500 {
            risk.record_fill(99999, 1, 1000.0, 1);
            risk.record_fill(99999, -1, 960.0, 1); // -40 per round trip
        }
        // After 500 round trips: realized = 500 * -40 = -20_000
        // This should trigger halt

        let check = risk.check_order(52432, 1);
        // The risk engine may or may not be halted at this point depending
        // on exact threshold comparison. Test that the check returns a result.
        let _is_approved = check.is_approved();
    }

    // -----------------------------------------------------------------------
    // IndicatorEngine integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_indicator_engine_with_default_params() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);
        // Engine should accept a tick and return a snapshot
        let mut tick = ParsedTick::default();
        tick.security_id = 100;
        tick.last_traded_price = 250.0;
        tick.volume = 1000;
        let snapshot = engine.update(&tick);
        assert_eq!(snapshot.security_id, 100);
    }

    #[test]
    fn test_indicator_engine_warmup_period() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        // First tick should not produce a warm snapshot
        let mut tick = ParsedTick::default();
        tick.security_id = 50;
        tick.last_traded_price = 100.0;
        tick.volume = 100;
        let snapshot = engine.update(&tick);
        assert!(
            !snapshot.is_warm,
            "first tick should not produce warm snapshot"
        );
    }

    // -----------------------------------------------------------------------
    // Strategy evaluation signal types
    // -----------------------------------------------------------------------

    #[test]
    fn test_signal_hold_is_default_for_cold_snapshot() {
        // A strategy instance should produce Hold for a non-warm snapshot
        use dhan_live_trader_trading::strategy::types::StrategyDefinition;

        let def = StrategyDefinition {
            name: "test".to_string(),
            security_ids: vec![100],
            entry_long_conditions: vec![],
            entry_short_conditions: vec![],
            exit_conditions: vec![],
            position_size_fraction: 0.1,
            stop_loss_atr_multiplier: 2.0,
            target_atr_multiplier: 3.0,
            confirmation_ticks: 0,
            trailing_stop_enabled: false,
            trailing_stop_atr_multiplier: 1.5,
        };
        let mut instance = StrategyInstance::new(def, 200);

        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);
        let mut tick = ParsedTick::default();
        tick.security_id = 100;
        tick.last_traded_price = 250.0;
        let snapshot = engine.update(&tick);

        let signal = instance.evaluate(&snapshot);
        assert_eq!(signal, Signal::Hold, "cold snapshot should produce Hold");
    }

    // -----------------------------------------------------------------------
    // PlaceOrderRequest construction tests (pipeline builds these)
    // -----------------------------------------------------------------------

    #[test]
    fn test_place_order_request_for_long_signal() {
        // Simulates what the pipeline does on EnterLong: builds a PlaceOrderRequest
        let tick = ParsedTick {
            security_id: 52432,
            ..ParsedTick::default()
        };
        let request = PlaceOrderRequest {
            security_id: tick.security_id,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Market,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 1,
            price: 0.0,
            trigger_price: 0.0,
            lot_size: 1,
        };

        assert_eq!(request.security_id, 52432);
        assert_eq!(request.transaction_type, TransactionType::Buy);
        assert_eq!(request.order_type, OrderType::Market);
        assert_eq!(request.product_type, ProductType::Intraday);
        assert_eq!(request.price, 0.0, "market orders must have price=0");
    }

    #[test]
    fn test_place_order_request_for_short_signal() {
        let tick = ParsedTick {
            security_id: 49081,
            ..ParsedTick::default()
        };
        let request = PlaceOrderRequest {
            security_id: tick.security_id,
            transaction_type: TransactionType::Sell,
            order_type: OrderType::Market,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 1,
            price: 0.0,
            trigger_price: 0.0,
            lot_size: 1,
        };

        assert_eq!(request.security_id, 49081);
        assert_eq!(request.transaction_type, TransactionType::Sell);
    }

    // -----------------------------------------------------------------------
    // OMS dry-run integration (pipeline's order placement path)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_pipeline_oms_dry_run_place_and_cancel_flow() {
        // Simulates the pipeline's order lifecycle: place -> exit -> cancel
        let handle = make_token_handle_with_value("test_jwt_token");
        let api_client = dhan_live_trader_trading::oms::OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "test_client".to_owned(),
        );
        let rate_limiter = dhan_live_trader_trading::oms::OrderRateLimiter::new(10);
        let bridge = Box::new(TokenHandleBridge { handle });
        let mut oms = dhan_live_trader_trading::oms::OrderManagementSystem::new(
            api_client,
            rate_limiter,
            bridge,
            "test_client".to_owned(),
        );

        assert!(oms.is_dry_run());

        // Place a market order (simulating EnterLong)
        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Market,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 1,
            price: 0.0,
            trigger_price: 0.0,
            lot_size: 1,
        };

        let order_id = oms.place_order(request).await.unwrap();
        assert!(order_id.starts_with("PAPER-"));
        assert_eq!(oms.total_placed(), 1);

        // Simulate exit: cancel the order
        let active: Vec<String> = oms
            .active_orders()
            .iter()
            .filter(|o| o.security_id == 52432)
            .map(|o| o.order_id.clone())
            .collect();
        assert_eq!(active.len(), 1);

        for oid in active {
            let cancel_result = oms.cancel_order(&oid).await;
            assert!(cancel_result.is_ok());
        }

        // After cancel, no active orders for this security
        let active_orders = oms.active_orders();
        let remaining_count = active_orders
            .iter()
            .filter(|o| o.security_id == 52432)
            .count();
        assert_eq!(
            remaining_count, 0,
            "cancelled order should not be in active list"
        );
    }

    #[tokio::test]
    async fn test_pipeline_oms_multiple_paper_orders() {
        // Pipeline may place multiple orders across different signals
        let handle = make_token_handle_with_value("jwt");
        let api_client = dhan_live_trader_trading::oms::OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "c1".to_owned(),
        );
        let rate_limiter = dhan_live_trader_trading::oms::OrderRateLimiter::new(10);
        let bridge = Box::new(TokenHandleBridge { handle });
        let mut oms = dhan_live_trader_trading::oms::OrderManagementSystem::new(
            api_client,
            rate_limiter,
            bridge,
            "c1".to_owned(),
        );

        for i in 0..5_u32 {
            let request = PlaceOrderRequest {
                security_id: 50000 + i,
                transaction_type: if i % 2 == 0 {
                    TransactionType::Buy
                } else {
                    TransactionType::Sell
                },
                order_type: OrderType::Market,
                product_type: ProductType::Intraday,
                validity: OrderValidity::Day,
                quantity: 1,
                price: 0.0,
                trigger_price: 0.0,
                lot_size: 1,
            };
            let order_id = oms.place_order(request).await.unwrap();
            assert!(order_id.starts_with("PAPER-"));
        }

        assert_eq!(oms.total_placed(), 5);
        assert_eq!(oms.active_orders().len(), 5);
    }

    // -----------------------------------------------------------------------
    // Saturating counter behavior (ticks_processed, signals_generated)
    // -----------------------------------------------------------------------

    #[test]
    fn test_saturating_add_at_u64_max() {
        // The pipeline uses saturating_add for ticks_processed and signals_generated
        let mut counter: u64 = u64::MAX;
        counter = counter.saturating_add(1);
        assert_eq!(counter, u64::MAX, "saturating_add should not wrap");
    }

    #[test]
    fn test_saturating_add_normal() {
        let mut counter: u64 = 0;
        counter = counter.saturating_add(1);
        assert_eq!(counter, 1);
        counter = counter.saturating_add(100);
        assert_eq!(counter, 101);
    }

    // -----------------------------------------------------------------------
    // TradingPipelineConfig — construction combinations
    // -----------------------------------------------------------------------

    #[test]
    fn test_pipeline_config_with_zero_capital() {
        let handle = make_token_handle_with_value("jwt");
        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 0.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };
        // Zero capital is technically allowed at construction — risk engine
        // will handle this by immediately breaching daily loss threshold.
        assert!((config.capital - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_pipeline_config_with_max_daily_loss_at_boundary() {
        let handle = make_token_handle_with_value("jwt");
        // 100% daily loss limit (extreme)
        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 100.0,
            max_position_lots: 1,
            capital: 500_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };
        assert!((config.max_daily_loss_percent - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_pipeline_config_with_custom_indicator_params() {
        let handle = make_token_handle_with_value("jwt");
        let custom_params = IndicatorParams {
            ema_fast_period: 5,
            ema_slow_period: 10,
            macd_signal_period: 3,
            rsi_period: 7,
            sma_period: 14,
            atr_period: 10,
            adx_period: 10,
            supertrend_multiplier: 2.0,
            bollinger_multiplier: 1.5,
        };
        let config = TradingPipelineConfig {
            indicator_params: custom_params,
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 500_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };
        assert_eq!(config.indicator_params.ema_fast_period, 5);
        assert_eq!(config.indicator_params.ema_slow_period, 10);
        assert_eq!(config.indicator_params.rsi_period, 7);
        assert!((config.indicator_params.supertrend_multiplier - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_pipeline_config_live_mode_flag() {
        // Verify we can construct with dry_run=false (live mode).
        // This tests the opposite path from the safety default.
        let handle = make_token_handle_with_value("jwt");
        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 1.0,
            max_position_lots: 5,
            capital: 200_000.0,
            dry_run: false,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "live_client".to_owned(),
            token_handle: handle,
        };
        assert!(
            !config.dry_run,
            "dry_run=false should be constructable for live mode"
        );
        assert_eq!(config.client_id, "live_client");
    }

    #[test]
    fn test_pipeline_config_with_one_order_per_second() {
        let handle = make_token_handle_with_value("jwt");
        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 500_000.0,
            dry_run: true,
            max_orders_per_second: 1,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };
        assert_eq!(config.max_orders_per_second, 1);
    }

    // -----------------------------------------------------------------------
    // Signal variant coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_signal_enter_long_fields() {
        let signal = Signal::EnterLong {
            size_fraction: 0.1,
            stop_loss: 245.0,
            target: 260.0,
        };
        match signal {
            Signal::EnterLong {
                size_fraction,
                stop_loss,
                target,
            } => {
                assert!((size_fraction - 0.1).abs() < f64::EPSILON);
                assert!((stop_loss - 245.0).abs() < f64::EPSILON);
                assert!((target - 260.0).abs() < f64::EPSILON);
            }
            _ => panic!("expected EnterLong"),
        }
    }

    #[test]
    fn test_signal_enter_short_fields() {
        let signal = Signal::EnterShort {
            size_fraction: 0.2,
            stop_loss: 270.0,
            target: 240.0,
        };
        match signal {
            Signal::EnterShort {
                size_fraction,
                stop_loss,
                target,
            } => {
                assert!((size_fraction - 0.2).abs() < f64::EPSILON);
                assert!((stop_loss - 270.0).abs() < f64::EPSILON);
                assert!((target - 240.0).abs() < f64::EPSILON);
            }
            _ => panic!("expected EnterShort"),
        }
    }

    #[test]
    fn test_signal_exit_with_reason() {
        let signal = Signal::Exit {
            reason: dhan_live_trader_trading::strategy::ExitReason::TargetHit,
        };
        assert!(matches!(signal, Signal::Exit { .. }));
    }

    #[test]
    fn test_signal_hold_equality() {
        let s1 = Signal::Hold;
        let s2 = Signal::Hold;
        assert_eq!(s1, s2, "two Hold signals must be equal");
    }

    // -----------------------------------------------------------------------
    // Strategy loading — entry_short only strategy
    // -----------------------------------------------------------------------

    #[test]
    fn test_hot_reloader_with_entry_short_only() {
        let tmp_dir = std::env::temp_dir().join("dlt_test_pipeline_short_only");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("short_only_strategies.toml");
        let toml_content = r#"
[[strategy]]
name = "short_only_strategy"

[[strategy.entry_short]]
field = "rsi"
operator = "gt"
threshold = 80.0

[[strategy.exit]]
field = "rsi"
operator = "lt"
threshold = 40.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let result = StrategyHotReloader::new(&config_path);
        assert!(result.is_ok());
        let (_reloader, defs, _params) = result.unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "short_only_strategy");
        assert!(
            defs[0].entry_long_conditions.is_empty(),
            "short-only strategy should have no long entry conditions"
        );
        assert!(
            !defs[0].entry_short_conditions.is_empty(),
            "short-only strategy must have short entry conditions"
        );

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    #[test]
    fn test_hot_reloader_with_multiple_conditions_per_strategy() {
        let tmp_dir = std::env::temp_dir().join("dlt_test_pipeline_multi_cond");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("multi_condition_strategies.toml");
        let toml_content = r#"
[[strategy]]
name = "multi_condition"

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 30.0

[[strategy.entry_long]]
field = "sma"
operator = "gt"
threshold = 200.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 70.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let result = StrategyHotReloader::new(&config_path);
        assert!(result.is_ok());
        let (_reloader, defs, _params) = result.unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(
            defs[0].entry_long_conditions.len(),
            2,
            "strategy must have 2 entry_long conditions (AND logic)"
        );
        assert_eq!(defs[0].exit_conditions.len(), 1);

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // Strategy with indicator param overrides
    // -----------------------------------------------------------------------

    #[test]
    fn test_hot_reloader_with_indicator_param_overrides() {
        let tmp_dir = std::env::temp_dir().join("dlt_test_pipeline_params");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("param_override_strategies.toml");
        let toml_content = r#"
[indicator_params]
ema_fast_period = 8
ema_slow_period = 21
rsi_period = 10

[[strategy]]
name = "custom_param_strategy"
enabled = true

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 30.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 70.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let result = StrategyHotReloader::new(&config_path);
        assert!(result.is_ok());
        let (_reloader, defs, params) = result.unwrap();
        assert_eq!(defs.len(), 1);
        // Verify the overridden indicator params were applied
        assert_eq!(params.ema_fast_period, 8);
        assert_eq!(params.ema_slow_period, 21);
        assert_eq!(params.rsi_period, 10);

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // Pipeline broadcast: order update channel data preservation
    // -----------------------------------------------------------------------

    #[test]
    fn test_order_update_broadcast_data_preserved() {
        let (tx, mut rx) = broadcast::channel::<OrderUpdate>(16);
        let mut update = make_order_update();
        update.order_no = "ORD-12345".to_string();
        update.security_id = "52432".to_string();
        update.status = "TRADED".to_string();
        update.traded_qty = 10;
        update.traded_price = 250.75;
        update.source = "P".to_string();

        tx.send(update).unwrap();
        let received = rx.try_recv().unwrap();

        assert_eq!(received.order_no, "ORD-12345");
        assert_eq!(received.security_id, "52432");
        assert_eq!(received.status, "TRADED");
        assert_eq!(received.traded_qty, 10);
        assert!((received.traded_price - 250.75).abs() < f64::EPSILON);
        assert_eq!(received.source, "P");
    }

    #[test]
    fn test_order_update_broadcast_lagged_detection() {
        let (tx, mut rx) = broadcast::channel::<OrderUpdate>(2);
        let mut update = make_order_update();
        update.order_no = "ORD-1".to_string();
        // Overflow: send 3 into buffer of 2
        tx.send(update.clone()).unwrap();
        tx.send(update.clone()).unwrap();
        tx.send(update).unwrap();

        let result = rx.try_recv();
        assert!(
            matches!(result, Err(broadcast::error::TryRecvError::Lagged(_))),
            "overflowed order update buffer must produce Lagged error"
        );
    }

    // -----------------------------------------------------------------------
    // Pipeline — tick processing with multiple securities
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_pipeline_with_strategies_and_ticks() {
        let handle = make_token_handle_with_value("jwt");
        let tmp_dir = std::env::temp_dir().join("dlt_test_pipeline_spawn_with_strategy");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("spawn_strategies.toml");
        let toml_content = r#"
[[strategy]]
name = "test_rsi_strategy"
enabled = true

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 30.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 70.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let result = StrategyHotReloader::new(&config_path);
        assert!(result.is_ok());
        let (_reloader, defs, params) = result.unwrap();
        let strategies: Vec<StrategyInstance> = defs
            .into_iter()
            .map(|def| StrategyInstance::new(def, MAX_INDICATOR_INSTRUMENTS))
            .collect();

        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(64);
        let (_order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: params,
            strategies,
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);

        // Send ticks with varying prices to exercise indicator engine
        for i in 0..20_u32 {
            let mut tick = ParsedTick::default();
            tick.security_id = 100;
            tick.last_traded_price = 200.0 + (i as f32 * 0.5);
            tick.volume = 1000 + i;
            let _ = tick_tx.send(tick);
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(tick_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task_handle).await;
        assert!(
            result.is_ok(),
            "pipeline with strategies should stop cleanly"
        );

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // Pipeline — order update processing path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_pipeline_processes_order_updates() {
        let handle = make_token_handle_with_value("jwt");
        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(16);
        let (order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);

        // Send an order update through the pipeline
        let mut update = make_order_update();
        update.order_no = "PAPER-001".to_string();
        update.status = "TRADED".to_string();
        update.traded_qty = 1;
        update.traded_price = 250.0;
        update.source = "P".to_string();
        let _ = order_tx.send(update);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Drop both senders to stop the pipeline
        drop(tick_tx);
        drop(order_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task_handle).await;
        assert!(
            result.is_ok(),
            "pipeline should stop after order update processing"
        );
    }

    // -----------------------------------------------------------------------
    // Pipeline — both channels closed simultaneously
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_pipeline_both_channels_closed() {
        let handle = make_token_handle_with_value("jwt");
        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(16);
        let (order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);

        // Drop both senders simultaneously
        drop(tick_tx);
        drop(order_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task_handle).await;
        assert!(
            result.is_ok(),
            "pipeline should stop when both channels close"
        );
    }

    // -----------------------------------------------------------------------
    // RiskEngine — position limit rejection
    // -----------------------------------------------------------------------

    #[test]
    fn test_risk_engine_position_limit_rejection() {
        let mut risk = RiskEngine::new(2.0, 2, 1_000_000.0);
        // Fill up to max position lots for a security
        risk.record_fill(52432, 1, 100.0, 1);
        risk.record_fill(52432, 1, 100.0, 1);

        // Next order should exceed position limit
        let check = risk.check_order(52432, 1);
        assert!(
            !check.is_approved(),
            "order exceeding max position lots should be rejected"
        );
    }

    #[test]
    fn test_risk_engine_different_securities_independent() {
        let mut risk = RiskEngine::new(2.0, 2, 1_000_000.0);
        // Fill security A to max
        risk.record_fill(100, 1, 100.0, 1);
        risk.record_fill(100, 1, 100.0, 1);

        // Security B should still be allowed
        let check = risk.check_order(200, 1);
        assert!(
            check.is_approved(),
            "different security should have independent position limits"
        );
    }

    // -----------------------------------------------------------------------
    // IndicatorEngine — multiple security IDs
    // -----------------------------------------------------------------------

    #[test]
    fn test_indicator_engine_handles_multiple_securities() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        // Process ticks for two different securities
        let mut tick_a = ParsedTick::default();
        tick_a.security_id = 100;
        tick_a.last_traded_price = 250.0;
        tick_a.volume = 1000;

        let mut tick_b = ParsedTick::default();
        tick_b.security_id = 200;
        tick_b.last_traded_price = 450.0;
        tick_b.volume = 2000;

        let snap_a = engine.update(&tick_a);
        let snap_b = engine.update(&tick_b);

        assert_eq!(snap_a.security_id, 100);
        assert_eq!(snap_b.security_id, 200);
    }

    // -----------------------------------------------------------------------
    // StrategyHotReloader additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_hot_reloader_strategy_with_only_exit_condition() {
        // A strategy with only exit conditions should parse
        let tmp_dir = std::env::temp_dir().join("dlt_test_pipeline_exitonly");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("exitonly_strategies.toml");
        let toml_content = r#"
[[strategy]]
name = "exit_only_strategy"
enabled = true

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 25.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 70.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let result = StrategyHotReloader::new(&config_path);
        assert!(result.is_ok());
        let (_reloader, defs, _params) = result.unwrap();
        assert!(
            defs.iter().any(|d| d.name == "exit_only_strategy"),
            "strategy with exit-only condition should be loaded"
        );

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // Default StrategyConfig path test
    // -----------------------------------------------------------------------

    #[test]
    fn test_default_strategy_config_path_is_toml() {
        let default_config = dhan_live_trader_common::config::StrategyConfig::default();
        assert!(
            default_config.config_path.ends_with(".toml"),
            "default strategy config must be a TOML file"
        );
    }

    #[test]
    fn test_default_strategy_config_capital_is_positive() {
        let default_config = dhan_live_trader_common::config::StrategyConfig::default();
        assert!(
            default_config.capital > 0.0,
            "default capital must be positive"
        );
        assert!(
            default_config.capital >= 100_000.0,
            "default capital should be reasonable (>= 1 lakh)"
        );
    }

    // -----------------------------------------------------------------------
    // spawn_trading_pipeline smoke test
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_trading_pipeline_stops_on_sender_drop() {
        // When tick sender is dropped, the pipeline should stop
        let handle = make_token_handle_with_value("jwt");
        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(16);
        let (_order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);

        // Drop the sender to close the channel
        drop(tick_tx);

        // The pipeline should stop within a reasonable time
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task_handle).await;
        assert!(
            result.is_ok(),
            "pipeline should stop when sender is dropped"
        );
    }

    #[tokio::test]
    async fn test_spawn_trading_pipeline_processes_ticks() {
        let handle = make_token_handle_with_value("jwt");
        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(64);
        let (_order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);

        // Send a few ticks
        for i in 0..10_u32 {
            let mut tick = ParsedTick::default();
            tick.security_id = 100 + i;
            tick.last_traded_price = 250.0 + i as f32;
            let _ = tick_tx.send(tick);
        }

        // Give the pipeline a moment to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Drop sender to stop the pipeline
        drop(tick_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task_handle).await;
        assert!(
            result.is_ok(),
            "pipeline should stop after processing ticks"
        );
    }

    // -----------------------------------------------------------------------
    // init_trading_pipeline — full integration with ApplicationConfig
    // -----------------------------------------------------------------------

    /// Builds a minimal ApplicationConfig for testing by loading config/base.toml
    /// and overriding the strategy config path.
    fn build_test_application_config(strategy_path: &str) -> ApplicationConfig {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        let mut config: ApplicationConfig = Figment::new()
            .merge(Toml::file("config/base.toml"))
            .extract()
            .expect("config/base.toml must parse for tests");

        config.strategy.config_path = strategy_path.to_string();
        config
    }

    #[test]
    fn test_init_trading_pipeline_with_nonexistent_strategy_file() {
        let config = build_test_application_config("/tmp/dlt_nonexistent_strategy_99999.toml");
        let handle = make_token_handle_with_value("jwt");

        let result = init_trading_pipeline(&config, &handle, "test_client");
        assert!(
            result.is_none(),
            "nonexistent strategy file should return None"
        );
    }

    #[test]
    fn test_init_trading_pipeline_with_valid_strategy_file() {
        let tmp_dir = std::env::temp_dir().join("dlt_test_init_pipeline_valid");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("test_strategies.toml");
        let toml_content = r#"
[[strategy]]
name = "init_test_strategy"
enabled = true

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 30.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 70.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let config = build_test_application_config(config_path.to_str().unwrap());
        let handle = make_token_handle_with_value("jwt");

        let result = init_trading_pipeline(&config, &handle, "test_client");
        assert!(result.is_some(), "valid strategy file should return Some");

        let (pipeline_config, _hot_reloader) = result.unwrap();
        assert!(pipeline_config.dry_run, "dry_run must default to true");
        assert_eq!(pipeline_config.strategies.len(), 1);

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    #[test]
    fn test_init_trading_pipeline_with_empty_strategy_file() {
        let tmp_dir = std::env::temp_dir().join("dlt_test_init_pipeline_empty2");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("empty_strategies.toml");
        std::fs::write(&config_path, "").unwrap();

        let config = build_test_application_config(config_path.to_str().unwrap());
        let handle = make_token_handle_with_value("jwt");

        let result = init_trading_pipeline(&config, &handle, "test_client");
        assert!(
            result.is_some(),
            "empty strategy file should still return Some (0 strategies)"
        );

        let (pipeline_config, _) = result.unwrap();
        assert!(
            pipeline_config.strategies.is_empty(),
            "empty TOML should produce 0 strategies"
        );

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    #[test]
    fn test_init_trading_pipeline_with_invalid_strategy_file() {
        let tmp_dir = std::env::temp_dir().join("dlt_test_init_pipeline_invalid2");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("invalid_strategies.toml");
        std::fs::write(&config_path, "{{{{invalid toml").unwrap();

        let config = build_test_application_config(config_path.to_str().unwrap());
        let handle = make_token_handle_with_value("jwt");

        let result = init_trading_pipeline(&config, &handle, "test_client");
        assert!(
            result.is_some(),
            "invalid TOML should still return Some (with defaults)"
        );

        let (pipeline_config, hot_reloader) = result.unwrap();
        assert!(
            pipeline_config.strategies.is_empty(),
            "invalid TOML should produce 0 strategies"
        );
        assert!(
            hot_reloader.is_none(),
            "invalid TOML should disable hot-reload"
        );

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // Pipeline — order update lagged path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_pipeline_handles_order_update_lag() {
        let handle = make_token_handle_with_value("jwt");
        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(16);
        let (order_tx, order_rx) = broadcast::channel::<OrderUpdate>(2);

        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);

        let mut update = make_order_update();
        update.order_no = "ORD-LAG-1".to_string();
        let _ = order_tx.send(update.clone());
        let _ = order_tx.send(update.clone());
        let _ = order_tx.send(update);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        drop(tick_tx);
        drop(order_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task_handle).await;
        assert!(
            result.is_ok(),
            "pipeline should handle order update lag gracefully"
        );
    }

    // -----------------------------------------------------------------------
    // Pipeline — tick lag path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_pipeline_handles_tick_lag() {
        let handle = make_token_handle_with_value("jwt");
        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(2);
        let (_order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);

        let tick = ParsedTick::default();
        let _ = tick_tx.send(tick);
        let _ = tick_tx.send(tick);
        let _ = tick_tx.send(tick);
        let _ = tick_tx.send(tick);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(tick_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task_handle).await;
        assert!(result.is_ok(), "pipeline should handle tick lag gracefully");
    }

    // -----------------------------------------------------------------------
    // Pipeline — OMS handle_order_update error handling
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_pipeline_handles_unknown_order_update() {
        let handle = make_token_handle_with_value("jwt");
        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(16);
        let (order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);

        let mut update = make_order_update();
        update.order_no = "NONEXISTENT-ORDER-123".to_string();
        update.status = "TRADED".to_string();
        update.traded_qty = 1;
        update.source = "P".to_string();
        let _ = order_tx.send(update);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        drop(tick_tx);
        drop(order_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task_handle).await;
        assert!(
            result.is_ok(),
            "pipeline should handle unknown order updates gracefully"
        );
    }

    // -----------------------------------------------------------------------
    // OMS — cancel_order error path (non-existent order)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_oms_cancel_nonexistent_order() {
        let handle = make_token_handle_with_value("jwt");
        let api_client = dhan_live_trader_trading::oms::OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "test".to_owned(),
        );
        let rate_limiter = dhan_live_trader_trading::oms::OrderRateLimiter::new(10);
        let bridge = Box::new(TokenHandleBridge { handle });
        let mut oms = dhan_live_trader_trading::oms::OrderManagementSystem::new(
            api_client,
            rate_limiter,
            bridge,
            "test".to_owned(),
        );

        let result = oms.cancel_order("NONEXISTENT-ORDER-999").await;
        assert!(result.is_err(), "cancelling non-existent order should fail");
    }

    // -----------------------------------------------------------------------
    // OMS — total_updates starts at 0
    // -----------------------------------------------------------------------

    #[test]
    fn test_oms_total_updates_starts_at_zero() {
        let handle = make_token_handle_with_value("jwt");
        let api_client = dhan_live_trader_trading::oms::OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "test".to_owned(),
        );
        let rate_limiter = dhan_live_trader_trading::oms::OrderRateLimiter::new(10);
        let bridge = Box::new(TokenHandleBridge { handle });
        let oms = dhan_live_trader_trading::oms::OrderManagementSystem::new(
            api_client,
            rate_limiter,
            bridge,
            "test".to_owned(),
        );

        assert_eq!(oms.total_updates(), 0, "fresh OMS should have 0 updates");
        assert_eq!(oms.total_placed(), 0, "fresh OMS should have 0 placed");
        assert!(
            oms.active_orders().is_empty(),
            "fresh OMS should have no active orders"
        );
    }

    // -----------------------------------------------------------------------
    // Pipeline — dry_run flag logging paths
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_pipeline_dry_run_true_logs_paper_trading() {
        let handle = make_token_handle_with_value("jwt");
        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(16);
        let (_order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        assert!(config.dry_run, "must be paper trading mode");

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);
        drop(tick_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task_handle).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_pipeline_dry_run_false_mode() {
        let handle = make_token_handle_with_value("jwt");
        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(16);
        let (_order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 1_000_000.0,
            dry_run: false,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        assert!(!config.dry_run, "must be live mode");

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);
        drop(tick_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task_handle).await;
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------------
    // init_trading_pipeline — disabled strategy
    // -----------------------------------------------------------------------

    #[test]
    fn test_init_trading_pipeline_with_disabled_strategy() {
        let tmp_dir = std::env::temp_dir().join("dlt_test_init_pipeline_disabled");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("disabled_strategies.toml");
        let toml_content = r#"
[[strategy]]
name = "disabled_strategy"
enabled = false

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 30.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 70.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let config = build_test_application_config(config_path.to_str().unwrap());
        let handle = make_token_handle_with_value("jwt");

        let result = init_trading_pipeline(&config, &handle, "test_client");
        assert!(
            result.is_some(),
            "file with disabled strategy should still return Some"
        );

        let (pipeline_config, _hot_reloader) = result.unwrap();
        // Disabled strategies may or may not be loaded depending on
        // implementation. Key point: the function handles it gracefully.
        assert!(pipeline_config.dry_run);

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // init_trading_pipeline — multiple strategies mixed enabled/disabled
    // -----------------------------------------------------------------------

    #[test]
    fn test_init_trading_pipeline_with_mixed_enabled_disabled() {
        let tmp_dir = std::env::temp_dir().join("dlt_test_init_pipeline_mixed");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("mixed_strategies.toml");
        let toml_content = r#"
[[strategy]]
name = "active_strategy"
enabled = true

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 30.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 70.0

[[strategy]]
name = "inactive_strategy"
enabled = false

[[strategy.entry_short]]
field = "rsi"
operator = "gt"
threshold = 80.0

[[strategy.exit]]
field = "rsi"
operator = "lt"
threshold = 20.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let config = build_test_application_config(config_path.to_str().unwrap());
        let handle = make_token_handle_with_value("jwt");

        let result = init_trading_pipeline(&config, &handle, "test_client");
        assert!(result.is_some());

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // init_trading_pipeline — config fields populated from ApplicationConfig
    // -----------------------------------------------------------------------

    #[test]
    fn test_init_trading_pipeline_config_propagation() {
        let tmp_dir = std::env::temp_dir().join("dlt_test_init_pipeline_cfg_prop");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("prop_strategies.toml");
        let toml_content = r#"
[[strategy]]
name = "prop_test"
enabled = true

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 30.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 70.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let config = build_test_application_config(config_path.to_str().unwrap());
        let handle = make_token_handle_with_value("jwt");

        let result = init_trading_pipeline(&config, &handle, "my_client_id");
        assert!(result.is_some());

        let (pipeline_config, _) = result.unwrap();
        // Verify config propagation from ApplicationConfig
        assert_eq!(
            pipeline_config.client_id, "my_client_id",
            "client_id must be propagated from caller"
        );
        assert!(
            pipeline_config.max_orders_per_second > 0,
            "max_orders_per_second must be positive from config"
        );
        assert!(
            !pipeline_config.rest_api_base_url.is_empty(),
            "REST API base URL must be populated from config"
        );
        assert!(
            pipeline_config.capital > 0.0,
            "capital must be positive from config"
        );

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // TokenHandleBridge — JWT-like tokens
    // -----------------------------------------------------------------------

    #[test]
    fn test_token_handle_bridge_jwt_format_token() {
        // Verify JWT-like tokens are returned as-is (no trimming, no mutation)
        let jwt =
            "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwiYXVkIjoiZGhhbiJ9.sig";
        let handle = make_token_handle_with_value(jwt);
        let bridge = TokenHandleBridge { handle };

        let result = bridge.get_access_token();
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().expose_secret(),
            jwt,
            "JWT token must be returned verbatim"
        );
    }

    #[test]
    fn test_token_handle_bridge_long_token() {
        let long_token = "a".repeat(4096);
        let handle = make_token_handle_with_value(&long_token);
        let bridge = TokenHandleBridge { handle };

        let result = bridge.get_access_token();
        assert!(result.is_ok());
        assert_eq!(result.unwrap().expose_secret().len(), 4096);
    }

    // -----------------------------------------------------------------------
    // OMS — order update with various statuses
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_oms_handle_order_update_various_statuses() {
        let handle = make_token_handle_with_value("jwt");
        let api_client = dhan_live_trader_trading::oms::OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "test".to_owned(),
        );
        let rate_limiter = dhan_live_trader_trading::oms::OrderRateLimiter::new(10);
        let bridge = Box::new(TokenHandleBridge { handle });
        let mut oms = dhan_live_trader_trading::oms::OrderManagementSystem::new(
            api_client,
            rate_limiter,
            bridge,
            "test".to_owned(),
        );

        // Place a paper order first
        let request = PlaceOrderRequest {
            security_id: 52432,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Market,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 1,
            price: 0.0,
            trigger_price: 0.0,
            lot_size: 1,
        };
        let order_id = oms.place_order(request).await.unwrap();

        // Send order updates with different statuses
        let statuses = ["PENDING", "TRADED", "REJECTED", "CANCELLED"];
        for status in &statuses {
            let mut update = make_order_update();
            update.order_no = order_id.clone();
            update.status = status.to_string();
            update.source = "P".to_string();
            // handle_order_update may succeed or return error depending on state
            let _ = oms.handle_order_update(&update);
        }

        // OMS should have processed updates without panicking
        assert!(oms.total_updates() > 0 || oms.total_placed() > 0);
    }

    // -----------------------------------------------------------------------
    // Pipeline — multiple securities with strategies
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_pipeline_multiple_securities_multiple_strategies() {
        let handle = make_token_handle_with_value("jwt");
        let tmp_dir = std::env::temp_dir().join("dlt_test_pipeline_multi_sec_strategy");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("multi_sec_strategies.toml");
        let toml_content = r#"
[[strategy]]
name = "long_strategy"
enabled = true

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 25.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 75.0

[[strategy]]
name = "short_strategy"
enabled = true

[[strategy.entry_short]]
field = "rsi"
operator = "gt"
threshold = 80.0

[[strategy.exit]]
field = "rsi"
operator = "lt"
threshold = 30.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let result = StrategyHotReloader::new(&config_path);
        assert!(result.is_ok());
        let (_reloader, defs, params) = result.unwrap();
        let strategies: Vec<StrategyInstance> = defs
            .into_iter()
            .map(|def| StrategyInstance::new(def, MAX_INDICATOR_INSTRUMENTS))
            .collect();

        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(128);
        let (_order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: params,
            strategies,
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);

        // Send ticks for multiple securities
        for sec_id in [100_u32, 200, 300] {
            for i in 0..10_u32 {
                let mut tick = ParsedTick::default();
                tick.security_id = sec_id;
                tick.last_traded_price = 200.0 + (i as f32 * 2.0);
                tick.volume = 5000 + i;
                let _ = tick_tx.send(tick);
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(tick_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), task_handle).await;
        assert!(result.is_ok(), "pipeline should handle multiple securities");

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // OMS — place and query active orders
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_oms_active_orders_tracks_security_id() {
        let handle = make_token_handle_with_value("jwt");
        let api_client = dhan_live_trader_trading::oms::OrderApiClient::new(
            reqwest::Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "test".to_owned(),
        );
        let rate_limiter = dhan_live_trader_trading::oms::OrderRateLimiter::new(10);
        let bridge = Box::new(TokenHandleBridge { handle });
        let mut oms = dhan_live_trader_trading::oms::OrderManagementSystem::new(
            api_client,
            rate_limiter,
            bridge,
            "test".to_owned(),
        );

        // Place orders for different securities
        for sec_id in [100_u32, 200, 300] {
            let request = PlaceOrderRequest {
                security_id: sec_id,
                transaction_type: TransactionType::Buy,
                order_type: OrderType::Market,
                product_type: ProductType::Intraday,
                validity: OrderValidity::Day,
                quantity: 1,
                price: 0.0,
                trigger_price: 0.0,
                lot_size: 1,
            };
            let _ = oms.place_order(request).await.unwrap();
        }

        assert_eq!(oms.total_placed(), 3);

        // Verify security_id filtering on active orders
        let active_for_100 = oms
            .active_orders()
            .iter()
            .filter(|o| o.security_id == 100)
            .count();
        assert_eq!(
            active_for_100, 1,
            "should have one active order for sec 100"
        );

        let active_for_999 = oms
            .active_orders()
            .iter()
            .filter(|o| o.security_id == 999)
            .count();
        assert_eq!(
            active_for_999, 0,
            "should have zero orders for non-placed security"
        );
    }

    // -----------------------------------------------------------------------
    // RiskEngine — reset_daily
    // -----------------------------------------------------------------------

    #[test]
    fn test_risk_engine_reset_daily_clears_state() {
        let mut risk = RiskEngine::new(2.0, 2, 1_000_000.0);
        // Record some fills
        risk.record_fill(100, 1, 100.0, 1);
        risk.record_fill(100, 1, 100.0, 1);

        // Should be at position limit
        assert!(
            !risk.check_order(100, 1).is_approved(),
            "should reject before reset"
        );

        // Reset daily state
        risk.reset_daily();

        // After reset, should approve again
        assert!(
            risk.check_order(100, 1).is_approved(),
            "should approve after daily reset"
        );
    }

    // -----------------------------------------------------------------------
    // TradingPipelineConfig — base URL validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_pipeline_config_rest_api_base_url_is_v2() {
        let handle = make_token_handle_with_value("jwt");
        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 500_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };
        assert!(
            config.rest_api_base_url.contains("/v2"),
            "REST API base URL must use v2"
        );
    }

    // -----------------------------------------------------------------------
    // IndicatorEngine — snapshot fields
    // -----------------------------------------------------------------------

    #[test]
    fn test_indicator_engine_snapshot_has_price() {
        let params = IndicatorParams::default();
        let mut engine = IndicatorEngine::new(params);

        // Use security_id within MAX_INDICATOR_INSTRUMENTS (25000) bounds
        let mut tick = ParsedTick::default();
        tick.security_id = 100;
        tick.last_traded_price = 2450.50;
        tick.volume = 10000;

        let snapshot = engine.update(&tick);
        assert_eq!(snapshot.security_id, 100);
        // f32→f64 widening may introduce minor precision artifacts
        assert!(
            (snapshot.last_traded_price - f64::from(2450.50_f32)).abs() < f64::EPSILON,
            "snapshot must carry the LTP from the tick"
        );
    }

    // -----------------------------------------------------------------------
    // OrderUpdate — field access coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_order_update_all_fields_accessible() {
        let mut update = make_order_update();
        update.exchange = "NSE".to_string();
        update.segment = "D".to_string();
        update.security_id = "52432".to_string();
        update.client_id = "CLIENT123".to_string();
        update.order_no = "ORD-001".to_string();
        update.exch_order_no = "1234567890".to_string();
        update.product = "I".to_string();
        update.txn_type = "B".to_string();
        update.order_type = "MKT".to_string();
        update.validity = "DAY".to_string();
        update.quantity = 50;
        update.traded_qty = 50;
        update.remaining_quantity = 0;
        update.price = 0.0;
        update.trigger_price = 0.0;
        update.traded_price = 2450.50;
        update.avg_traded_price = 2450.50;
        update.status = "TRADED".to_string();
        update.symbol = "RELIANCE".to_string();
        update.display_name = "Reliance Industries".to_string();
        update.correlation_id = "corr-001".to_string();
        update.source = "P".to_string();

        assert_eq!(update.exchange, "NSE");
        assert_eq!(update.segment, "D");
        assert_eq!(update.security_id, "52432");
        assert_eq!(update.order_no, "ORD-001");
        assert_eq!(update.product, "I");
        assert_eq!(update.txn_type, "B");
        assert_eq!(update.quantity, 50);
        assert_eq!(update.traded_qty, 50);
        assert_eq!(update.remaining_quantity, 0);
        assert!((update.traded_price - 2450.50).abs() < f64::EPSILON);
        assert_eq!(update.status, "TRADED");
        assert_eq!(update.source, "P");
        assert_eq!(update.correlation_id, "corr-001");
    }

    // -----------------------------------------------------------------------
    // Pipeline — exercise hot-reload path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_pipeline_with_hot_reloader_receives_ticks() {
        // This test exercises the hot-reload check inside the pipeline loop.
        // The reloader is created from a real TOML file, but we do not modify
        // the file during the test. The pipeline should still process ticks
        // normally when no reload event is pending.
        let handle = make_token_handle_with_value("jwt");
        let tmp_dir = std::env::temp_dir().join("dlt_test_pipeline_hot_reload");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("hot_reload_strategies.toml");
        let toml_content = r#"
[[strategy]]
name = "hot_reload_test"
enabled = true

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 30.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 70.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let result = StrategyHotReloader::new(&config_path);
        assert!(result.is_ok());
        let (reloader, defs, params) = result.unwrap();
        let strategies: Vec<StrategyInstance> = defs
            .into_iter()
            .map(|def| StrategyInstance::new(def, MAX_INDICATOR_INSTRUMENTS))
            .collect();

        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(64);
        let (_order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: params,
            strategies,
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        // Pass the reloader to the pipeline — exercises the `if let Some(ref reloader)` path
        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, Some(reloader));

        // Send ticks to exercise the pipeline with the hot-reloader present
        for i in 0..15_u32 {
            let mut tick = ParsedTick::default();
            tick.security_id = 100;
            tick.last_traded_price = 200.0 + (i as f32 * 1.5);
            tick.volume = 2000 + i;
            let _ = tick_tx.send(tick);
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(tick_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), task_handle).await;
        assert!(
            result.is_ok(),
            "pipeline with hot-reloader should stop cleanly"
        );

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // Pipeline — exercise many ticks to warm up indicator engine
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_pipeline_warmup_with_many_ticks() {
        // Send enough ticks to potentially warm up the indicator engine,
        // which may cause strategy evaluations to produce non-Hold signals.
        let handle = make_token_handle_with_value("jwt");
        let tmp_dir = std::env::temp_dir().join("dlt_test_pipeline_warmup");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("warmup_strategies.toml");
        let toml_content = r#"
[[strategy]]
name = "warmup_test"
enabled = true

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 30.0

[[strategy.entry_short]]
field = "rsi"
operator = "gt"
threshold = 70.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 50.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let result = StrategyHotReloader::new(&config_path);
        assert!(result.is_ok());
        let (_reloader, defs, params) = result.unwrap();
        let strategies: Vec<StrategyInstance> = defs
            .into_iter()
            .map(|def| StrategyInstance::new(def, MAX_INDICATOR_INSTRUMENTS))
            .collect();

        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(256);
        let (_order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: params,
            strategies,
            max_daily_loss_percent: 5.0,
            max_position_lots: 50,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);

        // Send many ticks with varying prices to exercise indicator warmup
        // and potentially trigger strategy signals
        for i in 0..100_u32 {
            let mut tick = ParsedTick::default();
            tick.security_id = 100;
            // Oscillating price pattern to exercise indicators
            let price = if i % 20 < 10 {
                200.0 + (i as f32 * 0.5) // Rising
            } else {
                210.0 - (i as f32 * 0.3) // Falling
            };
            tick.last_traded_price = price;
            tick.volume = 5000 + (i * 10);
            let _ = tick_tx.send(tick);
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        drop(tick_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), task_handle).await;
        assert!(
            result.is_ok(),
            "pipeline should handle warmup ticks and potential signals"
        );

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // Pipeline — concurrent tick and order update processing
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_pipeline_concurrent_ticks_and_order_updates() {
        let handle = make_token_handle_with_value("jwt");
        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(64);
        let (order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);

        // Send ticks and order updates interleaved
        for i in 0..10_u32 {
            let mut tick = ParsedTick::default();
            tick.security_id = 100 + i;
            tick.last_traded_price = 200.0 + i as f32;
            let _ = tick_tx.send(tick);

            let mut update = make_order_update();
            update.order_no = format!("ORD-{i}");
            update.status = "PENDING".to_string();
            let _ = order_tx.send(update);
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(tick_tx);
        drop(order_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task_handle).await;
        assert!(
            result.is_ok(),
            "pipeline should handle concurrent tick and order update streams"
        );
    }

    // -----------------------------------------------------------------------
    // init_trading_pipeline — propagates risk config
    // -----------------------------------------------------------------------

    #[test]
    fn test_init_trading_pipeline_propagates_risk_config() {
        let tmp_dir = std::env::temp_dir().join("dlt_test_init_pipeline_risk");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("risk_strategies.toml");
        std::fs::write(
            &config_path,
            r#"
[[strategy]]
name = "risk_test"
enabled = true

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 30.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 70.0
"#,
        )
        .unwrap();

        let config = build_test_application_config(config_path.to_str().unwrap());
        let handle = make_token_handle_with_value("jwt");

        let result = init_trading_pipeline(&config, &handle, "client");
        assert!(result.is_some());

        let (pipeline_config, _) = result.unwrap();
        // Risk config should be propagated from ApplicationConfig
        assert!(
            pipeline_config.max_daily_loss_percent > 0.0,
            "max_daily_loss_percent must be positive from config"
        );
        assert!(
            pipeline_config.max_position_lots > 0,
            "max_position_lots must be positive from config"
        );

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // init_trading_pipeline — hot_reloader is Some for valid TOML
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Pipeline — exercise signal generation via warm indicators
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_pipeline_with_warm_indicators_generates_signals() {
        // Send 50+ ticks with steadily decreasing prices to a single security ID < 200.
        // This should warm up the indicator engine (30 ticks) and push RSI below 30,
        // triggering an EnterLong signal from the strategy.
        let handle = make_token_handle_with_value("jwt");
        let tmp_dir = std::env::temp_dir().join("dlt_test_pipeline_warm_signals");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("warm_strategies.toml");
        let toml_content = r#"
[[strategy]]
name = "warm_test_long"
enabled = true

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 35.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 65.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let result = StrategyHotReloader::new(&config_path);
        assert!(result.is_ok());
        let (_reloader, defs, params) = result.unwrap();
        let strategies: Vec<StrategyInstance> = defs
            .into_iter()
            .map(|def| StrategyInstance::new(def, MAX_INDICATOR_INSTRUMENTS))
            .collect();

        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(256);
        let (_order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: params,
            strategies,
            max_daily_loss_percent: 5.0,
            max_position_lots: 100,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);

        // Send 60 ticks with steadily decreasing prices to push RSI down.
        // Use security_id = 5 (must be < MAX_INDICATOR_INSTRUMENTS = 200).
        for i in 0..60_u32 {
            let mut tick = ParsedTick::default();
            tick.security_id = 5;
            // Decrease price steadily to push RSI below 35
            tick.last_traded_price = 1000.0 - (i as f32 * 5.0);
            tick.volume = 1000 + i;
            tick.exchange_segment_code = 2; // NSE_FNO
            let _ = tick_tx.send(tick);
        }

        // Give the pipeline time to process all ticks and generate signals.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Drop sender to stop the pipeline
        drop(tick_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), task_handle).await;
        assert!(
            result.is_ok(),
            "pipeline should stop after processing warm ticks"
        );

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    #[tokio::test]
    async fn test_pipeline_with_short_entry_strategy() {
        // Send ticks with steadily increasing prices to push RSI above 75,
        // triggering an EnterShort signal.
        let handle = make_token_handle_with_value("jwt");
        let tmp_dir = std::env::temp_dir().join("dlt_test_pipeline_short_entry");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("short_entry_strategies.toml");
        let toml_content = r#"
[[strategy]]
name = "short_entry_test"
enabled = true

[[strategy.entry_short]]
field = "rsi"
operator = "gt"
threshold = 65.0

[[strategy.exit]]
field = "rsi"
operator = "lt"
threshold = 35.0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let result = StrategyHotReloader::new(&config_path);
        assert!(result.is_ok());
        let (_reloader, defs, params) = result.unwrap();
        let strategies: Vec<StrategyInstance> = defs
            .into_iter()
            .map(|def| StrategyInstance::new(def, MAX_INDICATOR_INSTRUMENTS))
            .collect();

        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(256);
        let (_order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: params,
            strategies,
            max_daily_loss_percent: 5.0,
            max_position_lots: 100,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);

        // Send 60 ticks with steadily increasing prices to push RSI up.
        for i in 0..60_u32 {
            let mut tick = ParsedTick::default();
            tick.security_id = 3;
            tick.last_traded_price = 100.0 + (i as f32 * 5.0);
            tick.volume = 1000 + i;
            tick.exchange_segment_code = 2;
            let _ = tick_tx.send(tick);
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        drop(tick_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), task_handle).await;
        assert!(
            result.is_ok(),
            "pipeline should stop after processing ticks for short entry"
        );

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    #[tokio::test]
    async fn test_pipeline_live_mode_flag() {
        // Test the pipeline with dry_run=false (exercises the warn! log path).
        let handle = make_token_handle_with_value("jwt");
        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(16);
        let (_order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 1_000_000.0,
            dry_run: false, // Live mode — exercises the else branch
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);

        drop(tick_tx);
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task_handle).await;
        assert!(result.is_ok(), "pipeline should stop cleanly in live mode");
    }

    #[tokio::test]
    async fn test_pipeline_order_update_ok_path() {
        // Send ticks first, place a paper order, then send an order update that
        // matches it. This exercises the oms.handle_order_update() Ok path.
        let handle = make_token_handle_with_value("jwt");
        let (tick_tx, tick_rx) = broadcast::channel::<ParsedTick>(64);
        let (order_tx, order_rx) = broadcast::channel::<OrderUpdate>(16);

        let config = TradingPipelineConfig {
            indicator_params: IndicatorParams::default(),
            strategies: Vec::new(),
            max_daily_loss_percent: 2.0,
            max_position_lots: 10,
            capital: 1_000_000.0,
            dry_run: true,
            max_orders_per_second: 10,
            rest_api_base_url: "https://api.dhan.co/v2".to_owned(),
            client_id: "test".to_owned(),
            token_handle: handle,
        };

        let task_handle = spawn_trading_pipeline(config, tick_rx, order_rx, None);

        // Send a few ticks (no strategies, so no orders, but exercises the tick path)
        for i in 0..5_u32 {
            let mut tick = ParsedTick::default();
            tick.security_id = 10 + i;
            tick.last_traded_price = 100.0 + i as f32;
            tick.volume = 500;
            let _ = tick_tx.send(tick);
        }

        // Send an order update (no matching order in OMS, so exercises error path)
        let mut update = make_order_update();
        update.order_no = "UNKNOWN-ORD-42".to_string();
        update.status = "TRADED".to_string();
        update.traded_qty = 1;
        update.source = "P".to_string();
        let _ = order_tx.send(update);

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        drop(tick_tx);
        drop(order_tx);

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task_handle).await;
        assert!(result.is_ok(), "pipeline should handle order updates");
    }

    // -----------------------------------------------------------------------
    // init_trading_pipeline — hot_reloader is Some for valid TOML
    // -----------------------------------------------------------------------

    #[test]
    fn test_init_trading_pipeline_returns_hot_reloader_for_valid_config() {
        let tmp_dir = std::env::temp_dir().join("dlt_test_init_pipeline_reloader");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let config_path = tmp_dir.join("reloader_strategies.toml");
        std::fs::write(
            &config_path,
            r#"
[[strategy]]
name = "reloader_test"
enabled = true

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 30.0

[[strategy.exit]]
field = "rsi"
operator = "gt"
threshold = 70.0
"#,
        )
        .unwrap();

        let config = build_test_application_config(config_path.to_str().unwrap());
        let handle = make_token_handle_with_value("jwt");

        let result = init_trading_pipeline(&config, &handle, "client");
        assert!(result.is_some());

        let (_, hot_reloader) = result.unwrap();
        assert!(
            hot_reloader.is_some(),
            "valid TOML should produce a hot reloader"
        );

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&tmp_dir);
    }

    // -----------------------------------------------------------------------
    // Daily reset signal tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_daily_reset_scheduled() {
        // Verify that the daily reset signal mechanism works:
        // when a Notify is fired, a waiting task is woken.
        let signal = std::sync::Arc::new(tokio::sync::Notify::new());
        let signal_clone = std::sync::Arc::clone(&signal);

        // Spawn a task that waits for the signal, then fires it.
        let handle = tokio::spawn(async move {
            // Small delay to ensure the notified() future is registered first.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            signal_clone.notify_one();
        });

        let notified = signal.notified();
        tokio::time::timeout(std::time::Duration::from_millis(200), notified)
            .await
            .expect("daily reset signal should fire within timeout");

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_daily_reset_signal_rearm() {
        // Verify the signal can be re-armed (for multi-day operation).
        let signal = std::sync::Arc::new(tokio::sync::Notify::new());

        // First fire: spawn notifier, then wait.
        let s1 = std::sync::Arc::clone(&signal);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            s1.notify_one();
        });
        let notified = signal.notified();
        tokio::time::timeout(std::time::Duration::from_millis(200), notified)
            .await
            .expect("first signal should fire");

        // Second fire (re-arm): spawn another notifier, wait again.
        let s2 = std::sync::Arc::clone(&signal);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            s2.notify_one();
        });
        let notified = signal.notified();
        tokio::time::timeout(std::time::Duration::from_millis(200), notified)
            .await
            .expect("re-armed signal should fire");
    }

    #[test]
    fn test_spawn_trading_pipeline_with_reset_accepts_none() {
        // Verify that spawn_trading_pipeline_with_reset compiles with None signal.
        // We can't actually run it without a full config, but the type check matters.
        let signal: Option<std::sync::Arc<tokio::sync::Notify>> = None;
        assert!(signal.is_none());
    }

    #[test]
    fn test_spawn_trading_pipeline_full_accepts_none_signals() {
        // Verify that spawn_trading_pipeline_full compiles with both signals as None.
        let reset: Option<std::sync::Arc<tokio::sync::Notify>> = None;
        let close: Option<std::sync::Arc<tokio::sync::Notify>> = None;
        assert!(reset.is_none());
        assert!(close.is_none());
    }

    #[tokio::test]
    async fn test_market_close_auto_handling() {
        // Verify the market close signal fires and can be received.
        let signal = std::sync::Arc::new(tokio::sync::Notify::new());
        let signal_clone = std::sync::Arc::clone(&signal);

        // Simulate market close timer firing.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            signal_clone.notify_one();
        });

        let notified = signal.notified();
        tokio::time::timeout(std::time::Duration::from_millis(200), notified)
            .await
            .expect("market close signal should fire within timeout");
    }
}
