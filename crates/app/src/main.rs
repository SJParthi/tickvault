//! Binary entry point for the dhan-live-trader application.
//!
//! Orchestrates the complete boot sequence:
//! Config → Observability → Logging → Notification → Auth → Persist → Universe → WebSocket → Pipeline → HTTP → Shutdown
//!
//! # Boot Sequence
//! 1. Load and validate configuration from `config/base.toml`
//! 2. Initialize observability (Prometheus metrics + OpenTelemetry tracing)
//! 3. Initialize structured logging with OpenTelemetry layer
//! 4. Initialize Telegram notification service (best-effort)
//! 5. Authenticate with Dhan API (SSM → TOTP → JWT)
//! 6. Set up QuestDB tick persistence (best-effort)
//! 7. Build F&O universe + subscription plan from instrument CSV
//! 8. Build WebSocket connection pool with planned instruments
//! 9. Spawn tick processing pipeline (pure capture — parse → filter → persist)
//! 10. Start axum API server (health, stats, portal)
//! 11. Spawn token renewal background task
//! 12. Await shutdown signal (Ctrl+C)

mod observability;

use std::net::SocketAddr;

use anyhow::{Context, Result};
use chrono::{FixedOffset, Utc};
use figment::Figment;
use figment::providers::{Format, Toml};
use secrecy::ExposeSecret;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use dhan_live_trader_common::config::ApplicationConfig;
use dhan_live_trader_common::constants::IST_UTC_OFFSET_SECONDS;
use dhan_live_trader_core::auth::secret_manager;
use dhan_live_trader_core::auth::token_manager::TokenManager;
use dhan_live_trader_core::instrument::{
    InstrumentLoadResult, build_subscription_plan, load_or_build_instruments,
    run_instrument_diagnostic,
};
use dhan_live_trader_core::notification::{NotificationEvent, NotificationService};
use dhan_live_trader_core::pipeline::run_tick_processor;
use dhan_live_trader_core::websocket::connection_pool::WebSocketConnectionPool;
use dhan_live_trader_core::websocket::types::InstrumentSubscription;

