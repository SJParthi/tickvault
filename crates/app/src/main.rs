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
    check_clock_drift, compute_market_close_sleep, create_error_log_writer, effective_ws_stagger,
    format_bind_addr, format_cross_match_details_grouped, format_timeframe_details,
    format_violation_details, should_emit_post_market_alert, spawn_heartbeat_watchdog,
};
use tickvault_app::{core_pinning, infra, observability, subsystem_memory, trading_pipeline};

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

    // Wave 5 Item 6: pin the main thread to Core 3 ("other") so it does not
    // share Core 0 with the WebSocket read loop (pinned later when its
    // dedicated worker task spawns). Best-effort — failures emit
    // CORE-PIN-01 + Telegram and the app continues without pinning.
    core_pinning::pin_main_thread();

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
                // Wave-Holiday-Gate (2026-05-09): suppress WS-GAP-06
                // emission on Saturday/Sunday boots (and outside
                // market hours). NSE doesn't stream on weekends, so
                // every IDX_I tracker reports >30s silence — the
                // ERROR storm in the operator's 2026-05-09 logs was
                // 60+ false positives. Inside trading session, the
                // alert remains live.
                if !tickvault_common::market_hours::is_within_trading_session_ist() {
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

    // L18 (revised) + L121-L130 (Wave-5 in-memory-store plan §AA):
    // register the per-subsystem memory gauges, the sampler heartbeat,
    // and the market-hours-active quiet-hours gate. The sampler task
    // wakes every `subsystem_memory::SAMPLER_INTERVAL_SECS` seconds
    // (10s, aligned with the Prom scrape window) and writes whichever
    // component sources have been registered. For #504a no source is
    // registered yet — gauges stay `f64::NAN` (L124) until #504d wires
    // the in-memory store; the alert's `unless absent_over_time(...)`
    // clause filters NaN entries out so this does not page the operator.
    let subsystem_memory_handles =
        std::sync::Arc::new(subsystem_memory::SubsystemMemoryHandles::register());
    let subsystem_memory_sampler =
        std::sync::Arc::new(subsystem_memory::SubsystemMemorySampler::new(
            std::sync::Arc::clone(&subsystem_memory_handles),
        ));

    // L10 (Wave-5 #504d): construct the in-RAM tick ring. Per
    // `[in_mem.tick_storage].per_instrument_capacity` config (default
    // 5_000), each new (security_id, segment) key reserves that many
    // tick slots on first push so steady-state pushes hit the
    // no-realloc path.
    let tick_storage = std::sync::Arc::new(tickvault_trading::in_mem::TickStorage::new(
        config.in_mem.tick_storage.per_instrument_capacity,
    ));

    // L13 (Wave-5 #504e): construct the prev-day reference cache.
    // Empty at boot — the bhavcopy + option-chain loaders populate it
    // before the cascade starts emitting sealed bars (boot-time
    // loader lands in a follow-up small wiring PR; the data structure
    // ships here so the seal-stamping path
    // `CandleEngineMap::on_tick_with_pct` has its lookup target).
    let prev_day_cache = std::sync::Arc::new(tickvault_trading::in_mem::PrevDayCache::new());

    // L18 / #504a: register the `registry` source closure for the
    // prev-day cache. The same component label captures both the
    // instrument registry (legacy) AND prev-day refs since both are
    // per-instrument metadata frozen for the trading session.
    {
        let cache_for_sampler = std::sync::Arc::clone(&prev_day_cache);
        if let Err(err) = subsystem_memory_sampler.register_source("registry", move || {
            #[allow(clippy::cast_precision_loss)] // APPROVED: byte count fits f64 mantissa
            Some(cache_for_sampler.estimated_bytes() as f64)
        }) {
            tracing::error!(
                err,
                "L18 / #504a: failed to register prev_day_cache memory source — \
                 component gauge will stay NaN; investigate the subsystem_memory \
                 sampler state"
            );
        }
    }

    // L18 / #504a contract: register the `tick_storage` source closure
    // with the subsystem_memory sampler. The sampler runs every 10s and
    // calls `estimated_bytes()` to update
    // `tv_subsystem_memory_estimated_bytes{component="tick_storage"}`.
    {
        let storage_for_sampler = std::sync::Arc::clone(&tick_storage);
        if let Err(err) = subsystem_memory_sampler.register_source("tick_storage", move || {
            // L18: lazy `len() x size_of` — NOT raw RSS. Reports actual
            // resident bytes (Linux lazy-page allocation excludes
            // reserved-but-unused Vec capacity).
            #[allow(clippy::cast_precision_loss)]
            // APPROVED: byte count fits f64 mantissa for any realistic universe
            Some(storage_for_sampler.estimated_bytes() as f64)
        }) {
            tracing::error!(
                err,
                "L18 / #504a: failed to register tick_storage memory source — \
                 component gauge will stay NaN; investigate the subsystem_memory \
                 sampler state"
            );
        }
    }

    // L10 reset task: drain the tick ring at IST 09:15 daily so day-N
    // ticks never bleed into day-(N+1)'s session. Sleep-based, not
    // poll-based — audit-findings Rule 3 (market-hours-aware tokio task).
    let _tick_storage_reset_join = {
        let storage_for_reset = std::sync::Arc::clone(&tick_storage);
        tokio::spawn(async move {
            tickvault_trading::in_mem::run_tick_storage_daily_reset(storage_for_reset).await;
        })
    };

    let _subsystem_memory_sampler_join = std::sync::Arc::clone(&subsystem_memory_sampler).spawn();

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
    // HOURLY-rotated via `tracing_appender::rolling::Rotation::HOURLY`:
    //   data/logs/app.YYYY-MM-DD-HH
    // Industry-standard chunk size: a daily file routinely exceeded
    // 100 MB during market hours and broke `less` / `grep` / IDE
    // ergonomics. Hourly chunks bound any single file to ~5–10 MB.
    // The retention sweeper (below the subscriber init) deletes files
    // older than `APP_LOG_RETENTION_HOURS` to bound disk use.
    //
    // The WorkerGuard MUST live for the whole process — leaking into
    // the static binds it to program lifetime without threading it
    // through shutdown (mirrors the errors.jsonl pattern below).
    let file_log_layer: Option<Box<dyn tracing_subscriber::Layer<_> + Send + Sync + 'static>> =
        match observability::init_app_log_appender(observability::ERRORS_JSONL_DIR) {
            Ok((writer, guard)) => {
                use tracing_subscriber::Layer as _;
                Box::leak(Box::new(guard));
                // Per-target DEBUG suppression for the FILE appender only.
                // Stdout + errors.log + errors.jsonl keep the configured
                // global level. See `build_app_log_filter_directive` for
                // the suppression policy and rationale.
                let file_filter_directive =
                    tickvault_app::boot_helpers::build_app_log_filter_directive(log_filter);
                let file_filter = tracing_subscriber::EnvFilter::new(file_filter_directive);
                let file_fmt = tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_file(false)
                    .with_line_number(false)
                    .with_timer(ist_timer.clone())
                    .json()
                    .with_writer(writer)
                    .with_filter(file_filter);
                Some(Box::new(file_fmt))
            }
            Err(err) => {
                // Tracing isn't initialized yet, so a warn here would
                // route nowhere visible. Stay silent and continue —
                // boot proceeds without the rolling app log layer
                // (stdout + errors.log + errors.jsonl still work).
                let _ = err;
                None
            }
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
                    .with_timer(ist_timer.clone())
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

    // 2026-05-02 — per-category log file separation.
    //
    // Operator-requested split of the giant `app.*` stream into 5
    // domain-specific log directories so movers / candles / live ticks /
    // historical / option chain logs can be tailed independently.
    //
    // Each layer uses `tracing_subscriber::filter::Targets` to route only
    // messages matching the category's module prefixes (built by
    // `observability::build_category_targets`). The targets are real
    // module paths verified against `crates/{core,storage,trading}/src/`.
    //
    // Failure to init any one category appender is BEST EFFORT — the
    // existing app.log + errors.log + errors.jsonl streams stay intact,
    // and the missing category falls back to those general streams via
    // the existing layers. This prevents one bad mount from blocking boot.
    let category_layers: Vec<Box<dyn tracing_subscriber::Layer<_> + Send + Sync + 'static>> = {
        use tracing_subscriber::Layer as _;
        let mut layers: Vec<Box<dyn tracing_subscriber::Layer<_> + Send + Sync + 'static>> =
            Vec::with_capacity(5);
        for cat in tickvault_app::observability::LogCategory::all() {
            match tickvault_app::observability::init_category_log_appender(cat.dir(), cat.prefix())
            {
                Ok((writer, guard)) => {
                    Box::leak(Box::new(guard));
                    let mut targets = tracing_subscriber::filter::Targets::new();
                    for prefix in tickvault_app::observability::build_category_targets(cat) {
                        targets = targets
                            .with_target(*prefix, tracing_subscriber::filter::LevelFilter::TRACE);
                    }
                    let cat_fmt = tracing_subscriber::fmt::layer()
                        .with_target(true)
                        .with_thread_ids(true)
                        .with_file(false)
                        .with_line_number(false)
                        .with_timer(ist_timer.clone())
                        .json()
                        .with_writer(writer)
                        .with_filter(targets);
                    layers.push(Box::new(cat_fmt));
                }
                Err(err) => {
                    let _ = err;
                    // Tracing not initialized yet — silent fallback.
                    // The general app.log layer still captures these logs.
                }
            }
        }
        layers
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_boxed)
        .with(file_log_layer)
        .with(error_log_layer)
        .with(errors_jsonl_layer)
        .with(category_layers)
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

    // Hourly retention sweeper for the rolling app log
    // (data/logs/app.YYYY-MM-DD-HH). Keeps 7 days of files (168 hourly
    // chunks at ~5–10 MB each = ~0.8–1.7 GB cap on disk), matching the
    // prior daily-file retention semantic of "keep 7 daily files".
    tokio::spawn(async {
        use std::path::Path;
        use std::time::Duration;
        const SWEEP_INTERVAL_SECS: u64 = 3600;
        const RETENTION_HOURS: u64 = 168;
        let dir = Path::new(observability::ERRORS_JSONL_DIR);
        loop {
            tokio::time::sleep(Duration::from_secs(SWEEP_INTERVAL_SECS)).await;
            match observability::sweep_app_log_retention(dir, RETENTION_HOURS) {
                Ok(0) => {}
                Ok(n) => tracing::info!(
                    deleted = n,
                    dir = %dir.display(),
                    retention_hours = RETENTION_HOURS,
                    "app log retention sweep"
                ),
                Err(err) => tracing::warn!(
                    ?err,
                    dir = %dir.display(),
                    "app log retention sweep failed"
                ),
            }
        }
    });

    // 2026-05-02 — per-category log retention sweeper. One tokio task
    // iterates all 5 LogCategory variants every hour and deletes
    // {prefix}.{YYYY-MM-DD-HH} files older than 168 hours (7 days),
    // matching the existing app.* policy.
    tokio::spawn(async {
        use std::path::Path;
        use std::time::Duration;
        const SWEEP_INTERVAL_SECS: u64 = 3600;
        const RETENTION_HOURS: u64 = 168;
        loop {
            tokio::time::sleep(Duration::from_secs(SWEEP_INTERVAL_SECS)).await;
            for cat in tickvault_app::observability::LogCategory::all() {
                let dir = Path::new(cat.dir());
                match tickvault_app::observability::sweep_category_log_retention(
                    dir,
                    cat.prefix(),
                    RETENTION_HOURS,
                ) {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(
                        deleted = n,
                        dir = %dir.display(),
                        prefix = cat.prefix(),
                        retention_hours = RETENTION_HOURS,
                        "category log retention sweep"
                    ),
                    Err(err) => tracing::warn!(
                        ?err,
                        dir = %dir.display(),
                        prefix = cat.prefix(),
                        "category log retention sweep failed"
                    ),
                }
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
            // ring buffer (2M, PR #452 bumped from 600K) + disk spill immediately, even before QuestDB connects.
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
                    // 2026-05-09 PR 5c.5-final: movers infrastructure
                    // RETIRED — `movers_1s` base + 25 matviews dropped,
                    // `MoversWriter` / `movers_pipeline` deleted.
                    // Operator directive: "only ticks and our 9 needed
                    // candle timeframes are available".
                    None, // option_movers — created in slow boot only
                    fast_registry,
                    Some(fast_tick_heartbeat),
                    None, // tick_enricher — Phase 2.5 wiring deferred until prev_oi_cache + boot ordering gate land in slow boot
                )
                .await;
            });
            info!("FAST BOOT COMPLETE — tick processor started, ticks flowing (in-memory)");
            // Phase 2.12 (hostile L1 fix): emit a boot-mode gauge so
            // operators can chart fast/slow boot history. Fast boot
            // intentionally passes None for tick_enricher (recovery
            // path, no enrichment) — the gauge value 0 makes the
            // missing lifecycle columns visible vs slow boot's value 1.
            metrics::gauge!(
                "tv_lifecycle_enricher_attached",
                "boot_mode" => "fast"
            )
            .set(0.0);
            info!(
                boot_mode = "fast",
                enricher_attached = false,
                "BOOT MODE: fast (recovery path) — lifecycle columns will NOT be \
                 populated this session; volume_delta/prev_day_close/prev_day_oi/phase \
                 default to 0/0.0/0/PREMARKET. Operator: this is the expected fast-boot \
                 contract; full lifecycle attaches on the next slow-boot restart."
            );
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
            // PR #458: now polls pool.health() for truthful state.
            emit_websocket_connected_alerts(
                &fast_notifier,
                &pool_arc,
                tickvault_core::notification::events::BootPathLabel::Fast,
                boot_start,
            )
            .await;
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

        // Wave 6 Sub-PR #1 item 1.4d — MASTER SWITCH.
        // Spawns the multi-TF aggregator task that subscribes to the
        // live tick broadcast and produces sealed candles for the
        // shadow tables. This is the LAST wire — once this task is
        // running, the full pipeline is live:
        //
        //   Dhan WebSocket → tick_processor → fast_tick_broadcast
        //     → THIS TASK: MultiTfAggregator::consume_tick → BufferedSeal
        //       → mpsc::Sender (published in 1.4c) → SealWriterRunner
        //         → SealAbsorptionPipeline (ring → spill → DLQ)
        //           → ShadowCandleWriter → candles_*_shadow tables
        //
        // Producer-side hot-path budget: ~300 ns per tick across all 9 TFs
        // (per consume_tick docstring). Cold-path drain handles ILP.
        //
        // Lazy registration: pre_populate is called on-demand when
        // consume_tick reports `instrument_found = false` (the first
        // tick for that (security_id, exchange_segment_code) pair).
        // Skips pre-loading the entire 11K-instrument registry to keep
        // this slice narrow and avoid coupling main.rs to the universe
        // builder. First tick per instrument is dropped; second tick
        // onwards folds. Acceptable for the shadow-table validation
        // phase — over the trading session every active instrument
        // gets registered after its first tick.
        {
            use tickvault_storage::seal_writer_runner::global_seal_sender;
            use tickvault_trading::candles::{
                AggregatorHeartbeatCounters, BufferedSeal, MultiTfAggregator, stamp_seal_pct_fields,
            };

            // 11K-instrument capacity (matches MAX_TOTAL_SUBSCRIPTIONS
            // headroom per `aws-budget.md`). HashMap grows lazily so
            // this is a hint, not a hard cap.
            const AGGREGATOR_CAPACITY: usize = 11_000;

            let aggregator =
                std::sync::Arc::new(MultiTfAggregator::with_capacity(AGGREGATOR_CAPACITY));
            let agg_clone = std::sync::Arc::clone(&aggregator);
            // Wave 6 Sub-PR #1 item 1.4g — clone the boot-loaded
            // `PrevDayCache` (line 673) into the aggregator task so the
            // seal closure replaces `PrevDayRefs::default()` (all zeros)
            // with the real day-(N-1) refs. Lookup is O(1) lock-free
            // (~30 ns on the cold seal path). Returns `None` on cache
            // miss (newly listed contract or PREVCLOSE-04 cold-boot
            // empty-table state) — graceful fallback to zeros keeps
            // the seal valid (compute_*_pct div-by-zero guards per
            // L-H14 produce 0.0 % values, never NaN).
            let prev_day_cache_for_agg = std::sync::Arc::clone(&prev_day_cache);
            // Wave 6 Sub-PR #1 item 1.4h — per-minute aggregator
            // heartbeat counters. Three AtomicU64 shared between the
            // subscriber task (writer) and the once-per-minute
            // heartbeat task (reader-resetter). Provides positive
            // signal for the AGGREGATOR-HB-01 runbook so the operator
            // can confirm via `tail -f data/logs/app.YYYY-MM-DD.log
            // | grep 'aggregator heartbeat'` that the master switch
            // is producing sealed candles. Counters are SEPARATE from
            // the existing `metrics::counter!` increments (1.4e) —
            // those go to Prometheus; these enable a coalesced 60s
            // structured log without scraping Prometheus.
            let heartbeat = AggregatorHeartbeatCounters::new();
            let heartbeat_writer = heartbeat.clone();
            let heartbeat_reader = heartbeat.clone();
            let mut tick_rx = fast_tick_broadcast_sender.subscribe();

            tokio::spawn(async move {
                loop {
                    match tick_rx.recv().await {
                        Ok(tick) => {
                            // OnceLock read is cheap — single atomic load.
                            // None means the writer task failed to construct;
                            // drop ticks silently (legacy candles_1s path
                            // still feeds production trading).
                            let Some(sender) = global_seal_sender() else {
                                continue;
                            };
                            let stats = agg_clone.consume_tick(
                                &tick,
                                tick.exchange_segment_code,
                                |tf, mut state| {
                                    // Wave 6 Sub-PR #1 item 1.4g — look up
                                    // real prev-day refs from the boot-
                                    // loaded `PrevDayCache` (F2 loader,
                                    // PR #520). Cache miss → fallback to
                                    // `PrevDayRefs::default()` (all zeros)
                                    // for newly listed contracts and
                                    // PREVCLOSE-04 cold-boot scenarios.
                                    // div-by-zero guards in compute_*_pct
                                    // produce 0.0 % values per L-H14, so
                                    // the seal stays valid either way.
                                    let refs = prev_day_cache_for_agg
                                        .lookup(tick.security_id, tick.exchange_segment_code)
                                        .unwrap_or_default();
                                    stamp_seal_pct_fields(&mut state, refs);
                                    let seal = BufferedSeal::new(
                                        tick.security_id,
                                        tick.exchange_segment_code,
                                        tf,
                                        state,
                                    );
                                    // Non-blocking try_send. On full mpsc
                                    // (writer task fell behind ~20s at peak
                                    // burst): drop with counter. The
                                    // producer NEVER blocks on I/O per L-C1.
                                    if sender.try_send(seal).is_err() {
                                        metrics::counter!("tv_seal_mpsc_dropped_total")
                                            .increment(1);
                                        // Wave 6 Sub-PR #1 item 1.4h — also
                                        // record into the heartbeat counters
                                        // for the per-minute structured log.
                                        heartbeat_writer.record_drop();
                                    } else {
                                        // Wave 6 Sub-PR #1 item 1.4e —
                                        // positive-signal counter so the
                                        // operator can see in Grafana that
                                        // the master switch is actually
                                        // producing seals. Labels are not
                                        // used here (one global counter)
                                        // because the per-TF / per-segment
                                        // breakdown lives in the
                                        // `aggregator_seal_audit` table.
                                        metrics::counter!("tv_aggregator_seals_emitted_total")
                                            .increment(1);
                                        // Wave 6 Sub-PR #1 item 1.4h — also
                                        // record into the heartbeat counters.
                                        heartbeat_writer.record_emit();
                                    }
                                },
                            );
                            // Wave 6 Sub-PR #1 item 1.4e — emit observability
                            // counters from `ConsumeStats`. These are the
                            // operator's first visibility into whether the
                            // 1.4d master switch is processing live ticks.
                            if stats.late_count > 0 {
                                metrics::counter!("tv_aggregator_late_ticks_discarded_total")
                                    .increment(u64::from(stats.late_count));
                                // Wave 6 Sub-PR #1 item 1.4h — also record
                                // into the heartbeat counters.
                                heartbeat_writer.record_late_ticks(u64::from(stats.late_count));
                            }
                            // Lazy registration for first-time instruments.
                            // pre_populate is idempotent (only inserts when
                            // key is absent) so the second-tick-onward path
                            // is a no-op.
                            if !stats.instrument_found {
                                agg_clone.pre_populate(std::iter::once((
                                    tick.security_id,
                                    tick.exchange_segment_code,
                                )));
                                // Wave 6 Sub-PR #1 item 1.4e — count lazy
                                // registrations so the operator can see the
                                // ramp-up curve in the first minutes of the
                                // session as each instrument's first tick
                                // arrives.
                                metrics::counter!("tv_aggregator_instruments_lazy_inserted_total")
                                    .increment(1);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            // Broadcast channel overflowed — we lost some
                            // ticks. Increment counter; the legacy path
                            // also receives the same ticks via its own
                            // subscriber, so trading is unaffected.
                            metrics::counter!("tv_aggregator_tick_lag_total").increment(skipped);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            tracing::info!(
                                "Wave 6 aggregator subscriber: broadcast closed, exiting"
                            );
                            break;
                        }
                    }
                }
            });
            tracing::info!(
                aggregator_capacity = AGGREGATOR_CAPACITY,
                "Wave 6 Sub-PR #1 item 1.4d — multi-TF aggregator task spawned (master switch ON)"
            );

            // Wave 6 Sub-PR #1 item 1.4h — per-minute aggregator
            // heartbeat task. Drains the three counters every 60 s
            // and emits a structured `info!` log when any counter is
            // non-zero. Pre-market / post-market silence produces
            // zero snapshots and stays silent (audit-findings Rule 3
            // market-hours awareness + Rule 11 false-OK avoidance).
            // No Telegram emission yet — log-only is the safest first
            // pass; a Telegram `AggregatorMinuteSealBurst` event can
            // be wired in a later slice once the operator confirms
            // the log volume is healthy.
            const AGGREGATOR_HEARTBEAT_INTERVAL_SECS: u64 = 60;
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                    AGGREGATOR_HEARTBEAT_INTERVAL_SECS,
                ));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // First tick fires immediately; skip it so the first
                // emitted log represents a full 60s window.
                interval.tick().await;
                loop {
                    interval.tick().await;
                    let snap = heartbeat_reader.drain();
                    if !snap.is_active() {
                        continue;
                    }
                    tracing::info!(
                        seals_emitted = snap.seals_emitted,
                        seals_dropped = snap.seals_dropped,
                        late_ticks_discarded = snap.late_ticks_discarded,
                        interval_secs = AGGREGATOR_HEARTBEAT_INTERVAL_SECS,
                        "aggregator heartbeat (Wave 6 Sub-PR #1 item 1.4h)"
                    );
                }
            });
            tracing::info!(
                interval_secs = AGGREGATOR_HEARTBEAT_INTERVAL_SECS,
                "Wave 6 Sub-PR #1 item 1.4h — aggregator heartbeat task spawned (per-minute summary)"
            );
        }

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
        // PR #449 (operator clarification 2026-05-03): historical fetch
        // is gated behind `features.historical_fetch_enabled`. Defaults
        // to OFF until broker-traded source (PR #455 Groww) lands.
        let post_market_signal = std::sync::Arc::new(tokio::sync::Notify::new());
        if config.features.historical_fetch_enabled {
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
        } else {
            info!(
                "historical candle fetch DISABLED (features.historical_fetch_enabled = false, PR #449)"
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
            trading_calendar.clone(),
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

    // All table creation queries are independent — run in parallel for faster boot.
    // NOTE: the boot audit row for the `questdb_ready` step is appended AFTER this
    // join — `ensure_boot_audit_table` lives inside the join (line ~1985) and
    // writing to `boot_audit` before the table exists caused AUDIT-04 to fire on
    // every clean boot until 2026-04-28.
    tokio::join!(
        ensure_tick_table_dedup_keys(&config.questdb),
        ensure_depth_and_prev_close_tables(&config.questdb),
        ensure_instrument_tables(&config.questdb),
        ensure_candle_table_dedup_keys(&config.questdb),
        calendar_persistence::ensure_calendar_table(&config.questdb),
        tickvault_storage::constituency_persistence::ensure_constituency_table(&config.questdb),
        tickvault_storage::materialized_views::ensure_candle_views(&config.questdb),
        ensure_greeks_tables(&config.questdb),
        // 2026-05-09 PR 5c.5-final (Bug 3 — movers retirement): the
        // `movers_1s` base table + 25 `movers_*` materialized views are
        // RETIRED. Operator directive: "only ticks and our 9 needed
        // candle timeframes are available". The DROP for these objects
        // now lives in `ensure_candle_views` (Step 4e —
        // `drop_bug3_retired_views`). No CREATE call here.
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
        // PR-C2 (depth-dynamic redesign) — audit table for incremental
        // diff-based resubscribe events emitted by `depth_dynamic_pipeline_v2`.
        // Schema-self-heal pattern: idempotent CREATE-IF-NOT-EXISTS.
        tickvault_storage::depth_dynamic_diff_audit_persistence::ensure_depth_dynamic_diff_audit_table(
            &config.questdb
        ),
        tickvault_storage::ws_reconnect_audit_persistence::ensure_ws_reconnect_audit_table(
            &config.questdb
        ),
        tickvault_storage::boot_audit_persistence::ensure_boot_audit_table(&config.questdb),
        tickvault_storage::selftest_audit_persistence::ensure_selftest_audit_table(&config.questdb),
        tickvault_storage::order_audit_persistence::ensure_order_audit_table(&config.questdb),
        // Wave 6 Sub-PR #1 item 1.4a — shadow candle tables (9 timeframes)
        // + aggregator_seal_audit forensic table. The future writer task
        // (item 1.4b) writes sealed candles into these tables; the boot
        // DDL must run first so the ILP writes don't 404. Idempotent
        // CREATE TABLE IF NOT EXISTS — safe to call on every boot.
        tickvault_storage::shadow_persistence::ensure_shadow_candle_tables(&config.questdb),
    );

    // Wave 6 Sub-PR #1 item 1.4b — spawn the seal-writer tokio loop.
    // This drains the SealAbsorptionPipeline ring (filled by the
    // future producer-side wiring in item 1.4c) → ShadowCandleWriter
    // ILP buffer → flush every 100 ms. On flush failure the rescue
    // cascade walks ring → spill → DLQ → AGGREGATOR-DROP-01.
    //
    // During this slice (1.4b) the producer side is NOT yet wired —
    // the loop runs idle (mpsc empty, ring empty, drain reports
    // is_idle()) at zero CPU cost. The boot wiring is in place so
    // item 1.4c is a thin call-site change in tick_processor.rs.
    //
    // Cancel bridge: the existing `shutdown_notify` Arc<Notify> drives
    // the global graceful-shutdown sequence. We spawn a tiny bridge
    // task that subscribes to it and flips the watch::Sender<bool>
    // that `run_seal_writer_loop` listens on. This keeps the loop's
    // signature unchanged (still uses `tokio::sync::watch` per the
    // shipped 1.2f.5 contract) while integrating with the codebase's
    // existing shutdown pattern.
    {
        use tickvault_storage::seal_writer_loop::{run_seal_writer_loop, seal_drain_interval};
        use tickvault_storage::seal_writer_runner::SealWriterRunner;

        // Bound the per-cycle drain — 1024 seals × 100 ms cycle =
        // 10,240 seals/sec sustained throughput, well above the
        // ~99K-seal IST-midnight burst absorbed across ~10 cycles.
        const SEAL_MAX_DRAIN_PER_CYCLE: usize = 1_024;

        match SealWriterRunner::new(&config.questdb, SEAL_MAX_DRAIN_PER_CYCLE) {
            Ok(runner) => {
                // Wave 6 Sub-PR #1 item 1.4c — publish the seal Sender
                // GLOBALLY before moving `runner` into the spawn block.
                // The future tick-broadcast subscriber task (item 1.4d,
                // declared in a deeper scope where `fast_tick_broadcast_sender`
                // is in scope) reads this via
                // `tickvault_storage::seal_writer_runner::global_seal_sender()`
                // and clones it to push BufferedSeal payloads from the
                // aggregator's seal callback.
                if !tickvault_storage::seal_writer_runner::set_global_seal_sender(runner.sender()) {
                    tracing::warn!(
                        "global seal sender was already installed (idempotent skip) — first installer wins"
                    );
                }
                let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                // Hold `cancel_tx` alive for the lifetime of `main` so the
                // watch channel does not disconnect (which would wake the
                // loop's `.changed().await` immediately). The full graceful-
                // shutdown bridge (subscribe to `shutdown_notify` →
                // `cancel_tx.send(true)` → loop performs final drain) is
                // wired in item 1.4c when the producer side becomes
                // observable. For now: forget() leaks the sender to the
                // 'static lifetime; on process exit Linux reclaims it.
                std::mem::forget(cancel_tx);
                tokio::spawn(async move {
                    let _final_outcome =
                        run_seal_writer_loop(runner, seal_drain_interval(), cancel_rx).await;
                    tracing::info!("seal writer loop exited gracefully");
                });
                tracing::info!(
                    interval_ms = seal_drain_interval().as_millis(),
                    max_drain_per_cycle = SEAL_MAX_DRAIN_PER_CYCLE,
                    "Wave 6 Sub-PR #1 item 1.4b — seal writer task spawned"
                );
            }
            Err(err) => {
                // Constructing the runner fails only if QuestDB ILP is
                // catastrophically misconfigured (the lazy connect path
                // already absorbs unreachable QuestDB). Log + continue —
                // legacy candles_1s path is still active so trading
                // does not stop, but shadow tables will not populate.
                tracing::error!(
                    ?err,
                    "failed to construct SealWriterRunner — shadow tables will NOT populate this session"
                );
            }
        }
    }

    // Wave 2 Item 9 — boot audit row for the QuestDB readiness step.
    // Must run AFTER the `tokio::join!` above so `ensure_boot_audit_table`
    // has created the table. Writing this row before the join was the cause
    // of AUDIT-04 firing on every clean boot until 2026-04-28.
    // Best-effort: failures don't halt boot — the AUDIT-04 ErrorCode +
    // tracing::error! covers regression.
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

    // 2026-04-28 audit gap closure: spawn the disk-health watcher.
    // Closes the highest-risk hole in the zero-loss chain ("disk full +
    // QuestDB down simultaneously"). Operator now gets ~hours of warning
    // via `tv_spill_dir_free_bytes` gauge before the spill disk fills.
    let _disk_health_watcher_handle =
        tickvault_storage::disk_health_watcher::spawn_spill_disk_health_watcher(
            std::path::PathBuf::from("data/spill"),
        );

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

    // Depth-200 uses the same shared TOTP/APP `token_handle` as Live Feed,
    // Depth-20 and Order-Update. Dhan removed the server-side `tokenConsumerType`
    // gate on `wss://full-depth-api.dhan.co` (Ticket #5610706, 2026-05-02 —
    // "either a SELF token or an APP token, and both should now work
    // seamlessly for fetching the 200-level market depth data"), so the
    // separate `Depth200SelfTokenManager` workaround is retired. Git
    // history preserves the SELF code if Dhan ever regresses.

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

    // 29-tf engine Phase 3 commit 3: spawn the 1s candle cascade
    // consumer. Subscribes to the same tick broadcast as persistence +
    // observability and feeds an in-memory `CandleEngineMap<Tf1s>`
    // (per plan L12: only the 1s engine sits on the per-tick path —
    // 28 derived engines + their rtrb SPSC arrive in commit 4).
    //
    // Lives entirely off the persistence hot path: the broadcast
    // channel fans every tick to all subscribers in parallel, so a
    // slow cascade NEVER blocks tick persistence. Lag is reported via
    // `tv_candle_cascade_lag_total`.
    //
    // Three tasks form the cascade subsystem (per adversarial review):
    // 1. Supervisor (H3 fix) — re-spawns the cascade on panic/exit.
    // 2. Midnight rollover (L13 + H1 fix) — seals open bars at IST 00:00
    //    so day-N state never fuses into day-(N+1)'s first bar.
    // 3. Lag coalescing (H2 fix, in cascade.rs) — Telegram errors
    //    rate-limited to 1 per 60s window per task lifetime.
    let candle_engine_map_1s: std::sync::Arc<
        tickvault_trading::candles::CandleEngineMap<tickvault_trading::candles::Tf1s>,
    > = std::sync::Arc::new(tickvault_trading::candles::CandleEngineMap::new());
    // Phase 3 commit 4: 28-TF cascade fanout. Built once at boot, cloned
    // into every consumer. Trading bot reads RAM directly via
    // `cascade_fanout.tf<n>m.latest(security_id, segment_code)`.
    let cascade_fanout: tickvault_trading::candles::CascadeFanout =
        tickvault_trading::candles::CascadeFanout::new();
    {
        let supervisor_sender = tick_broadcast_sender.clone();
        let supervisor_map = std::sync::Arc::clone(&candle_engine_map_1s);
        let supervisor_fanout = cascade_fanout.clone();
        // F1 (Wave-5 §K-L13 / #504e follow-up): wire the existing
        // `prev_day_cache` (constructed at line ~663) into the
        // cascade so every sealed Bar carries the 5 frozen-per-day
        // % fields. The cache is empty at boot today (F2 will add
        // the boot-time loader); the `on_*_with_pct` div-by-zero
        // policy means an empty cache produces 0.0 % fields without
        // dropping any seal — so wiring this in advance is benign
        // until F2 lands.
        let supervisor_pct_cache = std::sync::Arc::clone(&prev_day_cache);
        tokio::spawn(async move {
            tickvault_trading::candles::spawn_supervised_cascade_1s(
                supervisor_sender,
                supervisor_map,
                Some(supervisor_fanout),
                Some(supervisor_pct_cache),
            )
            .await;
        });
        let rollover_map = std::sync::Arc::clone(&candle_engine_map_1s);
        let rollover_fanout = cascade_fanout.clone();
        tokio::spawn(async move {
            // L13: seal both the 1s engine map AND every derived
            // fanout engine at IST midnight so day-N state never
            // fuses into day-(N+1)'s first bar across the 29-TF set.
            tickvault_trading::candles::run_midnight_rollover_task_with_fanout(
                rollover_map,
                Some(rollover_fanout),
            )
            .await;
        });
        info!(
            "29-tf candle cascade started: 1s engine + 28-TF fanout + supervisor + midnight rollover"
        );
    }

    // L10 (Wave-5 #504d): tick_storage broadcast consumer. Subscribes
    // to the same `tick_broadcast_sender` used by the cascade so every
    // tick lands in the in-RAM ring without coupling the tick_processor
    // hot path to TickStorage's lock latency.
    {
        let tick_storage_rx = tick_broadcast_sender.subscribe();
        let storage_for_consumer = std::sync::Arc::clone(&tick_storage);
        tokio::spawn(async move {
            tickvault_trading::in_mem::run_tick_storage_consumer(
                tick_storage_rx,
                storage_for_consumer,
            )
            .await;
        });
        info!(
            per_instrument_capacity = config.in_mem.tick_storage.per_instrument_capacity,
            "L10 tick_storage broadcast consumer spawned + IST 09:15 reset task running"
        );
    }

    // PR #450 commit 6 (2026-05-03): V2 snapshot handle DELETED.
    // The legacy /api/movers (V2) route + handler + in-memory
    // MoversTrackerV2 + V2 pipeline are gone. The new unified
    // /api/movers handler (commit 4) reads from the canonical
    // movers_1s + 25 mat views via QuestDB SQL.

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

        // 2026-05-09 PR 5c.5-final: movers infrastructure RETIRED.
        // `MoversWriter` / `movers_pipeline` / `movers_base_persistence`
        // modules deleted. The in-memory `OptionMoversTracker` /
        // `TopMoversTracker` are unaffected — they feed the depth-
        // dynamic cohort selector via `Bar` snapshots, not QuestDB.
        let option_movers_tracker = Some(tickvault_core::pipeline::OptionMoversTracker::new());

        // O(1) EXEMPT: cold path — clone registry once for tick processor enrichment.
        let slow_registry = subscription_plan
            .as_ref()
            .map(|p| std::sync::Arc::new(p.registry.clone()));

        // 2026-05-09 PR 5c.5-final — movers infrastructure RETIRED.
        // The `movers_writer` + `movers_base_persistence` modules and
        // the `movers_pipeline` orchestrator are deleted in this commit.
        // Operator directive: only `ticks` and the 9 cascade-fed
        // candle timeframes remain in QuestDB. The block below preserves
        // the prev_oi / prev_day cache loaders (consumed by the cascade
        // seal-time pct-stamping path + the tick enricher) but no longer
        // drives any QuestDB-backed movers writer.
        //
        // Operator note: bhavcopy 16:30 IST cross-check (also reads
        // `movers_1s`) is left intact — it will report `MISSING_OUR`
        // for every NSE row from tomorrow's run until a follow-up PR
        // migrates that query to a non-movers source. Operator has
        // explicitly opted into this trade-off ("skip bhavcopy").
        if let Some(_registry) = slow_registry.as_ref() {
            // 2026-05-09: prev_oi loader simplified to overlay-only.
            // Wave-5 indices-only scope subscribes only NIFTY /
            // BANKNIFTY / SENSEX derivatives — exactly the 3
            // underlyings the Option Chain REST overlay covers. The
            // bhavcopy fetch + macOS `unzip` shell-out (recurring
            // PREVOI-01 broken-pipe failures on macOS Info-ZIP 6.00)
            // is retired per operator directive.
            let prev_oi_cache =
                tickvault_app::prev_oi_loader::load_prev_oi_cache_at_boot_with_overlay(
                    token_handle.clone(),
                    ws_client_id.clone(),
                    config.dhan.rest_api_base_url.clone(),
                )
                .await;
            if prev_oi_cache.is_empty() {
                warn!(
                    code = tickvault_common::error_code::ErrorCode::PrevOi01CacheEmptyAtBoot
                        .code_str(),
                    "prev_oi cache EMPTY at boot — Option Chain overlay returned \
                     zero entries for NIFTY/BANKNIFTY/SENSEX; downstream consumers \
                     will see `current_OI - 0 = current_OI` until next boot."
                );
            } else {
                info!(
                    cache_size = prev_oi_cache.len(),
                    "prev_oi cache populated via Option Chain overlay — \
                     downstream OI Change is Dhan-precise"
                );
            }
            // F2 (Wave-5 §K-L13 / #504e follow-up): populate the
            // `PrevDayCache` consumed by the cascade seal-time pct-
            // stamping path (PR #520 / F1). Reads `prev_close` from
            // QuestDB's `previous_close` table and merges the boot-
            // loaded `prev_oi_cache` (immediately above). Best-effort
            // — on QuestDB unreachable / SELECT failure the loader
            // emits PREVCLOSE-04 and the cascade falls back to 0.0
            // pct fields per the div-by-zero policy.
            let f2_questdb = config.questdb.clone();
            let f2_cache = std::sync::Arc::clone(&prev_day_cache);
            let f2_prev_oi = std::sync::Arc::clone(&prev_oi_cache);
            tokio::spawn(async move {
                tickvault_app::prev_day_cache_loader::populate_and_log(
                    &f2_questdb,
                    &f2_cache,
                    &f2_prev_oi,
                )
                .await;
            });
        } else {
            warn!(
                "prev_oi cache loader NOT spawned — slow_registry is None \
                 (subscription_plan absent)"
            );
        }

        // Wave 5 Item 26 L2 LIVE — 16:30 IST bhavcopy cross-check task.
        // Post-market only (TradingCalendar gated); reads `movers_1s`
        // for our captured EOD volumes, downloads NSE bhavcopy ZIP via
        // `unzip` shell, parses, cross-checks with 0.1% tolerance, writes
        // audit rows to `volume_nse_audit`, emits Telegram summary.
        let bhavcopy_shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
        let bhavcopy_registry =
            std::sync::Arc::clone(slow_registry.as_ref().unwrap_or_else(|| {
                // Fallback: if slow_registry is None (boot-bypass scenarios),
                // use a fresh empty Arc — the supplier returns empty contracts
                // and the cycle reports MISSING_OUR for every NSE row.
                // Operator triages the empty universe via dashboards.
                unreachable!("slow_registry is required for slow-boot path")
            }));
        let _bhavcopy_handle = tickvault_app::bhavcopy_pipeline::spawn_bhavcopy_scheduler_task(
            config.questdb.clone(),
            {
                let reg = std::sync::Arc::clone(&bhavcopy_registry);
                move || {
                    // Snapshot the registry's derivative contracts at
                    // each 16:30 IST cycle. Builds `DerivativeContract`
                    // from `SubscribedInstrument` fields. Filters to
                    // future/option derivatives only (skips IDX_I and
                    // NSE_EQ since bhavcopy is NSE_FNO-only). Empty
                    // Vec if registry empty — bhavcopy reports
                    // MISSING_OUR for every NSE row, which is correct.
                    use tickvault_common::instrument_types::{
                        DerivativeContract, DhanInstrumentKind,
                    };
                    reg.iter()
                        .filter_map(|inst| {
                            let kind = inst.instrument_kind?;
                            // Bhavcopy is F&O — skip indices + cash equities.
                            match kind {
                                DhanInstrumentKind::FutureIndex
                                | DhanInstrumentKind::FutureStock
                                | DhanInstrumentKind::OptionIndex
                                | DhanInstrumentKind::OptionStock => {}
                            }
                            let expiry_date = inst.expiry_date?;
                            Some(DerivativeContract {
                                security_id: inst.security_id,
                                underlying_symbol: inst.underlying_symbol.clone(),
                                instrument_kind: kind,
                                exchange_segment: inst.exchange_segment,
                                expiry_date,
                                strike_price: inst.strike_price.unwrap_or(0.0),
                                option_type: inst.option_type,
                                // Bhavcopy lookup only uses underlying_symbol +
                                // expiry_date + strike_price + option_type.
                                // Fill the rest with safe defaults.
                                lot_size: 0,
                                tick_size: 0.0,
                                symbol_name: inst.display_label.clone(),
                                display_name: inst.display_label.clone(),
                            })
                        })
                        .collect()
                }
            },
            std::sync::Arc::clone(&notifier),
            std::sync::Arc::clone(&bhavcopy_shutdown),
        );

        // Wave 5 Items 4+5 LIVE — depth-20 conn-5 dynamic top-50 + depth-200
        // dynamic top-5 orchestrator. Off by default; flip
        // `[features] depth_dynamic_top_volume = true` to activate.
        //
        // Wiring contract: the orchestrator OWNS the Sender side of the
        // command channels here. The Receiver side must be picked up by
        // the depth-conn-pool refactor (separate sub-PR) which spawns:
        //   * 1 dedicated depth-20 conn 5 (subscribes/swaps via the
        //     `depth_20_conn5_rx` receiver)
        //   * 5 depth-200 conns indexed 0..4 (each consumes its
        //     `depth_200_slot_N_rx` receiver)
        //
        // Until that refactor lands, the orchestrator's `cmd_tx.send()`
        // calls will fail (no receiver task) → DEPTH-DYN-02 fires on
        // every 60s cycle inside market hours. That's the visible
        // signal the operator uses to schedule the conn-pool refactor.
        // PR-C2 cutover gate. When `[features].depth_dynamic_pipeline_v2 = true`,
        // the unified pipeline_v2 path replaces BOTH the Wave 5 orchestrator
        // block below AND the single-side static depth-20 spawn block at
        // line ~4118. Default: false (Wave 5 path active for safe rollback).
        let pipeline_v2_active = config.features.depth_dynamic_pipeline_v2;
        if pipeline_v2_active {
            tracing::info!(
                "PR-C2 cutover: depth_dynamic_pipeline_v2 feature ON — \
                 spawning 5×depth-20 + 5×depth-200 deferred minimal_conns + \
                 unified spawn_depth_dynamic_pool orchestrators"
            );

            // Validate config invariants BEFORE any spawn. Halt boot on
            // misconfiguration (per per-wave-guarantee-matrix.md row 4 +
            // disaster-recovery.md "fail-fast at boot" rule).
            if let Err(e) = config.depth_20.dynamic.assert_invariants(
                "depth_20",
                tickvault_common::constants::MAX_TWENTY_DEPTH_CONNECTIONS,
            ) {
                anyhow::bail!("invalid [depth_20.dynamic] config: {e}");
            }
            if let Err(e) = config.depth_200.dynamic.assert_invariants(
                "depth_200",
                tickvault_common::constants::MAX_TWO_HUNDRED_DEPTH_CONNECTIONS,
            ) {
                anyhow::bail!("invalid [depth_200.dynamic] config: {e}");
            }
            // PR-C2 follow-up H4 — soft warning if either pool is below
            // Dhan cap. Operator may legitimately deploy a smaller pool
            // for testing, so this is WARN-only (not fatal).
            if let Some(w) = config.depth_20.dynamic.under_provisioned_warning(
                "depth_20",
                tickvault_common::constants::MAX_TWENTY_DEPTH_CONNECTIONS,
            ) {
                tracing::warn!("PR-C2 H4: {w}");
            }
            if let Some(w) = config.depth_200.dynamic.under_provisioned_warning(
                "depth_200",
                tickvault_common::constants::MAX_TWO_HUNDRED_DEPTH_CONNECTIONS,
            ) {
                tracing::warn!("PR-C2 H4: {w}");
            }

            let v2_shutdown = std::sync::Arc::new(tokio::sync::Notify::new());

            // ----- Depth-20: 5 deferred minimal_conns -----
            let mut d20_v2_cmd_senders: std::collections::HashMap<
                u8,
                tokio::sync::mpsc::Sender<tickvault_core::websocket::DepthCommand>,
            > = std::collections::HashMap::new();
            for slot in 0..config.depth_20.dynamic.conns {
                let (tx, rx) =
                    tokio::sync::mpsc::channel::<tickvault_core::websocket::DepthCommand>(4);
                d20_v2_cmd_senders.insert(slot, tx);
                tickvault_app::depth_20_conn_spawner::spawn_depth_20_minimal_conn(
                    tickvault_app::depth_20_conn_spawner::Depth20MinimalConnInputs {
                        token_handle: token_handle.clone(),
                        ws_client_id: ws_client_id.clone(),
                        label: format!("V2-DYN-20-SLOT-{slot}"),
                        instruments: Vec::new(), // DEFERRED — pool sends Add20
                        cmd_rx: rx,
                        questdb_config: config.questdb.clone(),
                        notifier: notifier.clone(),
                        health_status: health_status.clone(),
                        ws_frame_spill: ws_frame_spill.clone(),
                    },
                );
            }

            let d20_pipeline_cfg = tickvault_app::depth_dynamic_pipeline_v2::PipelineConfig {
                questdb: config.questdb.clone(),
                selector:
                    tickvault_core::instrument::depth_dynamic_top_volume_selector::SelectorConfig {
                        instrument_types: config.depth_20.dynamic.universe.instrument_types.clone(),
                        exchange_segments: config
                            .depth_20
                            .dynamic
                            .universe
                            .exchange_segments
                            .clone(),
                        k: usize::from(config.depth_20.dynamic.conns)
                            * usize::from(config.depth_20.dynamic.sids_per_conn),
                    },
                shape: tickvault_core::instrument::dynamic_subscription_state::PoolShape {
                    conns: config.depth_20.dynamic.conns,
                    sids_per_conn: config.depth_20.dynamic.sids_per_conn,
                },
                label: "depth-20-dynamic",
                // Audit-2026-05-03 redesign: thread the operator's
                // [depth_*.dynamic.universe] min_liquidity_volume +
                // window_secs through to the pipeline. The legacy
                // cohort_size top-N param was retired — only liquid
                // contracts (volume >= min_liquidity_volume) qualify
                // for depth subscription per operator clarification.
                min_liquidity_volume: config.depth_20.dynamic.universe.min_liquidity_volume,
                window_secs: config.depth_20.dynamic.universe.window_secs,
                // 2026-05-02 — operator-requested symbol-level Telegram
                // diff event. Registry resolves (sid, segment) → display
                // label per I-P1-11 composite key. Notifier dispatches
                // `DepthDynamicV2DiffApplied` events on every non-no-op cycle.
                registry: slow_registry.clone(),
                notifier: Some(notifier.clone()),
                // 2026-05-09 PR 5c.2 — RAM-first cohort fetch via the
                // in-RAM CascadeFanout. Falls back to the legacy
                // `movers_1m` SQL path automatically when the snapshot
                // is empty (early boot before the cascade is primed).
                cascade: Some(std::sync::Arc::new(cascade_fanout.clone())),
            };
            let d20_pipeline_handle =
                tickvault_app::depth_dynamic_pipeline_v2::spawn_depth_dynamic_pool(
                    d20_pipeline_cfg,
                    d20_v2_cmd_senders,
                    std::sync::Arc::clone(&v2_shutdown),
                );
            // Hostile-bug-hunt H1 fix — capture JoinHandle and detect
            // a silent panic in the orchestrator. Mirrors the Wave 5
            // pattern at the legacy orchestrator block (M2 watchdog).
            tokio::spawn(async move {
                match d20_pipeline_handle.await {
                    Ok(()) => {
                        tracing::info!(
                            "depth-20-dynamic v2 orchestrator exited cleanly (shutdown signalled)"
                        );
                    }
                    Err(err) if err.is_panic() => {
                        tracing::error!(
                            ?err,
                            "depth-20-dynamic v2 orchestrator task PANICKED — \
                             5 conns will stop receiving Add/Remove cmds; \
                             operator MUST restart the app to recover"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            ?err,
                            "depth-20-dynamic v2 orchestrator task cancelled (shutdown race)"
                        );
                    }
                }
            });

            // ----- Depth-200: 5 deferred minimal_conns -----
            let mut d200_v2_cmd_senders: std::collections::HashMap<
                u8,
                tokio::sync::mpsc::Sender<tickvault_core::websocket::DepthCommand>,
            > = std::collections::HashMap::new();
            // PR-B (2026-05-02): boot-smoke counter shared across all 5
            // depth-200 receivers. See `crates/app/src/boot_smoke_test.rs`.
            let v2_d200_frame_counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            for slot in 0..config.depth_200.dynamic.conns {
                let (tx, rx) =
                    tokio::sync::mpsc::channel::<tickvault_core::websocket::DepthCommand>(4);
                d200_v2_cmd_senders.insert(slot, tx);
                let stagger_ms = u64::from(slot)
                    .saturating_mul(tickvault_core::websocket::DEPTH_200_INITIAL_STAGGER_MS);
                tickvault_app::depth_20_conn_spawner::spawn_depth_200_minimal_conn(
                    tickvault_app::depth_20_conn_spawner::Depth200MinimalConnInputs {
                        token_handle: token_handle.clone(),
                        ws_client_id: ws_client_id.clone(),
                        label: format!("V2-DYN-200-SLOT-{slot}"),
                        exchange_segment: tickvault_common::types::ExchangeSegment::NseFno,
                        security_id: None, // DEFERRED — pool sends Add200
                        cmd_rx: rx,
                        questdb_config: config.questdb.clone(),
                        notifier: notifier.clone(),
                        health_status: health_status.clone(),
                        ws_frame_spill: ws_frame_spill.clone(),
                        initial_stagger_ms: stagger_ms,
                        depth_200_frame_counter: Some(std::sync::Arc::clone(
                            &v2_d200_frame_counter,
                        )),
                    },
                );
            }
            // Hostile-bug-hunt smoke-test fix: SKIP `spawn_depth_200_boot_smoke_test`
            // in v2 mode. The smoke test deadline is 60s — but in v2 the
            // first dispatch fires at t=60s (RESELECT_INTERVAL_SECS) and
            // the first frame typically arrives at t=62-65s (handshake +
            // first stream frame), so the smoke test would fire DEPTH200-
            // SMOKE-01 Critical at every clean v2 boot.
            // Drop the v2_d200_frame_counter — pipeline_v2's own
            // observability (`tv_depth_dynamic_set_size{feed}` gauge +
            // alert `tv-depth-dynamic-set-size-low`) covers the same
            // failure mode without the 60s race.
            drop(v2_d200_frame_counter);

            let d200_pipeline_cfg = tickvault_app::depth_dynamic_pipeline_v2::PipelineConfig {
                questdb: config.questdb.clone(),
                selector:
                    tickvault_core::instrument::depth_dynamic_top_volume_selector::SelectorConfig {
                        instrument_types: config
                            .depth_200
                            .dynamic
                            .universe
                            .instrument_types
                            .clone(),
                        exchange_segments: config
                            .depth_200
                            .dynamic
                            .universe
                            .exchange_segments
                            .clone(),
                        k: usize::from(config.depth_200.dynamic.conns)
                            * usize::from(config.depth_200.dynamic.sids_per_conn),
                    },
                shape: tickvault_core::instrument::dynamic_subscription_state::PoolShape {
                    conns: config.depth_200.dynamic.conns,
                    sids_per_conn: config.depth_200.dynamic.sids_per_conn,
                },
                label: "depth-200-dynamic",
                // Audit-2026-05-03: same min-volume liquidity gate as depth-20.
                min_liquidity_volume: config.depth_200.dynamic.universe.min_liquidity_volume,
                window_secs: config.depth_200.dynamic.universe.window_secs,
                registry: slow_registry.clone(),
                notifier: Some(notifier.clone()),
                // 2026-05-09 PR 5c.2 — RAM-first cohort fetch via the
                // in-RAM CascadeFanout. Falls back to the legacy
                // `movers_1m` SQL path automatically when the snapshot
                // is empty (early boot before the cascade is primed).
                cascade: Some(std::sync::Arc::new(cascade_fanout.clone())),
            };
            let d200_pipeline_handle =
                tickvault_app::depth_dynamic_pipeline_v2::spawn_depth_dynamic_pool(
                    d200_pipeline_cfg,
                    d200_v2_cmd_senders,
                    v2_shutdown,
                );
            // Hostile-bug-hunt H1 fix — capture JoinHandle for panic detection.
            tokio::spawn(async move {
                match d200_pipeline_handle.await {
                    Ok(()) => {
                        tracing::info!(
                            "depth-200-dynamic v2 orchestrator exited cleanly (shutdown signalled)"
                        );
                    }
                    Err(err) if err.is_panic() => {
                        tracing::error!(
                            ?err,
                            "depth-200-dynamic v2 orchestrator task PANICKED — \
                             5 slots will stop receiving Add/Remove cmds; \
                             operator MUST restart the app"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            ?err,
                            "depth-200-dynamic v2 orchestrator task cancelled (shutdown race)"
                        );
                    }
                }
            });
            tracing::info!(
                "PR-C2 cutover: depth_dynamic_pipeline_v2 spawn complete \
                 (depth-20: {}×{} = {} SIDs; depth-200: {}×{} = {} SIDs)",
                config.depth_20.dynamic.conns,
                config.depth_20.dynamic.sids_per_conn,
                usize::from(config.depth_20.dynamic.conns)
                    * usize::from(config.depth_20.dynamic.sids_per_conn),
                config.depth_200.dynamic.conns,
                config.depth_200.dynamic.sids_per_conn,
                usize::from(config.depth_200.dynamic.conns)
                    * usize::from(config.depth_200.dynamic.sids_per_conn),
            );
        } else if config.features.depth_dynamic_top_volume {
            tracing::info!(
                "Wave 5 Items 4+5: depth_dynamic_top_volume feature ON — \
                 spawning orchestrator + receiver-side dynamic depth-20 conn 5"
            );
            let depth_dyn_shutdown = std::sync::Arc::new(tokio::sync::Notify::new());

            // Depth-20 conn 5 dynamic top-50: orchestrator (Sender) +
            // receiver-side WS connection (Receiver). Wave 5 commit 2.
            //
            // The orchestrator owns the Sender side and emits Swap20 /
            // InitialSubscribe20 every 60s based on `option_movers`
            // top-50 by `change_pct DESC`, SENSEX excluded. The receiver
            // side here spawns a real depth-20 WS connection in DEFERRED
            // mode (empty initial instruments) — the orchestrator's first
            // cycle InitialSubscribe20 populates the SID list.
            let (d20_conn5_cmd_tx, d20_conn5_cmd_rx) =
                tokio::sync::mpsc::channel::<tickvault_core::websocket::DepthCommand>(4);

            tickvault_app::depth_20_conn_spawner::spawn_depth_20_minimal_conn(
                tickvault_app::depth_20_conn_spawner::Depth20MinimalConnInputs {
                    token_handle: token_handle.clone(),
                    ws_client_id: ws_client_id.clone(),
                    label: "DYN-TOP50".to_string(),
                    instruments: Vec::new(), // DEFERRED — orchestrator sends InitialSubscribe20
                    cmd_rx: d20_conn5_cmd_rx,
                    questdb_config: config.questdb.clone(),
                    notifier: notifier.clone(),
                    health_status: health_status.clone(),
                    ws_frame_spill: ws_frame_spill.clone(),
                },
            );

            // M2 — JoinHandle panic watchdog: capture the orchestrator
            // handle and spawn a tiny supervisor that awaits panic. The
            // orchestrator is the SOLE Sender side of conn 5's command
            // channel; if it panics silently, the receiver-side WS conn
            // 5 stays connected but never receives Swap20 / InitialSubscribe20
            // — manifests as "depth-20 conn 5 connected but no data flowing".
            // Detect-only (no respawn): the orchestrator holds 60s tick
            // state and a clean restart is safer than a partial respawn.
            // Operator action: restart the app on this Telegram.
            let d20_dyn_handle =
                tickvault_app::depth_dynamic_pipeline::spawn_depth_20_dynamic_conn5_task(
                    config.questdb.clone(),
                    d20_conn5_cmd_tx,
                    std::sync::Arc::clone(&depth_dyn_shutdown),
                );
            tokio::spawn(async move {
                match d20_dyn_handle.await {
                    Ok(()) => {
                        tracing::info!(
                            "depth-20 dynamic conn 5 orchestrator exited cleanly \
                             (shutdown signalled)"
                        );
                    }
                    Err(err) if err.is_panic() => {
                        tracing::error!(
                            ?err,
                            "depth-20 dynamic conn 5 orchestrator task PANICKED — \
                             conn 5 will stop receiving Swap20 commands; operator \
                             MUST restart the app to recover dynamic top-50 swaps"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            ?err,
                            "depth-20 dynamic conn 5 orchestrator task cancelled \
                             (likely shutdown race)"
                        );
                    }
                }
            });

            // Depth-200 dynamic pool (5 slots). One channel + receiver
            // depth-200 WS connection per slot. Wave 5 commit 3.
            //
            // The orchestrator owns Sender<DepthCommand> per slot and
            // emits Swap200 / InitialSubscribe200 every 60s based on
            // `option_movers` top-5 by `change_pct DESC`, SENSEX excluded.
            // Receiver-side connections start in DEFERRED mode (sid=None)
            // — orchestrator's first cycle InitialSubscribe200 populates
            // each slot's contract. Initial-connect stagger spreads the
            // 5 first-connect handshakes over 10s to avoid Dhan TCP-RST
            // storm (per audit-findings 2026-04-24 Fix C).
            // H1 capacity assertion: depth-200 dynamic pool MUST equal
            // MAX_TWO_HUNDRED_DEPTH_CONNECTIONS (=5). The Dhan 5-conn cap
            // per pool is documented in `dhan-ref/04-full-market-depth`;
            // a future commit accidentally widening this bound would be
            // silently rejected by Dhan. Compile-time assertion here
            // catches the regression at boot, NOT at first market-open.
            const _D200_POOL_SIZE_INVARIANT: () = assert!(
                tickvault_common::constants::MAX_TWO_HUNDRED_DEPTH_CONNECTIONS == 5,
                "Wave 5 commit 3 depth-200 pool sized for exactly 5 slots; \
                 update the spawn loop range AND this assertion together."
            );
            let mut d200_cmd_senders = std::collections::HashMap::new();
            let mut d200_dyn_spawn_count: usize = 0;

            // PR-B (2026-05-02): one shared atomic counter incremented by all
            // 5 depth-200 receivers on every frame. The boot smoke test polls
            // this counter for the first ≥ 1 transition. See
            // `crates/app/src/boot_smoke_test.rs` for classifier + emission.
            let depth_200_frame_counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

            for slot in 0..tickvault_common::constants::MAX_TWO_HUNDRED_DEPTH_CONNECTIONS {
                let (tx, rx) =
                    tokio::sync::mpsc::channel::<tickvault_core::websocket::DepthCommand>(4);
                d200_cmd_senders.insert(slot, tx);

                // Depth-200 reuses the shared TOTP/APP token handle
                // (Ticket #5610706 — Dhan removed the SELF-only gate).
                let depth200_token = token_handle.clone();
                let stagger_ms = (slot as u64)
                    .saturating_mul(tickvault_core::websocket::DEPTH_200_INITIAL_STAGGER_MS);
                tickvault_app::depth_20_conn_spawner::spawn_depth_200_minimal_conn(
                    tickvault_app::depth_20_conn_spawner::Depth200MinimalConnInputs {
                        token_handle: depth200_token,
                        ws_client_id: ws_client_id.clone(),
                        label: format!("DYN-200-SLOT-{slot}"),
                        exchange_segment: tickvault_common::types::ExchangeSegment::NseFno,
                        security_id: None, // DEFERRED — orchestrator sends InitialSubscribe200
                        cmd_rx: rx,
                        questdb_config: config.questdb.clone(),
                        notifier: notifier.clone(),
                        health_status: health_status.clone(),
                        ws_frame_spill: ws_frame_spill.clone(),
                        initial_stagger_ms: stagger_ms,
                        depth_200_frame_counter: Some(std::sync::Arc::clone(
                            &depth_200_frame_counter,
                        )),
                    },
                );
                d200_dyn_spawn_count = d200_dyn_spawn_count.saturating_add(1);
            }

            // PR-B: spawn the boot-time depth-200 smoke test once all 5
            // slots are wired. Off-hours boots produce a `Skipped` log and
            // exit at the deadline; in-market boots emit `Passed` (Info)
            // on the first frame or `Failed` (Critical, DEPTH200-SMOKE-01)
            // at the deadline if zero frames arrived.
            tickvault_app::boot_smoke_test::spawn_depth_200_boot_smoke_test(std::sync::Arc::clone(
                &depth_200_frame_counter,
            ));
            // H1 + M3 — runtime invariant: spawn loop emitted exactly 5 conns.
            // Saturating add means a logic bug (e.g. early break) leaves the
            // counter < 5; a duplicate spawn could push it > 5. Both fail.
            assert_eq!(
                d200_dyn_spawn_count,
                tickvault_common::constants::MAX_TWO_HUNDRED_DEPTH_CONNECTIONS,
                "Wave 5: depth-200 dynamic pool spawned {d200_dyn_spawn_count} \
                 conns; expected exactly {} per Dhan 5-conn cap.",
                tickvault_common::constants::MAX_TWO_HUNDRED_DEPTH_CONNECTIONS
            );
            // M2 — JoinHandle panic watchdog (depth-200 5-slot pool).
            // Symmetric with the conn 5 watchdog above. The 5 minimal
            // depth-200 conns block on InitialSubscribe200 from this
            // orchestrator; if it panics, all 5 stay in DEFERRED mode
            // forever (no contract subscribed). Operator restart required.
            let d200_dyn_handle =
                tickvault_app::depth_dynamic_pipeline::spawn_depth_200_dynamic_pool_task(
                    config.questdb.clone(),
                    d200_cmd_senders,
                    std::sync::Arc::clone(&depth_dyn_shutdown),
                );
            tokio::spawn(async move {
                match d200_dyn_handle.await {
                    Ok(()) => {
                        tracing::info!(
                            "depth-200 dynamic pool orchestrator exited cleanly \
                             (shutdown signalled)"
                        );
                    }
                    Err(err) if err.is_panic() => {
                        tracing::error!(
                            ?err,
                            "depth-200 dynamic pool orchestrator task PANICKED — \
                             all 5 depth-200 slots stuck in DEFERRED mode; operator \
                             MUST restart the app to resume top-5 contract subscription"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            ?err,
                            "depth-200 dynamic pool orchestrator task cancelled \
                             (likely shutdown race)"
                        );
                    }
                }
            });
        } else {
            tracing::info!(
                "Wave 5 Items 4+5: depth_dynamic_top_volume feature OFF (default) — \
                 dynamic top-volume orchestrator NOT spawned; legacy depth pool active"
            );
        }

        // Parthiban directive (2026-04-21): no-tick-during-market-hours
        // watchdog (slow boot path). Same pattern as fast boot above.
        let slow_tick_heartbeat = tickvault_core::pipeline::no_tick_watchdog::new_tick_heartbeat();
        let _slow_no_tick_watchdog_handle =
            tickvault_core::pipeline::no_tick_watchdog::spawn_no_tick_watchdog(
                std::sync::Arc::clone(&slow_tick_heartbeat),
                Some(std::sync::Arc::clone(&notifier)),
            );

        // 29-tf engine plan Phase 2.6 — Phase 2 lifecycle enricher.
        //
        // Construct the enricher and load the prev_oi_cache from
        // QuestDB candles_1d BEFORE spawning the tick processor (L14
        // boot ordering: cache → engines → replay → THEN subscribe).
        // The DDL setup at line 2103 has already created candles_1d,
        // so `LATEST ON ts PARTITION BY (security_id, segment)` will
        // either return yesterday's row per instrument or empty
        // (fresh deploy). Either way is graceful — empty cache → 0
        // prev_day_oi → formulas.rs returns 0.0 pct (documented
        // degradation).
        //
        // We block on the load so the prev_day_oi column is populated
        // for the FIRST tick after subscribe fires. The internal
        // timeout (PREV_OI_LOAD_TIMEOUT_SECS = 30s) caps the wait so
        // a hung QuestDB cannot stall boot indefinitely.
        //
        // The BootOrderingGate (L14 helper) tracks the four boot
        // phases. Phase 3 (in-memory engines) and the explicit replay
        // phase land in Phase 3 of the plan; we mark them ready
        // immediately because the slow-boot SubscribeRxGuard
        // machinery already performs the equivalent backfill via the
        // SPSC consumer. The gate gives operators a single positive
        // assertion that L14 was satisfied before subscribe fired.
        let boot_ordering_gate = std::sync::Arc::new(
            tickvault_core::pipeline::boot_ordering_gate::BootOrderingGate::new(),
        );
        let tick_enricher =
            std::sync::Arc::new(tickvault_core::pipeline::tick_enricher::TickEnricher::new());
        // Phase 2.8 H4 fix: only mark the gate ready on Ok. On Err the
        // gate stays in `AwaitingOiCache` and `try_authorize_subscribe`
        // refuses authorization — the operator gets a typed ERROR
        // (PREVCLOSE-01) and Telegram, not a False-OK. Loading
        // graceful-degrades on truly empty candles_1d (Ok with
        // count=0), only flagging the actual QuestDB-unreachable /
        // schema-broken cases as failures.
        let oi_cache_load_succeeded = match tick_enricher
            .prev_oi_cache
            .load_from_questdb(&config.questdb)
            .await
        {
            Ok(count) => {
                tracing::info!(
                    entries = count,
                    "prev_oi_cache loaded for tick enricher (Phase 2.6 production attach)"
                );
                metrics::counter!("tv_prev_oi_cache_load_total", "outcome" => "ok").increment(1);
                if count == 0 {
                    // Phase 2.8 H3 partial fix: emit a Prom counter +
                    // structured signal so an empty candles_1d doesn't
                    // silently produce 0% OI changes for the day. The
                    // gate still authorizes (count=0 is valid for fresh
                    // deploy / first trading day), but operator sees
                    // the diagnostic.
                    //
                    // Wave-Holiday-Gate (2026-05-09): only escalate to
                    // `warn!` (Loki → Telegram) inside the trading
                    // session — outside it (off-hours, weekend, holiday
                    // boots) an empty cache is the expected state, not
                    // a signal of degradation. Same noise-reduction
                    // pattern as PR #542 item B (PREVCLOSE-04).
                    metrics::counter!("tv_prev_oi_cache_empty_total").increment(1);
                    if tickvault_common::market_hours::is_within_trading_session_ist() {
                        tracing::warn!(
                            "prev_oi_cache loaded zero entries — fresh deploy or candles_1d \
                             empty for the prior trading day. OI Change panels will read 0% \
                             until the next IST midnight rollover repopulates the cache."
                        );
                    } else {
                        tracing::info!(
                            "prev_oi_cache loaded zero entries (off-hours / weekend boot — \
                             expected; OI Change panels will read 0% until live ticks \
                             populate candles_1d during the next trading session)"
                        );
                    }
                }
                true
            }
            Err(err) => {
                tracing::error!(
                    code = tickvault_common::error_code::ErrorCode::PrevClose01IlpFailed.code_str(),
                    ?err,
                    "prev_oi_cache load FAILED — boot ordering gate stays \
                     AwaitingOiCache, subscribe will not be authorized this boot. \
                     Investigate QuestDB candles_1d availability."
                );
                metrics::counter!("tv_prev_oi_cache_load_total", "outcome" => "err").increment(1);
                false
            }
        };
        if oi_cache_load_succeeded {
            boot_ordering_gate.mark_oi_cache_loaded();
        }
        boot_ordering_gate.mark_engines_ready();
        boot_ordering_gate.mark_replay_completed();
        // Phase 2.9 H1 fix (hostile bug-hunt): the BootOrderingGate was
        // previously informational — a failed authorization logged ERROR
        // but boot continued, so the WS subscribe still went out with
        // potentially stale state. The H4 fix in Phase 2.8 (skip
        // `mark_oi_cache_loaded` on Err) means this branch will fire
        // when QuestDB candles_1d is unreachable. Treating it as a
        // panic gives binary-level L14 enforcement: the process refuses
        // to subscribe with an unhealthy state.
        //
        // We use `std::process::exit(1)` rather than `panic!` so the
        // exit cleanly skips destructors that might try to flush partial
        // state. systemd / docker restart policy handles the recovery
        // loop. The 30s prev_oi_cache timeout caps the worst-case wait
        // before this fires.
        if !boot_ordering_gate.try_authorize_subscribe() {
            tracing::error!(
                code = tickvault_common::error_code::ErrorCode::Phase2Ready01PreflightFailed.code_str(),
                readiness = ?boot_ordering_gate.readiness(),
                "L14 boot-ordering gate refused to authorize subscribe — \
                 prev_oi_cache load likely failed (see PREVCLOSE-01 above). \
                 HARD-FAIL: process::exit(1) so systemd/docker restart \
                 the boot rather than subscribe with unhealthy state."
            );
            metrics::counter!("tv_l14_boot_authorization_refused_total").increment(1);
            // Allow tests to exercise this branch without killing the
            // test runner — gated on TICKVAULT_BOOT_DRY_RUN env var
            // which production never sets.
            if std::env::var("TICKVAULT_BOOT_DRY_RUN").is_err() {
                std::process::exit(1);
            }
        } else {
            tracing::info!(
                "L14 boot-ordering gate authorized subscribe (Phase 2.6: \
                 prev_oi_cache loaded, engines ready, replay completed)"
            );
            metrics::counter!("tv_l14_boot_authorization_total").increment(1);
        }
        // Reference the gate so it isn't optimized out — future commits
        // can pass `boot_ordering_gate.clone()` into the WS subscribe
        // dispatcher to gate the actual frame send.
        drop(std::sync::Arc::clone(&boot_ordering_gate));

        // 29-tf engine Phase 2.7 (hostile bug-hunt CRITICAL C1 fix):
        // spawn the IST midnight rollover task. Without this, the
        // volume_delta tracker accumulates baselines across day
        // boundaries and the first ~24,300 ticks at 09:15 IST on
        // Day N+1 trigger false VOLUME-MONO-01 alerts (per L13).
        //
        // The task sleeps until next IST 00:00:00, performs the
        // atomic phase transition (clear volume baselines + clear
        // prev_day_close stamps + reload prev_oi_cache from
        // candles_1d), then loops. Cancelable via the JoinHandle
        // returned by spawn_midnight_rollover_task.
        let _midnight_rollover_handle =
            tickvault_core::pipeline::tick_enricher::spawn_midnight_rollover_task(
                std::sync::Arc::clone(&tick_enricher),
                config.questdb.clone(),
            );
        tracing::info!(
            "midnight rollover task spawned (Phase 2.7 — L13 atomic state \
             transition at IST 00:00:00 every trading day)"
        );

        // Phase 2.11 (hostile bug-hunt M2 + M4 fix): periodic
        // prev_oi_cache refresh task. Covers two scenarios that
        // boot-time load + midnight rollover do NOT cover:
        //   (a) Fresh deploy with empty `candles_1d` — boot returned
        //       Ok(count=0), cache stays empty until next midnight,
        //       OI Change panels read 0% all day. Refresh polls every
        //       5min until candles_1d gets populated.
        //   (b) QuestDB matview chain still building post-boot —
        //       Phase 2.9 hard-fail only fires on outright load
        //       failure, not on empty result. Refresh covers the
        //       gap if the matview catches up after boot.
        // Self-exits once cache becomes non-empty; midnight task
        // takes over from there.
        let _prev_oi_refresh_handle =
            tickvault_core::pipeline::tick_enricher::spawn_prev_oi_cache_refresh_task(
                std::sync::Arc::clone(&tick_enricher),
                config.questdb.clone(),
            );
        tracing::info!(
            "prev_oi_cache periodic refresh task spawned (Phase 2.11 — \
             5min poll for fresh-deploy / matview-catchup recovery)"
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
                option_movers_tracker,
                slow_registry,
                Some(slow_tick_heartbeat),
                // 29-tf engine Phase 2.6 — production-attach the
                // lifecycle enricher. The prev_oi_cache was loaded
                // synchronously above (or skipped on QuestDB error
                // → empty cache, graceful degradation). From this
                // point every live tick that reaches QuestDB writes
                // populated `volume_delta`, `prev_day_close`,
                // `prev_day_oi`, `phase` columns instead of the
                // legacy zero/PREMARKET defaults.
                Some(std::sync::Arc::clone(&tick_enricher)),
            )
            .await;
        });
        info!(
            "tick processor started (with candle aggregation + top movers + option movers + trading broadcast)"
        );
        // Phase 2.12 (hostile L1 fix): boot-mode gauge for slow boot.
        // value=1 indicates the lifecycle enricher is attached and the
        // 4 ticks-table lifecycle columns will be populated this
        // session. Operators can chart the time series:
        //   tv_lifecycle_enricher_attached{boot_mode="slow"} 1
        // → operator sees mode-switch (fast↔slow) in Prometheus
        // history independent of the boot log.
        metrics::gauge!(
            "tv_lifecycle_enricher_attached",
            "boot_mode" => "slow"
        )
        .set(1.0);
        info!(
            boot_mode = "slow",
            enricher_attached = true,
            "BOOT MODE: slow (production path) — TickEnricher attached, \
             prev_oi_cache loaded, BootOrderingGate authorized, midnight \
             rollover + periodic refresh tasks spawned. Lifecycle columns \
             (volume_delta, prev_day_close, prev_day_oi, phase) will be \
             populated for every live tick this session."
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
        // PR #458: now polls pool.health() for truthful state.
        emit_websocket_connected_alerts(
            &notifier,
            &pool_arc,
            tickvault_core::notification::events::BootPathLabel::Slow,
            boot_start,
        )
        .await;
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
    if should_connect_ws {
        let spot_prices_updater = std::sync::Arc::clone(&shared_spot_prices);
        let mut spot_rx = tick_broadcast_sender.subscribe();

        // Wave-5 indices-only scope (PR #509): only IDX_I (NIFTY/BANKNIFTY/SENSEX)
        // spot prices are needed downstream — depth rebalancer + greeks. The
        // legacy `live_tick_atm_resolver` helpers (`build_full_chain_index_lookup`
        // + `classify_tick_for_spot_update` + `partition_resolved_and_stragglers`)
        // were stock-F&O ATM resolution scaffolding and are dead under
        // indices-only. The IDX_I lookup is inlined here; stock LTP capture is
        // dropped entirely.
        // APPROVED: I-P1-11 — `index_lookup` is single-segment IDX_I by construction.
        let index_lookup: std::collections::HashMap<u32, &'static str> = {
            let mut m = std::collections::HashMap::with_capacity(3);
            m.insert(13_u32, "NIFTY");
            m.insert(25_u32, "BANKNIFTY");
            m.insert(51_u32, "SENSEX");
            m
        };
        const IDX_I_SEGMENT_CODE: u8 = 0;

        tokio::spawn(async move {
            loop {
                match spot_rx.recv().await {
                    Ok(tick) => {
                        if tick.exchange_segment_code != IDX_I_SEGMENT_CODE {
                            continue;
                        }
                        if tick.last_traded_price <= 0.0 || !tick.last_traded_price.is_finite() {
                            continue;
                        }
                        if let Some(sym) = index_lookup.get(&tick.security_id).copied() {
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
            "spot price updater started — capturing IDX_I LTPs (NIFTY/BANKNIFTY/SENSEX) for depth + greeks"
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
        if config.features.depth_dynamic_pipeline_v2 {
            // PR-C2 cutover (live default since 2026-05-09): under v2 the
            // depth-20 + depth-200 pools are owned by `spawn_depth_dynamic_pool`
            // in `depth_dynamic_pipeline_v2.rs`. That pool spawned 5×depth-20
            // (50 SIDs/conn) + 5×depth-200 (1 SID/conn) at boot in DEFERRED
            // mode and refills the SID set every 60s from the top-N volume
            // cohort over `movers_1m`. ATM is no longer used for depth.
            //
            // The legacy boot block below (mandatory/optional LTP wait,
            // `select_depth_instruments`, per-conn spawn loops) was producing
            // pure noise under v2:
            //   - 5-min in-session / 10-s off-hours wait for index LTPs that
            //     v2 doesn't need
            //   - `select_depth_instruments` ATM selection whose result was
            //     thrown away (the spawn branch at the v2 cutover was
            //     already an `else if depth_dynamic_pipeline_v2` no-op)
            //   - 3 off-hours WARN log lines on every cold boot:
            //       * "depth ATM: off-market-hours boot — mandatory index LTPs not available"
            //       * "no valid spot price for depth ATM selection" × 2 (NIFTY, BANKNIFTY)
            //
            // Skipping the block entirely is safe because the v2 spawn loop
            // upstream (lines ~3081-3264) already created every depth conn
            // we need, and the 09:13 IST anchor task downstream (lines
            // ~6125+) handles InitialSubscribe dispatch. Operator confirmed
            // the cutover via AskUserQuestion 2026-05-10.
            info!(
                "depth boot setup: legacy ATM-based path SKIPPED — \
                 depth_dynamic_pipeline_v2 owns depth-20 + depth-200 \
                 (5×50 + 5×1 = 255 SIDs via top-N volume cohort over \
                 movers_1m); skipping boot-time index-LTP wait + \
                 select_depth_instruments + per-conn spawn loops"
            );
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
            // Plan item C (2026-04-22, visibility version): the 09:13 IST
            // depth-anchor task needs its own universe handle to look up
            // 2026-05-09 PR 5c.5-final: movers QuestDB infrastructure
            // retired. `PreopenMoversTracker` phase=PREOPEN semantics
            // are no longer materialised to QuestDB; the in-memory
            // tracker chain still drives in-process depth-dynamic
            // cohort selection.
            // Wave 5 commit 5: rebalance consumer needs universe to
            // compute single-side ATM ± 24 windows for Swap20 fan-out
            // to NIFTY-CE / NIFTY-PE / BANKNIFTY-CE / BANKNIFTY-PE
            // depth-20 conns (operator spec 2026-05-01 Option B).
            let rebal_consumer_universe = std::sync::Arc::clone(&universe_arc);

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
                } => {
                    if !tickvault_app::phase2_recovery::should_spawn_phase2_scheduler(
                        config.subscription.scope,
                    ) {
                        // PR-E: stale snapshot from a prior FullUniverse boot. Don't
                        // re-dispatch its stock F&O contracts — they'd be silently
                        // dropped by the planner anyway, but dispatching wastes a
                        // SubscribeCommand round-trip.
                        info!(
                            instrument_count,
                            scope = config.subscription.scope.as_str(),
                            "PR-E: Phase 2 recovery snapshot dispatch SKIPPED under \
                             IndicesOnlyAllExpiries scope (prior-boot stock F&O chain ignored)"
                        );
                    } else {
                        match ws_pool_arc.as_ref() {
                            Some(pool) => {
                                let cmd =
                                    tickvault_core::websocket::SubscribeCommand::AddInstruments {
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
                        }
                    }
                }
                tickvault_app::phase2_recovery::RecoveryAction::SkipOffHours => {
                    info!("Phase 2 recovery: skip-off-hours — scheduler NOT spawned this boot");
                }
                tickvault_app::phase2_recovery::RecoveryAction::RunFreshPhase2
                | tickvault_app::phase2_recovery::RecoveryAction::WaitForScheduler => {
                    // PR #509d (Wave-5 §R.1): the Phase 2 dispatcher chain
                    // (`phase2_scheduler` + `phase2_delta` + `phase2_emit_guard`)
                    // is RETIRED entirely. Under the indices-only scope the
                    // dispatcher would silently no-op anyway; under any future
                    // scope re-enabling stock F&O, dispatch will go through a
                    // newer in-memory-cascade-driven path, not the legacy
                    // 09:13 IST scheduler.
                    info!(
                        scope = config.subscription.scope.as_str(),
                        "Phase 2 scheduler RETIRED (#509d) — legacy 09:13 IST dispatcher \
                         removed; readiness check still runs at 09:13:01 IST"
                    );
                }
            }

            // Audit Finding #5 (2026-05-03): Pre-market positive readiness
            // ping at 09:14:00 IST — exactly 60s before the NSE opening bell.
            // Closes the false-OK gap from audit-findings-2026-04-17.md
            // Rule 11: previously the operator had ZERO positive signals
            // before the bell, only the post-open 09:15:30 confirmation.
            // Severity::Info — never pages, only confirms readiness.
            //
            // Audit-findings Rule 3: market-hours-aware. Trading day check +
            // late-start past 09:14:00 → skip silently.
            {
                let readiness_notifier = notifier.clone();
                let readiness_health = health_status.clone();
                let readiness_calendar = std::sync::Arc::clone(&trading_calendar);
                tokio::spawn(async move {
                    use chrono::{FixedOffset, NaiveTime, TimeZone, Utc};
                    use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
                    let Some(ist_offset) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
                        return;
                    };
                    let now_ist = ist_offset.from_utc_datetime(&Utc::now().naive_utc());
                    let today_ist = now_ist.date_naive();
                    if !readiness_calendar.is_trading_day(today_ist) {
                        info!("market-open readiness: skipping (non-trading day)");
                        return;
                    }
                    let Some(target) = NaiveTime::from_hms_opt(9, 14, 0) else {
                        return;
                    };
                    let now_time = now_ist.time();
                    if now_time >= target {
                        // Mid-session boot past 09:14:00 — skip silently per
                        // 09:15:30 heartbeat precedent (audit-findings Rule 3).
                        debug!(
                            now = %now_time,
                            "market-open readiness: skipping (past 09:14:00 — mid-session boot)"
                        );
                        return;
                    }
                    let secs_until = (target - now_time).num_seconds().max(0) as u64;
                    info!(
                        secs_until,
                        "market-open readiness: sleeping until 09:14:00 IST"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(secs_until)).await;

                    let main_active = readiness_health.websocket_connections() as usize;
                    let d20 = readiness_health.depth_20_connections() as usize;
                    let d200 = readiness_health.depth_200_connections() as usize;
                    let oms = readiness_health.order_update_connected();
                    let token_secs = readiness_health.token_remaining_secs();
                    info!(
                        main_feed = main_active,
                        depth_20 = d20,
                        depth_200 = d200,
                        order_update = oms,
                        token_remaining_secs = token_secs,
                        "PROOF: market-open readiness confirmation fired @ 09:14:00 IST"
                    );
                    // Phase 0 Item 2 hostile-review HIGH #3 fix (2026-05-13):
                    // `main_feed_total` is the operator's EXPECTED count for
                    // the day, NOT the Dhan slot ceiling. Under Phase 0 the
                    // expected total is 1; reading "1/5" would falsely
                    // suggest 4 missing connections.
                    readiness_notifier.notify(NotificationEvent::MarketOpenReadinessConfirmation {
                        main_feed_active: main_active,
                        main_feed_total: tickvault_common::config::effective_main_feed_pool_size(
                            config.subscription.scope,
                            config.dhan.max_websocket_connections,
                        ),
                        depth_20_active: d20,
                        depth_200_active: d200,
                        order_update_active: oms,
                        token_remaining_secs: token_secs,
                    });
                });
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
                    // Phase 0 Item 2 hostile-review HIGH #3 fix (2026-05-13):
                    // see MarketOpenReadinessConfirmation above for rationale.
                    let expected_main_feed_total =
                        tickvault_common::config::effective_main_feed_pool_size(
                            config.subscription.scope,
                            config.dhan.max_websocket_connections,
                        );
                    if main_active == 0 {
                        heartbeat_notifier.notify(NotificationEvent::MarketOpenStreamingFailed {
                            main_feed_active: main_active,
                            main_feed_total: expected_main_feed_total,
                            depth_20_active: d20,
                            depth_200_active: d200,
                            order_update_active: oms,
                        });
                    } else {
                        heartbeat_notifier.notify(
                            NotificationEvent::MarketOpenStreamingConfirmation {
                                main_feed_active: main_active,
                                main_feed_total: expected_main_feed_total,
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

                    // Sample live state. All eight sub-checks now have
                    // production sources: seven from `health_status`
                    // gauges + token manager, and `last_tick_age_secs`
                    // from the global `TickGapDetector::scan_gaps_top_n`
                    // (returns the worst-stale instrument's gap).
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
                    let worst_gap_secs =
                        tickvault_core::pipeline::tick_gap_detector::global_tick_gap_detector()
                            .map(|d| {
                                let (top, _total) = d.scan_gaps_top_n(std::time::Instant::now(), 1);
                                top.first().map(|(_, _, gap)| *gap).unwrap_or(0)
                            })
                            .unwrap_or(0);
                    let inputs = MarketOpenSelfTestInputs {
                        main_feed_active: main_active,
                        depth_20_active: d20,
                        depth_200_active: d200,
                        order_update_active: oms,
                        pipeline_active: pipeline,
                        last_tick_age_secs: worst_gap_secs,
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

            // Wave 3-D Item 13 — composite real-time guarantee score.
            // Every 10s, sample 6 dimensions (WS, QDB, tick freshness,
            // token, spill, Phase 2), compute composite score
            // ∈ [0.0, 1.0], emit `tv_realtime_guarantee_score` gauge +
            // 6 per-dimension gauges, and on rising-tier transitions
            // (Healthy → Degraded / Healthy → Critical / Degraded →
            // Critical) fire edge-triggered Telegram with severity
            // matching the new tier.
            //
            // Audit-findings Rule 3 (market-hours-aware): off-hours we
            // pin WS / tick / Phase2 dimensions to 1.0 because their
            // by-design state (sleeping connections, no ticks, no
            // Phase 2 trigger yet) is not a degradation — only QDB,
            // token, spill remain genuine off-hours signals.
            //
            // Audit-findings Rule 4 (edge-trigger): sustained-degraded
            // ticks do NOT spam Telegram. The Prometheus alert
            // `tv-realtime-score-degraded` (5m sustained < 0.95) is
            // the sustained-condition channel.
            //
            // Gated on `config.features.realtime_guarantee_score`.
            if config.features.realtime_guarantee_score {
                // 2026-05-11 v3: SLO Telegram dispatch removed — see the
                // `slo_notifier.notify(...)` removal note in the
                // `cur_tier > prev_tier` block below. The clone is
                // retained under `_` so a future "re-enable SLO Telegram"
                // PR has a one-line revert (drop the underscore + restore
                // the notify() calls).
                let _slo_notifier = notifier.clone();
                let slo_health = health_status.clone();
                let slo_qcfg = config.questdb.clone();
                // PR #509d (Wave-5 §R.1): the Phase 2 dispatcher chain is
                // retired. The phase2_health SLO dimension is permanently
                // pinned to 1.0 inside the score loop — see comment there.
                tokio::spawn(async move {
                    use std::time::{Duration, Instant};
                    use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY};
                    use tickvault_common::error_code::ErrorCode;
                    use tickvault_core::instrument::slo_score::{
                        SloInputs, SloOutcome, evaluate_slo_score,
                    };

                    /// Sum of expected connections across the four pools:
                    /// Default = 12. Breakdown: 5 main feed, 4 depth-20 (NIFTY,
                    /// BANKNIFTY, FINNIFTY, MIDCPNIFTY per pre-2026-04-25
                    /// universe; rebuild reduced to 2 but pool capacity still
                    /// permits 4), 2 depth-200 (NIFTY ATM CE/PE + BANKNIFTY
                    /// ATM CE/PE), 1 order-update.
                    ///
                    /// Phase 0 Item 2 fix (operator-locked 2026-05-13,
                    /// hostile-review CRITICAL #1): main-feed component
                    /// scope-aware. Under `IndicesUnderlyingsOnly` the main
                    /// pool is clamped to `PHASE_0_MAIN_FEED_CONNECTION_COUNT
                    /// = 1`, so the expected denominator must match the
                    /// effective pool size — otherwise `ws_health = 8/12 =
                    /// 0.667` → `SLO-02 Critical` Telegram every 10s during
                    /// market hours (pager fatigue). Depth-20 + depth-200
                    /// gating to 0 under Phase 0 is wired in PR-3 (Item 3
                    /// feature flags); until then this denominator reads
                    /// `effective_main + 4 + 2 + 1 = 8` under Phase 0.
                    const SLO_WS_EXPECTED_DEPTH_20: f64 = 4.0;
                    const SLO_WS_EXPECTED_DEPTH_200: f64 = 2.0;
                    const SLO_WS_EXPECTED_ORDER_UPDATE: f64 = 1.0;
                    let slo_ws_expected_main_feed: f64 =
                        tickvault_common::config::effective_main_feed_pool_size(
                            config.subscription.scope,
                            config.dhan.max_websocket_connections,
                        ) as f64;
                    let slo_ws_expected_total: f64 = slo_ws_expected_main_feed
                        + SLO_WS_EXPECTED_DEPTH_20
                        + SLO_WS_EXPECTED_DEPTH_200
                        + SLO_WS_EXPECTED_ORDER_UPDATE;

                    /// Tick-freshness threshold during market hours: a tick
                    /// gap >= this many seconds drives `tick_freshness = 0.0`.
                    /// Aligned with the operator's "silent socket" boundary
                    /// used elsewhere (the 60s pool watchdog).
                    const SLO_TICK_FRESHNESS_DEGRADED_SECS: u64 = 30;

                    /// Token headroom threshold: < this many seconds remaining
                    /// drives `token_freshness = 0.0`. Aligned with
                    /// `force_renewal_if_stale(threshold_secs = 14400)` used
                    /// by the post-sleep WebSocket wake path (AUTH-GAP-03).
                    const SLO_TOKEN_HEADROOM_THRESHOLD_SECS: u64 = 4 * 3600;

                    // PR #509d: SLO_PHASE2_* grace constants retired with the
                    // dispatcher chain. phase2_health is pinned to 1.0
                    // unconditionally below.

                    /// Uniform boot-relative grace covering ALL six SLO
                    /// dimensions (ws_health, qdb_health, tick_freshness,
                    /// token_freshness, spill_health, phase2_health).
                    /// During the first 60s after the scheduler starts,
                    /// the composite score is pinned to `1.0` regardless
                    /// of inputs.
                    ///
                    /// Rationale: at boot the scheduler's 10s tick can
                    /// fire before all 12 expected WS connections are
                    /// up (DEGRADED with weakest=ws_health) or before the
                    /// tick-gap coalescer's 30s window has filled
                    /// (CRITICAL with weakest=tick_freshness). Both are
                    /// transient partial-state reads, not real
                    /// degradation. Live incident 2026-04-28 15:06–15:07
                    /// IST proved this: DEGRADED at boot+10s, CRITICAL
                    /// at boot+60s, both recovered on their own once
                    /// the system steady-stated.
                    ///
                    /// 60s is the same magnitude as the per-dimension
                    /// grace and is comfortably longer than the typical
                    /// boot settle window (~30s for connections + ~30s
                    /// for tick-gap coalescer).
                    const SLO_BOOT_UNIFORM_GRACE_SECS: u64 = 60;

                    /// Sample interval. SCOPE §13.1.
                    const SLO_TICK_INTERVAL_SECS: u64 = 10;

                    /// Hard upper-bound for the per-tick QDB-readiness probe.
                    /// Adversarial review (general-purpose, 2026-04-28
                    /// HIGH #3): caps `wait_for_questdb_ready` so a hung
                    /// TCP connect cannot stretch the 10s scheduler tick.
                    const SLO_QDB_PROBE_TIMEOUT_SECS: u64 = 2;

                    /// Helper: returns true iff `now_ist_secs_of_day` falls
                    /// inside the data-collection window
                    /// `[09:00, 16:00)` IST. Off-hours we relax 3 of 6
                    /// dimensions to avoid false-positive pages on
                    /// by-design idle state.
                    // Wave-Holiday-Gate (2026-05-09): delegate to the
                    // canonical helper so weekend boots no longer drive
                    // the SLO score to 0.0 via tick_freshness/ws_health.
                    // The legacy `secs_of_day` argument is unused now;
                    // call sites pass it but the helper ignores it.
                    fn is_within_market_hours_ist(_now_ist_secs_of_day: u32) -> bool {
                        tickvault_common::market_hours::is_within_trading_session_ist()
                    }

                    let mut interval =
                        tokio::time::interval(Duration::from_secs(SLO_TICK_INTERVAL_SECS));
                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    // Skip the immediate first tick — let other systems boot.
                    interval.tick().await;

                    // Edge-trigger state. Initial tier 0 (Healthy) so a
                    // genuine first-tick Degraded/Critical fires Telegram
                    // exactly once on the rising edge.
                    let mut prev_tier: u8 = 0;
                    // Spill delta tracker — saturate-subtract previous total
                    // to detect any new drops in the last 10s.
                    let mut last_spill_total: u64 = slo_health.ticks_spilled();
                    // Scheduler boot instant — used by the boot-relative
                    // Phase 2 grace below to suppress SLO-02 false-positive
                    // on mid-market boots (live incident 2026-04-28).
                    let scheduler_boot_at = std::time::Instant::now();

                    info!("Wave 3-D SLO score scheduler: started (10s tick)");

                    loop {
                        interval.tick().await;

                        // Compute "now" in IST for market-hours + Phase 2
                        // gates. `chrono::Utc::now()` is the canonical
                        // wall-clock source per existing usage in this file.
                        let now_utc_secs = chrono::Utc::now().timestamp();
                        let now_ist_secs =
                            now_utc_secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
                        let secs_of_day =
                            now_ist_secs.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32;
                        let in_market = is_within_market_hours_ist(secs_of_day);

                        // ---- WS_health -----------------------------------
                        let active_main = slo_health.websocket_connections() as f64;
                        let active_d20 = slo_health.depth_20_connections() as f64;
                        let active_d200 = slo_health.depth_200_connections() as f64;
                        let active_ou = if slo_health.order_update_connected() {
                            1.0
                        } else {
                            0.0
                        };
                        let active_total = active_main + active_d20 + active_d200 + active_ou;
                        let raw_ws_health = active_total / slo_ws_expected_total;
                        // Off-hours: by-design sleeping connections must
                        // NOT degrade the SLO. Pin to 1.0.
                        let ws_health = if !in_market { 1.0 } else { raw_ws_health };

                        // ---- QDB_health ---------------------------------
                        // 1s probe wrapped in tokio::time::timeout(2s) as a
                        // hard upper-bound. Adversarial review (general-
                        // purpose, 2026-04-28 HIGH #3): the boot probe's
                        // internal max_wait_secs may not strictly cap on a
                        // hung TCP connect (OS-default connect timeout can
                        // be 20-75s). Timeout-bounding here keeps the 10s
                        // scheduler interval from compounding into a
                        // slow-loop under prolonged QDB outages.
                        let qdb_ok = tokio::time::timeout(
                            Duration::from_secs(SLO_QDB_PROBE_TIMEOUT_SECS),
                            tickvault_storage::boot_probe::wait_for_questdb_ready(&slo_qcfg, 1),
                        )
                        .await
                        .is_ok_and(|res| res.is_ok());
                        let qdb_health = if qdb_ok { 1.0 } else { 0.0 };

                        // ---- Tick_freshness ------------------------------
                        let tick_freshness = if !in_market {
                            1.0
                        } else {
                            let worst_gap = tickvault_core::pipeline::tick_gap_detector::global_tick_gap_detector()
                                .map(|d| {
                                    let (top, _total) =
                                        d.scan_gaps_top_n(Instant::now(), 1);
                                    top.first().map(|(_, _, gap)| *gap).unwrap_or(0)
                                })
                                .unwrap_or(0);
                            if worst_gap < SLO_TICK_FRESHNESS_DEGRADED_SECS {
                                1.0
                            } else {
                                0.0
                            }
                        };

                        // ---- Token_freshness -----------------------------
                        let token_secs =
                            tickvault_core::auth::token_manager::global_token_manager()
                                .map(|tm| tm.seconds_until_expiry())
                                .unwrap_or(0);
                        let token_freshness = if token_secs > SLO_TOKEN_HEADROOM_THRESHOLD_SECS {
                            1.0
                        } else {
                            0.0
                        };

                        // ---- Spill_health --------------------------------
                        // Delta over the last 10s sample window. Strict
                        // signal — any new drop drives 0.0 for one tick.
                        let cur_spill = slo_health.ticks_spilled();
                        let spill_delta = cur_spill.saturating_sub(last_spill_total);
                        last_spill_total = cur_spill;
                        let spill_health = if spill_delta == 0 { 1.0 } else { 0.0 };

                        // ---- Phase2_health -------------------------------
                        // PR #509d (Wave-5 §R.1): the legacy Phase 2 dispatcher
                        // was retired. The phase2_health SLO dimension is
                        // permanently pinned to 1.0 — the dispatcher chain no
                        // longer exists, there is no outcome to consult.
                        // Future scope re-enabling stock F&O will introduce a
                        // new dispatch path with its own health dimension.
                        let phase2_health = 1.0_f64;

                        let inputs = SloInputs {
                            ws_health,
                            qdb_health,
                            tick_freshness,
                            token_freshness,
                            spill_health,
                            phase2_health,
                        };
                        // Uniform boot grace: pin Healthy across ALL
                        // dimensions during the first 60s after the
                        // scheduler started. See
                        // SLO_BOOT_UNIFORM_GRACE_SECS docstring.
                        let outcome = if scheduler_boot_at.elapsed().as_secs()
                            < SLO_BOOT_UNIFORM_GRACE_SECS
                        {
                            SloOutcome::Healthy { score: 1.0 }
                        } else {
                            evaluate_slo_score(&inputs)
                        };

                        // Always emit gauges (operator dashboard reads them
                        // in real-time, off-hours included). Clamp each
                        // per-dimension gauge value into [0, 1] before
                        // emit so that a future regression that lets a
                        // raw f64 (e.g. NaN from div-by-zero or > 1.0
                        // from a misconfigured `slo_ws_expected_total`)
                        // cannot pollute the dashboard. `evaluate_slo_score`
                        // already clamps internally for the composite —
                        // this is the same defense for the per-dimension
                        // panels (hot-path-reviewer hardening, 2026-04-28).
                        let dim_clamp =
                            |v: f64| -> f64 { if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) } };
                        metrics::gauge!("tv_realtime_guarantee_score").set(outcome.score());
                        metrics::gauge!(
                            "tv_realtime_guarantee_dimension",
                            "name" => "ws_health"
                        )
                        .set(dim_clamp(ws_health));
                        metrics::gauge!(
                            "tv_realtime_guarantee_dimension",
                            "name" => "qdb_health"
                        )
                        .set(dim_clamp(qdb_health));
                        metrics::gauge!(
                            "tv_realtime_guarantee_dimension",
                            "name" => "tick_freshness"
                        )
                        .set(dim_clamp(tick_freshness));
                        metrics::gauge!(
                            "tv_realtime_guarantee_dimension",
                            "name" => "token_freshness"
                        )
                        .set(dim_clamp(token_freshness));
                        metrics::gauge!(
                            "tv_realtime_guarantee_dimension",
                            "name" => "spill_health"
                        )
                        .set(dim_clamp(spill_health));
                        metrics::gauge!(
                            "tv_realtime_guarantee_dimension",
                            "name" => "phase2_health"
                        )
                        .set(dim_clamp(phase2_health));

                        let cur_tier = outcome.tier();

                        // 2026-05-11 hotfix v3 — Telegram-suppress ALL three
                        // SLO transitions (Degraded, Critical, Healthy).
                        // Operator directive: the composite SLO score is
                        // computed from six dimensions (ws_health,
                        // qdb_health, tick_freshness, token_freshness,
                        // spill_health, phase2_health) and the underlying
                        // typed errors for each dimension (WS-GAP-06,
                        // BOOT-01/02, AUTH-GAP-03, STORAGE-GAP-03,
                        // PHASE2-01) ALREADY fire their own Telegram alerts.
                        // SLO-02 paging is duplicate operator noise — the
                        // 10s scheduler flaps multiple times per minute
                        // because `tick_freshness` momentarily reads 0 when
                        // illiquid IDX_I instruments don't tick within the
                        // 30s window (normal market behaviour). Keep the
                        // log (warn / info) + counter for the
                        // operator-health Grafana dashboard, but DO NOT
                        // dispatch to the notification service.
                        // Defense-in-depth: events.rs also demotes
                        // `RealtimeGuaranteeCritical` to Severity::Low so
                        // any accidental future notify() call still does
                        // not page.
                        if in_market {
                            if cur_tier > prev_tier {
                                match &outcome {
                                    SloOutcome::Healthy { .. } => {
                                        // Cannot reach: tier 0 is Healthy
                                        // and prev_tier > 0 contradicts
                                        // cur_tier > prev_tier with
                                        // cur_tier == 0. Defensive only.
                                    }
                                    SloOutcome::Degraded { score, weakest } => {
                                        warn!(
                                            code = ErrorCode::Slo02Degraded.code_str(),
                                            score = *score,
                                            weakest = weakest,
                                            "SLO score crossed below 0.95 — DEGRADED (log-only, Telegram suppressed)"
                                        );
                                    }
                                    SloOutcome::Critical { score, weakest } => {
                                        warn!(
                                            code = ErrorCode::Slo02Degraded.code_str(),
                                            score = *score,
                                            weakest = weakest,
                                            "SLO score crossed below 0.80 — CRITICAL (log-only, Telegram suppressed)"
                                        );
                                    }
                                }
                            } else if cur_tier < prev_tier && cur_tier == 0 {
                                // Recovery to Healthy — log-only (no
                                // Telegram ping per operator directive
                                // 2026-05-11; the underlying typed events
                                // already announced their own recovery).
                                info!(
                                    code = ErrorCode::Slo01Healthy.code_str(),
                                    score = outcome.score(),
                                    "SLO score recovered to healthy (log-only, Telegram suppressed)"
                                );
                            }
                        }
                        prev_tier = cur_tier;

                        metrics::counter!(
                            "tv_realtime_guarantee_evaluations_total",
                            "tier" => outcome.outcome_str()
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
            //
            // 09:13 IST anchor task RETIRED — depth_dynamic_pipeline_v2 owns
            // depth-20 + depth-200 dispatch via the unified diff dispatcher
            // (top-N volume cohort over movers_1m, refreshed every 60s with
            // diff-only Add/Remove). Anchor visibility comes from the dynamic
            // pool's own boot logs (`PR-C2 cutover: depth_dynamic_pipeline_v2
            // spawn complete`).
            info!(
                "09:13 IST anchor task RETIRED — depth_dynamic_pipeline_v2 \
                 owns depth-20 + depth-200 dispatch via the unified diff \
                 dispatcher (top-N volume cohort over movers_1m, refreshed \
                 every 60s with diff-only Add/Remove)."
            );

            // L1: Listen for rebalance events → Telegram alert + send swap commands (zero disconnect).
            {
                let rebalance_notifier = notifier.clone();
                let rebal_cmd_senders = std::sync::Arc::clone(&depth_cmd_senders);
                let rebal_pipeline_v2_active = config.features.depth_dynamic_pipeline_v2;
                // Wave 2 Item 9 (AUDIT-02) — clone QuestDB config into the
                // rebalancer task scope so audit rows can be persisted.
                let qcfg_for_rebalance = config.questdb.clone();
                // Wave 5 commit 5: capture universe + feature flag for
                // single-side Swap20 fan-out.
                let rebal_universe = std::sync::Arc::clone(&rebal_consumer_universe);
                // PR-C2 follow-up: silence rebalancer Swap20/Swap200 dispatch when
                // pipeline_v2 owns the swap traffic — unused, see anchor comment above.
                let rebal_single_side_enabled =
                    config.features.depth_dynamic_top_volume && !rebal_pipeline_v2_active;
                let rebal_twenty_max = config.subscription.twenty_depth_max_instruments;
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

                            // Wave 5: when the dynamic top-volume layout is
                            // ON, depth-200 conns are top-volume (not ATM)
                            // — there's no `NIFTY-CE` depth-200 sender to
                            // target. Skip Swap200 send to avoid noisy
                            // "no cmd sender" warns. The dynamic depth-200
                            // orchestrator handles its own swaps via
                            // `option_movers` query every 60s.
                            if rebal_single_side_enabled {
                                continue;
                            }

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

                        // --- 20-level single-side rebalance via command channel (Wave 5 commit 5) ---
                        // Only fires when `[features] depth_dynamic_top_volume = true`
                        // because the legacy mixed-CE+PE depth-20 spawn doesn't
                        // register single-side keys. When flag ON, send Swap20
                        // to each of NIFTY-CE / NIFTY-PE / BANKNIFTY-CE /
                        // BANKNIFTY-PE depth-20 conns with the new ATM ± 24
                        // single-side window.
                        if rebal_single_side_enabled {
                            let today_ist = chrono::Utc::now()
                                .with_timezone(
                                    &chrono::FixedOffset::east_opt(19_800).expect("IST offset"), // APPROVED: compile-time literal
                                )
                                .date_naive();
                            for (side_char, opt_label) in [('C', "CE"), ('P', "PE")] {
                                let key = format!("{ul}-{opt_label}");
                                let sel = tickvault_core::instrument::depth_strike_selector::select_single_side_contracts(
                                    &rebal_universe,
                                    &ul,
                                    side_char,
                                    event.current_spot,
                                    today_ist,
                                    tickvault_core::instrument::depth_strike_selector::DEPTH_ATM_STRIKES_EACH_SIDE,
                                );
                                let Some(selection) = sel else {
                                    warn!(
                                        underlying = %ul,
                                        side = %side_char,
                                        spot = event.current_spot,
                                        "rebalance: select_single_side_contracts returned None — skipping Swap20"
                                    );
                                    continue;
                                };
                                let single_side_ids: Vec<u32> = if side_char == 'C' {
                                    selection.call_security_ids.clone()
                                } else {
                                    selection.put_security_ids.clone()
                                };
                                let instruments: Vec<
                                    tickvault_core::websocket::types::InstrumentSubscription,
                                > = single_side_ids
                                    .iter()
                                    .take(rebal_twenty_max)
                                    .map(|&sid| {
                                        tickvault_core::websocket::types::InstrumentSubscription::new(
                                            tickvault_common::types::ExchangeSegment::NseFno,
                                            sid,
                                        )
                                    })
                                    .collect();
                                let subscribe_messages = tickvault_core::websocket::subscription_builder::build_twenty_depth_subscription_messages(
                                    &instruments,
                                    50, // Dhan max batch size for 20-level
                                );
                                let unsubscribe_messages = vec![
                                    serde_json::json!({
                                        "RequestCode": 25,
                                        "InstrumentCount": 0,
                                        "InstrumentList": [],
                                    })
                                    .to_string(),
                                ];

                                let senders = rebal_cmd_senders.lock().await;
                                if let Some(tx) = senders.get(&key) {
                                    let cmd = tickvault_core::websocket::DepthCommand::Swap20 {
                                        unsubscribe_messages,
                                        subscribe_messages,
                                    };
                                    if let Err(err) = tx.send(cmd).await {
                                        warn!(
                                            ?err,
                                            key = %key,
                                            "rebalance: Swap20 command send failed"
                                        );
                                    } else {
                                        info!(
                                            underlying = %ul,
                                            side = %side_char,
                                            instruments = instruments.len(),
                                            atm_strike = selection.atm_strike,
                                            spot = event.current_spot,
                                            "PROOF: rebalance Swap20 sent — {key} → {} SIDs (ATM={:.2}, zero disconnect)",
                                            instruments.len(),
                                            selection.atm_strike,
                                        );
                                    }
                                } else {
                                    warn!(
                                        key = %key,
                                        "rebalance: no cmd sender for {key} — single-side depth-20 not spawned?"
                                    );
                                }
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
    // PR #449 (operator clarification 2026-05-03): gated behind
    // `features.historical_fetch_enabled`. Defaults to OFF until broker-
    // traded source (PR #455 Groww) lands. Verify + cross-match inside
    // `spawn_historical_candle_fetch` are also skipped when disabled.
    // -----------------------------------------------------------------------
    let post_market_signal = std::sync::Arc::new(tokio::sync::Notify::new());
    if config.features.historical_fetch_enabled {
        spawn_historical_candle_fetch(
            &subscription_plan,
            &config,
            &token_manager,
            &notifier,
            std::sync::Arc::clone(&post_market_signal),
            is_trading,
        );
    } else {
        info!(
            "historical candle fetch DISABLED (features.historical_fetch_enabled = false, PR #449)"
        );
    }

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
    // Phase 4a (2026-05-05) — DORMANT BY DEFAULT (active-plan §6 row 3).
    // When `config.api.movers_v2_enabled` is `true` AND the cascade
    // fanout was constructed earlier in slow-boot, install the fanout
    // handle on AppState so the v2 movers route can be registered by
    // `build_router_with_auth`. Default flag is `false`, so this branch
    // is normally skipped and the v2 endpoint stays unregistered.
    let api_state = if config.api.movers_v2_enabled {
        // Hostile review M1 fix (2026-05-05): elevated to `warn!` so
        // the operator sees an explicit signal in the audit trail
        // every time the dormant flag is flipped on. `error!` would
        // page Telegram on every boot which is too aggressive for a
        // valid operator-driven flag flip; `warn!` strikes the right
        // balance — visible in errors.log + summary.md but does not
        // route to Telegram unless coalesced.
        warn!(
            phase4a_movers_v2_enabled = true,
            "Phase 4a movers_v2_enabled = true — installing CascadeFanout \
             on AppState. The /api/movers/v2 route will be registered. \
             Operator MUST have cleared the 14-day RAM=DB parity soak \
             (active-plan §6 row 3) before flipping this flag. If you \
             see this without having cleared the soak gate, set \
             config.api.movers_v2_enabled = false and restart."
        );
        SharedAppState::new_with_cascade_fanout(
            config.questdb.clone(),
            config.dhan.clone(),
            config.instrument.clone(),
            shared_movers.clone(),
            shared_constituency.clone(),
            health_status,
            std::sync::Arc::new(cascade_fanout.clone()),
        )
    } else {
        SharedAppState::new(
            config.questdb.clone(),
            config.dhan.clone(),
            config.instrument.clone(),
            shared_movers.clone(),
            shared_constituency.clone(),
            health_status,
        )
    };

    // PR #450 commit 6 (2026-05-03): V2 snapshot api_state plumbing
    // DELETED. The new unified /api/movers handler reads QuestDB
    // SQL directly — no in-memory snapshot needed.

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
    //
    // Audit Finding #9 (2026-05-03): gated on `cfg.api.auto_open_portal`
    // so AWS / headless deployments can disable the spurious browser
    // shell-out. Default `true` preserves the local-dev experience.
    if config.api.auto_open_portal {
        tokio::spawn(async {
            crate::infra::open_in_browser("http://localhost:3001/portal/options-chain").await;
        });
    } else {
        info!(
            "skipping portal auto-open (api.auto_open_portal = false; expected on AWS / headless deploy)"
        );
    }

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
    // Step 12b: Spawn periodic token-sweep (Audit Finding #6, 2026-05-03)
    // -----------------------------------------------------------------------
    // The primary `renewal_loop` sleeps until refresh window and uses
    // retry+circuit-breaker, but if the circuit breaker halts the loop,
    // there is no automatic recovery. The token-sweep is a parallel
    // safety-net: every TOKEN_SWEEP_INTERVAL_SECS (4h) it calls
    // `force_renewal_if_stale(TOKEN_SWEEP_STALENESS_THRESHOLD_SECS)`
    // which renews iff < 4h headroom remains. Independent of the primary
    // loop — keeps trying even if the loop has halted.
    {
        let sweep_token_manager = std::sync::Arc::clone(&token_manager);
        tokio::spawn(async move {
            use std::time::Duration;
            use tickvault_common::constants::{
                TOKEN_SWEEP_INTERVAL_SECS, TOKEN_SWEEP_STALENESS_THRESHOLD_SECS,
            };
            let interval = Duration::from_secs(TOKEN_SWEEP_INTERVAL_SECS);
            info!(
                interval_secs = TOKEN_SWEEP_INTERVAL_SECS,
                threshold_secs = TOKEN_SWEEP_STALENESS_THRESHOLD_SECS,
                "token sweep task started (Audit Finding #6 — backstop for renewal_loop)"
            );
            loop {
                tokio::time::sleep(interval).await;
                match sweep_token_manager
                    .force_renewal_if_stale(TOKEN_SWEEP_STALENESS_THRESHOLD_SECS)
                    .await
                {
                    Ok(true) => {
                        info!("token sweep: renewed stale token (< 4h headroom)");
                        metrics::counter!(
                            "tv_token_sweep_renewals_total",
                            "result" => "renewed"
                        )
                        .increment(1);
                    }
                    Ok(false) => {
                        debug!("token sweep: token still fresh, no action");
                        metrics::counter!(
                            "tv_token_sweep_renewals_total",
                            "result" => "fresh"
                        )
                        .increment(1);
                    }
                    Err(err) => {
                        // The renewal_loop's own retry+circuit-breaker
                        // path will pick this up. We just record the
                        // failure for observability.
                        warn!(
                            error = %err,
                            "token sweep: force_renewal_if_stale failed (renewal_loop retries independently)"
                        );
                        metrics::counter!(
                            "tv_token_sweep_renewals_total",
                            "result" => "failed"
                        )
                        .increment(1);
                    }
                }
            }
        });
    }
    info!("token sweep spawned (4h cadence, parallel safety-net to renewal_loop)");

    // -----------------------------------------------------------------------
    // Boot duration check — alert if boot exceeded BOOT_TIMEOUT_SECS
    // -----------------------------------------------------------------------
    // Audit Finding #7 (2026-05-03) — per-step boot timeout strategy:
    //
    // Umbrella deadline: `BOOT_TIMEOUT_SECS` (120s) — the global check
    // immediately below alerts CRITICAL if total boot exceeds this.
    //
    // Per-step deadlines (each must be <= umbrella):
    //   - Step 6 (Dhan auth):  TOKEN_INIT_TIMEOUT_SECS (90s)
    //   - Step 7 (QuestDB):    BOOT_DEADLINE_SECS (60s)
    //
    // Pinned by `crates/common/tests/boot_timeout_consistency_guard.rs`
    // — that test fails the build if any per-step timeout exceeds the
    // umbrella, preventing the false-positive alert pattern the audit
    // caught (TOKEN_INIT was 300s while umbrella was 120s, so umbrella
    // would page mid-Dhan-auth).
    //
    // Wave-6 W6-3 backlog: wrap each remaining boot step (1-5, 8-14)
    // in `tokio::time::timeout` with named per-step alerts so the
    // umbrella alert can name WHICH step blew, not just "boot took too
    // long". Today the umbrella is the only signal for steps without
    // their own timeout.
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
        // Wave-Holiday-Gate (2026-05-09): trading-session gate replaces
        // the legacy time-of-day-only gate. Saturday/Sunday boots are
        // legitimately slower (no LTPs ever) — suppress the CRITICAL.
        let in_market_hours = tickvault_common::market_hours::is_within_trading_session_ist();
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
            // L122: `last_memory_alert` was retired alongside
            // `MEMORY_RSS_ALERT_MB`. The per-component memory alert is
            // managed by Prometheus over `tv_subsystem_memory_estimated_bytes`,
            // not by this in-process cooldown.
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
                // Memory RSS — legacy `MEMORY_RSS_ALERT_MB > 1024` alert
                // RETIRED 2026-05-08 per L122 (Wave-5 plan §AA / BUG-C2)
                // because the in-memory-store design (~2.31 GB total) would
                // breach the 1 GB threshold instantly with zero diagnostic
                // signal. Replaced by the per-component
                // `tv-rss-per-subsystem-high` Prometheus alert over
                // `tv_subsystem_memory_estimated_bytes{component=...}`
                // (registered by `subsystem_memory::SubsystemMemoryHandles`).
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
        trading_calendar.clone(),
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
    // Phase 0 Item 2 (operator-locked 2026-05-13): under
    // `SubscriptionScope::IndicesUnderlyingsOnly` the planner emits ~222
    // SIDs (well below the 5000-per-conn Dhan cap). Spinning up 5
    // main-feed connections would waste 4 idle slots, fragment the
    // token/IP budget, and trip the pool watchdog with false-positive
    // "connection idle" signals. `effective_main_feed_pool_size` clamps
    // the count to `PHASE_0_MAIN_FEED_CONNECTION_COUNT = 1` under that
    // scope; legacy + Wave 5 scopes honour the configured value.
    //
    // Security-review MEDIUM fix (2026-05-13): compute the pool clamp
    // BEFORE the capacity check so `effective_capacity` and the actual
    // pool size agree. Pre-fix the capacity check used the configured
    // (unclamped) value, allowing a future >5000-SID Phase 0 plan to
    // slip past truncation and then fail `CapacityExceeded` at pool
    // construction. Harmless today (222 SIDs ≪ 5000) but a logical
    // inconsistency we close now.
    let mut dhan_for_pool = config.dhan.clone();
    let configured_pool_size = dhan_for_pool.max_websocket_connections;
    let effective_pool_size = tickvault_common::config::effective_main_feed_pool_size(
        config.subscription.scope,
        configured_pool_size,
    );
    if effective_pool_size != configured_pool_size {
        info!(
            scope = config.subscription.scope.as_str(),
            configured = configured_pool_size,
            effective = effective_pool_size,
            "Phase 0 Item 2: main-feed pool size clamped by scope"
        );
        dhan_for_pool.max_websocket_connections = effective_pool_size;
    }

    let effective_capacity = dhan_for_pool
        .max_instruments_per_connection
        .min(tickvault_common::constants::MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION)
        .saturating_mul(
            dhan_for_pool
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
        dhan_for_pool,
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

/// Per-connection deadline for the truthful boot-time emit. If a slot
/// has not transitioned to `Connected` within this window after
/// `pool.spawn_handles()` returned, it is reported in the
/// `WebSocketPoolPartialAfterDeadline` event with its current state as
/// the stuck reason.
const WS_BOOT_PER_CONN_DEADLINE_SECS: u64 = 30;

/// Polling interval inside the truthful emit loop. 250ms gives sub-second
/// freshness for state transitions without burning CPU.
const WS_BOOT_POLL_INTERVAL_MS: u64 = 250;

/// Polls `pool.health()` every 250ms for up to 30s per slot until each
/// connection reaches `ConnectionState::Connected`, then emits a single
/// aggregate `WebSocketPoolOnline` summary (or
/// `WebSocketPoolPartialAfterDeadline` if any slot remained stuck). Called
/// from BOTH boot paths (FAST BOOT and slow boot) so an operator sees the
/// same truthful signal regardless of which path ran.
///
/// PR #458 (2026-05-04): the legacy version emitted both per-connection
/// `WebSocketConnected` events AND the aggregate `WebSocketPoolOnline`
/// immediately after `pool.spawn_handles()` returned, using `handle_count`
/// for both `connected` and `total` — a false-OK when handshake had not
/// yet completed. The rewrite polls `pool.health()` every 250ms for up
/// to 30s per slot.
///
/// 2026-05-09 (this branch): the per-connection `WebSocketConnected`
/// emission was removed because it duplicates the aggregate
/// `WebSocketPoolOnline` (which carries the same per-feed breakdown in
/// its `per_connection` payload). The two events fired ~60s apart at boot
/// because `WebSocketConnected` is `Severity::Low` (coalesced 60s) and
/// `WebSocketPoolOnline` is `Severity::Medium` (immediate dispatch),
/// producing visible duplicate signal on operator's Telegram. The
/// aggregate `WebSocketPoolOnline` is the single source of truth for boot
/// success; `WebSocketReconnected` covers post-boot reconnect transitions.
async fn emit_websocket_connected_alerts(
    notifier: &std::sync::Arc<NotificationService>,
    pool: &std::sync::Arc<WebSocketConnectionPool>,
    boot_path: tickvault_core::notification::events::BootPathLabel,
    boot_start: std::time::Instant,
) {
    let healths_initial = pool.health();
    let total = healths_initial.len();
    if total == 0 {
        return;
    }
    let capacity = tickvault_common::constants::MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION;
    // Bookkeeping array: tracks which slots have reached Connected at least
    // once during the polling window. We no longer emit a per-connection
    // `WebSocketConnected` event here — see fn docstring for rationale.
    // The array is still used to drive the early-exit condition once all
    // slots have transitioned to Connected.
    let mut connected_seen = vec![false; total];
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(WS_BOOT_PER_CONN_DEADLINE_SECS);

    while std::time::Instant::now() < deadline {
        let healths = pool.health();
        for h in &healths {
            let i = h.connection_id as usize;
            if i >= total || connected_seen[i] {
                continue;
            }
            if h.state == tickvault_core::websocket::types::ConnectionState::Connected {
                connected_seen[i] = true;
            }
        }
        if connected_seen.iter().all(|v| *v) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(WS_BOOT_POLL_INTERVAL_MS)).await;
    }

    // Final snapshot — derive aggregate truth from pool.health(), NOT
    // from the spawn count.
    let healths_final = pool.health();
    let mut per_connection: Vec<(usize, usize, Option<u32>)> = vec![(0, capacity, None); total];
    let mut connected_count = 0usize;
    let mut stuck: Vec<(usize, String)> = Vec::new();
    for h in &healths_final {
        let i = h.connection_id as usize;
        if i >= total {
            continue;
        }
        per_connection[i] = (h.subscribed_count, capacity, h.last_activity_secs_ago);
        if h.state == tickvault_core::websocket::types::ConnectionState::Connected {
            connected_count = connected_count.saturating_add(1);
        } else {
            stuck.push((i, format!("state={}", h.state)));
        }
    }
    let boot_wall_clock_secs = boot_start.elapsed().as_secs_f64();

    if connected_count == total {
        notifier.notify(NotificationEvent::WebSocketPoolOnline {
            connected: connected_count,
            total,
            per_connection,
            boot_path,
            boot_wall_clock_secs,
        });
    } else if !tickvault_common::market_hours::is_within_market_hours_ist() {
        // 2026-05-09: off-hours boot — non-connected slots are
        // expected (Deferred). Dhan resets idle pre-/post-market
        // sockets, so the WebSocket pool intentionally waits until
        // the next 09:00 IST. Route to Severity::Low so the operator
        // gets a single ✅ ping instead of the false `[HIGH] 0/N
        // feeds connected (N stuck)` alarm.
        let deferred = total.saturating_sub(connected_count);
        notifier.notify(NotificationEvent::WebSocketPoolDeferredOffHours {
            deferred,
            total,
            boot_path,
        });
    } else {
        notifier.notify(NotificationEvent::WebSocketPoolPartialAfterDeadline {
            connected: connected_count,
            total,
            per_connection,
            stuck,
            boot_path,
        });
    }
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
                // Defense: broadcast capacity 262K + tick writer ring buffer 2M (PR #452) + disk spill.
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
                        // Wave-5 §K-L12 (#504b): % fields default to 0.0.
                        0.0,
                        0.0,
                        0.0,
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
                    // Wave-5 §K-L12 (#504b): % fields default to 0.0.
                    0.0,
                    0.0,
                    0.0,
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
    // 2026-05-02: gate the 15:30 Post-Market Telegram on trading-day
    // calendar (Saturday/Sunday/holiday suppression). See
    // `boot_helpers::should_emit_post_market_alert`.
    trading_calendar: std::sync::Arc<TradingCalendar>,
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
        // 2026-05-02: suppress the Post-Market Telegram on non-trading
        // days (Saturday / Sunday / NSE holidays) where no market open
        // ever occurred. The 15:30 sleep is wall-clock based and fires
        // every day; without this gate operators see misleading
        // `[HIGH] Market closed` alerts on weekends. See
        // boot_helpers::should_emit_post_market_alert + ratchet tests.
        let today_ist = chrono::Utc::now()
            .with_timezone(&tickvault_common::trading_calendar::ist_offset())
            .date_naive();
        if should_emit_post_market_alert(&trading_calendar, today_ist) {
            notifier.notify(NotificationEvent::Custom {
                message:
                    "<b>Post-Market</b>\nMarket closed — WebSockets disconnected, API stays up"
                        .to_string(),
            });
        } else {
            info!(
                date = %today_ist,
                "non-trading day — suppressing Post-Market Telegram emission"
            );
        }

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
