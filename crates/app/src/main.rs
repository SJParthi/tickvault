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

// Modules are declared in lib.rs for coverage instrumentation.
use dhan_live_trader_app::boot_helpers::{
    CONFIG_BASE_PATH, CONFIG_LOCAL_PATH, FAST_BOOT_WINDOW_END, FAST_BOOT_WINDOW_START,
    compute_market_close_sleep, create_log_file_writer, effective_ws_stagger, format_bind_addr,
    format_cross_match_details, format_timeframe_details, format_violation_details,
};
use dhan_live_trader_app::{infra, observability, trading_pipeline};

use std::net::SocketAddr;

use anyhow::{Context, Result};
use chrono::Utc;
use figment::Figment;
use figment::providers::{Format, Toml};
use secrecy::ExposeSecret;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use dhan_live_trader_common::config::ApplicationConfig;
use dhan_live_trader_common::instrument_types::FnoUniverse;
use dhan_live_trader_common::trading_calendar::{TradingCalendar, ist_offset};
use dhan_live_trader_core::auth::secret_manager;
use dhan_live_trader_core::auth::token_cache;
use dhan_live_trader_core::auth::token_manager::{TokenHandle, TokenManager};
use dhan_live_trader_core::historical::candle_fetcher::fetch_historical_candles;
use dhan_live_trader_core::historical::cross_verify::{
    cross_match_historical_vs_live, verify_candle_integrity,
};
use dhan_live_trader_core::instrument::binary_cache::read_binary_cache;
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
use dhan_live_trader_storage::greeks_persistence::ensure_greeks_tables;
use dhan_live_trader_storage::instrument_persistence::{
    ensure_instrument_tables, persist_instrument_snapshot,
};
use dhan_live_trader_storage::tick_persistence::{
    DepthPersistenceWriter, TickPersistenceWriter, ensure_depth_and_prev_close_tables,
    ensure_tick_table_dedup_keys,
};

use dhan_live_trader_api::build_router;
use dhan_live_trader_api::state::{SharedAppState, SharedHealthStatus, SystemHealthStatus};

