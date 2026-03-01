//! Binary entry point for the dhan-live-trader application.
//!
//! Orchestrates the complete boot sequence:
//! Config → Logging → Notification → Auth → Persist → Universe → WebSocket → Pipeline → HTTP → Shutdown
//!
//! # Boot Sequence
//! 1. Load and validate configuration from `config/base.toml`
//! 2. Initialize structured logging (tracing)
//! 3. Initialize Telegram notification service (best-effort)
//! 4. Authenticate with Dhan API (SSM → TOTP → JWT)
//! 5. Set up QuestDB tick persistence (best-effort)
//! 6. Build F&O universe + subscription plan from instrument CSV
//! 7. Build WebSocket connection pool with planned instruments
//! 8. Spawn tick processing pipeline (pure capture — parse → filter → persist)
//! 9. Start axum API server (health, stats, portal)
//! 10. Spawn token renewal background task
//! 11. Await shutdown signal (Ctrl+C)

use std::net::SocketAddr;

use anyhow::{Context, Result};
use chrono::{FixedOffset, Utc};
use figment::Figment;
use figment::providers::{Format, Toml};
use secrecy::ExposeSecret;
use tracing::{error, info, warn};

use dhan_live_trader_common::config::ApplicationConfig;
use dhan_live_trader_common::constants::IST_UTC_OFFSET_SECONDS;
use dhan_live_trader_core::auth::secret_manager;
use dhan_live_trader_core::auth::token_manager::TokenManager;
use dhan_live_trader_core::instrument::{build_fno_universe, build_subscription_plan};
use dhan_live_trader_core::notification::{NotificationEvent, NotificationService};
use dhan_live_trader_core::pipeline::run_tick_processor;
use dhan_live_trader_core::websocket::connection_pool::WebSocketConnectionPool;
use dhan_live_trader_core::websocket::types::InstrumentSubscription;

use dhan_live_trader_storage::instrument_persistence::ensure_instrument_tables;
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
        .expect("failed to install rustls CryptoProvider — cannot proceed without TLS"); // APPROVED: bootstrap — TLS mandatory, failure is fatal

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
    // Step 3: Initialize notification service (best-effort — no-op if SSM unavailable)
    // -----------------------------------------------------------------------
    info!("initializing Telegram notification service");
    let notifier = NotificationService::initialize(&config.notification).await;

    // -----------------------------------------------------------------------
    // Step 4: Authenticate with Dhan API (best-effort for development)
    // -----------------------------------------------------------------------
    info!("authenticating with Dhan API via SSM → TOTP → JWT");

    let token_manager =
        match TokenManager::initialize(&config.dhan, &config.token, &config.network).await {
            Ok(manager) => {
                info!("authentication successful — token acquired");
                notifier.notify(NotificationEvent::AuthenticationSuccess);
                Some(manager)
            }
            Err(err) => {
                warn!(
                    error = %err,
                    "authentication failed — starting in offline mode (no live data)"
                );
                notifier.notify(NotificationEvent::AuthenticationFailed {
                    reason: err.to_string(),
                });
                None
            }
        };

    // -----------------------------------------------------------------------
    // Step 5: Set up QuestDB tick persistence (best-effort)
    // -----------------------------------------------------------------------
    info!("setting up QuestDB tables (ticks + instruments)");

    ensure_tick_table_dedup_keys(&config.questdb).await;
    ensure_instrument_tables(&config.questdb).await;

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
    // Step 6: Build F&O universe + subscription plan (best-effort)
    // -----------------------------------------------------------------------
    info!("building F&O universe from instrument CSV");

    let subscription_plan = match build_fno_universe(
        &config.dhan.instrument_csv_url,
        &config.dhan.instrument_csv_fallback_url,
        &config.instrument,
    )
    .await
    {
        Ok(universe) => {
            let ist = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS)
                .context("invalid IST offset constant")?;
            let today = Utc::now().with_timezone(&ist).date_naive();
            let plan = build_subscription_plan(&universe, &config.subscription, today);

            info!(
                major_indices = plan.summary.major_index_values,
                display_indices = plan.summary.display_indices,
                index_derivatives = plan.summary.index_derivatives,
                stock_equities = plan.summary.stock_equities,
                stock_derivatives = plan.summary.stock_derivatives,
                total = plan.summary.total,
                feed_mode = %plan.summary.feed_mode,
                "subscription plan ready"
            );
            Some(plan)
        }
        Err(err) => {
            warn!(
                error = %err,
                "F&O universe build failed — no instruments to subscribe"
            );
            None
        }
    };

    // -----------------------------------------------------------------------
    // Step 7: Build WebSocket connection pool (only if authenticated + plan ready)
    // -----------------------------------------------------------------------
    let (frame_receiver, ws_handles) =
        if let (Some(tm), Some(plan)) = (&token_manager, &subscription_plan) {
            info!("building WebSocket connection pool");

            let token_handle = tm.token_handle();
            let client_id = {
                let credentials = secret_manager::fetch_dhan_credentials()
                    .await
                    .context("failed to fetch Dhan client ID for WebSocket")?;
                credentials.client_id.expose_secret().to_string()
            };

            // Convert registry instruments → WebSocket subscription entries
            let instruments: Vec<InstrumentSubscription> = plan
                .registry
                .iter()
                .map(|inst| InstrumentSubscription::new(inst.exchange_segment, inst.security_id))
                .collect();

            let feed_mode = plan.summary.feed_mode;
            let instrument_count = instruments.len();

            let mut pool = WebSocketConnectionPool::new(
                token_handle,
                client_id,
                config.dhan.clone(),
                config.websocket.clone(),
                instruments,
                feed_mode,
            )
            .context("failed to create WebSocket connection pool")?;

            let receiver = pool.take_frame_receiver();
            let handles = pool.spawn_all();

            info!(
                connections = handles.len(),
                instruments = instrument_count,
                feed_mode = %feed_mode,
                "WebSocket pool started"
            );

            (Some(receiver), handles)
        } else {
            warn!("WebSocket pool skipped — running in offline mode");
            (None, Vec::new())
        };

    // -----------------------------------------------------------------------
    // Step 8: Spawn tick processor (pure capture — parse → filter → persist)
    // -----------------------------------------------------------------------
    let processor_handle = if let Some(receiver) = frame_receiver {
        let handle = tokio::spawn(async move {
            run_tick_processor(receiver, tick_writer).await;
        });
        info!("tick processor started");
        Some(handle)
    } else {
        info!("tick processor skipped — no frame source available");
        None
    };

    // -----------------------------------------------------------------------
    // Step 9: Start axum API server
    // -----------------------------------------------------------------------
    let api_state = SharedAppState::new(config.questdb.clone());

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
    // Step 10: Spawn token renewal background task (only if authenticated)
    // -----------------------------------------------------------------------
    let renewal_handle = if let Some(ref tm) = token_manager {
        let handle = tm.spawn_renewal_task();
        info!("token renewal task started");
        Some(handle)
    } else {
        None
    };

    // -----------------------------------------------------------------------
    // Step 11: Await shutdown signal
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
           Health:  http://{bind_addr}/health\n\
           Stats:   http://{bind_addr}/api/stats\n\
           Portal:  http://{bind_addr}/portal\n"
    );

    notifier.notify(NotificationEvent::StartupComplete { mode });

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for shutdown signal")?;

    info!("shutdown signal received — stopping gracefully");
    notifier.notify(NotificationEvent::ShutdownInitiated);

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
