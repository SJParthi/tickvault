//! Binary entry point for the dhan-live-trader application.
//!
//! Orchestrates the complete boot sequence:
//! Config → Observability → Logging → Notification → IP Verify → Auth → Persist → Universe → WebSocket → Pipeline → OrderUpdate → HTTP → Shutdown
//!
//! # Boot Sequence (optimized for fast restart)
//!
//! Steps 4+5 and QuestDB DDL queries run in parallel. Token cache skips
//! the Dhan HTTP auth call on crash recovery (~500ms-2s saved).
//!
//! 1. Load and validate configuration
//! 2. Initialize observability (Prometheus metrics + OpenTelemetry tracing)
//! 3. Initialize structured logging with OpenTelemetry layer
//! 4. (parallel) Notification service + Docker infra check
//! 5. Verify public IP against SSM static IP (BLOCKS BOOT on mismatch)
//! 6. Authenticate (token cache → SSM → TOTP → JWT)
//! 7. (parallel) QuestDB table setup (5 DDL queries concurrent)
//! 8. Build F&O universe + subscription plan
//! 9. Build WebSocket connection pool
//! 10. Spawn tick processing pipeline
//! 11. Spawn background historical candle fetch (cold path)
//! 12. Spawn order update WebSocket
//! 13. Start API server
//! 14. Spawn token renewal task
//! 15. Await shutdown signal

mod infra;
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
use dhan_live_trader_common::trading_calendar::TradingCalendar;
use dhan_live_trader_core::auth::secret_manager;
use dhan_live_trader_core::auth::token_cache;
use dhan_live_trader_core::auth::token_manager::{TokenHandle, TokenManager};
use dhan_live_trader_core::historical::candle_fetcher::fetch_historical_candles;
use dhan_live_trader_core::historical::cross_verify::verify_candle_integrity;
use dhan_live_trader_core::instrument::subscription_planner::SubscriptionPlan;
use dhan_live_trader_core::instrument::{
    InstrumentLoadResult, build_subscription_plan, load_or_build_instruments,
    run_instrument_diagnostic,
};
use dhan_live_trader_core::network::ip_verifier;
use dhan_live_trader_core::notification::{NotificationEvent, NotificationService};
use dhan_live_trader_core::pipeline::run_tick_processor;
use dhan_live_trader_core::websocket::connection_pool::WebSocketConnectionPool;
use dhan_live_trader_core::websocket::order_update_connection::run_order_update_connection;
use dhan_live_trader_core::websocket::types::{InstrumentSubscription, WebSocketError};