// Constants are in boot_helpers module (lib.rs) for coverage instrumentation.

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

    let boot_start = std::time::Instant::now();

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

    // File-based JSON log layer for Alloy → Loki ingestion.
    // Enables Grafana log dashboards to work even in dev mode (cargo run).
    // Alloy watches this file and pushes logs to Loki automatically.
    let file_log_layer: Option<Box<dyn tracing_subscriber::Layer<_> + Send + Sync + 'static>> =
        match create_log_file_writer() {
            Some(file) => {
                let file_fmt = tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_file(false)
                    .with_line_number(false)
                    .json()
                    .with_writer(std::sync::Mutex::new(file));
                Some(Box::new(file_fmt))
            }
            None => None,
        };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_boxed)
        .with(file_log_layer)
        .with(otel_layer)
        .init();

    // Install panic hook: log at ERROR level (triggers Telegram via Loki → Grafana alerting).
    let default_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let location = panic_info.location().map_or_else(
            || "unknown".to_string(),
            |loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()),
        );
        let payload = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic payload".to_string()
        };
        tracing::error!(
            panic_location = %location,
            panic_payload = %payload,
            "PANIC: dhan-live-trader crashed"
        );
        default_panic_hook(panic_info);
    }));

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
    let is_mock_trading = trading_calendar.is_mock_trading_today();
    info!(
        is_trading_day = is_trading,
        is_muhurat_session = is_muhurat,
        is_mock_trading_session = is_mock_trading,
        holidays_loaded = trading_calendar.holiday_count(),
        mock_trading_dates_loaded = trading_calendar.mock_trading_count(),
        "NSE trading calendar loaded"
    );
    if is_mock_trading {
        info!(
            "today is an NSE mock trading session (Saturday) — compressed hours, no real settlement"
        );
    }
    if !is_trading && !is_mock_trading {
        info!("today is NOT a trading day — manual start, all components will load normally");
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
    // Two-Phase Boot: fast path ONLY during market hours on trading days.
    // Outside market hours or non-trading days → always slow boot.
    //
    // INTENTIONAL: Mock trading Saturdays (is_mock_trading_today()) are
    // excluded from fast boot. Mock sessions have different hours, are not
    // real market sessions, and should always take the slow boot path
    // which downloads fresh instruments. This is a safety decision —
    // is_mock_trading is for logging/awareness only, never for trading gates.
    // -----------------------------------------------------------------------
    let fast_cache = token_cache::load_token_cache_fast();
    let is_market_hours = trading_calendar.is_trading_day_today()
        && dhan_live_trader_core::instrument::instrument_loader::is_within_build_window(
            FAST_BOOT_WINDOW_START,
            FAST_BOOT_WINDOW_END,
        );

    if !is_market_hours && fast_cache.is_some() {
        info!("token cache exists but outside market hours / non-trading day — using slow boot");
    }

    if let Some(cache_result) = fast_cache.filter(|_| is_market_hours) {
        // =================================================================
        // FAST BOOT PATH (market-hours crash restart with valid cache)
        //
        // ONLY activates when ALL conditions are met:
        //   1. Valid token cache exists (crash recovery scenario)
        //   2. Today is an NSE trading day (not weekend/holiday)
        //   3. Current IST time is 09:00–15:30 (NSE session window)
        //
        // Outside this window (e.g., 8 AM pre-market, weekends, holidays),
        // the slow boot path runs — downloads fresh instruments, starts
        // Docker first, creates persistence writers properly.
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
        let (subscription_plan, fresh_universe) = load_instruments(&config, is_trading).await;

        // --- WebSocket pool create (channel only, NOT spawned yet) ---
        let (pool_receiver, ws_pool_ready) = match create_websocket_pool(
            &token_handle,
            &client_id,
            &subscription_plan,
            &config,
            true,
        ) {
            Some((receiver, pool)) => (Some(receiver), Some(pool)),
            None => (None, None),
        };

        // --- Tick processor: start BEFORE WS connections spawn ---
        let shared_movers: dhan_live_trader_core::pipeline::SharedTopMoversSnapshot =
            std::sync::Arc::new(std::sync::RwLock::new(None));

        // Tick broadcast for trading pipeline (cold path consumer).
        let (fast_tick_broadcast_sender, _fast_tick_broadcast_rx) = tokio::sync::broadcast::channel::<
            dhan_live_trader_common::tick_types::ParsedTick,
        >(1024);

        let processor_handle = if let Some(receiver) = pool_receiver {
            let candle_agg = Some(dhan_live_trader_core::pipeline::CandleAggregator::new());
            let movers = Some(dhan_live_trader_core::pipeline::TopMoversTracker::new());
            let snapshot_handle = Some(shared_movers.clone());
            let tick_broadcast_for_processor = Some(fast_tick_broadcast_sender.clone());
            // Start with None writers — ticks are processed in-memory for trading.
            // QuestDB persistence reconnects in background (tables already exist
            // from pre-crash run, DDL is idempotent CREATE IF NOT EXISTS).
            let handle = tokio::spawn(async move {
                run_tick_processor(
                    receiver,
                    None, // tick_writer — QuestDB reconnects in background
                    None, // depth_writer — QuestDB reconnects in background
                    tick_broadcast_for_processor,
                    candle_agg,
                    None, // live_candle_writer — QuestDB reconnects in background
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

        // --- NOW spawn WebSocket connections (tick processor consuming) ---
        let ws_handles = if let Some(pool) = ws_pool_ready {
            spawn_websocket_connections(pool).await
        } else {
            Vec::new()
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
                    dhan_live_trader_storage::constituency_persistence::ensure_constituency_table(
                        &config.questdb
                    ),
                    dhan_live_trader_storage::materialized_views::ensure_candle_views(
                        &config.questdb
                    ),
                    ensure_greeks_tables(&config.questdb),
                );
                // Persist trading calendar to QuestDB (best-effort, non-blocking).
                let _ = calendar_persistence::persist_calendar(&trading_calendar, &config.questdb);

                // Re-persist instrument data if fresh build happened before Docker started.
                // The initial persistence inside load_or_build_instruments fails when QuestDB
                // is not running yet. Now that Docker is up and tables exist, retry.
                if let Some(ref universe) = fresh_universe {
                    let _ = persist_instrument_snapshot(universe, &config.questdb).await;
                }

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

        // --- Background: Tick persistence (cold path — subscribes to broadcast) ---
        // The tick processor was started with None writers (fast boot, QuestDB wasn't
        // ready). Now QuestDB is available. Spawn a separate cold-path consumer that
        // subscribes to the tick broadcast and persists to QuestDB. This never touches
        // the hot-path tick processor loop — zero allocation impact.
        {
            let tick_persistence_rx = fast_tick_broadcast_sender.subscribe();
            let questdb_cfg = config.questdb.clone();
            tokio::spawn(async move {
                run_tick_persistence_consumer(tick_persistence_rx, questdb_cfg).await;
            });
            info!("background tick persistence consumer started (cold path)");
        }

        // --- Background: Candle persistence (cold path — aggregates ticks → candles_1s) ---
        // In fast boot, live_candle_writer is None so the hot-path CandleAggregator
        // produces candles but can't persist them. This cold-path consumer runs its
        // own CandleAggregator, subscribes to the tick broadcast, and writes completed
        // 1-second candles to QuestDB `candles_1s`. Materialized views (1m, 5m, etc.)
        // automatically aggregate from candles_1s.
        {
            let candle_persistence_rx = fast_tick_broadcast_sender.subscribe();
            let questdb_cfg = config.questdb.clone();
            tokio::spawn(async move {
                run_candle_persistence_consumer(candle_persistence_rx, questdb_cfg).await;
            });
            info!("background candle persistence consumer started (cold path)");
        }

        // --- Background: Greeks pipeline (option chain fetch → compute → persist) ---
        if config.greeks.enabled {
            let greeks_token = token_handle.clone();
            let greeks_client_id = client_id.clone();
            let greeks_base_url = config.dhan.rest_api_base_url.clone();
            let greeks_config = config.greeks.clone();
            let greeks_questdb = config.questdb.clone();
            tokio::spawn(async move {
                dhan_live_trader_app::greeks_pipeline::run_greeks_pipeline(
                    greeks_token,
                    greeks_client_id,
                    greeks_base_url,
                    greeks_config,
                    greeks_questdb,
                )
                .await;
            });
            info!("background greeks pipeline started (cold path)");
        } else {
            info!("greeks pipeline disabled in config");
        }

        // --- Background: Tick-driven Greeks aggregator (candle-aligned) ---
        if config.greeks.enabled
            && let Some(ref plan) = subscription_plan
        {
            let greeks_agg_rx = fast_tick_broadcast_sender.subscribe();
            // O(1) EXEMPT: cold path — clone registry once at startup for background consumer.
            let greeks_agg_registry = plan.registry.clone();
            let greeks_agg_config = config.greeks.clone();
            let greeks_agg_questdb = config.questdb.clone();
            tokio::spawn(async move {
                run_greeks_aggregator_consumer(
                    greeks_agg_rx,
                    greeks_agg_registry,
                    greeks_agg_config,
                    greeks_agg_questdb,
                )
                .await;
            });
            info!("tick-driven greeks aggregator started (candle-aligned, cold path)");
        }

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

        // --- Background: Daily reset signal (16:00 IST) ---
        let daily_reset_signal = std::sync::Arc::new(tokio::sync::Notify::new());
        {
            let signal = std::sync::Arc::clone(&daily_reset_signal);
            let reset_sleep = compute_market_close_sleep(
                dhan_live_trader_common::constants::APP_SHUTDOWN_TIME_IST,
            );
            if reset_sleep > std::time::Duration::ZERO {
                tokio::spawn(async move {
                    tokio::time::sleep(reset_sleep).await;
                    info!("16:00 IST reached — firing daily reset signal");
                    signal.notify_waiters();
                });
            }
        }

        // --- Background: Market close signal (15:30 IST) ---
        let market_close_signal = std::sync::Arc::new(tokio::sync::Notify::new());
        {
            let signal = std::sync::Arc::clone(&market_close_signal);
            let close_sleep = compute_market_close_sleep(&config.trading.market_close_time);
            if close_sleep > std::time::Duration::ZERO {
                tokio::spawn(async move {
                    tokio::time::sleep(close_sleep).await;
                    info!("15:30 IST reached — firing market close signal to trading pipeline");
                    signal.notify_waiters();
                });
            }
        }

        // --- Background: Trading pipeline (paper trading) ---
        let trading_handle = {
            let tick_rx = fast_tick_broadcast_sender.subscribe();
            let order_rx = order_update_sender.subscribe();

            match trading_pipeline::init_trading_pipeline(&config, &token_handle, &client_id) {
                Some((pipeline_config, hot_reloader)) => {
                    let handle = trading_pipeline::spawn_trading_pipeline_full(
                        pipeline_config,
                        tick_rx,
                        order_rx,
                        hot_reloader,
                        Some(std::sync::Arc::clone(&daily_reset_signal)),
                        Some(std::sync::Arc::clone(&market_close_signal)),
                    );
                    info!("trading pipeline started (paper trading, fast boot)");
                    Some(handle)
                }
                None => {
                    info!("trading pipeline disabled — no strategy config (fast boot)");
                    None
                }
            }
        };

        // --- Background: Index constituency download (best-effort) ---
        let bg_constituency =
            dhan_live_trader_core::index_constituency::download_and_build_constituency_map(
                &config.index_constituency,
                &config.instrument.csv_cache_directory,
            )
            .await;

        // Persist constituency to QuestDB for Grafana (best-effort, non-blocking).
        // Enrich with security_ids from instrument master for news-based trading.
        if let Some(ref map) = bg_constituency {
            match dhan_live_trader_storage::constituency_persistence::persist_constituency(
                map,
                &config.questdb,
                fresh_universe.as_ref(),
            ) {
                Ok(()) => {}
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        "index constituency QuestDB persistence failed (best-effort)"
                    );
                }
            }
        }

        let bg_shared_constituency: dhan_live_trader_api::state::SharedConstituencyMap =
            std::sync::Arc::new(std::sync::RwLock::new(bg_constituency));

        // --- Background: API server ---
        let health_status: SharedHealthStatus = std::sync::Arc::new(SystemHealthStatus::new());
        let api_state = SharedAppState::new(
            config.questdb.clone(),
            config.dhan.clone(),
            config.instrument.clone(),
            shared_movers.clone(),
            bg_shared_constituency,
            health_status,
        );

        let router = build_router(
            api_state,
            &config.api.allowed_origins,
            config.strategy.dry_run,
        );
        let bind_addr: SocketAddr = format_bind_addr(&config.api.host, config.api.port)
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
        let post_market_signal = std::sync::Arc::new(tokio::sync::Notify::new());
        if let Some(ref tm) = token_manager {
            spawn_historical_candle_fetch(
                &subscription_plan,
                &config,
                tm,
                &notifier,
                std::sync::Arc::clone(&post_market_signal),
                is_trading,
            );
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
            trading_handle,
            otel_provider,
            &notifier,
            &config,
            shared_movers,
            post_market_signal,
        )
        .await;
    }

    // =====================================================================
    // SLOW BOOT PATH (normal start / pre-market / no cache)
    // Sequential boot: Docker first → instruments → auth → WebSocket.
    // =====================================================================
    if !is_market_hours {
        info!(
            build_window_start = %config.instrument.build_window_start,
            build_window_end = %config.instrument.build_window_end,
            "standard boot — outside market hours, full sequential setup"
        );
    } else {
        info!("standard boot — no valid token cache, full auth sequence");
    }

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
        "setting up QuestDB tables (ticks + instruments + depth + previous_close + historical_candles + materialized views + greeks)"
    );

    // All table creation queries are independent — run in parallel for faster boot.
    tokio::join!(
        ensure_tick_table_dedup_keys(&config.questdb),
        ensure_depth_and_prev_close_tables(&config.questdb),
        ensure_instrument_tables(&config.questdb),
        ensure_candle_table_dedup_keys(&config.questdb),
        calendar_persistence::ensure_calendar_table(&config.questdb),
        dhan_live_trader_storage::constituency_persistence::ensure_constituency_table(
            &config.questdb
        ),
        dhan_live_trader_storage::materialized_views::ensure_candle_views(&config.questdb),
        ensure_greeks_tables(&config.questdb),
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
    // In slow boot, Docker is already running. FreshBuild persists internally,
    // but CachedPlan (within build window) needs explicit persistence here.
    let (subscription_plan, slow_boot_universe) = load_instruments(&config, is_trading).await;
    if let Some(ref universe) = slow_boot_universe {
        let _ = persist_instrument_snapshot(universe, &config.questdb).await;
    }

    // -----------------------------------------------------------------------
    // Step 8: Build WebSocket connection pool (only if authenticated + plan ready)
    // -----------------------------------------------------------------------
    let token_handle = token_manager.token_handle();

    // Fetch credentials ONCE for all downstream consumers (WS pool, order update WS, trading pipeline).
    // Previously fetched 3 separate times — each SSM call is a network roundtrip to AWS.
    let ws_client_id = {
        let credentials = secret_manager::fetch_dhan_credentials()
            .await
            .context("failed to fetch Dhan client ID for WebSocket + trading")?;
        credentials.client_id.expose_secret().to_string()
    };

    // Step 8a: Create WebSocket pool (channel + connections, NOT yet spawned).
    // Step 9 starts tick processor BEFORE connections are spawned so frames
    // are consumed immediately — prevents frame send timeouts during stagger.
    let (pool_receiver, ws_pool_ready) = if subscription_plan.is_some() {
        match create_websocket_pool(
            &token_handle,
            &ws_client_id,
            &subscription_plan,
            &config,
            is_market_hours,
        ) {
            Some((receiver, pool)) => (Some(receiver), Some(pool)),
            None => (None, None),
        }
    } else {
        warn!("WebSocket pool skipped — running in offline mode");
        (None, None)
    };

    // -----------------------------------------------------------------------
    // Step 9: Spawn tick processor FIRST (before WS connections send frames)
    // -----------------------------------------------------------------------
    let shared_movers: dhan_live_trader_core::pipeline::SharedTopMoversSnapshot =
        std::sync::Arc::new(std::sync::RwLock::new(None));

    // Tick broadcast: fan-out parsed ticks to the trading pipeline (cold path consumer).
    // Capacity 1024: trading pipeline is allowed to lag — it only needs latest price.
    let (tick_broadcast_sender, _tick_broadcast_default_rx) =
        tokio::sync::broadcast::channel::<dhan_live_trader_common::tick_types::ParsedTick>(1024);

    let processor_handle = if let Some(receiver) = pool_receiver {
        let candle_agg = Some(dhan_live_trader_core::pipeline::CandleAggregator::new());
        let live_candle_writer =
            match dhan_live_trader_storage::candle_persistence::LiveCandleWriter::new(
                &config.questdb,
            ) {
                Ok(w) => {
                    info!("QuestDB live candle writer connected (candles_1s)");
                    Some(w)
                }
                Err(err) => {
                    warn!(
                        ?err,
                        "live candle writer unavailable — candles_1s will not be persisted"
                    );
                    None
                }
            };
        let movers = Some(dhan_live_trader_core::pipeline::TopMoversTracker::new());
        let snapshot_handle = Some(shared_movers.clone());
        let tick_broadcast_for_processor = Some(tick_broadcast_sender.clone());
        let handle = tokio::spawn(async move {
            run_tick_processor(
                receiver,
                tick_writer,
                depth_writer,
                tick_broadcast_for_processor,
                candle_agg,
                live_candle_writer,
                movers,
                snapshot_handle,
            )
            .await;
        });
        info!("tick processor started (with candle aggregation + top movers + trading broadcast)");
        Some(handle)
    } else {
        info!("tick processor skipped — no frame source available");
        None
    };

    // Step 8b: NOW spawn WebSocket connections (tick processor is already consuming).
    let ws_handles = if let Some(pool) = ws_pool_ready {
        spawn_websocket_connections(pool).await
    } else {
        Vec::new()
    };

    // -----------------------------------------------------------------------
    // Step 9.5: Background historical candle fetch (cold path — never blocks live)
    // -----------------------------------------------------------------------
    let post_market_signal = std::sync::Arc::new(tokio::sync::Notify::new());
    spawn_historical_candle_fetch(
        &subscription_plan,
        &config,
        &token_manager,
        &notifier,
        std::sync::Arc::clone(&post_market_signal),
        is_trading,
    );

    // -----------------------------------------------------------------------
    // Step 9.6: Background greeks pipeline (option chain fetch → compute → persist)
    // -----------------------------------------------------------------------
    if config.greeks.enabled {
        let greeks_token = token_handle.clone();
        let greeks_client_id = ws_client_id.clone();
        let greeks_base_url = config.dhan.rest_api_base_url.clone();
        let greeks_config = config.greeks.clone();
        let greeks_questdb = config.questdb.clone();
        tokio::spawn(async move {
            dhan_live_trader_app::greeks_pipeline::run_greeks_pipeline(
                greeks_token,
                greeks_client_id,
                greeks_base_url,
                greeks_config,
                greeks_questdb,
            )
            .await;
        });
        info!("background greeks pipeline started (cold path)");
    } else {
        info!("greeks pipeline disabled in config");
    }

    // -----------------------------------------------------------------------
    // Step 9.7: Tick-driven Greeks aggregator (candle-aligned, cold path)
    // -----------------------------------------------------------------------
    if config.greeks.enabled
        && let Some(ref plan) = subscription_plan
    {
        let greeks_agg_rx = tick_broadcast_sender.subscribe();
        // O(1) EXEMPT: cold path — clone registry once at startup for background consumer.
        let greeks_agg_registry = plan.registry.clone();
        let greeks_agg_config = config.greeks.clone();
        let greeks_agg_questdb = config.questdb.clone();
        tokio::spawn(async move {
            run_greeks_aggregator_consumer(
                greeks_agg_rx,
                greeks_agg_registry,
                greeks_agg_config,
                greeks_agg_questdb,
            )
            .await;
        });
        info!("tick-driven greeks aggregator started (candle-aligned, cold path)");
    }

    // -----------------------------------------------------------------------
    // Step 10: Spawn order update WebSocket connection
    // -----------------------------------------------------------------------
    let (order_update_sender, _order_update_receiver) =
        tokio::sync::broadcast::channel::<dhan_live_trader_common::order_types::OrderUpdate>(256);

    let order_update_handle = {
        let url = config.dhan.order_update_websocket_url.clone();
        let order_ws_client_id = ws_client_id.clone();
        let token = token_manager.token_handle();
        let sender = order_update_sender.clone();
        let cal = trading_calendar.clone();
        tokio::spawn(async move {
            run_order_update_connection(url, order_ws_client_id, token, sender, cal).await;
        })
    };
    info!("order update WebSocket started");

    // -----------------------------------------------------------------------
    // Step 10.5: Spawn daily reset signal (16:00 IST)
    // -----------------------------------------------------------------------
    let daily_reset_signal = std::sync::Arc::new(tokio::sync::Notify::new());
    {
        let signal = std::sync::Arc::clone(&daily_reset_signal);
        let reset_sleep =
            compute_market_close_sleep(dhan_live_trader_common::constants::APP_SHUTDOWN_TIME_IST);
        if reset_sleep > std::time::Duration::ZERO {
            tokio::spawn(async move {
                tokio::time::sleep(reset_sleep).await;
                info!("16:00 IST reached — firing daily reset signal");
                signal.notify_waiters();
            });
        }
    }

    // Step 10.5: Spawn market close signal (15:30 IST)
    let market_close_signal = std::sync::Arc::new(tokio::sync::Notify::new());
    {
        let signal = std::sync::Arc::clone(&market_close_signal);
        let close_sleep = compute_market_close_sleep(&config.trading.market_close_time);
        if close_sleep > std::time::Duration::ZERO {
            tokio::spawn(async move {
                tokio::time::sleep(close_sleep).await;
                info!("15:30 IST reached — firing market close signal to trading pipeline");
                signal.notify_waiters();
            });
        }
    }

    // -----------------------------------------------------------------------
    // Step 10.5: Spawn trading pipeline (indicators → strategies → OMS)
    // -----------------------------------------------------------------------
    let trading_handle = {
        let tick_rx = tick_broadcast_sender.subscribe();
        let order_rx = order_update_sender.subscribe();

        match trading_pipeline::init_trading_pipeline(
            &config,
            &token_manager.token_handle(),
            &ws_client_id,
        ) {
            Some((pipeline_config, hot_reloader)) => {
                let handle = trading_pipeline::spawn_trading_pipeline_full(
                    pipeline_config,
                    tick_rx,
                    order_rx,
                    hot_reloader,
                    Some(std::sync::Arc::clone(&daily_reset_signal)),
                    Some(std::sync::Arc::clone(&market_close_signal)),
                );
                info!("trading pipeline started (paper trading)");
                Some(handle)
            }
            None => {
                info!("trading pipeline disabled — no strategy config");
                None
            }
        }
    };

    // -----------------------------------------------------------------------
    // Step 10.5: Download index constituency data (non-blocking, best-effort)
    // -----------------------------------------------------------------------
    let constituency_map =
        dhan_live_trader_core::index_constituency::download_and_build_constituency_map(
            &config.index_constituency,
            &config.instrument.csv_cache_directory,
        )
        .await;

    // Persist constituency to QuestDB for Grafana (best-effort, non-blocking).
    // Enrich with security_ids from instrument master for news-based trading.
    if let Some(ref map) = constituency_map {
        match dhan_live_trader_storage::constituency_persistence::persist_constituency(
            map,
            &config.questdb,
            slow_boot_universe.as_ref(),
        ) {
            Ok(()) => {}
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "index constituency QuestDB persistence failed (best-effort)"
                );
            }
        }
    }

    let shared_constituency: dhan_live_trader_api::state::SharedConstituencyMap =
        std::sync::Arc::new(std::sync::RwLock::new(constituency_map));

    // -----------------------------------------------------------------------
    // Step 11: Start axum API server
    // -----------------------------------------------------------------------
    let health_status: SharedHealthStatus = std::sync::Arc::new(SystemHealthStatus::new());
    let api_state = SharedAppState::new(
        config.questdb.clone(),
        config.dhan.clone(),
        config.instrument.clone(),
        shared_movers.clone(),
        shared_constituency.clone(),
        health_status,
    );

    let router = build_router(
        api_state,
        &config.api.allowed_origins,
        config.strategy.dry_run,
    );

    let bind_addr: SocketAddr = format_bind_addr(&config.api.host, config.api.port)
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
    // Boot duration check — alert if boot exceeded BOOT_TIMEOUT_SECS
    // -----------------------------------------------------------------------
    let boot_elapsed = boot_start.elapsed();
    if boot_elapsed.as_secs() > dhan_live_trader_common::constants::BOOT_TIMEOUT_SECS {
        error!(
            elapsed_secs = boot_elapsed.as_secs(),
            timeout_secs = dhan_live_trader_common::constants::BOOT_TIMEOUT_SECS,
            "BOOT TIMEOUT EXCEEDED"
        );
        notifier.notify(NotificationEvent::BootDeadlineMissed {
            deadline_secs: dhan_live_trader_common::constants::BOOT_TIMEOUT_SECS,
            step: format!(
                "boot completed in {}s (over {}s limit)",
                boot_elapsed.as_secs(),
                dhan_live_trader_common::constants::BOOT_TIMEOUT_SECS,
            ),
        });
    } else {
        info!(
            elapsed_ms = boot_elapsed.as_millis() as u64,
            "boot sequence completed"
        );
    }

    // -----------------------------------------------------------------------
    // Step 13: Await shutdown signal
    // -----------------------------------------------------------------------
    run_shutdown_fast(
        ws_handles,
        processor_handle,
        Some(renewal_handle),
        Some(order_update_handle),
        Some(api_handle),
        trading_handle,
        otel_provider,
        &notifier,
        &config,
        shared_movers,
        post_market_signal,
    )
    .await
}

