//! Binary entry point for the dhan-live-trader application.
//!
//! Orchestrates the complete boot sequence:
//! Config → Auth → WebSocket → Parse → Candle → Persist → HTTP → Shutdown
//!
//! # Boot Sequence
//! 1. Load and validate configuration from `config/base.toml`
//! 2. Initialize structured logging (tracing)
//! 3. Authenticate with Dhan API (SSM → TOTP → JWT)
//! 4. Set up QuestDB tick persistence (best-effort)
//! 5. Build WebSocket connection pool with subscribed instruments
//! 6. Spawn tick processing pipeline
//! 7. Start axum API server with TradingView frontend
//! 8. Spawn token renewal background task
//! 9. Await shutdown signal (Ctrl+C)

use std::net::SocketAddr;

use anyhow::{Context, Result};
use figment::Figment;
use figment::providers::{Format, Toml};
use secrecy::ExposeSecret;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use dhan_live_trader_common::config::ApplicationConfig;
use dhan_live_trader_common::tick_types::{CandleBroadcastMessage, TickInterval, Timeframe};
use dhan_live_trader_common::types::{ExchangeSegment, FeedMode};

use dhan_live_trader_core::auth::secret_manager;
use dhan_live_trader_core::auth::token_manager::TokenManager;
use dhan_live_trader_core::pipeline::run_tick_processor;
use dhan_live_trader_core::websocket::connection_pool::WebSocketConnectionPool;
use dhan_live_trader_core::websocket::types::InstrumentSubscription;

use dhan_live_trader_storage::tick_persistence::{
    TickPersistenceWriter, ensure_tick_table_dedup_keys,
};

use dhan_live_trader_api::build_router;
use dhan_live_trader_api::state::SharedAppState;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Base config file path (relative to working directory).
const CONFIG_BASE_PATH: &str = "config/base.toml";