use dhan_live_trader_storage::calendar_persistence;
use dhan_live_trader_storage::candle_persistence::{
    CandlePersistenceWriter, ensure_candle_table_dedup_keys,
};
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

    // Build trading calendar (validated inside config.validate() already).
    let trading_calendar = std::sync::Arc::new(
        TradingCalendar::from_config(&config.trading)
            .context("failed to build trading calendar")?,
    );

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

    // Log trading day status — critical for operational awareness.
    let is_trading = trading_calendar.is_trading_day_today();
    let is_muhurat = trading_calendar.is_muhurat_trading_today();
    info!(
        is_trading_day = is_trading,
        is_muhurat_session = is_muhurat,
        holidays_loaded = trading_calendar.holiday_count(),
        "NSE trading calendar loaded"
    );
    if !is_trading {
        warn!("today is NOT a regular NSE trading day — market data and orders will be limited");
    }

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
    // Two-Phase Boot: try fast path first (cache hit → ticks in ~400ms),
    // fall back to full sequential boot on cache miss.
    // -----------------------------------------------------------------------
    let fast_cache = token_cache::load_token_cache_fast();

    if let Some(cache_result) = fast_cache {
        // =================================================================
        // FAST BOOT PATH (crash restart with valid cache)
        //
        // Critical path (ONLY WebSocket blocks):
        //   Config → Logging → Cache (2ms) → Instruments (0.5ms) →
        //   WebSocket connect (~400ms) → Tick processor → TICKS FLOWING
        //
        // Background (fire-and-forget, zero blocking):
        //   QuestDB DDL + Notification + Infra + SSM + renewal + everything else
        //
        // The tick processor starts with in-memory processing only (candle
        // aggregation + top movers). QuestDB persistence is NOT on the critical
        // path — it starts in background and was already running before crash.
        // The ~300ms gap (QuestDB DDL) is handled by QuestDB's existing tables
        // from the previous run. Persistence writers reconnect in background.
        // =================================================================
        info!("FAST BOOT: crash recovery — cached token + client_id valid, SSM deferred");

        let token_handle: TokenHandle = std::sync::Arc::new(arc_swap::ArcSwap::new(
            std::sync::Arc::new(Some(cache_result.token)),
        ));
        let client_id = cache_result.client_id;

        // --- IP verification + notification init (parallel, both needed before Dhan calls) ---
        // Notification must be ready so IP failure can alert via Telegram.
        // IP verification must pass before any WebSocket/REST call to Dhan.
        let (fast_notifier, ip_result) = tokio::join!(
            NotificationService::initialize(&config.notification),
            ip_verifier::verify_public_ip(),
        );
        match ip_result {
            Ok(result) => {
                fast_notifier.notify(NotificationEvent::IpVerificationSuccess {
                    verified_ip: result.verified_ip,
                });
            }
            Err(err) => {
                error!(error = %err, "FAST BOOT: IP verification failed — BLOCKING BOOT");
                fast_notifier.notify(NotificationEvent::IpVerificationFailed {
                    reason: err.to_string(),
                });
                return Ok(());
            }
        }

        // --- Load instruments (sub-1ms from rkyv cache during market hours) ---
        let subscription_plan = load_instruments(&config).await;

        // --- WebSocket connect: THE ONLY BLOCKING STEP (~400ms) ---
        let (frame_receiver, ws_handles) =
            build_websocket_pool(&token_handle, &client_id, &subscription_plan, &config).await;

        // --- Tick processor: TICKS FLOWING (in-memory, no persistence yet) ---
        let shared_movers: dhan_live_trader_core::pipeline::SharedTopMoversSnapshot =
            std::sync::Arc::new(std::sync::RwLock::new(None));

        let processor_handle = if let Some(receiver) = frame_receiver {
            let candle_agg = Some(dhan_live_trader_core::pipeline::CandleAggregator::new());
            let movers = Some(dhan_live_trader_core::pipeline::TopMoversTracker::new());
            let snapshot_handle = Some(shared_movers.clone());
            // Start with None writers — ticks are processed in-memory for trading.
            // QuestDB persistence reconnects in background (tables already exist
            // from pre-crash run, DDL is idempotent CREATE IF NOT EXISTS).
            let handle = tokio::spawn(async move {
                run_tick_processor(
                    receiver,
                    None, // tick_writer — QuestDB reconnects in background
                    None, // depth_writer — QuestDB reconnects in background
                    None,
                    candle_agg,
                    movers,
                    snapshot_handle,
                )
                .await;
            });
            info!("FAST BOOT COMPLETE — tick processor started, ticks flowing (in-memory)");
            Some(handle)
        } else {
            info!("tick processor skipped — no frame source available");
            None
        };

        // =================================================================
        // TICKS FLOWING — everything below is background, zero blocking.
        // All services start in parallel via tokio::join!.
        // =================================================================

        // Background: Docker infra + QuestDB DDL + SSM validation
        // Notification already initialized above (needed for IP verification).
        // All run concurrently. None of them block tick processing.
        let notifier = fast_notifier;
        let (_, deferred_token_manager) = tokio::join!(
            // Docker infra + QuestDB DDL
            async {
                infra::ensure_infra_running(&config.questdb).await;
                tokio::join!(
                    ensure_tick_table_dedup_keys(&config.questdb),
                    ensure_depth_and_prev_close_tables(&config.questdb),
                    ensure_instrument_tables(&config.questdb),
                    ensure_candle_table_dedup_keys(&config.questdb),
                    calendar_persistence::ensure_calendar_table(&config.questdb),
                    dhan_live_trader_storage::materialized_views::ensure_candle_views(
                        &config.questdb
                    ),
                );
                // Persist trading calendar to QuestDB (best-effort, non-blocking).
                let _ = calendar_persistence::persist_calendar(&trading_calendar, &config.questdb);
                info!("QuestDB DDL complete (background)");
            },
            // SSM validation + TokenManager for renewal
            async {
                let timeout = std::time::Duration::from_secs(
                    dhan_live_trader_common::constants::TOKEN_INIT_TIMEOUT_SECS,
                );
                tokio::time::timeout(
                    timeout,
                    TokenManager::initialize_deferred(
                        token_handle.clone(),
                        &client_id,
                        &config.dhan,
                        &config.token,
                        &config.network,
                        &notifier,
                    ),
                )
                .await
            },
        );

        // Handle deferred token manager result
        let token_manager = match deferred_token_manager {
            Ok(Ok(manager)) => {
                info!("deferred auth: SSM validated, token renewal ready");
                Some(manager)
            }
            Ok(Err(err)) => {
                error!(error = %err, "deferred auth failed — token renewal unavailable");
                notifier.notify(
                    dhan_live_trader_core::notification::events::NotificationEvent::AuthenticationFailed {
                        reason: format!("DEFERRED: {err} — ticks still flowing but renewal unavailable"),
                    },
                );
                None
            }
            Err(_elapsed) => {
                error!("deferred auth timed out — token renewal unavailable");
                None
            }
        };

        // --- Background: Order update WebSocket ---
        let (order_update_sender, _order_update_receiver) = tokio::sync::broadcast::channel::<
            dhan_live_trader_common::order_types::OrderUpdate,
        >(256);

        let order_update_handle = {
            let url = config.dhan.order_update_websocket_url.clone();
            let ws_client_id = client_id.clone();
            let token = token_handle.clone();
            let sender = order_update_sender.clone();
            let cal = trading_calendar.clone();
            tokio::spawn(async move {
                run_order_update_connection(url, ws_client_id, token, sender, cal).await;
            })
        };
        info!("order update WebSocket started (background)");

        // --- Background: API server ---
        let api_state = SharedAppState::new(
            config.questdb.clone(),
            config.dhan.clone(),
            config.instrument.clone(),
            shared_movers.clone(),
        );

        let router = build_router(api_state);
        let bind_addr: SocketAddr = format!("{}:{}", config.api.host, config.api.port)
            .parse()
            .context("invalid API bind address")?;
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .context("failed to bind API server")?;
        info!(address = %bind_addr, "API server listening (background)");

        let api_handle = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, router).await {
                error!(?err, "API server error");
            }
        });

        // --- Background: Token renewal ---
        let renewal_handle = token_manager.as_ref().map(|tm| {
            let handle = tm.spawn_renewal_task();
            info!("token renewal task started (background)");
            handle
        });

        // --- Background: Historical candle fetch ---
        if let Some(ref tm) = token_manager {
            spawn_historical_candle_fetch(&subscription_plan, &config, tm, &notifier);
        }

        notifier.notify(NotificationEvent::Custom {
            message: "<b>FAST BOOT</b>\nCrash recovery: ticks flowing, all services ready"
                .to_string(),
        });

        // --- Await shutdown ---
        return run_shutdown_fast(
            ws_handles,
            processor_handle,
            renewal_handle,
            Some(order_update_handle),
            Some(api_handle),
            otel_provider,
            &notifier,
            &config,
            shared_movers,
        )
        .await;
    }

    // =====================================================================
    // SLOW BOOT PATH (normal start / no cache)
    // Original sequential boot — unchanged behavior.
    // =====================================================================
    info!("standard boot — no valid token cache, full auth sequence");

    // -----------------------------------------------------------------------
    // Steps 4+5: Notification + Docker infra (parallel — independent of each other)
    // -----------------------------------------------------------------------
    info!("initializing notification service + checking Docker infra (parallel)");
    let (notifier, _) = tokio::join!(
        NotificationService::initialize(&config.notification),
        infra::ensure_infra_running(&config.questdb),
    );

    // -----------------------------------------------------------------------
    // Step 5.5: Verify public IP matches SSM static IP (BLOCKS BOOT on failure)
    // -----------------------------------------------------------------------
    // Must run AFTER notification (so Telegram alerts fire) and BEFORE auth
    // (so no Dhan API call ever happens from a non-whitelisted IP).
    info!("verifying public IP against SSM static IP");
    match ip_verifier::verify_public_ip().await {
        Ok(result) => {
            notifier.notify(NotificationEvent::IpVerificationSuccess {
                verified_ip: result.verified_ip,
            });
        }
        Err(err) => {
            error!(error = %err, "IP verification failed — BLOCKING BOOT");
            notifier.notify(NotificationEvent::IpVerificationFailed {
                reason: err.to_string(),
            });
            return Ok(());
        }
    }

    // -----------------------------------------------------------------------
    // Step 6: Authenticate with Dhan API (infinite retry for transient errors)
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
    // Step 6b: Set up QuestDB tick persistence (best-effort)
    // -----------------------------------------------------------------------
    info!(
        "setting up QuestDB tables (ticks + instruments + depth + previous_close + historical_candles_1m + materialized views)"
    );

    // All table creation queries are independent — run in parallel for faster boot.
    tokio::join!(
        ensure_tick_table_dedup_keys(&config.questdb),
        ensure_depth_and_prev_close_tables(&config.questdb),
        ensure_instrument_tables(&config.questdb),
        ensure_candle_table_dedup_keys(&config.questdb),
        calendar_persistence::ensure_calendar_table(&config.questdb),
        dhan_live_trader_storage::materialized_views::ensure_candle_views(&config.questdb),
    );

    // Persist trading calendar to QuestDB (best-effort, non-blocking).
    let _ = calendar_persistence::persist_calendar(&trading_calendar, &config.questdb);

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
    let subscription_plan: Option<SubscriptionPlan> = load_instruments(&config).await;

    // -----------------------------------------------------------------------
    // Step 8: Build WebSocket connection pool (only if authenticated + plan ready)
    // -----------------------------------------------------------------------
    let token_handle = token_manager.token_handle();
    let (frame_receiver, ws_handles) = if subscription_plan.is_some() {
        info!("building WebSocket connection pool");

        let ws_client_id = {
            let credentials = secret_manager::fetch_dhan_credentials()
                .await
                .context("failed to fetch Dhan client ID for WebSocket")?;
            credentials.client_id.expose_secret().to_string()
        };

        build_websocket_pool(&token_handle, &ws_client_id, &subscription_plan, &config).await
    } else {
        warn!("WebSocket pool skipped — running in offline mode");
        (None, Vec::new())
    };

    // -----------------------------------------------------------------------
    // Step 9: Spawn tick processor
    // -----------------------------------------------------------------------
    let shared_movers: dhan_live_trader_core::pipeline::SharedTopMoversSnapshot =
        std::sync::Arc::new(std::sync::RwLock::new(None));

    let processor_handle = if let Some(receiver) = frame_receiver {
        let candle_agg = Some(dhan_live_trader_core::pipeline::CandleAggregator::new());
        let movers = Some(dhan_live_trader_core::pipeline::TopMoversTracker::new());
        let snapshot_handle = Some(shared_movers.clone());
        let handle = tokio::spawn(async move {
            run_tick_processor(
                receiver,
                tick_writer,
                depth_writer,
                None,
                candle_agg,
                movers,
                snapshot_handle,
            )
            .await;
        });
        info!("tick processor started (with candle aggregation + top movers)");
        Some(handle)
    } else {
        info!("tick processor skipped — no frame source available");
        None
    };

    // -----------------------------------------------------------------------
    // Step 9.5: Background historical candle fetch (cold path — never blocks live)
    // -----------------------------------------------------------------------
    spawn_historical_candle_fetch(&subscription_plan, &config, &token_manager, &notifier);

    // -----------------------------------------------------------------------
    // Step 10: Spawn order update WebSocket connection
    // -----------------------------------------------------------------------
    let (order_update_sender, _order_update_receiver) =
        tokio::sync::broadcast::channel::<dhan_live_trader_common::order_types::OrderUpdate>(256);

    let order_update_handle = {
        let url = config.dhan.order_update_websocket_url.clone();
        let ws_client_id = {
            let credentials = secret_manager::fetch_dhan_credentials()
                .await
                .context("failed to fetch Dhan client ID for order update WebSocket")?;
            credentials.client_id.expose_secret().to_string()
        };
        let token = token_manager.token_handle();
        let sender = order_update_sender.clone();
        let cal = trading_calendar.clone();
        tokio::spawn(async move {
            run_order_update_connection(url, ws_client_id, token, sender, cal).await;
        })
    };
    info!("order update WebSocket started");

    // -----------------------------------------------------------------------
    // Step 11: Start axum API server
    // -----------------------------------------------------------------------
    let api_state = SharedAppState::new(
        config.questdb.clone(),
        config.dhan.clone(),
        config.instrument.clone(),
        shared_movers.clone(),
    );

    let router = build_router(api_state);

    let bind_addr: SocketAddr = format!("{}:{}", config.api.host, config.api.port)
        .parse()
        .context("invalid API bind address")?;

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .context("failed to bind API server")?;

    info!(address = %bind_addr, "API server listening");

    let api_handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router).await {
            error!(?err, "API server error");
        }
    });

    // -----------------------------------------------------------------------
    // Step 12: Spawn token renewal background task
    // -----------------------------------------------------------------------
    let renewal_handle = token_manager.spawn_renewal_task();
    info!("token renewal task started");

    // -----------------------------------------------------------------------
    // Step 13: Await shutdown signal
    // -----------------------------------------------------------------------
    run_shutdown_fast(
        ws_handles,
        processor_handle,
        Some(renewal_handle),
        Some(order_update_handle),
        Some(api_handle),
        otel_provider,
        &notifier,
        &config,
        shared_movers,
    )
    .await
}