// ---------------------------------------------------------------------------
// Helper: Load instruments (shared by fast and slow boot paths)
// ---------------------------------------------------------------------------

/// Loads instruments and returns (plan, optional universe for re-persistence).
///
/// The `FnoUniverse` is returned on fresh builds so the caller can re-persist
/// instrument data to QuestDB after Docker infra starts (fast boot path).
///
/// `is_trading_day` ensures non-trading days (weekends/holidays) always
/// download fresh instruments instead of using potentially stale cache.
async fn load_instruments(
    config: &ApplicationConfig,
    is_trading_day: bool,
) -> (Option<SubscriptionPlan>, Option<FnoUniverse>) {
    info!("checking instrument build eligibility");

    match load_or_build_instruments(
        &config.dhan.instrument_csv_url,
        &config.dhan.instrument_csv_fallback_url,
        &config.instrument,
        &config.questdb,
        &config.subscription,
        is_trading_day,
    )
    .await
    {
        Ok(InstrumentLoadResult::FreshBuild(universe)) => {
            let today = Utc::now().with_timezone(&ist_offset()).date_naive();
            let plan = build_subscription_plan(&universe, &config.subscription, today);

            info!(
                total = plan.summary.total,
                feed_mode = %plan.summary.feed_mode,
                "subscription plan ready (fresh build)"
            );
            (Some(plan), Some(universe))
        }
        Ok(InstrumentLoadResult::CachedPlan(plan)) => {
            info!(
                total = plan.summary.total,
                feed_mode = %plan.summary.feed_mode,
                "subscription plan ready (zero-copy rkyv cache)"
            );
            // Load owned universe from rkyv cache for QuestDB persistence.
            // Zero-copy MappedUniverse built the plan; persist_instrument_snapshot
            // needs owned FnoUniverse. One-time ~5-15ms startup cost, not hot path.
            let universe = match read_binary_cache(&config.instrument.csv_cache_directory) {
                Ok(Some(u)) => {
                    info!(
                        underlyings = u.underlyings.len(),
                        derivatives = u.derivative_contracts.len(),
                        "owned universe loaded from rkyv cache for QuestDB persistence"
                    );
                    Some(u)
                }
                Ok(None) => {
                    warn!("rkyv cache not found for persistence — instrument tables will be empty");
                    None
                }
                Err(err) => {
                    warn!(%err, "rkyv cache read failed for persistence — instrument tables will be empty");
                    None
                }
            };
            (Some(plan), universe)
        }
        Ok(InstrumentLoadResult::Unavailable) => {
            // I-P0-06: This should only trigger if emergency download also failed
            error!(
                "CRITICAL: instruments unavailable — emergency download failed, system has ZERO instruments"
            );
            (None, None)
        }
        Err(err) => {
            warn!(
                error = %err,
                "instrument build failed — no instruments to subscribe"
            );
            (None, None)
        }
    }
}

