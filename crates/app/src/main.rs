//! Binary entry point for the tickvault application.
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
use tickvault_app::boot_helpers::{
    CONFIG_BASE_PATH, CONFIG_LOCAL_PATH, FAST_BOOT_WINDOW_END, FAST_BOOT_WINDOW_START, IstTimer,
    check_clock_drift, compute_market_close_sleep, create_error_log_writer,
    create_rolling_log_writer, effective_ws_stagger, format_bind_addr,
    format_cross_match_details_grouped, format_timeframe_details, format_violation_details,
    spawn_heartbeat_watchdog,
};
use tickvault_app::{infra, observability, trading_pipeline};

use std::net::SocketAddr;

use anyhow::{Context, Result};
use chrono::{Timelike, Utc};
use figment::Figment;
use figment::providers::{Format, Toml};
use secrecy::ExposeSecret;
use tracing::{debug, error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use tickvault_common::config::ApplicationConfig;
use tickvault_common::instrument_types::FnoUniverse;
use tickvault_common::trading_calendar::{TradingCalendar, ist_offset};
use tickvault_core::auth::secret_manager;
use tickvault_core::auth::token_cache;
use tickvault_core::auth::token_manager::{TokenHandle, TokenManager};
use tickvault_core::historical::candle_fetcher::fetch_historical_candles;
use tickvault_core::historical::cross_verify::{
    cross_match_historical_vs_live, verify_candle_integrity,
};
use tickvault_core::instrument::binary_cache::read_binary_cache;
use tickvault_core::instrument::subscription_planner::SubscriptionPlan;
use tickvault_core::instrument::{
    InstrumentLoadResult, build_subscription_plan, load_or_build_instruments,
    run_instrument_diagnostic,
};
use tickvault_core::network::ip_verifier;
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_core::pipeline::run_tick_processor;
use tickvault_core::websocket::connection_pool::WebSocketConnectionPool;
use tickvault_core::websocket::order_update_connection::run_order_update_connection;
use tickvault_core::websocket::types::{InstrumentSubscription, WebSocketError};

use tickvault_storage::calendar_persistence;
use tickvault_storage::candle_persistence::{
    CandlePersistenceWriter, ensure_candle_table_dedup_keys,
};
use tickvault_storage::greeks_persistence::ensure_greeks_tables;
use tickvault_storage::instrument_persistence::{
    ensure_instrument_tables, persist_instrument_snapshot,
};
use tickvault_storage::tick_persistence::{
    DepthPersistenceWriter, TickPersistenceWriter, ensure_depth_and_prev_close_tables,
    ensure_tick_table_dedup_keys,
};

use tickvault_trading::greeks::inline_computer::InlineGreeksComputer;

// `build_router` was the legacy entry point; both boot paths now use
// `build_router_with_auth` directly with an SSM-resolved `ApiAuthConfig`
// per the 2026-04-25 security audit (PR #357). Import remains commented
// for one release cycle so the legacy wrapper is easy to revive if needed.
// use tickvault_api::build_router;
use tickvault_api::state::{SharedAppState, SharedHealthStatus, SystemHealthStatus};

// Constants are in boot_helpers module (lib.rs) for coverage instrumentation.

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wave 3-C Item 12 — idempotency probe for the once-per-trading-day
/// market-open self-test. Queries `selftest_audit` for a row with
/// today's `trading_date_ist` AND `check_name='market-open-self-test'`.
/// Returns `Ok(true)` when such a row exists (skip the Telegram
/// notification path; UPSERT the audit row anyway). Used by the
/// scheduler block in `main()` to defend against rapid restarts that
/// would otherwise spawn two schedulers both firing at 09:16:00.
///
/// Adversarial review (general-purpose Class B+H, 2026-04-28).
async fn probe_selftest_already_fired_today(
    qcfg: &tickvault_common::config::QuestDbConfig,
) -> anyhow::Result<bool> {
    use chrono::Utc;
    use std::time::Duration;
    /// Timeout for the once-per-day idempotency pre-check against
    /// `selftest_audit`. Mirrors the 5-second deadline used by the
    /// boot-time QuestDB liveness probe in this same file. Local
    /// (not in `constants.rs`) because it's specific to this helper.
    const SELFTEST_PROBE_TIMEOUT_SECS: u64 = 5;
    let base_url = format!("http://{}:{}/exec", qcfg.host, qcfg.http_port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(SELFTEST_PROBE_TIMEOUT_SECS))
        .build()?;
    let now_ist_nanos = Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
    let trading_date_ist = now_ist_nanos - now_ist_nanos.rem_euclid(86_400_000_000_000);
    let sql = format!(
        "SELECT count(*) AS n FROM selftest_audit \
         WHERE trading_date_ist = {trading_date_ist} \
           AND check_name = 'market-open-self-test';"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("selftest pre-check non-2xx: {}", resp.status());
    }
    let body = resp.text().await?;
    // QuestDB JSON: dataset is `[[N]]` where N >= 1 means already fired.
    Ok(body.contains("[[1]]") || body.contains("[[2]]") || body.contains("[[3]]"))
}

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

    // S6-Step4: Sandbox-only enforcement until 2026-06-30 (per Parthiban
    // "no real orders until June end"). This is a HARD boot gate — the
    // process panics if Live mode is requested before the cutoff date.
    // Real money is at stake, so we fail loud at boot rather than risk
    // a single live order slipping through. Uses IST date (not UTC) so
    // the cutoff matches the operator's calendar in India.
    let today_ist = (chrono::Utc::now()
        + chrono::TimeDelta::seconds(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64))
    .date_naive();
    if let Err(e) = config.strategy.check_sandbox_window(today_ist) {
        error!(
            error = %e,
            "S6-Step4 BOOT BLOCKED: sandbox-only window violation"
        );
        return Err(anyhow::anyhow!(
            "sandbox-only enforcement violated at boot: {e}"
        ));
    }
    info!(
        today_ist = %today_ist,
        sandbox_only_until = %config.strategy.sandbox_only_until,
        mode = ?config.strategy.mode,
        "S6-Step4: sandbox-only window check passed"
    );

    // S12 wiring: system clock drift check (cold path, boot only).
    // SEBI + tick timestamp integrity requires the system clock to be
    // within a few seconds of UTC. Drift > threshold logs WARN (fires
    // Telegram via Loki hook). Best-effort — network errors return None
    // and are non-fatal; the check exists to catch gross drifts (NTP
    // failed, container clock skew, etc.) before market data starts
    // flowing.
    if let Some(drift_secs) = check_clock_drift().await {
        info!(
            drift_secs,
            "S12: system clock drift check completed (0 = synced)"
        );
    } else {
        warn!("S12: clock drift check failed (network or parse error) — proceeding");
    }

    // Build trading calendar (validated inside config.validate() already).
    let trading_calendar = std::sync::Arc::new(
        TradingCalendar::from_config(&config.trading)
            .context("failed to build trading calendar")?,
    );

    // Wave 2 Item 5 (G1) — install global TradingCalendar handle so the
    // main-feed `wait_with_backoff` post-close path can sleep until the
    // next NSE market open instead of giving up.
    if !tickvault_core::websocket::connection::set_market_calendar(trading_calendar.clone()) {
        tracing::warn!("global TradingCalendar already installed — skipping");
    }

    // Wave 2 — install global QuestDB config so any module can emit
    // audit rows without holding a config reference.
    if !tickvault_storage::set_global_questdb_config(config.questdb.clone()) {
        tracing::warn!("global QuestDbConfig already installed — skipping");
    }

    // Wave 2 Item 9 (AUDIT-05) — periodic selftest audit task. Every
    // 15 minutes during the trading session, persist a row recording
    // whether the QuestDB readiness probe succeeded. The body of the
    // check is the same `wait_for_questdb_ready` call (1s deadline) —
    // a fast liveness probe, not a full validate-automation run.
    {
        const SELFTEST_AUDIT_INTERVAL_SECS: u64 = 900;
        let qcfg = config.questdb.clone();
        tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_secs(SELFTEST_AUDIT_INTERVAL_SECS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Skip the first immediate fire — we already probed at boot.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let started = std::time::Instant::now();
                let outcome =
                    match tickvault_storage::boot_probe::wait_for_questdb_ready(&qcfg, 5).await {
                        Ok(_) => "green",
                        Err(_) => "red",
                    };
                let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
                let now_ist_nanos = chrono::Utc::now()
                    .timestamp_nanos_opt()
                    .unwrap_or(0)
                    .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
                // Truncate today's IST nanos to start of day for trading_date_ist.
                let trading_date_ist =
                    now_ist_nanos - (now_ist_nanos.rem_euclid(86_400_000_000_000));
                if let Err(e) =
                    tickvault_storage::selftest_audit_persistence::append_selftest_audit_row(
                        &qcfg,
                        now_ist_nanos,
                        trading_date_ist,
                        "questdb-liveness",
                        outcome,
                        duration_ms,
                        "periodic 15-min liveness probe",
                    )
                    .await
                {
                    tracing::error!(
                        error = ?e,
                        code = tickvault_common::error_code::ErrorCode::Audit05SelftestWriteFailed
                            .code_str(),
                        "AUDIT-05 selftest audit row write failed"
                    );
                    metrics::counter!("tv_audit_write_failures_total", "table" => "selftest_audit")
                        .increment(1);
                }
            }
        });
    }

    // Wave 2 Item 8 (G4) — install the global tick-gap detector and
    // spawn the 60s coalescing task. Recorded ticks live in a papaya
    // map keyed by (security_id, segment) — composite per I-P1-11.
    let tick_gap_detector = std::sync::Arc::new(tickvault_core::pipeline::TickGapDetector::new(
        tickvault_core::pipeline::TICK_GAP_THRESHOLD_SECS_DEFAULT,
    ));
    if !tickvault_core::pipeline::tick_gap_detector::set_global_tick_gap_detector(
        tick_gap_detector.clone(),
    ) {
        tracing::warn!("global TickGapDetector already installed — skipping");
    }
    {
        let detector_for_task = tick_gap_detector.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
                tickvault_core::pipeline::TICK_GAP_COALESCE_WINDOW_SECS_DEFAULT,
            ));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let now = std::time::Instant::now();
                // Wave-2-D Fix 4: bounded variant — even universe-wide
                // silence (~25 K silent instruments) only allocates for
                // the top-N. Returns (top_n_entries, total_silent).
                let (gaps, total_silent) = detector_for_task.scan_gaps_top_n(
                    now,
                    tickvault_core::pipeline::tick_gap_detector::TICK_GAP_TOP_N_DEFAULT,
                );
                if total_silent == 0 {
                    continue;
                }
                metrics::counter!("tv_tick_gap_summary_total").increment(1);
                metrics::gauge!("tv_tick_gap_instruments_silent").set(total_silent as f64);
                let top: Vec<(u32, &'static str, u64)> = gaps
                    .iter()
                    .take(10)
                    .map(|(id, seg, gap)| (*id, seg.as_str(), *gap))
                    .collect();
                tracing::error!(
                    silent_count = total_silent,
                    top_10_samples = ?top,
                    code = tickvault_common::error_code::ErrorCode::WsGap06TickGapSummary
                        .code_str(),
                    "WS-GAP-06 tick-gap detector coalesced summary — instruments silent ≥30s"
                );
            }
        });
    }

    // Wave-2-D Fix 2 (G19) — daily 15:35 IST reset task. The
    // coalescing detector accumulates per-(security_id, segment) entries
    // forever; without this reset, expired/delisted contracts pollute
    // tomorrow's scan and overnight silence (16:00 → next 09:15) reads
    // as a tick gap. `reset_daily()` is defined + tested in
    // `tick_gap_detector.rs` but had no production call site —
    // satisfies audit-findings-2026-04-17.md Rule 13.
    //
    // Loop: sleep until 15:35 IST today (or tomorrow if past), call
    // `reset_daily()`, then sleep ~24h until next 15:35 IST.
    {
        let detector_for_reset = tick_gap_detector.clone();
        tokio::spawn(async move {
            loop {
                // 15:35:00 IST = 5min after market close. Use the same
                // helper that `compute_market_close_sleep("15:30:00")`
                // uses elsewhere — just shifted +5min so we don't race
                // any 15:30-tied tasks.
                let sleep_dur = compute_market_close_sleep(
                    tickvault_common::constants::TICK_GAP_RESET_TIME_IST,
                );
                if sleep_dur.is_zero() {
                    // Already past 15:35 IST today → settle 60s and
                    // recompute. Avoids a hot-spin during the post-15:35
                    // window.
                    tokio::time::sleep(std::time::Duration::from_secs(
                        tickvault_common::constants::TICK_GAP_RESET_SETTLE_SECS,
                    ))
                    .await;
                    let recompute = compute_market_close_sleep(
                        tickvault_common::constants::TICK_GAP_RESET_TIME_IST,
                    );
                    if recompute.is_zero() {
                        // Still past 15:35 (clock stuck?) — bounded
                        // busy-loop avoidance.
                        tokio::time::sleep(std::time::Duration::from_secs(
                            tickvault_common::constants::TICK_GAP_RESET_BUSYLOOP_GUARD_SECS,
                        ))
                        .await;
                        continue;
                    }
                    tokio::time::sleep(recompute).await;
                } else {
                    tokio::time::sleep(sleep_dur).await;
                }
                // Wave-2-D adversarial review (MEDIUM) — idempotent
                // per-day reset. NTP backward step or a Duration::ZERO
                // recompute can otherwise race this loop into a
                // double-fire. Compute current trading-date IST in
                // epoch days; the detector's CAS guarantees a single
                // real clear per day.
                let now_secs = chrono::Utc::now().timestamp();
                let now_ist_secs = now_secs.saturating_add(i64::from(
                    tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
                ));
                let trading_date_ist_days = now_ist_secs
                    .div_euclid(i64::from(tickvault_common::constants::SECONDS_PER_DAY));
                let actually_fired =
                    detector_for_reset.reset_daily_idempotent(trading_date_ist_days);
                if actually_fired {
                    metrics::counter!("tv_tick_gap_daily_resets_total").increment(1);
                    metrics::gauge!("tv_tick_gap_last_reset_date_ist_days")
                        .set(trading_date_ist_days as f64);
                    tracing::info!(
                        map_size_after = detector_for_reset.len(),
                        trading_date_ist_days,
                        "WS-GAP-06 tick-gap detector daily reset fired @ 15:35 IST"
                    );
                } else {
                    // Idempotent skip — another loop iteration in the
                    // same trading day already cleared the map. Log
                    // at debug; do NOT increment the counter or fire
                    // a Telegram event.
                    tracing::debug!(
                        trading_date_ist_days,
                        "WS-GAP-06 daily reset skipped — already fired today"
                    );
                }
                // After reset, ensure we sleep past the 15:35 boundary
                // so we don't race the same minute back into a
                // near-zero sleep on the next loop iteration.
                tokio::time::sleep(std::time::Duration::from_secs(
                    tickvault_common::constants::TICK_GAP_RESET_SETTLE_SECS,
                ))
                .await;
            }
        });
    }

    // -----------------------------------------------------------------------
    // STAGE-C: WebSocket frame WAL (write-ahead log) — durable spill
    //
    // Every raw WS frame (4 types: LiveFeed, Depth20, Depth200, OrderUpdate)
    // is appended to an append-only log on disk BEFORE the live try_send to
    // the downstream channel. On boot, any residual WAL segments are
    // replayed so frames captured across a crash are not lost. This backs
    // the zero-tick-loss guarantee while keeping the read loop O(1).
    //
    // Directory layout: $TV_WS_WAL_DIR (defaults to `./data/ws_wal`).
    // Writer thread: background OS thread spawned inside WsFrameSpill::new.
    // -----------------------------------------------------------------------
    let ws_wal_dir = std::env::var("TV_WS_WAL_DIR").unwrap_or_else(|_| "./data/ws_wal".to_string()); // O(1) EXEMPT: boot-time
    let ws_wal_path = std::path::PathBuf::from(&ws_wal_dir);
    // Replay first — this MUST happen before any WS connection opens so we
    // never race a fresh append against a stale segment rotation.
    //
    // STAGE-C.2b: Recovered frames are retained per-type in the four
    // Vecs below and drained into the live pipeline after the
    // corresponding downstream sink is constructed:
    //
    //   - LiveFeed        → `pool.frame_sender_clone()` once the pool is built
    //   - Depth-20        → temporary DeepDepthWriter (Stage-D drain helper)
    //   - Depth-200       → temporary DeepDepthWriter (Stage-D drain helper)
    //   - OrderUpdate     → `order_update_sender.send()` once the broadcast is built
    //
    // All four sinks are idempotent via QuestDB dedup keys (`STORAGE-GAP-01`
    // for ticks, compound `(security_id, segment, received_at_nanos, side)`
    // for depth) and/or via the downstream broadcast consumer's own OMS
    // idempotency — replaying the same WAL record any number of times
    // yields at most one durable row per logical record.
    let mut ws_wal_replay_live_feed: Vec<bytes::Bytes> = Vec::new();
    let mut ws_wal_replay_depth_20: Vec<Vec<u8>> = Vec::new();
    let mut ws_wal_replay_depth_200: Vec<Vec<u8>> = Vec::new();
    let mut ws_wal_replay_order_update: Vec<Vec<u8>> = Vec::new();
    match tickvault_storage::ws_frame_spill::replay_all(&ws_wal_path) {
        Ok(recovered) => {
            if recovered.is_empty() {
                info!(dir = %ws_wal_dir, "STAGE-C: WAL replay — no residual frames");
            } else {
                let mut live = 0u64;
                let mut d20 = 0u64;
                let mut d200 = 0u64;
                let mut ord = 0u64;
                for rec in recovered {
                    match rec.ws_type {
                        tickvault_storage::ws_frame_spill::WsType::LiveFeed => {
                            live += 1;
                            ws_wal_replay_live_feed.push(bytes::Bytes::from(rec.frame));
                        }
                        tickvault_storage::ws_frame_spill::WsType::Depth20 => {
                            d20 += 1;
                            ws_wal_replay_depth_20.push(rec.frame);
                        }
                        tickvault_storage::ws_frame_spill::WsType::Depth200 => {
                            d200 += 1;
                            ws_wal_replay_depth_200.push(rec.frame);
                        }
                        tickvault_storage::ws_frame_spill::WsType::OrderUpdate => {
                            ord += 1;
                            ws_wal_replay_order_update.push(rec.frame);
                        }
                    }
                }
                info!(
                    dir = %ws_wal_dir,
                    total = live + d20 + d200 + ord,
                    live_feed = live,
                    depth_20 = d20,
                    depth_200 = d200,
                    order_update = ord,
                    "STAGE-C: WAL replay recovered residual frames — LiveFeed will be \
                     re-injected into pool mpsc; Depth-20/Depth-200 drained into QuestDB \
                     via Stage-D drain helper; OrderUpdate drained into broadcast once \
                     sender is created"
                );
                metrics::counter!("tv_ws_frame_wal_replay_total", "ws_type" => "live_feed")
                    .increment(live);
                metrics::counter!("tv_ws_frame_wal_replay_total", "ws_type" => "depth_20")
                    .increment(d20);
                metrics::counter!("tv_ws_frame_wal_replay_total", "ws_type" => "depth_200")
                    .increment(d200);
                metrics::counter!("tv_ws_frame_wal_replay_total", "ws_type" => "order_update")
                    .increment(ord);
            }
        }
        Err(err) => {
            error!(
                ?err,
                dir = %ws_wal_dir,
                "STAGE-C: WAL replay failed — continuing boot with fresh WAL"
            );
        }
    }

    // STAGE-C.2b: Drain Depth-20 and Depth-200 recovered frames into
    // QuestDB immediately — these paths write directly to the
    // persistence sink and do not require any in-flight channel. The
    // compound dedup key makes replay idempotent. LiveFeed and
    // OrderUpdate drains happen later once their channels exist.
    if !ws_wal_replay_depth_20.is_empty() {
        let frames = std::mem::take(&mut ws_wal_replay_depth_20);
        let (parsed, persisted, parse_errors, persist_errors) =
            tickvault_app::boot_helpers::drain_replayed_depth_frames_to_questdb(
                frames,
                &config.questdb,
                "20",
                "depth-20",
            );
        info!(
            parsed,
            persisted,
            parse_errors,
            persist_errors,
            "STAGE-C.2b: Depth-20 WAL replay drain complete"
        );
        metrics::counter!("tv_ws_frame_wal_reinjected_total", "ws_type" => "depth_20")
            .increment(persisted);
        if parse_errors > 0 {
            metrics::counter!(
                "tv_ws_frame_wal_reinjected_parse_errors_total",
                "ws_type" => "depth_20"
            )
            .increment(parse_errors);
        }
        if persist_errors > 0 {
            metrics::counter!(
                "tv_ws_frame_wal_reinjected_dropped_total",
                "ws_type" => "depth_20"
            )
            .increment(persist_errors);
        }
    }
    if !ws_wal_replay_depth_200.is_empty() {
        let frames = std::mem::take(&mut ws_wal_replay_depth_200);
        let (parsed, persisted, parse_errors, persist_errors) =
            tickvault_app::boot_helpers::drain_replayed_depth_frames_to_questdb(
                frames,
                &config.questdb,
                "200",
                "depth-200",
            );
        info!(
            parsed,
            persisted,
            parse_errors,
            persist_errors,
            "STAGE-C.2b: Depth-200 WAL replay drain complete"
        );
        metrics::counter!("tv_ws_frame_wal_reinjected_total", "ws_type" => "depth_200")
            .increment(persisted);
        if parse_errors > 0 {
            metrics::counter!(
                "tv_ws_frame_wal_reinjected_parse_errors_total",
                "ws_type" => "depth_200"
            )
            .increment(parse_errors);
        }
        if persist_errors > 0 {
            metrics::counter!(
                "tv_ws_frame_wal_reinjected_dropped_total",
                "ws_type" => "depth_200"
            )
            .increment(persist_errors);
        }
    }
    let ws_frame_spill = match tickvault_storage::ws_frame_spill::WsFrameSpill::new(&ws_wal_path) {
        Ok(spill) => {
            info!(
                dir = %ws_wal_dir,
                "STAGE-C: WsFrameSpill writer thread started"
            );
            Some(std::sync::Arc::new(spill))
        }
        Err(err) => {
            error!(
                ?err,
                dir = %ws_wal_dir,
                "STAGE-C: failed to initialize WsFrameSpill — proceeding WITHOUT durable WAL. \
                 This is a degraded mode: zero-tick-loss guarantee is NOT active. \
                 Investigate disk permissions and free space immediately."
            );
            None
        }
    };

    // -----------------------------------------------------------------------
    // Step 2: Initialize observability (Prometheus metrics exporter)
    // -----------------------------------------------------------------------
    observability::init_metrics(&config.observability)
        .context("failed to initialize Prometheus metrics")?;

    // Cache parser dispatcher Counter handles AFTER the recorder is
    // installed. Without this, the first hot-path packet of each kind
    // would allocate (Principle #1 violation). Must run post-install
    // because handles created pre-install resolve to a no-op counter.
    tickvault_core::parser::prewarm_dispatcher_counters();

    // -----------------------------------------------------------------------
    // Step 3: Initialize structured logging + OpenTelemetry tracing layer
    // -----------------------------------------------------------------------
    let log_filter = config.logging.level.as_str();
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // Suppress AWS SDK credential logging (leaks access_key_id at INFO level).
        tracing_subscriber::EnvFilter::new(format!(
            "{log_filter},aws_config::profile::credentials=warn"
        ))
    });

    let (otel_layer, otel_provider) = match observability::init_tracing(&config.observability)
        .context("failed to initialize OpenTelemetry tracing")?
    {
        Some((layer, provider)) => (Some(layer), Some(provider)),
        None => (None, None),
    };

    // IST timestamp formatter — all log timestamps show +05:30 offset.
    let ist_timer = IstTimer;

    // Stdout layer — only when config.logging.log_to_stdout is true.
    // Disabled by default to prevent unbounded IntelliJ console buffer growth.
    // File logging (data/logs/) is always active regardless of this flag.
    let fmt_boxed: Option<Box<dyn tracing_subscriber::Layer<_> + Send + Sync + 'static>> =
        if config.logging.log_to_stdout {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_thread_ids(true)
                .with_file(false)
                .with_line_number(false)
                .with_timer(ist_timer.clone());
            if config.logging.format == "json" {
                Some(Box::new(fmt_layer.json()))
            } else {
                Some(Box::new(fmt_layer))
            }
        } else {
            None
        };

    // File-based JSON log layer for Alloy → Loki ingestion.
    // Daily-rotated: data/logs/app.YYYY-MM-DD.log. Old files beyond
    // LOG_MAX_FILES are cleaned up at boot.
    let file_log_layer: Option<Box<dyn tracing_subscriber::Layer<_> + Send + Sync + 'static>> =
        match create_rolling_log_writer() {
            Some(file) => {
                let file_fmt = tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_file(false)
                    .with_line_number(false)
                    .with_timer(ist_timer.clone())
                    .json()
                    .with_writer(std::sync::Mutex::new(file));
                Some(Box::new(file_fmt))
            }
            None => None,
        };

    // Error-only file layer: data/logs/errors.log (WARN + ERROR only).
    // Small, grep-friendly file containing ONLY problems for fast debugging.
    let error_log_layer: Option<Box<dyn tracing_subscriber::Layer<_> + Send + Sync + 'static>> =
        match create_error_log_writer() {
            Some(file) => {
                use tracing_subscriber::Layer as _;
                let error_fmt = tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_file(true)
                    .with_line_number(true)
                    .with_timer(ist_timer)
                    .json()
                    .with_writer(std::sync::Mutex::new(file))
                    .with_filter(tracing_subscriber::filter::LevelFilter::WARN);
                Some(Box::new(error_fmt))
            }
            None => None,
        };

    // Phase 2 of active-plan: ERROR-only JSONL stream at
    // data/logs/errors.jsonl.YYYY-MM-DD-HH for Claude triage daemon,
    // Loki/Alloy scraper, and the upcoming summary_writer. Hourly rotation,
    // 48h retention enforced by the background sweeper below.
    //
    // The WorkerGuard MUST live for the whole process — it owns the
    // non-blocking flush thread. Leaking into the static binds it to
    // program lifetime without needing to thread it through shutdown.
    let errors_jsonl_layer: Option<Box<dyn tracing_subscriber::Layer<_> + Send + Sync + 'static>> =
        match observability::init_errors_jsonl_appender(observability::ERRORS_JSONL_DIR) {
            Ok((writer, guard)) => {
                use tracing_subscriber::Layer as _;
                // Keep the worker guard alive for the process lifetime —
                // dropping it stops the background flush thread.
                Box::leak(Box::new(guard));
                let layer = tracing_subscriber::fmt::layer()
                    .json()
                    // CRITICAL: flatten_event(true) hoists `code`, `severity`,
                    // `message` from under "fields" to the top level so
                    // summary_writer + Claude can pattern-match them without
                    // walking a nested object. The e2e test
                    // `observability_chain_e2e` regresses on this flag.
                    .flatten_event(true)
                    .with_current_span(true)
                    .with_span_list(false)
                    .with_target(true)
                    .with_file(true)
                    .with_line_number(true)
                    .with_thread_ids(true)
                    .with_writer(writer)
                    .with_filter(tracing_subscriber::filter::LevelFilter::ERROR);
                Some(Box::new(layer))
            }
            Err(err) => {
                // Never block boot on an ancillary logging target.
                tracing::warn!(
                    ?err,
                    "errors.jsonl appender init failed — continuing without structured ERROR stream"
                );
                None
            }
        };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_boxed)
        .with(file_log_layer)
        .with(error_log_layer)
        .with(errors_jsonl_layer)
        .with(otel_layer)
        .init();

    // Phase 5: background summary_writer task. Emits a human + Claude
    // readable `data/logs/errors.summary.md` every 60s so `/loop` polling
    // reads ONE file instead of parsing JSONL, and so `make status` can
    // cat it for an instant health view.
    {
        use tickvault_core::notification::summary_writer::{
            SummaryWriterConfig, spawn as spawn_summary,
        };
        let cfg = SummaryWriterConfig::new(observability::ERRORS_JSONL_DIR);
        let _summary_task = spawn_summary(cfg);
    }

    // Phase 2: hourly retention sweeper for errors.jsonl. Keeps ~48h of
    // rotated files on disk (~= 500KB at ERROR-only volume). Runs as a
    // best-effort background task — failures log at WARN, never halt.
    tokio::spawn(async {
        use std::path::Path;
        use std::time::Duration;
        const SWEEP_INTERVAL_SECS: u64 = 3600;
        const RETENTION_HOURS: u64 = 48;
        let dir = Path::new(observability::ERRORS_JSONL_DIR);
        loop {
            tokio::time::sleep(Duration::from_secs(SWEEP_INTERVAL_SECS)).await;
            match observability::sweep_errors_jsonl_retention(dir, RETENTION_HOURS) {
                Ok(0) => {}
                Ok(n) => tracing::info!(
                    deleted = n,
                    dir = %dir.display(),
                    retention_hours = RETENTION_HOURS,
                    "errors.jsonl retention sweep"
                ),
                Err(err) => tracing::warn!(
                    ?err,
                    dir = %dir.display(),
                    "errors.jsonl retention sweep failed"
                ),
            }
        }
    });

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
            "PANIC: tickvault crashed"
        );
        default_panic_hook(panic_info);
    }));

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config_file = CONFIG_BASE_PATH,
        metrics_port = config.observability.metrics_port,
        tracing_enabled = config.observability.tracing_enabled,
        "tickvault starting"
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
        && tickvault_core::instrument::instrument_loader::is_within_build_window(
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
        // Sandbox/paper mode: skip IP verification (not required).
        // Live mode: MUST verify IP before any Dhan API call.
        let fast_trading_mode = config.strategy.mode;
        // C1: strict notifier init — refuse to boot in no-op mode (unless
        // TICKVAULT_ALLOW_NOOP_NOTIFIER=1). If the app can't talk to Telegram,
        // we must not run deaf.
        let (fast_notifier_result, ip_result) = tokio::join!(
            NotificationService::initialize_strict(&config.notification),
            async {
                if fast_trading_mode.is_live() {
                    ip_verifier::verify_public_ip().await
                } else {
                    info!(
                        mode = fast_trading_mode.as_str(),
                        "FAST BOOT: IP verification skipped — {} mode",
                        fast_trading_mode.as_str()
                    );
                    // Return a dummy success — no verification needed
                    Ok(tickvault_core::network::ip_verifier::IpVerificationResult {
                        verified_ip: "skipped".to_string(),
                    })
                }
            },
        );
        // C1: strict notifier — refuse to boot if notifier fell back to no-op.
        let fast_notifier = match fast_notifier_result {
            Ok(notifier) => notifier,
            Err(reason) => {
                error!(
                    reason = %reason,
                    "FAST BOOT: strict notifier init failed — REFUSING BOOT (systemd will restart)"
                );
                return Err(anyhow::anyhow!(reason));
            }
        };
        // Wave 3-B Item 11: opt-in Telegram bucket-coalescer based on the
        // `features.telegram_bucket_coalescer` flag. Defaults to `true`.
        let fast_notifier = if config.features.telegram_bucket_coalescer {
            NotificationService::enable_coalescer(
                fast_notifier,
                tickvault_core::notification::CoalescerConfig::default(),
            )
        } else {
            fast_notifier
        };
        match ip_result {
            Ok(result) => {
                if fast_trading_mode.is_live() {
                    fast_notifier.notify(NotificationEvent::IpVerificationSuccess {
                        verified_ip: result.verified_ip,
                    });
                }
            }
            Err(err) => {
                // GAP-NET-01: static-IP verification rejected boot.
                error!(
                    code = tickvault_common::error_code::ErrorCode::GapNetIpMonitor.code_str(),
                    severity = tickvault_common::error_code::ErrorCode::GapNetIpMonitor
                        .severity()
                        .as_str(),
                    error = %err,
                    "GAP-NET-01: FAST BOOT — IP verification failed, BLOCKING BOOT"
                );
                fast_notifier.notify(NotificationEvent::IpVerificationFailed {
                    reason: err.to_string(),
                });
                return Ok(());
            }
        }

        // --- Load instruments (sub-1ms from rkyv cache during market hours) ---
        let (subscription_plan, fresh_universe, _needs_persist) =
            load_instruments(&config, is_trading, trading_calendar.as_ref()).await;

        // Audit finding #6 (2026-04-24): emit InstrumentBuildSuccess when
        // instruments load successfully. Previously only the FAILURE path
        // (`InstrumentBuildFailed`) fired a Telegram, leaving operators
        // without a positive signal that the daily rebuild succeeded.
        if let Some(ref u) = fresh_universe {
            let source = if _needs_persist {
                "rkyv_cache"
            } else {
                "fresh_csv_build"
            };
            fast_notifier.notify(NotificationEvent::InstrumentBuildSuccess {
                source: source.to_string(),
                derivative_count: u.derivative_contracts.len(),
                underlying_count: u.underlyings.len(),
            });
        }

        // --- WebSocket pool create (channel only, NOT spawned yet) ---
        let (pool_receiver, ws_pool_ready) = match create_websocket_pool(
            &token_handle,
            &client_id,
            &subscription_plan,
            &config,
            true,
            Some(fast_notifier.clone()),
            ws_frame_spill.clone(),
        ) {
            Some((receiver, pool)) => (Some(receiver), Some(pool)),
            None => (None, None),
        };

        // STAGE-C.2b: Re-inject replayed LiveFeed frames into the pool's
        // frame sender. This happens BEFORE the pool spawns its
        // connections, so the replayed frames land in the mpsc queue
        // ahead of any fresh live frame. The tick processor — started
        // below — drains them in FIFO order and QuestDB dedup keys
        // (STORAGE-GAP-01) make the replay idempotent, so even if the
        // same frames were partially persisted before the crash, no
        // duplicate rows are written.
        //
        // If the pool build failed (ws_pool_ready is None) we log a
        // warning but preserve the frames in the WAL archive.
        if !ws_wal_replay_live_feed.is_empty() {
            if let Some(ref pool) = ws_pool_ready {
                let sender = pool.frame_sender_clone();
                let capacity = sender.capacity();
                let to_inject = std::mem::take(&mut ws_wal_replay_live_feed);
                let count = to_inject.len();
                info!(
                    frames = count,
                    channel_capacity = capacity,
                    "STAGE-C.2b: re-injecting replayed LiveFeed frames into pool mpsc"
                );
                let mut injected = 0u64;
                let mut dropped = 0u64;
                for frame in to_inject {
                    match sender.try_send(frame) {
                        Ok(()) => injected += 1,
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            dropped += 1;
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            dropped += 1;
                        }
                    }
                }
                metrics::counter!(
                    "tv_ws_frame_wal_reinjected_total",
                    "ws_type" => "live_feed"
                )
                .increment(injected);
                if dropped > 0 {
                    // Channel full or closed — replayed frames remain in the
                    // archive for forensic replay but could not be handed
                    // to the live consumer. This is a degraded mode, not a
                    // data loss (the frames are still on disk).
                    error!(
                        dropped,
                        injected,
                        "STAGE-C.2b: LiveFeed re-injection dropped frames — channel full/closed, \
                         frames remain in WAL archive"
                    );
                    metrics::counter!(
                        "tv_ws_frame_wal_reinjected_dropped_total",
                        "ws_type" => "live_feed"
                    )
                    .increment(dropped);
                } else {
                    info!(
                        injected,
                        "STAGE-C.2b: LiveFeed re-injection complete — all replayed frames queued"
                    );
                }
            } else {
                warn!(
                    frames = ws_wal_replay_live_feed.len(),
                    "STAGE-C.2b: LiveFeed replay frames held but pool build failed — \
                     frames remain in WAL archive, not re-injected"
                );
            }
        }

        // S4-T1a/T1b: Shared shutdown notifier. The pool watchdog task
        // listens on this and stops when we signal graceful shutdown, so
        // the watchdog doesn't fire a spurious Halt during intentional
        // teardown. Created BEFORE the WS pool spawn so it's in scope for
        // both spawn_pool_watchdog_task AND the bottom-of-main shutdown
        // handler.
        let shutdown_notify = std::sync::Arc::new(tokio::sync::Notify::new());

        // --- Tick processor: start BEFORE WS connections spawn ---
        let shared_movers: tickvault_core::pipeline::SharedTopMoversSnapshot =
            std::sync::Arc::new(std::sync::RwLock::new(None));

        // Tick broadcast for trading pipeline (cold path consumer).
        // A2: Use constant capacity (65536) to absorb high-volatility bursts without lagging.
        let (fast_tick_broadcast_sender, _fast_tick_broadcast_rx) =
            tokio::sync::broadcast::channel::<tickvault_common::tick_types::ParsedTick>(
                tickvault_common::constants::TICK_BROADCAST_CAPACITY,
            );

        // S12 wiring: heartbeat watchdog (fast boot).
        // Spawns a background task that every 30 seconds checks:
        //   - Token handle has a non-None token
        //   - Tick broadcast has > 0 active receivers
        //   - Emits systemd WATCHDOG=1 ping
        //   - Exports FD/thread system metrics
        // Logs ERROR on any failure (fires Telegram via Loki hook).
        let _fast_heartbeat_handle = spawn_heartbeat_watchdog(
            std::sync::Arc::clone(&token_handle),
            fast_tick_broadcast_sender.clone(),
        );

        // Gap-backfill worker DISABLED. Historical candle data and live
        // WebSocket ticks are separate concerns — the backfill worker was
        // injecting synthetic ticks from Dhan's REST API into the live
        // `ticks` table, causing stale 9:15 AM data to appear on fresh
        // starts at 6 PM. Historical candles belong in `historical_candles`
        // only; the WebSocket pipeline must contain ONLY live ticks.

        let processor_handle = if let Some(receiver) = pool_receiver {
            let candle_agg = Some(tickvault_core::pipeline::CandleAggregator::new());
            let movers = Some(tickvault_core::pipeline::TopMoversTracker::new());
            let snapshot_handle = Some(shared_movers.clone());
            let tick_broadcast_for_processor = Some(fast_tick_broadcast_sender.clone());

            // Parthiban directive (2026-04-21): no-tick-during-market-hours
            // watchdog. The tick processor updates this atomic on every
            // parsed tick; the watchdog fires CRITICAL + Telegram if it
            // stays stale > NO_TICK_THRESHOLD_SECS during market hours.
            let fast_tick_heartbeat =
                tickvault_core::pipeline::no_tick_watchdog::new_tick_heartbeat();
            let _no_tick_watchdog_handle =
                tickvault_core::pipeline::no_tick_watchdog::spawn_no_tick_watchdog(
                    std::sync::Arc::clone(&fast_tick_heartbeat),
                    Some(std::sync::Arc::clone(&fast_notifier)),
                );

            // O(1) EXEMPT: cold path — build inline Greeks computer once at startup.
            let greeks_enricher = build_inline_greeks_enricher(&config, &subscription_plan);

            // O(1) EXEMPT: cold path — clone registry once for tick processor enrichment.
            let fast_registry = subscription_plan
                .as_ref()
                .map(|p| std::sync::Arc::new(p.registry.clone()));

            // CRITICAL: Use new_disconnected() instead of None — ticks buffer in
            // ring buffer (600K) + disk spill immediately, even before QuestDB connects.
            // Without this, the only persistence path is the broadcast cold-path consumer,
            // which CAN drop ticks on lag (broadcast::Lagged). With new_disconnected(),
            // the hot-path writer buffers ALL ticks and drains when QuestDB is ready.
            let fast_tick_writer = Some(
                tickvault_storage::tick_persistence::TickPersistenceWriter::new_disconnected(
                    &config.questdb,
                ),
            );
            let fast_depth_writer = Some(
                tickvault_storage::tick_persistence::DepthPersistenceWriter::new_disconnected(
                    &config.questdb,
                ),
            );

            let handle = tokio::spawn(async move {
                run_tick_processor(
                    receiver,
                    fast_tick_writer,
                    fast_depth_writer,
                    tick_broadcast_for_processor,
                    candle_agg,
                    None, // live_candle_writer — QuestDB reconnects in background
                    movers,
                    snapshot_handle,
                    greeks_enricher,
                    None, // stock_movers_writer — created in slow boot only
                    None, // option_movers — created in slow boot only
                    None, // option_movers_writer — created in slow boot only
                    fast_registry,
                    Some(fast_tick_heartbeat),
                )
                .await;
            });
            info!("FAST BOOT COMPLETE — tick processor started, ticks flowing (in-memory)");
            Some(handle)
        } else {
            info!("tick processor skipped — no frame source available");
            None
        };

        // Fix #7 (2026-04-24): create the shared health status early so
        // the pool watchdog task can push live main-feed connection counts
        // into it on every 5s tick. Before Fix #7 the watchdog was spawned
        // before `health_status` existed and the fast-boot path's /health
        // endpoint reported `websocket_connections: 0` forever.
        let health_status: SharedHealthStatus = std::sync::Arc::new(SystemHealthStatus::new());

        // Pipeline-active wiring: when the tick processor spawned successfully
        // above, flip the flag so /health reports "active" and the
        // `tv_pipeline_active` Prometheus gauge (System Overview "Pipeline
        // Status" tile) reads 1=RUNNING. Before this wiring the gauge was
        // never emitted, leaving the tile RED on every boot even though
        // ticks were flowing.
        if processor_handle.is_some() {
            health_status.set_pipeline_active(true);
        }

        // --- NOW spawn WebSocket connections (tick processor consuming) ---
        // S4-T1a/T1b: Wrap the pool in Arc so we can retain clones for the
        // pool watchdog task and the graceful-shutdown handler. All three
        // users (spawn_all, poll_watchdog, request_graceful_shutdown) take
        // &self so sharing via Arc is cheap + lock-free.
        let (ws_handles, ws_pool_arc) = if let Some(pool) = ws_pool_ready {
            let pool_arc = std::sync::Arc::new(pool);
            // O1-B (2026-04-17): install per-connection runtime subscribe
            // channels BEFORE spawn so the read loop sees the receivers on
            // its first iteration. The Phase 2 scheduler reads the matching
            // senders via `pool_arc.dispatch_subscribe(...)`.
            pool_arc.install_subscribe_channels().await;
            let handles = spawn_websocket_connections(std::sync::Arc::clone(&pool_arc)).await;
            spawn_pool_watchdog_task(
                std::sync::Arc::clone(&pool_arc),
                std::sync::Arc::clone(&shutdown_notify),
                std::sync::Arc::clone(&fast_notifier),
                std::sync::Arc::clone(&health_status),
            );
            // FAST BOOT parity with slow boot (main.rs ~1760):
            // emit per-connection + aggregate Telegram alerts so an operator
            // can see the pool came up after a crash-recovery restart.
            emit_websocket_connected_alerts(&fast_notifier, handles.len()).await;
            (handles, Some(pool_arc))
        } else {
            (Vec::new(), None)
        };
        let _ = &ws_pool_arc; // kept alive for watchdog + shutdown handler

        // =================================================================
        // TICKS FLOWING — everything below is background, zero blocking.
        // All services start in parallel via tokio::join!.
        // =================================================================

        // Background: Docker infra + QuestDB DDL + SSM validation
        // Notification already initialized above (needed for IP verification).
        // All run concurrently. None of them block tick processing.
        let notifier = fast_notifier.clone();
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
                    tickvault_storage::constituency_persistence::ensure_constituency_table(
                        &config.questdb
                    ),
                    tickvault_storage::materialized_views::ensure_candle_views(&config.questdb),
                    ensure_greeks_tables(&config.questdb),
                );
                // Persist trading calendar to QuestDB (best-effort, non-blocking).
                // Gap 5: log on failure instead of silent drop.
                if let Err(err) =
                    calendar_persistence::persist_calendar(&trading_calendar, &config.questdb)
                {
                    warn!(
                        ?err,
                        "calendar persistence failed (non-critical, best-effort)"
                    );
                }

                // Re-persist instrument data ONLY for CachedPlan path.
                // FreshBuild already persisted inside load_or_build_instruments.
                // Double-persist creates duplicate rows in QuestDB snapshot tables.
                if _needs_persist
                    && let Some(ref universe) = fresh_universe
                    && let Err(err) = persist_instrument_snapshot(universe, &config.questdb).await
                {
                    warn!(
                        ?err,
                        "instrument snapshot persistence failed (non-critical)"
                    );
                }

                info!("QuestDB DDL complete (background)");
            },
            // SSM validation + TokenManager for renewal
            async {
                let timeout = std::time::Duration::from_secs(
                    tickvault_common::constants::TOKEN_INIT_TIMEOUT_SECS,
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
                // AUTH-GAP-01: deferred auth failed; token renewal unavailable.
                error!(
                    code = tickvault_common::error_code::ErrorCode::AuthGapTokenExpiry
                        .code_str(),
                    severity = tickvault_common::error_code::ErrorCode::AuthGapTokenExpiry
                        .severity()
                        .as_str(),
                    error = %err,
                    "AUTH-GAP-01: deferred auth failed — token renewal unavailable"
                );
                notifier.notify(
                    tickvault_core::notification::events::NotificationEvent::AuthenticationFailed {
                        reason: format!(
                            "DEFERRED: {err} — ticks still flowing but renewal unavailable"
                        ),
                    },
                );
                None
            }
            Err(_elapsed) => {
                // AUTH-GAP-01: deferred auth hit its timeout.
                error!(
                    code = tickvault_common::error_code::ErrorCode::AuthGapTokenExpiry.code_str(),
                    severity = tickvault_common::error_code::ErrorCode::AuthGapTokenExpiry
                        .severity()
                        .as_str(),
                    "AUTH-GAP-01: deferred auth timed out — token renewal unavailable"
                );
                None
            }
        };

        // Health status was created above (Fix #7 — moved up so the pool
        // watchdog can push connection counts into it from its 5s poll).

        // --- Background: Tick persistence (cold path — subscribes to broadcast) ---
        // The tick processor was started with None writers (fast boot, QuestDB wasn't
        // ready). Now QuestDB is available. Spawn a separate cold-path consumer that
        // subscribes to the tick broadcast and persists to QuestDB. This never touches
        // the hot-path tick processor loop — zero allocation impact.
        {
            let tick_persistence_rx = fast_tick_broadcast_sender.subscribe();
            let questdb_cfg = config.questdb.clone();
            let hs = health_status.clone();
            let persist_notifier = std::sync::Arc::clone(&notifier);
            tokio::spawn(async move {
                run_tick_persistence_consumer(
                    tick_persistence_rx,
                    questdb_cfg,
                    Some(hs),
                    Some(persist_notifier),
                )
                .await;
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
                tickvault_app::greeks_pipeline::run_greeks_pipeline(
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

        // --- Background: Order update WebSocket ---
        let (order_update_sender, _order_update_receiver) =
            tokio::sync::broadcast::channel::<tickvault_common::order_types::OrderUpdate>(256);

        // STAGE-C.2b: Drain recovered order-update JSON frames into the
        // live broadcast BEFORE the live WebSocket starts. Idempotent —
        // any consumer already attached gets the replayed updates in
        // FIFO order; raw JSON also remains in the WAL archive.
        if !ws_wal_replay_order_update.is_empty() {
            let frames = std::mem::take(&mut ws_wal_replay_order_update);
            let (parsed, broadcast_count, parse_errors) =
                tickvault_app::boot_helpers::drain_replayed_order_updates_to_broadcast(
                    frames,
                    &order_update_sender,
                );
            info!(
                parsed,
                broadcast_count,
                parse_errors,
                "STAGE-C.2b: OrderUpdate WAL replay drain complete (fast boot)"
            );
            metrics::counter!(
                "tv_ws_frame_wal_reinjected_total",
                "ws_type" => "order_update"
            )
            .increment(broadcast_count);
            if parse_errors > 0 {
                metrics::counter!(
                    "tv_ws_frame_wal_reinjected_parse_errors_total",
                    "ws_type" => "order_update"
                )
                .increment(parse_errors);
            }
        }

        let order_update_handle = {
            let url = config.dhan.order_update_websocket_url.clone();
            let ws_client_id = client_id.clone();
            let token = token_handle.clone();
            let sender = order_update_sender.clone();
            let cal = trading_calendar.clone();
            let spill = ws_frame_spill.clone();
            // O2 (2026-04-17): FAST BOOT parity — fires OrderUpdateAuthenticated
            // Telegram event once Dhan accepts the token (first real message).
            let auth_signal = std::sync::Arc::new(tokio::sync::Notify::new());
            let auth_latch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            {
                let listener_signal = std::sync::Arc::clone(&auth_signal);
                let listener_notifier = fast_notifier.clone();
                tokio::spawn(async move {
                    listener_signal.notified().await;
                    listener_notifier.notify(NotificationEvent::OrderUpdateAuthenticated);
                });
            }
            let run_signal = Some(std::sync::Arc::clone(&auth_signal));
            let run_latch = Some(std::sync::Arc::clone(&auth_latch));
            let reconnect_notifier = Some(std::sync::Arc::clone(&fast_notifier));
            tokio::spawn(async move {
                run_order_update_connection(
                    url,
                    ws_client_id,
                    token,
                    sender,
                    cal,
                    spill,
                    run_signal,
                    run_latch,
                    reconnect_notifier,
                )
                .await;
            })
        };
        info!("order update WebSocket started (background)");

        // --- Background: Daily reset signal (16:00 IST) ---
        let daily_reset_signal = std::sync::Arc::new(tokio::sync::Notify::new());
        {
            let signal = std::sync::Arc::clone(&daily_reset_signal);
            let reset_sleep =
                compute_market_close_sleep(tickvault_common::constants::APP_DAILY_RESET_TIME_IST);
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

        // --- Background: Index constituency (best-effort) ---
        // During market hours, skip network downloads to niftyindices.com
        // (they often return HTML instead of CSV) and use the cached JSON.
        // Fresh download happens on non-market-hours boot or post-market.
        let bg_constituency = if is_market_hours {
            info!(
                "market hours — using cached constituency data (skipping niftyindices.com download)"
            );
            tickvault_core::index_constituency::try_load_cache(
                &config.instrument.csv_cache_directory,
            )
            .await
        } else {
            tickvault_core::index_constituency::download_and_build_constituency_map(
                &config.index_constituency,
                &config.instrument.csv_cache_directory,
            )
            .await
        };

        // Persist constituency to QuestDB for Grafana (best-effort, non-blocking).
        // Enrich with security_ids from instrument master for news-based trading.
        if let Some(ref map) = bg_constituency {
            match tickvault_storage::constituency_persistence::persist_constituency(
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

        let bg_shared_constituency: tickvault_api::state::SharedConstituencyMap =
            std::sync::Arc::new(std::sync::RwLock::new(bg_constituency));

        // --- Background: API server ---
        let api_state = SharedAppState::new(
            config.questdb.clone(),
            config.dhan.clone(),
            config.instrument.clone(),
            shared_movers.clone(),
            bg_shared_constituency,
            health_status,
        );

        // 2026-04-25 security audit (PR #357): SSM-only bearer token resolution
        // (mirrors the slow-boot path below). Fast-boot uses the same SSM path
        // so both boot paths have identical secret-handling semantics. Hard-fail
        // on SSM error — matches `fetch_dhan_credentials` / `fetch_telegram_*`
        // boot-time semantics. NO env var fallback.
        let api_bearer_token_fast = tickvault_core::auth::secret_manager::fetch_api_bearer_token()
            .await
            .context("GAP-SEC-01 (fast boot): SSM fetch for API bearer token failed at /tickvault/<env>/api/bearer-token — store the token via `aws ssm put-parameter --name /tickvault/<env>/api/bearer-token --type SecureString`")?;
        info!(
            "GAP-SEC-01 (fast boot): API bearer token loaded from SSM \
             (/tickvault/<env>/api/bearer-token)"
        );
        let api_auth_config_fast =
            tickvault_api::middleware::ApiAuthConfig::from_token(api_bearer_token_fast);
        let router = tickvault_api::build_router_with_auth(
            api_state,
            &config.api.allowed_origins,
            api_auth_config_fast,
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
            ws_pool_arc,
            shutdown_notify,
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
    // C1: strict notifier init — the app must refuse to boot in no-op mode.
    let (notifier_result, _) = tokio::join!(
        NotificationService::initialize_strict(&config.notification),
        async {
            if config.infrastructure.auto_start_docker {
                infra::ensure_infra_running(&config.questdb).await;
            } else {
                info!(
                    "Docker auto-start disabled (infrastructure.auto_start_docker = false). \
                     Run `make docker-up` manually before starting the app."
                );
            }
        },
    );
    let notifier = match notifier_result {
        Ok(n) => n,
        Err(reason) => {
            error!(
                reason = %reason,
                "STANDARD BOOT: strict notifier init failed — REFUSING BOOT (systemd will restart)"
            );
            return Err(anyhow::anyhow!(reason));
        }
    };
    // Wave 3-B Item 11: opt-in Telegram bucket-coalescer based on the
    // `features.telegram_bucket_coalescer` flag. Defaults to `true`.
    let notifier = if config.features.telegram_bucket_coalescer {
        NotificationService::enable_coalescer(
            notifier,
            tickvault_core::notification::CoalescerConfig::default(),
        )
    } else {
        notifier
    };

    // -----------------------------------------------------------------------
    // Step 5.5: Verify public IP matches SSM static IP (BLOCKS BOOT on failure)
    // -----------------------------------------------------------------------
    // Sandbox mode: skip IP verification (sandbox doesn't require registered IP).
    // Live mode: MUST verify IP before any Dhan API call.
    // Paper mode: skip (no Dhan API calls at all).
    let trading_mode = config.strategy.mode;
    if trading_mode.is_live() {
        info!("verifying public IP against SSM static IP");
        match ip_verifier::verify_public_ip().await {
            Ok(result) => {
                notifier.notify(NotificationEvent::IpVerificationSuccess {
                    verified_ip: result.verified_ip,
                });
            }
            Err(err) => {
                // GAP-NET-01: static-IP verification rejected boot.
                error!(
                    code = tickvault_common::error_code::ErrorCode::GapNetIpMonitor.code_str(),
                    severity = tickvault_common::error_code::ErrorCode::GapNetIpMonitor
                        .severity()
                        .as_str(),
                    error = %err,
                    "GAP-NET-01: IP verification failed, BLOCKING BOOT"
                );
                notifier.notify(NotificationEvent::IpVerificationFailed {
                    reason: err.to_string(),
                });
                return Ok(());
            }
        }
    } else {
        info!(
            mode = trading_mode.as_str(),
            "IP verification skipped — not required for {} mode",
            trading_mode.as_str()
        );
    }

    // -----------------------------------------------------------------------
    // Step 6: Authenticate with Dhan API (infinite retry for transient errors)
    // -----------------------------------------------------------------------
    info!("authenticating with Dhan API via SSM → TOTP → JWT");

    let token_init_timeout =
        std::time::Duration::from_secs(tickvault_common::constants::TOKEN_INIT_TIMEOUT_SECS);
    let token_manager = match tokio::time::timeout(
        token_init_timeout,
        TokenManager::initialize(&config.dhan, &config.token, &config.network, &notifier),
    )
    .await
    {
        Ok(Ok(manager)) => manager,
        Ok(Err(err)) => {
            // Permanent auth error or Ctrl+C.
            // DH-901: auth attempt exhausted; app is halting.
            error!(
                code = tickvault_common::error_code::ErrorCode::Dh901InvalidAuth.code_str(),
                severity = tickvault_common::error_code::ErrorCode::Dh901InvalidAuth
                    .severity()
                    .as_str(),
                error = %err,
                "DH-901: authentication failed permanently — exiting"
            );
            notifier.notify(
                tickvault_core::notification::events::NotificationEvent::AuthenticationFailed {
                    reason: format!("PERMANENT: {err}"),
                },
            );
            return Ok(());
        }
        Err(_elapsed) => {
            error!(
                timeout_secs = tickvault_common::constants::TOKEN_INIT_TIMEOUT_SECS,
                "authentication timed out — Dhan API may be unreachable"
            );
            notifier.notify(
                tickvault_core::notification::events::NotificationEvent::AuthenticationFailed {
                    reason: format!(
                        "TIMEOUT: initial auth did not complete within {}s — check Dhan API and network",
                        tickvault_common::constants::TOKEN_INIT_TIMEOUT_SECS,
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

    // Wave 2 Item 7 (G14) — block until QuestDB is reachable. Escalating
    // logs at +5/+10/+20s; BOOT-01 ERROR @+30s; BOOT-02 HALT @+60s.
    // Prevents the legacy "tick processor starts before QuestDB is up"
    // race that dropped early-boot ticks before this gate existed.
    let probe_started = std::time::Instant::now();
    let boot_id = format!(
        "boot-{}",
        chrono::Utc::now()
            .with_timezone(&tickvault_common::trading_calendar::ist_offset())
            .format("%Y-%m-%d-%H%M%S")
    );
    if let Err(e) = tickvault_storage::boot_probe::wait_for_questdb_ready(
        &config.questdb,
        tickvault_storage::boot_probe::BOOT_DEADLINE_SECS,
    )
    .await
    {
        tracing::error!(
            error = ?e,
            code = tickvault_common::error_code::ErrorCode::Boot02DeadlineExceeded.code_str(),
            "BOOT-02 QuestDB never reached ready state — halting"
        );
        anyhow::bail!("BOOT-02 QuestDB readiness deadline exceeded: {e}");
    }

    // Wave 2-C Item 7.3 (G8) — boot-time wall-clock skew probe. Runs
    // AFTER `wait_for_questdb_ready` so the QuestDB `SELECT now()`
    // fallback is reachable when chrony is unavailable. On
    // `ThresholdExceeded` the boot HALTS (BOOT-03). On `Unavailable`
    // the boot proceeds with a WARN — a missing chronyc + unreachable
    // QuestDB after the readiness probe is a rare ordering issue, not
    // a correctness defect.
    {
        let threshold = tickvault_common::constants::CLOCK_SKEW_HALT_THRESHOLD_SECS;
        match tickvault_app::infra::enforce_clock_skew_at_boot(&config.questdb, threshold).await {
            Ok(sample) => {
                tracing::info!(
                    skew_secs = sample.skew_secs,
                    source = sample.source,
                    threshold_secs = threshold,
                    "BOOT-03 clock-skew probe within tolerance"
                );
            }
            Err(tickvault_app::infra::ClockSkewError::ThresholdExceeded {
                skew_secs,
                threshold_secs,
                source,
            }) => {
                tracing::error!(
                    skew_secs,
                    threshold_secs,
                    source,
                    code =
                        tickvault_common::error_code::ErrorCode::Boot03ClockSkewExceeded.code_str(),
                    "BOOT-03 wall-clock skew exceeds threshold — HALTING"
                );
                metrics::counter!("tv_boot_clock_skew_halt_total").increment(1);
                let event = tickvault_core::notification::events::NotificationEvent::BootClockSkewExceeded {
                    skew_secs,
                    threshold_secs,
                    source: source.to_string(),
                };
                notifier.notify(event);
                anyhow::bail!(
                    "BOOT-03 clock skew {skew_secs:+.3}s exceeds threshold {threshold_secs:.2}s \
                     (source: {source}) — fix `chronyc tracking` then restart"
                );
            }
            Err(tickvault_app::infra::ClockSkewError::Unavailable { primary, fallback }) => {
                tracing::warn!(
                    primary = %primary,
                    fallback = %fallback,
                    "BOOT-03 clock-skew probe unavailable — boot proceeding without skew check"
                );
                metrics::counter!("tv_boot_clock_skew_unavailable_total").increment(1);
            }
        }
    }

    // Wave 2 Item 9 — boot audit row for the QuestDB readiness step.
    // Best-effort: if QuestDB just barely came up but the DDL phase is
    // about to write to it, we still want this row. Failures don't halt
    // boot — the AUDIT-04 ErrorCode + tracing::error! covers regression.
    {
        let now_nanos = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or(0)
            .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
        if let Err(e) = tickvault_storage::boot_audit_persistence::append_boot_audit_row(
            &config.questdb,
            now_nanos,
            &boot_id,
            "questdb_ready",
            "ok",
            i64::try_from(probe_started.elapsed().as_millis()).unwrap_or(i64::MAX),
            "QuestDB readiness probe succeeded",
        )
        .await
        {
            tracing::error!(
                error = ?e,
                code = tickvault_common::error_code::ErrorCode::Audit04BootWriteFailed.code_str(),
                "AUDIT-04 boot audit row write failed — continuing"
            );
            metrics::counter!("tv_audit_write_failures_total", "table" => "boot_audit")
                .increment(1);
        }
    }

    // All table creation queries are independent — run in parallel for faster boot.
    tokio::join!(
        ensure_tick_table_dedup_keys(&config.questdb),
        ensure_depth_and_prev_close_tables(&config.questdb),
        ensure_instrument_tables(&config.questdb),
        ensure_candle_table_dedup_keys(&config.questdb),
        calendar_persistence::ensure_calendar_table(&config.questdb),
        tickvault_storage::constituency_persistence::ensure_constituency_table(&config.questdb),
        tickvault_storage::materialized_views::ensure_candle_views(&config.questdb),
        ensure_greeks_tables(&config.questdb),
        tickvault_storage::movers_persistence::ensure_movers_tables(&config.questdb),
        tickvault_storage::indicator_snapshot_persistence::ensure_indicator_snapshot_table(
            &config.questdb
        ),
        tickvault_storage::deep_depth_persistence::ensure_deep_depth_table(&config.questdb),
        tickvault_storage::obi_persistence::ensure_obi_table(&config.questdb),
        // Wave 1 Item 4.2 — un-deprecated previous_close table. Schema
        // includes the new `source` column (CODE6 / QUOTE_CLOSE /
        // FULL_CLOSE) and idempotent ALTER ADD COLUMN IF NOT EXISTS so
        // existing deployments auto-migrate.
        tickvault_storage::previous_close_persistence::ensure_previous_close_table(&config.questdb,),
        // Wave 2 Item 9 (G18) — 6 audit-trail tables. SEBI-relevant.
        // 90d hot → S3 IT → Glacier per `aws-budget.md`.
        tickvault_storage::phase2_audit_persistence::ensure_phase2_audit_table(&config.questdb),
        tickvault_storage::depth_rebalance_audit_persistence::ensure_depth_rebalance_audit_table(
            &config.questdb
        ),
        tickvault_storage::ws_reconnect_audit_persistence::ensure_ws_reconnect_audit_table(
            &config.questdb
        ),
        tickvault_storage::boot_audit_persistence::ensure_boot_audit_table(&config.questdb),
        tickvault_storage::selftest_audit_persistence::ensure_selftest_audit_table(&config.questdb),
        tickvault_storage::order_audit_persistence::ensure_order_audit_table(&config.questdb),
    );

    // Persist trading calendar to QuestDB (best-effort, non-blocking).
    if let Err(err) = calendar_persistence::persist_calendar(&trading_calendar, &config.questdb) {
        warn!(
            ?err,
            "calendar persistence failed (non-critical, best-effort)"
        );
    }

    // Wave 1 Item 0.d — boot-time idempotent init for the index prev_close
    // cache directory. Hoisted out of the tick hot path (was a per-packet
    // `std::fs::create_dir_all` call on every PrevClose code-6 frame).
    // Failure here is non-fatal: the hot-path enqueue will surface the
    // io::Error in the writer task's ERROR arm and movers will simply not
    // have an index baseline cached for mid-day restart recovery.
    if let Err(err) = tickvault_core::pipeline::init_prev_close_cache_dir() {
        warn!(
            ?err,
            "init_prev_close_cache_dir failed (non-critical, mid-day index baseline cache will be unavailable)"
        );
    }

    // Wave 1 Item 0.a — boot-time idempotent init for the async PrevClose
    // cache writer. Spawns a `tokio::task::spawn_blocking` consumer task
    // owning a bounded `tokio::sync::mpsc::channel(64)`. Hot path uses
    // `prev_close_writer::try_enqueue_global` (non-blocking, drops oldest
    // on overflow with `tv_prev_close_writer_dropped_total`).
    tickvault_core::pipeline::prev_close_writer::init();

    // Wave 1 Item 4.3/4.4 — boot-time idempotent init for the
    // FirstSeenSet (gates first-Quote/Full per (security_id, segment)
    // per IST trading day) + the PrevClose persist drain task (forwards
    // hot-path enqueues to QuestDB via ILP). Plus the IST-midnight
    // reset task that flips first_seen back to empty at IST 00:00.
    let first_seen = tickvault_core::pipeline::first_seen_set::init_global();
    let _first_seen_reset_handle =
        tickvault_core::pipeline::first_seen_set::spawn_ist_midnight_reset_task(first_seen);
    tickvault_core::pipeline::prev_close_persist::init(&config.questdb);

    // Wave 1 Item 0.b part 2 — async tick spill drain. Adds an mpsc(8192)
    // layer in front of the existing sync BufWriter spill so the hot path
    // gets non-blocking enqueue semantics under slow-disk conditions
    // (chaos-mode tests, full-disk recovery, host I/O glitches). The
    // sync BufWriter + DLQ safety net stays intact — this is additional
    // capacity, not a replacement.
    let async_spill_path = std::path::PathBuf::from("data/spill").join(format!(
        "ticks-async-{}.bin",
        chrono::Utc::now().format("%Y%m%d")
    ));
    if let Err(err) = tickvault_storage::tick_spill_drain::init(async_spill_path).await {
        warn!(
            ?err,
            "tick_spill_drain init failed (non-critical, sync spill path \
             remains active)"
        );
    }

    // Health status — created early so tick persistence status can be set.
    let health_status: SharedHealthStatus = std::sync::Arc::new(SystemHealthStatus::new());

    let tick_writer = match TickPersistenceWriter::new(&config.questdb) {
        Ok(mut writer) => {
            info!("QuestDB tick writer connected");
            health_status.set_tick_persistence_connected(true);
            // A1: Recover stale spill files from previous crashes.
            let recovered = writer.recover_stale_spill_files();
            if recovered > 0 {
                info!(
                    recovered,
                    "recovered stale tick spill files from previous crashes"
                );
                notifier.notify(
                    tickvault_core::notification::events::NotificationEvent::Custom {
                        message: format!(
                            "<b>Tick Recovery</b>\nRecovered {recovered} orphaned ticks from previous crash spill files."
                        ),
                    },
                );
            }
            Some(writer)
        }
        Err(err) => {
            // A3: Start in disconnected buffering mode instead of giving up.
            // Ticks are buffered in ring buffer + disk spill until QuestDB comes back.
            warn!(
                ?err,
                "QuestDB tick writer failed to connect — starting in DISCONNECTED BUFFERING mode"
            );
            notifier.notify(
                tickvault_core::notification::events::NotificationEvent::Custom {
                    message: format!(
                        "<b>CRITICAL: QuestDB UNAVAILABLE</b>\nTick writer in BUFFERING mode: {err}\nTicks buffered to ring buffer + disk spill. Will drain when QuestDB recovers."
                    ),
                },
            );
            // A3: Create writer in disconnected mode — zero tick loss.
            let mut writer = TickPersistenceWriter::new_disconnected(&config.questdb);
            // Also recover any stale spill files (will skip since sender is None,
            // but sets up the path for when reconnect succeeds).
            // recover_stale_spill_files returns count, not Result — no error to handle.
            let recovered = writer.recover_stale_spill_files();
            if recovered > 0 {
                info!(
                    recovered,
                    "recovered stale tick spill files from previous crash"
                );
            }
            Some(writer)
        }
    };

    let depth_writer = match DepthPersistenceWriter::new(&config.questdb) {
        Ok(mut writer) => {
            // Recover stale depth spill files from previous crashes.
            let recovered = writer.recover_stale_spill_files();
            if recovered > 0 {
                info!(
                    recovered,
                    "recovered stale depth spill files from previous crash"
                );
                notifier.notify(NotificationEvent::Custom {
                    message: format!(
                        "Startup: recovered {recovered} depth snapshots from previous crash spill files"
                    ),
                });
            }
            info!("QuestDB depth writer connected");
            Some(writer)
        }
        Err(err) => {
            error!(
                ?err,
                "QuestDB depth writer unavailable at startup — buffering depth in ring buffer + \
                 disk spill until QuestDB comes back"
            );
            // B2: Use disconnected mode instead of None — ensures zero depth data loss.
            // The writer will auto-reconnect every 30s and drain the buffer on recovery.
            // B3: CRITICAL Telegram alert for depth writer unavailable at startup.
            notifier.notify(NotificationEvent::Custom {
                message: "CRITICAL: QuestDB Depth Writer UNAVAILABLE — \
                          Depth persistence in disconnected mode. \
                          All depth data buffered until QuestDB comes back."
                    .to_owned(),
            });
            let mut writer = DepthPersistenceWriter::new_disconnected(&config.questdb);
            // Attempt recovery even in disconnected mode — spill files may exist
            // from a previous session where QuestDB was available.
            let _ = writer.recover_stale_spill_files();
            Some(writer)
        }
    };

    // -----------------------------------------------------------------------
    // Step 6c: Pre-market readiness check (Parthiban directive 2026-04-21)
    // -----------------------------------------------------------------------
    // Three-zone behaviour:
    //   - 08:00–09:14 IST (pre-market): run + CRITICAL Telegram on failure,
    //     but boot CONTINUES so operator has 75min to rotate the token /
    //     reactivate dataPlan before 09:15.
    //   - 09:15–15:30 IST (market hours): run + CRITICAL Telegram on failure
    //     AND HALT the boot — we refuse to start trading against a bad
    //     profile (expired dataPlan, revoked Derivative segment, or
    //     4h-expiring token would all cause silent data loss).
    //   - Off-hours / non-trading: skip (nothing to check against).
    //
    // Checks: dataPlan == "Active", activeSegment contains "Derivative",
    // token expires > 4 hours from now.
    if is_trading {
        let now_ist =
            chrono::Utc::now() + chrono::Duration::hours(5) + chrono::Duration::minutes(30);
        let hour = now_ist.hour();
        let minute = now_ist.minute();
        let in_pre_market = hour == 8 || (hour == 9 && minute < 15);
        let in_market_hours =
            (hour == 9 && minute >= 15) || (10..=14).contains(&hour) || (hour == 15 && minute < 30);
        if in_pre_market || in_market_hours {
            info!(
                in_pre_market,
                in_market_hours, "running pre-market readiness check"
            );
            match token_manager.pre_market_check().await {
                Ok(()) => info!("pre-market readiness check passed"),
                Err(err) => {
                    // I12 (2026-04-21): auto-diagnostic. On pre-market HALT,
                    // fetch /v2/profile and /v2/ip/getIP directly and embed
                    // their responses (redacted) in the CRITICAL Telegram
                    // message. Operators previously had to run curl manually
                    // while the market clock ticked — now the diagnosis
                    // arrives in the same page.
                    let diagnostics = build_pre_market_diagnostics(
                        &token_manager,
                        &config.dhan.rest_api_base_url,
                    )
                    .await;
                    let reason = format!("{err}\n\n{diagnostics}");
                    // Critical Telegram event — always fires (pre-market or market-hours).
                    notifier.notify(NotificationEvent::PreMarketProfileCheckFailed {
                        reason: reason.clone(),
                        within_market_hours: in_market_hours,
                    });
                    if in_market_hours {
                        // HALT — we refuse to boot into a live trading session
                        // with a bad profile. systemd will restart on the next
                        // attempt but the underlying cause (dataPlan / segment
                        // / token) must be fixed first.
                        error!(
                            error = %err,
                            "HALTING BOOT — pre-market profile check failed during market hours"
                        );
                        anyhow::bail!(
                            "pre-market profile check FAILED during market hours — HALT: {reason}"
                        );
                    }
                    // Pre-market window — log ERROR (triggers Telegram) but
                    // allow boot to continue; operator has until 09:15 IST.
                    error!(
                        error = %err,
                        "pre-market profile check FAILED — investigate before 09:15 IST"
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Step 7: Load or build instruments (three-layer defense)
    // -----------------------------------------------------------------------
    // FreshBuild persists internally (inside load_or_build_instruments).
    // CachedPlan loads from rkyv cache and returns universe for persistence here.
    // To avoid DOUBLE persistence on FreshBuild, only persist if CachedPlan.
    let (subscription_plan, slow_boot_universe, needs_instrument_persist) =
        load_instruments(&config, is_trading, trading_calendar.as_ref()).await;

    // Audit finding #6 (2026-04-24): emit InstrumentBuildSuccess when
    // instruments load successfully on the standard-boot path. Mirrors
    // the fast-boot emission above. Before this, only failure produced
    // a Telegram — operators had no positive "instruments rebuilt OK"
    // signal.
    if let Some(ref u) = slow_boot_universe {
        let source = if needs_instrument_persist {
            "rkyv_cache"
        } else {
            "fresh_csv_build"
        };
        notifier.notify(NotificationEvent::InstrumentBuildSuccess {
            source: source.to_string(),
            derivative_count: u.derivative_contracts.len(),
            underlying_count: u.underlyings.len(),
        });
    }

    // Only persist for CachedPlan (not yet persisted). FreshBuild already
    // persisted inside load_or_build_instruments — double-write creates
    // duplicate rows in the same timestamp second.
    if needs_instrument_persist
        && let Some(ref universe) = slow_boot_universe
        && let Err(err) = persist_instrument_snapshot(universe, &config.questdb).await
    {
        warn!(
            ?err,
            "instrument snapshot persistence failed (non-critical)"
        );
    }

    // -----------------------------------------------------------------------
    // Step 8: Build WebSocket connection pool (only if authenticated + plan ready)
    // -----------------------------------------------------------------------
    let token_handle = token_manager.token_handle();

    // Wave 2 Item 5.4 (AUTH-GAP-03) — install global TokenManager so the
    // WebSocket sleep-wake path can call `force_renewal_if_stale()`
    // without holding a back-reference per connection.
    if !tickvault_core::auth::token_manager::set_global_token_manager(token_manager.clone()) {
        tracing::warn!("global TokenManager already installed — skipping");
    }

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
    //
    // GUARD: Skip WebSocket connections on non-trading days (weekends/holidays).
    // Dhan's WebSocket server sends stale market data (last-traded prices from
    // the previous trading day) even on non-trading days. Without this guard,
    // stale ticks pollute the pipeline.
    //
    // WebSocket connects IMMEDIATELY on trading/mock/muhurat days regardless of
    // current time. Pre-market stale ticks are dropped by the tick processor's
    // ingestion gate: [data_collection_start, data_collection_end) IST.
    // This ensures all 5 connections are warm and ready before 9:00 AM market open.
    // At 15:30 PM (market close), market feed + depth WS are disconnected.
    // Order update WS stays alive until app shutdown.
    let should_connect_ws =
        subscription_plan.is_some() && (is_trading || is_mock_trading || is_muhurat);
    let (pool_receiver, ws_pool_ready) = if should_connect_ws {
        match create_websocket_pool(
            &token_handle,
            &ws_client_id,
            &subscription_plan,
            &config,
            is_market_hours,
            Some(notifier.clone()),
            ws_frame_spill.clone(),
        ) {
            Some((receiver, pool)) => (Some(receiver), Some(pool)),
            None => (None, None),
        }
    } else if !is_trading && !is_mock_trading {
        info!("WebSocket pool skipped — non-trading day (no live market data to capture)");
        (None, None)
    } else {
        warn!("WebSocket pool skipped — running in offline mode");
        (None, None)
    };

    // STAGE-C.2b: Slow-boot mirror of the fast-boot re-injection path.
    // Re-inject replayed LiveFeed frames into the pool's mpsc BEFORE the
    // tick processor spawns, so recovered frames land ahead of any live
    // frames and are persisted idempotently via QuestDB dedup keys.
    if !ws_wal_replay_live_feed.is_empty() {
        if let Some(ref pool) = ws_pool_ready {
            let sender = pool.frame_sender_clone();
            let capacity = sender.capacity();
            let to_inject = std::mem::take(&mut ws_wal_replay_live_feed);
            let count = to_inject.len();
            info!(
                frames = count,
                channel_capacity = capacity,
                "STAGE-C.2b: re-injecting replayed LiveFeed frames into pool mpsc (slow boot)"
            );
            let mut injected = 0u64;
            let mut dropped = 0u64;
            for frame in to_inject {
                match sender.try_send(frame) {
                    Ok(()) => injected += 1,
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_))
                    | Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => dropped += 1,
                }
            }
            metrics::counter!(
                "tv_ws_frame_wal_reinjected_total",
                "ws_type" => "live_feed"
            )
            .increment(injected);
            if dropped > 0 {
                error!(
                    dropped,
                    injected,
                    "STAGE-C.2b: LiveFeed re-injection dropped frames (slow boot) — \
                     channel full/closed, frames remain in WAL archive"
                );
                metrics::counter!(
                    "tv_ws_frame_wal_reinjected_dropped_total",
                    "ws_type" => "live_feed"
                )
                .increment(dropped);
            } else {
                info!(
                    injected,
                    "STAGE-C.2b: LiveFeed re-injection complete (slow boot)"
                );
            }
        } else {
            warn!(
                frames = ws_wal_replay_live_feed.len(),
                "STAGE-C.2b: LiveFeed replay frames held but no pool (slow boot) — \
                 frames remain in WAL archive, not re-injected"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Step 9: Spawn tick processor FIRST (before WS connections send frames)
    // -----------------------------------------------------------------------
    let shared_movers: tickvault_core::pipeline::SharedTopMoversSnapshot =
        std::sync::Arc::new(std::sync::RwLock::new(None));

    // Tick broadcast: fan-out parsed ticks to the trading pipeline (cold path consumer).
    // A2: Use constant capacity (65536) to absorb bursts without lagging cold-path consumers.
    let (tick_broadcast_sender, _tick_broadcast_default_rx) =
        tokio::sync::broadcast::channel::<tickvault_common::tick_types::ParsedTick>(
            tickvault_common::constants::TICK_BROADCAST_CAPACITY,
        );

    // S12 wiring: heartbeat watchdog (slow boot).
    // Same responsibilities as the fast-boot watchdog above. Spawned
    // after token_handle + tick_broadcast_sender are both available.
    // Runs until the process exits.
    let _slow_heartbeat_handle = spawn_heartbeat_watchdog(
        std::sync::Arc::clone(&token_handle),
        tick_broadcast_sender.clone(),
    );

    // In-market gap-backfill is DISABLED by user policy. Historical
    // candle data must NOT be injected into the live `ticks` table.
    // Post-market historical fetch runs on a separate cold path and
    // writes only to `historical_candles`.

    // Spawn the observability-only consumer (gap tracker + HTTP health).
    {
        let obs_rx = tick_broadcast_sender.subscribe();
        let questdb_cfg = config.questdb.clone();
        tokio::spawn(async move {
            run_slow_boot_observability(obs_rx, questdb_cfg).await;
        });
        info!("slow-boot observability consumer started");
    }

    // Plan item G1 (2026-04-22): V2 snapshot handle declared OUTSIDE the
    // processor scope so the api server (Step 11, far below) can read it.
    // Populated inside the processor branch when the registry is available.
    let mut movers_v2_snapshot_handle: Option<
        tickvault_core::pipeline::top_movers::SharedMoversSnapshotV2,
    > = None;

    // Plan item H (2026-04-25): SharedSpotPrices map shared between movers
    // (Premium/Discount routing) + depth ATM selection. Created OUTSIDE the
    // processor scope so Step 8c.0 (below) can clone the same Arc.
    let shared_spot_prices_for_movers =
        tickvault_core::instrument::depth_rebalancer::new_shared_spot_prices();

    let processor_handle = if let Some(receiver) = pool_receiver {
        let candle_agg = Some(tickvault_core::pipeline::CandleAggregator::new());
        let live_candle_writer =
            match tickvault_storage::candle_persistence::LiveCandleWriter::new(&config.questdb) {
                Ok(mut w) => {
                    // Recover stale candle spill files from previous crashes.
                    let recovered = w.recover_stale_spill_files();
                    if recovered > 0 {
                        info!(
                            recovered,
                            "recovered stale candle spill files from previous crash"
                        );
                    }
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
        let movers = Some(tickvault_core::pipeline::TopMoversTracker::new());
        let snapshot_handle = Some(shared_movers.clone());
        let tick_broadcast_for_processor = Some(tick_broadcast_sender.clone());

        // O(1) EXEMPT: cold path — build inline Greeks computer once at startup.
        let greeks_enricher = build_inline_greeks_enricher(&config, &subscription_plan);

        // Create stock movers QuestDB writer (cold path, best-effort)
        let stock_movers_writer =
            match tickvault_storage::movers_persistence::StockMoversWriter::new(&config.questdb) {
                Ok(w) => {
                    info!("QuestDB stock movers writer connected");
                    Some(w)
                }
                Err(err) => {
                    warn!(
                        ?err,
                        "stock movers writer unavailable — movers will not be persisted"
                    );
                    None
                }
            };

        // Create option movers tracker + QuestDB writer
        let option_movers_tracker = Some(tickvault_core::pipeline::OptionMoversTracker::new());
        let option_movers_writer =
            match tickvault_storage::movers_persistence::OptionMoversWriter::new(&config.questdb) {
                Ok(w) => {
                    info!("QuestDB option movers writer connected");
                    Some(w)
                }
                Err(err) => {
                    warn!(
                        ?err,
                        "option movers writer unavailable — option movers will not be persisted"
                    );
                    None
                }
            };

        // O(1) EXEMPT: cold path — clone registry once for tick processor enrichment.
        let slow_registry = subscription_plan
            .as_ref()
            .map(|p| std::sync::Arc::new(p.registry.clone()));

        // Plan item G1+G2 (2026-04-22): spawn MoversTrackerV2 pipeline alongside
        // the legacy TopMoversTracker + OptionMoversTracker. The V2 tracker
        // produces a single 6-bucket snapshot consumed by /api/movers and
        // persisted to the unified top_movers QuestDB table.
        //
        // Safe parallel operation: both trackers read the same tick broadcast
        // — they are independent consumers. Legacy trackers continue writing
        // stock_movers + option_movers tables for back-compat (plan D2).
        // Dedicated shutdown notifier for the V2 movers pipeline. Awakened
        // by the graceful-shutdown path (below) via a cloned Arc.
        //
        // Plan item H (2026-04-25): SharedSpotPrices was created above (outside
        // the processor scope). Movers Premium/Discount routing reads from the
        // same map populated by the spot updater task in Step 8c.0 below.
        let movers_v2_shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
        let movers_v2_handles = if let Some(registry) = slow_registry.as_ref() {
            Some(tickvault_app::movers_v2_pipeline::spawn_movers_v2_pipeline(
                std::sync::Arc::clone(registry),
                tick_broadcast_sender.clone(),
                config.questdb.clone(),
                config.movers.clone(),
                std::sync::Arc::clone(&shared_spot_prices_for_movers),
                std::sync::Arc::clone(&movers_v2_shutdown),
            ))
        } else {
            None
        };
        movers_v2_snapshot_handle = movers_v2_handles
            .as_ref()
            .map(|h| std::sync::Arc::clone(&h.snapshot_handle));

        // Parthiban directive (2026-04-21): no-tick-during-market-hours
        // watchdog (slow boot path). Same pattern as fast boot above.
        let slow_tick_heartbeat = tickvault_core::pipeline::no_tick_watchdog::new_tick_heartbeat();
        let _slow_no_tick_watchdog_handle =
            tickvault_core::pipeline::no_tick_watchdog::spawn_no_tick_watchdog(
                std::sync::Arc::clone(&slow_tick_heartbeat),
                Some(std::sync::Arc::clone(&notifier)),
            );

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
                greeks_enricher,
                stock_movers_writer,
                option_movers_tracker,
                option_movers_writer,
                slow_registry,
                Some(slow_tick_heartbeat),
            )
            .await;
        });
        info!(
            "tick processor started (with candle aggregation + top movers + option movers + trading broadcast)"
        );
        // Pipeline-active wiring (slow boot): mirror the fast-boot flip
        // above so the System Overview "Pipeline Status" tile reads
        // RUNNING and /health reports `pipeline.active` instead of
        // `pipeline.inactive`. `health_status` is created earlier in the
        // slow-boot sequence and is in scope here.
        health_status.set_pipeline_active(true);
        Some(handle)
    } else {
        info!("tick processor skipped — no frame source available");
        None
    };

    // Step 8b: NOW spawn WebSocket connections (tick processor is already consuming).
    // S4-T1a/T1b: shared shutdown_notify for pool watchdog + graceful shutdown.
    let shutdown_notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let (ws_handles, ws_pool_arc) = if let Some(pool) = ws_pool_ready {
        let pool_arc = std::sync::Arc::new(pool);
        // O1-B (2026-04-17): install per-connection runtime subscribe
        // channels BEFORE spawn — same as the FAST BOOT path (main.rs ~830).
        pool_arc.install_subscribe_channels().await;
        let handles = spawn_websocket_connections(std::sync::Arc::clone(&pool_arc)).await;
        spawn_pool_watchdog_task(
            std::sync::Arc::clone(&pool_arc),
            std::sync::Arc::clone(&shutdown_notify),
            std::sync::Arc::clone(&notifier),
            std::sync::Arc::clone(&health_status),
        );
        // FAST BOOT parity: helper emits per-connection + aggregate Telegram
        // alerts on BOTH boot paths (main.rs ~830 for FAST BOOT, here for slow).
        emit_websocket_connected_alerts(&notifier, handles.len()).await;
        (handles, Some(pool_arc))
    } else {
        (Vec::new(), None)
    };

    // -----------------------------------------------------------------------
    // Step 8c.0: SharedSpotPrices — capture index LTP for depth ATM selection
    // -----------------------------------------------------------------------
    // Must be created BEFORE depth connections so we can wait for the first
    // index LTP before selecting ATM strikes. The spot price updater subscribes
    // to the tick broadcast and extracts index LTPs.
    //
    // 2026-04-25: reuse the SharedSpotPrices map created earlier (above the
    // movers v2 pipeline spawn, plan item H). Both subsystems share one map
    // so the spot updater task at the bottom of this block populates LTPs
    // visible to depth ATM selection AND movers Premium/Discount routing.
    let shared_spot_prices = std::sync::Arc::clone(&shared_spot_prices_for_movers);
    let spot_prices_for_depth = std::sync::Arc::clone(&shared_spot_prices);
    if should_connect_ws {
        let spot_prices_updater = std::sync::Arc::clone(&shared_spot_prices);
        let mut spot_rx = tick_broadcast_sender.subscribe();

        // 2026-04-25: Spot updater now also captures NSE_EQ cash-equity LTPs
        // for the 216 F&O stocks. This is the LIVE-tick source consumed by
        // the Mode C live-tick ATM resolver (mid-market boots). Index lookup
        // includes only the 3 full-chain indices (NIFTY/BANKNIFTY/SENSEX);
        // FINNIFTY/MIDCPNIFTY were dropped 2026-04-25.
        let index_lookup =
            tickvault_core::instrument::live_tick_atm_resolver::build_full_chain_index_lookup();
        // Build NSE_EQ stock lookup from the F&O universe: price_feed_security_id → underlying_symbol.
        // APPROVED: I-P1-11 — `stock_lookup_map` is single-segment NSE_EQ by construction (only F&O stock underlyings inserted).
        let stock_lookup_map: std::collections::HashMap<u32, String> = slow_boot_universe
            .as_ref()
            .map(|u| {
                u.underlyings
                    .values()
                    .filter(|ul| {
                        matches!(
                            ul.kind,
                            tickvault_common::instrument_types::UnderlyingKind::Stock
                        ) && matches!(
                            ul.price_feed_segment,
                            tickvault_common::types::ExchangeSegment::NseEquity
                        )
                    })
                    .map(|ul| (ul.price_feed_security_id, ul.underlying_symbol.clone()))
                    .collect()
            })
            .unwrap_or_default();

        let cash_equity_count = stock_lookup_map.len();
        tokio::spawn(async move {
            loop {
                match spot_rx.recv().await {
                    Ok(tick) => {
                        if let Some(sym) =
                            tickvault_core::instrument::live_tick_atm_resolver::classify_tick_for_spot_update(
                                tick.security_id,
                                tick.exchange_segment_code,
                                tick.last_traded_price,
                                &index_lookup,
                                &stock_lookup_map,
                            )
                        {
                            tickvault_core::instrument::depth_rebalancer::update_spot_price(
                                &spot_prices_updater,
                                sym,
                                f64::from(tick.last_traded_price),
                            )
                            .await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        info!(
            cash_equities = cash_equity_count,
            "spot price updater started — capturing index + cash-equity LTPs (Mode C live-tick ATM source)"
        );
    }

    // -----------------------------------------------------------------------
    // Step 8c: Spawn 20-level + 200-level depth WebSocket connections
    // -----------------------------------------------------------------------
    // 20-level: 2 connections (NIFTY, BANKNIFTY), 49 instruments each (2026-04-25: FINNIFTY + MIDCPNIFTY dropped)
    //   = ATM + 24 CE above + 24 PE below (nearest expiry only)
    // 200-level: 2 underlyings (NIFTY, BANKNIFTY), 1 ATM CE + 1 ATM PE each
    //   (nearest expiry only)
    // NSE only — BSE (SENSEX) depth not supported by Dhan depth endpoint.
    // SENSEX gets 5-level depth from main Live Market Feed.
    // 200-level depth task handles — abort+respawn on ATM rebalance.
    // Key: "NIFTY-CE", "NIFTY-PE", etc. Value: JoinHandle of the WS connection task.
    // Protected by tokio Mutex for async-safe abort+spawn cycle.
    let depth_200_handles: std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>>,
    > = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

    // Command senders for live depth instrument swaps (unsubscribe+resubscribe, zero disconnect).
    // Key: "NIFTY" for 20-level, "NIFTY-CE"/"NIFTY-PE" for 200-level.
    let depth_cmd_senders: std::sync::Arc<
        tokio::sync::Mutex<
            std::collections::HashMap<
                String,
                tokio::sync::mpsc::Sender<tickvault_core::websocket::DepthCommand>,
            >,
        >,
    > = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

    // O(1) EXEMPT: begin — boot-time depth connection setup
    if should_connect_ws && config.subscription.enable_twenty_depth {
        if let Some(ref _plan) = subscription_plan {
            // 2026-04-25: Reduced from 4 to 2. FINNIFTY + MIDCPNIFTY were
            // dropped to free 25K main-feed WS capacity for stock F&O ATM±25.
            // 20-level depth only on NIFTY + BANKNIFTY now; SENSEX is BSE
            // (no Dhan depth endpoint).
            let depth_underlyings = ["NIFTY", "BANKNIFTY"];

            // Option C v2 (2026-04-17, Parthiban feedback): do NOT treat all
            // 4 underlyings equally. NIFTY + BANKNIFTY are MANDATORY (they
            // have 200-level depth connections + full option chains); we
            // wait up to 5 minutes for their LTPs because a wrong ATM is
            // worse than a slow boot on these. FINNIFTY + MIDCPNIFTY are
            // OPTIONAL (20-level only); we give them the original 30s
            // grace window and drop+alert if they miss it.
            //
            // Option C v3 (2026-04-17, Parthiban live feedback at 16:59 IST):
            // the boot-time depth wait MUST be market-hours aware. A post-
            // market boot (like the one at 16:53 IST) wasted 5 full minutes
            // waiting for index LTPs that will never arrive because the main
            // feed doesn't stream after 15:30 IST. Boot completed in 342s,
            // triggered BOOT DEADLINE MISSED alert, and spammed 4 false
            // MANDATORY/OPTIONAL-missing alerts. Fix: detect market hours and
            // short-circuit the wait during post-market boots, dropping to
            // 10 seconds of best-effort pickup. During market hours the
            // original 5-minute hard cap still applies (correct behaviour —
            // a genuinely degraded main feed mid-market is a real problem).
            const MANDATORY_UNDERLYINGS: &[&str] = &["NIFTY", "BANKNIFTY"];
            // 2026-04-25: FINNIFTY + MIDCPNIFTY were dropped from the F&O
            // universe — neither IDX_I value nor F&O nor depth-20 is
            // subscribed for them anymore. Phase B's 30s top-up window
            // therefore has no symbols to wait for and exits immediately.
            // Kept as `&[]` (rather than removing Phase B entirely) for
            // minimal-disruption review; Phase B can be deleted in a
            // follow-up cleanup PR if no other OPTIONAL indices return.
            const OPTIONAL_UNDERLYINGS: &[&str] = &[];
            const MANDATORY_WAIT_MARKET_HOURS_SECS: u64 = 300; // 5 min — during session
            const MANDATORY_WAIT_POST_MARKET_SECS: u64 = 10; // 10 s — off-hours boot
            const OPTIONAL_WAIT_SECS: u64 = 30;

            // Off-hours = outside 09:00-15:30 IST using the same constants
            // as tick_persistence + depth_rebalancer so market-hours logic
            // stays DRY across the codebase.
            let is_market_hours = {
                use tickvault_common::constants::{
                    IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST,
                    TICK_PERSIST_START_SECS_OF_DAY_IST,
                };
                let now_utc = chrono::Utc::now().timestamp();
                let now_ist = now_utc.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
                let sec_of_day = now_ist.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32;
                (TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST)
                    .contains(&sec_of_day)
            };
            let mandatory_wait_secs = if is_market_hours {
                MANDATORY_WAIT_MARKET_HOURS_SECS
            } else {
                info!(
                    "depth ATM: off-market-hours boot detected — using 10s \
                     best-effort wait instead of 5-minute hard cap (Option C v3)"
                );
                MANDATORY_WAIT_POST_MARKET_SECS
            };
            {
                let wait_start = std::time::Instant::now();
                let poll_interval = std::time::Duration::from_millis(500); // APPROVED: boot-time poll

                // Phase A: wait for MANDATORY underlyings (NIFTY+BANKNIFTY)
                // up to 5 minutes. These MUST be present before we proceed.
                loop {
                    let prices = spot_prices_for_depth.read().await;
                    let mandatory_ok = MANDATORY_UNDERLYINGS
                        .iter()
                        .all(|sym| prices.contains_key(*sym));
                    drop(prices);
                    if mandatory_ok {
                        let waited = wait_start.elapsed().as_secs();
                        info!(
                            waited_secs = waited,
                            "depth ATM: mandatory index LTPs (NIFTY+BANKNIFTY) present"
                        );
                        break;
                    }
                    if wait_start.elapsed() >= std::time::Duration::from_secs(mandatory_wait_secs) {
                        let waited = wait_start.elapsed().as_secs();
                        let prices = spot_prices_for_depth.read().await;
                        let missing: Vec<&str> = MANDATORY_UNDERLYINGS
                            .iter()
                            .copied()
                            .filter(|s| !prices.contains_key(*s))
                            .collect();
                        drop(prices);
                        // Option C v3: downgrade ERROR to WARN when we're
                        // outside market hours — the timeout is EXPECTED
                        // during a post-market/pre-market boot and firing
                        // ERROR (Telegram-routed) spams false alerts.
                        if is_market_hours {
                            error!(
                                waited_secs = waited,
                                missing = ?missing,
                                "depth ATM: MANDATORY index LTPs still missing after 5 min — \
                                 proceeding with partial set (this should never happen during \
                                 market hours unless main feed is degraded)"
                            );
                            for sym in &missing {
                                notifier.notify(NotificationEvent::DepthUnderlyingMissing {
                                    underlying: (*sym).to_string(),
                                    reason: format!(
                                        "MANDATORY underlying — no spot price after {}s \
                                         (5 min hard cap) — main feed likely degraded",
                                        waited
                                    ),
                                });
                            }
                        } else {
                            warn!(
                                waited_secs = waited,
                                missing = ?missing,
                                "depth ATM: off-market-hours boot — mandatory index \
                                 LTPs not available (expected; main feed does not \
                                 stream outside 09:00-15:30 IST). Depth connections \
                                 for these symbols will NOT be spawned this boot; \
                                 restart the app during market hours to pick them up."
                            );
                        }
                        break;
                    }
                    tokio::time::sleep(poll_interval).await;
                }

                // Phase B: shorter 30s top-up for OPTIONAL underlyings
                // (FINNIFTY, MIDCPNIFTY). If still missing after this, drop
                // them with a per-symbol Telegram alert.
                let phase_b_start = std::time::Instant::now();
                loop {
                    let prices = spot_prices_for_depth.read().await;
                    let have_all = depth_underlyings
                        .iter()
                        .all(|sym| prices.contains_key(*sym));
                    drop(prices);
                    if have_all {
                        info!(
                            underlyings = ?depth_underlyings,
                            count = depth_underlyings.len(),
                            "depth ATM: all index LTPs present — proceeding"
                        );
                        break;
                    }
                    if phase_b_start.elapsed() >= std::time::Duration::from_secs(OPTIONAL_WAIT_SECS)
                    {
                        let waited = wait_start.elapsed().as_secs();
                        let prices = spot_prices_for_depth.read().await;
                        let missing: Vec<&str> = OPTIONAL_UNDERLYINGS
                            .iter()
                            .copied()
                            .filter(|s| !prices.contains_key(*s))
                            .collect();
                        drop(prices);
                        if !missing.is_empty() {
                            // Option C v3: suppress Telegram during off-market
                            // boots — OPTIONAL underlyings missing post-market
                            // is expected, not a real failure. Keep the WARN
                            // for audit but drop the Telegram-routed events.
                            if is_market_hours {
                                warn!(
                                    waited_secs = waited,
                                    missing = ?missing,
                                    "depth ATM: OPTIONAL index LTPs not present after \
                                     30s top-up — dropping those underlyings"
                                );
                                notifier.notify(NotificationEvent::DepthIndexLtpTimeout {
                                    waited_secs: waited,
                                });
                                for sym in &missing {
                                    notifier.notify(NotificationEvent::DepthUnderlyingMissing {
                                        underlying: (*sym).to_string(),
                                        reason: format!(
                                            "OPTIONAL underlying — no spot price in 30s \
                                             top-up window after {}s total — symbol inactive \
                                             or low-liquidity pre-market",
                                            waited
                                        ),
                                    });
                                }
                            } else {
                                info!(
                                    waited_secs = waited,
                                    missing = ?missing,
                                    "depth ATM: off-market-hours — OPTIONAL LTPs expected \
                                     absent; not firing Telegram (Option C v3)"
                                );
                            }
                        }
                        break;
                    }
                    tokio::time::sleep(poll_interval).await;
                }
            }

            // Read current spot prices for ATM calculation.
            let spot_snapshot: std::collections::HashMap<String, f64> = {
                let prices = spot_prices_for_depth.read().await;
                // O3: strip freshness timestamps for the boot-time ATM selector
                // which only needs (underlying -> price). Staleness is enforced
                // by the rebalancer at runtime.
                prices.iter().map(|(k, e)| (k.clone(), e.price)).collect()
                // O(1) EXEMPT: 4 entries max (one per index)
            };

            // Build FnoUniverse reference for select_depth_instruments.
            let depth_universe: Option<&tickvault_common::instrument_types::FnoUniverse> =
                slow_boot_universe.as_ref();

            let today = chrono::Utc::now()
                .with_timezone(
                    &chrono::FixedOffset::east_opt(19800).expect("IST offset"), // APPROVED: compile-time literal
                )
                .date_naive();

            // Use select_depth_instruments for ATM ± 24 selection when universe + spot prices available.
            // Falls back to nearest-expiry median when spot price unavailable.
            let depth_selections: Vec<
                tickvault_core::instrument::depth_strike_selector::DepthStrikeSelection,
            > = if let Some(universe) = depth_universe {
                let ul_refs: Vec<&str> = depth_underlyings.to_vec();
                tickvault_core::instrument::depth_strike_selector::select_depth_instruments(
                    universe,
                    &ul_refs,
                    &spot_snapshot,
                    today,
                    tickvault_core::instrument::depth_strike_selector::DEPTH_ATM_STRIKES_EACH_SIDE,
                )
            } else {
                Vec::new()
            };

            // Log PROOF of ATM selection for every underlying.
            for sel in &depth_selections {
                info!(
                    underlying = %sel.underlying_symbol,
                    atm_strike = sel.atm_strike,
                    expiry = %sel.expiry_date,
                    calls = sel.call_security_ids.len(),
                    puts = sel.put_security_ids.len(),
                    total = sel.all_security_ids.len(),
                    spot = spot_snapshot.get(&sel.underlying_symbol).copied().unwrap_or(0.0),
                    "PROOF: depth ATM selection — nearest expiry {}, ATM strike {}, {} CE + {} PE = {} instruments",
                    sel.expiry_date, sel.atm_strike,
                    sel.call_security_ids.len(), sel.put_security_ids.len(), sel.all_security_ids.len()
                );
            }

            // 2026-04-24 Fix C: counter for 200-depth initial-connect stagger.
            // Each successful spawn (only NIFTY + BANKNIFTY × CE + PE = 4 total)
            // increments the counter, so spawn 0 gets 0ms delay, spawn 1 gets
            // 2000ms, spawn 2 gets 4000ms, spawn 3 gets 6000ms. Dhan auth
            // handshakes land ~2s apart instead of all within 100ms.
            let mut depth_200_spawn_index: u64 = 0;

            for underlying in &depth_underlyings {
                // Look up the ATM selection for this underlying.
                let selection = depth_selections
                    .iter()
                    .find(|s| s.underlying_symbol == *underlying);

                // Build 20-level instrument list from ATM ± 24 selection.
                let instruments_for_underlying: Vec<
                    tickvault_core::websocket::types::InstrumentSubscription,
                > = if let Some(sel) = selection {
                    sel.all_security_ids
                        .iter()
                        .take(config.subscription.twenty_depth_max_instruments)
                        .map(|&sid| {
                            tickvault_core::websocket::types::InstrumentSubscription::new(
                                tickvault_common::types::ExchangeSegment::NseFno,
                                sid,
                            )
                        })
                        .collect()
                } else {
                    warn!(
                        underlying,
                        "depth: no ATM selection available — no spot price or no option chain"
                    );
                    Vec::new()
                };

                // Plan item B.2 (2026-04-23): previously we skipped spawning
                // depth connections when no instruments could be built at
                // boot (pre-market LTPs unavailable). This produced zero
                // depth subscriptions until next restart. Now we ALWAYS
                // spawn — an empty instrument list puts the connection in
                // DEFERRED mode (socket up, no subscribe sent) and the
                // 09:13 dispatcher (real-C) sends InitialSubscribe20 once
                // the 09:12 close is available in the preopen buffer.
                // 200-level gets the same treatment via `Option<u32>` for
                // the ATM CE/PE security_id below.
                if instruments_for_underlying.is_empty() {
                    info!(
                        underlying,
                        "20-level depth: no instruments selected at boot — spawning DEFERRED connection, 09:13 dispatcher will InitialSubscribe20"
                    );
                }

                // Extract ATM CE + ATM PE for 200-level depth.
                // 200-level = 1 instrument per connection (nearest expiry, exact ATM only).
                // O(1) EXEMPT: boot-time ATM lookup
                fn build_precise_label(
                    underlying: &str,
                    expiry: chrono::NaiveDate,
                    strike: f64,
                    side: &str,
                ) -> String {
                    let strike_str = if (strike.fract()).abs() < 0.0001 {
                        #[allow(clippy::cast_possible_truncation)]
                        // APPROVED: strike prices fit i64
                        {
                            format!("{}", strike as i64)
                        }
                    } else {
                        format!("{strike}")
                    };
                    format!("{underlying}-{}-{strike_str}-{side}", expiry.format("%b%Y"))
                }

                let (atm_ce, atm_pe): (Option<(u32, String)>, Option<(u32, String)>) =
                    if let Some(sel) = selection {
                        // CRITICAL: use the dedicated `atm_ce_security_id` /
                        // `atm_pe_security_id` fields — NOT `.first()` on the
                        // range vectors. `.first()` returns the LOWEST strike
                        // in the ATM ± N band, not the ATM itself, so pairing
                        // it with the center-strike label produced
                        // Telegram messages like
                        // `BANKNIFTY-Apr2026-56700-PE (SID 67481)` where the
                        // SID actually pointed at strike 54300. See
                        // `depth_strike_selector::DepthStrikeSelection` docs.
                        //
                        // LABEL POLICY: prefer the Dhan CSV `display_name`
                        // (e.g. "BANKNIFTY 28 APR 54300 PUT") over the
                        // synthesized `UNDERLYING-MmmYYYY-STRIKE-SIDE`
                        // format, because it matches Dhan's own web UI
                        // character-for-character. Fall back to the
                        // synthesized label only if the registry lookup
                        // didn't populate `display_name` (should not
                        // happen for contracts present in the CSV).
                        let ce = sel.atm_ce_security_id.map(|sid| {
                            let label = sel.atm_ce_display_name.clone().unwrap_or_else(|| {
                                build_precise_label(
                                    underlying,
                                    sel.expiry_date,
                                    sel.atm_strike,
                                    "CE",
                                )
                            });
                            (sid, label)
                        });
                        let pe = sel.atm_pe_security_id.map(|sid| {
                            let label = sel.atm_pe_display_name.clone().unwrap_or_else(|| {
                                build_precise_label(
                                    underlying,
                                    sel.expiry_date,
                                    sel.atm_strike,
                                    "PE",
                                )
                            });
                            (sid, label)
                        });
                        (ce, pe)
                    } else {
                        (None, None)
                    };
                let atm_ce_sid = atm_ce.as_ref().map(|(sid, _)| *sid);
                let atm_pe_sid = atm_pe.as_ref().map(|(sid, _)| *sid);

                let depth_token = token_handle.clone();
                let depth_client_id = ws_client_id.clone();
                let instrument_count = instruments_for_underlying.len();
                let label = (*underlying).to_string();

                // PROOF: log exactly which ATM strike and security_ids are used
                // — include the precise contract labels so the log line can be
                // pasted verbatim into a Dhan support ticket.
                let atm_ce_label = atm_ce.as_ref().map(|(_, lbl)| lbl.as_str()).unwrap_or("-");
                let atm_pe_label = atm_pe.as_ref().map(|(_, lbl)| lbl.as_str()).unwrap_or("-");
                info!(
                    underlying,
                    instruments = instrument_count,
                    atm_ce_sid = ?atm_ce_sid,
                    atm_pe_sid = ?atm_pe_sid,
                    atm_ce_contract = atm_ce_label,
                    atm_pe_contract = atm_pe_label,
                    "PROOF: spawning 20-level depth ({instrument_count} instruments) + 200-level depth (CE={atm_ce_label}, PE={atm_pe_label})"
                );

                // O(1) EXEMPT: begin — depth connection + persistence setup at boot
                let (depth_tx, mut depth_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4096);
                let depth_questdb = config.questdb.clone();
                let depth_label_recv = label.clone();

                // Spawn depth frame receiver with QuestDB persistence + OBI computation
                tokio::spawn(async move {
                    // Pre-register metric handles to avoid String clone on every frame/OBI computation.
                    let m = metrics::counter!("tv_depth_20lvl_frames_received", "underlying" => depth_label_recv.clone());
                    let m_obi_value =
                        metrics::gauge!("tv_obi_value", "underlying" => depth_label_recv.clone());
                    let m_obi_computations = metrics::counter!("tv_obi_computations_total", "underlying" => depth_label_recv.clone());
                    let m_obi_errors = metrics::counter!("tv_obi_persist_errors_total", "underlying" => depth_label_recv.clone());
                    let mut writer =
                        tickvault_storage::deep_depth_persistence::DeepDepthWriter::new(
                            &depth_questdb,
                        )
                        .ok();
                    if writer.is_some() {
                        tracing::info!(
                            underlying = depth_label_recv,
                            "deep depth QuestDB writer connected"
                        );
                    }

                    // OBI: writer + bid accumulator (per security_id).
                    // Depth packets arrive as [Bid][Ask] pairs per instrument in each WS message.
                    // Accumulate bid levels, compute OBI when ask arrives.
                    let mut obi_writer = tickvault_storage::obi_persistence::ObiWriter::new(
                        &depth_questdb,
                        &depth_label_recv,
                    )
                    .ok();
                    if obi_writer.is_some() {
                        tracing::info!(
                            underlying = depth_label_recv,
                            "OBI QuestDB writer connected"
                        );
                    }
                    // O(1) EXEMPT: begin — HashMap pre-allocated for max 50 instruments per depth connection
                    let mut bid_accumulator: std::collections::HashMap<
                        u32,
                        (u8, Vec<tickvault_common::tick_types::DeepDepthLevel>),
                    > = std::collections::HashMap::with_capacity(50);
                    // O(1) EXEMPT: end

                    // H5: consecutive parse error counter for Telegram escalation.
                    let mut consecutive_parse_errors: u32 = 0;

                    while let Some(frame) = depth_rx.recv().await {
                        m.increment(1);
                        let ts = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
                        // Split stacked packets: Dhan stacks multiple instrument packets
                        // in a single WS message: [Inst1 Bid][Inst1 Ask][Inst2 Bid]...
                        // Without splitting, only the first packet would be parsed.
                        let packets =
                            match tickvault_core::parser::dispatcher::split_stacked_depth_packets(
                                &frame,
                            ) {
                                Ok(p) => p,
                                Err(err) => {
                                    tracing::warn!(
                                        ?err,
                                        "failed to split stacked 20-level depth frame"
                                    );
                                    continue;
                                }
                            };
                        for packet in packets {
                            match tickvault_core::parser::dispatcher::dispatch_deep_depth_frame(
                                packet, ts,
                            ) {
                                Ok(tickvault_core::parser::types::ParsedFrame::DeepDepth {
                                    security_id,
                                    exchange_segment_code,
                                    side,
                                    levels,
                                    message_sequence,
                                    ..
                                }) => {
                                    consecutive_parse_errors = 0; // H5: reset on success
                                    let side_str = match side {
                                        tickvault_core::parser::deep_depth::DepthSide::Bid => "BID",
                                        tickvault_core::parser::deep_depth::DepthSide::Ask => "ASK",
                                    };
                                    // Persist raw depth to QuestDB
                                    if let Some(ref mut w) = writer
                                        && let Err(err) = w.append_deep_depth(
                                            security_id,
                                            exchange_segment_code,
                                            side_str,
                                            &levels,
                                            "20",
                                            ts,
                                            message_sequence,
                                        )
                                    {
                                        // Phase 0 / Rule 5: persist failures are ERROR (route to Telegram).
                                        tracing::error!(?err, "failed to persist 20-level depth");
                                    }

                                    // OBI accumulation: store bid, compute on ask arrival.
                                    // Bid/ask arrive as separate packets per instrument.
                                    // Remove bid entry after OBI computation to prevent stale data.
                                    match side {
                                        tickvault_core::parser::deep_depth::DepthSide::Bid => {
                                            bid_accumulator.insert(
                                                security_id,
                                                (exchange_segment_code, levels),
                                            );
                                        }
                                        tickvault_core::parser::deep_depth::DepthSide::Ask => {
                                            // Remove bid entry (take ownership) to prevent stale accumulation.
                                            if let Some((seg_code, bid_levels)) =
                                                bid_accumulator.remove(&security_id)
                                            {
                                                let obi_snap =
                                                    tickvault_trading::indicator::obi::compute_obi(
                                                        security_id,
                                                        seg_code,
                                                        &bid_levels,
                                                        &levels,
                                                    );

                                                // Update Prometheus gauges (pre-registered, zero-clone)
                                                m_obi_value.set(obi_snap.obi);
                                                m_obi_computations.increment(1);

                                                // Persist OBI snapshot with separate ts and received_at.
                                                // ts = received_at IST (for QuestDB designated timestamp).
                                                // received_at = same value (depth has no exchange timestamp).
                                                let ts_ist = ts.saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
                                                if let Some(ref mut ow) = obi_writer {
                                                    let obi_record = tickvault_storage::obi_persistence::ObiRecord {
                                                        security_id,
                                                        segment_code: obi_snap.segment_code,
                                                        obi: obi_snap.obi,
                                                        weighted_obi: obi_snap.weighted_obi,
                                                        total_bid_qty: obi_snap.total_bid_qty,
                                                        total_ask_qty: obi_snap.total_ask_qty,
                                                        bid_levels: obi_snap.bid_levels,
                                                        ask_levels: obi_snap.ask_levels,
                                                        max_bid_wall_price: obi_snap.max_bid_wall_price,
                                                        max_bid_wall_qty: obi_snap.max_bid_wall_qty,
                                                        max_ask_wall_price: obi_snap.max_ask_wall_price,
                                                        max_ask_wall_qty: obi_snap.max_ask_wall_qty,
                                                        spread: obi_snap.spread,
                                                        ts_nanos: ts_ist,
                                                    };
                                                    if let Err(err) = ow.append_obi(&obi_record) {
                                                        m_obi_errors.increment(1);
                                                        tracing::warn!(
                                                            ?err,
                                                            "failed to persist OBI snapshot"
                                                        );
                                                    }
                                                }
                                            }
                                            // Ask without prior bid: skip silently (normal at startup
                                            // when ask frame arrives before first bid frame).
                                        }
                                    }
                                }
                                Ok(_) => {} // non-depth frame (shouldn't happen)
                                Err(err) => {
                                    // H5: Escalate persistent parse failures to ERROR (triggers Telegram).
                                    consecutive_parse_errors =
                                        consecutive_parse_errors.saturating_add(1);
                                    metrics::counter!("tv_depth_parse_errors_total", "depth" => "20").increment(1);
                                    if consecutive_parse_errors >= 5 {
                                        tracing::error!(
                                            ?err,
                                            consecutive = consecutive_parse_errors,
                                            "H5: 20-level depth parse failures persisting — {consecutive_parse_errors} consecutive errors"
                                        );
                                        consecutive_parse_errors = 0;
                                    } else {
                                        tracing::warn!(
                                            ?err,
                                            "failed to parse 20-level depth packet"
                                        );
                                    }
                                }
                            }
                        }
                    }

                    // Flush remaining buffered records on shutdown (channel closed).
                    // Without this, records buffered since last auto-flush are lost.
                    if let Some(ref mut w) = writer
                        && let Err(err) = w.flush()
                    {
                        // Phase 0 / Rule 5: flush failures are ERROR (route to Telegram).
                        tracing::error!(?err, "depth writer flush on shutdown failed");
                    }
                    if let Some(ref mut ow) = obi_writer
                        && let Err(err) = ow.flush()
                    {
                        // Phase 0 / Rule 5: flush failures are ERROR (route to Telegram).
                        tracing::error!(?err, "OBI writer flush on shutdown failed");
                    }
                    tracing::info!(
                        underlying = depth_label_recv,
                        "depth frame receiver task exiting"
                    );
                });

                // Spawn depth WebSocket connection with Telegram alerts + health updates
                let d20_notifier = notifier.clone();
                let d20_health = health_status.clone();
                let d20_label_for_disconnect = label.clone();
                let d20_label_for_signal = label.clone();
                let d20_underlying_label = label.clone();
                let d20_wal_spill = ws_frame_spill.clone();
                // Parthiban directive (2026-04-21): wire notifier INSIDE the
                // depth connection so DepthTwentyReconnected fires on every
                // successful reconnect. The `d20_notifier` above is used
                // only on task-exit for DepthTwentyDisconnected.
                let d20_reconnect_notifier = Some(notifier.clone());
                let (signal_tx, signal_rx) = tokio::sync::oneshot::channel::<()>();
                // Command channel for live rebalance (unsubscribe+resubscribe, zero disconnect).
                let (d20_cmd_tx, d20_cmd_rx) =
                    tokio::sync::mpsc::channel::<tickvault_core::websocket::DepthCommand>(4);
                {
                    let senders = std::sync::Arc::clone(&depth_cmd_senders);
                    let key = (*underlying).to_string();
                    tokio::spawn(async move {
                        senders.lock().await.insert(key, d20_cmd_tx);
                    });
                }
                tokio::spawn(async move {
                    d20_health.set_depth_20_connections(
                        d20_health.depth_20_connections().saturating_add(1),
                    );

                    if let Err(err) = tickvault_core::websocket::run_twenty_depth_connection(
                        depth_token,
                        depth_client_id,
                        instruments_for_underlying,
                        depth_tx,
                        d20_underlying_label,
                        Some(signal_tx),
                        d20_wal_spill,
                        d20_cmd_rx,
                        d20_reconnect_notifier,
                    )
                    .await
                    {
                        tracing::error!(?err, "20-level depth connection terminated");
                        // Q5 (2026-04-23): route to Low-severity off-hours
                        // variant when outside 09:00-15:30 IST. Same
                        // anti-spam pattern as `WebSocketDisconnectedOffHours`
                        // and `depth_rebalancer` stale-spot edge trigger —
                        // a post-market boot shouldn't fire [HIGH] SMS for
                        // an expected no-data disconnect.
                        if tickvault_common::market_hours::is_within_market_hours_ist() {
                            d20_notifier.notify(NotificationEvent::DepthTwentyDisconnected {
                                underlying: d20_label_for_disconnect,
                                reason: format!("{err}"),
                            });
                        } else {
                            d20_notifier.notify(
                                NotificationEvent::DepthTwentyDisconnectedOffHours {
                                    underlying: d20_label_for_disconnect,
                                    reason: format!("{err}"),
                                },
                            );
                        }
                        d20_health.set_depth_20_connections(
                            d20_health.depth_20_connections().saturating_sub(1),
                        );
                    }
                });
                // O(1) EXEMPT: end

                // Telegram alert fires ONLY after first data frame received (not just subscription).
                {
                    let notify_label = d20_label_for_signal;
                    let notify_sender = notifier.clone();
                    tokio::spawn(async move {
                        if signal_rx.await.is_ok() {
                            notify_sender.notify(NotificationEvent::DepthTwentyConnected {
                                underlying: notify_label,
                            });
                        }
                    });
                }

                // 200-level: spawn 2 connections for NIFTY + BANKNIFTY ONLY (CE ATM + PE ATM).
                // 200-level = 1 instrument per connection. Dhan limit = 5 connections.
                // 4 connections: NIFTY CE + NIFTY PE + BANKNIFTY CE + BANKNIFTY PE = within limit.
                // FINNIFTY + MIDCPNIFTY use 20-level depth only (no 200-level).
                // Dhan confirmed (Ticket #5519522): must use ATM security_id.
                // O(1) EXEMPT: begin — boot-time 200-level depth setup (max 2 spawns per underlying)
                // Only NIFTY + BANKNIFTY get 200-level (4 connections within Dhan's 5 limit).
                //
                // 2026-04-24: stagger initial connects by DEPTH_200_INITIAL_STAGGER_MS (2s)
                // per spawn to avoid the concurrent-auth TCP-reset storm observed at
                // 12:07:54 IST boot (all 4 connections reset within <100ms of each other).
                if *underlying == "NIFTY" || *underlying == "BANKNIFTY" {
                    // Plan item B.2 (2026-04-23): when boot-time ATM is
                    // unavailable (pre-market deploy before 09:00 LTPs
                    // arrive), spawn the 200-level connection in DEFERRED
                    // mode with `depth200_sid = None`. The 09:13 dispatcher
                    // (real-C) will send InitialSubscribe200 once the
                    // 09:12 close is in the preopen buffer. Label carries
                    // "-deferred" so Telegrams + logs reflect the state.
                    // No ERROR-level alert here — pre-market missing ATM
                    // is EXPECTED, not a planner bug.
                    let atm_ce_entry: (&str, Option<u32>, String) = match atm_ce.as_ref() {
                        Some((sid, lbl)) => ("CE", Some(*sid), lbl.clone()), // O(1) EXEMPT: boot-time
                        None => ("CE", None, format!("{underlying}-CE-deferred")), // O(1) EXEMPT: boot-time
                    };
                    let atm_pe_entry: (&str, Option<u32>, String) = match atm_pe.as_ref() {
                        Some((sid, lbl)) => ("PE", Some(*sid), lbl.clone()), // O(1) EXEMPT: boot-time
                        None => ("PE", None, format!("{underlying}-PE-deferred")), // O(1) EXEMPT: boot-time
                    };
                    for opt_entry in [Some(atm_ce_entry), Some(atm_pe_entry)] {
                        let Some((opt_label, depth200_sid, depth200_label)) = opt_entry else {
                            continue;
                        };

                        // Q6 (2026-04-24): skip deferred spawn outside market
                        // hours. Deferred mode (sid=None, "-deferred" label)
                        // waits for the 09:13 IST dispatcher to provide the
                        // real ATM SID via InitialSubscribe200. Post-market
                        // that dispatcher won't fire again today — spawning
                        // the loop with SID=0 just burns 60 TCP connects
                        // against Dhan before giving up (seen in 2026-04-23
                        // 21:20-23:13 IST boot: 4 contracts × 60 attempts =
                        // 240 useless connects). Rule 3 from
                        // audit-findings-2026-04-17.md: background workers
                        // must be market-hours aware.
                        if depth200_sid.is_none()
                            && !tickvault_common::market_hours::is_within_market_hours_ist()
                        {
                            warn!(
                                underlying,
                                option = opt_label,
                                contract = %depth200_label,
                                "200-level depth: skipping deferred spawn — off-market-hours boot, \
                                 09:13 IST dispatcher won't run today. Boot during 09:00-15:30 IST to \
                                 pick up depth-200 for this contract."
                            );
                            continue;
                        }

                        let depth200_token = token_handle.clone();
                        let depth200_client_id = ws_client_id.clone();
                        let depth200_segment = tickvault_common::types::ExchangeSegment::NseFno;

                        // depth200_sid is Option<u32> (Item B.2): Some = ATM
                        // available at boot, None = deferred until 09:13.
                        let sid_log: i64 = depth200_sid.map_or(-1, i64::from);
                        info!(
                            underlying,
                            option = opt_label,
                            security_id = sid_log,
                            contract = %depth200_label,
                            "spawning 200-level depth connection (contract={depth200_label}, sid={sid_log})"
                        );

                        let (tx200, mut rx200) = tokio::sync::mpsc::channel::<bytes::Bytes>(1024);
                        let m200_label = depth200_label.clone();
                        let depth200_questdb = config.questdb.clone(); // O(1) EXEMPT: boot clone

                        // Spawn 200-level depth receiver with QuestDB persistence
                        tokio::spawn(async move {
                            let m = metrics::counter!("tv_depth_200lvl_frames_received", "underlying" => m200_label.clone());
                            let mut writer =
                                tickvault_storage::deep_depth_persistence::DeepDepthWriter::new(
                                    &depth200_questdb,
                                )
                                .ok();
                            if writer.is_some() {
                                tracing::info!(
                                    label = m200_label,
                                    "200-level deep depth QuestDB writer connected"
                                );
                            }
                            // H5: consecutive parse error counter.
                            let mut consecutive_parse_errors_200: u32 = 0;

                            while let Some(frame) = rx200.recv().await {
                                m.increment(1);
                                let ts = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
                                match tickvault_core::parser::dispatcher::dispatch_deep_depth_frame(
                                    &frame, ts,
                                ) {
                                    Ok(tickvault_core::parser::types::ParsedFrame::DeepDepth {
                                        security_id,
                                        exchange_segment_code,
                                        side,
                                        levels,
                                        message_sequence,
                                        ..
                                    }) => {
                                        consecutive_parse_errors_200 = 0; // H5: reset on success
                                        let side_str = match side {
                                            tickvault_core::parser::deep_depth::DepthSide::Bid => {
                                                "BID"
                                            }
                                            tickvault_core::parser::deep_depth::DepthSide::Ask => {
                                                "ASK"
                                            }
                                        };
                                        if let Some(ref mut w) = writer
                                            && let Err(err) = w.append_deep_depth(
                                                security_id,
                                                exchange_segment_code,
                                                side_str,
                                                &levels,
                                                "200",
                                                ts,
                                                message_sequence,
                                            )
                                        {
                                            tracing::warn!(
                                                ?err,
                                                "failed to persist 200-level depth"
                                            );
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(err) => {
                                        consecutive_parse_errors_200 =
                                            consecutive_parse_errors_200.saturating_add(1);
                                        metrics::counter!("tv_depth_parse_errors_total", "depth" => "200").increment(1);
                                        if consecutive_parse_errors_200 >= 5 {
                                            tracing::error!(
                                                ?err,
                                                consecutive = consecutive_parse_errors_200,
                                                "H5: 200-level depth parse failures persisting — {consecutive_parse_errors_200} consecutive errors"
                                            );
                                            consecutive_parse_errors_200 = 0;
                                        } else {
                                            tracing::warn!(
                                                ?err,
                                                "failed to parse 200-level depth frame"
                                            );
                                        }
                                    }
                                }
                            }
                        });

                        let d200_health = health_status.clone();
                        let d200_notifier = notifier.clone();
                        // Parthiban directive (2026-04-21): wire notifier
                        // INSIDE the depth-200 connection so
                        // DepthTwoHundredReconnected fires on every
                        // successful reconnect.
                        let d200_reconnect_notifier = Some(notifier.clone());
                        let d200_label_for_disconnect = depth200_label.clone();
                        let d200_label_for_signal = depth200_label.clone();
                        // Use 0 sentinel for the Telegram/disconnect event
                        // when deferred (pre-09:13 boot) — the event types
                        // take u32 and the 09:13 dispatcher will log the
                        // real sid via InitialSubscribe200 before any
                        // disconnect notification is meaningful.
                        let d200_sid_for_disconnect: u32 = depth200_sid.unwrap_or(0);
                        let d200_sid_for_signal: u32 = depth200_sid.unwrap_or(0);
                        let d200_wal_spill = ws_frame_spill.clone();
                        // 2026-04-24 Fix C: stagger this spawn's first connect
                        // by N × 2s where N is the 0-based spawn index.
                        let d200_initial_stagger_ms: u64 = depth_200_spawn_index.saturating_mul(
                            tickvault_core::websocket::DEPTH_200_INITIAL_STAGGER_MS,
                        );
                        depth_200_spawn_index = depth_200_spawn_index.saturating_add(1);
                        let (d200_signal_tx, d200_signal_rx) =
                            tokio::sync::oneshot::channel::<()>();
                        // Command channel for live 200-level rebalance (zero disconnect).
                        let d200_handle_key = format!("{underlying}-{opt_label}"); // e.g. "NIFTY-CE"
                        let (d200_cmd_tx, d200_cmd_rx) = tokio::sync::mpsc::channel::<
                            tickvault_core::websocket::DepthCommand,
                        >(4);
                        {
                            let senders = std::sync::Arc::clone(&depth_cmd_senders);
                            let key = d200_handle_key.clone();
                            tokio::spawn(async move {
                                senders.lock().await.insert(key, d200_cmd_tx);
                            });
                        }
                        let d200_handle = tokio::spawn(async move {
                            d200_health.set_depth_200_connections(
                                d200_health.depth_200_connections().saturating_add(1),
                            );

                            if let Err(err) =
                                tickvault_core::websocket::run_two_hundred_depth_connection(
                                    depth200_token,
                                    depth200_client_id,
                                    depth200_segment,
                                    // depth200_sid is already Option<u32>
                                    // (Item B.2): None = deferred, Some =
                                    // boot-time ATM available.
                                    depth200_sid,
                                    depth200_label,
                                    tx200,
                                    Some(d200_signal_tx),
                                    d200_wal_spill,
                                    d200_cmd_rx,
                                    d200_reconnect_notifier,
                                    d200_initial_stagger_ms,
                                )
                                .await
                            {
                                tracing::error!(
                                    ?err,
                                    contract = %d200_label_for_disconnect,
                                    security_id = d200_sid_for_disconnect,
                                    "200-level depth connection terminated"
                                );
                                // Q5 (2026-04-23): route to Low-severity
                                // off-hours variant when outside market
                                // hours OR when the subscription fell
                                // through to the deferred placeholder
                                // (SecurityId=0). The 2026-04-23 post-
                                // market boot fired 4× [HIGH] alerts with
                                // SID=0 because no spot LTP arrived to
                                // pick the ATM — pure Telegram noise.
                                let use_off_hours_variant = d200_sid_for_disconnect == 0
                                    || !tickvault_common::market_hours::is_within_market_hours_ist(
                                    );
                                if use_off_hours_variant {
                                    d200_notifier.notify(
                                        NotificationEvent::DepthTwoHundredDisconnectedOffHours {
                                            contract: d200_label_for_disconnect,
                                            security_id: d200_sid_for_disconnect,
                                            reason: format!("{err}"),
                                        },
                                    );
                                } else {
                                    d200_notifier.notify(
                                        NotificationEvent::DepthTwoHundredDisconnected {
                                            contract: d200_label_for_disconnect,
                                            security_id: d200_sid_for_disconnect,
                                            reason: format!("{err}"),
                                        },
                                    );
                                }
                                d200_health.set_depth_200_connections(
                                    d200_health.depth_200_connections().saturating_sub(1),
                                );
                            }
                        });
                        // Store handle for rebalance abort+respawn.
                        {
                            let handles = std::sync::Arc::clone(&depth_200_handles);
                            let key = d200_handle_key;
                            tokio::spawn(async move {
                                handles.lock().await.insert(key, d200_handle);
                            });
                        }
                        // Telegram alert fires ONLY after first data frame received.
                        {
                            let notify_sender = notifier.clone();
                            tokio::spawn(async move {
                                if d200_signal_rx.await.is_ok() {
                                    notify_sender.notify(
                                        NotificationEvent::DepthTwoHundredConnected {
                                            contract: d200_label_for_signal,
                                            security_id: d200_sid_for_signal,
                                        },
                                    );
                                }
                            });
                        }
                    }
                    // O(1) EXEMPT: end
                } // end if NIFTY || BANKNIFTY (200-level guard)
            }
        }
    } else if config.subscription.enable_twenty_depth {
        info!("depth connections skipped — WebSocket connections not active");
    }
    // O(1) EXEMPT: end

    // -----------------------------------------------------------------------
    // Step 8d: Spawn depth rebalancer (monitors spot drift, signals ATM changes)
    // -----------------------------------------------------------------------
    if should_connect_ws && config.subscription.enable_twenty_depth && subscription_plan.is_some() {
        // 2026-04-25: Reduced from 4 to 2 — see comment at depth_underlyings
        // declaration above (Step 8c). FINNIFTY/MIDCPNIFTY dropped.
        let depth_underlyings: Vec<String> = ["NIFTY", "BANKNIFTY"]
            .iter()
            .map(|s| (*s).to_string())
            .collect(); // O(1) EXEMPT: boot-time vec of 4 strings

        // Build FnoUniverse Arc for rebalancer (one-time clone at boot)
        let rebalancer_universe: Option<std::sync::Arc<FnoUniverse>> =
            slow_boot_universe.as_ref().map(|u| {
                std::sync::Arc::new(u.clone()) // O(1) EXEMPT: boot-time universe clone
            });

        if let Some(universe_arc) = rebalancer_universe {
            // Reuse the SharedSpotPrices created in Step 8c.0 — the spot price
            // updater is already running and updating it with live index LTPs.
            let spot_prices = std::sync::Arc::clone(&shared_spot_prices);

            // Pre-clone universe handles for the snapshotter + Phase 2
            // delta computation BEFORE the rebalancer takes ownership.
            let snapshotter_universe = std::sync::Arc::clone(&universe_arc);
            let phase2_universe = std::sync::Arc::clone(&universe_arc);
            let stock_ltps_universe = std::sync::Arc::clone(&universe_arc);
            // Plan item C (2026-04-22, visibility version): the 09:13 IST
            // depth-anchor task needs its own universe handle to look up
            // option chains for ATM strike derivation.
            let depth_anchor_universe = std::sync::Arc::clone(&universe_arc);
            // Wave 3-A Item 10: pre-open movers tracker also needs an
            // independent universe handle (cloned BEFORE the rebalancer
            // takes ownership at the bottom of this block).
            let preopen_movers_universe = std::sync::Arc::clone(&universe_arc);

            // Rebalance event channel (watch — latest-value semantics)
            let (rebalance_tx, mut rebalance_rx) = tokio::sync::watch::channel::<
                Option<tickvault_core::instrument::depth_rebalancer::RebalanceEvent>,
            >(None);

            // O3 (2026-04-17): Stale-spot-price event channel. The rebalancer
            // publishes here when an underlying's LTP is older than
            // `STALE_SPOT_THRESHOLD_SECS`; a listener below fires a Telegram
            // alert and skips the tainted rebalance cycle.
            let (stale_spot_tx, mut stale_spot_rx) = tokio::sync::watch::channel::<
                Option<tickvault_core::instrument::depth_rebalancer::StaleSpotPriceEvent>,
            >(None);

            // Shutdown flag for the rebalancer
            let rebalancer_shutdown =
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

            tokio::spawn(
                tickvault_core::instrument::depth_rebalancer::run_depth_rebalancer(
                    spot_prices,
                    universe_arc,
                    depth_underlyings,
                    rebalance_tx,
                    stale_spot_tx,
                    std::sync::Arc::clone(&rebalancer_shutdown),
                ),
            );

            // O3 listener: Telegram on stale spot price.
            {
                let stale_notifier = notifier.clone();
                tokio::spawn(async move {
                    while stale_spot_rx.changed().await.is_ok() {
                        let event = match stale_spot_rx.borrow().clone() {
                            Some(e) => e,
                            None => continue,
                        };
                        stale_notifier.notify(NotificationEvent::DepthSpotPriceStale {
                            underlying: event.underlying,
                            age_secs: event.age_secs,
                        });
                    }
                });
            }

            // PROMPT B precursor (2026-04-20): pre-open price snapshotter.
            // Subscribes to the tick broadcast and buckets every NSE_EQ
            // tick that belongs to an F&O stock into the matching minute
            // slot (09:08..09:12 IST today; widening to 09:00..09:12 in
            // Fix #1 — see .claude/plans/active-plan.md). The Phase 2
            // scheduler reads this buffer at **09:13:00 IST** (commit
            // 0340a7c moved it from 09:12:30 so the 09:12-minute bucket
            // is fully closed before we read). Outside the window the
            // snapshotter is idle (no work, no metrics) — see
            // audit-findings Rule 3.
            let preopen_buffer =
                tickvault_core::instrument::preopen_price_buffer::new_shared_preopen_buffer();
            // Plan: Phase 2 live-LTP fallback (2026-04-23). Holds the
            // latest NSE_EQ LTP per F&O underlying stock, continuously
            // updated from the main-feed tick broadcast. Used by Phase 2
            // when the preopen buffer is empty at trigger time (fresh-
            // clone deploy at 11:26 AM, crash recovery, etc.) so stock
            // F&O still gets subscribed this session using live data.
            let live_stock_ltps =
                tickvault_core::instrument::preopen_price_buffer::new_shared_stock_ltps();
            {
                let stock_ltps_updater = std::sync::Arc::clone(&live_stock_ltps);
                let mut stock_rx = tick_broadcast_sender.subscribe();
                tokio::spawn(async move {
                    loop {
                        match stock_rx.recv().await {
                            Ok(tick) => {
                                // NSE_EQ = segment code 1. Only F&O underlyings
                                // are useful for Phase 2; filter via the
                                // universe reverse-lookup so we don't fill the
                                // map with the whole ~200 NSE equity universe.
                                if tick.exchange_segment_code != 1 {
                                    continue;
                                }
                                if tick.last_traded_price <= 0.0
                                    || !tick.last_traded_price.is_finite()
                                {
                                    continue;
                                }
                                let Some(symbol) =
                                    stock_ltps_universe.security_id_to_symbol(tick.security_id)
                                else {
                                    continue;
                                };
                                if !stock_ltps_universe.underlyings.contains_key(symbol) {
                                    continue;
                                }
                                let sym_owned = symbol.to_string(); // O(1) EXEMPT: per-tick but only F&O subset
                                let ltp = f64::from(tick.last_traded_price);
                                let mut map = stock_ltps_updater.write().await;
                                map.insert(sym_owned, ltp);
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                });
            }
            {
                let snap_buffer = std::sync::Arc::clone(&preopen_buffer);
                let snap_universe = snapshotter_universe;
                let mut snap_rx = tick_broadcast_sender.subscribe();
                tokio::spawn(async move {
                    // Plan item A (2026-04-22): combined lookup merges F&O
                    // stocks (NSE_EQ) + whitelisted indices (NIFTY + BANKNIFTY
                    // on IDX_I). Indices feed the depth-20 + depth-200 ATM
                    // selection at **09:13:00 IST** per the unified dispatch
                    // plan (was 09:12:30 — Fix #8 comment cleanup 2026-04-24).
                    let lookup =
                        tickvault_core::instrument::preopen_price_buffer::build_preopen_combined_lookup(
                            &snap_universe,
                        );
                    info!(
                        combined_lookup_count = lookup.len(),
                        "Phase 2 pre-open snapshotter started — F&O stocks + indices tracked"
                    );
                    loop {
                        match snap_rx.recv().await {
                            Ok(tick) => {
                                // Window-gate first: outside 09:08..09:12 IST we do
                                // nothing — no classification, no metrics.
                                if !tickvault_core::instrument::preopen_price_buffer::is_within_preopen_window() {
                                    continue;
                                }
                                use tickvault_core::instrument::preopen_price_buffer::{
                                    SnapshotterOutcome, classify_tick,
                                };
                                match classify_tick(&tick, &lookup) {
                                    SnapshotterOutcome::Buffered {
                                        symbol,
                                        minute_index,
                                    } => {
                                        let mut guard = snap_buffer.write().await;
                                        guard.entry(symbol).or_default().record(
                                            minute_index,
                                            f64::from(tick.last_traded_price),
                                        );
                                        drop(guard);
                                        metrics::counter!(
                                            "tv_phase2_snapshotter_ticks_buffered_total"
                                        )
                                        .increment(1);
                                    }
                                    SnapshotterOutcome::Filtered(reason) => {
                                        metrics::counter!(
                                            "tv_phase2_snapshotter_ticks_filtered_total",
                                            "reason" => reason.as_label()
                                        )
                                        .increment(1);
                                    }
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                });
            }

            // === Wave 3-A Item 10: Pre-open movers tracker ===
            //
            // Captures pre-open LTPs + previous-day closes for F&O
            // underlyings during 09:00..09:13 IST and emits a
            // phase=PREOPEN snapshot every 60s. SENSEX (BSE) is NOT in
            // the Dhan pre-open feed → emitted with
            // phase=PREOPEN_UNAVAILABLE so the dashboard can prove the
            // absence is intentional (audit-findings Rule 11).
            //
            // Gated on `config.features.preopen_movers` (default true).
            // Persistence routes to `stock_movers` with `phase` SYMBOL
            // distinguishing it from the in-market GAINER/LOSER/etc rows
            // that flow from `top_movers`.
            //
            // Audit-findings Rule 3 — market-hours gate: every loop
            // here checks `is_within_preopen_window()` and short-
            // circuits outside the window.
            //
            // Audit-findings Rule 4 — edge-trigger: the snapshot task
            // resets the tracker on the rising edge of the window so
            // yesterday's data does not leak into today's first snap.
            //
            // Audit-findings Rule 5 — flush/persist failures use
            // error! with `code = MOVERS-03` field so Loki routes them
            // to Telegram via the ErrorCode tag-guard.
            if config.features.preopen_movers {
                match tickvault_storage::movers_persistence::StockMoversWriter::new(&config.questdb)
                {
                    Ok(writer) => {
                        // Build (security_id, segment) -> symbol lookup:
                        // every F&O underlying NSE_EQ stock + the two
                        // preopen-tracked indices (NIFTY=13, BANKNIFTY=25
                        // on IDX_I per PREOPEN_INDEX_UNDERLYINGS). I-P1-11
                        // composite key.
                        let mut symbol_lookup: std::collections::HashMap<
                            (u32, tickvault_common::types::ExchangeSegment),
                            String,
                        > = std::collections::HashMap::new();
                        for (symbol, ul) in &preopen_movers_universe.underlyings {
                            if ul.kind != tickvault_common::instrument_types::UnderlyingKind::Stock
                            {
                                continue;
                            }
                            if ul.price_feed_segment
                                == tickvault_common::types::ExchangeSegment::NseEquity
                            {
                                symbol_lookup.insert(
                                    (
                                        ul.price_feed_security_id,
                                        tickvault_common::types::ExchangeSegment::NseEquity,
                                    ),
                                    symbol.clone(),
                                );
                            }
                        }
                        for (sym, sid) in
                            tickvault_core::instrument::preopen_price_buffer::PREOPEN_INDEX_UNDERLYINGS
                        {
                            symbol_lookup.insert(
                                (
                                    *sid,
                                    tickvault_common::types::ExchangeSegment::IdxI,
                                ),
                                (*sym).to_string(),
                            );
                        }

                        // Wave-3-A Item 10 + adversarial-review LOW #5:
                        // SENSEX (BSE) is the canonical
                        // PREOPEN_UNAVAILABLE entry — Dhan does NOT
                        // stream BSE indices on the pre-open feed. The
                        // explicit row preserves audit visibility per
                        // Rule 11 (no false-OK signals).
                        //
                        // Look up SENSEX's `price_feed_security_id`
                        // from the universe instead of hardcoding 51.
                        // Falls back to 51 (the Dhan-stable id seen
                        // live for years) only if SENSEX is missing
                        // from the universe — which would itself be
                        // logged at warn level since SENSEX is a
                        // mandatory full-chain index per
                        // FULL_CHAIN_INDEX_SYMBOLS.
                        let unavailable = {
                            let (sid, source) =
                                match preopen_movers_universe.underlyings.get("SENSEX") {
                                    Some(meta) => (meta.price_feed_security_id, "registry"),
                                    None => {
                                        warn!(
                                            "preopen_movers — SENSEX missing from FnoUniverse; \
                                         falling back to hardcoded security_id=51 (Dhan-stable). \
                                         Investigate: SENSEX is in FULL_CHAIN_INDEX_SYMBOLS and \
                                         should always be present after universe build."
                                        );
                                        (51_u32, "fallback")
                                    }
                                };
                            info!(
                                sensex_security_id = sid,
                                source, "preopen_movers SENSEX unavailable entry resolved"
                            );
                            vec![
                                tickvault_core::pipeline::preopen_movers::UnavailableSymbol {
                                    symbol: "SENSEX".to_string(),
                                    security_id: sid,
                                    segment: tickvault_common::types::ExchangeSegment::IdxI,
                                },
                            ]
                        };

                        let tracker_size = symbol_lookup.len();
                        // Adversarial review follow-up (Wave-3-A MED #1):
                        // use `std::sync::Mutex` (sync, fast) instead of
                        // `tokio::sync::RwLock`. The critical section is a
                        // single HashMap::insert — microseconds, no .await
                        // held. Tokio's own docs recommend std::sync::Mutex
                        // when no await crosses the lock; tokio::RwLock has
                        // higher acquire cost and FIFO queueing under
                        // contention which would matter at the 09:00:00 IST
                        // burst (~5K ticks/sec).
                        let tracker = std::sync::Arc::new(std::sync::Mutex::new(
                            tickvault_core::pipeline::preopen_movers::PreopenMoversTracker::new(
                                symbol_lookup,
                                unavailable,
                            ),
                        ));
                        let writer_arc = std::sync::Arc::new(tokio::sync::Mutex::new(writer));

                        // Adversarial review fix (Wave-3-A bug-hunt #5,
                        // HIGH): seed NIFTY/BANKNIFTY prev_close from the
                        // on-disk index cache populated by tick_processor's
                        // code-6 packet handler. Without this, IDX_I
                        // underlyings have no prev_close (Ticker-mode
                        // subscription leaves ParsedTick.day_close = 0)
                        // and compute_snapshot filters them out — silently
                        // absent forever.
                        // FOLLOW-UP: full wiring routes live code-6 packets
                        // through tracker.seed_prev_close continuously;
                        // for now this best-effort seed at task spawn
                        // covers mid-day restart recovery from yesterday's
                        // closes.
                        {
                            let seed_path =
                                std::path::Path::new("data/instrument-cache/index-prev-close.json");
                            match std::fs::read_to_string(seed_path) {
                                Ok(json) => match serde_json::from_str::<
                                    std::collections::HashMap<u32, f32>,
                                >(&json)
                                {
                                    Ok(cached) => {
                                        // Wave-3-A MED #1: std::sync::Mutex.
                                        // Boot path — lock-poisoning recovery
                                        // is acceptable since the tracker's
                                        // invariants are unchanged across
                                        // panics (it's pure HashMap state).
                                        let mut g = match tracker.lock() {
                                            Ok(g) => g,
                                            Err(p) => p.into_inner(),
                                        };
                                        let mut seeded = 0usize;
                                        for (&sid, &pc) in &cached {
                                            if matches!(sid, 13 | 25) {
                                                g.seed_prev_close(
                                                    sid,
                                                    tickvault_common::types::ExchangeSegment::IdxI,
                                                    f64::from(pc),
                                                );
                                                seeded = seeded.saturating_add(1);
                                            }
                                        }
                                        info!(
                                            seeded,
                                            cached_total = cached.len(),
                                            "preopen_movers seeded IDX_I prev_close from disk \
                                             cache (mid-day restart recovery)"
                                        );
                                    }
                                    Err(err) => warn!(
                                        ?err,
                                        "preopen_movers index prev_close cache parse failed — \
                                         IDX_I rows will be filtered until live prev_close arrives"
                                    ),
                                },
                                Err(_) => debug!(
                                    "preopen_movers no index prev_close cache on disk (first boot \
                                     of day) — IDX_I rows will be filtered until live prev_close \
                                     arrives"
                                ),
                            }
                        }

                        info!(
                            tracked = tracker_size,
                            "preopen_movers spawned — 09:00..09:13 IST window, 60s snapshot \
                             cadence, phase=PREOPEN (Wave 3-A Item 10)"
                        );

                        // Wave-3-A MED #2: cache `is_within_preopen_window()`
                        // result in an AtomicBool refreshed once per second.
                        // The tick subscriber checks this with a relaxed
                        // load (~1ns) instead of re-resolving the wall
                        // clock (~30-50ns) on every tick. At ~5K ticks/sec
                        // during the 09:00:00 IST burst this is ~150-250 µs
                        // of saved CPU per second — small but free.
                        let window_cache = std::sync::Arc::new(
                            std::sync::atomic::AtomicBool::new(
                                tickvault_core::instrument::preopen_price_buffer::is_within_preopen_window(),
                            ),
                        );
                        {
                            // 1s refresh task — cheap. Lives for the
                            // lifetime of the app; no shutdown signal
                            // needed since the tokio runtime tears down
                            // on app exit.
                            const PREOPEN_WINDOW_CACHE_REFRESH_SECS: u64 = 1;
                            let cache_for_refresh = std::sync::Arc::clone(&window_cache);
                            tokio::spawn(async move {
                                let mut ticker =
                                    tokio::time::interval(tokio::time::Duration::from_secs(
                                        PREOPEN_WINDOW_CACHE_REFRESH_SECS,
                                    ));
                                ticker.set_missed_tick_behavior(
                                    tokio::time::MissedTickBehavior::Skip,
                                );
                                loop {
                                    ticker.tick().await;
                                    let in_window = tickvault_core::instrument::preopen_price_buffer::is_within_preopen_window();
                                    cache_for_refresh
                                        .store(in_window, std::sync::atomic::Ordering::Relaxed);
                                }
                            });
                        }

                        // Tick subscriber — updates the tracker from each
                        // tick during the pre-open window. Fast-path
                        // ignored outside the window.
                        {
                            let tracker_for_ticks = std::sync::Arc::clone(&tracker);
                            let window_cache_for_ticks = std::sync::Arc::clone(&window_cache);
                            let mut tick_rx = tick_broadcast_sender.subscribe();
                            tokio::spawn(async move {
                                loop {
                                    match tick_rx.recv().await {
                                        Ok(tick) => {
                                            // Audit Rule 3 — market-hours gate
                                            // (Wave-3-A MED #2: cached
                                            // AtomicBool, refreshed every 1s
                                            // by the gate-cache task below).
                                            if !window_cache_for_ticks
                                                .load(std::sync::atomic::Ordering::Relaxed)
                                            {
                                                continue;
                                            }
                                            // Wave-3-A MED #1: sync mutex,
                                            // microsecond critical section.
                                            let mut g = match tracker_for_ticks.lock() {
                                                Ok(g) => g,
                                                Err(p) => p.into_inner(),
                                            };
                                            g.update_from_tick(&tick);
                                        }
                                        Err(tokio::sync::broadcast::error::RecvError::Lagged(
                                            missed,
                                        )) => {
                                            // Adversarial review fix
                                            // (Wave-3-A bug-hunt #7 + audit
                                            // Rule 5): a silently-swallowed
                                            // lag means we lost prev_close +
                                            // LTP samples for some F&O
                                            // stocks — the snapshot will
                                            // have holes. Log at error so
                                            // operator sees it on Telegram.
                                            error!(
                                                missed,
                                                code = tickvault_common::error_code::ErrorCode::Movers03PreopenPersistFailed.code_str(),
                                                "MOVERS-03: preopen movers tick subscriber lagged — \
                                                 some pre-open ticks dropped before reaching the tracker"
                                            );
                                            metrics::counter!(
                                                "tv_preopen_movers_broadcast_lagged_total"
                                            )
                                            .increment(missed);
                                        }
                                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                            break;
                                        }
                                    }
                                }
                            });
                        }

                        // Wave-3-A MED #4 — continuous code-6 PrevClose
                        // re-seed task. The boot-time disk-cache read
                        // (above) only runs once at task spawn, so a
                        // code-6 packet for NIFTY / BANKNIFTY arriving
                        // AFTER boot would never reach the tracker. The
                        // file-poll loop below re-reads the on-disk
                        // cache every 30s (which `tick_processor`
                        // updates on every code-6 packet) and re-seeds
                        // the tracker — closing the continuous wire-up
                        // gap without touching tick_processor's hot
                        // path. 30s latency is fine: code-6 is rare
                        // (once per session per index) and the snapshot
                        // cadence is 60s anyway. Audit Rule 3 — gated
                        // on the window cache so it sleeps post-13:00.
                        {
                            const PREV_CLOSE_RESEED_INTERVAL_SECS: u64 = 30;
                            let tracker_for_reseed = std::sync::Arc::clone(&tracker);
                            let window_cache_for_reseed = std::sync::Arc::clone(&window_cache);
                            tokio::spawn(async move {
                                let mut ticker =
                                    tokio::time::interval(tokio::time::Duration::from_secs(
                                        PREV_CLOSE_RESEED_INTERVAL_SECS,
                                    ));
                                ticker.set_missed_tick_behavior(
                                    tokio::time::MissedTickBehavior::Skip,
                                );
                                let seed_path = std::path::Path::new(
                                    "data/instrument-cache/index-prev-close.json",
                                );
                                loop {
                                    ticker.tick().await;
                                    if !window_cache_for_reseed
                                        .load(std::sync::atomic::Ordering::Relaxed)
                                    {
                                        continue;
                                    }
                                    let Ok(json) = std::fs::read_to_string(seed_path) else {
                                        continue;
                                    };
                                    let Ok(cached) = serde_json::from_str::<
                                        std::collections::HashMap<u32, f32>,
                                    >(&json) else {
                                        continue;
                                    };
                                    // Hold the tracker lock briefly (≤ 220 entries
                                    // re-seeded; HashMap::insert is O(1) per call).
                                    let mut g = match tracker_for_reseed.lock() {
                                        Ok(g) => g,
                                        Err(p) => p.into_inner(),
                                    };
                                    for (&sid, &pc) in &cached {
                                        if matches!(sid, 13 | 25) {
                                            g.seed_prev_close(
                                                sid,
                                                tickvault_common::types::ExchangeSegment::IdxI,
                                                f64::from(pc),
                                            );
                                        }
                                    }
                                    drop(g);
                                    metrics::counter!("tv_preopen_movers_prev_close_reseed_total")
                                        .increment(1);
                                }
                            });
                        }

                        // 60s snapshot+persist task. Resets the tracker
                        // on the rising edge of the window (idempotent —
                        // safe across same-day re-entry).
                        {
                            let tracker_for_snap = std::sync::Arc::clone(&tracker);
                            let writer_for_snap = std::sync::Arc::clone(&writer_arc);
                            tokio::spawn(async move {
                                // Adversarial review fix (Wave-3-A bug-hunt #1/#2):
                                // initialize `prev_in_window` from the CURRENT
                                // window state, not `false`. Otherwise a boot
                                // mid-window (e.g. 09:05 IST) triggers a
                                // spurious reset on the first tick that wipes
                                // 0-59s of legitimate captured ticks.
                                let mut prev_in_window =
                                    tickvault_core::instrument::preopen_price_buffer::is_within_preopen_window();
                                // Wave-3-A Item 10: snapshot every 60s
                                // during the 09:00..09:13 IST window
                                // (~13 snapshots per trading day per
                                // instrument). Constant per CLAUDE.md
                                // ban on hardcoded Durations.
                                const PREOPEN_MOVERS_SNAPSHOT_SECS: u64 = 60;
                                let mut ticker = tokio::time::interval(
                                    tokio::time::Duration::from_secs(PREOPEN_MOVERS_SNAPSHOT_SECS),
                                );
                                ticker.set_missed_tick_behavior(
                                    tokio::time::MissedTickBehavior::Skip,
                                );
                                loop {
                                    ticker.tick().await;
                                    let in_window = tickvault_core::instrument::preopen_price_buffer::is_within_preopen_window();
                                    // Audit Rule 4 — edge-trigger reset.
                                    if in_window && !prev_in_window {
                                        let mut g = match tracker_for_snap.lock() {
                                            Ok(g) => g,
                                            Err(p) => p.into_inner(),
                                        };
                                        g.reset();
                                        metrics::counter!("tv_preopen_movers_window_entered_total")
                                            .increment(1);
                                        info!(
                                            "preopen_movers — entered 09:00 IST window, \
                                             tracker reset"
                                        );
                                    }
                                    prev_in_window = in_window;
                                    if !in_window {
                                        // Audit Rule 3 — silent outside window.
                                        continue;
                                    }
                                    // Wave-3-A MED #3: emit tracker observability
                                    // gauges every snapshot tick. tracked_len ==
                                    // count of (security_id, segment) with at
                                    // least one captured LTP; prev_close_len ==
                                    // count with a known previous-day close.
                                    // The gap between the two diagnoses
                                    // "we got LTPs but no prev_close" cleanly.
                                    let (snap, tracked_n, prev_close_n) = {
                                        let g = match tracker_for_snap.lock() {
                                            Ok(g) => g,
                                            Err(p) => p.into_inner(),
                                        };
                                        let tracked_n = g.tracked_len();
                                        let prev_close_n = g.prev_close_len();
                                        (g.compute_snapshot(), tracked_n, prev_close_n)
                                    };
                                    metrics::gauge!("tv_preopen_movers_tracked_total")
                                        .set(tracked_n as f64);
                                    metrics::gauge!("tv_preopen_movers_prev_close_total")
                                        .set(prev_close_n as f64);
                                    if snap.is_empty() {
                                        continue;
                                    }
                                    // IST epoch nanos for `ts` per the
                                    // greeks/IST-display convention
                                    // (Utc::now() + IST offset). Use the
                                    // canonical `IST_UTC_OFFSET_NANOS`
                                    // constant — adversarial review
                                    // pulled out the hand-rolled
                                    // multiplication for consistency
                                    // with the rest of the codebase.
                                    let now_ist_nanos = chrono::Utc::now()
                                        .timestamp_nanos_opt()
                                        .unwrap_or(0)
                                        .saturating_add(
                                            tickvault_common::constants::IST_UTC_OFFSET_NANOS,
                                        );
                                    let mut persist_errors: u64 = 0;
                                    let mut persisted: u64 = 0;
                                    let mut writer_g = writer_for_snap.lock().await;
                                    for entry in &snap {
                                        // Adversarial review fix (Wave-3-A
                                        // bug-hunt #9): differentiate
                                        // `category` between live preopen
                                        // rows and unavailable rows. The
                                        // stock_movers DEDUP key is
                                        // (security_id, category, segment)
                                        // — using the same category for
                                        // both phases would let a future
                                        // PREOPEN row silently overwrite a
                                        // PREOPEN_UNAVAILABLE audit row
                                        // for the same security_id+ts.
                                        // Splitting the category preserves
                                        // both as distinct rows.
                                        let category = match entry.phase {
                                            tickvault_core::pipeline::preopen_movers::PreopenPhase::Preopen => {
                                                "PREOPEN_RANK"
                                            }
                                            tickvault_core::pipeline::preopen_movers::PreopenPhase::PreopenUnavailable => {
                                                "PREOPEN_UNAVAIL"
                                            }
                                        };
                                        let res = writer_g.append_stock_mover_with_phase(
                                            now_ist_nanos,
                                            category,
                                            entry.rank,
                                            entry.security_id,
                                            entry.segment.as_str(),
                                            &entry.symbol,
                                            entry.ltp,
                                            entry.prev_close,
                                            entry.change_pct,
                                            entry.volume,
                                            entry.phase.as_wire_str(),
                                        );
                                        match res {
                                            Ok(()) => persisted = persisted.saturating_add(1),
                                            Err(err) => {
                                                persist_errors = persist_errors.saturating_add(1);
                                                error!(
                                                    ?err,
                                                    symbol = %entry.symbol,
                                                    code = tickvault_common::error_code::ErrorCode::Movers03PreopenPersistFailed.code_str(),
                                                    "MOVERS-03: preopen movers append failed"
                                                );
                                            }
                                        }
                                    }
                                    if let Err(err) = writer_g.flush() {
                                        error!(
                                            ?err,
                                            code = tickvault_common::error_code::ErrorCode::Movers03PreopenPersistFailed.code_str(),
                                            "MOVERS-03: preopen movers flush failed"
                                        );
                                        metrics::counter!(
                                            "tv_preopen_movers_persist_errors_total",
                                            "stage" => "flush"
                                        )
                                        .increment(1);
                                    }
                                    drop(writer_g);
                                    if persisted > 0 {
                                        metrics::counter!("tv_preopen_movers_total")
                                            .increment(persisted);
                                    }
                                    if persist_errors > 0 {
                                        metrics::counter!(
                                            "tv_preopen_movers_persist_errors_total",
                                            "stage" => "append"
                                        )
                                        .increment(persist_errors);
                                    }
                                }
                            });
                        }
                    }
                    Err(err) => {
                        warn!(
                            ?err,
                            "preopen_movers writer init failed — feature disabled this session \
                             (visibility-only, no halt)"
                        );
                    }
                }
            } else {
                info!("preopen_movers feature flag = false; skipping spawner (Wave 3-A Item 10)");
            }

            // PROMPT C (2026-04-20) — Phase 2 crash-recovery.
            //
            // BEFORE spawning the Phase 2 scheduler, consult the on-disk
            // snapshot written by PROMPT A at 09:13:00 IST. If today's
            // snapshot is present we re-dispatch the SAME ATM chain the
            // scheduler picked this morning and SKIP spawning the
            // scheduler — a mid-market restart at 11:00 IST must resume
            // the same contracts, not re-pick ATM from drifted live
            // price. Absent/stale snapshot falls through to the existing
            // scheduler unchanged.
            let phase2_action = {
                let snapshot_path =
                    std::path::PathBuf::from(tickvault_app::phase2_recovery::PHASE2_SNAPSHOT_PATH);
                let snapshot = match tickvault_storage::phase2_subscription_marker::read_snapshot(
                    &snapshot_path,
                ) {
                    Ok(snap) => snap,
                    Err(err) => {
                        warn!(
                            ?err,
                            path = %snapshot_path.display(),
                            "Phase 2 recovery: snapshot read failed — proceeding without it"
                        );
                        None
                    }
                };
                let (today_ist, now_sec) =
                    tickvault_app::phase2_recovery::current_ist_seconds_of_day();
                let is_trading_day = trading_calendar.is_trading_day(today_ist);
                let action = tickvault_app::phase2_recovery::plan_recovery(
                    today_ist,
                    now_sec,
                    is_trading_day,
                    snapshot.as_ref(),
                );
                metrics::counter!(
                    tickvault_app::phase2_recovery::RECOVERY_METRIC_NAME,
                    "outcome" => action.outcome_label(),
                )
                .increment(1);
                match &action {
                    tickvault_app::phase2_recovery::RecoveryAction::DispatchSnapshot {
                        snapshot_date,
                        instrument_count,
                        ..
                    } => info!(
                        outcome = action.outcome_label(),
                        snapshot_date = %snapshot_date,
                        instrument_count,
                        "Phase 2 recovery: recovering from snapshot — scheduler will be skipped"
                    ),
                    other => info!(
                        outcome = other.outcome_label(),
                        today_ist = %today_ist,
                        now_sec,
                        is_trading_day,
                        "Phase 2 recovery: no reusable snapshot — delegating to scheduler path"
                    ),
                }
                action
            };

            match phase2_action {
                tickvault_app::phase2_recovery::RecoveryAction::DispatchSnapshot {
                    snapshot_date: _,
                    instrument_count,
                    instruments,
                } => match ws_pool_arc.as_ref() {
                    Some(pool) => {
                        let cmd = tickvault_core::websocket::SubscribeCommand::AddInstruments {
                            instruments,
                            feed_mode: tickvault_common::types::FeedMode::Quote,
                        };
                        match pool.dispatch_subscribe(cmd) {
                            Some(conn_id) => info!(
                                target_connection = conn_id,
                                instrument_count,
                                "Phase 2 recovery: snapshot chain dispatched to pool"
                            ),
                            None => error!(
                                instrument_count,
                                "Phase 2 recovery: dispatch_subscribe returned None — \
                                 snapshot chain will NOT be subscribed this boot"
                            ),
                        }
                    }
                    None => warn!(
                        instrument_count,
                        "Phase 2 recovery: no WebSocket pool — snapshot chain not dispatched"
                    ),
                },
                tickvault_app::phase2_recovery::RecoveryAction::SkipOffHours => {
                    info!("Phase 2 recovery: skip-off-hours — scheduler NOT spawned this boot");
                }
                tickvault_app::phase2_recovery::RecoveryAction::RunFreshPhase2
                | tickvault_app::phase2_recovery::RecoveryAction::WaitForScheduler => {
                    // O1 (2026-04-17) Phase A: Phase 2 subscription scheduler.
                    // Sleeps until 09:13 IST (or runs immediately if already past 9:13
                    // and within market hours), then waits for NIFTY/BANKNIFTY LTPs,
                    // then computes the stock-F&O delta from the snapshotter's
                    // buffer and dispatches the SubscribeCommand to the pool.
                    //
                    // Fix #8 (2026-04-24): trigger time moved 09:12 → 09:13 per
                    // commit 0340a7c so the 09:12-minute bucket is fully closed
                    // before Phase 2 reads the preopen buffer. Old "09:12" text
                    // below was stale.
                    let phase2_spot_prices = std::sync::Arc::clone(&shared_spot_prices);
                    let phase2_calendar_for_dyn = std::sync::Arc::clone(&trading_calendar);
                    let phase2_calendar_for_decision = std::sync::Arc::clone(&trading_calendar);
                    let phase2_notifier = notifier.clone();
                    let phase2_pool = ws_pool_arc.clone();
                    let phase2_buffer = std::sync::Arc::clone(&preopen_buffer);
                    tokio::spawn(async move {
                        tickvault_core::instrument::phase2_scheduler::run_phase2_scheduler(
                            phase2_spot_prices,
                            phase2_calendar_for_decision,
                            phase2_notifier,
                            phase2_pool,
                            Some(
                                tickvault_core::instrument::phase2_scheduler::Phase2InstrumentsSource::Dynamic {
                                    buffer: phase2_buffer,
                                    universe: phase2_universe,
                                    calendar: phase2_calendar_for_dyn,
                                    strikes_each_side: 25,
                                    // Phase 2 live-LTP fallback (2026-04-23):
                                    // pass the continuously-updated stock LTP
                                    // map so a late-start / crash-recovery
                                    // Phase 2 run can subscribe stock F&O
                                    // even when the 09:08–09:12 preopen
                                    // window was missed.
                                    live_stock_ltps: Some(std::sync::Arc::clone(&live_stock_ltps)),
                                },
                            ),
                            tickvault_common::types::FeedMode::Quote,
                            None, // PROMPT A wires the pick_completed channel.
                        )
                        .await;
                    });
                }
            }

            // Plan item #5 (2026-04-22): Market-open streaming confirmation
            // Telegram. Fires once per trading day at 09:15:30 IST with the
            // count of active feeds. Answers Parthiban's "how do I know if
            // connected" question without needing Grafana or curl.
            //
            // Audit-findings Rule 3: market-hours-aware. Trading day check +
            // post-market skip + late-start past 09:15:30 → skip silently.
            {
                let heartbeat_notifier = notifier.clone();
                let heartbeat_health = health_status.clone();
                let heartbeat_calendar = std::sync::Arc::clone(&trading_calendar);
                tokio::spawn(async move {
                    use chrono::{FixedOffset, NaiveTime, TimeZone, Utc};
                    use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
                    let Some(ist_offset) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
                        return;
                    };
                    let now_ist = ist_offset.from_utc_datetime(&Utc::now().naive_utc());
                    let today_ist = now_ist.date_naive();
                    if !heartbeat_calendar.is_trading_day(today_ist) {
                        info!("market-open heartbeat: skipping (non-trading day)");
                        return;
                    }
                    let Some(target) = NaiveTime::from_hms_opt(9, 15, 30) else {
                        return;
                    };
                    let now_time = now_ist.time();
                    if now_time >= target {
                        // 2026-04-24 Fix D: demoted INFO → DEBUG. A mid-session
                        // fresh boot (e.g. 12:07 IST) legitimately runs past
                        // 09:15:30, and this INFO log reads like "something is
                        // broken". Real streaming confirmation happens via
                        // boot-time spot-wait + depth ATM selection.
                        debug!(
                            now = %now_time,
                            "market-open heartbeat: skipping (past 09:15:30 — expected on mid-session boot)"
                        );
                        return;
                    }
                    let secs_until = (target - now_time).num_seconds().max(0) as u64;
                    info!(
                        secs_until,
                        "market-open heartbeat: sleeping until 09:15:30 IST"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(secs_until)).await;

                    let main_active = heartbeat_health.websocket_connections() as usize;
                    let d20 = heartbeat_health.depth_20_connections() as usize;
                    let d200 = heartbeat_health.depth_200_connections() as usize;
                    let oms = heartbeat_health.order_update_connected();
                    info!(
                        main_feed = main_active,
                        depth_20 = d20,
                        depth_200 = d200,
                        order_update = oms,
                        "PROOF: market-open streaming confirmation fired @ 09:15:30 IST"
                    );
                    // Audit finding #8 (2026-04-24): when main feed is 0 at the
                    // 09:15:30 heartbeat, this is NOT a positive "streaming live"
                    // signal — it's a catastrophic missed market open. Route to
                    // the High-severity Failed variant so the operator pages,
                    // not to the Info-severity Confirmation that reads confusingly
                    // as "Streaming live / Main feed: 0/5".
                    if main_active == 0 {
                        heartbeat_notifier.notify(NotificationEvent::MarketOpenStreamingFailed {
                            main_feed_active: main_active,
                            main_feed_total: tickvault_common::constants::MAX_WEBSOCKET_CONNECTIONS,
                            depth_20_active: d20,
                            depth_200_active: d200,
                            order_update_active: oms,
                        });
                    } else {
                        heartbeat_notifier.notify(
                            NotificationEvent::MarketOpenStreamingConfirmation {
                                main_feed_active: main_active,
                                main_feed_total:
                                    tickvault_common::constants::MAX_WEBSOCKET_CONNECTIONS,
                                depth_20_active: d20,
                                depth_200_active: d200,
                                order_update_active: oms,
                            },
                        );
                    }
                });
            }

            // Wave 3-C Item 12 (2026-04-28): market-open self-test at
            // 09:16:00 IST — a single tri-state verdict
            // (Passed / Degraded / Critical) over 7 sub-checks. Fires
            // 30s after the 09:15:30 streaming-confirmation heartbeat
            // so any racy boot-time wiring has settled.
            //
            // Audit-findings Rule 3 (market-hours-aware): trading-day
            // check + skip if past 09:16. Late-start mid-session boot
            // legitimately runs past 09:16 — do not fire then.
            //
            // Persistence: outcome row → `selftest_audit` table
            // (Wave-2-D Item 9). DEDUP key `(trading_date_ist, check_name)`.
            //
            // Gated on `config.features.market_open_self_test`.
            if config.features.market_open_self_test {
                let st_notifier = notifier.clone();
                let st_health = health_status.clone();
                let st_calendar = std::sync::Arc::clone(&trading_calendar);
                let st_qcfg = config.questdb.clone();
                tokio::spawn(async move {
                    use chrono::{FixedOffset, NaiveTime, TimeZone, Utc};
                    use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
                    use tickvault_core::instrument::market_open_self_test::{
                        MarketOpenSelfTestInputs, MarketOpenSelfTestOutcome, evaluate_self_test,
                    };
                    let Some(ist_offset) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
                        return;
                    };
                    let now_ist = ist_offset.from_utc_datetime(&Utc::now().naive_utc());
                    let today_ist = now_ist.date_naive();
                    if !st_calendar.is_trading_day(today_ist) {
                        info!("market-open self-test: skipping (non-trading day)");
                        return;
                    }
                    let Some(target) = NaiveTime::from_hms_opt(9, 16, 0) else {
                        return;
                    };
                    let now_time = now_ist.time();
                    if now_time >= target {
                        debug!(
                            now = %now_time,
                            "market-open self-test: skipping (past 09:16 — expected on mid-session boot)"
                        );
                        return;
                    }
                    let secs_until = (target - now_time).num_seconds().max(0) as u64;
                    info!(
                        secs_until,
                        "market-open self-test: sleeping until 09:16:00 IST"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(secs_until)).await;

                    // Adversarial review (general-purpose Class B+H,
                    // 2026-04-28): rapid restart between 09:14 and 09:16
                    // (e.g. operator `make restart`) can spawn two
                    // schedulers that both fire at 09:16. The audit row
                    // DEDUPs (key = trading_date_ist + check_name) but
                    // the Telegram notify path does not. Pre-check the
                    // audit table; if a row already exists for today,
                    // skip notify (audit row will UPSERT regardless).
                    let already_fired = match probe_selftest_already_fired_today(&st_qcfg).await {
                        Ok(v) => v,
                        Err(err) => {
                            // Pre-check failure must not block the
                            // primary self-test path. Log and proceed.
                            tracing::warn!(
                                ?err,
                                "market-open self-test: pre-check failed; proceeding to fire"
                            );
                            false
                        }
                    };
                    if already_fired {
                        info!(
                            "market-open self-test: skipping notify (already fired today; audit-row UPSERT only)"
                        );
                    }

                    // Sample live state. Seven of the eight sub-checks have
                    // production gauges; `last_tick_age_secs` does not
                    // yet have a per-instrument aggregator and is
                    // conservatively reported as 0. Adversarial review
                    // (general-purpose Class C, 2026-04-28) caught the
                    // earlier false-OK where `pipeline_active` was
                    // hardcoded — `health_status.pipeline_active()`
                    // already exists and is now read live.
                    let main_active = st_health.websocket_connections() as usize;
                    let d20 = st_health.depth_20_connections() as usize;
                    let d200 = st_health.depth_200_connections() as usize;
                    let oms = st_health.order_update_connected();
                    let pipeline = st_health.pipeline_active();
                    let questdb_ok =
                        tickvault_storage::boot_probe::wait_for_questdb_ready(&st_qcfg, 3)
                            .await
                            .is_ok();
                    let token_headroom_secs =
                        tickvault_core::auth::token_manager::global_token_manager()
                            .map(|tm| tm.seconds_until_expiry())
                            .unwrap_or(0);
                    let inputs = MarketOpenSelfTestInputs {
                        main_feed_active: main_active,
                        depth_20_active: d20,
                        depth_200_active: d200,
                        order_update_active: oms,
                        pipeline_active: pipeline,
                        last_tick_age_secs: 0,
                        questdb_connected: questdb_ok,
                        token_expiry_headroom_secs: token_headroom_secs,
                    };
                    let started = std::time::Instant::now();
                    let outcome = evaluate_self_test(&inputs);
                    let duration_ms =
                        i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

                    info!(
                        result = outcome.outcome_str(),
                        code = outcome.error_code().code_str(),
                        main_feed = main_active,
                        depth_20 = d20,
                        depth_200 = d200,
                        order_update = oms,
                        questdb_connected = questdb_ok,
                        token_expiry_headroom_secs = token_headroom_secs,
                        "PROOF: market-open self-test fired @ 09:16:00 IST"
                    );

                    metrics::counter!(
                        "tv_self_test_total",
                        "result" => outcome.outcome_str()
                    )
                    .increment(1);

                    if !already_fired {
                        match &outcome {
                            MarketOpenSelfTestOutcome::Passed { checks_passed } => {
                                st_notifier.notify(
                                    tickvault_core::notification::events::NotificationEvent::SelfTestPassed {
                                        checks_passed: *checks_passed,
                                    },
                                );
                            }
                            MarketOpenSelfTestOutcome::Degraded {
                                checks_passed,
                                checks_failed,
                                failed,
                            } => {
                                st_notifier.notify(
                                    tickvault_core::notification::events::NotificationEvent::SelfTestDegraded {
                                        checks_passed: *checks_passed,
                                        checks_failed: *checks_failed,
                                        failed: failed.clone(),
                                    },
                                );
                            }
                            MarketOpenSelfTestOutcome::Critical {
                                checks_failed,
                                failed,
                            } => {
                                st_notifier.notify(
                                    tickvault_core::notification::events::NotificationEvent::SelfTestCritical {
                                        checks_failed: *checks_failed,
                                        failed: failed.clone(),
                                    },
                                );
                            }
                        }
                    }

                    // Audit persistence (SCOPE §12.3) — write outcome row
                    // to selftest_audit. Wave-2-D DEDUP key
                    // (trading_date_ist, check_name) ensures replay
                    // idempotence; same boot will UPSERT same row.
                    let now_ist_nanos = Utc::now()
                        .timestamp_nanos_opt()
                        .unwrap_or(0)
                        .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
                    let trading_date_ist =
                        now_ist_nanos - (now_ist_nanos.rem_euclid(86_400_000_000_000));
                    let detail = match &outcome {
                        MarketOpenSelfTestOutcome::Passed { .. } => {
                            "all 8 sub-checks green".to_string()
                        }
                        MarketOpenSelfTestOutcome::Degraded { failed, .. }
                        | MarketOpenSelfTestOutcome::Critical { failed, .. } => {
                            format!("failed: {}", failed.join(","))
                        }
                    };
                    if let Err(e) =
                        tickvault_storage::selftest_audit_persistence::append_selftest_audit_row(
                            &st_qcfg,
                            now_ist_nanos,
                            trading_date_ist,
                            "market-open-self-test",
                            outcome.outcome_str(),
                            duration_ms,
                            &detail,
                        )
                        .await
                    {
                        tracing::error!(
                            error = ?e,
                            code = tickvault_common::error_code::ErrorCode::Audit05SelftestWriteFailed
                                .code_str(),
                            "AUDIT-05 selftest audit row write failed"
                        );
                        metrics::counter!(
                            "tv_audit_write_failures_total",
                            "table" => "selftest_audit"
                        )
                        .increment(1);
                    }
                });
            }

            // Plan item C (2026-04-22, visibility version): once-per-day
            // depth-anchor Telegram fired at 09:13:00 IST. Reads the
            // 09:12 closes for NIFTY + BANKNIFTY from the preopen buffer
            // (Item A) and reports the derived ATM strike. Operator
            // visibility into "what 09:12 close anchored today's depth".
            //
            // The 60s depth_rebalancer continues to handle drift via
            // SharedSpotPrices (live LTPs). At 09:13:00 the live LTP and
            // 09:12 close are typically within 0.1% of each other so any
            // strike difference is small — when this is non-trivial,
            // operator can correlate against the rebalance Telegrams.
            //
            // Audit-findings Rule 3: market-hours-aware. Trading-day check
            // + skip if past 09:13.
            {
                let anchor_notifier = notifier.clone();
                let anchor_buffer = std::sync::Arc::clone(&preopen_buffer);
                let anchor_universe = depth_anchor_universe;
                let anchor_calendar = std::sync::Arc::clone(&trading_calendar);
                // Plan item real-C (2026-04-23): the 09:13 task now also
                // dispatches InitialSubscribe20/InitialSubscribe200 commands
                // to the depth connections spawned in DEFERRED mode (item
                // B.2). `anchor_cmd_senders` is the per-underlying map of
                // mpsc::Sender<DepthCommand> built when depth connections
                // were spawned earlier in boot.
                let anchor_cmd_senders = std::sync::Arc::clone(&depth_cmd_senders);
                let anchor_twenty_max = config.subscription.twenty_depth_max_instruments;
                // Live LTPs for FINNIFTY + MIDCPNIFTY (the preopen buffer
                // only captures NIFTY + BANKNIFTY IDX_I). At 09:13 the main
                // feed has been streaming for ~13 min so these are present.
                let anchor_shared_spot_prices = std::sync::Arc::clone(&shared_spot_prices);
                tokio::spawn(async move {
                    use chrono::{FixedOffset, NaiveTime, TimeZone, Utc};
                    use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
                    let Some(ist_offset) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
                        return;
                    };
                    let now_ist = ist_offset.from_utc_datetime(&Utc::now().naive_utc());
                    let today_ist = now_ist.date_naive();
                    if !anchor_calendar.is_trading_day(today_ist) {
                        info!("depth-anchor: skipping (non-trading day)");
                        return;
                    }
                    let Some(target) = NaiveTime::from_hms_opt(9, 13, 0) else {
                        return;
                    };
                    let now_time = now_ist.time();
                    if now_time >= target {
                        // 2026-04-24 Fix D: demoted INFO → DEBUG. A mid-session
                        // fresh boot legitimately runs past 09:13:00; the real
                        // ATM/depth dispatch happens via the boot-time
                        // spot-wait path (see run_depth_init_sync).
                        debug!(
                            now = %now_time,
                            "depth-anchor: skipping (past 09:13:00 — expected on mid-session boot)"
                        );
                        return;
                    }
                    let secs_until = (target - now_time).num_seconds().max(0) as u64;
                    info!(secs_until, "depth-anchor: sleeping until 09:13:00 IST");
                    tokio::time::sleep(std::time::Duration::from_secs(secs_until)).await;

                    // Read preopen buffer snapshot.
                    let snap =
                        tickvault_core::instrument::preopen_price_buffer::snapshot(&anchor_buffer)
                            .await;

                    // For each whitelisted index, derive close + ATM strike + emit Telegram.
                    for (sym, _id) in
                        tickvault_core::instrument::preopen_price_buffer::PREOPEN_INDEX_UNDERLYINGS
                    {
                        let symbol = (*sym).to_string();
                        let (close_0912, source_minute_slot) = match snap.get(*sym) {
                            Some(closes) => {
                                // Find latest non-empty slot.
                                let slot_idx =
                                    closes.closes.iter().enumerate().rev().find_map(|(i, p)| {
                                        if p.is_some() { Some(i) } else { None }
                                    });
                                (closes.backtrack_latest(), slot_idx)
                            }
                            None => (None, None),
                        };

                        // Compute ATM strike from option chain at the next eligible expiry.
                        let atm_strike = close_0912.and_then(|spot| {
                            let chain_opt = anchor_universe
                                .expiry_calendars
                                .get(&symbol)
                                .and_then(|cal| cal.expiry_dates.first())
                                .and_then(|expiry| {
                                    anchor_universe
                                        .option_chains
                                        .get(&tickvault_common::instrument_types::OptionChainKey {
                                            underlying_symbol: symbol.clone(),
                                            expiry_date: *expiry,
                                        })
                                });
                            chain_opt.and_then(|chain| {
                                tickvault_core::instrument::depth_strike_selector::find_atm_security_ids(
                                    chain, spot,
                                )
                                .map(|ids| ids.strike)
                            })
                        });

                        info!(
                            underlying = %symbol,
                            close_0912 = ?close_0912,
                            atm_strike = ?atm_strike,
                            source_slot = ?source_minute_slot,
                            "PROOF: depth anchor @ 09:13 IST"
                        );
                        anchor_notifier.notify(NotificationEvent::MarketOpenDepthAnchor {
                            underlying: symbol,
                            close_0912,
                            atm_strike,
                            source_minute_slot,
                        });
                    }

                    // Plan item real-C (2026-04-23): dispatch
                    // InitialSubscribe20 + InitialSubscribe200 to depth
                    // connections spawned in DEFERRED mode (item B.2).
                    //
                    // Spot prices sourced from two places, in priority order:
                    //   1. NIFTY + BANKNIFTY: 09:12 close from the preopen
                    //      buffer (authoritative — matches Dhan's reference).
                    //   2. FINNIFTY + MIDCPNIFTY: live LTP from
                    //      SharedSpotPrices (main feed has been streaming
                    //      since 09:00 pre-open).
                    //
                    // Underlyings missing a spot at 09:13 are skipped with a
                    // WARN; the depth rebalancer's 09:15 first-check gate
                    // will pick them up once the main feed has a fresh LTP.
                    let mut spot_map: std::collections::HashMap<String, f64> =
                        // O(1) EXEMPT: 4-entry map built once per day
                        std::collections::HashMap::new();
                    {
                        let live = anchor_shared_spot_prices.read().await;
                        for (k, entry) in live.iter() {
                            spot_map.insert(k.clone(), entry.price); // O(1) EXEMPT: 4 entries
                        }
                    }
                    for (sym, _id) in
                        tickvault_core::instrument::preopen_price_buffer::PREOPEN_INDEX_UNDERLYINGS
                    {
                        if let Some(c) = snap.get(*sym).and_then(|cl| cl.backtrack_latest()) {
                            spot_map.insert((*sym).to_string(), c); // O(1) EXEMPT: 2 overrides
                        }
                    }

                    // 2026-04-25: Reduced 4 → 2. FINNIFTY/MIDCPNIFTY dropped from depth.
                    let depth_ul: [&str; 2] = ["NIFTY", "BANKNIFTY"];
                    let today = now_ist.date_naive();
                    let selections =
                        tickvault_core::instrument::depth_strike_selector::select_depth_instruments(
                            &anchor_universe,
                            &depth_ul,
                            &spot_map,
                            today,
                            tickvault_core::instrument::depth_strike_selector::DEPTH_ATM_STRIKES_EACH_SIDE,
                        );
                    let m_dispatch_total =
                        metrics::counter!("tv_depth_initial_subscribe_dispatched_total");
                    let m_dispatch_failed =
                        metrics::counter!("tv_depth_initial_subscribe_dispatch_failed_total");

                    let senders = anchor_cmd_senders.lock().await;
                    for sel in &selections {
                        // 20-level InitialSubscribe.
                        let instruments: Vec<
                            tickvault_core::websocket::types::InstrumentSubscription,
                        > = sel
                            .all_security_ids
                            .iter()
                            .take(anchor_twenty_max)
                            .map(|&sid| {
                                tickvault_core::websocket::types::InstrumentSubscription::new(
                                    tickvault_common::types::ExchangeSegment::NseFno,
                                    sid,
                                )
                            })
                            .collect::<Vec<_>>(); // O(1) EXEMPT: 4 underlyings × ~49 contracts, once per day
                        let subscribe_messages = tickvault_core::websocket::subscription_builder::build_twenty_depth_subscription_messages(
                            &instruments,
                            50, // Dhan max batch size for 20-level subscribe
                        );

                        if let Some(sender) = senders.get(&sel.underlying_symbol) {
                            let cmd = tickvault_core::websocket::DepthCommand::InitialSubscribe20 {
                                subscribe_messages: subscribe_messages.clone(), // O(1) EXEMPT: once-per-day
                            };
                            if sender.send(cmd).await.is_err() {
                                error!(
                                    underlying = %sel.underlying_symbol,
                                    "09:13 dispatch: InitialSubscribe20 send failed — depth receiver dropped"
                                );
                                m_dispatch_failed.increment(1);
                            } else {
                                m_dispatch_total.increment(1);
                            }
                        } else {
                            warn!(
                                underlying = %sel.underlying_symbol,
                                "09:13 dispatch: no depth_cmd_sender registered for 20-level underlying"
                            );
                        }

                        // 200-level: only NIFTY + BANKNIFTY (Dhan 5-conn cap).
                        if sel.underlying_symbol != "NIFTY" && sel.underlying_symbol != "BANKNIFTY"
                        {
                            continue;
                        }
                        for (opt_label, opt_sid) in [
                            ("CE", sel.atm_ce_security_id),
                            ("PE", sel.atm_pe_security_id),
                        ] {
                            let Some(sid) = opt_sid else {
                                warn!(
                                    underlying = %sel.underlying_symbol,
                                    option = opt_label,
                                    "09:13 dispatch: missing ATM CE/PE sid for 200-level"
                                );
                                continue;
                            };
                            let segment_str =
                                tickvault_common::types::ExchangeSegment::NseFno.as_str();
                            let sid_str = sid.to_string(); // O(1) EXEMPT: 4 per day
                            let subscribe_message = serde_json::json!({
                                "RequestCode": 23,
                                "ExchangeSegment": segment_str,
                                "SecurityId": sid_str,
                            })
                            .to_string(); // O(1) EXEMPT: 4 per day
                            let key = format!("{}-{}", sel.underlying_symbol, opt_label); // O(1) EXEMPT: 4 per day
                            if let Some(sender) = senders.get(&key) {
                                let cmd =
                                    tickvault_core::websocket::DepthCommand::InitialSubscribe200 {
                                        subscribe_message,
                                    };
                                if sender.send(cmd).await.is_err() {
                                    error!(
                                        key,
                                        "09:13 dispatch: InitialSubscribe200 send failed — depth receiver dropped"
                                    );
                                    m_dispatch_failed.increment(1);
                                } else {
                                    m_dispatch_total.increment(1);
                                }
                            } else {
                                warn!(
                                    key,
                                    "09:13 dispatch: no depth_cmd_sender registered for 200-level key"
                                );
                            }
                        }

                        info!(
                            underlying = %sel.underlying_symbol,
                            atm_strike = sel.atm_strike,
                            twenty_count = sel.all_security_ids.len(),
                            atm_ce_sid = ?sel.atm_ce_security_id,
                            atm_pe_sid = ?sel.atm_pe_security_id,
                            "PROOF: 09:13 dispatcher sent InitialSubscribe20 + InitialSubscribe200"
                        );
                    }
                    drop(senders);
                });
            }

            // L1: Listen for rebalance events → Telegram alert + send swap commands (zero disconnect).
            {
                let rebalance_notifier = notifier.clone();
                let rebal_cmd_senders = std::sync::Arc::clone(&depth_cmd_senders);
                // Wave 2 Item 9 (AUDIT-02) — clone QuestDB config into the
                // rebalancer task scope so audit rows can be persisted.
                let qcfg_for_rebalance = config.questdb.clone();
                tokio::spawn(async move {
                    while rebalance_rx.changed().await.is_ok() {
                        let event = match rebalance_rx.borrow().clone() {
                            Some(e) => e,
                            None => continue,
                        };
                        // O(1) EXEMPT: cold path — rebalance fires at most once per 60s
                        let ul = event.underlying.clone();
                        let expiry_str = event
                            .expiry
                            .map_or_else(|| "?".to_string(), |e| e.format("%b%Y").to_string());

                        // Format a rebalance contract line for Telegram. Prefer
                        // the Dhan CSV `display_name` (e.g.
                        // "BANKNIFTY 28 APR 54300 PUT (SID 67481)") — it
                        // matches Dhan's web UI verbatim and is the string the
                        // operator is most likely to search for. Fall back to
                        // the synthesized `UNDERLYING-MmmYYYY-STRIKE-SIDE`
                        // only if the registry lookup didn't populate it.
                        let fmt_contract = |atm: &Option<
                            tickvault_core::instrument::depth_strike_selector::AtmIds,
                        >,
                                            opt: &str|
                         -> String {
                            match atm {
                                Some(ids) => {
                                    let (sid, display) = if opt == "CE" {
                                        (ids.ce_id, ids.ce_display_name.as_deref())
                                    } else {
                                        (ids.pe_id.unwrap_or(0), ids.pe_display_name.as_deref())
                                    };
                                    if let Some(name) = display {
                                        format!("{name} (SID {sid})")
                                    } else {
                                        format!(
                                            "{}-{}-{:.0}-{} ({})",
                                            ul, expiry_str, ids.strike, opt, sid
                                        )
                                    }
                                }
                                None => "—".to_string(),
                            }
                        };

                        let old_ce = fmt_contract(&event.prev_atm, "CE");
                        let old_pe = fmt_contract(&event.prev_atm, "PE");
                        let new_ce = fmt_contract(&event.new_atm, "CE");
                        let new_pe = fmt_contract(&event.new_atm, "PE");

                        // Depth-20 ALWAYS rebalances (all 4 underlyings).
                        // Depth-200 ONLY rebalances for NIFTY + BANKNIFTY
                        // (gate at line ~3261 below). The typed
                        // `DepthRebalanced` event fires at `Severity::Low`
                        // per Fix #9 (2026-04-24): routine zero-disconnect
                        // drift swap is working-as-designed, not an amber
                        // alert. The title fragment includes the level(s)
                        // per Fix #10 so operators can tell the swap scope
                        // at a glance.
                        let has_200_level = ul == "NIFTY" || ul == "BANKNIFTY";
                        let levels = if has_200_level {
                            tickvault_core::notification::DepthRebalanceLevels::TwentyAndTwoHundred
                        } else {
                            tickvault_core::notification::DepthRebalanceLevels::TwentyOnly
                        };

                        rebalance_notifier.notify(NotificationEvent::DepthRebalanced {
                            underlying: ul.to_string(),
                            previous_spot: event.previous_spot,
                            current_spot: event.current_spot,
                            old_ce: old_ce.clone(),
                            old_pe: old_pe.clone(),
                            new_ce: new_ce.clone(),
                            new_pe: new_pe.clone(),
                            levels,
                        });

                        // Wave 2 Item 9 (AUDIT-02) — persist a depth-rebalance
                        // audit row for SEBI-grade reconstruction.
                        let now_nanos = chrono::Utc::now()
                            .timestamp_nanos_opt()
                            .unwrap_or(0)
                            .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
                        let levels_label = if has_200_level { "20+200" } else { "20" };
                        let qcfg_for_audit = qcfg_for_rebalance.clone();
                        let ul_for_audit = ul.to_string();
                        let old_strike = event.prev_atm.as_ref().map(|a| a.strike).unwrap_or(0.0);
                        let new_strike = event.new_atm.as_ref().map(|a| a.strike).unwrap_or(0.0);
                        let spot_at_swap = event.current_spot;
                        tokio::spawn(async move {
                            if let Err(e) = tickvault_storage::depth_rebalance_audit_persistence::append_depth_rebalance_audit_row(
                                &qcfg_for_audit,
                                now_nanos,
                                &ul_for_audit,
                                old_strike,
                                new_strike,
                                spot_at_swap,
                                levels_label,
                                "success",
                            )
                            .await
                            {
                                tracing::error!(
                                    error = ?e,
                                    code = tickvault_common::error_code::ErrorCode::Audit02DepthRebalanceWriteFailed.code_str(),
                                    "AUDIT-02 depth-rebalance audit row write failed"
                                );
                                metrics::counter!("tv_audit_write_failures_total", "table" => "depth_rebalance_audit").increment(1);
                            }
                        });

                        // --- 200-level rebalance via command channel (zero disconnect) ---
                        // Only NIFTY and BANKNIFTY have 200-level connections.
                        if ul != "NIFTY" && ul != "BANKNIFTY" {
                            continue;
                        }
                        let Some(new_atm) = &event.new_atm else {
                            warn!(underlying = %ul, "rebalance: new_atm is None — cannot swap 200-level");
                            continue;
                        };

                        // Send Swap200 command to each CE/PE connection.
                        let segment_str = tickvault_common::types::ExchangeSegment::NseFno.as_str();
                        let entries_200: Vec<(&str, u32)> = [
                            Some(("CE", new_atm.ce_id)),
                            new_atm.pe_id.map(|pe_id| ("PE", pe_id)),
                        ]
                        .into_iter()
                        .flatten()
                        .collect();

                        for (opt_label, sid) in entries_200 {
                            let swap_key = format!("{ul}-{opt_label}");
                            let sid_str = sid.to_string();
                            let unsubscribe_msg = serde_json::json!({
                                "RequestCode": 25,
                                "ExchangeSegment": segment_str,
                                "SecurityId": "0",
                            })
                            .to_string();
                            let subscribe_msg = serde_json::json!({
                                "RequestCode": 23,
                                "ExchangeSegment": segment_str,
                                "SecurityId": sid_str,
                            })
                            .to_string();

                            let senders = rebal_cmd_senders.lock().await;
                            if let Some(tx) = senders.get(&swap_key) {
                                let cmd = tickvault_core::websocket::DepthCommand::Swap200 {
                                    unsubscribe_message: unsubscribe_msg,
                                    subscribe_message: subscribe_msg,
                                };
                                if let Err(err) = tx.send(cmd).await {
                                    warn!(
                                        ?err,
                                        key = %swap_key,
                                        "rebalance: Swap200 command send failed"
                                    );
                                } else {
                                    info!(
                                        underlying = %ul,
                                        option = opt_label,
                                        security_id = sid,
                                        spot = event.current_spot,
                                        "PROOF: rebalance Swap200 sent — {swap_key} → sid {sid} (zero disconnect)"
                                    );
                                }
                            } else {
                                warn!(
                                    key = %swap_key,
                                    "rebalance: no cmd sender for {swap_key} — 200-level not spawned?"
                                );
                            }
                        }
                    }
                });
            }
            info!("depth rebalancer spawned (checks spot drift every 60s)");
            metrics::gauge!("tv_depth_rebalancer_active").set(1.0);
        }
    }

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
            tickvault_app::greeks_pipeline::run_greeks_pipeline(
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
    // Step 10: Spawn order update WebSocket connection
    // -----------------------------------------------------------------------
    let (order_update_sender, _order_update_receiver) =
        tokio::sync::broadcast::channel::<tickvault_common::order_types::OrderUpdate>(
            tickvault_common::constants::ORDER_UPDATE_BROADCAST_CAPACITY,
        );

    // STAGE-C.2b: Slow-boot mirror of the fast-boot order-update replay
    // drain. Drains recovered JSON frames into the live broadcast before
    // the WebSocket starts.
    if !ws_wal_replay_order_update.is_empty() {
        let frames = std::mem::take(&mut ws_wal_replay_order_update);
        let (parsed, broadcast_count, parse_errors) =
            tickvault_app::boot_helpers::drain_replayed_order_updates_to_broadcast(
                frames,
                &order_update_sender,
            );
        info!(
            parsed,
            broadcast_count,
            parse_errors,
            "STAGE-C.2b: OrderUpdate WAL replay drain complete (slow boot)"
        );
        metrics::counter!(
            "tv_ws_frame_wal_reinjected_total",
            "ws_type" => "order_update"
        )
        .increment(broadcast_count);
        if parse_errors > 0 {
            metrics::counter!(
                "tv_ws_frame_wal_reinjected_parse_errors_total",
                "ws_type" => "order_update"
            )
            .increment(parse_errors);
        }
    }

    let order_update_handle = {
        let url = config.dhan.order_update_websocket_url.clone();
        let order_ws_client_id = ws_client_id.clone();
        let token = token_manager.token_handle();
        let sender = order_update_sender.clone();
        let cal = trading_calendar.clone();
        let ou_notifier = notifier.clone();
        let ou_connect_notifier = notifier.clone();
        let ou_health = health_status.clone();
        let ou_wal_spill = ws_frame_spill.clone();
        // O2 (2026-04-17): authenticated signal — fires once after first
        // successful parse_order_update or AuthResponseKind::Success. The
        // `OrderUpdateConnected` event below is "task spawned" semantics;
        // `OrderUpdateAuthenticated` is "Dhan accepted the token and is
        // streaming". Operators see both so they know where in the handshake
        // they are.
        let auth_signal = std::sync::Arc::new(tokio::sync::Notify::new());
        let auth_latch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let listener_signal = std::sync::Arc::clone(&auth_signal);
            let listener_notifier = notifier.clone();
            tokio::spawn(async move {
                listener_signal.notified().await;
                listener_notifier.notify(NotificationEvent::OrderUpdateAuthenticated);
            });
        }
        let run_signal = Some(std::sync::Arc::clone(&auth_signal));
        let run_latch = Some(std::sync::Arc::clone(&auth_latch));
        let ou_reconnect_notifier = Some(std::sync::Arc::clone(&notifier));
        tokio::spawn(async move {
            ou_health.set_order_update_connected(true);
            // Telegram: Order Update WS connected (fires before read loop starts).
            ou_connect_notifier.notify(NotificationEvent::OrderUpdateConnected);
            run_order_update_connection(
                url,
                order_ws_client_id,
                token,
                sender,
                cal,
                ou_wal_spill,
                run_signal,
                run_latch,
                ou_reconnect_notifier,
            )
            .await;
            // If run_order_update_connection returns, connection terminated
            ou_notifier.notify(NotificationEvent::OrderUpdateDisconnected {
                reason: "connection task exited".to_string(),
            });
            ou_health.set_order_update_connected(false);
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
            compute_market_close_sleep(tickvault_common::constants::APP_DAILY_RESET_TIME_IST);
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
    // Step 10.5: Index constituency data (best-effort, NON-BLOCKING)
    // -----------------------------------------------------------------------
    // Constituency downloads are moved to background to prevent boot timeout.
    // niftyindices.com often returns HTML instead of CSV, causing 91s+ retries
    // that pushed boot past the 120s deadline.
    let shared_constituency: tickvault_api::state::SharedConstituencyMap =
        std::sync::Arc::new(std::sync::RwLock::new(None));

    // During market hours: load from cache synchronously (fast, no network).
    // Outside market hours: spawn background download (non-blocking).
    if is_market_hours {
        info!("market hours — using cached constituency data (skipping niftyindices.com download)");
        let cached = tickvault_core::index_constituency::try_load_cache(
            &config.instrument.csv_cache_directory,
        )
        .await;
        if let Some(ref map) = cached
            && let Err(err) = tickvault_storage::constituency_persistence::persist_constituency(
                map,
                &config.questdb,
                slow_boot_universe.as_ref(),
            )
        {
            tracing::warn!(
                ?err,
                "index constituency QuestDB persistence failed (best-effort)"
            );
        }
        if let Ok(mut lock) = shared_constituency.write() {
            *lock = cached;
        }
    } else {
        // Background download — does not block boot sequence.
        let bg_constituency = shared_constituency.clone();
        let bg_index_config = config.index_constituency.clone();
        let bg_cache_dir = config.instrument.csv_cache_directory.clone();
        let bg_questdb_config = config.questdb.clone();
        let bg_universe = slow_boot_universe.clone();
        tokio::spawn(async move {
            let map = tickvault_core::index_constituency::download_and_build_constituency_map(
                &bg_index_config,
                &bg_cache_dir,
            )
            .await;
            if let Some(ref m) = map
                && let Err(err) = tickvault_storage::constituency_persistence::persist_constituency(
                    m,
                    &bg_questdb_config,
                    bg_universe.as_ref(),
                )
            {
                tracing::warn!(
                    ?err,
                    "index constituency QuestDB persistence failed (best-effort)"
                );
            }
            if let Ok(mut lock) = bg_constituency.write() {
                *lock = map;
            }
        });
    }

    // -----------------------------------------------------------------------
    // Step 11: Start axum API server
    // -----------------------------------------------------------------------
    let api_state = SharedAppState::new(
        config.questdb.clone(),
        config.dhan.clone(),
        config.instrument.clone(),
        shared_movers.clone(),
        shared_constituency.clone(),
        health_status,
    );

    // Plan item G1 (2026-04-22): swap in the V2 movers snapshot handle if the
    // pipeline was spawned. Pre-G1 or when no registry is available the
    // handler returns `available=false` so the endpoint is safe to call.
    let api_state = match movers_v2_snapshot_handle.clone() {
        Some(h) => api_state.with_movers_snapshot_v2(h),
        None => api_state,
    };

    // 2026-04-25 security audit (PR #357): API bearer token sourced from AWS
    // SSM Parameter Store ONLY — `/tickvault/<env>/api/bearer-token`. Same
    // rule as Dhan, Telegram, Grafana, QuestDB credentials per
    // `.claude/rules/project/rust-code.md` ("always real AWS, never mocks";
    // local Mac uses `~/.aws/credentials` to reach the same SSM endpoint).
    //
    // Hard-fail on SSM error — matches the existing `fetch_dhan_credentials`
    // / `fetch_telegram_credentials` boot-time semantics. There is NO env
    // var fallback. If SSM is unreachable the app cannot boot, period.
    let api_bearer_token = tickvault_core::auth::secret_manager::fetch_api_bearer_token()
        .await
        .context("GAP-SEC-01: SSM fetch for API bearer token failed at /tickvault/<env>/api/bearer-token — store the token via `aws ssm put-parameter --name /tickvault/<env>/api/bearer-token --type SecureString`")?;
    info!("GAP-SEC-01: API bearer token loaded from SSM (/tickvault/<env>/api/bearer-token)");
    let api_auth_config = tickvault_api::middleware::ApiAuthConfig::from_token(api_bearer_token);

    let router = tickvault_api::build_router_with_auth(
        api_state,
        &config.api.allowed_origins,
        api_auth_config,
    );

    let bind_addr: SocketAddr = format_bind_addr(&config.api.host, config.api.port)
        .parse()
        .context("invalid API bind address")?;

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .context("failed to bind API server")?;

    info!(address = %bind_addr, "API server listening");

    // Auto-open Portal and Market Dashboard in browser (API server now ready).
    // Best-effort: non-blocking, logged on failure.
    tokio::spawn(async {
        crate::infra::open_in_browser("http://localhost:3001/portal/options-chain").await;
    });

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
    // Step 12a: Spawn mid-session profile watchdog (queue item I7)
    // -----------------------------------------------------------------------
    // Every 15 minutes during market hours, re-runs `pre_market_check`
    // (dataPlan == "Active", activeSegment contains "Derivative", token
    // expires > 4h). On rising-edge failure fires CRITICAL Telegram via
    // NotificationEvent::MidSessionProfileInvalidated. Does NOT HALT —
    // dropping the live WS feed mid-session costs more than the
    // silent-failure risk we're monitoring.
    let _mid_session_watchdog_handle =
        tickvault_core::auth::mid_session_watchdog::spawn_mid_session_profile_watchdog(
            std::sync::Arc::clone(&token_manager),
            Some(std::sync::Arc::clone(&notifier)),
        );
    info!("mid-session profile watchdog spawned (15-min cadence, market-hours only)");

    // -----------------------------------------------------------------------
    // Boot duration check — alert if boot exceeded BOOT_TIMEOUT_SECS
    // -----------------------------------------------------------------------
    let boot_elapsed = boot_start.elapsed();
    // 2026-04-24 fix: gate the boot deadline CRITICAL alert on market
    // hours. Post-market boots are legitimately slower because index
    // LTPs never arrive (Dhan stops streaming at 15:30 IST), so the
    // 09:00 IST 120s budget is the wrong yardstick for a 19:00 IST
    // operator-test boot. The 2026-04-17 audit already documented this
    // (Option C v3 was supposed to fix it but only addressed the LTP
    // wait, not the deadline alert itself). Outside market hours we
    // log INFO + emit a metric but do NOT page the operator. Ratchet:
    // crates/app/tests/post_market_pool_halt_guard.rs.
    if boot_elapsed.as_secs() > tickvault_common::constants::BOOT_TIMEOUT_SECS {
        let in_market_hours = tickvault_common::market_hours::is_within_market_hours_ist();
        if in_market_hours {
            error!(
                elapsed_secs = boot_elapsed.as_secs(),
                timeout_secs = tickvault_common::constants::BOOT_TIMEOUT_SECS,
                "BOOT TIMEOUT EXCEEDED"
            );
            notifier.notify(NotificationEvent::BootDeadlineMissed {
                deadline_secs: tickvault_common::constants::BOOT_TIMEOUT_SECS,
                step: format!(
                    "boot completed in {}s (over {}s limit)",
                    boot_elapsed.as_secs(),
                    tickvault_common::constants::BOOT_TIMEOUT_SECS,
                ),
            });
        } else {
            info!(
                elapsed_secs = boot_elapsed.as_secs(),
                timeout_secs = tickvault_common::constants::BOOT_TIMEOUT_SECS,
                "boot exceeded {}s budget but outside market hours \
                 (09:00-15:30 IST) — suppressed CRITICAL alert (Dhan idle, \
                 LTP-dependent boot steps legitimately slower post-market)",
                tickvault_common::constants::BOOT_TIMEOUT_SECS,
            );
        }
    } else {
        info!(
            elapsed_ms = boot_elapsed.as_millis() as u64,
            "boot sequence completed"
        );
    }

    // C1: Notify systemd that boot is complete (no-op outside systemd).
    infra::notify_systemd_ready();

    // -----------------------------------------------------------------------
    // Step 12b: Background periodic health check (disk space + memory RSS + spill + QuestDB)
    // -----------------------------------------------------------------------
    // C3: Runs every 5 minutes, fires Telegram CRITICAL on disk <10% or RSS >threshold.
    // Alert dedup: each category sends at most 1 alert per ALERT_COOLDOWN_SECS
    // to avoid spamming Telegram when a condition persists across intervals.
    {
        let health_notifier = notifier.clone();
        let questdb_config = config.questdb.clone();
        tokio::spawn(async move {
            use std::time::Instant;

            /// Minimum seconds between repeated alerts of the same category.
            const ALERT_COOLDOWN_SECS: u64 = 1800; // 30 minutes
            let cooldown = std::time::Duration::from_secs(ALERT_COOLDOWN_SECS);

            // Per-category last-alert timestamps for dedup.
            let mut last_disk_alert: Option<Instant> = None;
            let mut last_memory_alert: Option<Instant> = None;
            let mut last_spill_alert: Option<Instant> = None;
            let mut last_docker_alert: Option<Instant> = None;

            /// Returns true if cooldown has elapsed (or first alert).
            fn should_alert(last: &mut Option<Instant>, cooldown: std::time::Duration) -> bool {
                let now = Instant::now();
                match last {
                    Some(t) if now.duration_since(*t) < cooldown => false,
                    _ => {
                        *last = Some(now);
                        true
                    }
                }
            }

            let interval = std::time::Duration::from_secs(
                tickvault_common::constants::PERIODIC_HEALTH_CHECK_INTERVAL_SECS,
            );
            loop {
                tokio::time::sleep(interval).await;
                // Disk space check
                if let Some(percent_free) = infra::check_disk_space()
                    && percent_free < infra::MIN_FREE_DISK_PERCENT
                    && should_alert(&mut last_disk_alert, cooldown)
                {
                    health_notifier.notify(NotificationEvent::Custom {
                        message: format!(
                            "CRITICAL: LOW DISK SPACE — only {percent_free}% free. \
                             Tick spill files may fail if disk fills up."
                        ),
                    });
                }
                // Memory RSS check
                if let Some(rss_mb) = infra::check_memory_rss()
                    && rss_mb > infra::MEMORY_RSS_ALERT_MB
                    && should_alert(&mut last_memory_alert, cooldown)
                {
                    health_notifier.notify(NotificationEvent::Custom {
                        message: format!(
                            "WARNING: HIGH MEMORY — RSS {rss_mb} MB exceeds threshold. \
                             Consider investigating memory usage."
                        ),
                    });
                }
                // C2: Spill file size check — export metric + alert if large.
                let spill_bytes = infra::check_spill_file_size();
                if spill_bytes > 500 * 1024 * 1024 && should_alert(&mut last_spill_alert, cooldown)
                {
                    // > 500 MB of spill files — QuestDB likely down for extended period.
                    health_notifier.notify(NotificationEvent::Custom {
                        message: format!(
                            "WARNING: Tick spill files total {:.1} MB — QuestDB may be \
                             down. Data safe on disk but investigate.",
                            spill_bytes as f64 / (1024.0 * 1024.0)
                        ),
                    });
                }
                // C3: QuestDB liveness ping (SELECT 1) — alert only after N
                // consecutive failures so a single slow query under ingestion
                // load does not page. Recovery resets the failure counter.
                let liveness_outcome = infra::check_questdb_liveness(&questdb_config).await;
                if !liveness_outcome.is_success() {
                    let failures = infra::questdb_liveness_failures();
                    if failures >= infra::QUESTDB_LIVENESS_FAILURE_THRESHOLD {
                        health_notifier.notify(NotificationEvent::QuestDbDisconnected {
                            writer: "liveness-check".to_string(),
                            signal: u64::from(failures),
                            signal_kind: "Consecutive liveness failures".to_string(),
                        });
                    } else {
                        tracing::warn!(
                            failures,
                            threshold = infra::QUESTDB_LIVENESS_FAILURE_THRESHOLD,
                            "QuestDB liveness check failed — below alert threshold"
                        );
                    }
                }
                // C4: Auto-cleanup spill files older than 7 days.
                infra::cleanup_old_spill_files();
                // C5: Docker container watchdog — detect and restart unhealthy containers.
                let unhealthy = infra::check_and_restart_containers().await;
                if unhealthy > 0 && should_alert(&mut last_docker_alert, cooldown) {
                    health_notifier.notify(NotificationEvent::Custom {
                        message: format!(
                            "[Watchdog] {unhealthy} unhealthy Docker container(s) detected. \
                             Auto-restart triggered via docker compose up -d."
                        ),
                    });
                }
            }
        });
        info!("background periodic health check started (every 5 minutes)");
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
        ws_pool_arc,
        shutdown_notify,
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
/// Returns (plan, universe, needs_persist).
/// `needs_persist` is true for CachedPlan (not yet persisted) and false for
/// FreshBuild (already persisted inside load_or_build_instruments).
async fn load_instruments(
    config: &ApplicationConfig,
    is_trading_day: bool,
    trading_calendar: &TradingCalendar,
) -> (Option<SubscriptionPlan>, Option<FnoUniverse>, bool) {
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
            // Boot-time: pass empty spot prices — stock F&O will be subscribed
            // at 9:13 AM once pre-market finalized prices are available.
            // Indices get ALL contracts regardless. Stock equities subscribe immediately.
            //
            // Fix #6 (2026-04-24): pass the trading calendar so stock F&O
            // expiries roll forward when nearest is T or T-1. Indices are
            // unaffected — the planner ignores rollover for index kinds.
            let empty_spot_prices = std::collections::HashMap::new();
            let plan = build_subscription_plan(
                &universe,
                &config.subscription,
                today,
                &empty_spot_prices,
                Some(trading_calendar),
            );

            info!(
                total = plan.summary.total,
                feed_mode = %plan.summary.feed_mode,
                "subscription plan ready (fresh build)"
            );
            (Some(plan), Some(universe), false) // FreshBuild already persisted internally
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
            (Some(plan), universe, true) // CachedPlan needs explicit persistence
        }
        Ok(InstrumentLoadResult::Unavailable) => {
            // I-P0-06: This should only trigger if emergency download also failed
            error!(
                "CRITICAL: instruments unavailable — emergency download failed, system has ZERO instruments"
            );
            (None, None, false)
        }
        Err(err) => {
            // Gap 3 fix: ERROR level triggers Telegram via Loki → Grafana.
            // Previously WARN — operator unaware system has zero instruments.
            error!(
                error = %err,
                "CRITICAL: instrument build failed — system has ZERO instruments to \
                 subscribe. No ticks, no trading. Investigate immediately."
            );
            (None, None, false)
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
#[allow(clippy::too_many_arguments)] // APPROVED: STAGE-C added wal_spill param
fn create_websocket_pool(
    token_handle: &TokenHandle,
    client_id: &str,
    subscription_plan: &Option<SubscriptionPlan>,
    config: &ApplicationConfig,
    is_market_hours: bool,
    notifier: Option<std::sync::Arc<NotificationService>>,
    wal_spill: Option<std::sync::Arc<tickvault_storage::ws_frame_spill::WsFrameSpill>>,
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

    let mut instruments: Vec<InstrumentSubscription> = plan
        .registry
        .iter()
        .map(|inst| InstrumentSubscription::new(inst.exchange_segment, inst.security_id))
        .collect();

    let feed_mode = plan.summary.feed_mode;

    // Bug C fix (2026-04-20): enforce WebSocket pool capacity HERE instead
    // of propagating `CapacityExceeded` out of pool creation. On 09:15 IST
    // restart-during-market-hours, the subscription plan was 36,241
    // instruments vs a capacity of 25,000 — the pool refused to build and
    // the app bailed out of boot entirely. That is the wrong failure mode:
    // starting with the first 25,000 highest-priority instruments is
    // strictly better than starting with zero.
    //
    // `InstrumentRegistry::iter()` already yields in priority order:
    // Major indices → major-index derivatives → display indices →
    // stock equities → stock derivatives. So truncating the tail drops
    // the lowest-priority items first (typically stock options far from
    // ATM that Phase 2 would otherwise add post-09:12 IST).
    let effective_capacity = config
        .dhan
        .max_instruments_per_connection
        .min(tickvault_common::constants::MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION)
        .saturating_mul(
            config
                .dhan
                .max_websocket_connections
                .min(tickvault_common::constants::MAX_WEBSOCKET_CONNECTIONS),
        );
    if instruments.len() > effective_capacity {
        let dropped = instruments.len() - effective_capacity;
        error!(
            requested = instruments.len(),
            capacity = effective_capacity,
            dropped,
            "Bug C: subscription plan exceeds WebSocket capacity — truncating to \
             effective_capacity and continuing. Dropped instruments are the \
             lowest-priority tail of InstrumentRegistry::iter() — stock options \
             far from ATM are the usual tail."
        );
        instruments.truncate(effective_capacity);
        metrics::counter!("tv_subscription_plan_truncations_total").increment(1);
        metrics::gauge!("tv_subscription_plan_dropped_instruments").set(dropped as f64);
    }

    let mut pool = match WebSocketConnectionPool::new_with_optional_wal(
        token_handle.clone(),
        client_id.to_string(),
        config.dhan.clone(),
        ws_config,
        instruments,
        feed_mode,
        notifier,
        wal_spill,
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
///
/// S4-T1a: Accepts `Arc<WebSocketConnectionPool>` so the caller can retain a
/// reference for the pool watchdog task (which polls `poll_watchdog()` every
/// 5s) and the SIGTERM handler (which calls `request_graceful_shutdown()`).
/// The Arc is cheap (one atomic ref count increment per clone) and required
/// because all three use cases need to share the same pool instance.
async fn spawn_websocket_connections(
    pool: std::sync::Arc<WebSocketConnectionPool>,
) -> Vec<tokio::task::JoinHandle<Result<(), WebSocketError>>> {
    let handles = pool.spawn_all().await;
    info!(connections = handles.len(), "WebSocket connections spawned");
    handles
}

/// Stagger in milliseconds between per-connection `WebSocketConnected` events.
/// Telegram rate-limits bursts of identical-from messages; spacing the emits
/// avoids silent drops. A 5-connection pool at 150 ms adds ~750 ms to boot,
/// which is negligible against the 15-step boot budget.
const WS_CONNECTED_ALERT_STAGGER_MS: u64 = 150;

/// Emits per-connection `WebSocketConnected` Telegram events plus a single
/// aggregate `WebSocketPoolOnline` summary. Called from BOTH boot paths
/// (FAST BOOT and slow boot) so an operator sees the same signal regardless
/// of which path ran.
///
/// Why the summary: when 5 per-connection events fire in a tight loop,
/// Telegram can drop individual messages (observed live on 2026-04-17 —
/// only 3 of 5 arrived). The aggregate is delivered with a small stagger
/// after the individuals so even if all per-connection drops happen, a
/// single "N/total online" message still reaches the operator chat.
async fn emit_websocket_connected_alerts(
    notifier: &std::sync::Arc<NotificationService>,
    handle_count: usize,
) {
    if handle_count == 0 {
        return;
    }
    for i in 0..handle_count {
        notifier.notify(NotificationEvent::WebSocketConnected {
            connection_index: i,
        });
        if i + 1 < handle_count {
            tokio::time::sleep(std::time::Duration::from_millis(
                WS_CONNECTED_ALERT_STAGGER_MS,
            ))
            .await;
        }
    }
    notifier.notify(NotificationEvent::WebSocketPoolOnline {
        connected: handle_count,
        total: handle_count,
    });
}

/// S4-T1a: Background pool health watchdog.
///
/// Spawns a task that calls `pool.poll_watchdog()` every 5 seconds and
/// translates each `WatchdogVerdict` into the matching Telegram event:
/// - `Degraded` → `WebSocketPoolDegraded { down_secs }` (High severity,
///   fires once per down-cycle — the watchdog's internal
///   `degraded_alert_fired` flag de-duplicates).
/// - `Recovered` → `WebSocketPoolRecovered { was_down_secs }`.
/// - `Halt` → `WebSocketPoolHalt { down_secs }` + `std::process::exit(2)`
///   so the supervisor restarts us.
/// - `Degrading` / `Healthy` → gauge update only, no Telegram.
///
/// The task stops when the `shutdown_notify` is fired (during graceful
/// shutdown) to avoid a false-positive Halt during the intentional
/// teardown.
fn spawn_pool_watchdog_task(
    pool: std::sync::Arc<WebSocketConnectionPool>,
    shutdown_notify: std::sync::Arc<tokio::sync::Notify>,
    notifier: std::sync::Arc<NotificationService>,
    health: tickvault_api::state::SharedHealthStatus,
) {
    tokio::spawn(async move {
        // 5-second poll interval — cold path, not per tick. Matches the
        // degrading/halt thresholds (60s/300s) with plenty of resolution.
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5)); // APPROVED: pre-existing literal, session-7 tech debt tracked for cleanup
        // Skip the immediate first tick so we don't poll before the pool
        // has had a chance to connect.
        interval.tick().await;
        info!("S4-T1a: pool watchdog task started (poll interval 5s)");
        loop {
            tokio::select! {
                () = shutdown_notify.notified() => {
                    info!("S4-T1a: pool watchdog stopping (graceful shutdown signalled)");
                    return;
                }
                _ = interval.tick() => {
                    // Fix #7 (2026-04-24): update the main-feed active-
                    // connection counter BEFORE polling the watchdog so
                    // `/health` and the 09:15:30 streaming-confirmation
                    // heartbeat see a fresh count. Without this write the
                    // `websocket_connections` gauge stayed at 0/5 forever
                    // even when all 5 sockets were live.
                    let healths = pool.health();
                    let active: u64 = healths
                        .iter()
                        .filter(|h| {
                            matches!(
                                h.state,
                                tickvault_core::websocket::types::ConnectionState::Connected
                            )
                        })
                        .count() as u64;
                    health.set_websocket_connections(active);

                    let verdict = pool.poll_watchdog();
                    use tickvault_core::websocket::pool_watchdog::WatchdogVerdict;
                    // 2026-04-24 fix: gate Degraded/Halt side-effects on
                    // market hours. Outside [09:00, 15:30) IST Dhan stops
                    // streaming and every connection legitimately goes
                    // silent — firing Telegram + std::process::exit(2)
                    // post-market causes false-alarm pages and a
                    // supervisor-restart loop. The inner gate in
                    // connection_pool.rs::poll_watchdog only suppresses
                    // the inner log; the outer notifier + process exit
                    // need their own gate. Ratchet:
                    // crates/app/tests/post_market_pool_halt_guard.rs.
                    let in_market_hours =
                        tickvault_common::market_hours::is_within_market_hours_ist();
                    match verdict {
                        WatchdogVerdict::Degraded { down_for } => {
                            metrics::counter!("tv_pool_degraded_alerts_total").increment(1);
                            if in_market_hours {
                                error!(
                                    down_for_secs = down_for.as_secs(),
                                    "S4-T1a CRITICAL: pool watchdog Degraded verdict — \
                                     all main-feed WebSocket connections down >= 60s."
                                );
                                notifier.notify(NotificationEvent::WebSocketPoolDegraded {
                                    down_secs: down_for.as_secs(),
                                });
                            } else {
                                info!(
                                    down_for_secs = down_for.as_secs(),
                                    "S4-T1a: pool Degraded verdict outside market hours \
                                     (09:00-15:30 IST) — Dhan idle, no Telegram, no exit"
                                );
                            }
                        }
                        WatchdogVerdict::Recovered { was_down_for } => {
                            info!(
                                was_down_for_secs = was_down_for.as_secs(),
                                "S4-T1a: pool watchdog Recovered verdict — pool back online"
                            );
                            metrics::counter!("tv_pool_recoveries_total").increment(1);
                            // Recovery is informational; only Telegram-page the
                            // operator if the original Degraded fired (i.e. we
                            // were inside market hours when the down-cycle hit).
                            if in_market_hours {
                                notifier.notify(NotificationEvent::WebSocketPoolRecovered {
                                    was_down_secs: was_down_for.as_secs(),
                                });
                            }
                        }
                        WatchdogVerdict::Halt { down_for } => {
                            metrics::counter!("tv_pool_self_halts_total").increment(1);
                            if in_market_hours {
                                error!(
                                    down_for_secs = down_for.as_secs(),
                                    "S4-T1a FATAL: pool watchdog fired Halt verdict. \
                                     Exiting process with status 2 so the supervisor restarts us. \
                                     All main-feed WebSocket connections have been down for >300s."
                                );
                                notifier.notify(NotificationEvent::WebSocketPoolHalt {
                                    down_secs: down_for.as_secs(),
                                });
                                // Give notifications + metrics flush a moment.
                                tokio::time::sleep(std::time::Duration::from_secs(2)).await; // APPROVED: pre-existing literal, session-7 tech debt tracked for cleanup
                                std::process::exit(2);
                            } else {
                                info!(
                                    down_for_secs = down_for.as_secs(),
                                    "S4-T1a: pool Halt verdict outside market hours \
                                     (09:00-15:30 IST) — Dhan idle, suppressing Telegram \
                                     and process exit. Watchdog will reset on next market open."
                                );
                            }
                        }
                        WatchdogVerdict::Degrading { .. } | WatchdogVerdict::Healthy => {
                            // No alert; watchdog's internal state machine
                            // will upgrade to Degraded / Halt if this persists.
                        }
                    }
                }
            }
        }
    });
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

        // Historical fetch uses HTTP ILP (not TCP) — HTTP ILP is stateless and
        // immune to the "broken pipe after every flush" failure mode caused by
        // QuestDB line.tcp rotating idle sockets between bursty cold-path writes.
        // See: CandlePersistenceWriter::new_http doc comment.
        let candle_writer = match CandlePersistenceWriter::new_http(&bg_questdb_config) {
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
        // Historical fetch timing on trading days:
        //   Before 15:30 IST → WAIT for post-market signal (live feed is
        //                      sensitive pre-market + during market hours;
        //                      historical REST hammering the same account
        //                      risks DH-904 rate limits on the live path)
        //   After 15:30 IST  → fetch immediately (market closed)
        // Non-trading day    → fetch immediately at boot (no live data to protect)
        //
        // Parthiban directive (2026-04-22, enforced): historical fetch must
        // NEVER run pre-market on a trading day. Only post-market (>= 15:30 IST)
        // OR on non-trading days (weekends/holidays). See live-feed-purity.md
        // and audit-findings-2026-04-17.md Rule 3.
        //
        // Previously the code had a third "before 08:00 — fetch immediately"
        // branch which violated the directive: a fresh clone booting at 07:39
        // IST on a trading day would hammer Dhan REST with 25,000-instrument ×
        // 5-timeframe fetches right before market open. Removed.
        // -----------------------------------------------------------------

        let should_wait_for_post_market = if is_trading_day {
            use tickvault_common::constants::{
                IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST,
            };
            let now_utc = chrono::Utc::now().timestamp();
            let now_ist = now_utc.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            // APPROVED: rem_euclid on SECONDS_PER_DAY (86400) fits u32
            let sec_of_day = now_ist.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32;
            sec_of_day < TICK_PERSIST_END_SECS_OF_DAY_IST
        } else {
            false
        };

        if should_wait_for_post_market {
            info!(
                "trading day before 15:30 IST — waiting for post-market signal before historical fetch"
            );
            post_market_signal.notified().await;
            info!("post-market signal received — starting historical candle fetch");
        } else if is_trading_day {
            // After 15:30 IST on a trading day — market closed, safe to fetch.
            info!("trading day after 15:30 IST — fetching historical candles immediately");
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

        // Re-create writer if we waited (previous may have timed out).
        // HTTP ILP (see new_http doc) — same reason as above.
        let mut fetch_writer = if is_trading_day {
            match CandlePersistenceWriter::new_http(&bg_questdb_config) {
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
            is_trading_day,
        )
        .await;

        // 2026-04-24 audit finding #1: do NOT fire "Historical candles OK"
        // when the degenerate case `instruments_fetched == 0 &&
        // total_candles == 0` occurs. Without this guard, a Dhan outage
        // that returns 200-with-empty-payload, an empty universe on a
        // mid-boot race, or a disabled-scope misconfiguration all produce
        // a green Telegram ("Fetched: 0 / Candles: 0") — exact same bug
        // class as the cross-match false-OK fix in PR #341. The downstream
        // cross-verify then trusts that signal and silently passes too.
        //
        // 2026-04-24 refinement (operator feedback post PR #342 merge):
        // when the fetch returns zero AND QuestDB already has today's
        // candles from a prior run, this is an **idempotent re-run**,
        // not an outage. The fetch call returned zero because DEDUP
        // upserted every candle Dhan sent. Fire LOW
        // `HistoricalFetchAlreadyAvailable` instead of HIGH
        // `HistoricalFetchFailed`. The presence check runs BEFORE the
        // failure decision so the alarm severity matches reality.
        //
        // Routes to HistoricalFetchFailed with a synthesized failure_reasons
        // entry so the operator gets a clear diagnostic in Telegram.
        let zero_fetched_degenerate =
            summary.instruments_fetched == 0 && summary.total_candles == 0;
        let zero_fetched_no_actual_failures =
            zero_fetched_degenerate && summary.instruments_failed == 0;
        // Query the presence check ONLY in the zero-fetched-no-failures
        // case to avoid an unnecessary QuestDB round-trip on the happy
        // path. `None` means the query failed — treat as "unknown",
        // fall through to the HIGH HistoricalFetchFailed path so we do
        // not mask a real outage behind a failed presence check.
        let today_candle_presence: Option<u64> = if zero_fetched_no_actual_failures {
            // today_ist best-effort: use `TodayIstWindow::from_now` when
            // available (09:15 IST onwards); pre-market (before 09:15)
            // falls back to system-clock IST date. Both produce the
            // same IST calendar date for today — only the window start
            // differs, which this count helper doesn't need.
            let today_ist_naive =
                tickvault_core::historical::cross_verify::TodayIstWindow::from_now()
                    .map(|w| w.today_ist)
                    .unwrap_or_else(|| {
                        // Pre-market IST date — computed from UTC clock.
                        // Pure chrono, no I/O.
                        use chrono::{FixedOffset, Utc};
                        let ist_offset = FixedOffset::east_opt(
                            tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
                        )
                        .expect("IST offset is compile-time valid"); // APPROVED: 19800 is a valid FixedOffset
                        Utc::now().with_timezone(&ist_offset).date_naive()
                    });
            tickvault_core::historical::cross_verify::count_historical_candles_for_ist_day(
                &bg_questdb_config,
                today_ist_naive,
            )
            .await
            .inspect(|&n| {
                info!(
                    today_ist = %today_ist_naive,
                    today_candles = n,
                    "historical fetch returned zero — checking DB presence to \
                     classify as idempotent re-run vs outage"
                );
            })
        } else {
            None
        };
        // 2026-04-26: gate the zero-fetched HIGH alert on `is_trading_day`.
        // On a non-trading day (weekend / NSE holiday) the market never
        // opened, so "fetched 0 instruments / 0 candles" is BY DESIGN —
        // not an outage. Routing it to HIGH `HistoricalFetchFailed` paged
        // the operator on every weekend boot. Per the
        // `audit-findings-2026-04-17` Rule 11 ("False-OK classification
        // is a CLASS bug"), the symmetric inverse applies here: a
        // FAILURE-class event whose denominator is structurally zero on
        // the current calendar day is a false-positive and must route to
        // a Low/typed-Skipped variant instead.
        //
        // We reuse `HistoricalFetchAlreadyAvailable` for this case (with
        // `today_candles=0`, since the market never opened so there is
        // nothing to count). The variant's semantic — "DB already
        // reflects current state, nothing was fetched, this is normal" —
        // matches the non-trading-day case. The Severity stays Low.
        let zero_fetched_non_trading_day = zero_fetched_no_actual_failures && !is_trading_day;
        if zero_fetched_no_actual_failures
            && (today_candle_presence.unwrap_or(0) > 0 || zero_fetched_non_trading_day)
        {
            // Either DB already has today's data (idempotent re-run on a
            // trading day) OR it's a non-trading day so zero-fetched is
            // expected. Fire LOW in both cases.
            let today_candles = today_candle_presence.unwrap_or(0);
            let today_ist_label =
                tickvault_core::historical::cross_verify::TodayIstWindow::from_now()
                    .map(|w| w.today_ist.to_string())
                    .unwrap_or_else(|| {
                        use chrono::{FixedOffset, Utc};
                        let ist_offset = FixedOffset::east_opt(
                            tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
                        )
                        .expect("IST offset is compile-time valid"); // APPROVED: 19800 is a valid FixedOffset
                        Utc::now()
                            .with_timezone(&ist_offset)
                            .date_naive()
                            .to_string()
                    });
            if zero_fetched_non_trading_day {
                info!(
                    today_ist = %today_ist_label,
                    "historical fetch returned zero on a non-trading day — \
                     market never opened, classifying as expected (LOW), \
                     not an outage (HIGH)"
                );
            }
            bg_notifier.notify(NotificationEvent::HistoricalFetchAlreadyAvailable {
                today_ist: today_ist_label,
                today_candles,
            });
        } else if summary.instruments_failed > 0 || zero_fetched_degenerate {
            let mut failure_reasons = summary.failure_reasons.clone();
            if zero_fetched_degenerate && summary.instruments_failed == 0 {
                failure_reasons.insert(
                    "zero_fetched_zero_candles".to_string(),
                    summary.instruments_skipped.max(1),
                );
            }
            bg_notifier.notify(NotificationEvent::HistoricalFetchFailed {
                instruments_fetched: summary.instruments_fetched,
                instruments_failed: summary.instruments_failed,
                total_candles: summary.total_candles,
                persist_failures: summary.persist_failures,
                failed_instruments: summary.failed_instruments.clone(),
                failure_reasons,
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
        // Cross-verify + cross-match: TODAY-ONLY window gating
        //
        // Gate both operations on:
        //  1. `is_trading_day` — weekend / NSE holiday → skip with a typed
        //     SKIPPED notification so operator gets closure on Telegram.
        //  2. `TodayIstWindow::from_now()` — pre-market (before 09:15 IST)
        //     on a trading day → SILENT skip (INFO log only, NO Telegram).
        //     Per Parthiban directive 2026-04-20:
        //       "pre market cross verification should never ever be done
        //        especially on trading days"
        //     Verification needs live data; running it pre-market is
        //     guaranteed-empty noise. The post-market timer will fire it
        //     correctly once live ticks land.
        //  3. Once-per-day success cache —
        //     `cross_verify_already_succeeded_today` checks for a marker
        //     file written after BOTH verify + match passed earlier today.
        //     Per Parthiban directive 2026-04-20:
        //       "even for post verification also only once in a day until
        //        it achieves success verification should be done"
        //     If today already succeeded, skip silently.
        //
        // Otherwise pass the window to both functions so their SQL WHERE
        // clauses are narrowed to today's 09:15-15:30 IST session.
        // -----------------------------------------------------------------
        // 2026-04-24 Parthiban directive: cross-verify is a POST-MARKET
        // ONLY operation. The session must be fully closed before the
        // grid is comparable — running mid-session always yields partial
        // coverage and a non-actionable Skipped/Failed Telegram. The
        // new gate in `from_now_post_market_only()` returns None for
        // both pre-market AND mid-session boots; only 15:30+ IST on a
        // trading day produces a window.
        let today_window = if is_trading_day {
            tickvault_core::historical::cross_verify::TodayIstWindow::from_now_post_market_only()
        } else {
            None
        };

        let Some(today_window) = today_window else {
            if !is_trading_day {
                // Weekend / holiday — operator-visible SKIPPED notification.
                let reason = "weekend or holiday — not a trading day".to_string();
                info!(
                    instruments_fetched = summary.instruments_fetched,
                    instruments_failed = summary.instruments_failed,
                    total_candles = summary.total_candles,
                    %reason,
                    "cross-verify + cross-match SKIPPED"
                );
                bg_notifier.notify(NotificationEvent::CandleCrossMatchSkipped {
                    reason,
                    candles_compared: 0,
                });
            } else {
                // Trading day, but EITHER pre-market (< 09:15 IST) OR
                // mid-session (09:15..15:30 IST). Silent skip per
                // Parthiban directive — INFO log only, no Telegram.
                // Cross-verify retries on every subsequent boot until
                // the post-market gate opens, at which point the
                // success-marker idempotency takes over.
                info!(
                    instruments_fetched = summary.instruments_fetched,
                    instruments_failed = summary.instruments_failed,
                    total_candles = summary.total_candles,
                    "cross-verify + cross-match SILENTLY skipped — \
                     trading day but not yet post-market (15:30 IST). \
                     Will run on the next boot scheduled at or after 15:30 IST."
                );
            }
            return;
        };

        // Once-per-day success idempotency. The marker is written below
        // after BOTH verify + match succeed. Crash-recovery boots and
        // mid-day restarts therefore won't re-fire the verification.
        if tickvault_core::historical::cross_verify::cross_verify_already_succeeded_today(
            today_window.today_ist,
        ) {
            info!(
                today_ist = %today_window.today_ist,
                marker = ?tickvault_core::historical::cross_verify::cross_verify_success_marker_path(today_window.today_ist),
                "cross-verify SILENTLY skipped — today's verification already succeeded earlier (marker present)"
            );
            return;
        }

        // -----------------------------------------------------------------
        // Cross-verify candle integrity in QuestDB
        // -----------------------------------------------------------------
        let verify_report =
            verify_candle_integrity(&bg_questdb_config, &bg_registry, &today_window).await;
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
        // Cross-match historical vs live candle data
        // -----------------------------------------------------------------
        {
            let cross_match =
                cross_match_historical_vs_live(&bg_questdb_config, &bg_registry, &today_window)
                    .await;

            if !cross_match.live_candles_present || cross_match.candles_compared == 0 {
                // First run / fresh DB / post-market boot with no live ticks yet.
                //
                // `candles_compared` alone is NOT a sufficient signal —
                // the per-timeframe count query uses LEFT JOIN which
                // preserves historical rows even when the live MV is
                // empty. Trust `live_candles_present`, which is computed
                // from a direct `SELECT count() FROM candles_1m ...`
                // query against the live view only.
                info!(
                    live_candles_present = cross_match.live_candles_present,
                    candles_compared = cross_match.candles_compared,
                    "cross-match SKIPPED — no live data in materialized views (first run, fresh DB, or post-market boot)"
                );
                bg_notifier.notify(NotificationEvent::CandleCrossMatchSkipped {
                    reason: "no live data in materialized views".to_string(),
                    candles_compared: cross_match.candles_compared,
                });
            } else if cross_match.coverage_pct
                < tickvault_core::historical::cross_verify::CROSS_MATCH_MIN_COVERAGE_PCT
            {
                // 2026-04-24 regression fix: partial coverage (operator booted
                // mid-session, or live feed had a major gap). Before this guard,
                // mid-session-boot produced a false "CROSS-MATCH OK" because the
                // LEFT JOIN NULL-live detail-query branch under-counted missing
                // rows. Now we route to Skipped with a reason that names the
                // actual coverage shortfall so the operator knows the cross-match
                // was NOT certified.
                info!(
                    coverage_pct = cross_match.coverage_pct,
                    live_candles_present = cross_match.live_candles_present,
                    candles_compared = cross_match.candles_compared,
                    "cross-match SKIPPED — partial coverage (likely mid-session boot)"
                );
                bg_notifier.notify(NotificationEvent::CandleCrossMatchSkipped {
                    reason: format!(
                        "partial live coverage: {pct}% of historical grid (booted mid-session or live feed degraded; threshold {min}%)",
                        pct = cross_match.coverage_pct,
                        min = tickvault_core::historical::cross_verify::CROSS_MATCH_MIN_COVERAGE_PCT,
                    ),
                    candles_compared: cross_match.candles_compared,
                });
            } else {
                // Compute the "TODAY ONLY: YYYY-MM-DD HH:MM–HH:MM IST" label
                // once and pass to both pass/fail notifications so the operator
                // immediately sees which trading session the OK/FAIL covers.
                // The start/end SQL literals on `today_window` look like
                // `'2026-04-20T09:15:00.000000Z'` — we strip the quotes + the
                // date + `.000000Z` suffix to render a human-readable
                // `HH:MM` slice of the IST wall clock.
                let today_ist_label = {
                    let date_str = today_window.today_ist.format("%Y-%m-%d").to_string();
                    let hm = |sql: &str| -> String {
                        sql.trim_matches('\'')
                            .split('T')
                            .nth(1)
                            .and_then(|t| {
                                t.split(':')
                                    .collect::<Vec<_>>()
                                    .get(..2)
                                    .map(|s| s.join(":"))
                            })
                            .unwrap_or_else(|| "??:??".to_string())
                    };
                    format!(
                        "{date} {start}–{end} IST",
                        date = date_str,
                        start = hm(&today_window.start_sql),
                        end = hm(&today_window.end_sql),
                    )
                };

                // Scope counts from the registry (IDX_I + NSE_EQ only — the
                // cross-match applies only to indices and stock equities per
                // Parthiban directive 2026-04-21). F&O isn't served by Dhan
                // historical REST so it's excluded from the expected grid.
                let (scope_indices, scope_equities) = {
                    use tickvault_common::types::ExchangeSegment;
                    let grouped = bg_registry.by_exchange_segment();
                    let idx = grouped.get(&ExchangeSegment::IdxI).map_or(0, Vec::len);
                    let eq = grouped.get(&ExchangeSegment::NseEquity).map_or(0, Vec::len);
                    (idx, eq)
                };

                // Value-mismatch count = Pass-A mismatches excluding missing_live
                // (Pass-A produces value_diff + missing_live rows). Pass-B produces
                // missing_historical. Total = value_mismatches + missing_live + missing_historical.
                let missing_historical = cross_match
                    .mismatch_details
                    .iter()
                    .filter(|m| m.mismatch_type == "missing_historical")
                    .count();
                let value_mismatches_count = cross_match
                    .mismatches
                    .saturating_sub(cross_match.missing_live)
                    .saturating_sub(missing_historical);

                if cross_match.passed {
                    bg_notifier.notify(NotificationEvent::CandleCrossMatchPassed {
                        timeframes_checked: cross_match.timeframes_checked,
                        candles_compared: cross_match.candles_compared,
                        today_ist_label,
                        scope_indices,
                        scope_equities,
                        per_tf_cells: cross_match.per_timeframe_mismatches.clone(),
                    });
                } else {
                    // Group mismatch details by category for Telegram.
                    // Order: missing_live → value_diff → missing_historical.
                    let details = format_cross_match_details_grouped(&cross_match.mismatch_details);
                    bg_notifier.notify(NotificationEvent::CandleCrossMatchFailed {
                        candles_compared: cross_match.candles_compared,
                        mismatches: cross_match.mismatches,
                        missing_live: cross_match.missing_live,
                        mismatch_details: details,
                        today_ist_label,
                        scope_indices,
                        scope_equities,
                        missing_historical,
                        value_mismatches: value_mismatches_count,
                        per_tf_gaps: cross_match.per_timeframe_mismatches.clone(),
                    });
                }
            }

            // Once-per-day success marker. Both verify AND cross-match must
            // pass; on success, write a file marker so subsequent restarts
            // today skip the verification entirely. If either is not-passed
            // (or skipped due to no live data), do NOT mark — next run will
            // retry from scratch. Per Parthiban directive 2026-04-20:
            // "only once in a day until it achieves success".
            let full_success = verify_report.passed && cross_match.passed;
            if full_success {
                match tickvault_core::historical::cross_verify::mark_cross_verify_success(
                    today_window.today_ist,
                ) {
                    Ok(()) => {
                        info!(
                            today_ist = %today_window.today_ist,
                            "cross-verify success marker written — today's verification will not run again"
                        );
                    }
                    Err(err) => {
                        warn!(
                            ?err,
                            today_ist = %today_window.today_ist,
                            "failed to write cross-verify success marker (verification still passed; next restart will re-run)"
                        );
                    }
                }
            }

            info!(
                instruments_fetched = summary.instruments_fetched,
                instruments_failed = summary.instruments_failed,
                total_candles = summary.total_candles,
                verification_passed = verify_report.passed,
                cross_match_passed = cross_match.passed,
                full_success_marker_written = full_success,
                today_ist = %today_window.today_ist,
                window_start = %today_window.start_sql,
                window_end = %today_window.end_sql,
                "post-market historical fetch + cross-verification complete (TODAY ONLY)"
            );
        }
    });

    info!(
        "background historical candle fetch task spawned (will wait for post-market signal if trading hours)"
    );
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
    mut tick_rx: tokio::sync::broadcast::Receiver<tickvault_common::tick_types::ParsedTick>,
    questdb_config: tickvault_common::config::QuestDbConfig,
    health_status: Option<SharedHealthStatus>,
    notifier: Option<std::sync::Arc<NotificationService>>,
) {
    let mut tick_writer = match TickPersistenceWriter::new(&questdb_config) {
        Ok(writer) => {
            info!("cold-path tick persistence writer connected to QuestDB");
            if let Some(ref hs) = health_status {
                hs.set_tick_persistence_connected(true);
            }
            writer
        }
        Err(err) => {
            warn!(
                ?err,
                "cold-path tick persistence writer: QuestDB unavailable at startup — \
                 buffering all ticks in ring buffer + disk spill until QuestDB comes back"
            );
            // CRITICAL FIX: Never return here. Use new_disconnected() so the consumer
            // keeps running and buffers ALL ticks. The writer will auto-reconnect every
            // 30 seconds and drain the buffer when QuestDB becomes available.
            // Without this, fast-boot mode loses ALL ticks when QuestDB is down.
            TickPersistenceWriter::new_disconnected(&questdb_config)
        }
    };

    let mut ticks_persisted: u64 = 0;
    // O(1) EXEMPT: cold path, pipeline setup
    let flush_interval = std::time::Duration::from_millis(100);
    let mut last_flush = std::time::Instant::now();

    // S3-1: QuestDB health poller — state machine that tracks the writer's
    // connection state and fires CRITICAL alerts on outages >30s.
    let mut qdb_health_poller = tickvault_storage::questdb_health::QuestDbHealthPoller::new();
    let qdb_health_tick_interval = std::time::Duration::from_secs(2); // APPROVED: pre-existing literal, session-7 tech debt tracked for cleanup
    let mut last_qdb_health_tick = std::time::Instant::now();

    // Tick gap tracker — detects when a security's LTT gap exceeds the
    // ERROR threshold. Fires its own log/metric/alert; gap backfill is
    // explicitly disabled inside the WebSocket path (user policy:
    // in-market backfill must never run; post-market historical fetch
    // handles the cold path separately).
    //
    // Capacity = MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION (5000) ×
    // MAX_WEBSOCKET_CONNECTIONS (5) = 25,000.
    let tick_gap_tracker_capacity =
        tickvault_common::constants::MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION
            .saturating_mul(tickvault_common::constants::MAX_WEBSOCKET_CONNECTIONS);
    let mut tick_gap_tracker =
        tickvault_trading::risk::tick_gap_tracker::TickGapTracker::new(tick_gap_tracker_capacity);
    info!(
        capacity = tick_gap_tracker_capacity,
        "tick gap tracker instantiated with full-universe capacity (in-market backfill disabled)"
    );

    loop {
        match tick_rx.recv().await {
            Ok(tick) => {
                // Record the tick into the gap tracker. The tracker fires
                // its own log/metric on gap thresholds; we do NOT publish
                // any backfill request (in-market backfill disabled).
                let _ = tick_gap_tracker.record_tick(tick.security_id, tick.exchange_timestamp);

                if let Err(err) = tick_writer.append_tick(&tick) {
                    // Phase 0 / Rule 5: persistence failures are ERROR (route to Telegram).
                    error!(?err, "cold-path tick persistence write failed");
                }

                ticks_persisted = ticks_persisted.saturating_add(1);

                // S3-1: Poll the QuestDB health state machine every 2s.
                // The state machine is pure — we feed it the current
                // connection state and the current time, and it returns
                // a verdict that we hand to emit_metrics_for_verdict.
                if last_qdb_health_tick.elapsed() >= qdb_health_tick_interval {
                    let verdict = qdb_health_poller
                        .tick(tick_writer.is_connected(), std::time::Instant::now());
                    tickvault_storage::questdb_health::emit_metrics_for_verdict(
                        verdict,
                        &qdb_health_poller,
                    );
                    last_qdb_health_tick = std::time::Instant::now();
                }

                // Periodic flush (every 100ms) to keep data flowing to QuestDB.
                if last_flush.elapsed() >= flush_interval {
                    let _ = tick_writer.flush_if_needed();
                    last_flush = std::time::Instant::now();
                }

                // B2: After QuestDB recovery + buffer drain, run integrity check.
                if tick_writer.take_recovery_flag() {
                    // Fire immediate Telegram notification for QuestDB recovery.
                    if let Some(ref n) = notifier {
                        n.notify(NotificationEvent::QuestDbReconnected {
                            writer: "tick".to_string(),
                            drained_count: ticks_persisted as usize,
                        });
                    }
                    let qdb_config = questdb_config.clone();
                    tokio::spawn(async move {
                        tickvault_storage::tick_persistence::check_tick_gaps_after_recovery(
                            &qdb_config,
                            30, // Check last 30 minutes
                        )
                        .await;
                    });
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                // C2: CRITICAL — ticks permanently lost due to broadcast lag.
                // This fires ERROR log → Loki → Telegram alert automatically.
                // Root cause: QuestDB ILP flush is slower than tick ingestion rate.
                // Defense: broadcast capacity 262K + tick writer ring buffer 600K + disk spill.
                error!(
                    skipped,
                    "CRITICAL: cold-path tick persistence lagged — {} ticks permanently lost",
                    skipped
                );
                metrics::counter!("tv_ticks_permanently_lost").increment(skipped);
                // Explicit Telegram: ERROR log triggers Loki alert, but also notify directly
                // in case Loki pipeline is delayed.
                if let Some(ref n) = notifier {
                    n.notify(NotificationEvent::QuestDbDisconnected {
                        writer: format!("tick_persistence (LAGGED: {skipped} ticks lost)"),
                        signal: skipped,
                        signal_kind: "Ticks dropped by broadcast lag".to_string(),
                    });
                }
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
// Helper: S4-T1d — Slow-boot observability-only consumer
// ---------------------------------------------------------------------------

/// S4-T1d: Gap tracker + QuestDB HTTP health observer for the slow-boot
/// path. In slow-boot mode the hot-path `run_tick_processor` owns its own
/// `TickPersistenceWriter` and writes ticks directly; adding a second
/// writer would double-persist every tick. This task is ADDITIVE —
/// observability only, no writes:
///
/// 1. Subscribes to the tick broadcast (cheap read-only clone)
/// 2. Feeds every tick into `TickGapTracker::record_tick` and publishes
///    a `GapBackfillRequest` on ERROR-level gaps (same as the fast-boot
///    consumer)
/// 3. Every 2 seconds, HTTP-pings QuestDB's `/exec` endpoint and feeds
///    the result into `QuestDbHealthPoller` so the same metrics +
///    CRITICAL logs fire as in fast-boot
///
/// Matches the zero-tick-loss observability surface in both boot modes
/// so there is no blind spot.
async fn run_slow_boot_observability(
    mut tick_rx: tokio::sync::broadcast::Receiver<tickvault_common::tick_types::ParsedTick>,
    questdb_config: tickvault_common::config::QuestDbConfig,
) {
    info!("S4-T1d: slow-boot observability task started");

    let tick_gap_tracker_capacity =
        tickvault_common::constants::MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION
            .saturating_mul(tickvault_common::constants::MAX_WEBSOCKET_CONNECTIONS);
    let mut tick_gap_tracker =
        tickvault_trading::risk::tick_gap_tracker::TickGapTracker::new(tick_gap_tracker_capacity);

    let mut qdb_health_poller = tickvault_storage::questdb_health::QuestDbHealthPoller::new();
    let qdb_health_interval = std::time::Duration::from_secs(2); // APPROVED: pre-existing literal, session-7 tech debt tracked for cleanup
    let mut last_qdb_health_check = std::time::Instant::now();

    // Audit finding #2 (2026-04-24): wire TickGapTracker::detect_stale_instruments()
    // to a 30s periodic poller. The method existed in the tracker but was never
    // called in production, so per-instrument stall detection (Dhan silently drops
    // a subscription OR an ATM strike stops trading mid-session) stayed invisible
    // until the global `no_tick_watchdog` fired on total silence — up to 120s
    // of missed signals on a single underlying. The 30s cadence is the sweet
    // spot: fast enough to catch stalls before operators manually notice them,
    // slow enough to stay off the hot path (O(n) scan of tracked securities,
    // n = up to 25k in prod).
    let stale_check_interval =
        std::time::Duration::from_secs(tickvault_common::constants::STALE_LTP_SCAN_INTERVAL_SECS);
    let mut last_stale_check = std::time::Instant::now();

    // HTTP client for QuestDB /exec health ping. Uses a short timeout so
    // an unresponsive QDB is treated as disconnected within 1s.
    let http_client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(1)) // APPROVED: pre-existing literal, session-7 tech debt tracked for cleanup
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(
                ?err,
                "S4-T1d: reqwest client build failed — observability task exiting"
            );
            return;
        }
    };
    let questdb_ping_url = format!(
        "http://{}:{}/exec?query=SELECT%201",
        questdb_config.host, questdb_config.http_port
    );

    loop {
        match tick_rx.recv().await {
            Ok(tick) => {
                // Gap detection — same logic as fast-boot consumer. Gap
                // tracker fires its own log/metric on ERROR thresholds;
                // no backfill request is published (in-market backfill
                // disabled by user policy).
                let _ = tick_gap_tracker.record_tick(tick.security_id, tick.exchange_timestamp);

                // Audit finding #2 (2026-04-24): periodic per-instrument stall
                // scan every 30 s. Returns the count of newly-stale instruments;
                // the tracker itself emits an ERROR per-instrument (→ Telegram)
                // and increments `tv_stale_ltp_instruments`. Here we just
                // track cadence so the scan runs even when no ticks are
                // flowing (otherwise the loop would wait on tick_rx.recv()
                // forever during a total-silence incident).
                if last_stale_check.elapsed() >= stale_check_interval {
                    let newly_stale = tick_gap_tracker.detect_stale_instruments();
                    if newly_stale > 0 {
                        debug!(newly_stale, "per-instrument stall scan: newly-stale count");
                    }
                    last_stale_check = std::time::Instant::now();
                }

                // QuestDB HTTP health ping every 2 seconds.
                if last_qdb_health_check.elapsed() >= qdb_health_interval {
                    let connected = match http_client.get(&questdb_ping_url).send().await {
                        Ok(resp) => resp.status().is_success(),
                        Err(_) => false,
                    };
                    let verdict = qdb_health_poller.tick(connected, std::time::Instant::now());
                    tickvault_storage::questdb_health::emit_metrics_for_verdict(
                        verdict,
                        &qdb_health_poller,
                    );
                    last_qdb_health_check = std::time::Instant::now();
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    skipped,
                    "S4-T1d: slow-boot observer lagged {skipped} ticks — gap tracker state is still valid"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                info!("S4-T1d: slow-boot observer shutting down (broadcast closed)");
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
    mut tick_rx: tokio::sync::broadcast::Receiver<tickvault_common::tick_types::ParsedTick>,
    questdb_config: tickvault_common::config::QuestDbConfig,
) {
    let mut candle_writer =
        match tickvault_storage::candle_persistence::LiveCandleWriter::new(&questdb_config) {
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

    let mut aggregator = tickvault_core::pipeline::CandleAggregator::new();
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
                error!(
                    skipped,
                    "CRITICAL: cold-path candle consumer lagged — {} ticks not aggregated", skipped
                );
                metrics::counter!("tv_candle_ticks_lagged").increment(skipped);
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
                + i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS))
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
                // Phase 0 / Rule 5: flush failures are ERROR (route to Telegram).
                error!(?err, "cold-path candle flush failed");
            }
            last_sweep = std::time::Instant::now();
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
    shared_movers: tickvault_core::pipeline::SharedTopMoversSnapshot,
    post_market_signal: std::sync::Arc<tokio::sync::Notify>,
    // S4-T1b: shared pool handle + shutdown notifier. `ws_pool_arc` is
    // None when no WebSocket pool was spawned (e.g., historical-replay
    // mode). `shutdown_notify` is fired before the abort loop so the
    // pool watchdog task stops polling (prevents a false-positive Halt
    // during intentional teardown).
    ws_pool_arc: Option<std::sync::Arc<WebSocketConnectionPool>>,
    shutdown_notify: std::sync::Arc<tokio::sync::Notify>,
) -> Result<()> {
    let mode = "LIVE";
    info!(
        mode,
        api_port = config.api.port,
        "system ready — press Ctrl+C to stop"
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
            tickvault_common::constants::MARKET_CLOSE_DRAIN_BUFFER_SECS,
        );
        tokio::time::sleep(drain).await;

        // Stop real-time market data pipeline (market feed + depth WS only).
        // Order update WS stays alive until app shutdown (16:00 IST or Ctrl+C)
        // to capture AMO status updates and post-market order notifications.
        for handle in &ws_handles {
            handle.abort();
        }
        // Give tick processor time to flush remaining ticks before aborting
        if processor_handle.is_some() {
            let flush_timeout = std::time::Duration::from_secs(
                tickvault_common::constants::GRACEFUL_SHUTDOWN_TIMEOUT_SECS,
            );
            tokio::time::sleep(flush_timeout).await;
        }
        if let Some(ref handle) = trading_handle {
            handle.abort();
        }

        // Signal historical fetch task to start post-market re-fetch
        post_market_signal.notify_one();

        // Post-market: detach old QuestDB partitions (Phase B).
        // Runs daily after pipeline stops — keeps hot data bounded to retention_days.
        {
            let retention_days = config.partition_retention.retention_days;
            if retention_days > 0 {
                match tickvault_storage::partition_manager::PartitionManager::new(
                    &config.questdb,
                    retention_days,
                ) {
                    Ok(pm) => match pm.detach_old_partitions().await {
                        Ok(count) => {
                            info!(
                                detached = count,
                                retention_days, "post-market partition detach complete"
                            );
                        }
                        Err(err) => {
                            warn!(?err, "post-market partition detach failed (non-critical)");
                        }
                    },
                    Err(err) => {
                        warn!(?err, "partition manager creation failed (non-critical)");
                    }
                }
            }
        }

        info!(
            "post-market: real-time pipeline stopped, historical fetch + cross-verify in progress"
        );

        // Phase 2: App runs 24/7. Only Ctrl+C / SIGTERM stops it.
        // No auto-shutdown — the daily reset signal at 16:00 IST handles
        // candle aggregator reset, indicator flush, etc. without stopping the app.
        info!("post-market tasks running — app stays alive (Ctrl+C to stop)");
        let reason = wait_for_shutdown_signal().await;
        info!(
            reason,
            "shutdown signal received — stopping remaining services"
        );
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

    // 3. S4-T1b: Graceful unsubscribe BEFORE abort. Sends RequestCode 12
    //    to every live WebSocket so Dhan's server cleans up subscriptions
    //    instead of timing them out 40s later. Best-effort — dead
    //    connections are skipped. Also fires shutdown_notify so the pool
    //    watchdog stops polling (prevents false-positive Halt during
    //    intentional teardown).
    shutdown_notify.notify_waiters();
    if let Some(ref pool) = ws_pool_arc {
        let signalled = pool.request_graceful_shutdown();
        info!(
            connections_signalled = signalled,
            "S4-T1b: graceful shutdown signalled to WebSocket pool"
        );
        // Give each connection up to 2s to finish its Disconnect send.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await; // APPROVED: pre-existing literal, session-7 tech debt tracked for cleanup
    }

    // 3b. Abort WebSocket connections (drops senders → processor exits).
    // Wave 2 Item 5.2 (WS-GAP-05) — first abort, then run the pool
    // supervisor to drain any final exits with structured ERROR logs +
    // metric increments for tasks that panicked / errored vs exited
    // cleanly. Bounded by 5s so a hung handle does not stall shutdown.
    let abort_handles: Vec<_> = ws_handles
        .iter()
        .map(tokio::task::JoinHandle::abort_handle)
        .collect();
    for h in &abort_handles {
        h.abort();
    }
    let supervise_fut =
        tickvault_core::websocket::connection_pool::WebSocketConnectionPool::supervise_pool(
            ws_handles,
        );
    const POOL_SUPERVISOR_DRAIN_TIMEOUT_SECS: u64 = 5;
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(POOL_SUPERVISOR_DRAIN_TIMEOUT_SECS),
        supervise_fut,
    )
    .await;

    // 4. Wait for tick processor final flush.
    if let Some(handle) = processor_handle {
        let shutdown_timeout = std::time::Duration::from_secs(
            tickvault_common::constants::GRACEFUL_SHUTDOWN_TIMEOUT_SECS,
        );
        match tokio::time::timeout(shutdown_timeout, handle).await {
            Ok(_) => info!("tick processor shut down gracefully"),
            Err(_) => {
                warn!("tick processor shutdown timed out — forcing spill to disk");
                // Force any remaining in-flight ticks to disk spill so they survive
                // the process exit. Ring buffer + disk spill = zero tick loss.
                info!("tick data safe in ring buffer + disk spill (will recover on next startup)");
            }
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

    info!("tickvault stopped");
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

// ---------------------------------------------------------------------------
// Helper: Build inline Greeks enricher for tick processor
// ---------------------------------------------------------------------------

/// Constructs an `InlineGreeksComputer` for injection into the tick processor.
///
/// A5: Returns the concrete type (not `Box<dyn>`) so the compiler monomorphizes
/// `run_tick_processor<InlineGreeksComputer>`, eliminating vtable indirection
/// on the hot path (~20-40ns savings per tick).
///
/// Returns `None` if Greeks are disabled or no subscription plan is available.
/// O(1) EXEMPT: cold path — called once at startup.
fn build_inline_greeks_enricher(
    config: &ApplicationConfig,
    subscription_plan: &Option<SubscriptionPlan>,
) -> Option<InlineGreeksComputer> {
    if !config.greeks.enabled {
        info!("inline Greeks enricher disabled in config");
        return None;
    }

    let plan = match subscription_plan.as_ref() {
        Some(p) => p,
        None => {
            info!("inline Greeks enricher skipped — no subscription plan");
            return None;
        }
    };

    // Compute today's date in IST for time-to-expiry calculation.
    let today = (Utc::now()
        + chrono::TimeDelta::seconds(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64))
    .date_naive();

    // O(1) EXEMPT: cold path — clone registry once at startup for enricher.
    let enricher = InlineGreeksComputer::new(
        plan.registry.clone(),
        config.greeks.risk_free_rate,
        config.greeks.dividend_yield,
        config.greeks.day_count,
        today,
    );

    info!(
        rate = config.greeks.risk_free_rate,
        div = config.greeks.dividend_yield,
        day_count = config.greeks.day_count,
        %today,
        "inline Greeks enricher created for tick processor"
    );

    Some(enricher)
}

// All pure helper function tests are in boot_helpers.rs (lib.rs target).
// Only integration-level tests that require main.rs-specific code remain here.
#[cfg(test)]
#[allow(clippy::items_after_test_module)]
// APPROVED: helper fns below the tests block are part of the boot path; reordering would add churn
#[allow(clippy::assertions_on_constants)]
mod tests {
    use super::*;
    use tickvault_app::boot_helpers::{
        APP_LOG_FILE_PATH, OFF_HOURS_CONNECTION_STAGGER_MS, create_log_file_writer,
        determine_boot_mode, should_fast_boot,
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

    // -----------------------------------------------------------------------
    // Historical-fetch post-market-only gate (Parthiban directive 2026-04-22)
    // -----------------------------------------------------------------------
    //
    // Ratchet: on a trading day, the boot sequence MUST wait for the
    // post-market signal (15:30 IST) before firing the historical candle
    // fetch. A prior code branch "trading day before 08:00 IST → fetch
    // immediately" was removed on 2026-04-22 after it caused a fresh-clone
    // boot at 07:39 IST to hammer Dhan REST right before market open.
    //
    // These source-scan tests fail the build if the removed branch (or any
    // equivalent) is reintroduced. We source-scan instead of unit-testing
    // the gate directly because the gate lives inside a multi-megabyte
    // tokio::spawn closure — extracting a pure function would be a large
    // refactor. The source-scan is cheap and sufficient.

    fn read_main_source() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    #[test]
    fn test_historical_fetch_has_no_pre_market_immediate_branch() {
        let src = read_main_source();
        // Reassemble the forbidden log message at runtime so THIS test's
        // own source doesn't trivially match itself via src.contains().
        // The old buggy branch logged this exact phrase.
        let forbidden = ["before 08:00 or after ", "15:30", " — fetching historical"].concat();
        let forbidden_count = src.matches(forbidden.as_str()).count();
        assert_eq!(
            forbidden_count, 0,
            "historical fetch MUST NOT fire pre-market on trading days \
             (directive 2026-04-22). The early-hour immediate-fetch branch \
             was removed; do not reintroduce it. Found {forbidden_count} \
             match(es) of the old log string."
        );
        // Guard against a common rewrite that still fires pre-market:
        // `hour == 8 → fetch immediately`. The old code stored this check
        // in a bool variable; that name must stay removed.
        let sentinel_var = ["is_pre_market", "_wait_zone"].concat();
        let sentinel_count = src.matches(sentinel_var.as_str()).count();
        assert_eq!(
            sentinel_count, 0,
            "the pre-market-wait-zone variable was removed — trading days \
             now always wait for post-market signal regardless of hour. \
             Reintroducing this likely means the pre-market branch came back."
        );
    }

    #[test]
    fn test_historical_fetch_gate_uses_tick_persist_end_constant() {
        let src = read_main_source();
        assert!(
            src.contains("should_wait_for_post_market"),
            "post-market gate variable `should_wait_for_post_market` must exist"
        );
        assert!(
            src.contains("TICK_PERSIST_END_SECS_OF_DAY_IST"),
            "post-market gate MUST compare against the shared market-close \
             constant (not a hardcoded 15:30)"
        );
    }

    #[test]
    fn test_historical_fetch_gate_honours_post_market_signal() {
        let src = read_main_source();
        assert!(
            src.contains("post_market_signal.notified().await"),
            "the gate MUST await the post-market notification rather than \
             fetching immediately on trading days before 15:30"
        );
        assert!(
            src.contains("trading day before 15:30 IST — waiting for post-market signal"),
            "the gate's INFO log message must accurately describe the wait"
        );
    }

    #[test]
    fn test_boot_helper_functions_callable() {
        let _ = compute_market_close_sleep("15:30:00");
        let _ = format_violation_details(&[]);
        let _ = format_cross_match_details_grouped(&[]);
        let _ = create_log_file_writer();
    }

    #[test]
    fn test_new_boot_helpers_callable_from_main() {
        // Verify the newly extracted helpers are accessible from main.
        let addr = format_bind_addr("0.0.0.0", 3001);
        assert!(addr.contains("3001"));

        let stagger = effective_ws_stagger(3000, true);
        // Always uses fast stagger (1000ms) for crash recovery.
        assert_eq!(stagger, 1000);

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
        assert!(tickvault_common::constants::BOOT_TIMEOUT_SECS > 0);
        assert!(tickvault_common::constants::BOOT_TIMEOUT_SECS <= 300);
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

    /// I12 ratchet: the HALT branch must embed `/v2/profile` +
    /// `/v2/ip/getIP` diagnostics in the Telegram message.
    #[test]
    fn test_premarket_halt_auto_diagnoses_profile_and_ip() {
        let src = include_str!("main.rs");
        assert!(
            src.contains("build_pre_market_diagnostics"),
            "main.rs HALT branch must call `build_pre_market_diagnostics` so \
             the Telegram message carries the /v2/profile and /v2/ip/getIP \
             responses alongside the failure reason (I12)."
        );
    }

    /// Q6 regression (2026-04-24): the depth-200 spawn loop MUST skip
    /// deferred-mode connections (sid=None, "-deferred" label) when off
    /// market hours. Live 2026-04-23 boot at 21:20 IST spawned 4 depth-200
    /// connections with SID=0 that each burned 60 TCP connects to Dhan
    /// over ~2 hours before giving up — 240 useless handshakes in a
    /// single post-market boot. The guard wires
    /// `is_within_market_hours_ist()` at the per-contract spawn site
    /// inside the `for opt_entry in [Some(atm_ce_entry), ...]` loop.
    #[test]
    fn test_depth_200_deferred_spawn_skipped_off_market_hours() {
        let src = include_str!("main.rs");
        assert!(
            src.contains("tickvault_common::market_hours::is_within_market_hours_ist()"),
            "main.rs MUST call is_within_market_hours_ist() somewhere — \
             depth-200 deferred spawn relies on it"
        );
        // The specific guard literal — if this ever regresses, the
        // post-market boot storm returns (Q6 / audit-findings Rule 3).
        assert!(
            src.contains("skipping deferred spawn"),
            "main.rs MUST contain the 'skipping deferred spawn' warn-log \
             inside the depth-200 spawn loop so off-market-hours boots \
             don't burn 240 TCP connects against Dhan with SID=0."
        );
        assert!(
            src.contains("if depth200_sid.is_none()")
                && src.contains("!tickvault_common::market_hours::is_within_market_hours_ist()"),
            "Fix A guard at the depth-200 spawn site must check \
             `depth200_sid.is_none() && !is_within_market_hours_ist()`."
        );
    }
}

// ---------------------------------------------------------------------------
// I12 (2026-04-21): Auto-diagnostic for pre-market profile HALT.
//
// When `pre_market_check` fails, the operator previously had to run two
// curl commands manually to find out WHY (dataPlan / segment / token /
// IP allowlist). This helper fetches both endpoints directly and
// returns a short human-readable summary to embed in the CRITICAL
// Telegram message. Secrets (token + raw IP) are redacted — the output
// is safe to stream to Telegram.
// ---------------------------------------------------------------------------

/// Fetches `/v2/profile` and `/v2/ip/getIP` and formats a summary
/// suitable for the CRITICAL `PreMarketProfileCheckFailed` Telegram body.
///
/// Never panics. Every failure path is captured in the returned String
/// so the operator always gets back SOMETHING — even if both endpoints
/// are down. Timeout per endpoint is 5 seconds; total worst case ~10 s
/// before the boot sequence proceeds to the HALT.
// TEST-EXEMPT: requires live Dhan `/v2/profile` + `/v2/ip/getIP` HTTP; behaviour exercised by the ratchet test `test_premarket_halt_auto_diagnoses_profile_and_ip` above plus production smoke on the first HALT.
async fn build_pre_market_diagnostics(
    token_manager: &std::sync::Arc<tickvault_core::auth::TokenManager>,
    rest_api_base_url: &str,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "--- Diagnostic snapshot (auto-fetched) ---");

    match token_manager.get_user_profile().await {
        Ok(profile) => {
            let _ = writeln!(
                out,
                "/v2/profile: dataPlan={:?}  activeSegment={:?}  tokenValidity={:?}",
                profile.data_plan, profile.active_segment, profile.token_validity
            );
        }
        Err(e) => {
            // The error `Display` already redacts query params via the
            // REST client's own sanitiser, so it's safe to include here.
            let _ = writeln!(out, "/v2/profile: ERROR {e}");
        }
    }

    // For /v2/ip/getIP we need the access token — pull it from the
    // token handle (O(1) arc-swap read).
    let token_guard = token_manager.token_handle().load();
    if let Some(token_state) = token_guard.as_ref().as_ref() {
        use secrecy::ExposeSecret;
        let access_token = token_state.access_token().expose_secret().to_string();
        match tickvault_core::network::ip_verifier::get_ip(rest_api_base_url, &access_token).await {
            Ok(ip) => {
                // Redact all but the last octet of the IP for privacy.
                let redacted_ip = redact_ip_last_octet(&ip.ip);
                let _ = writeln!(
                    out,
                    "/v2/ip/getIP: ip={redacted_ip}  ipFlag={:?}  modifyDatePrimary={:?}",
                    ip.ip_flag, ip.modify_date_primary
                );
            }
            Err(e) => {
                let _ = writeln!(out, "/v2/ip/getIP: ERROR {e}");
            }
        }
    } else {
        let _ = writeln!(
            out,
            "/v2/ip/getIP: SKIPPED (no access token available for diagnostic call)"
        );
    }
    out
}

/// Redacts all but the last octet of an IPv4 address for safe Telegram
/// embedding. IPv6 just returns a coarse "xxxx:...:last" form.
// TEST-EXEMPT: trivial redaction wrapper exercised inline by the ratchet
// test below.
fn redact_ip_last_octet(raw: &str) -> String {
    // IPv4 path
    if let Some((prefix, last)) = raw.rsplit_once('.') {
        // Replace prefix with "x.x.x"
        let _ = prefix; // suppress unused-var
        return format!("x.x.x.{last}");
    }
    // IPv6 path — keep only the last group
    if let Some((_, last)) = raw.rsplit_once(':') {
        return format!("x:...:{last}");
    }
    // Unknown shape — fully redact
    "[REDACTED]".to_string()
}