// ---------------------------------------------------------------------------
// Helper: Load instruments (shared by fast and slow boot paths)
// ---------------------------------------------------------------------------

async fn load_instruments(config: &ApplicationConfig) -> Option<SubscriptionPlan> {
    info!("checking instrument build eligibility");

    match load_or_build_instruments(
        &config.dhan.instrument_csv_url,
        &config.dhan.instrument_csv_fallback_url,
        &config.instrument,
        &config.questdb,
        &config.subscription,
    )
    .await
    {
        Ok(InstrumentLoadResult::FreshBuild(universe)) => {
            let ist = match FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) {
                Some(tz) => tz,
                None => {
                    error!("invalid IST offset constant");
                    return None;
                }
            };
            let today = Utc::now().with_timezone(&ist).date_naive();
            let plan = build_subscription_plan(&universe, &config.subscription, today);

            info!(
                total = plan.summary.total,
                feed_mode = %plan.summary.feed_mode,
                "subscription plan ready (fresh build)"
            );
            Some(plan)
        }
        Ok(InstrumentLoadResult::CachedPlan(plan)) => {
            info!(
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
            warn!(
                error = %err,
                "instrument build failed — no instruments to subscribe"
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: Build WebSocket connection pool (shared by fast and slow boot paths)
// ---------------------------------------------------------------------------

async fn build_websocket_pool(
    token_handle: &TokenHandle,
    client_id: &str,
    subscription_plan: &Option<SubscriptionPlan>,
    config: &ApplicationConfig,
) -> (
    Option<tokio::sync::mpsc::Receiver<bytes::Bytes>>,
    Vec<tokio::task::JoinHandle<Result<(), WebSocketError>>>,
) {
    let plan = match subscription_plan {
        Some(p) => p,
        None => {
            warn!("WebSocket pool skipped — no subscription plan");
            return (None, Vec::new());
        }
    };

    info!("building WebSocket connection pool");

    let instruments: Vec<InstrumentSubscription> = plan
        .registry
        .iter()
        .map(|inst| InstrumentSubscription::new(inst.exchange_segment, inst.security_id))
        .collect();

    let feed_mode = plan.summary.feed_mode;
    let instrument_count = instruments.len();

    let mut pool = match WebSocketConnectionPool::new(
        token_handle.clone(),
        client_id.to_string(),
        config.dhan.clone(),
        config.websocket.clone(),
        instruments,
        feed_mode,
    ) {
        Ok(pool) => pool,
        Err(err) => {
            error!(?err, "failed to create WebSocket connection pool");
            return (None, Vec::new());
        }
    };

    let receiver = pool.take_frame_receiver();
    let handles = pool.spawn_all().await;

    info!(
        connections = handles.len(),
        instruments = instrument_count,
        feed_mode = %feed_mode,
        "WebSocket pool started"
    );

    (Some(receiver), handles)
}

// ---------------------------------------------------------------------------
// Helper: Spawn background historical candle fetch
// ---------------------------------------------------------------------------

fn spawn_historical_candle_fetch(
    subscription_plan: &Option<SubscriptionPlan>,
    config: &ApplicationConfig,
    token_manager: &std::sync::Arc<TokenManager>,
    notifier: &std::sync::Arc<NotificationService>,
) {
    let plan = match subscription_plan
        .as_ref()
        .filter(|_| config.historical.enabled)
    {
        Some(p) => p,
        None => return,
    };

    let bg_registry = plan.registry.clone();
    let bg_dhan_config = config.dhan.clone();
    let bg_historical_config = config.historical.clone();
    let bg_questdb_config = config.questdb.clone();
    let bg_token_handle = token_manager.token_handle();
    let bg_notifier = std::sync::Arc::clone(notifier);

    tokio::spawn(async move {
        // Fetch client_id from SSM (background — doesn't block boot)
        let client_id = match secret_manager::fetch_dhan_credentials().await {
            Ok(creds) => creds.client_id,
            Err(err) => {
                warn!(
                    ?err,
                    "failed to fetch credentials for historical candle fetch"
                );
                bg_notifier.notify(NotificationEvent::HistoricalFetchFailed {
                    instruments_fetched: 0,
                    instruments_failed: 0,
                    total_candles: 0,
                });
                return;
            }
        };

        let mut candle_writer = match CandlePersistenceWriter::new(&bg_questdb_config) {
            Ok(writer) => writer,
            Err(err) => {
                warn!(?err, "failed to create candle writer for background fetch");
                bg_notifier.notify(NotificationEvent::HistoricalFetchFailed {
                    instruments_fetched: 0,
                    instruments_failed: 0,
                    total_candles: 0,
                });
                return;
            }
        };

        info!("starting background historical candle fetch");

        let summary = fetch_historical_candles(
            &bg_registry,
            &bg_dhan_config,
            &bg_historical_config,
            &bg_token_handle,
            &client_id,
            &mut candle_writer,
        )
        .await;

        if summary.instruments_failed > 0 {
            bg_notifier.notify(NotificationEvent::HistoricalFetchFailed {
                instruments_fetched: summary.instruments_fetched,
                instruments_failed: summary.instruments_failed,
                total_candles: summary.total_candles,
            });
        }

        // Cross-verify candle integrity in QuestDB
        let verify_report = verify_candle_integrity(&bg_questdb_config).await;
        if !verify_report.passed {
            bg_notifier.notify(NotificationEvent::CandleVerificationFailed {
                instruments_checked: verify_report.instruments_checked,
                instruments_with_gaps: verify_report.instruments_with_gaps,
            });
        }

        info!(
            instruments_fetched = summary.instruments_fetched,
            instruments_failed = summary.instruments_failed,
            total_candles = summary.total_candles,
            verification_passed = verify_report.passed,
            "background historical candle fetch complete"
        );
    });

    info!("background historical candle fetch spawned (non-blocking)");
}

// ---------------------------------------------------------------------------
// Helper: Graceful shutdown (shared by fast and slow boot paths)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)] // APPROVED: shutdown orchestration requires all handles
async fn run_shutdown_fast(
    ws_handles: Vec<tokio::task::JoinHandle<Result<(), WebSocketError>>>,
    processor_handle: Option<tokio::task::JoinHandle<()>>,
    renewal_handle: Option<tokio::task::JoinHandle<()>>,
    order_update_handle: Option<tokio::task::JoinHandle<()>>,
    api_handle: Option<tokio::task::JoinHandle<()>>,
    otel_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    notifier: &std::sync::Arc<NotificationService>,
    config: &ApplicationConfig,
    shared_movers: dhan_live_trader_core::pipeline::SharedTopMoversSnapshot,
) -> Result<()> {
    let bind_addr: SocketAddr = format!("{}:{}", config.api.host, config.api.port)
        .parse()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 8080)));

    let mode = "LIVE";
    info!(
        mode,
        "system ready — press Ctrl+C to stop\n\
         \n\
           Terminal:   http://{bind_addr}/\n\
           Health:     http://{bind_addr}/health\n\
           Stats:      http://{bind_addr}/api/stats\n\
           Portal:     http://{bind_addr}/portal\n\
           Rebuild:    POST http://{bind_addr}/api/instruments/rebuild\n"
    );

    notifier.notify(NotificationEvent::StartupComplete { mode });

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for shutdown signal")?;

    info!("shutdown signal received — stopping gracefully");
    notifier.notify(NotificationEvent::ShutdownInitiated);

    // Second Ctrl+C → force exit.
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        warn!("second shutdown signal received — forcing immediate exit");
        std::process::exit(1);
    });

    // 1. Stop token renewal.
    if let Some(handle) = renewal_handle {
        handle.abort();
    }

    // 2. Abort order update WebSocket.
    if let Some(handle) = order_update_handle {
        handle.abort();
    }

    // 3. Abort WebSocket connections (drops senders → processor exits).
    for handle in ws_handles {
        handle.abort();
    }

    // 4. Wait for tick processor final flush.
    if let Some(handle) = processor_handle {
        let shutdown_timeout = std::time::Duration::from_secs(
            dhan_live_trader_common::constants::GRACEFUL_SHUTDOWN_TIMEOUT_SECS,
        );
        match tokio::time::timeout(shutdown_timeout, handle).await {
            Ok(_) => info!("tick processor shut down gracefully"),
            Err(_) => warn!("tick processor shutdown timed out — aborting"),
        }
    }

    // 5. Stop API server.
    if let Some(handle) = api_handle {
        handle.abort();
    }

    // 6. Flush OpenTelemetry.
    drop(otel_provider);
    drop(shared_movers);

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
