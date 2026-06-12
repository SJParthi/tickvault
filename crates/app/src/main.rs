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

// APPROVED: clippy 1.95 tightened these doc-formatting lints; this binary
// crate root predates them. Allow rather than churn doc comments for a
// cosmetic markdown-rendering nicety with zero runtime/behavior impact.
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::doc_overindented_list_items)]

// Modules are declared in lib.rs for coverage instrumentation.
use tickvault_app::boot_helpers::{
    CONFIG_BASE_PATH, CONFIG_LOCAL_PATH, FAST_BOOT_WINDOW_END, FAST_BOOT_WINDOW_START, IstTimer,
    check_clock_drift, compute_market_close_sleep, create_error_log_writer, effective_ws_stagger,
    format_bind_addr, should_emit_post_market_alert, spawn_heartbeat_watchdog,
};
use tickvault_app::{infra, observability, subsystem_memory, trading_pipeline};

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
use tickvault_common::config::SubscriptionScope;
use tickvault_common::instrument_types::FnoUniverse;
use tickvault_common::trading_calendar::{TradingCalendar, ist_offset};
use tickvault_core::auth::secret_manager;
use tickvault_core::auth::token_cache;
use tickvault_core::auth::token_manager::{TokenHandle, TokenManager};
// PR #6b (2026-05-19): binary_cache, InstrumentLoadResult,
// load_or_build_instruments imports retired — universe is now static.
use tickvault_core::instrument::build_subscription_plan;
use tickvault_core::instrument::subscription_planner::SubscriptionPlan;
use tickvault_core::network::ip_verifier;
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_core::pipeline::run_tick_processor;
use tickvault_core::websocket::connection_pool::WebSocketConnectionPool;
use tickvault_core::websocket::order_update_connection::run_order_update_connection;
use tickvault_core::websocket::types::{InstrumentSubscription, WebSocketError};

// PR-E (2026-05-26): ensure_candle_table_dedup_keys retired alongside
// the deleted candle_persistence module + historical_candles table.
// PR #3 (2026-05-19): `greeks_persistence` retired. Migration SQL
// `scripts/migrate-drop-greeks-tables.sql` drops the option_greeks /
// pcr_snapshots / dhan_option_chain_raw / greeks_verification tables.
use tickvault_storage::tick_persistence::{TickPersistenceWriter, ensure_tick_table_dedup_keys};

// PR #3 (2026-05-19): `InlineGreeksComputer` retired alongside the
// trading::greeks module. `build_inline_greeks_enricher` below now
// returns `Option<NoopGreeksEnricher>::None` so the tick processor's
// generic over `GreeksEnricher` keeps compiling without computing.
use tickvault_common::tick_types::NoopGreeksEnricher;

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

// #T2b (2026-05-20): probe_selftest_already_fired_today removed — it
// SELECTed the deleted selftest_audit table for once-a-day dedup. The
// market-open self-test scheduler now relies on its own timing.

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

/// Exit code returned by the `--check-trading-day` gate.
///
/// The holiday-gate shell script (`deploy/aws/holiday-gate.sh`) reads `$?` —
/// NOT stdout (the app denies `print_stdout`/`print_stderr`). Contract:
/// - `0`  = today (IST) is a trading day → let the app start.
/// - `75` = today is a weekend / NSE holiday → gate self-stops the instance.
///
/// Pure so the contract is unit-testable without touching the clock/config.
fn trading_day_gate_exit_code(is_trading_day: bool) -> i32 {
    if is_trading_day { 0 } else { 75 }
}

/// Decides the QuestDB liveness recovery edge: emit `QuestDbReconnected`
/// iff liveness just succeeded AND a `QuestDbDisconnected` was previously
/// alerted. Pure so the edge contract is unit-testable without the clock
/// or a live QuestDB. Edge-triggered per audit-findings Rule 4 — the caller
/// clears the `prev_disconnect_alerted` flag after a true result so the
/// recovery alert fires exactly once per incident.
const fn should_emit_questdb_reconnected(
    prev_disconnect_alerted: bool,
    liveness_success: bool,
) -> bool {
    prev_disconnect_alerted && liveness_success
}

/// `--check-trading-day` short-circuit: load config → build the calendar →
/// evaluate IST today → exit with [`trading_day_gate_exit_code`].
///
/// FAIL-OPEN: any config/calendar load error exits `70`, which the gate script
/// treats as "let the app start" — so the gate can NEVER stop the box on a real
/// trading day because of a transient load failure. Single source of truth: the
/// SAME `config/base.toml` NSE holiday list the app itself uses (no duplication).
fn run_trading_day_gate() -> ! {
    let config: ApplicationConfig = match Figment::new()
        .merge(Toml::file(CONFIG_BASE_PATH))
        .merge(Toml::file(CONFIG_LOCAL_PATH))
        .extract()
    {
        Ok(c) => c,
        // FAIL-OPEN on load error (exit 70 → gate lets the app start).
        Err(_) => std::process::exit(70),
    };
    let calendar = match TradingCalendar::from_config(&config.trading) {
        Ok(c) => c,
        Err(_) => std::process::exit(70),
    };
    let today_ist = (chrono::Utc::now()
        + chrono::TimeDelta::seconds(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64))
    .date_naive();
    std::process::exit(trading_day_gate_exit_code(
        calendar.is_trading_day(today_ist),
    ));
}

