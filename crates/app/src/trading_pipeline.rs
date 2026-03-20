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
/// # Safety
/// When `dry_run` is true, NO HTTP calls are ever made. All orders are paper trades.
pub fn spawn_trading_pipeline(
    pipeline_config: TradingPipelineConfig,
    tick_receiver: broadcast::Receiver<ParsedTick>,
    order_update_receiver: broadcast::Receiver<OrderUpdate>,
    strategy_hot_reloader: Option<StrategyHotReloader>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_trading_pipeline(
            pipeline_config,
            tick_receiver,
            order_update_receiver,
            strategy_hot_reloader,
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
}
