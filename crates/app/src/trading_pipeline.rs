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