#[tokio::main]
async fn main() -> Result<()> {
    // -----------------------------------------------------------------------
    // Step -1: Holiday-gate CLI short-circuit (cold path, no TLS / no runtime).
    // -----------------------------------------------------------------------
    // `tickvault --check-trading-day` is invoked by the boot-time holiday gate
    // (deploy/aws/holiday-gate.sh via the tickvault-holiday-gate.service oneshot)
    // BEFORE the app proper starts. It exits 0 (trading day) / 75 (holiday) /
    // 70 (load error → fail-open). Must run before CryptoProvider install — the
    // gate needs no TLS and exits immediately.
    if std::env::args().any(|a| a == "--check-trading-day") {
        run_trading_day_gate();
    }

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
    // Merge order (last-write-wins): base.toml → config/<env>.toml → local.toml.
    // The per-environment override is selected by TV_ENVIRONMENT (preferred) /
    // ENVIRONMENT, the SAME variable secret_manager uses for the SSM prefix, so
    // config + secrets always agree. For dev/local there is no override file,
    // so this is byte-identical to the previous base + local behaviour.
    let config_env = tickvault_app::boot_helpers::resolve_config_env();
    let mut config_figment = Figment::new().merge(Toml::file(CONFIG_BASE_PATH));
    if let Some(env_path) = tickvault_app::boot_helpers::config_env_path(&config_env) {
        config_figment = config_figment.merge(Toml::file(env_path));
    }
    let config: ApplicationConfig = config_figment
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

    // #T2b (2026-05-20): the periodic 15-min selftest-audit task was
    // removed with the selftest_audit table (QuestDB table cleanup).

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
    // Every raw WS frame (2 types: LiveFeed, OrderUpdate)
    // is appended to an append-only log on disk BEFORE the live try_send to
    // the downstream channel. On boot, any residual WAL segments are
    // replayed so frames captured across a crash are not lost. This backs
    // the zero-tick-loss guarantee while keeping the read loop O(1).
    //
    // Directory layout: $TV_WS_WAL_DIR (defaults to `./data/ws_wal`).
    // Writer thread: background OS thread spawned inside WsFrameSpill::new.
    //
    // Depth-20/Depth-200 paths retired per operator lock 2026-05-15
    // (websocket-connection-scope-lock.md). The Depth20/Depth200 variants
    // remain in `ws_frame_spill::WsType` as orphan; any replayed records
    // of those types are silently dropped here.
    // -----------------------------------------------------------------------
    // Single source of truth for the WAL dir (hostile-review H1: shared
    // with the 15:40 conservation audit so the two sites can never drift).
    let ws_wal_path = tickvault_app::tick_conservation_boot::ws_wal_dir();
    let ws_wal_dir = ws_wal_path.display().to_string(); // O(1) EXEMPT: boot-time
    // Replay first — this MUST happen before any WS connection opens so we
    // never race a fresh append against a stale segment rotation.
    // TICK-SEQ-01: carry each replayed frame's `frame_seq` so re-injected
    // frames reuse the SAME capture sequence as their original live write
    // (replay-stable). v1 records replay with frame_seq=0.
    let mut ws_wal_replay_live_feed: Vec<(u64, bytes::Bytes)> = Vec::new();
    let mut ws_wal_replay_order_update: Vec<Vec<u8>> = Vec::new();
    match tickvault_storage::ws_frame_spill::replay_all(&ws_wal_path) {
        Ok(recovered) => {
            if recovered.is_empty() {
                info!(dir = %ws_wal_dir, "STAGE-C: WAL replay — no residual frames");
            } else {
                let mut live = 0u64;
                let mut ord = 0u64;
                for rec in recovered {
                    match rec.ws_type {
                        tickvault_storage::ws_frame_spill::WsType::LiveFeed => {
                            live += 1;
                            ws_wal_replay_live_feed
                                .push((rec.frame_seq, bytes::Bytes::from(rec.frame)));
                        }
                        tickvault_storage::ws_frame_spill::WsType::OrderUpdate => {
                            ord += 1;
                            ws_wal_replay_order_update.push(rec.frame);
                        }
                    }
                }
                info!(
                    dir = %ws_wal_dir,
                    total = live + ord,
                    live_feed = live,
                    order_update = ord,
                    "STAGE-C: WAL replay recovered residual frames — LiveFeed will be \
                     re-injected into pool mpsc; OrderUpdate drained into broadcast once \
                     sender is created"
                );
                metrics::counter!("tv_ws_frame_wal_replay_total", "ws_type" => "live_feed")
                    .increment(live);
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
                "STAGE-C: failed to initialize WsFrameSpill — HALTING boot (fail-closed). \
                 The durable WAL is the zero-tick-loss guarantee (ring → spill → WAL); \
                 running WITHOUT it would admit SILENT frame loss under channel \
                 backpressure. Fix disk permissions/free space, then restart."
            );
            // Zero-tick-loss fail-closed (operator mandate 2026-06-02: "ticks
            // should never ever be lost … irrespective of any situation").
            // The WAL is the durable floor of the ring → spill → WAL chain; if it
            // can't init, the guarantee is void, so we REFUSE to run. systemd
            // Restart=always re-launches and the operator is paged by the ERROR
            // above — a loud restart loop beats a silent lossy session.
            std::process::exit(1);
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
    // PR #6a (2026-05-19): --instrument-diagnostic CLI flag RETIRED
    // (4-IDX_I LOCKED_UNIVERSE — diagnostic.rs module deleted; no CSV
    // download/parse/validate cycle to diagnose).

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
    // PR #6b (2026-05-19): inlined `is_within_build_window` after retiring
    // instrument_loader.rs. The window (09:00:00..15:30:00 IST) matches the
    // tick-persist window per audit-findings Rule 3.
    let is_market_hours = trading_calendar.is_trading_day_today() && {
        use tickvault_common::constants::{
            IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST,
            TICK_PERSIST_START_SECS_OF_DAY_IST,
        };
        let _ = (FAST_BOOT_WINDOW_START, FAST_BOOT_WINDOW_END); // keep imports live until removal in later slice
        let now_utc = chrono::Utc::now().timestamp();
        let now_ist = now_utc.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
        let sec_of_day = now_ist.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32;
        (TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST).contains(&sec_of_day)
    };

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
            load_instruments(&config, is_trading, trading_calendar.as_ref()).await?;

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
        // PR #2 (2026-05-18): `shared_movers` snapshot retired alongside
        // the deleted `top_movers` / `option_movers` modules.

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
            // Candle-engine re-architecture #T1b: Engine A (the legacy 1s
            // `CandleAggregator` → `candles_1s` path) DELETED. The
            // multi-TF aggregator (Engine B) is the only candle engine.
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
            let _ = fast_registry; // PR #2: instrument_registry was used by deleted movers persistence helpers
            let handle = tokio::spawn(async move {
                run_tick_processor(
                    receiver,
                    fast_tick_writer,
                    tick_broadcast_for_processor,
                    greeks_enricher,
                    Some(fast_tick_heartbeat),
                    None, // tick_enricher — Phase 2.5 wiring deferred until prev_oi_cache + boot ordering gate land in slow boot
                    tickvault_common::always_on::current(), // §30 GIFT exemption
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
                // Candle-engine re-architecture #T1b — drop legacy candle
                // objects before creating Engine B's plain tables (see the
                // slow-boot path for the full rationale).
                tickvault_storage::shadow_persistence::drop_legacy_candle_objects(&config.questdb)
                    .await;
                tokio::join!(
                    ensure_tick_table_dedup_keys(&config.questdb),
                    // PR-E (2026-05-26): historical_candles table retired.
                    tickvault_storage::shadow_persistence::ensure_shadow_candle_tables(
                        &config.questdb
                    ),
                    // Candle-engine re-architecture #T1b: `ensure_candle_views`
                    // (Engine C matview cascade) RETIRED.
                    // PR #3 (2026-05-19): `ensure_greeks_tables` retired.
                    // #T4 (2026-05-20): `ensure_depth_and_prev_close_tables`
                    // + `calendar_persistence` retired.
                );

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
            // TICK-SEQ-01 PR-2b: fast boot's `run_tick_processor` (started with a
            // lossless `new_disconnected` writer above) is now the SOLE seq-correct
            // ticks persister — exactly matching slow boot. The old
            // `run_tick_persistence_consumer` was a redundant SECOND persister
            // (broadcast-fed, lossy on lag, seq-LESS) that, under the new
            // `capture_seq` dedup key, wrote every tick with a DIFFERENT seq than
            // the hot-path writer → duplicate rows. We reuse the existing slow-boot
            // observability consumer so fast boot KEEPS the gap-tracking +
            // QuestDB-health-poll surface WITHOUT a second writer (no blind spot,
            // no duplicates).
            let observability_rx = fast_tick_broadcast_sender.subscribe();
            let questdb_cfg = config.questdb.clone();
            tokio::spawn(async move {
                run_slow_boot_observability(observability_rx, questdb_cfg).await;
            });
            info!(
                "fast-boot observability consumer started (gap-track + QuestDB health; \
                 single seq-correct persister owns ticks)"
            );
        }

        // Candle-engine re-architecture #T1b: the cold-path Engine-A
        // candle-persistence consumer (`run_candle_persistence_consumer`
        // → `candles_1s`) DELETED. Engine B (the multi-TF aggregator
        // subscriber, wired below) is the sole candle path; it runs in
        // both fast and slow boot.

        // --- Background: Greeks pipeline (option chain fetch → compute → persist) ---
        //
        // PR #3 (2026-05-19): greeks pipeline RETIRED. Under the 4-IDX_I
        // LOCKED_UNIVERSE there are no live option contracts on the
        // WebSocket to compute Greeks from. Option Chain REST overlay
        // (PR #8) ships Dhan-computed greeks separately.
        info!("greeks pipeline retired (PR #3)");

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

        // FAST-BOOT candle-sealing fix (2026-06-01): the seal-writer loop
        // publishes the GLOBAL seal Sender the aggregator needs to emit
        // candles. It was wired in the SLOW boot path ONLY, so FAST BOOT
        // captured ticks but sealed ZERO candles (the candles_1m=0 bug —
        // `global_seal_sender()` was None, so the aggregator's per-tick
        // closure skipped every tick via `else { continue }`). Wire it here
        // too, BEFORE the aggregator, so the Sender is published when the
        // first tick folds. set_global_seal_sender is idempotent (first wins).
        spawn_seal_writer_loop(&config.questdb);

        // Candle-engine re-architecture #T1b — wire Engine B (the only
        // candle engine: 21-TF aggregator → 21 plain `candles_<tf>`
        // tables). Subscriber + heartbeat + IST-midnight force-seal
        // tasks all live in the shared `spawn_engine_b_aggregator` helper.
        spawn_engine_b_aggregator(
            &fast_tick_broadcast_sender,
            std::sync::Arc::clone(&prev_day_cache),
            std::sync::Arc::clone(&trading_calendar),
        );

        // --- Background: Index constituency (best-effort) ---
        // During market hours, skip network downloads to niftyindices.com
        // (they often return HTML instead of CSV) and use the cached JSON.
        // PR #6a (2026-05-19): index-constituency loader RETIRED under
        // 4-IDX_I LOCKED_UNIVERSE (operator lock 2026-05-15). NSE index
        // composition (which stocks are in NIFTY, etc.) isn't needed when
        // only the 4 indices themselves are tracked.

        // --- Background: API server ---
        let api_state = SharedAppState::new(
            config.questdb.clone(),
            config.dhan.clone(),
            config.instrument.clone(),
            // clone (not move) so the post-market task spawn below can also hold it
            health_status.clone(),
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

        // PR-C (2026-05-26): Dhan historical fetch chain DELETED entirely
        // per operator directive — pre-market buffer + gap_fill +
        // cross_verify chains all retired. The runtime is now spot-only
        // NIFTY 50 strategy (no VWAP, no futures, no historical fetch).

        notifier.notify(NotificationEvent::Custom {
            message: "<b>FAST BOOT</b>\nCrash recovery: ticks flowing, all services ready"
                .to_string(),
        });

        // CRITICAL: tell systemd the service is up. The unit is Type=notify, so
        // systemd kills the process at TimeoutStartSec (default 90s) unless it
        // receives sd_notify(READY=1). The slow-boot path sends this near the
        // end of main(), but the fast-boot (crash-recovery) path returns into
        // run_shutdown_fast() below and never reaches that call site. Without
        // this line the fast-boot path was SIGTERM'd every 90s, restarted into
        // crash-recovery (cached token + market hours), and looped forever —
        // which is exactly why ticks never persisted on AWS. No-op when
        // NOTIFY_SOCKET is unset (e.g. local `cargo run`).
        infra::notify_systemd_ready();

        // Boot-symmetry fix (2026-06-09): the post-market 15:31 cross-verify +
        // 15:31:30 EOD digest were slow-boot-only, so a mid-session crash restart
        // (this fast-boot path) silently skipped both. Spawn the same tasks here.
        // The EOD digest always runs. The cross-verify reads the globally-stashed
        // DailyUniverse (stashed inside cold_build_daily_universe, which the cold
        // fast-boot path also goes through); the INSTANT warm-snapshot fast-path
        // builds no DailyUniverse object, so cross-verify there finds none and
        // self-skips (honest limitation — warm-path cross-verify is a follow-up).
        // Each task also self-skips if past its IST trigger or on a non-trading day.
        spawn_post_market_tasks(
            notifier.clone(),
            std::sync::Arc::clone(&health_status),
            std::sync::Arc::clone(&trading_calendar),
            std::sync::Arc::clone(&token_handle),
            &config,
        );

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
    // Step 6a-prime: Dual-instance SSM lock (Phase 0 Item 19)
    // -----------------------------------------------------------------------
    // RESILIENCE-01: only ONE tickvault process per Dhan client-id may
    // ever be live. Two processes against the same account fight over
    // static-IP enforcement (Item 18), fragment the 5-conn WebSocket
    // budget, and silently break order reconciliation. We hold an
    // SSM-Parameter-Store-backed lock (90s TTL, 30s heartbeat) for the
    // lifetime of the process; this gate fails the boot if another
    // live peer is already holding it. SSM is the source of truth so
    // an AWS prod instance and a dev Mac sharing the same env name
    // cannot accidentally run in parallel.
    //
    // Runs AFTER auth (Step 6) so the boot-halt Telegram + SNS path
    // is fully wired. Runs BEFORE Step 6a static IP boot gate so we
    // don't burn Dhan API quota on a peer-side race we'd lose anyway.
    //
    // Like the static IP gate, this is live-mode only — sandbox/paper
    // modes don't subscribe to depth or place real orders, so a second
    // sandbox instance is not a regulatory hazard.
    // Phase 0 Item 19f — chain bridge from the broader `shutdown_notify`
    // (constructed at Step 8b below) to the heartbeat's own `Notify`.
    // `None` outside live mode or when the lock acquire path skips the
    // heartbeat spawn.
    let mut instance_lock_shutdown_chain: Option<std::sync::Arc<tokio::sync::Notify>> = None;
    let instance_lock_handle: Option<tokio::task::JoinHandle<()>> = if trading_mode.is_live() {
        info!("Phase 0 Item 19: acquiring dual-instance SSM lock");

        // SSM is the source of truth for the lock (operator lock
        // 2026-05-24 — replaced the earlier in-memory KV implementation
        // alongside the CloudWatch-only migration). Constructing the
        // client here keeps the lock self-contained; no auxiliary
        // service needs to be running. Costs: standard-tier SSM
        // PutParameter is free; ~2,880 calls/day at the 30 s heartbeat
        // interval is well under the 40 TPS SSM rate cap.
        let ssm_client = std::sync::Arc::new(
            tickvault_core::auth::secret_manager::create_ssm_client_public().await,
        );

        // host_id composition: pid + boot_random + aws_instance_id (when
        // present). The instance lock path is env-qualified, so dev /
        // sandbox / prod cannot collide.
        let env = tickvault_core::auth::secret_manager::resolve_environment()
            .context("Phase 0 Item 19: cannot resolve environment for lock path")?;
        let host_id = tickvault_core::instance_lock::generate_host_id(
            std::process::id(),
            // Boot-once 64-bit value derived from nanos-since-UNIX-EPOCH.
            // Not cryptographically random, but the goal here is
            // cross-host uniqueness within the 90s TTL window — two
            // boxes booting at the same nanosecond is exceedingly
            // unlikely, and rand isn't a workspace dep.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
            None,
        );
        let lock_key = tickvault_core::instance_lock::compute_instance_lock_path(&env);

        match tickvault_core::instance_lock::try_acquire_instance_lock(&ssm_client, &env, &host_id)
            .await
        {
            Ok(tickvault_core::instance_lock::AcquireOutcome::Acquired) => {
                info!(
                    env = %env,
                    host_id = %host_id,
                    lock_key = %lock_key,
                    ttl_secs = tickvault_core::instance_lock::INSTANCE_LOCK_TTL_SECS,
                    "Phase 0 Item 19: dual-instance lock acquired"
                );
            }
            Ok(tickvault_core::instance_lock::AcquireOutcome::AlreadyHeld { holder }) => {
                error!(
                    code = tickvault_common::error_code::ErrorCode::Resilience01DualInstanceDetected
                        .code_str(),
                    severity = tickvault_common::error_code::ErrorCode::Resilience01DualInstanceDetected
                        .severity()
                        .as_str(),
                    env = %env,
                    host_id = %host_id,
                    lock_key = %lock_key,
                    peer = %holder,
                    "Phase 0 Item 19: another tickvault process holds the lock — BLOCKING BOOT"
                );
                notifier.notify(
                    tickvault_core::notification::events::NotificationEvent::DualInstanceDetected {
                        holder,
                        lock_key,
                    },
                );
                return Ok(());
            }
            Err(err) => {
                // SSM PutParameter / GetParameter failed (network blip,
                // IAM denial, throttle). Same HALT semantics as
                // already-held: we cannot prove there's no peer.
                error!(
                    code = tickvault_common::error_code::ErrorCode::Resilience01DualInstanceDetected
                        .code_str(),
                    severity = tickvault_common::error_code::ErrorCode::Resilience01DualInstanceDetected
                        .severity()
                        .as_str(),
                    error = %err,
                    env = %env,
                    host_id = %host_id,
                    lock_key = %lock_key,
                    "Phase 0 Item 19: SSM acquire-attempt failed — BLOCKING BOOT"
                );
                notifier.notify(
                    tickvault_core::notification::events::NotificationEvent::DualInstanceDetected {
                        holder: format!("(ssm-error: {err})"),
                        lock_key,
                    },
                );
                return Ok(());
            }
        }

        // Lock held — spawn the heartbeat. The heartbeat owns its
        // own `Notify` shutdown source; main.rs's broader
        // `shutdown_notify` (constructed at the Step 8b stage) is
        // chained into it via the bridge task installed further
        // down (Item 19f). On the bridge firing, the heartbeat
        // releases the lock by deleting the SSM Parameter so the next
        // boot sees a clean slate immediately.
        let heartbeat_shutdown_inner = std::sync::Arc::new(tokio::sync::Notify::new());
        let heartbeat_shutdown_for_chain = heartbeat_shutdown_inner.clone();
        let heartbeat_handle = tickvault_core::instance_lock::spawn_instance_lock_heartbeat(
            ssm_client,
            env,
            host_id,
            heartbeat_shutdown_inner,
        );
        instance_lock_shutdown_chain = Some(heartbeat_shutdown_for_chain);
        Some(heartbeat_handle)
    } else {
        info!(
            "Phase 0 Item 19: skipping dual-instance lock (mode={:?} — sandbox/paper do not \
             place real orders)",
            trading_mode
        );
        None
    };
    // Keep the heartbeat handle alive for the lifetime of main.
    // Dropped on return (boot-halt or graceful shutdown). The task
    // itself observes the dropped Notify on shutdown when Item 19e
    // wires the chained shutdown.
    let _instance_lock_handle = instance_lock_handle;

    // -----------------------------------------------------------------------
    // Step 6a: Dhan-side static IP boot gate (Phase 0 Item 18 + 18b)
    // -----------------------------------------------------------------------
    // Step 5.5 above already confirmed our egress IP matches the SSM
    // value (public IP-echo vs SSM). This second gate asks Dhan itself
    // — `/v2/ip/getIP` reports `ordersAllowed` which is the same flag
    // the exchange reads at order-acceptance time. Without this check
    // an IP that looks correct to the world but hasn't propagated to
    // Dhan's whitelist would let boot proceed and then orders would
    // silently be rejected at trade time.
    //
    // Item 18b: when Dhan returns `orders_allowed = false` we retry up
    // to STATIC_IP_BOOT_RETRY_MAX_ATTEMPTS times with a 60s interval
    // (max 30 min). Structural failures (empty response,
    // match_status_not_ok) halt immediately — retrying them would just
    // burn API quota.
    //
    // Skipped outside live mode for the same reason as Step 5.5
    // (sandbox + paper modes do not place real orders).
    if trading_mode.is_live() {
        info!(
            max_attempts = tickvault_common::constants::STATIC_IP_BOOT_RETRY_MAX_ATTEMPTS,
            interval_secs = tickvault_common::constants::STATIC_IP_BOOT_RETRY_INTERVAL_SECS,
            "Phase 0 Item 18+18b: verifying Dhan-side static IP whitelist (/v2/ip/getIP)"
        );
        let access_token = {
            use secrecy::ExposeSecret;
            let guard = token_manager.token_handle().load();
            match guard.as_ref().as_ref() {
                Some(state) => state.access_token().expose_secret().to_string(),
                None => {
                    // The auth step above succeeded, so the token MUST be
                    // present here. If it isn't, fail boot loudly rather
                    // than skip the check.
                    error!(
                        code = tickvault_common::error_code::ErrorCode::GapNetIpMonitor.code_str(),
                        severity = tickvault_common::error_code::ErrorCode::GapNetIpMonitor
                            .severity()
                            .as_str(),
                        "Phase 0 Item 18: token unavailable immediately after auth — BLOCKING BOOT"
                    );
                    notifier.notify(NotificationEvent::StaticIpBootCheckFailed {
                        reason: "empty_response".to_string(),
                        orders_allowed: false,
                        ip_match_status: String::new(),
                        attempts_made: 1,
                    });
                    return Ok(());
                }
            }
        };

        let max_attempts = tickvault_common::constants::STATIC_IP_BOOT_RETRY_MAX_ATTEMPTS;
        let retry_interval = std::time::Duration::from_secs(
            tickvault_common::constants::STATIC_IP_BOOT_RETRY_INTERVAL_SECS,
        );
        let mut attempts_made: u32 = 0;
        let halt_reason: Option<&'static str> = loop {
            attempts_made = attempts_made.saturating_add(1);
            let outcome_result = ip_verifier::verify_static_ip_at_boot(
                &config.dhan.rest_api_base_url,
                &access_token,
            )
            .await;
            let outcome = match outcome_result {
                Ok(o) => o,
                Err(err) => {
                    error!(
                        code = tickvault_common::error_code::ErrorCode::GapNetIpMonitor.code_str(),
                        severity = tickvault_common::error_code::ErrorCode::GapNetIpMonitor
                            .severity()
                            .as_str(),
                        error = %err,
                        attempts_made,
                        "Phase 0 Item 18: /v2/ip/getIP request failed — BLOCKING BOOT"
                    );
                    break Some("empty_response");
                }
            };

            let action = ip_verifier::classify_static_ip_boot_retry_action(
                &outcome,
                attempts_made,
                max_attempts,
            );
            match action {
                ip_verifier::StaticIpBootRetryAction::Pass => {
                    info!(
                        attempts_made,
                        "Phase 0 Item 18: Dhan reports orders allowed from this IP"
                    );
                    let ip_flag =
                        match ip_verifier::get_ip(&config.dhan.rest_api_base_url, &access_token)
                            .await
                        {
                            Ok(resp) => resp.ip_flag,
                            Err(_) => String::new(),
                        };
                    notifier.notify(NotificationEvent::StaticIpBootCheckPassed {
                        ip_flag: ip_flag.clone(),
                    });
                    break None;
                }
                ip_verifier::StaticIpBootRetryAction::Retry { next_attempt } => {
                    info!(
                        next_attempt,
                        max_attempts,
                        "Phase 0 Item 18b: Dhan reports orders not allowed — sleeping before retry"
                    );
                    notifier.notify(NotificationEvent::StaticIpBootCheckRetrying {
                        attempt: next_attempt,
                        max_attempts,
                    });
                    tokio::time::sleep(retry_interval).await;
                }
                ip_verifier::StaticIpBootRetryAction::Halt { reason } => {
                    error!(
                        code = tickvault_common::error_code::ErrorCode::GapNetIpMonitor.code_str(),
                        severity = tickvault_common::error_code::ErrorCode::GapNetIpMonitor
                            .severity()
                            .as_str(),
                        reason,
                        attempts_made,
                        "Phase 0 Item 18: Dhan static IP check FAILED — BLOCKING BOOT"
                    );
                    break Some(reason);
                }
            }
        };

        if let Some(reason_label) = halt_reason {
            // Best-effort re-read of the raw fields for the Telegram
            // payload + audit row. If this also fails the operator
            // still sees the typed reason which is the actionable
            // signal.
            let (orders_allowed, ip_match_status, _ip_flag) =
                match ip_verifier::get_ip(&config.dhan.rest_api_base_url, &access_token).await {
                    Ok(resp) => (resp.orders_allowed, resp.ip_match_status, resp.ip_flag),
                    Err(_) => (false, String::new(), String::new()),
                };
            notifier.notify(NotificationEvent::StaticIpBootCheckFailed {
                reason: reason_label.to_string(),
                orders_allowed,
                ip_match_status,
                attempts_made,
            });
            return Ok(());
        }
    } else {
        info!(
            mode = trading_mode.as_str(),
            "Phase 0 Item 18: Dhan static IP check skipped — not required for {} mode",
            trading_mode.as_str()
        );
    }

    // -----------------------------------------------------------------------
    // Step 6b: Set up QuestDB tick persistence (best-effort)
    // -----------------------------------------------------------------------
    info!("setting up QuestDB tables (ticks + candles + option_chain + historical_candles)");

    // Wave 2 Item 7 (G14) — block until QuestDB is reachable. Escalating
    // logs at +5/+10/+20s; BOOT-01 ERROR @+30s; BOOT-02 HALT @+60s.
    // Prevents the legacy "tick processor starts before QuestDB is up"
    // race that dropped early-boot ticks before this gate existed.
    let _probe_started = std::time::Instant::now();
    let _boot_id = format!(
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

    // Step 6c (daily-universe instrument fetch) moved into `load_instruments`
    // (the `DailyUniverse` scope arm) so the built universe flows straight
    // into the subscription plan. See `load_daily_universe_plan`.

    // Candle-engine re-architecture #T1b — drop the legacy candle objects
    // (Engine A `candles_1s` base table + Engine C's 9 `candles_<tf>`
    // materialized views + the retired Wave-6 `candles_<tf>_shadow`
    // tables) BEFORE `ensure_shadow_candle_tables` creates Engine B's
    // 21 plain `candles_<tf>` tables. The 9 matviews are NAMED
    // `candles_1m` … `candles_1d` — the exact plain-table names — so a
    // surviving matview makes `CREATE TABLE IF NOT EXISTS candles_1m`
    // a silent no-op. Must run sequentially before the join.
    tickvault_storage::shadow_persistence::drop_legacy_candle_objects(&config.questdb).await;

    // All table creation queries are independent — run in parallel for faster boot.
    // NOTE: the boot audit row for the `questdb_ready` step is appended AFTER this
    // join — `ensure_boot_audit_table` lives inside the join (line ~1985) and
    // writing to `boot_audit` before the table exists caused AUDIT-04 to fire on
    // every clean boot until 2026-04-28.
    tokio::join!(
        ensure_tick_table_dedup_keys(&config.questdb),
        // PR-E (2026-05-26): historical_candles table retired.
        // Candle-engine re-architecture #T1b: `ensure_candle_views`
        // (Engine C — `candles_1s` matview cascade) RETIRED. The 21
        // plain `candles_<tf>` tables are created by
        // `ensure_shadow_candle_tables` below; the legacy matviews are
        // dropped by `drop_legacy_candle_objects` before this join.
        // PR #3 (2026-05-19): `ensure_greeks_tables` retired alongside
        // the deleted greeks_persistence module.
        // PR #4 (2026-05-19): `ensure_deep_depth_table` retired alongside
        // the deleted depth-20/200 pipelines (operator lock 2026-05-15).
        // #T4 (2026-05-20): `ensure_depth_and_prev_close_tables`,
        // `calendar_persistence`, `indicator_snapshot_persistence`,
        // `obi_persistence`, `previous_close_persistence` ensure_* calls
        // retired — market_depth / nse_holidays / indicator_snapshots /
        // obi_snapshots / previous_close tables dropped.
        // #T2b (2026-05-20): the 8 audit-table ensure_* calls
        // (ws_reconnect / auth_renewal / boot / selftest / order /
        // gap_fill / last_tick / orphan_position) were removed with
        // their persistence modules in the QuestDB table cleanup.
        // Option-chain minute-snapshot pipeline (2026-05-16, PR #2 of 5).
        // Forensic table for the 3-times-per-minute Dhan option chain
        // fetches that feed BRUTEX strike-selection. One row per
        // (underlying, strike, side, minute). SEBI 5y retention; same
        // 90d-hot → S3 IT → Glacier lifecycle as the other audit tables.
        // Idempotent CREATE TABLE IF NOT EXISTS — safe to call every boot.
        // See `.claude/plans/friday-may-15-mega/topic-OPTION-CHAIN-MINUTE-SNAPSHOT.md`.
        tickvault_storage::option_chain_minute_snapshot_persistence::ensure_option_chain_minute_snapshot_table(
            &config.questdb
        ),
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

    // #T2b (2026-05-20): the boot_audit row write was removed with the
    // boot_audit table (QuestDB table cleanup).

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
    //
    // G3 (zero-tick-loss audit PR-5): run the watcher UNDER A SUPERVISOR
    // (mirrors the WS-GAP-05 pool supervisor) so a panic in the watcher
    // respawns it + logs DISK-WATCHER-01 + increments
    // `tv_disk_watcher_respawn_total` (CloudWatch-alarmed) instead of
    // silently vanishing — previously this handle was bound to `_` and a
    // watcher panic killed disk-free monitoring with no signal.
    let _disk_health_watcher_supervisor =
        tickvault_storage::disk_health_watcher::spawn_supervised_spill_disk_health_watcher(
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

    // #T4 (2026-05-20): depth_writer construction removed — `market_depth`
    // table dropped; the IDX_I-only universe runs Quote mode (no depth).

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
                    // The profile/IP pre-market check is a LIVE-ORDER-TRADING
                    // gate (dataPlan / activeSegment / static-IP must be valid
                    // before placing real orders). In the no-orders DATA-CAPTURE
                    // phase (mode != Live, e.g. Paper) it must NOT block boot —
                    // capturing ticks needs neither a validated profile nor a
                    // static IP, and a HALT here just crash-loops the app and
                    // stops all data capture. Only HALT when we will trade live.
                    if in_market_hours && config.strategy.mode.is_live() {
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
                    // Pre-market window OR non-live (data-capture) mode: log
                    // ERROR (triggers Telegram) but allow boot to CONTINUE so
                    // the market-data feed + tick capture still run. No live
                    // orders are placed in this mode, so a bad profile/IP is
                    // not a trading-safety risk.
                    error!(
                        error = %err,
                        mode = ?config.strategy.mode,
                        "pre-market profile check FAILED — continuing (data-capture mode or pre-09:15; no live orders placed)"
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
        load_instruments(&config, is_trading, trading_calendar.as_ref()).await?;

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

    // Boot-timing proof Telegram (DailyUniverse scope only — sentinel-guarded
    // so Indices4Only emits nothing). Reads the wall-clock stashed by
    // `load_daily_universe_plan`; gives the operator REAL daily evidence the
    // O(1) warm path is working: warm-skip boots are sub-second regardless of
    // the applicable-F&O master size; a full rebuild is the batched-seconds
    // cold path.
    if let Some(message) = format_instrument_load_telegram(
        INSTRUMENT_LOAD_ELAPSED_MS.load(std::sync::atomic::Ordering::Relaxed),
        INSTRUMENT_LOAD_WARM_SKIPPED.load(std::sync::atomic::Ordering::Relaxed),
        INSTRUMENT_LOAD_TOTAL_ROWS.load(std::sync::atomic::Ordering::Relaxed),
    ) {
        notifier.notify(NotificationEvent::Custom { message });
    }

    // Only persist for CachedPlan (not yet persisted). FreshBuild already
    // persisted inside load_or_build_instruments — double-write creates
    // duplicate rows in the same timestamp second.
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
    // PR #2 (2026-05-18): `shared_movers` snapshot retired alongside
    // the deleted `top_movers` / `option_movers` modules.

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

    // PR4 (operator 2026-06-01): bounded prev-day OHLCV fetch. Cold path —
    // fetches yesterday's single daily candle per subscribed SID into the
    // separate `prev_day_ohlcv` table (never `ticks`); fail-soft per symbol;
    // never blocks live ticks. Only runs when the daily universe was built
    // (`stashed_universe()` is `None` under Indices4Only or on build failure).
    if let Some(pd_universe) = tickvault_app::prev_day_ohlcv_boot::stashed_universe() {
        match TradingCalendar::from_config(&config.trading) {
            Ok(cal) => {
                // Compute the dates HERE (calendar in scope) so the task
                // doesn't need to move the non-Clone TradingConfig/Calendar.
                let now_ist = chrono::Utc::now()
                    + chrono::Duration::seconds(i64::from(
                        tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
                    ));
                let today = now_ist.date_naive();
                let from = tickvault_app::prev_day_ohlcv_boot::previous_trading_day(today, |d| {
                    cal.is_trading_day(d)
                });
                // Capture the expected universe size BEFORE the Arc moves into
                // the fetch, so the post-fetch coverage verification can compare.
                let pd_expected = pd_universe.subscription_targets.len();
                let pd_token = std::sync::Arc::clone(&token_handle);
                let pd_qcfg = config.questdb.clone();
                let pd_base = config.dhan.rest_api_base_url.clone();
                tokio::spawn(async move {
                    use tickvault_app::prev_day_ohlcv_boot::{
                        PrevDayCoverage, evaluate_prev_day_coverage, prev_day_csv_dir,
                        write_prev_day_coverage_csv,
                    };
                    let summary = tickvault_app::prev_day_ohlcv_boot::run_prev_day_ohlcv_fetch(
                        pd_universe,
                        pd_token,
                        pd_qcfg,
                        pd_base,
                        from,
                        today,
                    )
                    .await;
                    // Verify coverage (false-OK guard) + write the operator-
                    // visible CSV (`make prev-day-show`). Fail-soft on FS error.
                    if let Err(err) = write_prev_day_coverage_csv(
                        prev_day_csv_dir(),
                        today,
                        pd_expected,
                        &summary,
                    ) {
                        warn!(?err, "prev_day_ohlcv: coverage CSV write failed");
                    }
                    match evaluate_prev_day_coverage(pd_expected, summary.fetched) {
                        PrevDayCoverage::Ok { pct } => info!(
                            pct,
                            fetched = summary.fetched,
                            expected = pd_expected,
                            "PROOF: prev-day OHLCV coverage OK"
                        ),
                        PrevDayCoverage::Degraded { pct } => warn!(
                            pct,
                            fetched = summary.fetched,
                            expected = pd_expected,
                            skipped = summary.skipped,
                            failed = summary.failed,
                            "prev-day OHLCV coverage DEGRADED — symbols missing yesterday's candle"
                        ),
                        PrevDayCoverage::Empty => error!(
                            expected = pd_expected,
                            failed = summary.failed,
                            "prev-day OHLCV coverage EMPTY — no yesterday candles fetched"
                        ),
                    }
                });
                info!("prev-day OHLCV boot fetch task spawned");
            }
            Err(err) => {
                warn!(
                    ?err,
                    "prev_day_ohlcv: calendar build failed — boot fetch skipped"
                );
            }
        }
    }

    // Candle-engine re-architecture #T1b — wire Engine B (the only
    // candle engine). Engine C (`candles_1s` → `CandleEngineMap<Tf1s>`
    // → `CascadeFanout` matview cascade + `run_midnight_rollover_task_with_fanout`)
    // DELETED. The shared `spawn_engine_b_aggregator` helper spawns the
    // subscriber + heartbeat + IST-midnight force-seal tasks (the
    // force-seal task replaces the deleted cascade midnight rollover).
    spawn_engine_b_aggregator(
        &tick_broadcast_sender,
        std::sync::Arc::clone(&prev_day_cache),
        std::sync::Arc::clone(&trading_calendar),
    );
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

    // PR #4 (2026-05-19): SharedSpotPrices map RETIRED — depth-20/200 +
    // movers pipelines that consumed it are deleted per operator lock
    // 2026-05-15 (websocket-connection-scope-lock.md).

    let processor_handle = if let Some(receiver) = pool_receiver {
        // Candle-engine re-architecture #T1b: Engine A (the legacy 1s
        // `CandleAggregator` + `LiveCandleWriter` → `candles_1s` path)
        // DELETED. Engine B (the multi-TF aggregator → 21 plain
        // `candles_<tf>` tables) is the only candle engine.
        // PR #2 (2026-05-18): TopMoversTracker / OptionMoversTracker
        // / shared_movers snapshot retired alongside the deleted movers
        // pipeline. Under the 4-IDX_I-only universe a top-N gainers/
        // losers/most-active snapshot is meaningless.
        let tick_broadcast_for_processor = Some(tick_broadcast_sender.clone());

        // O(1) EXEMPT: cold path — build inline Greeks computer once at startup.
        let greeks_enricher = build_inline_greeks_enricher(&config, &subscription_plan);

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
            // #T4 (2026-05-20): the `previous_close` QuestDB table was
            // dropped. The boot-time `prev_day_cache_loader` SELECT
            // against it is retired — `PrevDayCache` stays empty and
            // the cascade seal-time pct path falls back to 0.0 pct
            // fields per the documented div-by-zero policy.
        } else {
            warn!(
                "prev_oi cache loader NOT spawned — slow_registry is None \
                 (subscription_plan absent)"
            );
        }

        // PR #6a (2026-05-19): bhavcopy 16:30 IST cross-check task RETIRED.
        // Under 4-IDX_I LOCKED_UNIVERSE (operator lock 2026-05-15) there are
        // no F&O subscriptions to cross-check against NSE bhavcopy. The
        // bhavcopy_cross_check + bhavcopy_fetcher + bhavcopy_scheduler modules
        // and the bhavcopy_pipeline.rs app-side runner are all deleted.
        // volume_nse_audit QuestDB table is KEPT on disk per SEBI 5-year
        // retention pending operator-triggered DROP TABLE migration.

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
        // 1d-historical-only regression fix (2026-06-02): the prev_oi cache
        // now loads from `prev_day_ohlcv` (repointed from `candles_1d` in
        // PR #979). Unlike `candles_1d` — which is always CREATEd early in boot
        // by `ensure_shadow_candle_tables` — `prev_day_ohlcv` is a NEW table
        // whose `ensure_*` runs inside the concurrently-spawned boot fetch
        // task, so on a fresh box's first boot with the new binary it may not
        // exist yet when the gate-blocking load below runs. A missing table
        // makes `load_from_questdb` return Err → the boot-ordering gate stays
        // `AwaitingOiCache` → `try_authorize_subscribe()` fails →
        // `std::process::exit(1)` → systemd marks tickvault.service failed →
        // the deploy aborts. Ensure the table exists HERE (idempotent
        // CREATE TABLE IF NOT EXISTS) so the load hits an existing — possibly
        // empty (Ok count=0, graceful) — table. A genuine QuestDB-unreachable
        // still Errs and fail-closes as designed (L14 invariant preserved).
        tickvault_storage::prev_day_ohlcv_persistence::ensure_prev_day_ohlcv_table(&config.questdb)
            .await;
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
            // PR #5 (2026-05-19): retagged from Phase2Ready01PreflightFailed
            // (retired with the Phase 2 dispatcher) to PrevClose01IlpFailed
            // which matches the message's "PREVCLOSE-01" reference per the
            // error_code_tag_guard meta-test invariant.
            tracing::error!(
                code = tickvault_common::error_code::ErrorCode::PrevClose01IlpFailed.code_str(),
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

        let _ = slow_registry; // PR #2: instrument_registry was used by deleted movers persistence helpers
        let handle = tokio::spawn(async move {
            run_tick_processor(
                receiver,
                tick_writer,
                tick_broadcast_for_processor,
                greeks_enricher,
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
                tickvault_common::always_on::current(), // §30 GIFT exemption
            )
            .await;
        });
        info!("tick processor started (with candle aggregation + trading broadcast)");
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

    // Phase 0 Item 19f — chain `shutdown_notify` into the instance-lock
    // heartbeat's own Notify. Without this bridge the heartbeat would
    // only see the broader shutdown when its own Notify fires (never,
    // in practice); now Ctrl-C / 15:30 IST close / pool-halt all
    // trigger the heartbeat's `GracefulRelease` audit row + lock
    // release before the process exits.
    if let Some(heartbeat_shutdown) = instance_lock_shutdown_chain.take() {
        let shutdown_signal = shutdown_notify.clone();
        tokio::spawn(async move {
            shutdown_signal.notified().await;
            heartbeat_shutdown.notify_one();
        });
    }

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
    // Step 8d: PR #4 (2026-05-19) — depth-20/200 rebalancer RETIRED per
    // operator lock 2026-05-15 (websocket-connection-scope-lock.md).
    // The pre-open snapshotter + Phase 2 recovery + readiness/heartbeat
    // schedulers + SLO score scheduler remain inside this block because
    // they are not depth-specific.
    // -----------------------------------------------------------------------
    if should_connect_ws && subscription_plan.is_some() {
        // C1 fix (PR-2): the DayOhlcTracker + 09:14 readiness + 09:15:30
        // streaming-confirmation tasks below are NOT universe-specific — they
        // filter ticks by segment, not by an instrument list. They must run
        // whenever a subscription plan exists, under BOTH Indices4Only and
        // DailyUniverse. Previously this was gated on `slow_boot_universe`
        // being Some; under DailyUniverse the universe is None (the ~250-SID
        // plan is the source of truth, not an FnoUniverse), which silently
        // skipped all three tasks — a false-OK regression. The outer
        // `subscription_plan.is_some()` (above) is the correct gate.
        {
            // -----------------------------------------------------------------
            // DayOhlcTracker boot wiring (post 2026-05-26 simplification).
            //
            // Per operator directive 2026-05-26 the Dhan historical /
            // pre-market buffer code was removed. `day_open` for the 4
            // LOCKED IDX_I SIDs is now the FIRST OBSERVED LIVE TICK LTP
            // after the IST midnight reset — no external arming required.
            //
            // Two tokio tasks spawned here:
            //   1. tick consumer  — drain tick broadcast, route IDX_I ticks
            //                       to update_tick() which auto-arms on
            //                       first call and advances day_high/low/
            //                       close on subsequent calls.
            //   2. midnight reset — IST 00:00:00 clears prev-day state so
            //                       the next live tick re-arms.
            // -----------------------------------------------------------------
            let day_ohlc_tracker =
                std::sync::Arc::new(tickvault_trading::in_mem::DayOhlcTracker::new());
            {
                let consumer_tracker = std::sync::Arc::clone(&day_ohlc_tracker);
                let consumer_rx = tick_broadcast_sender.subscribe();
                let _consumer_handle =
                    tickvault_app::day_ohlc_orchestrator::spawn_day_ohlc_tick_consumer(
                        consumer_tracker,
                        consumer_rx,
                    );
            }
            {
                let reset_tracker = std::sync::Arc::clone(&day_ohlc_tracker);
                let _reset_handle =
                    tickvault_app::day_ohlc_orchestrator::spawn_midnight_reset_task(reset_tracker);
            }
            info!(
                "DayOhlcTracker boot wired (tick consumer + midnight reset; day_open = first live tick LTP)"
            );

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
                    let oms = readiness_health.order_update_connected();
                    let token_secs = readiness_health.token_remaining_secs();
                    info!(
                        main_feed = main_active,
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
                    let oms = heartbeat_health.order_update_connected();
                    info!(
                        main_feed = main_active,
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
                            order_update_active: oms,
                        });
                    } else {
                        heartbeat_notifier.notify(
                            NotificationEvent::MarketOpenStreamingConfirmation {
                                main_feed_active: main_active,
                                main_feed_total: expected_main_feed_total,
                                order_update_active: oms,
                            },
                        );
                    }
                });
            }

            // Post-market tasks (EOD digest 15:31:30 IST + 1-minute cross-verify
            // 15:31:00 IST). Extracted to spawn_post_market_tasks so the fast-boot
            // path runs them too (boot-symmetry, 2026-06-09).
            spawn_post_market_tasks(
                notifier.clone(),
                std::sync::Arc::clone(&health_status),
                std::sync::Arc::clone(&trading_calendar),
                std::sync::Arc::clone(&token_handle),
                &config,
            );

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

                    // #T2b (2026-05-20): the selftest_audit DB pre-check
                    // was removed with the table. The scheduler fires
                    // once per process; `already_fired` stays false.
                    let already_fired = false;
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
                    // §31 universe-completeness — read the live universe the
                    // boot stashed (same source as the post-market cross-check
                    // above). None (no universe at 09:16) → (0, 0) → both
                    // sub-checks fail loudly, which is correct.
                    let (index_values, ntm_constituents) =
                        tickvault_app::prev_day_ohlcv_boot::stashed_universe()
                            .map(|u| {
                                use tickvault_core::instrument::daily_universe::InstrumentRole;
                                (
                                    u.count_by_role(InstrumentRole::Index),
                                    u.index_constituent_count(),
                                )
                            })
                            .unwrap_or((0, 0));
                    let inputs = MarketOpenSelfTestInputs {
                        main_feed_active: main_active,
                        order_update_active: oms,
                        pipeline_active: pipeline,
                        last_tick_age_secs: worst_gap_secs,
                        questdb_connected: questdb_ok,
                        token_expiry_headroom_secs: token_headroom_secs,
                        index_values_subscribed: index_values,
                        ntm_constituents_subscribed: ntm_constituents,
                    };
                    let started = std::time::Instant::now();
                    let outcome = evaluate_self_test(&inputs);
                    let _duration_ms =
                        i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

                    info!(
                        result = outcome.outcome_str(),
                        code = outcome.error_code().code_str(),
                        main_feed = main_active,
                        order_update = oms,
                        questdb_connected = questdb_ok,
                        token_expiry_headroom_secs = token_headroom_secs,
                        index_values,
                        ntm_constituents,
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

                    // #T2b (2026-05-20): the selftest_audit outcome row
                    // write was removed with the table. The self-test
                    // result is still delivered via Telegram + Loki.
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
                    /// market hours (pager fatigue).
                    ///
                    /// Phase 0 Item 3 (operator-locked 2026-05-13): depth-20
                    /// + depth-200 expected components ALSO scope-aware via
                    /// `should_spawn_depth_dynamic_pipeline`. Under
                    /// `IndicesUnderlyingsOnly` the depth pipeline is parked
                    /// → expected count is 0, matching the actual 0 active.
                    /// Phase 0 denominator: `1 + 0 + 0 + 1 = 2` (matches
                    /// active when main + OMS up → `ws_health = 1.0`).
                    // PR #4 (2026-05-19): depth-20 + depth-200 expected
                    // components dropped — depth pipelines retired entirely
                    // per operator lock 2026-05-15.
                    const SLO_WS_EXPECTED_ORDER_UPDATE: f64 = 1.0;
                    let slo_ws_expected_main_feed: f64 =
                        tickvault_common::config::effective_main_feed_pool_size(
                            config.subscription.scope,
                            config.dhan.max_websocket_connections,
                        ) as f64;
                    let slo_ws_expected_total: f64 =
                        slo_ws_expected_main_feed + SLO_WS_EXPECTED_ORDER_UPDATE;

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
                        let active_ou = if slo_health.order_update_connected() {
                            1.0
                        } else {
                            0.0
                        };
                        let active_total = active_main + active_ou;
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
                        // 2026-05-26: INDIA VIX (SID 21) is a derived
                        // volatility index that legitimately ticks every
                        // 30-60s during quiet sessions — its silence is
                        // not a degradation, so excluding it stops the
                        // SLO-02 flap the operator saw on 2026-05-26.
                        // We scan the top-N gaps and take the worst that
                        // is NOT a SLO-excluded SID. N=10 is comfortably
                        // above 4 (the entire universe) so we always see
                        // the full picture when called.
                        const SLO_TICK_FRESHNESS_EXCLUDED_SIDS: &[u32] = &[21]; // INDIA VIX
                        const SLO_TICK_FRESHNESS_SCAN_TOP_N: usize = 10;
                        let tick_freshness = if !in_market {
                            1.0
                        } else {
                            let worst_gap = tickvault_core::pipeline::tick_gap_detector::global_tick_gap_detector()
                                .map(|d| {
                                    let (top, _total) = d.scan_gaps_top_n(
                                        Instant::now(),
                                        SLO_TICK_FRESHNESS_SCAN_TOP_N,
                                    );
                                    top.iter()
                                        .find(|(sid, _, _)| {
                                            !SLO_TICK_FRESHNESS_EXCLUDED_SIDS.contains(sid)
                                        })
                                        .map(|(_, _, gap)| *gap)
                                        .unwrap_or(0)
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
        }
    }

    // -----------------------------------------------------------------------
    // Step 9.5: Historical candle fetch — RETIRED (PR-C 2026-05-26)
    // Operator directive 2026-05-26 removed the entire Dhan historical
    // fetch chain (candle_fetcher + cross_verify + post_open_cross_check
    // + cross_verify_scheduler + post_market_fetch_window). Spot-only
    // NIFTY 50 strategy locked; no VWAP, no futures, no historical fetch.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Step 9.6: Background greeks pipeline (option chain fetch → compute → persist)
    //
    // Phase 0 Item 7 follow-up (operator-locked 2026-05-13): this second
    // PR #3 (2026-05-19): greeks pipeline RETIRED — see slow-boot site
    // earlier in this file for the full rationale.
    // -----------------------------------------------------------------------
    info!("greeks pipeline retired (PR #3)");

    // -----------------------------------------------------------------------
    // Option-chain minute-snapshot scheduler (PR #5 of 5 — 2026-05-16)
    //
    // Drives the 3-times-per-minute Dhan option-chain fetch (operator-
    // locked schedule: SENSEX :53 / BANKNIFTY :56 / NIFTY :59) into a
    // shared RAM cache that the future BRUTEX strategy reads from.
    //
    // The scheduler is opt-in (`config.option_chain_minute_snapshot.enabled
    // = false` by default) so a fresh deployment doesn't surprise-fetch.
    //
    // Boot-time validator (`option_chain_schedule::validate_option_chain_schedule`)
    // runs before spawn — invalid TOML HALTs the app via
    // `OptionChainConfigInvalid` Severity::Critical Telegram instead of
    // silently running a broken schedule.
    //
    // See `.claude/plans/friday-may-15-mega/topic-OPTION-CHAIN-MINUTE-SNAPSHOT.md`.
    // -----------------------------------------------------------------------
    if config.option_chain_minute_snapshot.enabled {
        match tickvault_common::option_chain_schedule::validate_option_chain_schedule(
            &config.option_chain_minute_snapshot.underlyings,
        ) {
            Ok(()) => {
                info!(
                    underlyings = config.option_chain_minute_snapshot.underlyings.len(),
                    "option-chain minute-snapshot schedule validated — spawning scheduler"
                );
                let oc_token = token_handle.clone();
                let oc_client_id = ws_client_id.clone();
                let oc_base_url = config.dhan.rest_api_base_url.clone();
                let oc_config = config.option_chain_minute_snapshot.clone();
                let oc_notifier = notifier.clone();
                let oc_cache = tickvault_core::option_chain::snapshot_cache::SnapshotCache::new();
                // The cache handle is stored on the app-wide state in
                // a follow-up so the future strategy can read from it.
                // For now (PR #5), spawning is sufficient — the cache
                // accumulates snapshots ready for the next consumer.
                // Clone OWNED inputs once so the warmup-client constructor
                // below (which also takes ownership) can reuse them.
                let oc_token_for_warmup = oc_token.clone();
                let oc_client_id_for_warmup = oc_client_id.clone();
                let oc_base_url_for_warmup = oc_base_url.clone();

                match tickvault_core::option_chain::client::OptionChainClient::new(
                    oc_token,
                    oc_client_id,
                    oc_base_url,
                ) {
                    Ok(oc_client) => {
                        // 2026-05-25 — Per-day current-expiry cache.
                        // Shared between (a) boot REHYDRATE from
                        // QuestDB (`option_chain_cache_loader`),
                        // (b) 09:00:30 IST warmup task
                        // (`expiry_warmup::spawn_expiry_warmup_task`),
                        // and (c) the minute-snapshot scheduler
                        // (`spawn_snapshot_scheduler`). All three hold
                        // cloned handles to the same papaya map.
                        let current_expiry_cache = tickvault_core::option_chain::current_expiry_cache::CurrentExpiryCache::new();

                        // L3 RECONCILE — rehydrate cache from QuestDB
                        // (fast crash recovery, <1s). Logs at
                        // info!/warn!/error! per outcome.
                        tickvault_app::option_chain_cache_loader::rehydrate_and_log(
                            &config.questdb,
                            &current_expiry_cache,
                        )
                        .await;

                        // L4 PREVENT — daily 09:00:30 IST warmup task.
                        // Reuses the same token/client_id/base_url
                        // constructors as the scheduler.
                        match tickvault_core::option_chain::client::OptionChainClient::new(
                            oc_token_for_warmup,
                            oc_client_id_for_warmup,
                            oc_base_url_for_warmup,
                        ) {
                            Ok(warmup_client) => {
                                let _warmup_handle = tickvault_core::option_chain::expiry_warmup::spawn_expiry_warmup_task(
                                    warmup_client,
                                    current_expiry_cache.clone(),
                                    oc_notifier.clone(),
                                );
                                info!("option-chain 09:00:30 IST expiry warmup task spawned");
                            }
                            Err(err) => {
                                error!(
                                    ?err,
                                    "option-chain warmup client construction failed — \
                                     warmup task NOT spawned (scheduler will inline-fallback)"
                                );
                            }
                        }

                        let _ = tickvault_core::option_chain::snapshot_scheduler::spawn_snapshot_scheduler(
                            oc_config,
                            oc_client,
                            oc_cache,
                            current_expiry_cache,
                            oc_notifier,
                            config.questdb.clone(),
                        );
                        info!("option-chain minute-snapshot scheduler spawned");
                    }
                    Err(err) => {
                        error!(
                            ?err,
                            "option-chain client construction failed — scheduler NOT spawned"
                        );
                    }
                }
            }
            Err(schedule_err) => {
                // Operator-charter §F: invalid config is a HALT-class
                // event. Fire the typed Telegram + exit instead of
                // silently disabling the feature.
                error!(
                    error = %schedule_err,
                    "option-chain schedule INVALID — refusing to spawn scheduler"
                );
                notifier.notify(
                    tickvault_core::notification::NotificationEvent::OptionChainConfigInvalid {
                        reason: schedule_err.to_string(),
                    },
                );
            }
        }
    } else {
        info!("option-chain minute-snapshot pipeline disabled in config");
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
    // PR #6a (2026-05-19): index-constituency loader RETIRED under
    // 4-IDX_I LOCKED_UNIVERSE. NSE index composition (which stocks are in
    // NIFTY, etc.) isn't needed when only the 4 indices themselves are
    // tracked. The niftyindices.com download was previously gated behind
    // is_market_hours to avoid blocking boot; both paths now removed.

    // -----------------------------------------------------------------------
    // Step 11: Start axum API server
    // -----------------------------------------------------------------------
    let api_state = SharedAppState::new(
        config.questdb.clone(),
        config.dhan.clone(),
        config.instrument.clone(),
        health_status,
    );

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

    // PR #7d (2026-05-19): `/portal/*` HTML frontend retired. The
    // post-boot browser auto-open + `api.auto_open_portal` config flag
    // are both gone. Replacement surface for operator UX is Grafana
    // (auto-opened above by the infra block), Telegram alerts, MCP
    // tools, and the QuestDB Console at `localhost:9000`.

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
            // QuestDB liveness recovery-edge state: track whether a
            // `QuestDbDisconnected` (Critical) was alerted and the peak
            // consecutive-failure count, so we emit a symmetric
            // `QuestDbReconnected` (Medium) exactly once on recovery —
            // audit-findings Rule 11 (no false-OK / anxiety gap) + Rule 4
            // (edge-triggered, not level-triggered).
            let mut questdb_disconnect_alerted = false;
            let mut questdb_peak_failed_checks: u32 = 0;

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
                let liveness_success = liveness_outcome.is_success();
                if !liveness_success {
                    let failures = infra::questdb_liveness_failures();
                    // `check_questdb_liveness` resets the atomic to 0 on
                    // success, so capture the peak here while still down —
                    // it is the honest downtime signal for the recovery alert.
                    questdb_peak_failed_checks = questdb_peak_failed_checks.max(failures);
                    if failures >= infra::QUESTDB_LIVENESS_FAILURE_THRESHOLD {
                        health_notifier.notify(NotificationEvent::QuestDbDisconnected {
                            writer: "liveness-check".to_string(),
                            signal: u64::from(failures),
                            signal_kind: "Consecutive liveness failures".to_string(),
                        });
                        questdb_disconnect_alerted = true;
                    } else {
                        tracing::warn!(
                            failures,
                            threshold = infra::QUESTDB_LIVENESS_FAILURE_THRESHOLD,
                            "QuestDB liveness check failed — below alert threshold"
                        );
                    }
                } else if should_emit_questdb_reconnected(
                    questdb_disconnect_alerted,
                    liveness_success,
                ) {
                    // Recovery edge — symmetric with the Critical disconnect
                    // alert so the operator is told the incident resolved
                    // (audit-findings Rule 11). Fires exactly once: the flag
                    // is cleared here and only re-armed by a fresh disconnect.
                    health_notifier.notify(NotificationEvent::QuestDbReconnected {
                        writer: "liveness-check".to_string(),
                        failed_checks_before_recovery: questdb_peak_failed_checks,
                    });
                    questdb_disconnect_alerted = false;
                    questdb_peak_failed_checks = 0;
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
        ws_pool_arc,
        shutdown_notify,
        trading_calendar.clone(),
    )
    .await
}

// ---------------------------------------------------------------------------
// Boot-timing proof — real-time evidence the O(1) warm path is working
// ---------------------------------------------------------------------------
//
// `load_daily_universe_plan` (Step 6c) writes the whole instrument-load
// wall-clock here so the caller (which DOES hold the `notifier`) can emit a
// single operator-facing Telegram line each boot. Sentinel `u64::MAX` =
// "not measured this boot" (e.g. the Indices4Only scope never runs the
// daily-universe fetch), so the caller suppresses the Telegram in that case.
//
// Why globals and not a return-value thread: `load_instruments` and its
// match arms return `(plan, universe, needs_persist)` shared across two boot
// paths + two scopes; widening that tuple for one observability field would
// touch every arm. A trio of atomics set in one place + read in one place is
// the smaller, lower-risk surface.
static INSTRUMENT_LOAD_ELAPSED_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX);
static INSTRUMENT_LOAD_WARM_SKIPPED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static INSTRUMENT_LOAD_TOTAL_ROWS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Formats the operator-facing boot-timing Telegram line.
///
/// Returns `None` when `elapsed_ms == u64::MAX` (the sentinel meaning "not
/// measured this boot" — e.g. the Indices4Only scope skips the daily-universe
/// fetch entirely), so the caller emits nothing in that case.
///
/// Plain-English, auto-driver-readable per the 10 Telegram commandments: no
/// library names, no file paths, no version numbers — just "how long + warm
/// or cold + how many rows".
fn format_instrument_load_telegram(
    elapsed_ms: u64,
    warm_skipped: bool,
    total_rows: u64,
) -> Option<String> {
    if elapsed_ms == u64::MAX {
        return None;
    }
    // Integer seconds-with-one-decimal without float formatting surprises.
    let secs_whole = elapsed_ms / 1000;
    let secs_tenths = (elapsed_ms % 1000) / 100;
    let detail = if warm_skipped {
        "universe unchanged — reused the existing list (no rebuild needed)"
    } else {
        "full rebuild — fresh list pulled and saved"
    };
    Some(format!(
        "📦 Instrument load: {secs_whole}.{secs_tenths}s ✅ — {detail} ({total_rows} rows on file)"
    ))
}

// ---------------------------------------------------------------------------
// Helper: Load instruments (shared by fast and slow boot paths)
// ---------------------------------------------------------------------------

/// Loads instruments from the static `FnoUniverse::locked_4_idx_i()`.
///
/// PR #6b (2026-05-19): the entire CSV download/parse/validate pipeline is
/// retired under the operator-locked 4-IDX_I scope (operator lock 2026-05-15
/// per `websocket-connection-scope-lock.md`). The universe is now a 4-SID
/// compile-time constant via `LOCKED_UNIVERSE` (NIFTY/BANKNIFTY/SENSEX/INDIA
/// VIX). No CSV download, no parse, no validate, no rkyv cache, no daily
/// refresh — every boot reads the same 4 SIDs.
///
/// Returns (plan, universe, needs_persist). `needs_persist` is always
/// `false` because the synthetic universe has no F&O contracts to persist;
/// the 4 IDX_I rows are written via the standard instrument-master DDL
/// idempotency path.
#[allow(clippy::unused_async)] // APPROVED: Indices4Only arm has no await; DailyUniverse arm awaits
async fn load_instruments(
    config: &ApplicationConfig,
    _is_trading_day: bool,
    trading_calendar: &TradingCalendar,
) -> anyhow::Result<(Option<SubscriptionPlan>, Option<FnoUniverse>, bool)> {
    match config.subscription.scope {
        SubscriptionScope::Indices4Only => {
            info!("loading 4-IDX_I LOCKED_UNIVERSE (no CSV download / parse / validate)");

            let universe = FnoUniverse::locked_4_idx_i();
            let today = Utc::now().with_timezone(&ist_offset()).date_naive();
            // Empty spot prices — no F&O underlyings to ATM-select.
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
                "subscription plan ready (LOCKED_UNIVERSE — 4 IDX_I SIDs)"
            );

            // needs_persist=false: synthetic universe has no F&O derivative rows
            // to write beyond the 4 IDX_I entries handled by the DDL path.
            Ok((Some(plan), Some(universe), false))
        }
        #[cfg(feature = "daily_universe_fetcher")]
        SubscriptionScope::DailyUniverse => load_daily_universe_plan(config).await,
        #[cfg(not(feature = "daily_universe_fetcher"))]
        SubscriptionScope::DailyUniverse => {
            anyhow::bail!(
                "subscription scope is DailyUniverse but the daily_universe_fetcher feature is \
                 not compiled in — rebuild with `--features daily_universe_fetcher`"
            )
        }
    }
}

/// Step 6c — daily-universe fetch + build the ~250-SID subscription plan.
///
/// FAIL-CLOSED per operator §4 lock: `max_attempts = None` (infinite retry,
/// escalating Telegram at attempts 4/11/21); boot BLOCKS until a fresh,
/// validated CSV is in hand. A reconcile/audit (QuestDB) error HALTS boot
/// (`?`-propagated) rather than proceeding on unknown state. The built
/// universe flows straight into `build_subscription_plan_from_daily_universe`
/// (Quote mode §8, ~250 SIDs). `needs_persist = false` — the lifecycle
/// reconcile already persisted inside `run_daily_universe_boot`.
#[cfg(feature = "daily_universe_fetcher")]
async fn load_daily_universe_plan(
    config: &ApplicationConfig,
) -> anyhow::Result<(Option<SubscriptionPlan>, Option<FnoUniverse>, bool)> {
    use anyhow::Context;
    use tickvault_core::instrument::instrument_snapshot;
    use tickvault_core::instrument::subscription_planner::build_subscription_plan_from_daily_universe;

    info!("Step 6c: daily-universe instrument fetch (scope=DailyUniverse, fail-closed §4)");

    // Today's IST trading date — the snapshot cache key.
    let now_ist = chrono::Utc::now()
        + chrono::TimeDelta::seconds(i64::from(
            tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
        ));
    let today_date = now_ist.date_naive().format("%Y-%m-%d").to_string();

    // ── FAST PATH (PR-2): same-day snapshot → instant warm resubscribe ──
    // Operator-authorized relaxation of the §4 "boot blocks until fresh CSV"
    // rule for SAME-DAY crash recovery ONLY (lock §29). A date-keyed snapshot
    // means today's universe was already fetched + validated + lifecycle-
    // written by an earlier successful boot. We resubscribe instantly (~1ms)
    // and run the full cold rebuild in the BACKGROUND to refresh the lifecycle
    // master + snapshot. The fast path never SKIPS the audit/lifecycle work —
    // it DEFERS it past first-tick. The FIRST boot of the day (no snapshot)
    // still takes the fail-closed slow path below.
    let warm_timer = std::time::Instant::now();
    if let Some(universe) = instrument_snapshot::load_plan_snapshot_for_today(&today_date) {
        let plan = build_subscription_plan_from_daily_universe(&universe);
        let elapsed_ms = u64::try_from(warm_timer.elapsed().as_millis()).unwrap_or(u64::MAX);
        info!(
            universe_size = universe.total_count(),
            total = plan.summary.total,
            feed_mode = %plan.summary.feed_mode,
            elapsed_ms,
            "subscription plan ready (DailyUniverse — INSTANT warm resubscribe from snapshot)"
        );
        // Timing-proof telemetry: warm path, no cold rebuild this boot.
        // total_rows MUST be the real universe size (was hardcoded 0 — a healthy
        // 243-SID warm boot then mislabelled itself "0 rows on file", making an
        // empty universe indistinguishable from a healthy one). 2026-06-02 fix.
        let warm_total_rows = u64::try_from(universe.total_count()).unwrap_or(0);
        record_instrument_load_telemetry(elapsed_ms, 0, 0, true, warm_total_rows);

        // Background reconcile: refresh lifecycle master + snapshot off the
        // critical path. Failure here does NOT halt — first-tick already flows
        // from the snapshot, and lifecycle for TODAY was already written by the
        // earlier boot that produced the snapshot.
        let questdb = config.questdb.clone();
        let date_for_bg = today_date.clone();
        tokio::spawn(async move {
            match cold_build_daily_universe(&questdb, false).await {
                Ok((_outcome, fresh_universe)) => {
                    if let Err(e) =
                        instrument_snapshot::write_plan_snapshot(&fresh_universe, &date_for_bg)
                    {
                        warn!(error = %e, "background reconcile: snapshot refresh write failed");
                    } else {
                        info!(
                            "background reconcile complete — lifecycle master + snapshot refreshed"
                        );
                    }
                }
                Err(e) => {
                    error!(
                        code = tickvault_common::error_code::ErrorCode::InstrFetch01CsvHardFailed
                            .code_str(),
                        error = %e,
                        "background reconcile failed — warm plan still live; retries next boot"
                    );
                }
            }
        });

        return Ok((Some(plan), None, false));
    }

    // ── SLOW PATH: first boot of the day (no snapshot) — full cold build ──
    // §4 fail-closed: infinite retry, boot BLOCKS until a fresh validated CSV
    // is in hand. A reconcile/audit error HALTS boot (?-propagated).
    let (outcome, daily_universe) = cold_build_daily_universe(&config.questdb, false)
        .await
        .context("Step 6c: daily-universe cold build failed (fail-closed §4) — halting")?;

    // Persist the snapshot so a same-day crash takes the fast path above.
    // Best-effort: a failed cache write is degraded-not-broken (next boot
    // cold-builds again).
    if let Err(e) = instrument_snapshot::write_plan_snapshot(&daily_universe, &today_date) {
        warn!(
            error = %e,
            date = %today_date,
            "plan snapshot write failed — same-day crash will cold-build"
        );
    }

    let plan = build_subscription_plan_from_daily_universe(&daily_universe);
    info!(
        universe_size = outcome.universe_size,
        total = plan.summary.total,
        feed_mode = %plan.summary.feed_mode,
        upserted = outcome.reconcile.apply.upserted,
        expired = outcome.reconcile.apply.expired,
        "subscription plan ready (DailyUniverse — ~250 SIDs, Quote mode)"
    );

    // Boot-timing proof: REAL evidence the O(1) warm path is working in
    // production every day. Per-phase breakdown (download → build → reconcile)
    // shows WHERE a cold boot spends its seconds.
    let elapsed_ms = outcome.elapsed_ms;
    let warm_skipped = outcome.reconcile.warm_skipped;
    let total_rows = u64::try_from(outcome.total_rows).unwrap_or(0);
    let download_ms = outcome.download_ms;
    let reconcile_ms = outcome.reconcile_ms;
    let build_ms = elapsed_ms
        .saturating_sub(download_ms)
        .saturating_sub(reconcile_ms);
    info!(
        elapsed_ms,
        download_ms,
        build_ms,
        reconcile_ms,
        warm_skipped,
        total_rows,
        universe_size = outcome.universe_size,
        "instrument load timing (O(1) warm-path proof; per-phase download→build→reconcile)"
    );
    record_instrument_load_telemetry(
        elapsed_ms,
        download_ms,
        reconcile_ms,
        warm_skipped,
        total_rows,
    );

    Ok((Some(plan), None, false))
}

/// Emit the instrument-load timing telemetry — Prometheus gauges + the
/// process-global atomics the boot-timing Telegram line reads. Shared by both
/// the warm (snapshot) and cold (full build) paths so the gauges/atomics are
/// written exactly once per load, regardless of path.
#[cfg(feature = "daily_universe_fetcher")]
fn record_instrument_load_telemetry(
    elapsed_ms: u64,
    download_ms: u64,
    reconcile_ms: u64,
    warm_skipped: bool,
    total_rows: u64,
) {
    let build_ms = elapsed_ms
        .saturating_sub(download_ms)
        .saturating_sub(reconcile_ms);
    metrics::gauge!("tv_instrument_load_duration_ms").set(elapsed_ms as f64);
    metrics::gauge!("tv_instrument_load_download_ms").set(download_ms as f64);
    metrics::gauge!("tv_instrument_load_build_ms").set(build_ms as f64);
    metrics::gauge!("tv_instrument_load_reconcile_ms").set(reconcile_ms as f64);
    metrics::gauge!("tv_instrument_load_warm_skipped").set(if warm_skipped { 1.0 } else { 0.0 });
    metrics::gauge!("tv_instrument_load_total_rows").set(total_rows as f64);
    INSTRUMENT_LOAD_ELAPSED_MS.store(elapsed_ms, std::sync::atomic::Ordering::Relaxed);
    INSTRUMENT_LOAD_WARM_SKIPPED.store(warm_skipped, std::sync::atomic::Ordering::Relaxed);
    INSTRUMENT_LOAD_TOTAL_ROWS.store(total_rows, std::sync::atomic::Ordering::Relaxed);
}

/// Cold build of the daily universe: wait for QuestDB → ensure lifecycle/audit
/// tables → download the Detailed CSV → run the §4 fail-closed boot (infinite
/// retry, lifecycle reconcile). Returns the boot outcome + the built universe.
///
/// Shared by the slow boot path (awaited) and the warm-path background
/// reconcile (spawned). Takes only `&QuestDbConfig` so it is cheap to clone
/// into the background task.
#[cfg(feature = "daily_universe_fetcher")]
async fn cold_build_daily_universe(
    questdb: &tickvault_common::config::QuestDbConfig,
    dry_run: bool,
) -> anyhow::Result<(
    tickvault_app::daily_universe_boot::DailyUniverseBootOutcome,
    std::sync::Arc<tickvault_core::instrument::daily_universe::DailyUniverse>,
)> {
    use anyhow::Context;
    use std::sync::Arc;
    use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
    use tickvault_core::instrument::csv_downloader::CsvDownloader;
    use tickvault_storage::instrument_fetch_audit_persistence::ensure_instrument_fetch_audit_table;
    use tickvault_storage::instrument_lifecycle_persistence::{
        ensure_instrument_lifecycle_audit_table, ensure_instrument_lifecycle_table,
    };

    // QuestDB must be reachable before the reconcile writes (idempotent).
    tickvault_storage::boot_probe::wait_for_questdb_ready(
        questdb,
        tickvault_storage::boot_probe::BOOT_DEADLINE_SECS,
    )
    .await
    .context("daily-universe: QuestDB not ready for reconcile")?;

    // Idempotent CREATE TABLE IF NOT EXISTS — downstream writers assume these
    // 3 lifecycle/audit tables exist (they do not self-ensure).
    ensure_instrument_lifecycle_table(questdb).await;
    ensure_instrument_lifecycle_audit_table(questdb).await;
    ensure_instrument_fetch_audit_table(questdb).await;

    // IST wall-clock nanoseconds (host-clock based) — single `now_utc` so `now`
    // and `today-midnight` are consistent. Fail-closed on a clock outside the
    // representable nanos range rather than stamping epoch-0 audit rows.
    let now_utc = chrono::Utc::now();
    let offset_nanos = i64::from(IST_UTC_OFFSET_SECONDS) * 1_000_000_000;
    let now_ist_nanos = now_utc
        .timestamp_nanos_opt()
        .context("daily-universe: system clock outside representable nanosecond range")?
        + offset_nanos;
    let now_ist = now_utc + chrono::TimeDelta::seconds(i64::from(IST_UTC_OFFSET_SECONDS));
    let today_ist_nanos = now_ist
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|midnight| midnight.and_utc().timestamp_nanos_opt())
        .context("daily-universe: IST midnight outside representable nanosecond range")?;

    let downloader =
        Arc::new(CsvDownloader::new().context("daily-universe: CsvDownloader init failed")?);
    let fetch_fn = move |_attempt: u32| {
        let downloader = Arc::clone(&downloader);
        async move { downloader.fetch_csv().await }
    };
    // §31 NTM (Sub-PR #10b): best-effort fetch of the NIFTY Total Market
    // constituents. DEGRADE+ALERT (operator 2026-06-06): a None here means the
    // niftyindices source failed — the universe builds on the indices +
    // F&O-underlyings core set and `NTM-CONSTITUENCY-01` already paged. The
    // Dhan CSV path below stays fail-closed (§4) regardless.
    let ntm_map = fetch_ntm_constituency_map(now_ist.date_naive()).await;

    // §4: infinite retry (None) — boot blocks until a fresh CSV is in hand.
    let (outcome, universe) = tickvault_app::daily_universe_boot::run_daily_universe_boot(
        questdb,
        fetch_fn,
        now_ist_nanos,
        today_ist_nanos,
        dry_run,
        None,
        ntm_map,
    )
    .await
    .context("daily-universe boot failed (fail-closed §4)")?;

    // Operator lock 2026-06-01 §30: install the always-on (no market-hours
    // filter) exemption set ONCE from the freshly-built universe. GIFT Nifty
    // (sid 5024, NSE-IX, ~21 h/day) is the only entry today. Read later by
    // the tick processor + candle aggregator via
    // `tickvault_common::always_on::current()`. Empty set if GIFT absent.
    let always_on = universe.always_on_segments();
    info!(
        always_on_count = always_on.len(),
        "§30 always-on (no market-hours filter) set installed"
    );
    tickvault_common::always_on::init_always_on_segments(always_on);

    // PR4 (operator 2026-06-01): stash the freshly-built universe so the
    // boot prev-day OHLCV fetch task (spawned later, where the token + REST
    // base URL are in scope) can read the ~243 subscription targets.
    tickvault_app::prev_day_ohlcv_boot::stash_universe(std::sync::Arc::clone(&universe));

    // §31 item 2 (NTM Map-B): populate the FULL per-index constituency mapping
    // (all ~46 tracked indices) into the `index_constituency` table. MAP-ONLY +
    // fully decoupled — it re-fetches independently and NEVER touches the live
    // subscription. Spawned fire-and-forget so it never delays boot or the feed;
    // degrade-safe (logs warn! + returns on any failure).
    tokio::spawn(
        tickvault_app::index_constituency_boot::persist_index_constituency_mapping(
            questdb.clone(),
            now_ist.date_naive(),
            today_ist_nanos,
            dry_run,
        ),
    );

    Ok((outcome, universe))
}

/// §31 NTM (Sub-PR #10b): best-effort fetch of the NIFTY Total Market (and the
/// other tracked-index) constituents from niftyindices.com, assembled into an
/// [`IndexConstituencyMap`] for the universe builder's resolve+bridge step.
///
/// **DEGRADE+ALERT (operator AskUserQuestion 2026-06-06):** this NEVER blocks
/// boot. On any failure (downloader init, all slugs failed → empty map) it logs
/// `error!(code = NTM-CONSTITUENCY-01)` (→ Telegram via the 5-sink pipeline) and
/// returns `None`, so the universe builds on the indices + F&O-underlyings core
/// set. The Dhan CSV path stays fail-closed (§4) independently. Per-slug
/// download/parse failures are already logged inside `build_constituency_map`.
///
/// Cold path — runs once at boot.
async fn fetch_ntm_constituency_map(
    today: chrono::NaiveDate,
) -> Option<tickvault_common::instrument_types::IndexConstituencyMap> {
    use std::time::Duration;
    use tickvault_common::constants::{
        NTM_CONSTITUENCY_FETCH_TIMEOUT_SECS, NTM_CONSTITUENCY_SLUGS,
    };
    use tickvault_common::error_code::ErrorCode;
    use tickvault_core::instrument::index_constituency::build_constituency_map;
    use tickvault_core::instrument::index_constituency::downloader::ConstituencyDownloader;

    let downloader = match ConstituencyDownloader::new() {
        Ok(d) => d,
        Err(err) => {
            error!(
                code = ErrorCode::NtmConstituency01SourceDegraded.code_str(),
                reason = "downloader_init",
                %err,
                "NTM constituency downloader init failed — degrading to core universe"
            );
            return None;
        }
    };
    // §31 item 4: subscribe the NTM union ONLY (NTM_CONSTITUENCY_SLUGS), NOT the
    // full ~49-index list (whose union ~= the entire NSE → would breach the
    // [100,1200] envelope and HALT boot). Total-fetch timeout bounds a stalled
    // niftyindices before the 09:00 open → degrade.
    let map = match tokio::time::timeout(
        Duration::from_secs(NTM_CONSTITUENCY_FETCH_TIMEOUT_SECS),
        build_constituency_map(&downloader, NTM_CONSTITUENCY_SLUGS, today),
    )
    .await
    {
        Ok(map) => map,
        Err(_elapsed) => {
            error!(
                code = ErrorCode::NtmConstituency01SourceDegraded.code_str(),
                reason = "timeout",
                timeout_secs = NTM_CONSTITUENCY_FETCH_TIMEOUT_SECS,
                "NTM constituency fetch exceeded its boot budget — degrading to core universe"
            );
            return None;
        }
    };
    if map.index_to_constituents.is_empty() {
        error!(
            code = ErrorCode::NtmConstituency01SourceDegraded.code_str(),
            reason = "fetch_or_parse",
            indices_failed = map.build_metadata.indices_failed,
            "NTM constituency fetch returned no indices — degrading to core universe \
             (today runs without the cash-only NTM constituents)"
        );
        return None;
    }
    info!(
        indices = map.index_to_constituents.len(),
        unique_stocks = map.stock_to_indices.len(),
        "NTM constituency map fetched for the daily-universe build"
    );
    Some(map)
}

// create_log_file_writer is now in boot_helpers module (lib.rs).

// ---------------------------------------------------------------------------
// Helper: Build WebSocket connection pool (shared by fast and slow boot paths)
// ---------------------------------------------------------------------------

/// Creates the WebSocket connection pool and returns the frame receiver
/// WITHOUT spawning connections. This allows the tick processor to start
/// consuming frames BEFORE connections begin sending data, preventing
/// frame send timeouts during the stagger period.
#[allow(clippy::too_many_arguments)] // APPROVED: STAGE-C added wal_spill
fn create_websocket_pool(
    token_handle: &TokenHandle,
    client_id: &str,
    subscription_plan: &Option<SubscriptionPlan>,
    config: &ApplicationConfig,
    is_market_hours: bool,
    notifier: Option<std::sync::Arc<NotificationService>>,
    wal_spill: Option<std::sync::Arc<tickvault_storage::ws_frame_spill::WsFrameSpill>>,
) -> Option<(
    tokio::sync::mpsc::Receiver<(u64, bytes::Bytes)>,
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

    // Phase 0 Item 4 (operator-locked 2026-05-13): scope-aware activity
    // watchdog threshold. Under IndicesUnderlyingsOnly the data rate is
    // dense (113-448 frames/sec aggregate across a single conn) so the
    // legacy 50s threshold is wasteful — tighten to 3s (IDX_I's expected
    // 1-3 ticks/sec window). Under legacy scopes preserve 50s.
    let configured_watchdog_threshold = ws_config.activity_watchdog_threshold_secs;
    // PR #5 (2026-05-19): `effective_main_feed_watchdog_threshold_secs`
    // helper inlined here after `phase2_recovery` module retirement
    // (operator lock 2026-05-15, websocket-connection-scope-lock.md).
    let effective_watchdog_threshold = match config.subscription.scope {
        // AWS-lifecycle LOCKED (PR #7b) — 4 IDX_I SIDs only, IDX_I idle
        // tolerance applies (3s — the expected 1-3 tick/sec window).
        tickvault_common::config::SubscriptionScope::Indices4Only => {
            tickvault_core::websocket::activity_watchdog::WATCHDOG_THRESHOLD_IDX_I_SECS
        }
        // Sub-PR #1 of 2026-05-27 — `DailyUniverse` variant introduced;
        // production code-path activation lands in Sub-PRs #2-#13. Same
        // IDX_I watchdog threshold applies until #8 tunes it for the
        // 250-SID mixed universe (indices + NSE_EQ underlyings).
        // See `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`.
        tickvault_common::config::SubscriptionScope::DailyUniverse => {
            tickvault_core::websocket::activity_watchdog::WATCHDOG_THRESHOLD_IDX_I_SECS
        }
    };
    if effective_watchdog_threshold != configured_watchdog_threshold {
        info!(
            scope = config.subscription.scope.as_str(),
            configured_secs = configured_watchdog_threshold,
            effective_secs = effective_watchdog_threshold,
            "Phase 0 Item 4: main-feed activity-watchdog threshold clamped by scope"
        );
        ws_config.activity_watchdog_threshold_secs = effective_watchdog_threshold;
    }

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
    // AWS-lifecycle LOCKED (PR #7b): under the single-variant scope the
    // planner emits exactly 4 IDX_I SIDs (well below the 5000-per-conn
    // Dhan cap). Spinning up 5 main-feed connections would waste 4 idle
    // slots, fragment the token/IP budget, and trip the pool watchdog
    // with false-positive "connection idle" signals.
    // `effective_main_feed_pool_size` always returns
    // `PHASE_0_MAIN_FEED_CONNECTION_COUNT = 1`.
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
        // Real-tick freshness (not frame freshness): the most-recent GENUINE
        // tick across all instruments, sourced from the global TickGapDetector
        // (populated only by real tick frames, never Dhan keep-alive pings).
        // `None` => zero real ticks captured yet. Surfacing this in the
        // "live and ready" message closes the 2026-06-02 false-OK where the
        // per-feed "last update Xs ago" counted pings and could read healthy
        // while no real ticks flowed. Cold path — once per boot summary.
        let last_real_tick_age_secs =
            tickvault_core::pipeline::tick_gap_detector::global_tick_gap_detector()
                .and_then(|d| d.freshest_tick_age_secs(std::time::Instant::now()))
                .and_then(|secs| u32::try_from(secs).ok());
        notifier.notify(NotificationEvent::WebSocketPoolOnline {
            connected: connected_count,
            total,
            per_connection,
            boot_path,
            boot_wall_clock_secs,
            last_real_tick_age_secs,
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

                    // Pre-market deferral fix (2026-06-03): OUTSIDE market
                    // hours the main-feed pool is intentionally DEFERRED —
                    // it opens zero TCP sockets until 09:00 IST because Dhan
                    // idle-resets pre-market connections. So every connection
                    // reads "down" during 08:42→09:00 and the watchdog stamps
                    // an `AllDown { since }` at ~08:42 boot. Reset the
                    // watchdog on each off-hours poll so that stale `since`
                    // never carries into the `is_within_market_hours_ist()`
                    // boundary: without this, the first in-hours poll at
                    // 09:00:00 saw down_for ≈ 1055s > POOL_HALT_SECS (300s)
                    // and tripped Halt → std::process::exit(2) → supervisor
                    // restart, paging [HIGH] WS POOL HALT + [HIGH] FAST BOOT
                    // every single market open. The first in-hours poll now
                    // starts a FRESH 300s window, giving the deferred feed
                    // its normal reconnect time; the genuine in-market
                    // "all-down for 300s → halt" safety property (the
                    // Halt arm above, gated by in_market_hours) is unchanged.
                    if !in_market_hours {
                        pool.reset_watchdog();
                    }
                }
            }
        }
    });
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

// compute_market_close_sleep is now in boot_helpers module (lib.rs).

// ---------------------------------------------------------------------------
// Helper: Engine B — multi-TF candle aggregator (shared by fast + slow boot)
// ---------------------------------------------------------------------------

/// Candle-engine re-architecture #T1b — wire Engine B (the only candle
/// engine).
///
/// Spawns three tokio tasks, all driven off the live tick broadcast:
///
/// 1. **Aggregator subscriber** — folds every live tick into the 21-TF
///    [`MultiTfAggregator`]; on each TF boundary cross the sealed
///    candle is pct-stamped and pushed into the seal-writer ring (which
///    drains to the 21 plain `candles_<tf>` tables).
/// 2. **Per-minute heartbeat** — coalesced 60s structured log of
///    seals-emitted / dropped / late-discarded (AGGREGATOR-HB-01).
/// 3. **IST-midnight force-seal** — at IST 00:00:00 each trading day,
///    force-seals every open bucket of every instrument so day-N state
///    never fuses into day-(N+1)'s first bar. Replaces the deleted
///    Engine-C `run_midnight_rollover_task_with_fanout`.
///
/// Both boot paths (fast crash-recovery + slow normal start) call this
/// so candle aggregation is identical regardless of boot mode.
// WIRING-EXEMPT: call sites are the fast-boot + slow-boot blocks in `main` below.
/// Spawns the Wave 6 seal-writer loop and publishes its GLOBAL seal Sender so
/// the multi-TF aggregator (Engine B) can emit sealed candles into the
/// `candles_<tf>` tables.
///
/// MUST run on BOTH boot paths. If the Sender is never published,
/// `global_seal_sender()` returns None and the aggregator's per-tick closure
/// skips every tick (`else { continue }`), leaving `candles_*` empty. This is
/// the 2026-06-01 `candles_1m=0` bug: the seal-writer was wired in the slow
/// boot path only, so FAST BOOT captured ticks but sealed no candles.
/// `set_global_seal_sender` is idempotent (first installer wins), so calling
/// this on whichever boot path runs is safe.
fn spawn_seal_writer_loop(questdb_config: &tickvault_common::config::QuestDbConfig) {
    use tickvault_storage::seal_writer_loop::{run_seal_writer_loop, seal_drain_interval};
    use tickvault_storage::seal_writer_runner::SealWriterRunner;

    // 1024 seals × 100 ms cycle = ~10,240 seals/sec sustained — well above
    // the ~99K-seal IST-midnight burst absorbed across ~10 cycles.
    const SEAL_MAX_DRAIN_PER_CYCLE: usize = 1_024;

    match SealWriterRunner::new(questdb_config, SEAL_MAX_DRAIN_PER_CYCLE) {
        Ok(runner) => {
            if !tickvault_storage::seal_writer_runner::set_global_seal_sender(runner.sender()) {
                tracing::warn!(
                    "global seal sender already installed (idempotent skip) — first installer wins"
                );
            }
            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            // Hold the watch sender for the process lifetime so the loop's
            // `.changed().await` does not wake on a disconnected channel.
            std::mem::forget(cancel_tx);
            tokio::spawn(async move {
                let _final_outcome =
                    run_seal_writer_loop(runner, seal_drain_interval(), cancel_rx).await;
                tracing::info!("seal writer loop exited gracefully");
            });
            tracing::info!(
                interval_ms = seal_drain_interval().as_millis(),
                max_drain_per_cycle = SEAL_MAX_DRAIN_PER_CYCLE,
                "seal writer task spawned — Engine B candle sealing enabled"
            );
        }
        Err(err) => {
            tracing::error!(
                ?err,
                "failed to construct SealWriterRunner — candles will NOT seal this session"
            );
        }
    }
}

fn spawn_engine_b_aggregator(
    tick_broadcast_sender: &tokio::sync::broadcast::Sender<
        tickvault_common::tick_types::ParsedTick,
    >,
    prev_day_cache: std::sync::Arc<tickvault_trading::in_mem::PrevDayCache>,
    trading_calendar: std::sync::Arc<TradingCalendar>,
) {
    use tickvault_storage::seal_writer_runner::global_seal_sender;
    use tickvault_trading::candles::{
        AggregatorHeartbeatCounters, BufferedSeal, MultiTfAggregator, TfIndex,
        stamp_seal_pct_fields,
    };

    // 11K-instrument capacity (matches MAX_TOTAL_SUBSCRIPTIONS headroom
    // per `aws-budget.md`). HashMap grows lazily so this is a hint.
    const AGGREGATOR_CAPACITY: usize = 11_000;

    // §30: GIFT Nifty (always-on) candles must form across its full ~21h
    // session — pass the boot-installed exemption set into the aggregator.
    let aggregator = std::sync::Arc::new(
        MultiTfAggregator::with_capacity(AGGREGATOR_CAPACITY)
            .with_always_on(tickvault_common::always_on::current()),
    );

    // --- Task 1: aggregator subscriber (per-tick fold + seal) ---
    let agg_clone = std::sync::Arc::clone(&aggregator);
    let prev_day_cache_for_agg = std::sync::Arc::clone(&prev_day_cache);
    let heartbeat = AggregatorHeartbeatCounters::new();
    let heartbeat_writer = heartbeat.clone();
    let heartbeat_reader = heartbeat.clone();
    let mut tick_rx = tick_broadcast_sender.subscribe();

    tokio::spawn(async move {
        loop {
            match tick_rx.recv().await {
                Ok(tick) => {
                    let Some(sender) = global_seal_sender() else {
                        continue;
                    };
                    let stats = agg_clone.consume_tick(
                        &tick,
                        tick.exchange_segment_code,
                        |tf, mut state| {
                            // 1d historical-only (operator directive 2026-06-02):
                            // the 1d timeframe is NEVER tick-calculated. It is
                            // pulled once each morning from Dhan historical into
                            // `prev_day_ohlcv`. Drop any D1 seal the aggregator
                            // emits so it never reaches `candles_1d`. The
                            // aggregator still seals all 21 TFs internally
                            // (fixed 21-slot arrays + `42 = 2×21` unit test
                            // intact); only this write boundary skips D1.
                            if tf == TfIndex::D1 {
                                return;
                            }
                            let mut refs = prev_day_cache_for_agg
                                .lookup(tick.security_id, tick.exchange_segment_code)
                                .unwrap_or_default();
                            // Operator decision 2026-05-28: take the prev-day
                            // close straight from the live ticks `close`
                            // column (captured into the candle state), not the
                            // QuestDB PrevDayCache (which is empty in the
                            // Engine-B runtime). Drives close_pct_from_prev_day.
                            refs.prev_day_close = state.prev_day_close;
                            stamp_seal_pct_fields(&mut state, refs);
                            // G3 real-time proof: capture the % BEFORE `state`
                            // is moved into the seal. A non-zero value proves
                            // the percentage-change column is populating live.
                            let close_pct_nonzero = state.close_pct_from_prev_day != 0.0;
                            let seal = BufferedSeal::new(
                                tick.security_id,
                                tick.exchange_segment_code,
                                tf,
                                state,
                            );
                            if sender.try_send(seal).is_err() {
                                metrics::counter!("tv_seal_mpsc_dropped_total").increment(1);
                                heartbeat_writer.record_drop();
                            } else {
                                metrics::counter!("tv_aggregator_seals_emitted_total").increment(1);
                                heartbeat_writer.record_emit();
                                if close_pct_nonzero {
                                    metrics::counter!("tv_aggregator_close_pct_nonzero_total")
                                        .increment(1);
                                    heartbeat_writer.record_close_pct_nonzero();
                                }
                            }
                        },
                    );
                    if stats.late_count > 0 {
                        metrics::counter!("tv_aggregator_late_ticks_discarded_total")
                            .increment(u64::from(stats.late_count));
                        heartbeat_writer.record_late_ticks(u64::from(stats.late_count));
                    }
                    // Option B: a 1-bucket-late tick re-folded its OWN minute's
                    // high/low/close and was re-emitted via on_seal (UPSERT
                    // replaced the candle row). Observable, not a silent merge.
                    if stats.amended_count > 0 {
                        metrics::counter!("tv_aggregator_amended_ticks_total")
                            .increment(u64::from(stats.amended_count));
                        heartbeat_writer.record_amended_ticks(u64::from(stats.amended_count));
                    }
                    if !stats.instrument_found {
                        agg_clone.pre_populate(std::iter::once((
                            tick.security_id,
                            tick.exchange_segment_code,
                        )));
                        metrics::counter!("tv_aggregator_instruments_lazy_inserted_total")
                            .increment(1);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    metrics::counter!("tv_aggregator_tick_lag_total").increment(skipped);
                    // H2-lite (zero-tick-loss PR-8b): the aggregator fell so far
                    // behind the ~52s TICK_BROADCAST_CAPACITY buffer that the
                    // broadcast dropped `skipped` ticks from ITS view. This was a
                    // SILENT counter bump; make it LOUD (audit Rule 5 — a
                    // candle-data-loss event must be `error!`, never silent).
                    //
                    // CRITICAL ASSURANCE: the dropped ticks are NOT lost and NOT
                    // reordered. The lossless + ORDERED durable record is the WAL
                    // frame spill (`ws_frame_spill`: raw frames captured by the WS
                    // read loop BEFORE any broadcast fan-out — single-producer FIFO
                    // segments, ring→spill→DLQ, replayed in append order on boot).
                    // This broadcast `Lagged` is downstream of that WAL, so it can
                    // only affect the DERIVED candles for this window — never the
                    // durable tick record, and never tick ORDER. The 15:31 IST
                    // post-market 1-minute cross-verify pinpoints the affected
                    // minutes, rebuildable from the WAL-backed, ts-ordered `ticks`
                    // table. Tick routing + order on the live read loop are
                    // untouched by this change.
                    tracing::error!(
                        skipped,
                        code =
                            tickvault_common::error_code::ErrorCode::AggregatorLag01TickLagDropped
                                .code_str(),
                        "candle aggregator tick-broadcast LAGGED — derived candles for this \
                         window may under-count; ticks remain safe + ordered in the ticks table; \
                         rebuild via the post-market 1m cross-verify"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!("Engine B aggregator subscriber: broadcast closed, exiting");
                    break;
                }
            }
        }
    });
    tracing::info!(
        aggregator_capacity = AGGREGATOR_CAPACITY,
        "candle-engine #T1b — multi-TF aggregator task spawned (Engine B)"
    );

    // --- Task 2: per-minute heartbeat ---
    const AGGREGATOR_HEARTBEAT_INTERVAL_SECS: u64 = 60;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            AGGREGATOR_HEARTBEAT_INTERVAL_SECS,
        ));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
                close_pct_nonzero = snap.close_pct_nonzero,
                interval_secs = AGGREGATOR_HEARTBEAT_INTERVAL_SECS,
                "aggregator heartbeat (AGGREGATOR-HB-01)"
            );
        }
    });
    tracing::info!("candle-engine #T1b — aggregator heartbeat task spawned");

    // --- Task 3: IST-midnight boundary force-seal ---
    // Replaces the deleted Engine-C `run_midnight_rollover_task_with_fanout`.
    // At IST 00:00:00 each trading day, force-seals every open bucket so
    // day-N candle state never fuses into day-(N+1)'s first bar. Each
    // sealed bar is pct-stamped and routed into the SAME seal-writer ring
    // the per-tick path uses (`global_seal_sender()`).
    let agg_for_boundary = std::sync::Arc::clone(&aggregator);
    let prev_day_cache_for_boundary = std::sync::Arc::clone(&prev_day_cache);
    tokio::spawn(async move {
        loop {
            // Sleep until the next IST midnight (bounded helper, ≤ 24h).
            let sleep_secs = tickvault_common::market_hours::secs_until_next_ist_midnight().max(1);
            tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;

            // Only force-seal on trading days — a non-trading-day
            // midnight has no open buckets worth flushing. `is_trading_day_today`
            // reads the IST calendar date internally.
            if !trading_calendar.is_trading_day_today() {
                tracing::info!("IST-midnight force-seal: skipping (non-trading day)");
                continue;
            }

            let Some(sender) = global_seal_sender() else {
                tracing::warn!("IST-midnight force-seal: seal sender not installed — skipping");
                continue;
            };

            let mut sealed: u64 = 0;
            let mut dropped: u64 = 0;
            agg_for_boundary.force_seal_all(|security_id, segment_code, tf, mut state| {
                // 1d historical-only (operator directive 2026-06-02): D1 is
                // never tick-sealed — drop it at this write boundary too. See
                // the per-tick seal site above for the full rationale.
                if tf == TfIndex::D1 {
                    return;
                }
                let mut refs = prev_day_cache_for_boundary
                    .lookup(security_id, segment_code)
                    .unwrap_or_default();
                // Live prev-day close from the candle state (see per-tick
                // seal site above) — operator decision 2026-05-28.
                refs.prev_day_close = state.prev_day_close;
                stamp_seal_pct_fields(&mut state, refs);
                // G3 real-time proof (capture before move) — same contract as
                // the per-tick seal site above.
                let close_pct_nonzero = state.close_pct_from_prev_day != 0.0;
                let seal = BufferedSeal::new(security_id, segment_code, tf, state);
                if sender.try_send(seal).is_err() {
                    metrics::counter!("tv_seal_mpsc_dropped_total").increment(1);
                    dropped = dropped.saturating_add(1);
                } else {
                    metrics::counter!("tv_aggregator_seals_emitted_total").increment(1);
                    sealed = sealed.saturating_add(1);
                    if close_pct_nonzero {
                        metrics::counter!("tv_aggregator_close_pct_nonzero_total").increment(1);
                    }
                }
            });
            tracing::info!(
                sealed,
                dropped,
                "IST-midnight force-seal complete — open buckets flushed"
            );
        }
    });
    tracing::info!("candle-engine #T1b — IST-midnight force-seal task spawned");
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
    trading_handle: Option<tokio::task::JoinHandle<()>>,
    otel_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    notifier: &std::sync::Arc<NotificationService>,
    config: &ApplicationConfig,
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

/// PR #3 (2026-05-19): Greeks enricher RETIRED. Under the 4-IDX_I
/// LOCKED_UNIVERSE there are no live option contracts on the WebSocket
/// to compute Greeks from. This stub returns `Option<NoopGreeksEnricher>::None`
/// so the tick processor's generic over `GreeksEnricher` keeps compiling
/// (concrete type provided; value is None so the enrich branch is dead code).
/// O(1) EXEMPT: cold path — called once at startup.
fn build_inline_greeks_enricher(
    _config: &ApplicationConfig,
    _subscription_plan: &Option<SubscriptionPlan>,
) -> Option<NoopGreeksEnricher> {
    info!("inline Greeks enricher retired (PR #3) — tick processor runs without greeks");
    None
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

    // PR-C (2026-05-26): historical-fetch source-scan guards removed —
    // the entire spawn_historical_candle_fetch chain is deleted.

    #[test]
    fn test_boot_helper_functions_callable() {
        let _ = compute_market_close_sleep("15:30:00");
        let _ = create_log_file_writer();
    }

    #[test]
    fn test_should_emit_questdb_reconnected_edge_semantics() {
        // Recovery edge: previously alerted disconnect + liveness now OK → emit.
        assert!(should_emit_questdb_reconnected(true, true));
        // Never alerted (no incident) + OK → do NOT emit a spurious recovery.
        assert!(!should_emit_questdb_reconnected(false, true));
        // Still down (liveness failing) → do NOT emit recovery, regardless of flag.
        assert!(!should_emit_questdb_reconnected(true, false));
        assert!(!should_emit_questdb_reconnected(false, false));
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
    fn test_format_instrument_load_telegram_warm_skip() {
        let msg = format_instrument_load_telegram(412, true, 219_431)
            .expect("measured boot must produce a message");
        assert!(msg.contains("0.4s"), "elapsed rendered: {msg}");
        assert!(msg.contains("unchanged"), "warm-skip phrasing: {msg}");
        assert!(msg.contains("219431"), "row count: {msg}");
        // Telegram commandments: no library names / file paths.
        assert!(!msg.contains("rkyv") && !msg.contains(".rs") && !msg.contains("QuestDB"));
    }

    #[test]
    fn test_format_instrument_load_telegram_full_rebuild() {
        let msg = format_instrument_load_telegram(6234, false, 219_431)
            .expect("measured boot must produce a message");
        assert!(msg.contains("6.2s"), "elapsed rendered: {msg}");
        assert!(msg.contains("full rebuild"), "cold-path phrasing: {msg}");
    }

    #[test]
    fn test_trading_day_gate_exit_code_trading() {
        // Trading day → 0 so the gate lets the app start.
        assert_eq!(trading_day_gate_exit_code(true), 0);
    }

    #[test]
    fn test_trading_day_gate_exit_code_non_trading() {
        // Weekend/holiday → 75 so the gate self-stops the instance.
        assert_eq!(trading_day_gate_exit_code(false), 75);
        // 75 must be distinct from the fail-open load-error code 70.
        assert_ne!(trading_day_gate_exit_code(false), 70);
    }

    #[test]
    fn test_format_instrument_load_telegram_sentinel_suppressed() {
        // u64::MAX == "not measured this boot" (e.g. Indices4Only scope) →
        // no Telegram at all.
        assert!(format_instrument_load_telegram(u64::MAX, false, 0).is_none());
    }

    #[test]
    fn test_format_instrument_load_telegram_sub_100ms() {
        // A warm-skip can be tens of ms — render as 0.0s, never panic.
        let msg = format_instrument_load_telegram(37, true, 4).expect("message");
        assert!(msg.contains("0.0s"), "sub-100ms rendered: {msg}");
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

    // Candle-engine re-architecture #T1b: `test_live_candle_no_ist_offset`
    // removed — Engine A (`LiveCandleWriter` / `compute_live_candle_nanos`
    // → `candles_1s`) is deleted. Engine B's seal timestamps derive from
    // `TfIndex::bucket_start(tick.exchange_timestamp)` (already IST epoch
    // seconds, no offset) — covered by `aggregator_cell` + `seal_ring` tests.

    // PR-E (2026-05-26): test_historical_candle_adds_ist_offset retired
    // alongside the deleted candle_persistence module.

    // PR #3 (2026-05-19): `test_greeks_pipeline_adds_ist_offset` retired
    // alongside the deleted `greeks_pipeline.rs` file.

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

    /// Q6 regression (2026-04-24) RETIRED — PR #4 (2026-05-19) deleted
    /// the depth-20/200 spawn loops entirely per operator lock 2026-05-15
    /// (websocket-connection-scope-lock.md). The guard no longer has a
    /// call site to protect because the depth pipelines are gone.
    #[test]
    fn test_depth_200_deferred_spawn_retired() {
        // Build the banned literal at runtime so the assertion itself
        // doesn't trip the source scan against `main.rs`.
        let banned = format!("{}{}", "spawn_depth_200", "_minimal_conn");
        let src = include_str!("main.rs");
        // Count occurrences — the runtime-built `banned` string appears
        // once (here) by virtue of the `format!` arguments, so the
        // tolerated baseline is "no real call sites" not "zero matches".
        let occurrences = src.matches(banned.as_str()).count();
        assert_eq!(
            occurrences, 0,
            "PR #4 (2026-05-19) retired the depth-200 spawn — that helper \
             function must NOT reappear without operator approval"
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

/// Spawn the scheduled daily tasks — end-of-day digest (15:31:30 IST), the
/// 1-minute cross-verify (15:31:00 IST), and the REST-health canary
/// (09:05 / 12:00 / 15:25 IST — DHAN-REST-400, 2026-06-10). Called from BOTH
/// boot paths so a mid-session fast-boot restart runs them too
/// (boot-symmetry, 2026-06-09). Each spawned task self-skips if past its IST
/// trigger(s) or on a non-trading day, so calling this from a late/
/// non-trading boot is safe (no-op).
fn spawn_post_market_tasks(
    notifier: std::sync::Arc<NotificationService>,
    health_status: SharedHealthStatus,
    trading_calendar: std::sync::Arc<TradingCalendar>,
    token_handle: TokenHandle,
    config: &ApplicationConfig,
) {
    // Phase 0 Item 22d (2026-05-15): End-of-day digest at
    // 15:31:30 IST — 90s after the 15:30 close so the
    // market-close shutdown signal has settled. Severity::Info
    // — never pages, only a daily positive ping that the feed
    // stayed up + token has overnight headroom.
    //
    // Audit-findings Rule 3 (market-hours-aware): trading-day
    // check + skip silently if past 15:31:30 IST (mid-evening
    // boot legitimately runs past this point).
    //
    // Operator-charter §G: plain-English action line when the
    // JWT will expire before tomorrow's opening bell.
    {
        let eod_notifier = notifier.clone();
        let eod_health = health_status.clone();
        let eod_calendar = std::sync::Arc::clone(&trading_calendar);
        let eod_main_feed_total = tickvault_common::config::effective_main_feed_pool_size(
            config.subscription.scope,
            config.dhan.max_websocket_connections,
        );
        tokio::spawn(async move {
            use chrono::{FixedOffset, NaiveTime, TimeZone, Utc};
            use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
            let Some(ist_offset) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
                return;
            };
            let now_ist = ist_offset.from_utc_datetime(&Utc::now().naive_utc());
            let today_ist = now_ist.date_naive();
            if !eod_calendar.is_trading_day(today_ist) {
                info!("end-of-day digest: skipping (non-trading day)");
                return;
            }
            let Some(target) = NaiveTime::from_hms_opt(15, 31, 30) else {
                return;
            };
            let now_time = now_ist.time();
            if now_time >= target {
                // Mid-evening boot past 15:31:30 — skip silently
                // (audit-findings Rule 3).
                debug!(
                    now = %now_time,
                    "end-of-day digest: skipping (past 15:31:30 — mid-evening boot)"
                );
                return;
            }
            let secs_until = (target - now_time).num_seconds().max(0) as u64;
            info!(secs_until, "end-of-day digest: sleeping until 15:31:30 IST");
            tokio::time::sleep(std::time::Duration::from_secs(secs_until)).await;

            let main_active = eod_health.websocket_connections() as usize;
            let token_hours = eod_health.token_remaining_secs() / 3600;
            let trading_date_ist = today_ist.format("%Y-%m-%d").to_string();
            info!(
                main_feed = main_active,
                token_remaining_hours = token_hours,
                trading_date = %trading_date_ist,
                "PROOF: end-of-day digest fired @ 15:31:30 IST"
            );
            eod_notifier.notify(NotificationEvent::EndOfDayDigest {
                trading_date_ist,
                main_feed_active: main_active,
                main_feed_total: eod_main_feed_total,
                token_remaining_hours: token_hours,
            });
        });
    }

    // Operator directive 2026-06-02: post-market 1-minute
    // cross-verification at 15:31:00 IST. For every subscribed SPOT
    // instrument, compare our live `candles_1m` OHLCV against Dhan's
    // authoritative intraday 1-minute candles, EXACT match, and write
    // mismatches to the `cross_verify_1m_audit` table + a per-day CSV
    // (`data/cross-verify/`). The per-day mismatch COUNT is the quality
    // signal. Cold path, fail-soft, market-hours-gated (audit Rule 3).
    if let Some(cv_universe) = tickvault_app::prev_day_ohlcv_boot::stashed_universe() {
        // Build owned spot targets HERE (universe Arc in scope) so the
        // task doesn't hold the Arc. Skip rows with an unparseable SID.
        let cv_targets: Vec<tickvault_app::cross_verify_1m_boot::CrossVerifyTarget> = cv_universe
            .subscription_targets
            .iter()
            .filter_map(|t| {
                t.csv_row.security_id.trim().parse::<i64>().ok().map(|sid| {
                    tickvault_app::cross_verify_1m_boot::CrossVerifyTarget {
                        security_id: sid,
                        segment: t.csv_row.segment.trim().to_string(),
                        symbol: t.csv_row.symbol_name.trim().to_string(),
                        instrument: tickvault_app::prev_day_ohlcv_boot::instrument_type_for_role(
                            t.role,
                        ),
                    }
                })
            })
            .collect();
        let cv_token = std::sync::Arc::clone(&token_handle);
        let cv_qcfg = config.questdb.clone();
        let cv_base = config.dhan.rest_api_base_url.clone();
        let cv_calendar = std::sync::Arc::clone(&trading_calendar);
        // Visibility directive 2026-06-10: the INNER task runs the
        // verification and returns a typed outcome; the OUTER supervisor
        // turns that outcome into the typed Telegram event so the daily
        // summary can never be silently dropped. Outcome contract:
        //   Ok(Some((date, summary))) → the run happened → summary event
        //   Ok(None)                  → legitimate skip (non-trading day /
        //                               past-trigger boot / no targets)
        //   Err(reason)               → internal failure → Aborted event
        //   JoinError::is_panic()     → task crashed → Aborted event
        //   JoinError cancelled       → graceful shutdown — no page
        let cv_inner = tokio::spawn(async move {
            use chrono::{FixedOffset, TimeZone, Timelike, Utc};
            use tickvault_app::cross_verify_1m_boot::{
                CrossVerifyStart, decide_cross_verify_start,
            };
            use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
            let Some(ist_offset) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
                // Effectively unreachable (constant offset) — surfaced as
                // an abort, never a silent skip.
                return Err("IST offset construction failed");
            };
            let now_ist = ist_offset.from_utc_datetime(&Utc::now().naive_utc());
            let today_ist = now_ist.date_naive();
            if cv_targets.is_empty() {
                info!("cross_verify_1m: no spot targets — skipping");
                return Ok(None);
            }
            // Operator on-demand override: `make cross-verify-now` sets
            // TICKVAULT_CROSS_VERIFY_NOW=1 to run the verification right
            // now (proving the pipeline without waiting for 15:31 IST on
            // a live trading day). Fail-soft: a forced run on a quiet day
            // just yields an empty/degraded report, never fabricated data
            // (and the summary event renders it honestly as
            // "nothing could be compared", never a green PASS).
            let force_now = std::env::var("TICKVAULT_CROSS_VERIFY_NOW")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            let now_secs_of_day = now_ist.time().num_seconds_from_midnight();
            let is_trading_day = cv_calendar.is_trading_day(today_ist);
            match decide_cross_verify_start(now_secs_of_day, is_trading_day, force_now) {
                CrossVerifyStart::SkipNonTradingDay => {
                    info!("cross_verify_1m: skipping (non-trading day)");
                    return Ok(None);
                }
                CrossVerifyStart::SkipPastTrigger => {
                    debug!(
                        now = %now_ist.time(),
                        "cross_verify_1m: skipping (past 15:31 — mid-evening boot)"
                    );
                    return Ok(None);
                }
                CrossVerifyStart::RunNow => {
                    info!(
                        "cross_verify_1m: TICKVAULT_CROSS_VERIFY_NOW set — running \
                                 on-demand NOW (operator dry-run)"
                    );
                }
                CrossVerifyStart::SleepThenRun(secs_until) => {
                    info!(secs_until, "cross_verify_1m: sleeping until 15:31:00 IST");
                    tokio::time::sleep(std::time::Duration::from_secs(secs_until)).await;
                }
            }

            // IST-midnight-as-epoch nanos (our candles_1m `ts` stores
            // IST wall-clock as an epoch — data-integrity.md). The day
            // window for the SELECT is [midnight, midnight+24h).
            let day_start_secs = today_ist
                .and_hms_opt(0, 0, 0)
                .map(|dt| dt.and_utc().timestamp())
                .unwrap_or(0);
            let day_start_ist_nanos = day_start_secs.saturating_mul(1_000_000_000);
            let run_ts_ist_nanos = Utc::now()
                .timestamp()
                .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS))
                .saturating_mul(1_000_000_000);

            let summary = tickvault_app::cross_verify_1m_boot::run_cross_verify_1m(
                &cv_targets,
                cv_token,
                cv_qcfg,
                cv_base,
                today_ist,
                day_start_ist_nanos,
                run_ts_ist_nanos,
                tickvault_app::cross_verify_1m_boot::default_csv_dir(),
            )
            .await;
            info!(
                instruments = summary.instruments_checked,
                compared = summary.stats.compared,
                mismatches = summary.stats.mismatches,
                missing = summary.stats.missing_ours,
                degraded = summary.degraded,
                "PROOF: cross_verify_1m fired @ 15:31:00 IST"
            );
            Ok(Some((today_ist, summary)))
        });
        let cv_notifier = notifier.clone();
        tokio::spawn(async move {
            match cv_inner.await {
                Ok(Ok(Some((cv_date, summary)))) => {
                    // The once-per-day deliverable (visibility directive
                    // 2026-06-10). Severity is data-dependent inside the
                    // event: Info only on a clean compared>0 run.
                    cv_notifier.notify(NotificationEvent::CrossVerify1mSummary {
                        trading_date_ist: cv_date.format("%Y-%m-%d").to_string(),
                        instruments: summary.instruments_checked,
                        compared: summary.stats.compared,
                        mismatches: summary.stats.mismatches,
                        missing: summary.stats.missing_ours,
                        degraded: summary.degraded,
                    });
                }
                // Legitimate skip — already logged by the inner task.
                Ok(Ok(None)) => {}
                Ok(Err(reason)) => {
                    error!(reason, "cross_verify_1m: task failed before running");
                    cv_notifier.notify(NotificationEvent::CrossVerify1mAborted {
                        detail: reason.to_string(),
                    });
                }
                Err(join_err) if join_err.is_panic() => {
                    error!(
                        %join_err,
                        "cross_verify_1m: task crashed before producing the daily summary"
                    );
                    cv_notifier.notify(NotificationEvent::CrossVerify1mAborted {
                        detail: format!("the check task crashed: {join_err}"),
                    });
                }
                Err(_) => {
                    // Cancellation during graceful shutdown (16:30 IST
                    // auto-stop, `make stop`) — normal teardown, NOT an
                    // abort. No page.
                    info!("cross_verify_1m: task cancelled during shutdown");
                }
            }
        });
        info!("cross_verify_1m: post-market verification task spawned");
    }

    // Operator task DHAN-REST-400 (2026-06-10): REST-health canary — one
    // cheap GET /v2/profile at 09:05 / 12:00 / 15:25 IST on trading days.
    // On non-2xx it pages HIGH (REST-CANARY-01) with the HTTP status, the
    // exact final URL (token-redacted) and the bounded secret-redacted
    // response body — so an 08:45-class REST death is known by 09:05, not
    // discovered at 15:33 by the cross-verify. Spawned from BOTH boot paths
    // (this fn is the shared site); self-skips on non-trading days and when
    // booted past 15:25 IST (audit Rule 3).
    {
        let canary_token = std::sync::Arc::clone(&token_handle);
        let canary_base = config.dhan.rest_api_base_url.clone();
        let canary_calendar = std::sync::Arc::clone(&trading_calendar);
        tokio::spawn(async move {
            use chrono::{FixedOffset, TimeZone, Timelike, Utc};
            use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
            let Some(ist_offset) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
                return;
            };
            let now_ist = ist_offset.from_utc_datetime(&Utc::now().naive_utc());
            let is_trading_day = canary_calendar.is_trading_day(now_ist.date_naive());
            tickvault_app::rest_canary_boot::run_rest_canary(
                canary_token,
                canary_base,
                is_trading_day,
                now_ist.time().num_seconds_from_midnight(),
            )
            .await;
        });
        info!("rest_canary: REST-health probe task spawned (09:05 / 12:00 / 15:25 IST)");
    }

    // Operator directive 2026-06-10 ("Go ahead to achieve zero tick loss"):
    // daily end-to-end tick-conservation audit at 15:40:00 IST. Reconciles
    // the WAL disk log (every frame Dhan delivered) against the processor
    // outcome counters (self-scraped from /metrics) and the QuestDB `ticks`
    // row count, then writes one forensic row to `tick_conservation_audit`.
    // Residual > 0 → error! TICK-CONSERVE-01 (Telegram). Cold path,
    // fail-soft, market-hours-gated (audit Rule 3). Runbook:
    // `.claude/rules/project/tick-conservation-audit-error-codes.md`.
    {
        let tc_qcfg = config.questdb.clone();
        let tc_metrics_port = config.observability.metrics_port;
        let tc_calendar = std::sync::Arc::clone(&trading_calendar);
        // Single source of truth for the WAL dir (shared with STAGE-C).
        let tc_wal_dir = tickvault_app::tick_conservation_boot::ws_wal_dir();
        tokio::spawn(async move {
            use chrono::{FixedOffset, TimeZone, Timelike, Utc};
            use tickvault_app::tick_conservation_boot::{
                ConservationStart, boot_covers_full_session, decide_conservation_start,
                run_tick_conservation_audit,
            };
            use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
            let Some(ist_offset) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
                return;
            };
            let boot_ist = ist_offset.from_utc_datetime(&Utc::now().naive_utc());
            let today_ist = boot_ist.date_naive();
            let boot_secs_of_day = boot_ist.time().num_seconds_from_midnight();
            let force_now = std::env::var("TICKVAULT_TICK_CONSERVE_NOW")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            let is_trading_day = tc_calendar.is_trading_day(today_ist);
            match decide_conservation_start(boot_secs_of_day, is_trading_day, force_now) {
                ConservationStart::SkipNonTradingDay => {
                    info!("tick_conservation: skipping (non-trading day)");
                    return;
                }
                ConservationStart::SkipPastTrigger => {
                    debug!(
                        now = %boot_ist.time(),
                        "tick_conservation: skipping (past 15:40 — mid-evening boot)"
                    );
                    return;
                }
                ConservationStart::RunNow => {
                    info!(
                        "tick_conservation: TICKVAULT_TICK_CONSERVE_NOW set — running \
                         on-demand NOW (operator dry-run)"
                    );
                }
                ConservationStart::SleepThenRun(secs_until) => {
                    info!(secs_until, "tick_conservation: sleeping until 15:40:00 IST");
                    tokio::time::sleep(std::time::Duration::from_secs(secs_until)).await;
                }
            }

            // IST day number for WAL attribution + the ticks-table window
            // (ts stores IST-epoch nanos — data-integrity.md).
            let now_utc_secs = Utc::now().timestamp();
            let ist_secs = now_utc_secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
            // APPROVED: epoch day number fits u64 trivially.
            let target_ist_day = ist_secs.max(0) as u64 / 86_400;
            let trading_date_ist_nanos = i64::try_from(target_ist_day)
                .unwrap_or(0)
                .saturating_mul(86_400)
                .saturating_mul(1_000_000_000);
            let run_ts_ist_nanos = ist_secs.saturating_mul(1_000_000_000);

            run_tick_conservation_audit(
                &tc_wal_dir,
                &tc_qcfg,
                tc_metrics_port,
                target_ist_day,
                trading_date_ist_nanos,
                run_ts_ist_nanos,
                boot_covers_full_session(boot_secs_of_day),
            )
            .await;
            info!("PROOF: tick_conservation audit fired @ 15:40:00 IST");
        });
        info!("tick_conservation: daily WAL-vs-DB audit task spawned");
    }
}