use dhan_live_trader_storage::instrument_persistence::ensure_instrument_tables;
use dhan_live_trader_storage::tick_persistence::{
    DepthPersistenceWriter, TickPersistenceWriter, ensure_depth_and_prev_close_tables,
    ensure_tick_table_dedup_keys,
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
    // Step 2: Initialize observability (Prometheus metrics exporter)
    // -----------------------------------------------------------------------
    observability::init_metrics(&config.observability)
        .context("failed to initialize Prometheus metrics")?;

    // -----------------------------------------------------------------------
    // Step 3: Initialize structured logging + OpenTelemetry tracing layer
    // -----------------------------------------------------------------------
    let log_filter = config.logging.level.as_str();
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_filter));

    let (otel_layer, otel_provider) = match observability::init_tracing(&config.observability)
        .context("failed to initialize OpenTelemetry tracing")?
    {
        Some((layer, provider)) => (Some(layer), Some(provider)),
        None => (None, None),
    };

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_thread_ids(true)
        .with_file(false)
        .with_line_number(false);

    // Box the fmt layer so both JSON and text branches produce the same type,
    // allowing a single subscriber chain (required for OpenTelemetryLayer<S> inference).
    let fmt_boxed: Box<dyn tracing_subscriber::Layer<_> + Send + Sync + 'static> =
        if config.logging.format == "json" {
            Box::new(fmt_layer.json())
        } else {
            Box::new(fmt_layer)
        };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_boxed)
        .with(otel_layer)
        .init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config_file = CONFIG_BASE_PATH,
        metrics_port = config.observability.metrics_port,
        tracing_enabled = config.observability.tracing_enabled,
        "dhan-live-trader starting"
    );

    // -----------------------------------------------------------------------
    // CLI: --instrument-diagnostic (run diagnostic and exit)
    // -----------------------------------------------------------------------
    if std::env::args().any(|arg| arg == "--instrument-diagnostic") {
        info!("running instrument diagnostic (--instrument-diagnostic flag detected)");
        let report = run_instrument_diagnostic(
            &config.dhan.instrument_csv_url,
            &config.dhan.instrument_csv_fallback_url,
            &config.instrument,
        )
        .await;

        let json = serde_json::to_string_pretty(&report)
            .unwrap_or_else(|err| format!("{{\"error\": \"serialization failed: {err}\"}}"));
        #[allow(clippy::print_stdout)] // APPROVED: CLI diagnostic output to stdout, not logging
        {
            println!("{json}"); // APPROVED: CLI diagnostic requires stdout output
        }

        if report.healthy {
            info!("instrument diagnostic: ALL CHECKS PASSED");
        } else {
            let failed: Vec<_> = report
                .checks
                .iter()
                .filter(|c| !c.passed)
                .map(|c| c.name.as_str())
                .collect();
            error!(
                failed_checks = ?failed,
                "instrument diagnostic: SOME CHECKS FAILED"
            );
            std::process::exit(1);
        }
        return Ok(());
    }

    // -----------------------------------------------------------------------
    // Step 4: Initialize notification service (best-effort — no-op if SSM unavailable)
    // -----------------------------------------------------------------------
    info!("initializing Telegram notification service");
    let notifier = NotificationService::initialize(&config.notification).await;

    // -----------------------------------------------------------------------
    // Step 5: Authenticate with Dhan API (infinite retry for transient errors)
    // -----------------------------------------------------------------------
    info!("authenticating with Dhan API via SSM → TOTP → JWT");

    let token_init_timeout =
        std::time::Duration::from_secs(dhan_live_trader_common::constants::TOKEN_INIT_TIMEOUT_SECS);
    let token_manager = match tokio::time::timeout(
        token_init_timeout,
        TokenManager::initialize(&config.dhan, &config.token, &config.network, &notifier),
    )
    .await
    {
        Ok(Ok(manager)) => manager,
        Ok(Err(err)) => {
            // Permanent auth error or Ctrl+C.
            error!(error = %err, "authentication failed permanently — exiting");
            notifier.notify(
                dhan_live_trader_core::notification::events::NotificationEvent::AuthenticationFailed {
                    reason: format!("PERMANENT: {err}"),
                },
            );
            return Ok(());
        }
        Err(_elapsed) => {
            error!(
                timeout_secs = dhan_live_trader_common::constants::TOKEN_INIT_TIMEOUT_SECS,
                "authentication timed out — Dhan API may be unreachable"
            );
            notifier.notify(
                dhan_live_trader_core::notification::events::NotificationEvent::AuthenticationFailed {
                    reason: format!(
                        "TIMEOUT: initial auth did not complete within {}s — check Dhan API and network",
                        dhan_live_trader_common::constants::TOKEN_INIT_TIMEOUT_SECS,
                    ),
                },
            );
            return Ok(());
        }
    };

    // -----------------------------------------------------------------------
    // Step 6: Set up QuestDB tick persistence (best-effort)
    // -----------------------------------------------------------------------
    info!("setting up QuestDB tables (ticks + instruments + depth + previous_close)");

    ensure_tick_table_dedup_keys(&config.questdb).await;
    ensure_depth_and_prev_close_tables(&config.questdb).await;
    ensure_instrument_tables(&config.questdb).await;

    let tick_writer = match TickPersistenceWriter::new(&config.questdb) {
        Ok(writer) => {
            info!("QuestDB tick writer connected");
            Some(writer)
        }
        Err(err) => {
            error!(
                ?err,
                "QuestDB tick writer unavailable — ticks will NOT be persisted"
            );
            notifier.notify(
                dhan_live_trader_core::notification::events::NotificationEvent::Custom {
                    message: format!(
                        "<b>QuestDB UNAVAILABLE</b>\nTick writer failed: {err}\nTicks will NOT be persisted until restart."
                    ),
                },
            );
            None
        }
    };

    let depth_writer = match DepthPersistenceWriter::new(&config.questdb) {
        Ok(writer) => {
            info!("QuestDB depth writer connected");
            Some(writer)
        }
        Err(err) => {
            error!(
                ?err,
                "QuestDB depth writer unavailable — market depth will NOT be persisted"
            );
            None
        }
    };

    // -----------------------------------------------------------------------
    // Step 7: Load or build instruments (three-layer defense)
    // -----------------------------------------------------------------------
    info!("checking instrument build eligibility");

    let subscription_plan = match load_or_build_instruments(
        &config.dhan.instrument_csv_url,
        &config.dhan.instrument_csv_fallback_url,
        &config.instrument,
        &config.questdb,
        &config.subscription,
    )
    .await
    {
        Ok(InstrumentLoadResult::FreshBuild(universe)) => {
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
                "subscription plan ready (fresh build)"
            );

            notifier.notify(NotificationEvent::InstrumentBuildSuccess {
                source: universe.build_metadata.csv_source,
                derivative_count: universe.derivative_contracts.len(),
                underlying_count: universe.underlyings.len(),
            });
            Some(plan)
        }
        Ok(InstrumentLoadResult::CachedPlan(plan)) => {
            info!(
                major_indices = plan.summary.major_index_values,
                display_indices = plan.summary.display_indices,
                index_derivatives = plan.summary.index_derivatives,
                stock_equities = plan.summary.stock_equities,
                stock_derivatives = plan.summary.stock_derivatives,
                total = plan.summary.total,
                feed_mode = %plan.summary.feed_mode,
                "subscription plan ready (zero-copy rkyv cache)"
            );
            Some(plan)
        }
        Ok(InstrumentLoadResult::Unavailable) => {
            info!("instruments: no cache available during market hours");
            None
        }
        Err(err) => {
            let trigger_url = format!(
                "http://{}:{}/api/instruments/rebuild",
                config.api.host, config.api.port
            );
            warn!(
                error = %err,
                "instrument build failed — no instruments to subscribe"
            );
            notifier.notify(NotificationEvent::InstrumentBuildFailed {
                reason: err.to_string(),
                manual_trigger_url: trigger_url,
            });
            None
        }
    };

    // -----------------------------------------------------------------------
    // Step 8: Build WebSocket connection pool (only if authenticated + plan ready)
    // -----------------------------------------------------------------------
    let (frame_receiver, ws_handles) = if let Some(plan) = &subscription_plan {
        let tm = &token_manager;
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
        let handles = pool.spawn_all().await;

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
    // Step 9: Spawn tick processor (pure capture — parse → filter → persist)
    // -----------------------------------------------------------------------
    let processor_handle = if let Some(receiver) = frame_receiver {
        let handle = tokio::spawn(async move {
            run_tick_processor(receiver, tick_writer, depth_writer).await;
        });
        info!("tick processor started");
        Some(handle)
    } else {
        info!("tick processor skipped — no frame source available");
        None
    };

    // -----------------------------------------------------------------------
    // Step 10: Start axum API server
    // -----------------------------------------------------------------------
    let api_state = SharedAppState::new(
        config.questdb.clone(),
        config.dhan.clone(),
        config.instrument.clone(),
    );

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
    // Step 11: Spawn token renewal background task
    // -----------------------------------------------------------------------
    let renewal_handle = token_manager.spawn_renewal_task();
    info!("token renewal task started");

    // -----------------------------------------------------------------------
    // Step 12: Await shutdown signal
    // -----------------------------------------------------------------------
    let mode = "LIVE";
    info!(
        mode,
        "system ready — press Ctrl+C to stop\n\
         \n\
           Health:     http://{bind_addr}/health\n\
           Stats:      http://{bind_addr}/api/stats\n\
           Portal:     http://{bind_addr}/portal\n\
           Rebuild:    POST http://{bind_addr}/api/instruments/rebuild\n\
         \n\
           Grafana:    http://localhost:3000\n\
           Prometheus: http://localhost:9090\n\
           Jaeger:     http://localhost:16686\n\
           QuestDB:    http://localhost:9000\n"
    );

    notifier.notify(NotificationEvent::StartupComplete { mode });

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for shutdown signal")?;

    info!("shutdown signal received — stopping gracefully");
    notifier.notify(NotificationEvent::ShutdownInitiated);

    // Spawn a second signal listener: if the operator sends another Ctrl+C
    // during shutdown, exit immediately instead of hanging.
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        warn!("second shutdown signal received — forcing immediate exit");
        std::process::exit(1);
    });

    // 1. Stop token renewal (no dependencies).
    renewal_handle.abort();

    // 2. Abort WebSocket connections. This drops all frame_sender handles,
    //    which causes the tick processor's recv() to return None and exit
    //    its loop — triggering the final QuestDB flush.
    for handle in ws_handles {
        handle.abort();
    }

    // 3. Wait for the tick processor to finish its final flush (bounded).
    if let Some(handle) = processor_handle {
        let shutdown_timeout = std::time::Duration::from_secs(
            dhan_live_trader_common::constants::GRACEFUL_SHUTDOWN_TIMEOUT_SECS,
        );
        match tokio::time::timeout(shutdown_timeout, handle).await {
            Ok(_) => info!("tick processor shut down gracefully"),
            Err(_) => warn!("tick processor shutdown timed out — aborting"),
        }
    }

    // 4. Stop API server.
    api_handle.abort();

    // 5. Flush pending OpenTelemetry spans before exit (Drop triggers batch flush).
    drop(otel_provider);

    info!("dhan-live-trader stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn config_base_path_is_toml() {
        assert!(
            CONFIG_BASE_PATH.ends_with(".toml"),
            "config path must be a TOML file"
        );
    }

    #[test]
    fn config_local_path_is_toml() {
        assert!(
            CONFIG_LOCAL_PATH.ends_with(".toml"),
            "local config path must be a TOML file"
        );
    }

    #[test]
    fn socket_addr_parses_valid_host_port() {
        let addr: Result<SocketAddr, _> = "0.0.0.0:8080".parse();
        assert!(addr.is_ok(), "valid host:port must parse");
    }

    #[test]
    fn socket_addr_rejects_invalid() {
        let addr: Result<SocketAddr, _> = "not_a_socket".parse();
        assert!(addr.is_err(), "invalid address must fail");
    }
}