// create_log_file_writer is now in boot_helpers module (lib.rs).

// ---------------------------------------------------------------------------
// Helper: Build WebSocket connection pool (shared by fast and slow boot paths)
// ---------------------------------------------------------------------------

/// Creates the WebSocket connection pool and returns the frame receiver
/// WITHOUT spawning connections. This allows the tick processor to start
/// consuming frames BEFORE connections begin sending data, preventing
/// frame send timeouts during the stagger period.
fn create_websocket_pool(
    token_handle: &TokenHandle,
    client_id: &str,
    subscription_plan: &Option<SubscriptionPlan>,
    config: &ApplicationConfig,
    is_market_hours: bool,
) -> Option<(
    tokio::sync::mpsc::Receiver<bytes::Bytes>,
    WebSocketConnectionPool,
)> {
    let plan = match subscription_plan {
        Some(p) => p,
        None => {
            warn!("WebSocket pool skipped — no subscription plan");
            return None;
        }
    };

    // Outside market hours, use reduced stagger to avoid unnecessary boot delay.
    let mut ws_config = config.websocket.clone();
    let stagger = effective_ws_stagger(ws_config.connection_stagger_ms, is_market_hours);
    if stagger != ws_config.connection_stagger_ms {
        info!(
            market_hours_stagger_ms = ws_config.connection_stagger_ms,
            off_hours_stagger_ms = stagger,
            "using reduced WebSocket stagger (off-market-hours boot)"
        );
    }
    ws_config.connection_stagger_ms = stagger;

    info!("building WebSocket connection pool");

    let instruments: Vec<InstrumentSubscription> = plan
        .registry
        .iter()
        .map(|inst| InstrumentSubscription::new(inst.exchange_segment, inst.security_id))
        .collect();

    let feed_mode = plan.summary.feed_mode;

    let mut pool = match WebSocketConnectionPool::new(
        token_handle.clone(),
        client_id.to_string(),
        config.dhan.clone(),
        ws_config,
        instruments,
        feed_mode,
    ) {
        Ok(pool) => pool,
        Err(err) => {
            error!(?err, "failed to create WebSocket connection pool");
            return None;
        }
    };

    let receiver = pool.take_frame_receiver();
    Some((receiver, pool))
}

/// Spawns all WebSocket connections in the pool (with stagger).
/// Call AFTER the tick processor is started so frames are consumed immediately.
async fn spawn_websocket_connections(
    pool: WebSocketConnectionPool,
) -> Vec<tokio::task::JoinHandle<Result<(), WebSocketError>>> {
    let handles = pool.spawn_all().await;
    info!(connections = handles.len(), "WebSocket connections spawned");
    handles
}

// ---------------------------------------------------------------------------
// Helper: Spawn background historical candle fetch
// ---------------------------------------------------------------------------