/// Local override config file path (git-ignored, optional).
/// Overrides Docker hostnames with localhost for `cargo run` on host.
const CONFIG_LOCAL_PATH: &str = "config/local.toml";

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // -----------------------------------------------------------------------
    // Step 0: Install rustls CryptoProvider (must happen before ANY TLS usage)
    // -----------------------------------------------------------------------
    // rustls 0.23+ requires an explicit CryptoProvider. Both tokio-tungstenite
    // (WSS to Dhan) and reqwest (HTTPS to Dhan REST) depend on rustls.
    // Using aws-lc-rs as the provider (already in the dependency tree).
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls CryptoProvider — cannot proceed without TLS");

    // -----------------------------------------------------------------------
    // Step 1: Load and validate configuration
    // -----------------------------------------------------------------------
    let config: ApplicationConfig = Figment::new()
        .merge(Toml::file(CONFIG_BASE_PATH))
        .merge(Toml::file(CONFIG_LOCAL_PATH))
        .extract()
        .context("failed to load configuration from config/base.toml")?;

    config
        .validate()
        .context("configuration validation failed")?;

    // -----------------------------------------------------------------------
    // Step 2: Initialize structured logging
    // -----------------------------------------------------------------------
    let log_filter = config.logging.level.as_str();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_filter)),
        )
        .with_target(true)
        .with_thread_ids(true)
        .with_file(false)
        .with_line_number(false);

    if config.logging.format == "json" {
        subscriber.json().init();
    } else {
        subscriber.init();
    }

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config_file = CONFIG_BASE_PATH,
        "dhan-live-trader starting"
    );

    // -----------------------------------------------------------------------
    // Step 3: Authenticate with Dhan API (best-effort for development)
    // -----------------------------------------------------------------------
    info!("authenticating with Dhan API via SSM → TOTP → JWT");

    let token_manager =
        match TokenManager::initialize(&config.dhan, &config.token, &config.network).await {
            Ok(manager) => {
                info!("authentication successful — token acquired");
                Some(manager)
            }
            Err(err) => {
                warn!(
                    error = %err,
                    "authentication failed — starting in offline mode (no live data)"
                );
                None
            }
        };

    // -----------------------------------------------------------------------
    // Step 4: Set up QuestDB tick persistence (best-effort)
    // -----------------------------------------------------------------------
    info!("setting up QuestDB tick persistence");

    ensure_tick_table_dedup_keys(&config.questdb).await;

    let tick_writer = match TickPersistenceWriter::new(&config.questdb) {
        Ok(writer) => {
            info!("QuestDB tick writer connected");
            Some(writer)
        }
        Err(err) => {
            warn!(
                ?err,
                "QuestDB tick writer unavailable — ticks will not be persisted"
            );
            None
        }
    };

    // -----------------------------------------------------------------------
    // Step 5: Build WebSocket connection pool (only if authenticated)
    // -----------------------------------------------------------------------
    let (frame_receiver, ws_handles) = if let Some(ref tm) = token_manager {
        info!("building WebSocket connection pool");

        let token_handle = tm.token_handle();
        let client_id = {
            let credentials = secret_manager::fetch_dhan_credentials()
                .await
                .context("failed to fetch Dhan client ID for WebSocket")?;
            credentials.client_id.expose_secret().to_string()
        };

        // For MVP: subscribe to the 5 core index instruments.
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::IdxI, 13), // NIFTY 50
            InstrumentSubscription::new(ExchangeSegment::IdxI, 25), // BANK NIFTY
            InstrumentSubscription::new(ExchangeSegment::IdxI, 27), // FINNIFTY
            InstrumentSubscription::new(ExchangeSegment::IdxI, 442), // MIDCPNIFTY
            InstrumentSubscription::new(ExchangeSegment::IdxI, 21), // INDIA VIX
        ];

        let instrument_count = instruments.len();

        let mut pool = WebSocketConnectionPool::new(
            token_handle,
            client_id,
            config.dhan.clone(),
            config.websocket.clone(),
            instruments,
            FeedMode::Full,
        )
        .context("failed to create WebSocket connection pool")?;

        let receiver = pool.take_frame_receiver();
        let handles = pool.spawn_all();

        info!(
            connections = handles.len(),
            instruments = instrument_count,
            "WebSocket pool started"
        );

        (Some(receiver), handles)
    } else {
        warn!("WebSocket pool skipped — running in offline mode");
        (None, Vec::new())
    };

    // -----------------------------------------------------------------------
    // Step 6: Create candle broadcast channel + spawn tick processor
    // -----------------------------------------------------------------------
    let (candle_tx, _candle_rx) =
        broadcast::channel::<CandleBroadcastMessage>(config.pipeline.candle_broadcast_capacity);

    let candle_tx_clone = candle_tx.clone();

    // Use all standard timeframes + tick intervals
    let timeframes: Vec<Timeframe> = Timeframe::all_standard().to_vec();
    let tick_intervals: Vec<TickInterval> = TickInterval::all_standard().to_vec();

    let processor_handle = if let Some(receiver) = frame_receiver {
        let handle = tokio::spawn(async move {
            run_tick_processor(
                receiver,
                candle_tx_clone,
                tick_writer,
                &timeframes,
                &tick_intervals,
            )
            .await;
        });

        info!(
            time_intervals = Timeframe::all_standard().len(),
            tick_intervals = TickInterval::all_standard().len(),
            "tick processor started"
        );
        Some(handle)
    } else {
        info!("tick processor skipped — no frame source available");
        None
    };

    // -----------------------------------------------------------------------
    // Step 7: Start axum API server
    // -----------------------------------------------------------------------
    let api_state = SharedAppState::new(candle_tx, config.questdb.clone());

    let router = build_router(api_state);

    let bind_addr: SocketAddr = format!("{}:{}", config.api.host, config.api.port)
        .parse()
        .context("invalid API bind address")?;

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .context("failed to bind API server")?;

    info!(
        address = %bind_addr,
        "API server listening — open http://{} in browser",
        bind_addr
    );

    let api_handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router).await {
            error!(?err, "API server error");
        }
    });

    // -----------------------------------------------------------------------
    // Step 8: Spawn token renewal background task (only if authenticated)
    // -----------------------------------------------------------------------
    let renewal_handle = if let Some(ref tm) = token_manager {
        let handle = tm.spawn_renewal_task();
        info!("token renewal task started");
        Some(handle)
    } else {
        None
    };

    // -----------------------------------------------------------------------
    // Step 9: Await shutdown signal
    // -----------------------------------------------------------------------
    let mode = if token_manager.is_some() {
        "LIVE"
    } else {
        "OFFLINE"
    };
    info!(
        mode,
        "system ready — press Ctrl+C to stop\n\
         \n\
           📊 TradingView chart:  http://{bind_addr}\n\
           🔌 WebSocket:          ws://{bind_addr}/ws/live\n\
           ❤️  Health:             http://{bind_addr}/health\n\
           📈 Intervals:          http://{bind_addr}/api/intervals\n"
    );

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for shutdown signal")?;

    info!("shutdown signal received — stopping gracefully");

    // Cancel background tasks
    if let Some(handle) = renewal_handle {
        handle.abort();
    }
    if let Some(handle) = processor_handle {
        handle.abort();
    }
    api_handle.abort();

    // Wait briefly for clean shutdown
    for handle in ws_handles {
        handle.abort();
    }

    info!("dhan-live-trader stopped");
    Ok(())
}