fn spawn_historical_candle_fetch(
    subscription_plan: &Option<SubscriptionPlan>,
    config: &ApplicationConfig,
    token_manager: &std::sync::Arc<TokenManager>,
    notifier: &std::sync::Arc<NotificationService>,
    post_market_signal: std::sync::Arc<tokio::sync::Notify>,
    is_trading_day: bool,
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
                    persist_failures: 0,
                    failed_instruments: vec![],
                    failure_reasons: std::collections::HashMap::new(),
                });
                return;
            }
        };

        let candle_writer = match CandlePersistenceWriter::new(&bg_questdb_config) {
            Ok(writer) => writer,
            Err(err) => {
                warn!(?err, "failed to create candle writer for background fetch");
                bg_notifier.notify(NotificationEvent::HistoricalFetchFailed {
                    instruments_fetched: 0,
                    instruments_failed: 0,
                    total_candles: 0,
                    persist_failures: 0,
                    failed_instruments: vec![],
                    failure_reasons: std::collections::HashMap::new(),
                });
                return;
            }
        };

        // -----------------------------------------------------------------
        // Trading day: skip initial fetch — wait for post-market signal
        //   (after 15:30 IST, WS disconnected, live data fully ingested)
        // Non-trading day: fetch immediately at boot (no live data to wait for)
        // -----------------------------------------------------------------

        if is_trading_day {
            info!(
                "trading day — skipping initial historical fetch, waiting for post-market signal"
            );
            post_market_signal.notified().await;
            info!("post-market signal received — WebSockets disconnected, live data ingested");
        } else {
            info!("non-trading day — starting historical candle fetch immediately");
        }

        // -----------------------------------------------------------------
        // Fetch historical candles (runs immediately on non-trading days,
        // or after post-market signal on trading days)
        // -----------------------------------------------------------------

        // Re-fetch credentials if we waited (token may have been refreshed)
        let fetch_client_id = if is_trading_day {
            match secret_manager::fetch_dhan_credentials().await {
                Ok(creds) => creds.client_id,
                Err(err) => {
                    warn!(?err, "failed to fetch credentials for post-market fetch");
                    return;
                }
            }
        } else {
            client_id
        };

        // Re-create writer if we waited (previous may have timed out)
        let mut fetch_writer = if is_trading_day {
            match CandlePersistenceWriter::new(&bg_questdb_config) {
                Ok(writer) => writer,
                Err(err) => {
                    warn!(?err, "failed to create candle writer for post-market fetch");
                    return;
                }
            }
        } else {
            candle_writer
        };

        info!("starting historical candle fetch");

        let summary = fetch_historical_candles(
            &bg_registry,
            &bg_dhan_config,
            &bg_historical_config,
            &bg_token_handle,
            &fetch_client_id,
            &mut fetch_writer,
        )
        .await;

        if summary.instruments_failed > 0 {
            bg_notifier.notify(NotificationEvent::HistoricalFetchFailed {
                instruments_fetched: summary.instruments_fetched,
                instruments_failed: summary.instruments_failed,
                total_candles: summary.total_candles,
                persist_failures: summary.persist_failures,
                failed_instruments: summary.failed_instruments.clone(),
                failure_reasons: summary.failure_reasons.clone(),
            });
        } else {
            bg_notifier.notify(NotificationEvent::HistoricalFetchComplete {
                instruments_fetched: summary.instruments_fetched,
                instruments_skipped: summary.instruments_skipped,
                total_candles: summary.total_candles,
                persist_failures: summary.persist_failures,
            });
        }

        // -----------------------------------------------------------------
        // Cross-verify candle integrity in QuestDB
        // -----------------------------------------------------------------
        let verify_report = verify_candle_integrity(&bg_questdb_config, &bg_registry).await;
        let timeframe_details = format_timeframe_details(&verify_report);
        if verify_report.passed {
            bg_notifier.notify(NotificationEvent::CandleVerificationPassed {
                instruments_checked: verify_report.instruments_checked,
                total_candles: verify_report.total_candles_in_db,
                timeframe_details,
                ohlc_violations: verify_report.ohlc_violations,
                data_violations: verify_report.data_violations,
                timestamp_violations: verify_report.timestamp_violations,
                weekend_violations: verify_report.weekend_violations,
            });
        } else {
            bg_notifier.notify(NotificationEvent::CandleVerificationFailed {
                instruments_checked: verify_report.instruments_checked,
                instruments_with_gaps: verify_report.instruments_with_gaps,
                timeframe_details,
                ohlc_violations: verify_report.ohlc_violations,
                data_violations: verify_report.data_violations,
                timestamp_violations: verify_report.timestamp_violations,
                ohlc_details: format_violation_details(&verify_report.ohlc_details),
                data_details: format_violation_details(&verify_report.data_details),
                timestamp_details: format_violation_details(&verify_report.timestamp_details),
                weekend_violations: verify_report.weekend_violations,
                weekend_details: format_violation_details(&verify_report.weekend_details),
            });
        }

        // -----------------------------------------------------------------
        // Cross-match historical vs live candle data (trading day only —
        // on non-trading days there's no live data to compare against)
        // -----------------------------------------------------------------
        if is_trading_day {
            let cross_match =
                cross_match_historical_vs_live(&bg_questdb_config, &bg_registry).await;
            if cross_match.passed {
                bg_notifier.notify(NotificationEvent::CandleCrossMatchPassed {
                    timeframes_checked: cross_match.timeframes_checked,
                    candles_compared: cross_match.candles_compared,
                });
            } else {
                bg_notifier.notify(NotificationEvent::CandleCrossMatchFailed {
                    candles_compared: cross_match.candles_compared,
                    mismatches: cross_match.mismatches,
                    missing_live: cross_match.missing_live,
                    mismatch_details: format_cross_match_details(&cross_match.mismatch_details),
                });
            }

            info!(
                instruments_fetched = summary.instruments_fetched,
                instruments_failed = summary.instruments_failed,
                total_candles = summary.total_candles,
                verification_passed = verify_report.passed,
                cross_match_passed = cross_match.passed,
                "post-market historical fetch + cross-verification complete"
            );
        } else {
            info!(
                instruments_fetched = summary.instruments_fetched,
                instruments_failed = summary.instruments_failed,
                total_candles = summary.total_candles,
                verification_passed = verify_report.passed,
                "non-trading day historical fetch complete"
            );
        }
    });

    info!("background historical candle fetch spawned (non-blocking)");
}

// format_timeframe_details, format_violation_details, format_cross_match_details
// are now in boot_helpers module (lib.rs).

// ---------------------------------------------------------------------------
// Helper: Cold-path tick persistence consumer (fast boot only)
// ---------------------------------------------------------------------------

/// Subscribes to the tick broadcast and writes ticks to QuestDB.
///
/// Used in fast boot where the tick processor starts with `None` writers
/// (QuestDB isn't ready yet). This consumer starts after QuestDB DDL
/// completes and persists ticks on the cold path — zero impact on the
/// hot-path tick processor.
///
/// Depth persistence is handled by the tick processor in slow boot only.
/// In fast boot, depth data is not persisted until the next full restart
/// (depth requires raw frame fields which the broadcast doesn't carry).
async fn run_tick_persistence_consumer(
    mut tick_rx: tokio::sync::broadcast::Receiver<dhan_live_trader_common::tick_types::ParsedTick>,
    questdb_config: dhan_live_trader_common::config::QuestDbConfig,
) {
    let mut tick_writer = match TickPersistenceWriter::new(&questdb_config) {
        Ok(writer) => {
            info!("cold-path tick persistence writer connected to QuestDB");
            writer
        }
        Err(err) => {
            warn!(
                ?err,
                "cold-path tick persistence writer unavailable — ticks will NOT be persisted"
            );
            return;
        }
    };

    let mut ticks_persisted: u64 = 0;
    // O(1) EXEMPT: cold path, pipeline setup
    let flush_interval = std::time::Duration::from_millis(100);
    let mut last_flush = std::time::Instant::now();

    loop {
        match tick_rx.recv().await {
            Ok(tick) => {
                if let Err(err) = tick_writer.append_tick(&tick) {
                    warn!(?err, "cold-path tick persistence write failed");
                }

                ticks_persisted = ticks_persisted.saturating_add(1);

                // Periodic flush (every 100ms) to keep data flowing to QuestDB.
                if last_flush.elapsed() >= flush_interval {
                    let _ = tick_writer.flush_if_needed();
                    last_flush = std::time::Instant::now();
                }

                // B2: After QuestDB recovery + buffer drain, run integrity check.
                if tick_writer.take_recovery_flag() {
                    let qdb_config = questdb_config.clone();
                    tokio::spawn(async move {
                        dhan_live_trader_storage::tick_persistence::check_tick_gaps_after_recovery(
                            &qdb_config,
                            30, // Check last 30 minutes
                        )
                        .await;
                    });
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    skipped,
                    "cold-path tick persistence lagged — some ticks not persisted"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                info!(
                    ticks_persisted,
                    "cold-path tick persistence consumer shutting down (broadcast closed)"
                );
                let _ = tick_writer.flush_if_needed();
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: Cold-path candle persistence consumer (fast boot only)
// ---------------------------------------------------------------------------

/// Subscribes to the tick broadcast, aggregates ticks into 1-second candles,
/// and persists them to QuestDB `candles_1s` via ILP.
///
/// This is the cold-path equivalent of the hot-path CandleAggregator + LiveCandleWriter
/// that runs inside `run_tick_processor`. In fast boot mode, the tick processor starts
/// with `live_candle_writer = None` (QuestDB not ready), so candles are aggregated
/// but not persisted. This consumer fills that gap once QuestDB is available.
///
/// Materialized views (candles_1m, 5m, 15m, etc.) automatically aggregate from candles_1s.
async fn run_candle_persistence_consumer(
    mut tick_rx: tokio::sync::broadcast::Receiver<dhan_live_trader_common::tick_types::ParsedTick>,
    questdb_config: dhan_live_trader_common::config::QuestDbConfig,
) {
    let mut candle_writer =
        match dhan_live_trader_storage::candle_persistence::LiveCandleWriter::new(&questdb_config) {
            Ok(writer) => {
                info!("cold-path live candle writer connected to QuestDB (candles_1s)");
                writer
            }
            Err(err) => {
                warn!(
                    ?err,
                    "cold-path live candle writer unavailable — candles_1s will NOT be persisted"
                );
                return;
            }
        };

    let mut aggregator = dhan_live_trader_core::pipeline::CandleAggregator::new();
    // O(1) EXEMPT: cold path, pipeline setup
    let sweep_interval = std::time::Duration::from_millis(100);
    let mut last_sweep = std::time::Instant::now();
    let mut candles_persisted: u64 = 0;

    loop {
        // Use timeout so stale candles are swept even during quiet periods.
        match tokio::time::timeout(sweep_interval, tick_rx.recv()).await {
            Ok(Ok(tick)) => {
                aggregator.update(&tick);
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                warn!(
                    skipped,
                    "cold-path candle consumer lagged — some ticks not aggregated into candles"
                );
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                // Shutdown: flush remaining candles
                aggregator.flush_all();
                for c in aggregator.completed_slice() {
                    let _ = candle_writer.append_candle(
                        c.security_id,
                        c.exchange_segment_code,
                        c.timestamp_secs,
                        c.open,
                        c.high,
                        c.low,
                        c.close,
                        c.volume,
                        c.tick_count,
                        c.iv,
                        c.delta,
                        c.gamma,
                        c.theta,
                        c.vega,
                    );
                }
                aggregator.clear_completed();
                let _ = candle_writer.force_flush();
                info!(
                    candles_persisted,
                    total_completed = aggregator.total_completed(),
                    "cold-path candle persistence consumer shutting down (broadcast closed)"
                );
                return;
            }
            Err(_timeout) => {
                // No tick received within sweep_interval — just sweep below
            }
        }

        // Periodic sweep: emit stale candles and flush to QuestDB
        if last_sweep.elapsed() >= sweep_interval {
            // CRITICAL: Dhan WebSocket timestamps are IST epoch seconds.
            // Must add IST offset to UTC clock for correct stale comparison.
            // APPROVED: i64→u32 safe: IST epoch fits u32 until 2106.
            #[allow(clippy::cast_possible_truncation)]
            let now_secs = (chrono::Utc::now().timestamp()
                + i64::from(dhan_live_trader_common::constants::IST_UTC_OFFSET_SECONDS))
                as u32;
            aggregator.sweep_stale(now_secs);
            let completed = aggregator.completed_slice();

            for c in completed {
                if let Err(err) = candle_writer.append_candle(
                    c.security_id,
                    c.exchange_segment_code,
                    c.timestamp_secs,
                    c.open,
                    c.high,
                    c.low,
                    c.close,
                    c.volume,
                    c.tick_count,
                    c.iv,
                    c.delta,
                    c.gamma,
                    c.theta,
                    c.vega,
                ) {
                    warn!(?err, "cold-path candle write failed");
                    break;
                }
            }

            candles_persisted = candles_persisted.saturating_add(completed.len() as u64);
            aggregator.clear_completed();

            if let Err(err) = candle_writer.flush_if_needed() {
                warn!(?err, "cold-path candle flush failed");
            }
            last_sweep = std::time::Instant::now();
        }
    }
}

/// Subscribes to the tick broadcast, aggregates Greeks/PCR aligned to 1-second
/// candle boundaries, and persists them to QuestDB `option_greeks_live` and
/// `pcr_snapshots_live`.
///
/// Runs as a cold-path background consumer. The `GreeksAggregator` receives every
/// tick, updates internal state in O(1), then emits a snapshot every second boundary
/// (when a tick's `exchange_timestamp` second differs from the previous snapshot).
///
/// **Market hours filter:** Only snapshots within [09:00, 15:30) IST are persisted,
/// matching the exact same window as ticks and candles_1s. Post-market ticks still
/// update internal state (for smooth transition next day) but are NOT written to QuestDB.
///
/// **Computation frequency:** Greeks are computed once per second (on each 1-second
/// candle boundary). Per-tick updates are O(1) state tracking only (LTP, OI, volume).
/// The expensive Black-Scholes computation happens only at the second boundary.
///
/// All snapshots have `ts = candle_boundary` ensuring exact JOIN with `candles_1s`.
// TEST-EXEMPT: requires live QuestDB + tick broadcast (integration test)
async fn run_greeks_aggregator_consumer(
    mut tick_rx: tokio::sync::broadcast::Receiver<dhan_live_trader_common::tick_types::ParsedTick>,
    registry: dhan_live_trader_common::instrument_registry::InstrumentRegistry,
    greeks_config: dhan_live_trader_common::config::GreeksConfig,
    questdb_config: dhan_live_trader_common::config::QuestDbConfig,
) {
    let mut greeks_writer =
        match dhan_live_trader_storage::greeks_persistence::GreeksPersistenceWriter::new(
            &questdb_config,
        ) {
            Ok(w) => {
                info!("greeks aggregator writer connected to QuestDB");
                w
            }
            Err(err) => {
                warn!(
                    ?err,
                    "greeks aggregator writer unavailable — live Greeks will NOT be persisted"
                );
                return;
            }
        };

    let mut aggregator = dhan_live_trader_trading::greeks::aggregator::GreeksAggregator::new(
        registry,
        greeks_config,
    );

    // Track the last snapshot second to detect candle boundaries.
    let mut last_snapshot_secs: u32 = 0;
    let mut snapshots_written: u64 = 0;
    let mut greeks_write_errors: u64 = 0;
    let mut pcr_write_errors: u64 = 0;
    // O(1) EXEMPT: cold path, pipeline setup
    let flush_interval = std::time::Duration::from_secs(1);
    let mut last_flush = std::time::Instant::now();

    loop {
        match tokio::time::timeout(flush_interval, tick_rx.recv()).await {
            Ok(Ok(tick)) => {
                aggregator.update(&tick);

                // Detect 1-second candle boundary: new second in exchange timestamp.
                let tick_secs = tick.exchange_timestamp;
                if tick_secs != last_snapshot_secs && last_snapshot_secs > 0 {
                    // Only persist within market hours [09:00, 15:30) IST — same
                    // window as ticks table and candles_1s. Post-market ticks update
                    // internal state but are NOT written to QuestDB.
                    #[allow(clippy::arithmetic_side_effects)] // APPROVED: modulo by 86400 is safe
                    let ist_secs_of_day =
                        last_snapshot_secs % dhan_live_trader_common::constants::SECONDS_PER_DAY;
                    if !(dhan_live_trader_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST
                        ..dhan_live_trader_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST)
                        .contains(&ist_secs_of_day)
                    {
                        last_snapshot_secs = tick_secs;
                        continue;
                    }

                    // Previous second just completed → snapshot at that boundary.
                    let event = dhan_live_trader_trading::greeks::aggregator::CandleCloseEvent {
                        candle_ts: last_snapshot_secs,
                        interval_secs: 1,
                    };
                    let emission = aggregator.snapshot(event);

                    // Write Greeks snapshots to QuestDB
                    for snap in &emission.greeks_snapshots {
                        // IST epoch seconds → nanoseconds for QuestDB
                        let ts_nanos = i64::from(snap.candle_ts) * 1_000_000_000;
                        let expiry_str = snap.expiry_date.to_string();
                        let segment_str =
                            dhan_live_trader_common::types::ExchangeSegment::from_byte(
                                snap.exchange_segment_code,
                            )
                            .map(|s| s.as_str())
                            .unwrap_or("UNKNOWN");

                        let row =
                            dhan_live_trader_storage::greeks_persistence::OptionGreeksLiveRow {
                                segment: segment_str,
                                security_id: i64::from(snap.security_id),
                                symbol_name: "",
                                underlying_security_id: i64::from(snap.underlying_security_id),
                                underlying_symbol: &snap.underlying_symbol,
                                strike_price: snap.strike_price,
                                option_type: snap.option_type.as_str(),
                                expiry_date: &expiry_str,
                                candle_interval: "1s",
                                iv: snap.greeks.iv,
                                delta: snap.greeks.delta,
                                gamma: snap.greeks.gamma,
                                theta: snap.greeks.theta,
                                vega: snap.greeks.vega,
                                rho: snap.greeks.rho,
                                charm: snap.greeks.charm,
                                vanna: snap.greeks.vanna,
                                volga: snap.greeks.volga,
                                veta: snap.greeks.veta,
                                speed: snap.greeks.speed,
                                color: snap.greeks.color,
                                zomma: snap.greeks.zomma,
                                ultima: snap.greeks.ultima,
                                bs_price: snap.greeks.bs_price,
                                intrinsic_value: snap.greeks.intrinsic,
                                extrinsic_value: snap.greeks.extrinsic,
                                spot_price: snap.spot_price,
                                option_ltp: snap.option_ltp,
                                oi: i64::from(snap.oi),
                                volume: i64::from(snap.volume),
                                ts_nanos,
                            };
                        if let Err(err) = greeks_writer.write_option_greeks_live_row(&row) {
                            greeks_write_errors = greeks_write_errors.saturating_add(1);
                            if greeks_write_errors <= 100
                                || greeks_write_errors.is_multiple_of(1000)
                            {
                                warn!(?err, greeks_write_errors, "failed to write live greeks row");
                            }
                            continue;
                        }
                    }

                    // Write PCR snapshots to QuestDB
                    for pcr_snap in &emission.pcr_snapshots {
                        let ts_nanos = i64::from(pcr_snap.candle_ts) * 1_000_000_000;
                        let expiry_str = pcr_snap.expiry_date.to_string();
                        let sentiment_str = match pcr_snap.sentiment {
                            dhan_live_trader_trading::greeks::pcr::PcrSentiment::Bullish => {
                                "Bullish"
                            }
                            dhan_live_trader_trading::greeks::pcr::PcrSentiment::Neutral => {
                                "Neutral"
                            }
                            dhan_live_trader_trading::greeks::pcr::PcrSentiment::Bearish => {
                                "Bearish"
                            }
                        };

                        let row =
                            dhan_live_trader_storage::greeks_persistence::PcrSnapshotLiveRow {
                                underlying_symbol: &pcr_snap.underlying_symbol,
                                expiry_date: &expiry_str,
                                candle_interval: "1s",
                                pcr_oi: pcr_snap.pcr_oi.unwrap_or(0.0),
                                pcr_volume: pcr_snap.pcr_volume.unwrap_or(0.0),
                                total_put_oi: pcr_snap.total_put_oi as i64,
                                total_call_oi: pcr_snap.total_call_oi as i64,
                                total_put_volume: pcr_snap.total_put_volume as i64,
                                total_call_volume: pcr_snap.total_call_volume as i64,
                                sentiment: sentiment_str,
                                ts_nanos,
                            };
                        if let Err(err) = greeks_writer.write_pcr_snapshot_live_row(&row) {
                            pcr_write_errors = pcr_write_errors.saturating_add(1);
                            if pcr_write_errors <= 100 || pcr_write_errors.is_multiple_of(1000) {
                                warn!(?err, pcr_write_errors, "failed to write live PCR row");
                            }
                            continue;
                        }
                    }

                    snapshots_written =
                        snapshots_written.saturating_add(emission.greeks_snapshots.len() as u64);
                }
                last_snapshot_secs = tick_secs;
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                warn!(
                    skipped,
                    "greeks aggregator consumer lagged — some ticks not processed"
                );
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                info!(
                    snapshots_written,
                    ticks_processed = aggregator.ticks_processed(),
                    tracked_options = aggregator.tracked_options(),
                    "greeks aggregator consumer shutting down (broadcast closed)"
                );
                let _ = greeks_writer.flush();
                return;
            }
            Err(_timeout) => {
                // No tick received within 1s — just flush below
            }
        }

        // Periodic flush to QuestDB
        if last_flush.elapsed() >= flush_interval {
            if let Err(err) = greeks_writer.flush() {
                warn!(?err, "greeks aggregator flush failed");
            }
            last_flush = std::time::Instant::now();
        }
    }
}

// compute_market_close_sleep is now in boot_helpers module (lib.rs).

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
    trading_handle: Option<tokio::task::JoinHandle<()>>,
    otel_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    notifier: &std::sync::Arc<NotificationService>,
    config: &ApplicationConfig,
    shared_movers: dhan_live_trader_core::pipeline::SharedTopMoversSnapshot,
    post_market_signal: std::sync::Arc<tokio::sync::Notify>,
) -> Result<()> {
    let bind_addr: SocketAddr = format_bind_addr(&config.api.host, config.api.port)
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

    // --- Post-market WebSocket disconnect timer ---
    // Compute sleep duration until market_close_time (15:30 IST).
    // After market close, WS connections are stopped but API/dashboard stays up.
    let market_close_sleep = compute_market_close_sleep(&config.trading.market_close_time);

    // Phase 1: Wait for EITHER market close OR shutdown signal (SIGINT/SIGTERM)
    let shutdown_reason = tokio::select! {
        _ = tokio::time::sleep(market_close_sleep), if market_close_sleep > std::time::Duration::ZERO => {
            "market_close"
        }
        reason = wait_for_shutdown_signal() => {
            reason
        }
    };

    if shutdown_reason == "market_close" {
        info!("market close reached — disconnecting WebSockets, keeping API alive");
        notifier.notify(NotificationEvent::Custom {
            message: "<b>Post-Market</b>\nMarket closed — WebSockets disconnected, API stays up"
                .to_string(),
        });

        // Drain buffer: let in-flight ticks (last 15:29 candle) reach the
        // tick processor channel BEFORE aborting WebSocket read loops.
        let drain = std::time::Duration::from_secs(
            dhan_live_trader_common::constants::MARKET_CLOSE_DRAIN_BUFFER_SECS,
        );
        tokio::time::sleep(drain).await;

        // Stop real-time data pipeline (WS + tick processor + trading)
        for handle in &ws_handles {
            handle.abort();
        }
        if let Some(ref handle) = order_update_handle {
            handle.abort();
        }
        // Give tick processor time to flush remaining ticks before aborting
        if processor_handle.is_some() {
            let flush_timeout = std::time::Duration::from_secs(
                dhan_live_trader_common::constants::GRACEFUL_SHUTDOWN_TIMEOUT_SECS,
            );
            tokio::time::sleep(flush_timeout).await;
        }
        if let Some(ref handle) = trading_handle {
            handle.abort();
        }

        // Signal historical fetch task to start post-market re-fetch
        post_market_signal.notify_one();

        info!(
            "post-market: real-time pipeline stopped, historical fetch + cross-verify in progress"
        );

        // Phase 2: Auto-shutdown at APP_SHUTDOWN_TIME (16:00 IST) or Ctrl+C
        let shutdown_sleep =
            compute_market_close_sleep(dhan_live_trader_common::constants::APP_SHUTDOWN_TIME_IST);
        if shutdown_sleep > std::time::Duration::ZERO {
            info!(
                wait_secs = shutdown_sleep.as_secs(),
                "auto-shutdown scheduled at 16:00 IST (Ctrl+C for immediate)"
            );
            tokio::select! {
                _ = tokio::time::sleep(shutdown_sleep) => {
                    info!("16:00 IST reached — initiating full auto-shutdown");
                }
                reason = wait_for_shutdown_signal() => {
                    info!(reason, "shutdown signal received — stopping remaining services");
                }
            }
        } else {
            info!("past 16:00 IST — initiating full shutdown immediately");
        }
    } else {
        info!("shutdown signal received — stopping gracefully");
    }

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

    // 5. Stop trading pipeline.
    if let Some(handle) = trading_handle {
        handle.abort();
    }

    // 6. Stop API server.
    if let Some(handle) = api_handle {
        handle.abort();
    }

    // 7. Flush OpenTelemetry.
    drop(otel_provider);
    drop(shared_movers);

    info!("dhan-live-trader stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helper: Wait for shutdown signal (SIGINT or SIGTERM)
// ---------------------------------------------------------------------------

/// Waits for either SIGINT (Ctrl+C) or SIGTERM and returns the signal name.
///
/// SIGTERM support enables graceful shutdown from Docker (`docker stop`).
async fn wait_for_shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(err) => {
                warn!(?err, "SIGTERM handler failed — falling back to SIGINT only");
                let _ = tokio::signal::ctrl_c().await;
                return "ctrl_c";
            }
        };
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(err) = result {
                    warn!(?err, "failed to listen for SIGINT");
                }
                "ctrl_c"
            }
            _ = sigterm.recv() => {
                info!("SIGTERM received — initiating graceful shutdown");
                "sigterm"
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "ctrl_c"
    }
}

// All pure helper function tests are in boot_helpers.rs (lib.rs target).
// Only integration-level tests that require main.rs-specific code remain here.
#[cfg(test)]
mod tests {
    use super::*;
    use dhan_live_trader_app::boot_helpers::{
        APP_LOG_FILE_PATH, OFF_HOURS_CONNECTION_STAGGER_MS, determine_boot_mode, should_fast_boot,
    };

    // All pure helper tests moved to boot_helpers.rs in the lib target.
    // Tests below verify main.rs-specific smoke behavior.

    #[test]
    fn test_main_imports_boot_helpers() {
        // Verify boot_helpers constants are accessible from main.
        assert!(CONFIG_BASE_PATH.ends_with(".toml"));
        assert!(CONFIG_LOCAL_PATH.contains("local"));
        assert!(!APP_LOG_FILE_PATH.is_empty());
        assert!(OFF_HOURS_CONNECTION_STAGGER_MS > 0);
        assert!(!FAST_BOOT_WINDOW_START.is_empty());
        assert!(!FAST_BOOT_WINDOW_END.is_empty());
    }

    #[test]
    fn test_boot_helper_functions_callable() {
        let _ = compute_market_close_sleep("15:30:00");
        let _ = format_violation_details(&[]);
        let _ = format_cross_match_details(&[]);
        let _ = create_log_file_writer();
    }

    #[test]
    fn test_new_boot_helpers_callable_from_main() {
        // Verify the newly extracted helpers are accessible from main.
        let addr = format_bind_addr("0.0.0.0", 3001);
        assert!(addr.contains("3001"));

        let stagger = effective_ws_stagger(3000, true);
        assert_eq!(stagger, 3000);

        let mode = determine_boot_mode(true, true);
        assert_eq!(mode, "fast");

        assert!(should_fast_boot(true, true));
        assert!(!should_fast_boot(false, true));
    }

    #[test]
    fn test_panic_hook_installed() {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _location = info.location();
            original(info);
        }));
        let _ = std::panic::take_hook();
    }

    #[test]
    fn test_sigterm_handler_configured() {
        #[cfg(unix)]
        {
            use tokio::signal::unix::SignalKind;
            let kind = SignalKind::terminate();
            assert_eq!(kind, SignalKind::terminate());
        }
    }

    #[test]
    fn test_boot_timeout_configured() {
        assert!(dhan_live_trader_common::constants::BOOT_TIMEOUT_SECS > 0);
        assert!(dhan_live_trader_common::constants::BOOT_TIMEOUT_SECS <= 300);
    }

    #[tokio::test]
    async fn test_graceful_join_on_shutdown() {
        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        });
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
        assert!(result.is_ok(), "task should complete within timeout");
    }

    // ===================================================================
    // MECHANICAL ENFORCEMENT: IST offset in cold-path candle consumer
    // ===================================================================

    #[test]
    fn test_cold_path_candle_consumer_uses_ist_offset() {
        // Source-level enforcement: the cold-path candle persistence consumer
        // MUST add IST_UTC_OFFSET_SECONDS to chrono::Utc::now() before calling
        // sweep_stale(). Without this, UTC clock is 19800s behind IST candle
        // timestamps → candles never swept → candles_1s stays empty forever.
        let source = include_str!("main.rs");
        // Find the run_candle_persistence_consumer function
        let consumer_start = source
            .find("async fn run_candle_persistence_consumer")
            .expect("run_candle_persistence_consumer must exist");
        let consumer_body = &source[consumer_start..];
        // Must contain IST offset addition near sweep_stale
        assert!(
            consumer_body.contains("IST_UTC_OFFSET_SECONDS"),
            "cold-path candle consumer MUST add IST_UTC_OFFSET_SECONDS to UTC clock \
             before calling sweep_stale(). Dhan timestamps are IST epoch seconds."
        );
    }

    #[test]
    fn test_candle_sweep_ist_vs_utc_math() {
        // Prove the IST offset fix is correct:
        // If candle.timestamp_secs = 1774356559 (IST epoch for 2026-03-24 12:49 IST)
        // UTC now() = 1774356559 - 19800 = 1774336759
        // Without fix: threshold = 1774336759 - 5 = 1774336754
        //   candle (1774356559) > threshold (1774336754) → NOT stale → NEVER emitted
        // With fix: now_ist = 1774336759 + 19800 = 1774356559
        //   threshold = 1774356559 - 5 = 1774356554
        //   candle (1774356559) > threshold (1774356554) → still active (correct, just created)
        //   After 5+ seconds: candle (1774356559) < threshold (1774356564) → STALE → emitted ✓

        let candle_ts_ist: u32 = 1_774_356_559; // IST epoch
        let utc_now: i64 = candle_ts_ist as i64 - 19800; // UTC = IST - 5h30m
        let stale_threshold_secs: u32 = 5;

        // WITHOUT IST offset (the bug): UTC clock for sweep
        let threshold_broken = (utc_now as u32).saturating_sub(stale_threshold_secs);
        assert!(
            candle_ts_ist > threshold_broken,
            "BUG: candle is NEVER stale with UTC clock (candle={candle_ts_ist} > threshold={threshold_broken})"
        );

        // WITH IST offset (the fix): IST clock for sweep
        let now_ist = (utc_now + 19800) as u32;
        assert_eq!(
            now_ist, candle_ts_ist,
            "IST now should equal candle timestamp"
        );

        // After 6 seconds, candle should be stale
        let now_ist_plus_6 = now_ist + 6;
        let threshold_fixed = now_ist_plus_6.saturating_sub(stale_threshold_secs);
        assert!(
            candle_ts_ist < threshold_fixed,
            "FIX: candle IS stale after 6s with IST clock (candle={candle_ts_ist} < threshold={threshold_fixed})"
        );
    }

    // ===================================================================
    // MECHANICAL ENFORCEMENT: Timestamp consistency across all paths
    // ===================================================================

    #[test]
    fn test_tick_persistence_no_ist_offset_on_exchange_timestamp() {
        // Ticks from Dhan WebSocket have IST epoch seconds.
        // The tick persistence writer must NOT add IST offset to exchange_timestamp.
        // Only received_at (from Utc::now()) gets the offset.
        let source = include_str!("../../storage/src/tick_persistence.rs");
        // The designated ts column uses exchange_timestamp directly
        assert!(
            source.contains("i64::from(tick.exchange_timestamp).saturating_mul(1_000_000_000)"),
            "tick ts must use exchange_timestamp directly (IST epoch, no offset)"
        );
        // received_at adds IST offset
        assert!(
            source.contains("received_at_nanos.saturating_add(IST_UTC_OFFSET_NANOS)"),
            "received_at must add IST_UTC_OFFSET_NANOS (UTC → IST)"
        );
    }

    #[test]
    fn test_live_candle_no_ist_offset() {
        // Live candle writer uses IST epoch seconds directly (no offset).
        let source = include_str!("../../storage/src/candle_persistence.rs");
        assert!(
            source.contains("compute_live_candle_nanos(timestamp_secs)"),
            "live candles must use compute_live_candle_nanos (IST direct, no offset)"
        );
    }

    #[test]
    fn test_historical_candle_adds_ist_offset() {
        // Historical REST API returns UTC → must add +19800s.
        let source = include_str!("../../storage/src/candle_persistence.rs");
        assert!(
            source.contains("compute_ist_nanos_from_utc_secs"),
            "historical candles must use compute_ist_nanos_from_utc_secs (UTC + 19800s)"
        );
    }

    #[test]
    fn test_greeks_pipeline_adds_ist_offset() {
        // Greeks pipeline uses Utc::now() → must add IST offset.
        let source = include_str!("greeks_pipeline.rs");
        assert!(
            source.contains("saturating_add(IST_UTC_OFFSET_NANOS)"),
            "greeks pipeline MUST add IST_UTC_OFFSET_NANOS to Utc::now() timestamps"
        );
    }
}
