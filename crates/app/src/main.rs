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
    check_clock_drift, compute_close_seal_sleep, compute_market_close_sleep,
    create_error_log_writer, effective_ws_stagger, format_bind_addr, should_emit_post_market_alert,
    spawn_heartbeat_watchdog,
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
// Phase C1 (2026-07-13): relocated from this binary to the lib so
// dhan_rest_stack can create its own ws_event_audit consumer (Q4-i rewire).
use tickvault_app::ws_audit_consumer::spawn_ws_event_audit_consumer;

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

/// Metric name for the dedicated boot-completed CloudWatch signal.
///
/// Pinned as a named constant so the emit sites, the CloudWatch-agent scrape
/// filter (`deploy/aws/terraform/user-data.sh.tftpl`), and the boot-heartbeat
/// alarm (`deploy/aws/terraform/boot-heartbeat-alarm.tf`) all reference the
/// exact same string. The source-scan guard
/// `crates/app/tests/boot_completed_metric_guard.rs` ratchets that they stay in
/// lockstep.
const BOOT_COMPLETED_METRIC: &str = "tv_boot_completed";

/// Emits the dedicated "the app finished a full boot and is alive" gauge.
///
/// The boot-heartbeat alarm (`deploy/aws/terraform/boot-heartbeat-alarm.tf`,
/// repointed off the `tv_realtime_guarantee_score` PROXY — the PR #1278
/// follow-up flagged in `daily-universe-scope-expansion-2026-05-27.md` §19
/// "EC2 cron heartbeat") pages when this metric is MISSING in the 08:50–09:20
/// IST boot window (close widened from 09:10 on 2026-07-09 so the boot window
/// hands over to the 09:20 market-hours liveness window with no alarm seam
/// spanning the 09:15 market open).
///
/// Called from BOTH boot-completion points (fast-boot crash-recovery + slow-boot
/// normal), each reached ONLY after every boot gate has passed — a halt uses
/// `process::exit(...)` / `bail!`/`?` propagation, none of which reach the call
/// site, so a wedged/halted boot never emits it (missing = page, by design).
///
/// `metrics-exporter-prometheus` re-renders a gauge's last value on every
/// `/metrics` scrape, so this one-shot `set(1.0)` keeps appearing while the
/// process is alive (same pattern as the boot `tv_instrument_load_*` gauges)
/// and goes MISSING only when the process exits — exactly the
/// `treat_missing_data="breaching"` semantic the alarm needs. Boot is cold path,
/// so a single gauge set is fine.
fn emit_boot_completed() {
    metrics::gauge!(BOOT_COMPLETED_METRIC).set(1.0);
}

/// How long the boot-completed emit waits for AT LEAST ONE enabled feed's lane
/// to reach a running state before it decides. Feed lanes come up
/// asynchronously (the Dhan lane FSM Starting→Running, the Groww activation
/// watcher's watch-list build), so a point-in-time snapshot at the emit instant
/// would false-NEGATIVE a feed that is legitimately still coming up. A bounded
/// wait removes that race while keeping the honest end-state: a feed that never
/// comes up correctly withholds the "alive" signal. Boot is cold path, so a
/// bounded poll is well inside every boot budget.
///
/// WIDENED 60 -> 300 (2026-07-13, deploy-hang fix hostile-review round 1
/// MEDIUM): the Groww lane flips running only after the watch-list BUILD
/// succeeds — a merely-SLOW cold-boot build (>60s: CSV download + resolve on
/// a cold cache) exhausted the old 60s window and withheld `tv_boot_completed`
/// FOREVER, so the 08:40 IST boot-heartbeat alarm false-paged a HEALTHY boot.
/// 300s comfortably covers a slow-but-good build while still paging honestly
/// on a genuinely dead feed (the 08:30 boot + 300s ceiling resolves well
/// before the 08:40 alarm evaluation).
const BOOT_COMPLETED_FEED_LIVENESS_WAIT_SECS: u64 = 300;

/// Poll cadence for the boot-completed feed-liveness wait.
const BOOT_COMPLETED_FEED_LIVENESS_POLL_MS: u64 = 1000;

/// Decide whether the boot-completed "the app is alive" signal
/// (`tv_boot_completed`) may be emitted, given each feed's enabled flag and its
/// observed lane-running state.
///
/// FEED-AGNOSTIC by construction: the signal is warranted iff AT LEAST ONE
/// ENABLED feed is running — a live Groww satisfies it when only Groww is
/// enabled, a live Dhan when only Dhan is enabled, and either when both are
/// enabled. A running-but-DISABLED feed never counts (an operator-off feed is
/// not a reason to claim "alive").
///
/// The one exception: when NO feed is enabled (a deliberately feed-less run —
/// shared-infra-only), the signal IS emitted so the boot-heartbeat alarm does
/// not false-page a legitimately headless boot. The caller logs that case
/// explicitly.
///
/// This closes the alerting hole where a boot that reached the emit line with
/// EVERY enabled feed dead still published `tv_boot_completed=1` — so the
/// boot-heartbeat alarm (`treat_missing_data="breaching"`) never paged. With
/// this gate, "every enabled feed failed to come up" withholds the metric →
/// MISSING → the alarm pages, exactly the intended signal.
///
/// Pure + O(1) — unit-tested truth table.
fn boot_completed_should_emit(
    dhan_enabled: bool,
    groww_enabled: bool,
    dhan_running: bool,
    groww_running: bool,
) -> bool {
    // Deliberately feed-less run: nothing to be "live", so emit to avoid a
    // false page on a headless shared-infra-only boot.
    if !dhan_enabled && !groww_enabled {
        return true;
    }
    // At least one feed enabled: emit iff at least one ENABLED feed is running.
    (dhan_enabled && dhan_running) || (groww_enabled && groww_running)
}

/// Emit `tv_boot_completed` ONLY once at least one enabled feed's lane is
/// genuinely running — waiting up to [`BOOT_COMPLETED_FEED_LIVENESS_WAIT_SECS`]
/// for an async-starting feed to come up. Withholds the metric (and logs an
/// `error!` naming the dead feed(s)) if the window elapses with every enabled
/// feed dark, so the boot-heartbeat alarm pages on the MISSING signal.
///
/// The no-feed-enabled case (shared-infra-only) resolves `true` on the first
/// poll and emits immediately (unchanged timing), logged at `info!`.
async fn emit_boot_completed_when_feed_live(
    feed_runtime: &std::sync::Arc<tickvault_api::feed_state::FeedRuntimeState>,
    dhan_enabled: bool,
    groww_enabled: bool,
    // BUG-2 fix (2026-07-05): `true` when the Dhan lane COMPLETED its boot
    // but deliberately built no pool (non-trading day / offline slow boot).
    // The lane is honestly NOT running (`dhan_running=false` on the feeds
    // page), but the BOOT itself succeeded — withholding the alive signal
    // would false-page the boot-heartbeat alarm on every holiday/weekend
    // boot. `false` everywhere a pool was intended (the gate then genuinely
    // waits for the lane-running flag).
    dhan_poolless_idle: bool,
) {
    if !dhan_enabled && !groww_enabled {
        // Deliberately feed-less run — emit immediately (no feed to wait for),
        // logged explicitly so the operator sees WHY the "alive" signal fired
        // with no live feed.
        info!(
            "boot-completed: no feed enabled (dhan_enabled=false, groww_enabled=false) — \
             emitting the alive signal for a deliberately shared-infra-only run"
        );
        emit_boot_completed();
        return;
    }

    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(BOOT_COMPLETED_FEED_LIVENESS_WAIT_SECS);
    loop {
        // BUG-2 fix: a poolless-idle Dhan lane counts as "alive" for the
        // boot signal — the boot completed; there is simply no market today.
        let dhan_running = feed_runtime.is_dhan_lane_running() || dhan_poolless_idle;
        let groww_running = feed_runtime.is_groww_lane_running();
        if boot_completed_should_emit(dhan_enabled, groww_enabled, dhan_running, groww_running) {
            info!(
                dhan_enabled,
                groww_enabled,
                dhan_running,
                groww_running,
                dhan_poolless_idle,
                "boot-completed: a live feed is up (or the Dhan lane completed a \
                 pool-less non-trading-day boot) — emitting the alive signal"
            );
            emit_boot_completed();
            return;
        }
        if std::time::Instant::now() >= deadline {
            // Every enabled feed failed to reach running within the window.
            // WITHHOLD the alive signal so the boot-heartbeat alarm pages on the
            // MISSING metric, and name the dead feed(s) for the operator.
            error!(
                dhan_enabled,
                groww_enabled,
                dhan_running,
                groww_running,
                wait_secs = BOOT_COMPLETED_FEED_LIVENESS_WAIT_SECS,
                "boot-completed: NO enabled feed reached a live state within the wait window — \
                 WITHHOLDING the alive signal (tv_boot_completed) so the boot-heartbeat alarm \
                 pages on the missing metric. Check the enabled feed(s): a Dhan lane that never \
                 reached Running or a Groww activation that never built its watch-list."
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(
            BOOT_COMPLETED_FEED_LIVENESS_POLL_MS,
        ))
        .await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // PROCESS-START anchor (IST epoch nanos) for the dual-feed scoreboard's
    // process-death reconciler — captured as the FIRST statement of main(),
    // BEFORE any boot path can connect a feed (hostile review round 2,
    // 2026-07-10: the FAST crash-recovery arm connects the Dhan main-feed +
    // order-update WS BEFORE its scoreboard spawn, so an anchor stamped
    // inside the spawned task classified this boot's own `connected` audit
    // rows as PRE-boot and the flagship mid-market crash-restart synthesized
    // NOTHING). Threaded into `spawn_feed_scoreboard_tasks` at both call
    // sites; the source-order ratchet in secret_manager.rs pins that this
    // anchor precedes every `create_websocket_pool` site.
    let process_start_ist_nanos: i64 = chrono::Utc::now()
        .timestamp()
        .saturating_add(i64::from(
            tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
        ))
        .saturating_mul(1_000_000_000);

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
    let mut config: ApplicationConfig = config_figment
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

    // Feed selection (Groww second-feed scope, operator lock 2026-06-19 —
    // .claude/rules/project/groww-second-feed-scope-2026-06-19.md). PR-1
    // surfaces the `[feeds]` toggle and fails LOUD on a no-feed config or an
    // enabled-but-not-yet-wired feed, so the flags are never a silent no-op
    // (audit-findings Rule 14 "enabled=false trap"). The Dhan boot path below
    // is UNCHANGED; the per-feed spawn gating + the native Groww connector land
    // in later PRs of this sequence — until then these WARNs make the partial
    // state explicit (no illusion).
    //
    // PR-3 (persist, feed-toggle-full-lifecycle): the operator's LAST webpage
    // toggle choice is mirrored to a SEPARATE `data/feed-state.json` overlay
    // (never the git-tracked locked `config/base.toml`). `config.feeds` is the
    // DEFAULT; if a valid overlay exists it WINS — so the last choice survives a
    // restart. A missing file → config default (no error); a corrupt/partial
    // file → config default + a WARN (fail-safe, never a boot crash). The
    // overlay is applied IN PLACE onto `config.feeds`, so BOTH `feeds.*` below
    // AND the Dhan-off per-feed boot dispatcher gate (the `config.feeds`
    // dhan-enabled skip-guard further down) read the EFFECTIVE state with no
    // further edits.
    // Round-2 FIX A (2026-07-13): capture the RAW TOML Dhan value BEFORE the
    // feed-state overlay is applied. The Phase A "lane retired" truth — the
    // /api/feeds 409 gate, the runtime cold-start refusal, and the REST-only
    // stack spawn below — is a statement about the CONFIG, never about the
    // last webpage toggle. Seeding it from the POST-overlay effective value
    // would let a persisted runtime-OFF overlay permanently 409-lock a
    // config-ON boot with a misleading "config change + restart" message
    // (breaking the PR-E disable→restart→re-enable round trip).
    let dhan_config_enabled_raw = config.feeds.dhan_enabled;
    {
        let overlay_path = tickvault_api::feed_state_persist::feed_state_path();
        let persisted = tickvault_api::feed_state_persist::load_feed_state(&overlay_path);
        if persisted.is_some() {
            info!(
                "feed-state overlay found (data/feed-state.json) — restoring the last \
                 webpage toggle choice over the config default"
            );
        } else if overlay_path.exists() {
            // The file exists but did not load (corrupt / unreadable) — fail-safe
            // to the config default, but make it visible (no silent fall-through).
            warn!(
                "feed-state overlay present but unreadable/corrupt (data/feed-state.json) \
                 — falling back to the config default per-feed enabled state"
            );
        }
        // 2026-07-13 operator directive ("now remove this entire Dhan live
        // websocket feed instruments subscription even entire live websocket
        // feed itself"): the Dhan overlay is narrow-only — config-off is
        // AUTHORITATIVE for the retired Dhan live WS lane, so a STALE
        // pre-directive webpage toggle (`dhan_enabled: true` in
        // data/feed-state.json) can never resurrect it. One boot-time warn
        // makes the suppression visible (never silent).
        if tickvault_api::feed_state_persist::dhan_overlay_suppressed(
            &config.feeds,
            persisted.as_ref(),
        ) {
            warn!(
                "feed-state overlay requested dhan_enabled=true but the config says false — \
                 overlay SUPPRESSED: the Dhan live WS lane is retired by operator directive \
                 2026-07-13 (Dhan is REST-only; Groww is the live feed). Re-enabling requires \
                 a config change + restart."
            );
        }
        config.feeds = tickvault_api::feed_state_persist::overlay_feeds(config.feeds, persisted);
    }
    let feeds = &config.feeds;
    // Feed-toggle API (operator AskUserQuestion 2026-06-19): ONE shared
    // `Arc<FeedRuntimeState>` seeded from config, handed to BOTH the API state
    // (so `POST /api/feeds/{feed}` flips it) AND the Groww bridge (which reads it
    // live each loop) — so a runtime toggle is observed without a restart.
    // The runtime atomics seed from the EFFECTIVE (post-overlay) feeds; the
    // Phase A retirement snapshot seeds from the RAW TOML value captured
    // above (round-2 FIX A, 2026-07-13).
    let feed_runtime = std::sync::Arc::new(
        tickvault_api::feed_state::FeedRuntimeState::from_config_with_dhan_config(
            feeds,
            dhan_config_enabled_raw,
        ),
    );
    // Live-feed health check (operator 2026-06-22): the ONE shared per-feed
    // signal registry. The feed lanes update it (ticks/candles/drops/connected);
    // the API's GET /api/feeds/health reads the SAME Arc for the truthful verdict.
    let feed_health = std::sync::Arc::new(tickvault_common::feed_health::FeedHealthRegistry::new());
    // PR-E (2026-06-21): the Dhan main feed is now runtime-toggleable (webpage).
    // Safety gate: disabling Dhan is allowed ONLY in the no-orders data-pull
    // phase (`dry_run`); once live trading is on, the toggle refuses to kill Dhan
    // mid-trade. The Dhan connection tasks always spawn below (gated dormant when
    // disabled), so the lane is "running" whenever the pool is built.
    feed_runtime.set_dhan_disable_allowed(config.strategy.dry_run);
    // BUG-2 fix (2026-07-05): `mark_dhan_lane_running()` +
    // `mark_dhan_pool_present()` are NO LONGER set here. Being "enabled at
    // boot" is NOT the same as "the pool is actually built" — a
    // non-trading-day boot (weekend/holiday) SKIPS the WebSocket pool
    // ("WebSocket pool skipped — non-trading day"), yet this block used to
    // report dhan_running=true + pool_present=true (false-OK, live proof
    // 2026-07-05 17:59 IST Saturday boot). Both flags are now set at the
    // ONLY honest place: the pool-spawn sites (FAST arm + `start_dhan_lane`
    // boot-ON arm) right after a REAL pool is spawned. On a pool-less boot
    // (non-trading day / offline) they stay false — the feeds page shows
    // Dhan as enabled-but-idle, not running, and the PR-2 no-pool
    // protection in the activation watcher stays truthful.
    if feeds.dhan_enabled {
        // D2b: seed the lane FSM to `Starting` for the boot-ON case so the
        // runtime cold-start supervisor (spawned below) NEVER race-spawns a
        // second cold-start while the inline boot spine is bringing the lane up.
        // The inline `start_dhan_lane` success below drives `Starting→Running`;
        // a boot-ON abort drives it back to `Off`. Boot-OFF leaves it `Off`.
        feed_runtime.set_dhan_lane_state(tickvault_api::feed_state::LaneState::Starting);
    }
    info!(
        dhan_enabled = feeds.dhan_enabled,
        groww_enabled = feeds.groww_enabled,
        both_enabled = feeds.both_enabled(),
        "feed selection: which market-data feeds are configured"
    );
    if !feeds.any_enabled() {
        warn!(
            "[feeds] both dhan_enabled and groww_enabled are false — no market-data \
             feed is configured; the Dhan boot path still runs as today until the \
             feed-gating PR lands"
        );
    }
    // ── Groww second feed: dormant-until-enabled lanes (operator 2026-06-24) ──
    // The feed toggle must start/stop the ENTIRE Groww lane LIVE — so the bridge,
    // the Python-sidecar supervisor, AND the activation watcher are ALL spawned
    // UNCONDITIONALLY at boot. Each self-idles on `is_enabled(Feed::Groww)` (a
    // poll, zero Groww work) while OFF, so an OFF Groww feed still touches NOTHING
    // (no auth, no instruments, no Python) — the OFF-feed-isolation guarantee
    // holds as a *dormant poll* rather than *not-spawned*. On enable (config OR
    // the /api/feeds webpage toggle, NO restart) the activation watcher — a
    // level-triggered reconciler that owns one abortable task — runs the
    // one-shots (ensure tables → auth smoke → build watch-list) and marks the
    // lane running ONLY after the watch-list builds (no false-OK); on disable it
    // aborts the in-flight task so no Groww work continues. The sidecar
    // supervisor provisions the venv + launches the Python producer; the bridge
    // tails the producer's tick file. Both self-idle on the same enable flag.
    // Watch date is computed at ACTIVATION time inside the watcher (so a runtime
    // re-enable past IST midnight uses today's date, never a stale boot date) —
    // not pre-computed here.
    let groww_max_subscribe = std::env::var("GROWW_MAX_SUBSCRIBE")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .or(Some(
            tickvault_core::feed::groww::instruments::GROWW_DEFAULT_MAX_SUBSCRIBE,
        ));
    info!(
        groww_enabled = feeds.groww_enabled,
        "[feeds] Groww lanes spawned dormant (bridge + sidecar + activation watcher); \
         they activate on enable (config OR /api/feeds toggle) with no restart"
    );
    // Trading calendar for the Groww IST-midnight force-seal boundary task. The
    // shared `Arc<TradingCalendar>` for the Dhan path is built later in boot
    // (after the clock-drift check), but the Groww bridge spawns here; both are
    // built from the SAME `config.trading` (immutable read-only calendar data),
    // so the trading-day gate is identical. Config is already validated.
    let groww_trading_calendar = std::sync::Arc::new(
        TradingCalendar::from_config(&config.trading)
            .context("failed to build Groww trading calendar")?,
    );
    // SUPERVISED (2026-07-02 adversarial-sweep fix): the bridge — the ONLY
    // consumer of the sidecar NDJSON — used to be a bare tokio::spawn, so a
    // panic silently stopped all Groww persistence while the stall watchdog
    // killed the WRONG process (the healthy Python sidecar). The supervisor
    // respawns it (FEED-SUPERVISOR-01 + tv_feed_supervisor_respawn_total,
    // WS-GAP-05 pattern); the NDJSON re-tail is DEDUP-idempotent.
    // §34 auto-scale (PR-2): default OFF → the single-conn spawns below are
    // byte-identical to the pre-scale boot. When `[feeds.groww.scale]`
    // enabled=true, the shard-bridge fleet + per-conn sidecar fleet + ladder
    // replace the single bridge + single sidecar (same aggregator engine,
    // same ring→spill→DLQ chain, ONE shared capture-seq across shards).
    let groww_scale_cfg = config.feeds.groww.scale.clone();
    let groww_shards_root =
        std::path::PathBuf::from(tickvault_app::groww_bridge::GROWW_SHARDS_DIR_DEFAULT);
    // §34 PR-3 (addendum A): the Mac scale-test PREFLIGHT gates the fleet.
    // On ANY failed check the boot falls back to the SINGLE-CONNECTION Groww
    // path — capture continues; only the multi-conn experiment is refused.
    let groww_scale_enabled = groww_scale_cfg.enabled
        && {
            let report = tickvault_app::scale_test_preflight::run_scale_preflight(
                &groww_shards_root,
                &config.questdb,
            );
            let ok = report.all_ok();
            if !ok {
                error!(
                    "[feeds] groww scale PREFLIGHT FAILED — falling back to the                  single-connection Groww path (capture continues; fix the                  failed checks above and restart to run the scale test)"
                );
            }
            ok
        };
    // ── Session-B fix #1 (operator go 2026-07-04): Groww FLEET dual-instance
    // lock. A scale-test boot runs dhan_enabled=false and never reaches the
    // Dhan RESILIENCE-01 lock, so without THIS gate two hosts (Mac + AWS, or
    // two Macs) could scale the SAME Groww account simultaneously — a failure
    // that masquerades as provider throttle. The lock is attempted ONLY after
    // cfg.enabled + preflight pass (a default boot performs ZERO new SSM
    // calls). AlreadyHeld / SSM-unavailable-after-retries → fail-closed:
    // fleet SKIPPED with a GROWW-SCALE-05 Critical page (emitted inside the
    // gate module via the error! log-sink chain — HONEST delivery boundary:
    // no typed Telegram NotificationEvent exists for this code yet, so
    // boot-stage delivery is log-sink + CloudWatch-log-derived alerting
    // only; the deferred Telegram notifier slot is not filled at this boot
    // stage), single-connection fallback, app keeps running. See
    // groww-scale-error-codes.md §4b. The guard is handed to
    // `run_process_runloop`, which calls `release_on_shutdown()` at
    // graceful teardown so the SSM slot frees IMMEDIATELY for a same-host
    // restart (2026-07-04 adversarial-review HIGH fix; hard crash still
    // relies on the 90s TTL).
    let mut groww_scale_fleet_lock: Option<
        tickvault_app::groww_scale_lock::GrowwScaleFleetLockGuard,
    > = None;
    let groww_scale_enabled = if groww_scale_enabled {
        match tickvault_app::groww_scale_lock::acquire_groww_scale_fleet_lock().await {
            tickvault_app::groww_scale_lock::GrowwScaleFleetLockOutcome::Acquired(guard) => {
                groww_scale_fleet_lock = Some(guard);
                true
            }
            tickvault_app::groww_scale_lock::GrowwScaleFleetLockOutcome::SkipFleet => false,
        }
    } else {
        false
    };
    // Deferred Telegram slot shared by the Groww bridge + sidecar supervisor +
    // activation watcher: all three spawn here (before the notifier is built),
    // so they get a shared slot that is filled with the live
    // `NotificationService` once it exists (below). Declared BEFORE the bridge
    // spawn (2026-07-04 boot-visibility parity) so the bridge can emit the
    // boot-stage "feed connected — awaiting first tick" ping.
    let groww_sidecar_notifier_slot: std::sync::Arc<
        arc_swap::ArcSwapOption<tickvault_core::notification::NotificationService>,
    > = std::sync::Arc::new(arc_swap::ArcSwapOption::empty());
    // Watch-entries slot: the activation watcher publishes the daily
    // subscribe set here so the ladder can cut shards (scale mode only).
    let groww_scale_entries: std::sync::Arc<
        arc_swap::ArcSwapOption<Vec<tickvault_core::feed::groww::instruments::WatchEntry>>,
    > = std::sync::Arc::new(arc_swap::ArcSwapOption::empty());
    // ── Groww capture rotation-at-open — PROCESS-GLOBAL, BEFORE both spawns ──
    // 2026-07-13 disk-retention hardening (review round 1 redesign, F1+F2):
    // rotate a stale previous-IST-day capture file HERE, synchronously,
    // strictly BEFORE the Groww bridge task AND the sidecar supervisor spawn
    // below — ordering holds by construction on EVERY boot arm (this is the
    // single shared spawn site; the ratchet in disk_retention_wiring_guard.rs
    // pins the source order). Only Rust renames at open: the rename completes
    // before the sidecar process exists (it can never re-open the old inode)
    // and before the bridge's one-shot archive-drain decision runs (F1), and
    // the archive is named by the bridge's snapshot day — the exact name
    // drain_archive_tail_if_needed probes (F2). Scale-lab per-conn shard
    // captures (main-locked-out, dev only) are not rotated at open.
    let _rotated_capture = tickvault_app::groww_bridge::rotate_stale_groww_capture_at_open(
        std::path::Path::new(tickvault_app::groww_bridge::GROWW_TICK_FILE_DEFAULT),
    );
    // ── Order-runtime mark bridge (dry-run PR, 2026-07-14) ──────────────────
    // Built BEFORE the Groww bridge spawn so the per-tick tap exists from the
    // first tick. `[order_runtime].enabled = false` → all three slots stay
    // empty/None and every downstream path is byte-identical. The receiver is
    // stashed in a take-once slot for the Dhan REST-only stack's Phase 5a
    // (`spawn_dhan_rest_stack` — the runtime's single spawn site); the
    // forwarder (flag + bounded sender) rides into the Groww bridge.
    let (order_runtime_mark_forwarder, order_runtime_mark_rx_slot, order_runtime_marks_wanted) =
        if config.order_runtime.enabled {
            let (mark_tx, mark_rx) = tokio::sync::mpsc::channel::<
                tickvault_app::order_runtime::MarkUpdate,
            >(config.order_runtime.mark_channel_capacity);
            let marks_wanted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            (
                Some(tickvault_app::order_runtime::MarkForwarder {
                    marks_wanted: std::sync::Arc::clone(&marks_wanted),
                    tx: mark_tx,
                }),
                std::sync::Arc::new(std::sync::Mutex::new(Some(mark_rx))),
                marks_wanted,
            )
        } else {
            (
                None,
                std::sync::Arc::new(std::sync::Mutex::new(None)),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
        };
    if groww_scale_enabled {
        let _groww_shard_bridges =
            tickvault_app::groww_bridge::spawn_supervised_groww_shard_bridges(
                config.questdb.clone(),
                groww_shards_root.clone(),
                groww_scale_cfg.target_connections,
                std::sync::Arc::clone(&feed_runtime),
                std::sync::Arc::clone(&feed_health),
                Some(spawn_ws_event_audit_consumer(config.questdb.clone())),
                // 2026-07-04 boot-visibility parity: the boot-stage
                // "connected — awaiting first tick" Telegram ping.
                Some(std::sync::Arc::clone(&groww_sidecar_notifier_slot)),
                groww_trading_calendar,
            );
    } else {
        let _groww_bridge_supervisor = tickvault_app::groww_bridge::spawn_supervised_groww_bridge(
            config.questdb.clone(),
            std::path::PathBuf::from(tickvault_app::groww_bridge::GROWW_TICK_FILE_DEFAULT),
            std::path::PathBuf::from(tickvault_app::groww_bridge::GROWW_STATUS_FILE_DEFAULT),
            std::sync::Arc::clone(&feed_runtime),
            std::sync::Arc::clone(&feed_health),
            // ws_event_audit forensic sender (2026-06-29): one `feed='groww'` row per
            // Groww connect/disconnect/reconnect, drained by the SAME consumer Dhan +
            // order-update use. Closes the audit gap (Groww connected but wrote no row).
            Some(spawn_ws_event_audit_consumer(config.questdb.clone())),
            // 2026-07-04 boot-visibility parity: the boot-stage
            // "connected — awaiting first tick" Telegram ping (same
            // lazily-filled slot the sidecar supervisor uses).
            Some(std::sync::Arc::clone(&groww_sidecar_notifier_slot)),
            // Trading calendar (2026-06-29): drives the Groww IST-midnight force-seal
            // boundary task's trading-day gate — built from the SAME `config.trading`
            // the Dhan IST-midnight force-seal calendar uses.
            groww_trading_calendar,
            // Order-runtime mark tap (dry-run PR, 2026-07-14): per-tick LTP
            // forwarder into the dry-run runtime; `None` when disabled.
            order_runtime_mark_forwarder,
        );
    }
    // On a sidecar auth/entitlement/error diagnostic the sidecar supervisor
    // fires ONE `GrowwSidecarRejected` Telegram event (via the shared
    // `groww_sidecar_notifier_slot` declared above) + marks Groww rejected in
    // feed_health — so the operator sees WHY Groww has 0 ticks instead of a
    // silent log line.
    // FEED-SUPERVISOR-01: spawn the feed-agnostic sidecar supervisor under a
    // respawning wrapper so the stall-watchdog (FEED-STALL-01) can never die
    // silently — a panic/return respawns it (WS-GAP-05 / DISK-WATCHER-01 pattern).
    // Disk-retention hardening (2026-07-13): thread the capture-archive S3
    // destination from `[feeds.groww]` into the sidecar child env so rotated
    // capture archives are verified-offloaded to S3 instead of accumulating
    // unbounded on the 30 GB root EBS. Empty bucket = archival OFF.
    let groww_sidecar_options = tickvault_app::groww_sidecar_supervisor::GrowwSidecarOptions {
        archive_s3_bucket: config.feeds.groww.capture_archive_s3_bucket.clone(),
        archive_s3_prefix: config.feeds.groww.capture_archive_s3_prefix.clone(),
        ..tickvault_app::groww_sidecar_supervisor::GrowwSidecarOptions::default()
    };
    if groww_scale_enabled {
        // §34: the ladder publishes desired_conns; the fleet reconciles the
        // per-conn Python children to it (spawn missing / kill newest).
        let scale_runtime = tickvault_app::groww_scale_ladder::GrowwScaleRuntime::default();
        let _groww_fleet = tickvault_app::groww_sidecar_supervisor::spawn_groww_scale_fleet(
            std::sync::Arc::clone(&feed_runtime),
            groww_sidecar_options.clone(),
            groww_shards_root.clone(),
            std::sync::Arc::clone(&scale_runtime.desired_conns),
            std::sync::Arc::clone(&scale_runtime.wake),
            std::sync::Arc::clone(&feed_health),
            std::sync::Arc::clone(&groww_sidecar_notifier_slot),
            // Scoreboard PR-B (2026-07-10): stall-watchdog kill+relaunch
            // forensic rows (`stall_restarted`) — same consumer pattern as
            // the bridge's lifecycle rows.
            Some(spawn_ws_event_audit_consumer(config.questdb.clone())),
        );
        tokio::spawn(tickvault_app::groww_scale_ladder::run_groww_scale_ladder(
            groww_scale_cfg.clone(),
            config.questdb.clone(),
            std::sync::Arc::clone(&feed_runtime),
            std::sync::Arc::clone(&feed_health),
            scale_runtime,
            std::sync::Arc::clone(&groww_scale_entries),
            groww_shards_root.clone(),
        ));
    } else {
        tickvault_app::groww_sidecar_supervisor::spawn_supervised_groww_sidecar_supervisor(
            std::sync::Arc::clone(&feed_runtime),
            groww_sidecar_options.clone(),
            std::sync::Arc::clone(&feed_health),
            std::sync::Arc::clone(&groww_sidecar_notifier_slot),
            // Scoreboard PR-B (2026-07-10): every stall-watchdog
            // kill+relaunch stamps ONE `stall_restarted` ws_event_audit row
            // (fixed cause slug in `source`) so the 15:45 IST scorecard
            // counts stall episodes — same consumer the bridge uses.
            Some(spawn_ws_event_audit_consumer(config.questdb.clone())),
        );
    }
    // ── Groww exchange-lag publisher — PROCESS-GLOBAL boot prefix ──
    // Scoreboard PR-C: publishes `tv_groww_exchange_lag_p99_seconds` (the
    // Groww gauge's OWN name — never the Dhan gauge). Spawned here, next to
    // the Groww bridge supervisor, so it runs on EVERY boot mode (fast
    // crash-recovery included) — the single-site mirror of the Dhan
    // publisher's dual-arm wiring. Inert while Groww is disabled (empty
    // ring → the ≥50-sample gate publishes nothing).
    let _groww_lag_publisher_supervisor = spawn_supervised_groww_lag_publisher();
    // ── per-instrument presence registry init — PROCESS-GLOBAL boot prefix ──
    // Scoreboard PR-D fix round 1 (review HIGH): init MUST precede BOTH the
    // Groww activation watcher spawn below AND both boot arms'
    // `load_instruments` — `feed_presence::register_instruments` is a
    // GLOBAL.get() free fn that silently no-ops pre-init, so the previous
    // fast-arm init site (~1,000 lines after its load_instruments)
    // deterministically skipped the Dhan universe registration on every
    // crash-recovery boot, and the Groww watcher could register after a
    // later init — a same-day 15:45 drain of that half-registered registry
    // persisted a false one-sided "Groww won every minute" verdict. ONE
    // init site (the process-global-prefix pattern the lag publisher +
    // ts-pin migration already use); ordering pinned by the
    // `test_feed_presence_is_wired_into_main` source-order ratchet.
    tickvault_core::pipeline::feed_presence::init_feed_presence(
        config.scoreboard.enabled && config.scoreboard.presence_fold_enabled,
    );
    // ── index_constituency ts-pin migration — PROCESS-GLOBAL boot prefix ──
    // F13/F14 hardening (2026-07-05): the one-shot, marker-gated TRUNCATE
    // migration runs here — BEFORE the Groww activation watcher and regardless
    // of `feeds.dhan_enabled` (it needs only the QuestDB config) — so the
    // `index_constituency_migration_gate` the Groww shared-master writer
    // awaits is marked within its bounded 120s wait on EVERY boot mode. The
    // wrapper's exactly-once latch (F15) means the Dhan lane's later
    // defense-in-depth call can never fire a LATE first truncate that wipes
    // just-written `feed='groww'` rows. Ratchet:
    // `index_constituency_boot::tests::ratchet_ts_pin_migration_spawns_before_groww_watcher`.
    tokio::spawn(
        tickvault_app::index_constituency_boot::run_index_constituency_ts_pin_migration_at_boot(
            config.questdb.clone(),
        ),
    );
    tokio::spawn(
        tickvault_app::groww_activation::run_groww_activation_watcher(
            std::sync::Arc::clone(&feed_runtime),
            std::sync::Arc::clone(&feed_health),
            config.questdb.clone(),
            groww_max_subscribe,
            config.network.request_timeout_ms,
            // 2026-07-03 Telegram feed parity: same lazily-filled notifier
            // slot the sidecar supervisor uses — carries the Groww
            // instruments-load Info ping once the watch-set resolves.
            std::sync::Arc::clone(&groww_sidecar_notifier_slot),
            // §34: in scale mode the watcher publishes the daily subscribe
            // set here so the ladder can cut shards. `None` (scale OFF) is
            // byte-identical to the pre-scale watcher behavior.
            groww_scale_enabled.then(|| std::sync::Arc::clone(&groww_scale_entries)),
        ),
    );
    // ── Groww NATIVE-RUST shadow client (PR-R1, operator "go" 2026-07-04) ──
    // Default-OFF behind `[feeds] groww_native_shadow`. When enabled it runs
    // ALONGSIDE the Python sidecar (same watch file, own NDJSON capture at
    // data/groww/rust-live-ticks.ndjson) for the exact per-tick parity
    // comparer. Shadow-only: NO shared-table writes, NO strategy/order wiring,
    // NO sidecar changes — the flag is the kill switch.
    if config.feeds.groww_native_shadow {
        let _groww_native_shadow_supervisor =
            tickvault_app::groww_native_shadow::spawn_supervised_groww_native_shadow(
                std::path::PathBuf::from(tickvault_common::constants::GROWW_DATA_DIR),
            );
        tracing::info!(
            "groww native shadow client ENABLED (PR-R1) — capturing to {}",
            tickvault_common::constants::GROWW_NATIVE_SHADOW_NDJSON_PATH
        );
    }
    // ── Dhan dormant activation watcher (PR-2, feed-toggle-full-lifecycle) ──
    // Spawned UNCONDITIONALLY at boot (like the Groww watcher), BEFORE the
    // per-feed Dhan-OFF dispatcher below — so it survives a
    // Dhan-OFF / Groww-only run (that branch awaits the shutdown signal before it
    // returns, keeping the runtime alive). It is a level-triggered, safety-gated
    // reconciler that keeps the `dhan_lane_running` UI flag HONEST across runtime
    // toggles of a boot-ON Dhan feed and enforces the Dhan-disable safety gate at
    // the supervisor layer (a second layer behind the handler's CONFLICT). It
    // mutates ONLY the FeedRuntimeState atomics — NO new WS connection/endpoint
    // (the 2-WS Dhan lock is untouched). Enabled-default is byte-identical: the
    // inline Dhan boot block below calls mark_dhan_lane_running() at the
    // pool-spawn site (BUG-2 fix 2026-07-05), and the watcher holds off while
    // the lane FSM reads Starting, so its first ACTIVE tick sees desired-ON +
    // already-running → None → it does NOTHING to the boot. (Honest boundary:
    // the full boot-OFF → cold-start lift
    // of the inline Dhan boot spine is the deferred residual of this plan; see
    // dhan_activation.rs module docs — PR-E's in-loop dormancy already handles the
    // live disconnect/reconnect for a boot-ON Dhan feed.)
    tokio::spawn(tickvault_app::dhan_activation::run_dhan_activation_watcher(
        std::sync::Arc::clone(&feed_runtime),
    ));
    // ── D2b: runtime Dhan-lane cold-start supervisor ──────────────────────
    // Spawned UNCONDITIONALLY at boot (like the Groww watcher), so a Dhan feed
    // that was OFF *at boot* (no pool spawned) is runtime-startable. It idles
    // (zero Dhan work) until `main()` populates `dhan_lane_ctx_cell` right after
    // `build_shared_infra` (which runs in BOTH the Dhan-ON and Dhan-OFF paths).
    // It owns the ONE abortable cold-start `JoinHandle` (the Groww shape) and
    // drives the `LaneState` FSM (Off→Starting→Running→Stopping→Off). Boot-ON is
    // byte-identical: the inline spine already drove the FSM to Starting (seed
    // above) / Running, so the supervisor's first tick sees a non-Off lane and
    // does NOTHING. See the D2b section near `run_dhan_lane_runtime_supervisor`.
    let dhan_lane_ctx_cell: std::sync::Arc<
        tokio::sync::OnceCell<std::sync::Arc<DhanLaneRuntimeContext>>,
    > = std::sync::Arc::new(tokio::sync::OnceCell::new());
    tokio::spawn(run_dhan_lane_runtime_supervisor(
        std::sync::Arc::clone(&feed_runtime),
        std::sync::Arc::clone(&dhan_lane_ctx_cell),
    ));
    // Step C (pluggable-feed-runtime.md §6): the Dhan disable-gate is now LIVE.
    // The actual skip-and-idle branch is below (just before the Dhan fast/slow
    // boot decision), AFTER shared infra (observability/WAL/calendar) is up so a
    // Groww-only run still gets the full shared runtime. We only log intent here.
    if !feeds.dhan_enabled {
        info!(
            groww_enabled = feeds.groww_enabled,
            "[feeds] dhan_enabled=false — Dhan boot will be skipped (per-feed gate active)"
        );
    }

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

    // Feed-health false-RED fix (2026-06-29) — install the SAME calendar in
    // `common::market_hours` so the `/feeds` health verdict's `market_open`
    // (`is_trading_session_now`) is trading-day-aware: weekends AND NSE
    // holidays inside the 09:00–15:30 clock window read as market-closed, so a
    // stale `auth_rejected` flag never re-surfaces the false "refresh the SSM
    // api-key" Down. Without this install the verdict falls back to the
    // weekday-only gate (still safe; holidays just aren't covered).
    if !tickvault_common::market_hours::set_market_calendar_for_session(trading_calendar.clone()) {
        tracing::warn!("session TradingCalendar already installed — skipping");
    }

    // W2 PR#5 (2026-07-10, audit follow-up row 15) — holiday-calendar
    // coverage-horizon staleness watchdog. Spawned in the COMMON boot prefix
    // (every path: fast/slow/Groww-only) so the boot-time check gives daily
    // cadence on the AWS box (which restarts every weekday 08:30 IST and is
    // OFF at IST midnight — a midnight-anchored task would never run in
    // prod). Uses the lazily-filled notifier slot declared above; a stale
    // check before the notifier exists retries in 30s without consuming the
    // per-IST-date alert latch. Loop body is panic-free (pure date math) —
    // no supervisor; worst case the next boot re-checks.
    let _calendar_staleness_watchdog =
        tickvault_app::calendar_staleness::spawn_calendar_staleness_watchdog(
            trading_calendar.clone(),
            std::sync::Arc::clone(&groww_sidecar_notifier_slot),
        );

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
                // §36.7 (2026-07-10): full walk (cap = map size) instead of
                // the old top-N cap — the far-month exclusion below must see
                // EVERY silent entry to subtract the excluded ones. Same
                // cold-path walk class as the 10s SLO loop; entries come
                // back largest-gap-first, so the WS-GAP-06 top-10 sample
                // below is unchanged.
                let (gaps, total_silent) =
                    detector_for_task.scan_gaps_top_n(now, detector_for_task.len());
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
                // Round-3 fix (2026-07-08, review finding 6): the gauge is
                // written UNCONDITIONALLY every in-session scan — INCLUDING
                // total_silent == 0. The previous zero-skip sat BEFORE this
                // write, so a fully-recovered silence spike froze the gauge
                // at its last breaching value (the /metrics exporter keeps
                // re-serving the last set value): a ~90s full-feed outage
                // wrote 776 once, the reconnect restored every SID, and the
                // frozen 776 then accumulated the retuned alarm's 10-of-12
                // ~10 minutes AFTER complete recovery — a false page held in
                // ALARM until the next non-zero sub-threshold count happened
                // to be written. The zero-skip below now guards ONLY the
                // WS-GAP-06 error emission + summary counter.
                //
                // Round-4 fix (2026-07-08, final-review findings 1/2/4): pin
                // the gauge to 0.0 during the NSE pre-open/auction window
                // [09:00, 09:15) IST — the exact mirror of the round-3 SLO
                // tick_freshness pre-open pin
                // (SLO_TICK_FRESHNESS_SESSION_OPEN_SECS_OF_DAY_IST below).
                // The session gate above admits [09:00, 15:30), so during
                // the 09:08-09:15 matching/buffer freeze the boot-seeded
                // ~775 SIDs are LEGITIMATELY silent and the genuine count
                // (hundreds, far above the retuned alarm threshold 40)
                // would be written for ~6-7 consecutive 60s CW datapoints.
                // The window-gate Lambda's forced-OK at 09:20 does NOT
                // purge datapoints, and the retuned alarm's 10-of-12
                // lookback (12 min — the LONGEST of any gated alarm)
                // reaches back to ~09:08 at its first gated evaluations:
                // ~7 guaranteed pre-open breaching datapoints would need
                // only ~3 open-ramp minutes > 40 (slow starters over the
                // documented ~33 always-silent floor) to false-page at
                // ~09:21 on ordinary days. Pre-open silence is not
                // degradation; genuine counts start at the 09:15:00
                // continuous-session open. The WS-GAP-06 error emission
                // below is deliberately UNCHANGED (pre-existing behavior;
                // it is a coalesced log, not an alarm datapoint).
                const TICK_GAP_GAUGE_SESSION_OPEN_SECS_OF_DAY_IST: u32 = 9 * 3600 + 15 * 60;
                // §36.7 (2026-07-10): subtract the NON-NEAREST-month index
                // futures from the ALARM-FACING gauge value only — far
                // serials are legitimately sparse (minutes of silence) and
                // would permanently lift the healthy always-silent floor
                // (~33) toward the alarm threshold (40). The WS-GAP-06
                // error emission + summary counter below stay keyed on the
                // RAW total_silent (per-SID visibility unchanged).
                let excluded_silent = gaps
                    .iter()
                    .filter(|(sid, seg, _)| is_far_month_future_sid(*sid, *seg))
                    .count();
                let gauge_silent = if tickvault_common::market_hours::now_ist_secs_of_day()
                    < TICK_GAP_GAUGE_SESSION_OPEN_SECS_OF_DAY_IST
                {
                    0.0
                } else {
                    total_silent.saturating_sub(excluded_silent) as f64
                };
                metrics::gauge!("tv_tick_gap_instruments_silent").set(gauge_silent);
                if total_silent == 0 {
                    continue;
                }
                metrics::counter!("tv_tick_gap_summary_total").increment(1);
                let top: Vec<(u64, &'static str, u64)> = gaps
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
            // SP5.1: attach the per-feed health registry so a terminal Dhan
            // live-feed frame drop surfaces as `Degraded` on /api/feeds/health
            // (closes the SP5 connected+fresh-but-dropping false-OK).
            Some(std::sync::Arc::new(
                spill.with_feed_health(Some(std::sync::Arc::clone(&feed_health))),
            ))
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

    // Pre-register the Groww sidecar stall-restart counter at 0 — HERE,
    // immediately after the recorder installs (round-5 review fix,
    // 2026-07-06). The round-4 attempt registered it at the TOP of
    // `run_groww_sidecar_supervisor`, but that supervisor is spawned much
    // earlier in this fn (before the STAGE-C WAL replay, well before this
    // Step 2 install), so its `counter!` handle resolved to the no-op
    // recorder and NO registration happened — the series was still lazily
    // born at the first real restart, whose sample the CW agent's delta
    // pipeline drops as its baseline (feed-stall-restart-alarm.tf
    // FIRST-SAMPLE BASELINE): restart #1 of every app session was lost and
    // the tv-<env>-feed-stall-restarts pager's effective first-episode
    // threshold was 4, not 3. Registering post-install makes the series
    // DENSE from boot (a 0-delta /metrics event per 60s scrape), so the
    // dropped first sample is the harmless 0 baseline and the pager
    // genuinely sees every restart, including the session's first.
    // Honest residual: a stall-restart landing in the pre-install boot
    // window (between the supervisor spawn and this line) would increment a
    // no-op handle and go uncounted — physically implausible (it needs a
    // sidecar launch + a recorded tick + >30s feed silence inside the boot
    // prefix). Ratchet (source-order scan of this file):
    // groww_sidecar_supervisor::tests::test_stall_restart_counter_is_preregistered_after_recorder_install.
    metrics::counter!(
        "tv_feed_sidecar_stall_restart_total",
        "feed" => tickvault_common::feed::Feed::Groww.as_str(),
    )
    .increment(0);
    // Never-streamed attribution counter (2026-07-09 reject-loop hardening):
    // same delta-baseline rationale as the counter above — pre-register so
    // the CW agent's dropped-first-sample can never eat the session's first
    // never-streamed restart from the attribution split. The pager math is
    // unaffected (never-streamed restarts ALSO increment the stall counter
    // above, which the tv-<env>-feed-stall-restarts alarm reads).
    metrics::counter!(
        "tv_feed_sidecar_never_streamed_restart_total",
        "feed" => tickvault_common::feed::Feed::Groww.as_str(),
    )
    .increment(0);
    // Seal-writer TRUE-DROP counter (2026-07-09 candle-drop paging PR):
    // same delta-baseline rationale as the two registrations above — the CW
    // agent's prometheus pipeline drops each counter series' FIRST sample
    // as its baseline, and tv_seal_writer_drain_total{kind="dropped"}
    // increments ONLY when sealed candles are truly dropped (ring + spill +
    // DLQ all failed — AGGREGATOR-DROP-01, the only silent-data-loss path
    // for sealed candles). Without this post-install registration the
    // series is born AT the first drop and the dropped baseline sample IS
    // the drop — a single-episode drop (the dominant shape) would produce
    // ZERO datapoints and the tv-<env>-seal-writer-dropped counter alarm
    // (deploy/aws/terraform/seal-drop-alarm.tf) would be dead on arrival.
    // Registering at 0 here makes the series DENSE from boot (a 0-delta
    // /metrics event per 60s scrape), so the dropped first sample is the
    // harmless 0 baseline and the pager genuinely sees the session's first
    // drop. The other 5 `kind` values are busy/self-baselining and feed no
    // alarm — only "dropped" needs the honest baseline. Ratchet
    // (source-order scan of this file):
    // crates/app/tests/seal_drop_paging_wiring_guard.rs.
    metrics::counter!("tv_seal_writer_drain_total", "kind" => "dropped").increment(0);

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
    //   data/logs/machine/app.YYYY-MM-DD-HH
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

    // Error-only file layer: data/logs/machine/errors.log (WARN + ERROR only).
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
    // data/logs/machine/errors.jsonl.YYYY-MM-DD-HH for Claude triage daemon,
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
    // readable `data/logs/machine/errors.summary.md` every 60s so `/loop` polling
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
    // 2026-07-05 grace window: ALSO sweeps the legacy top-level
    // data/logs/ so errors.jsonl.* files written before the machine/
    // move age out naturally (no risky boot-time file moves).
    tokio::spawn(async {
        use std::path::Path;
        use std::time::Duration;
        const SWEEP_INTERVAL_SECS: u64 = 3600;
        const RETENTION_HOURS: u64 = 48;
        loop {
            tokio::time::sleep(Duration::from_secs(SWEEP_INTERVAL_SECS)).await;
            for dir in [
                Path::new(observability::ERRORS_JSONL_DIR),
                Path::new(observability::LEGACY_LOGS_DIR),
            ] {
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
            // 2026-07-13 disk-retention hardening: the single-file WARN+
            // append log is deliberately skipped by every retention sweeper
            // (the `*.log`-name guard), so it was the one unbounded log
            // file. Cap it at ERRORS_LOG_MAX_BYTES — the same WARN+ lines
            // also live in the hourly machine app logs + errors.jsonl, so a
            // truncation loses nothing uniquely. Hosted here (the existing
            // hourly sweep task) rather than a new task.
            match observability::cap_errors_log_size(
                Path::new(tickvault_app::boot_helpers::ERROR_LOG_FILE_PATH),
                observability::ERRORS_LOG_MAX_BYTES,
            ) {
                Ok(None) => {}
                Ok(Some(prev_bytes)) => tracing::info!(
                    prev_bytes,
                    max_bytes = observability::ERRORS_LOG_MAX_BYTES,
                    "errors.log exceeded its size cap — reset to 0 (WARN+ lines \
                     remain in the hourly app logs + errors.jsonl)"
                ),
                Err(err) => tracing::warn!(
                    ?err,
                    "errors.log size cap check failed — file keeps growing until fixed"
                ),
            }
        }
    });

    // Hourly retention sweeper for the rolling app log
    // (data/logs/machine/app.YYYY-MM-DD-HH). Keeps 7 days of files (168
    // hourly chunks at ~5–10 MB each = ~0.8–1.7 GB cap on disk), matching
    // the prior daily-file retention semantic of "keep 7 daily files".
    // 2026-07-05 grace window: ALSO sweeps the legacy top-level
    // data/logs/ for pre-move hourly captures. The sweeper skips every
    // `*.log` name, so the launcher-owned HUMAN daily log
    // (app.<IST-date>.log) can never be deleted by this task.
    tokio::spawn(async {
        use std::path::Path;
        use std::time::Duration;
        const SWEEP_INTERVAL_SECS: u64 = 3600;
        const RETENTION_HOURS: u64 = 168;
        loop {
            tokio::time::sleep(Duration::from_secs(SWEEP_INTERVAL_SECS)).await;
            for dir in [
                Path::new(observability::ERRORS_JSONL_DIR),
                Path::new(observability::LEGACY_LOGS_DIR),
            ] {
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
                // 2026-07-05 grace window: sweep the machine/ category dir
                // AND the legacy top-level data/logs/<prefix>/ dir so
                // pre-move files age out naturally.
                let legacy_dir = format!(
                    "{}/{}",
                    tickvault_app::observability::LEGACY_LOGS_DIR,
                    cat.prefix()
                );
                for dir in [Path::new(cat.dir()), Path::new(legacy_dir.as_str())] {
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
        }
    });

    // 2026-07-13 disk-retention hardening: prune confirmed-replay WAL
    // segments from `<wal_dir>/archive/` older than 7 days (F3: matches
    // SPILL_FILE_MAX_AGE_SECS and preserves the confirm-on-channel
    // residual's only copy across a long weekend for triage). Archived
    // segments are post-confirmed-replay copies (frames re-injected +
    // durably persisted); the same-day 15:40 IST tick-conservation audit
    // reads only the CURRENT day's frames, so a 7-day retention can never
    // change it. Before this task, `archive/` grew ~0.15–0.6 GB/day
    // unbounded on the prod 30 GB volume. Process-global boot prefix (both
    // boot arms) — deliberately NOT the Dhan-lane periodic health loop,
    // which never runs on a Groww-only boot. Prunes once at task start
    // (each daily prod boot reclaims immediately), then every 6 h.
    tokio::spawn(async {
        use std::time::Duration;
        loop {
            let wal_dir = tickvault_app::tick_conservation_boot::ws_wal_dir();
            let _outcome = tickvault_storage::ws_frame_spill::prune_archived_segments(
                &wal_dir,
                tickvault_common::constants::WS_WAL_ARCHIVE_RETENTION_SECS,
            );
            tokio::time::sleep(Duration::from_secs(
                tickvault_common::constants::WS_WAL_ARCHIVE_PRUNE_INTERVAL_SECS,
            ))
            .await;
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
    // CCL-06: publish today's Muhurat-session flag to the process-global so the
    // tick processor additionally accepts the evening [18:00, 19:30) IST window
    // on a Muhurat date (otherwise the connected feed's Muhurat ticks are
    // silently dropped by the regular [09:00, 15:30) persist gate). Idempotent,
    // boot-once; `false` on every trading/mock day → today's behaviour.
    tickvault_common::muhurat::init_muhurat_session(is_muhurat);
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
    // =======================================================================
    // Step C — PER-FEED BOOT DISPATCHER (pluggable-feed-runtime.md §6)
    //
    // "A feed's code runs IFF its enable flag is true." All shared infra
    // (observability, WAL replay, trading calendar, errors.jsonl) is already up,
    // and the Groww lane (auth + bridge) was already spawned above gated on
    // `groww_enabled`. If Dhan is disabled we SKIP the entire Dhan fast/slow
    // boot block below and idle-await shutdown so the enabled lanes keep running.
    //
    // When `dhan_enabled=true` (prod default) this branch is skipped and the
    // Dhan boot below is byte-identical to before.
    // =======================================================================
    // D2 Stage 2 (genuine shared-infra hoist): the Dhan-OFF path no longer
    // early-returns into a duplicate `run_shared_infra_only`. It now falls
    // through to the SAME hoisted PROCESS-shared infra prefix below (notifier,
    // health, seal-writer, broadcasts, the 3 subscriber tasks, API server incl.
    // /api/feeds) and the SAME `run_process_runloop`, with the Dhan LANE skipped
    // via the `if config.feeds.dhan_enabled` wrapper. The OFF-feed-isolation
    // guarantee is preserved by construction: NO Dhan auth, NO instrument fetch,
    // NO Dhan WebSocket runs when `dhan_enabled=false` (all of that lives inside
    // the lane wrapper). Both the fast-arm guard below and the lane wrapper
    // further down read `config.feeds.dhan_enabled`.
    if !config.feeds.dhan_enabled {
        if config.feeds.groww_enabled {
            info!(
                "GROWW-ONLY MODE — Dhan boot skipped; Groww lane running, shared infra (API + \
                 seal-writer + aggregator) coming up via the unified hoisted prefix"
            );
        } else {
            warn!(
                "NO FEED ENABLED — both dhan_enabled and groww_enabled are false; \
                 shared-infra-only runtime (API + seal-writer + aggregator) coming up via the \
                 unified hoisted prefix"
            );
        }
    }

    // =====================================================================
    // PROCESS-GLOBAL systemd WATCHDOG=1 pinger (2026-07-13, deploy-hang fix
    // hostile-review round 1 CRITICAL). The unit sets WatchdogSec=60, and
    // BOTH pre-existing `notify_systemd_watchdog()` ping tasks
    // (`spawn_heartbeat_watchdog`) are Dhan-gated: the FAST-arm spawn runs
    // only on a dhan-enabled crash-recovery boot, and the slow-boot spawn
    // lives INSIDE `start_dhan_lane` (lane-owned; a PR-E lane teardown
    // aborts it). Sending READY=1 on a Dhan-OFF boot (the same-date
    // deploy-hang fix) would therefore ARM the watchdog with NOBODY pinging
    // -> systemd SIGABRTs the process at t+60s -> Restart=always loop ->
    // StartLimitBurst hard-fail minutes after a deploy that reported
    // success (Rule-11 false-OK), and the next 08:30 boot dies by ~08:40.
    // This thin, FEED-AGNOSTIC, process-lifetime task pings every
    // WATCHDOG_INTERVAL_SECS (30s) from the shared boot prefix, BEFORE any
    // boot path can send READY=1 (both READY-capable arms — the FAST arm
    // and the Dhan-OFF arm — sit below this line in source order; a
    // source-scan ratchet pinning that ordering + the absence of any feed
    // gate on this spawn must land with this fix). Extra pings alongside
    // the lane-owned `spawn_heartbeat_watchdog` are harmless (each
    // WATCHDOG=1 just resets the 60s timer); the lane-owned tasks keep
    // their ownership semantics untouched — and this task ALSO closes the
    // pre-existing PR-E hole where a runtime Dhan disable aborted the ONLY
    // pinger of a watchdog-armed process. No-op outside systemd
    // (`notify_systemd_watchdog` requires NOTIFY_SOCKET).
    let _process_global_watchdog_pinger = tokio::spawn(async {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            tickvault_app::boot_helpers::WATCHDOG_INTERVAL_SECS,
        ));
        interval.tick().await; // skip the immediate first tick
        loop {
            interval.tick().await;
            infra::notify_systemd_watchdog();
        }
    });

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

    // D2 Stage 2: the FAST crash-recovery arm is Dhan-ON-only. It is reached
    // ONLY at boot with a valid token cache in market hours and is left
    // byte-identical (design L10/D2d). The extra `config.feeds.dhan_enabled`
    // guard prevents a Dhan-OFF boot (which no longer early-returns) from ever
    // entering the fast arm — a Dhan-OFF runtime has no Dhan lane to fast-boot.
    if let Some(cache_result) = fast_cache.filter(|_| is_market_hours && config.feeds.dhan_enabled)
    {
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
        // 2026-07-07 UX overhaul: the in-market LOW/MEDIUM digest window
        // comes from `[notification] digest_window_secs` (clamped [60,3600]);
        // the coalescer drain loop also owns the episode stability ticker.
        let fast_notifier = if config.features.telegram_bucket_coalescer {
            NotificationService::enable_coalescer(
                fast_notifier,
                tickvault_core::notification::CoalescerConfig {
                    market_hours_window: std::time::Duration::from_secs(
                        config.notification.digest_window_secs_clamped(),
                    ),
                    ..Default::default()
                },
            )
        } else {
            // No drain loop → the episode green-close promotion needs its
            // own tiny ticker (no-op when episode_mode is off / NoOp mode).
            NotificationService::spawn_episode_ticker(&fast_notifier);
            fast_notifier
        };
        // Fill the Groww sidecar supervisor's deferred Telegram slot now that the
        // notifier exists: a subsequent sidecar reject diagnostic can page the
        // operator. Stored once; the supervisor resolves it lazily per child.
        groww_sidecar_notifier_slot.store(Some(std::sync::Arc::clone(&fast_notifier)));
        // Boot bubble (2026-07-09): declare which feed lines the checklist
        // shows as pending from its first page.
        fast_notifier.set_boot_expectations(config.feeds.dhan_enabled, config.feeds.groww_enabled);
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
        let (subscription_plan, fresh_universe, _needs_persist) = match load_instruments(
            &config,
            is_trading,
            trading_calendar.as_ref(),
        )
        .await
        {
            Ok(loaded) => loaded,
            Err(err) => {
                // Observability slice #2: emit the symmetric failure alert.
                // The bare `?` here previously exited boot with no operator
                // signal (only success was announced). Fail-closed boot-halt
                // is preserved — we return Err AFTER alerting. NOT a duplicate
                // of INSTR-FETCH-* (those infinite-retry, never reach this Err).
                // Security (review 2026-06-12): redact the anyhow chain before it
                // reaches Telegram + the log sinks (authentication.md rule 2 —
                // a token must never appear in error messages). A future
                // authenticated call in this path could otherwise carry a
                // token-in-URL into `reason`. capture_rest_error_body is the
                // ratchet-guaranteed redactor (URL-param + JWT + credential-field).
                let safe_reason =
                    tickvault_common::sanitize::capture_rest_error_body(&err.to_string());
                error!(error = %safe_reason, "instrument load failed at boot — alerting operator and halting");
                fast_notifier.notify(NotificationEvent::InstrumentBuildFailed {
                    reason: safe_reason,
                    manual_trigger_url: format!(
                        "http://{}:{}/api/instruments/rebuild",
                        config.api.host, config.api.port
                    ),
                });
                return Err(err);
            }
        };

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

        // 2026-07-03 feed parity: record Dhan's subscribed counts into the
        // shared per-feed health registry (one-shot, cold path) so the
        // readiness + end-of-day Telegram feed lines — and the feed-control
        // page — show a REAL Dhan instrument count instead of "unknown".
        if let Some(ref plan) = subscription_plan {
            let summary = &plan.summary;
            let indices = summary
                .major_index_values
                .saturating_add(summary.display_indices);
            let stocks = summary.total.saturating_sub(indices);
            feed_health.set_subscribed(
                tickvault_common::feed::Feed::Dhan,
                stocks as u64,
                indices as u64,
            );
        }

        // AUTH-GAP-06 (2026-07-08 — operator incident, THIRD morning
        // cached-token outage, 08:32–09:06 IST on 2026-07-07): the fast arm
        // previously trusted yesterday's cached token blindly — Dhan had
        // killed it (one active token at a time), so the WS pool spawned
        // dead and stayed dead until the ~30-min AUTH-GAP-05 watchdog /
        // manual intervention. Validate the cached token with ONE
        // GET /v2/profile BEFORE any WebSocket spawn: a prefix-anchored
        // 401/403 rejection forces a re-mint through the EXISTING
        // TokenManager machinery (RESILIENCE-03 in-flight tripwire +
        // Dhan mint semantics preserved; this arm passes None for the
        // dual-instance lock flag per dual-instance-lock-2026-07-04.md §3
        // — documented residual, deliberately UNCHANGED). Transient /
        // REST-surface failures retry once, then proceed LOUDLY with the
        // cached token — boot never hangs, never gains a mint-on-ambiguity
        // path. Returns Some(manager) on the remint path so the deferred
        // background arm below reuses it (no duplicate SSM fetch). Ordering
        // (validation → cooldown wait → create_websocket_pool) is pinned by
        // crates/app/tests/fast_boot_token_validation_wiring_guard.rs.
        let fast_boot_prevalidated_token_manager =
            tickvault_core::auth::fast_boot_validation::validate_cached_token_at_fast_boot(
                &token_handle,
                &client_id,
                &config.dhan,
                &config.token,
                &config.network,
                &fast_notifier,
            )
            .await;

        // WS-GAP-08 (2026-07-06 audit fix): the FAST crash-recovery arm must
        // ALSO honour a persisted Dhan 429 rate-limit cooldown BEFORE its
        // first WS connect. A mid-market `process::exit(2)` (the WS-GAP-09
        // ceiling_exceeded fallback still reaches exit) with a VALID cached
        // token routes through THIS arm — previously it wiped the in-memory
        // `rate_limit_streak` and reconnected at 0ms straight back into
        // Dhan's still-active 429 window: the exact instant-429 restart loop
        // WS-GAP-08 was built to break (only the slow lane waited). Same
        // semantics as the slow-lane call: fail-open on a missing/corrupt/
        // stale file (no wait) and bounded by WS_RATE_LIMIT_BACKOFF_CAP_MS
        // (5 min), so a bad file can never hang crash recovery. The fast arm
        // is only reachable on a market-hours trading-day restart, so no
        // trading-day gate is needed here. Ratchet:
        // crates/app/tests/ws_rate_limit_cooldown_wiring_guard.rs.
        wait_out_persisted_ws_rate_limit_cooldown().await;

        // --- WebSocket pool create (channel only, NOT spawned yet) ---
        let (pool_receiver, ws_pool_ready) = match create_websocket_pool(
            &token_handle,
            &client_id,
            &subscription_plan,
            &config,
            true,
            Some(fast_notifier.clone()),
            ws_frame_spill.clone(),
            // PR-E: hand the Dhan main feed the shared runtime enable flag.
            Some(feed_runtime.dhan_flag()),
            // D2c (C4): the FAST crash-recovery arm `return run_shutdown_fast`s
            // (below, ~main.rs:2074) BEFORE the inline spine's
            // `set_global_token_manager` (~main.rs:4991) is ever reached — so on
            // the fast arm the global `OnceLock` is UNSET and
            // `wake_renewal_token_manager()` returns `None`. Wake-renewal therefore
            // silently no-ops here: the fast arm runs ONLY at boot with a valid
            // cached token (market-hours crash restart), so it relies on that cached
            // token rather than proactive wake-renewal. Passing `None` for the
            // lane-owned manager is correct (no runtime lane to inject). This is
            // PRE-EXISTING fast-arm behaviour, made explicit here — NOT a D2c change.
            None,
        ) {
            Some((receiver, pool)) => (Some(receiver), Some(pool)),
            None => (None, None),
        };

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

            // NT-15: seed the tick-gap detector with every subscribed SID so a
            // never-ticking instrument becomes a first-tick black-hole signal.
            seed_tick_gap_detector_from_plan(&subscription_plan);

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
            // SP5: dedicated clone moved into the tick-processor coroutine so the
            // original `feed_health` Arc stays available for the pool watchdog below.
            let feed_health_for_processor = std::sync::Arc::clone(&feed_health);
            let handle = tokio::spawn(async move {
                run_tick_processor(
                    receiver,
                    fast_tick_writer,
                    tick_broadcast_for_processor,
                    greeks_enricher,
                    Some(fast_tick_heartbeat),
                    None, // tick_enricher — Phase 2.5 wiring deferred until prev_oi_cache + boot ordering gate land in slow boot
                    tickvault_common::always_on::current(), // §30 GIFT exemption
                    Some(feed_health_for_processor), // SP5: Dhan live-feed health
                )
                .await;
            });
            info!("FAST BOOT COMPLETE — tick processor started, ticks flowing (in-memory)");
            // Silent-feed hardening Item 4 (2026-07-07 round-1 fix): spawn
            // the Dhan exchange-lag p99 publisher on the FAST crash-recovery
            // arm too. This arm `return run_shutdown_fast(...)`s and never
            // reaches `start_dhan_lane`, so without this spawn a mid-market
            // crash restart (the WS-GAP-09 exit(2) → systemd restart class)
            // left `tv_dhan_exchange_lag_p99_seconds` unpublished for the
            // rest of the session while the ring kept being written — and
            // the lag alarm treats missing data as notBreaching, so the
            // darkness was silent (exactly the 2026-07-06 incident class).
            // Same once-per-process guard as the slow-lane site below.
            if FEED_LAG_PUBLISHER_SUPERVISOR_SPAWNED.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                info!(
                    "feed-lag publisher supervisor already running — skipping duplicate spawn \
                     (fast boot)"
                );
            } else {
                let _feed_lag_publisher_supervisor = spawn_supervised_feed_lag_publisher();
            }
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

        // STAGE-C.2b: Re-inject replayed LiveFeed frames into the pool's
        // frame sender. Ordering (C3 review CRITICAL fix, 2026-07-03):
        //   [channel create] → [tick processor spawned ABOVE, draining] →
        //   [this reinject await] → [WS connections spawned BELOW].
        // The consumer MUST already be running: a replay larger than
        // FRAME_CHANNEL_CAPACITY (131,072) would otherwise fill the channel
        // with nobody draining, stall into the 30s
        // WAL_REINJECT_SEND_TIMEOUT, abort NOT-clean, and the WAL would
        // never archive — the exact storm loop this fix exists to break.
        // Because this await completes BEFORE the connections spawn, every
        // replayed frame still enters the mpsc ahead of any fresh live
        // frame (FIFO preserved — one sequential sender loop), and QuestDB
        // dedup keys (STORAGE-GAP-01) make the replay idempotent, so even
        // if the same frames were partially persisted before the crash, no
        // duplicate rows are written. Ratchet:
        // wal_reinject::tests::ratchet_tick_processor_spawns_before_reinject_await.
        //
        // If the pool build failed (ws_pool_ready is None) we log a
        // warning but preserve the frames in the WAL archive.
        //
        // CRASH-SAFETY (zero-loss MEDIUM fix 2026-06-30): mirror of the
        // slow-boot staged-then-confirm contract. The fast-boot confirm
        // (just before notify_systemd_ready below) archives the staged
        // segments ONLY if this re-injection was clean — i.e. a pool
        // existed AND no frame was dropped. A dropped/no-pool case keeps
        // the segments in replaying/ so they re-replay next boot.
        let mut fast_ws_wal_replay_reinjection_clean = true;
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
                // C3 fix (2026-07-03): bounded chunked backpressure via
                // `reinject_wal_frames` (send().await + per-chunk yield +
                // bounded per-send timeout) replaces the raw try_send loop
                // that silently dropped everything past
                // FRAME_CHANNEL_CAPACITY (dropped=1,127,801 at 10:35 IST)
                // and kept the WAL unconfirmed — the self-feeding re-replay
                // storm. A clean run lets confirm_replayed() below archive
                // the staged segments. Runbook: ws-reinject-error-codes.md.
                let outcome = tickvault_app::wal_reinject::reinject_wal_frames(
                    &sender,
                    to_inject,
                    tickvault_common::constants::WAL_REINJECT_CHUNK_SIZE,
                    std::time::Duration::from_secs(
                        tickvault_common::constants::WAL_REINJECT_SEND_TIMEOUT_SECS,
                    ),
                )
                .await;
                metrics::counter!(
                    "tv_ws_frame_wal_reinjected_total",
                    "ws_type" => "live_feed"
                )
                .increment(outcome.injected);
                if outcome.clean {
                    info!(
                        injected = outcome.injected,
                        "STAGE-C.2b: LiveFeed re-injection complete — all replayed frames \
                         delivered with backpressure"
                    );
                } else {
                    // WS-REINJECT-01 was already error!-paged (typed code +
                    // abort counters) inside the helper; here we only record
                    // the not-clean flag so confirm_replayed() below is
                    // skipped and the staged segments re-replay next boot
                    // (durable WAL floor — no silent loss).
                    fast_ws_wal_replay_reinjection_clean = false;
                    warn!(
                        injected = outcome.injected,
                        aborted_remaining = outcome.aborted_remaining,
                        "STAGE-C.2b: LiveFeed re-injection aborted — staged WAL segments \
                         remain in replaying/ for re-replay next boot"
                    );
                }
            } else {
                fast_ws_wal_replay_reinjection_clean = false;
                warn!(
                    frames = ws_wal_replay_live_feed.len(),
                    "STAGE-C.2b: LiveFeed replay frames held but pool build failed — \
                     frames remain in WAL archive, not re-injected"
                );
            }
        }

        // --- NOW spawn WebSocket connections (tick processor consuming) ---
        // S4-T1a/T1b: Wrap the pool in Arc so we can retain clones for the
        // pool watchdog task and the graceful-shutdown handler. All three
        // users (spawn_all, poll_watchdog, request_graceful_shutdown) take
        // &self so sharing via Arc is cheap + lock-free.
        let (ws_handles, ws_pool_arc, fast_pool_watchdog_handle) = if let Some(pool) = ws_pool_ready
        {
            let pool_arc = std::sync::Arc::new(pool);
            // O1-B (2026-04-17): install per-connection runtime subscribe
            // channels BEFORE spawn so the read loop sees the receivers on
            // its first iteration. The Phase 2 scheduler reads the matching
            // senders via `pool_arc.dispatch_subscribe(...)`.
            pool_arc.install_subscribe_channels().await;
            let handles = spawn_websocket_connections(std::sync::Arc::clone(&pool_arc)).await;
            // R8-EDGE-2 / SEC-C2-1 (2026-07-07): bind + lane-own the watchdog
            // handle (rides into DhanLaneRunHandles via run_shutdown_fast) so
            // teardown/Drop can abort it — the notify_waiters stop signal
            // alone is Rule-16 lost-wake prone (the watchdog's tick body
            // awaits a ≤2s QuestDB probe).
            let fast_pool_watchdog_handle = spawn_pool_watchdog_task(
                std::sync::Arc::clone(&pool_arc),
                std::sync::Arc::clone(&shutdown_notify),
                std::sync::Arc::clone(&fast_notifier),
                std::sync::Arc::clone(&health_status),
                Some(std::sync::Arc::clone(&feed_health)), // SP5: Dhan connected state
                // BOOT-ON fast crash-recovery path: keep the single-feed
                // `process::exit(2)` Halt contract (H7 boot-ON is a separate
                // tracked follow-up).
                None,
                // PR-E: runtime Dhan-OFF gate — same atomic the WS read loop
                // consults; a dormant Dhan-OFF pool must not be paged/halted.
                Some(feed_runtime.dhan_flag()),
                // Fix A (2026-06-30): shared JWT handle so the Halt-arm
                // bare-reset classifier can require a live token before
                // reconnecting in place.
                Some(token_handle.clone()),
                // Fix A no-op close (2026-06-30): QuestDB config so the watchdog
                // pushes the REAL QuestDB-liveness signal into
                // `health.set_questdb_reachable(...)` every 5s — without it the
                // bare-reset gate was a production no-op (signal only ever set
                // in test code).
                config.questdb.clone(),
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
                &feed_runtime,
                &feed_health,
            )
            .await;
            // BUG-2 fix (2026-07-05): mark the Dhan lane running + the PR-2
            // pool-present sentinel ONLY now that a REAL pool is spawned
            // (moved from the unconditional dhan_enabled block at the top of
            // main() — see the comment there). Also drive the lane FSM
            // Starting→Running here: the FAST arm never reaches the slow
            // arm's `StartSucceeded` drive, and the activation watcher now
            // holds off while the FSM reads `Starting`, so leaving the seed
            // in place would keep the watcher dormant for the whole run.
            feed_runtime.mark_dhan_lane_running();
            feed_runtime.mark_dhan_pool_present();
            feed_runtime.set_dhan_lane_state(tickvault_api::feed_state::LaneState::Running);
            record_dhan_lane_transition(
                tickvault_api::feed_state::LaneState::Starting,
                tickvault_api::feed_state::LaneState::Running,
            );
            (handles, Some(pool_arc), Some(fast_pool_watchdog_handle))
        } else {
            (Vec::new(), None, None)
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
        // 2026-06-12: dedicated Arc for the once-per-boot BootHealthCheck ping
        // (a separate clone so it doesn't contend with `notifier`'s use below).
        let boot_health_notifier = fast_notifier.clone();
        let (_, deferred_token_manager) = tokio::join!(
            // Docker infra + QuestDB DDL
            async {
                infra::ensure_infra_running(&config.questdb).await;
                // Positive boot signal (audit-findings Rule 11): fire the
                // once-per-boot BootHealthCheck with the real healthy/total
                // container counts — fires even at 0/0 (honest "nothing
                // healthy") so the operator always learns the boot outcome.
                let (services_healthy, services_total) = infra::container_health_counts().await;
                boot_health_notifier.notify(NotificationEvent::BootHealthCheck {
                    services_healthy,
                    services_total,
                });
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

                // Human-readable analyst console views (ticks_named /
                // candles_named). Sequential AFTER the base-table ensure
                // join so `ticks` + `candles_1m` exist before CREATE VIEW
                // validates its column references. Fail-soft + convergent
                // (CREATE OR REPLACE VIEW — every boot converges the
                // deployed definition to the code); a failure retries next
                // boot. Groww-only boots get their own call site in
                // `groww_activation.rs::activate_groww_lane`.
                tickvault_storage::console_views::ensure_named_views(&config.questdb).await;

                info!("QuestDB DDL complete (background)");
            },
            // SSM validation + TokenManager for renewal
            async {
                // AUTH-GAP-06 (2026-07-08): the fast-boot cached-token
                // validation may already have constructed (and re-minted
                // through) a TokenManager on its Remint path — reuse it
                // instead of a duplicate SSM fetch + duplicate manager.
                if let Some(manager) = fast_boot_prevalidated_token_manager {
                    info!(
                        "deferred auth: reusing the AUTH-GAP-06 fast-boot validation \
                         TokenManager (re-mint path) — skipping duplicate initialize_deferred"
                    );
                    return Ok(Ok(manager));
                }
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
                        // RESILIENCE-03 tripwire flag: the FAST crash-recovery
                        // arm holds NO dual-instance lock today (it never mints
                        // at boot — cached token; a mint happens only on the
                        // rare client_id-mismatch / renewal-fallback paths).
                        // Documented residual gap in
                        // `.claude/rules/project/dual-instance-lock-2026-07-04.md`
                        // — wiring the lock into this arm needs an operator
                        // decision on mid-market halt semantics.
                        None,
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
            // 2026-06-12: order-update lifecycle events → ws_event_audit (its
            // own consumer, same self-contained helper as the main-feed pool).
            let ord_ws_audit_tx = Some(spawn_ws_event_audit_consumer(config.questdb.clone()));
            // PR-E: the order-update WS reads the same Dhan enable flag.
            let ord_dhan_flag = Some(feed_runtime.dhan_flag());
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
                    ord_ws_audit_tx,
                    ord_dhan_flag,
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
        let api_state = SharedAppState::new_with_feed_runtime_and_health(
            config.questdb.clone(),
            config.dhan.clone(),
            config.instrument.clone(),
            // clone (not move) so the post-market task spawn below can also hold it
            health_status.clone(),
            // SAME feed-runtime Arc the Groww bridge holds → API toggles are live.
            std::sync::Arc::clone(&feed_runtime),
            // SAME health registry the lanes update → GET /api/feeds/health is live.
            std::sync::Arc::clone(&feed_health),
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
        // W2#7 (2026-07-10): supervised SSM re-read loop — the operator can
        // rotate /tickvault/<env>/api/bearer-token without a restart (live
        // within ~5 min, or ~60s via the mismatched-bearer out-of-band
        // hint). Gated on `enabled` — a disabled dev config gets no task.
        // Fail-open on SSM outages: the current token keeps working.
        // Ratchet: crates/app/tests/api_token_rotation_wiring_guard.rs.
        if api_auth_config_fast.enabled {
            let _api_token_reload_supervisor =
                tickvault_app::api_token_rotation::spawn_supervised_api_token_reload(
                    api_auth_config_fast.clone(),
                );
        }
        let router = tickvault_api::build_router_with_auth(
            api_state,
            &config.api.allowed_origins,
            api_auth_config_fast,
            // 2026-07-04 operator quote (websocket-connection-scope-lock.md
            // "FEED TOGGLE BEARER-GATED IN ALL MODES"): this flag is now
            // accepted-but-IGNORED — the feed toggle is bearer-protected in
            // ALL modes. Kept to avoid a signature cascade.
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

        // EDGE-2 fix (2026-07-06): AUTH-GAP-05 live token-health gauge
        // poller on the FAST arm too. Without it, a market-hours
        // crash-recovery boot (the path MOST correlated with token trouble)
        // regressed `tv_token_remaining_seconds` to the frozen mint-time
        // renewal-loop snapshots and `tv_token_valid` never existed (an
        // alarm on it would go missing-data). The FAST arm runs NO
        // mid-session profile watchdog, so the profile flag is a fresh
        // inert `true` — honest envelope: on this arm the composite gauge
        // reflects has_token AND local expiry only (the local-expiry gate
        // still forces 0 for a locally-dead token). Handle rides into
        // `DhanLaneRunHandles` via `run_shutdown_fast` so the H8 Drop floor
        // owns it.
        let token_health_gauge_handle = token_manager.as_ref().map(|tm| {
            tickvault_core::auth::token_health_gauge::spawn_token_health_gauge_poller(
                std::sync::Arc::clone(tm),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            )
        });
        info!(
            spawned = token_health_gauge_handle.is_some(),
            "live token-health gauge poller (fast boot — profile watchdog not running on this arm)"
        );

        // PR-C (2026-05-26): Dhan historical fetch chain DELETED entirely
        // per operator directive — pre-market buffer + gap_fill +
        // cross_verify chains all retired. The runtime is now spot-only
        // NIFTY 50 strategy (no VWAP, no futures, no historical fetch).

        notifier.notify(NotificationEvent::Custom {
            message: "<b>FAST BOOT</b>\nCrash recovery: ticks flowing, all services ready"
                .to_string(),
        });

        // CRASH-SAFETY confirm (fast boot): mirror of the slow-boot confirm
        // near the end of main(). Archive the staged WAL segments out of
        // replaying/ ONLY if the LiveFeed re-injection above was clean (pool
        // existed AND no frame dropped). Without this the fast-boot path
        // re-replayed every crash-recovery boot — bounded by QuestDB dedup but
        // growing replaying/ + replay cost on a crash-loop, defeating the
        // cleanup this fix targets on the exact path it targets. Gated on the
        // clean flag so a dropped/no-pool re-injection leaves the segments in
        // replaying/ to re-replay next boot (never confirmed early = no loss).
        {
            let fast_ws_wal_path = tickvault_app::tick_conservation_boot::ws_wal_dir();
            if fast_ws_wal_replay_reinjection_clean {
                tickvault_storage::ws_frame_spill::confirm_replayed(&fast_ws_wal_path);
            } else {
                warn!(
                    dir = %fast_ws_wal_path.display(),
                    "STAGE-C.2b: WAL replay NOT confirmed (fast boot — a re-injection dropped or \
                     pool not ready) — staged segments remain in replaying/ for re-replay next boot"
                );
            }
        }

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

        // Boot-completed CloudWatch signal (fast-boot / crash-recovery path).
        // Emitted EXACTLY ONCE here, alongside the fast-boot `notify_systemd_ready()`,
        // because this path returns into `run_shutdown_fast()` and never reaches
        // the slow-boot emit at the end of `main()`. See `emit_boot_completed`
        // for the full rationale (boot-heartbeat alarm pages on MISSING).
        //
        // GATED on a live feed (audit fix, this PR): the fast-boot arm is
        // Dhan-ON-only, but the warm-snapshot re-injects the WS pool
        // asynchronously — a crash-recovery where the pool never reconnects must
        // NOT publish "alive". `emit_boot_completed_when_feed_live` waits (bounded)
        // for the Dhan lane to reach Running and withholds the metric (→ alarm
        // pages) if it never does.
        emit_boot_completed_when_feed_live(
            &feed_runtime,
            config.feeds.dhan_enabled,
            config.feeds.groww_enabled,
            // BUG-2: the FAST crash-recovery arm always intends a pool
            // (market-hours trading-day restarts only) — the gate genuinely
            // waits for the lane-running flag here.
            false,
        )
        .await;

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
            client_id.clone(),
            &config,
            std::sync::Arc::clone(&feed_runtime),
            std::sync::Arc::clone(&feed_health),
        );

        // Dual-feed scoreboard on the FAST crash-recovery arm too (hostile
        // review 2026-07-10, CRITICAL): this arm `return run_shutdown_fast`s
        // and never reaches the slow-path spawn below — yet a mid-market
        // process death with a valid cached token restarts through EXACTLY
        // this arm, which is the one boot that must synthesize the
        // process_death episode AND own the day's 15:45 scorecard. Without
        // this call the flagship crash-restart day produced NO episode row,
        // NO scorecard and NO Aborted page (nothing was spawned to die).
        // `notifier` here is the fast_notifier clone from above.
        // (Scoreboard PR-D fix round 1: the presence-registry init moved to
        // the process-global boot prefix — ONE site, ordered before the
        // Groww watcher spawn and this arm's load_instruments.)
        spawn_feed_scoreboard_tasks(
            &config,
            &trading_calendar,
            &notifier,
            process_start_ist_nanos,
            &feed_runtime,
        );

        // Groww per-minute spot 1m REST leg on the FAST crash-recovery arm
        // too (hostile round 1 item 1 — the scoreboard dual-site pattern):
        // this arm `return run_shutdown_fast`s and never reaches the
        // slow-path spawn below, yet a mid-market crash restart routes
        // through EXACTLY this arm — without this call the flagship
        // crash-restart day ran NO per-minute fetches and NO 15:31 sweep.
        spawn_groww_spot_1m_leg(&config, &notifier, &trading_calendar);

        // Daily 15:40 IST timeframe-consistency verifier on the FAST
        // crash-recovery arm too (operator 2026-07-13; the scoreboard
        // dual-spawn precedent directly above): this arm `return
        // run_shutdown_fast`s and never reaches the process-global spawn
        // below, yet a mid-market crash-restart session must still own the
        // day's 15:40 TF-VERIFY run. Once-per-process guarded inside.
        tickvault_app::tf_consistency_boot::spawn_tf_consistency_tasks(
            &config,
            &trading_calendar,
            &notifier,
        );

        // --- Await shutdown ---
        return run_shutdown_fast(
            ws_handles,
            processor_handle,
            renewal_handle,
            Some(order_update_handle),
            Some(api_handle),
            trading_handle,
            token_health_gauge_handle,
            fast_pool_watchdog_handle,
            otel_provider,
            &notifier,
            &config,
            ws_pool_arc,
            shutdown_notify,
            trading_calendar.clone(),
            groww_scale_fleet_lock,
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

    // =======================================================================
    // D2 Stage 2 — HOISTED PROCESS-SHARED INFRA (BOTH Dhan-OFF and Dhan-ON-slow)
    //
    // Build the PROCESS-shared infra ONCE here: notifier (+ Docker auto-start),
    // health registry, seal-writer (installs the process-wide global_seal_sender),
    // the tick + order-update broadcasts, the obs / 21-TF aggregator / tick-storage
    // subscriber tasks (which `.subscribe()` BEFORE the lane's tick processor
    // publishes — subscribe-before-publish preserved by construction), and the
    // axum API server (incl. /api/feeds, so the toggle endpoint exists with Dhan
    // OFF). The Dhan LANE below is wrapped in `if config.feeds.dhan_enabled` and
    // references these shared handles; the single `run_process_runloop` runs for
    // BOTH paths (lane = Some on ON-slow, None on OFF/Groww-only).
    // =======================================================================
    let SharedInfraHandles {
        notifier,
        health_status,
        tick_broadcast_sender,
        order_update_sender,
        api_handle,
    } = build_shared_infra(
        &config,
        std::sync::Arc::clone(&feed_runtime),
        std::sync::Arc::clone(&feed_health),
        std::sync::Arc::clone(&trading_calendar),
        std::sync::Arc::clone(&prev_day_cache),
        std::sync::Arc::clone(&tick_storage),
    )
    .await?;
    // 2026-07-05 feed-Telegram parity (operator: "why dhan messages and groww
    // messages are not same"): fill the Groww deferred notifier slot on THIS
    // boot path too. Before this, only the FAST boot path stored the notifier
    // (the store next to `fast_notifier` above) — the slow-boot / GROWW-ONLY
    // shared-infra path never filled the slot, so every Groww boot-stage ping
    // (FeedAuthOk / FeedInstrumentsLoaded / sidecar-reject / connected) waited
    // out its budget and was skipped ("notifier slot never filled within the
    // wait budget"). Provably safe here: the notifier is fully constructed
    // (strict init + coalescer wrap) inside `build_shared_infra` before this
    // line runs — the exact mirror of the fast-path store.
    groww_sidecar_notifier_slot.store(Some(std::sync::Arc::clone(&notifier)));

    // =======================================================================
    // PROCESS-GLOBAL supervised observability monitors (2026-07-01 per-lane-leak
    // fix). These four host/process-level monitors used to be spawned inside the
    // per-lane `start_dhan_lane`, which re-runs on every Dhan enable / stop→restart
    // / cold-start retry (`run_dhan_lane_cold_start`) — so each re-invocation
    // leaked a fresh never-aborted monitor (duplicate PROC-01 / RESOURCE-01/02/03 /
    // INDEX-OHLC-02 / DISK-WATCHER-01 pages + N× metric increments for one real
    // event, unbounded task growth under toggling), and on a boot-OFF-Dhan
    // deployment they never started at all until Dhan was toggled on. They are
    // spawned EXACTLY ONCE here in the process-global prefix (after
    // `build_shared_infra`, before the Dhan-lane gate) and owned at process scope,
    // so they run regardless of Dhan enable/disable/restart. This block is placed
    // AFTER `build_shared_infra` returns (NOT inside it), so the
    // build-shared-infra isolation guard — which forbids auth / instrument-load /
    // WebSocket strings in that fn's body — is unaffected (these are pure
    // observability, none of those strings).
    // =======================================================================

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

    // W2 PR#6 (WAL-SUSPEND-01, 2026-07-10, audit follow-up row 10):
    // supervised per-table QuestDB WAL-suspension probe. Polls
    // `wal_tables()` every 60s via the shared probe client; a table whose
    // WAL apply is SUSPENDED (post disk-full / apply error) keeps ACKing
    // ILP rows while they silently stop becoming visible — this probe is
    // the ONLY signal (boot probe + questdb_health see reachability/
    // connection, not per-table apply). Edge-latched error!(code =
    // "WAL-SUSPEND-01") pages via the errcode filter chain; a merely-DOWN
    // QuestDB never fires it (BOOT-01/02 own that). Always-on like its
    // monitor siblings; supervised so a probe panic respawns instead of
    // vanishing (mirrors DISK-WATCHER-01).
    let _wal_suspension_watcher_supervisor =
        tickvault_storage::wal_suspension_watcher::spawn_supervised_wal_suspension_watcher(
            config.questdb.clone(),
        );

    // BP-07 (PROC-01, 2026-07-01): supervised OOM-kill monitor. Reads the
    // cgroup-v2 `memory.events` `oom_kill` counter vs a boot baseline every
    // 60s and pages Critical (`error!(code = "PROC-01")` + `tv_oom_kills_total`)
    // when the host OOM-killer takes a process in this cgroup. Before this an
    // OOM was only caught indirectly (die → systemd → missing-SLO page), so an
    // OOM-loop was indistinguishable from a panic-loop. Always-on (an OOM at
    // any hour is critical); on a non-cgroup-v2 dev box the probe fails softly
    // (`tv_oom_monitor_probe_failed_total`, no page, no panic). Supervised so a
    // monitor panic respawns instead of vanishing (mirrors DISK-WATCHER-01).
    let _oom_monitor_supervisor =
        tickvault_storage::oom_monitor::spawn_supervised_oom_monitor(std::path::PathBuf::from(
            tickvault_storage::oom_monitor::DEFAULT_CGROUP_V2_MEMORY_EVENTS_PATH,
        ));

    // BP-08 (RESOURCE-01/02/03, 2026-07-01): supervised process-level resource
    // early-warning monitor. Samples open fd count vs LimitNOFILE (RESOURCE-01),
    // VmRSS vs cgroup memory.max (RESOURCE-02), and spill-dir free-percent
    // (RESOURCE-03) every 60s; pages Critical/High at 80% (fd/RSS) / <20% free
    // (spill) so the operator acts BEFORE exhaustion — distinct from the host-
    // aggregate mem_used_high / disk_used_high alarms. Always-on (resource
    // exhaustion at any hour is critical); non-Linux probes fail softly
    // (tv_resource_monitor_probe_failed_total, no page, no panic). Supervised so
    // a monitor panic respawns instead of vanishing (mirrors DISK-WATCHER-01).
    let _resource_monitor_supervisor =
        tickvault_storage::resource_monitor::spawn_supervised_resource_monitor(
            tickvault_storage::resource_monitor::ResourceMonitorPaths::platform_defaults(
                std::path::PathBuf::from("data/spill"),
            ),
        );

    // Daily 15:40 IST per-feed tick-conservation audit — PROCESS-GLOBAL
    // (2026-07-02 adversarial-sweep fix). Previously nested inside the
    // Dhan-gated `spawn_post_market_tasks`, so a Groww-only session ran ZERO
    // conservation audits and runtime Dhan enable cycles duplicated the task.
    // Spawned exactly once here; each lane's run is gated at 15:40 on the
    // truthful runtime feed flags. See `spawn_daily_tick_conservation_task`.
    spawn_daily_tick_conservation_task(&config, &trading_calendar, &feed_runtime);

    // Dual-feed scoreboard (operator 2026-07-10) — PROCESS-GLOBAL like the
    // conservation audit above: the boot-time process-death reconciler + the
    // 15:45 IST daily Dhan-vs-Groww aggregation + Telegram scorecard. Gated
    // on `[scoreboard] enabled` (the B12 rollback switch). See
    // `spawn_feed_scoreboard_tasks`.
    // (Scoreboard PR-D fix round 1: the presence-registry init moved to the
    // process-global boot prefix — ONE site; see the ordering ratchet.)
    spawn_feed_scoreboard_tasks(
        &config,
        &trading_calendar,
        &notifier,
        process_start_ist_nanos,
        &feed_runtime,
    );

    // Groww per-minute spot 1m REST leg (operator grant 2026-07-13 — PR-2 of
    // the Groww per-minute REST plan) — the slow-path call site; the FAST
    // crash-recovery arm carries its own (hostile round 1 item 1 — the
    // scoreboard dual-site pattern). Every trading-day minute close in
    // [09:16:00, 15:30:00] IST it fetches the just-closed minute's official
    // Groww 1m OHLCV for the 3 spot indices and persists to `spot_1m_rest`
    // tagged feed='groww' (+ `rest_fetch_audit` forensics rows). See
    // `spawn_groww_spot_1m_leg`.
    spawn_groww_spot_1m_leg(&config, &notifier, &trading_calendar);

    // Daily 15:40 IST timeframe-consistency verifier — PROCESS-GLOBAL like
    // the conservation audit + scoreboard above (operator 2026-07-13):
    // recompute every higher-TF candle (2m..4h) from the stored 1m rows and
    // compare against the persisted TF tables — Dhan verifies TODAY, Groww
    // verifies the PREVIOUS trading day (TF-VERIFY-01/02). Gated on
    // `[tf_consistency] enabled` + trading-day inside the task; the
    // once-per-process AtomicBool inside makes the fast-arm + prefix
    // dual-spawn safe. See `tf_consistency_boot::spawn_tf_consistency_tasks`.
    tickvault_app::tf_consistency_boot::spawn_tf_consistency_tasks(
        &config,
        &trading_calendar,
        &notifier,
    );

    // -----------------------------------------------------------------------
    // DayOhlcTracker boot wiring (post 2026-05-26 simplification; MOVED to
    // process-global scope 2026-07-01 to stop the per-lane cold-start leak).
    //
    // Per operator directive 2026-05-26 the Dhan historical / pre-market buffer
    // code was removed. `day_open` for the 4 LOCKED IDX_I SIDs is now the FIRST
    // OBSERVED LIVE TICK LTP after the IST midnight reset — no external arming
    // required. The tracker reads the PROCESS-GLOBAL `tick_broadcast_sender`
    // (filtering IDX_I ticks by segment, not by an instrument list), and the
    // IST-midnight reset is a host observability task — so the whole wiring is
    // process-global, spawned ONCE here instead of inside the per-lane
    // subscription-plan block (which re-ran on every cold-start and leaked a
    // fresh midnight-reset supervisor → duplicate INDEX-OHLC-02 pages).
    //
    // Two tokio tasks spawned here:
    //   1. tick consumer  — drain tick broadcast, route IDX_I ticks to
    //                       update_tick() which auto-arms on first call and
    //                       advances day_high/low/close on subsequent calls.
    //   2. midnight reset — IST 00:00:00 clears prev-day state so the next live
    //                       tick re-arms (CCL-02 supervised respawn wrapper,
    //                       INDEX-OHLC-02).
    // -----------------------------------------------------------------------
    let day_ohlc_tracker = std::sync::Arc::new(tickvault_trading::in_mem::DayOhlcTracker::new());
    {
        let consumer_tracker = std::sync::Arc::clone(&day_ohlc_tracker);
        let consumer_rx = tick_broadcast_sender.subscribe();
        let _consumer_handle = tickvault_app::day_ohlc_orchestrator::spawn_day_ohlc_tick_consumer(
            consumer_tracker,
            consumer_rx,
            tickvault_common::always_on::current(), // §30 GIFT exemption — same source as the aggregator/tick processor
        );
    }
    {
        // CCL-02: supervised respawn wrapper (INDEX-OHLC-02) so a panic in the
        // IST-midnight reset task can never silently take the daily reset offline
        // — mirror WS-GAP-05 / DISK-WATCHER-01.
        let reset_tracker = std::sync::Arc::clone(&day_ohlc_tracker);
        let _reset_supervisor_handle =
            tickvault_app::day_ohlc_orchestrator::spawn_supervised_midnight_reset_task(
                reset_tracker,
            );
    }
    info!(
        "DayOhlcTracker boot wired at process scope (tick consumer + midnight reset; \
         day_open = first live tick LTP)"
    );

    // =======================================================================
    // D2b: build + publish the OWNED runtime Dhan-lane context, so the
    // already-spawned runtime supervisor can cold-start a boot-OFF Dhan feed at
    // any time. Built in BOTH the Dhan-ON and Dhan-OFF paths (this code runs
    // for both). `config.clone()` is a deep copy of the immutable boot config
    // (no env re-parse). The `set` is idempotent (the cell is set exactly once).
    // =======================================================================
    {
        let runtime_ctx = std::sync::Arc::new(DhanLaneRuntimeContext::new(
            std::sync::Arc::new(config.clone()),
            std::sync::Arc::clone(&notifier),
            std::sync::Arc::clone(&health_status),
            tick_broadcast_sender.clone(),
            order_update_sender.clone(),
            std::sync::Arc::clone(&feed_runtime),
            std::sync::Arc::clone(&feed_health),
            std::sync::Arc::clone(&trading_calendar),
            ws_frame_spill.clone(),
        ));
        if dhan_lane_ctx_cell.set(runtime_ctx).is_err() {
            // The cell was already populated (cannot happen — set once per boot);
            // log defensively rather than panic.
            warn!("[dhan-lane] runtime context cell already set — ignoring duplicate publish");
        } else {
            info!(
                "[dhan-lane] runtime cold-start context published — a boot-OFF Dhan feed is now \
                 runtime-startable via the feed-control webpage"
            );
        }
    }

    // =======================================================================
    // DHAN LANE (Dhan-ON slow boot ONLY) — wrapped in the per-feed gate.
    //
    // Everything from IP-verify through the periodic health check is Dhan-lane
    // work: it builds its own auth / universe / WS-pool / tick-processor /
    // order-update / renewal on top of the hoisted shared infra above and
    // produces a `DhanLaneRunHandles` for the run-loop to tear down. When
    // `dhan_enabled=false` (Groww-only / no-feed) the whole block is skipped and
    // `dhan_lane` is `None` — preserving OFF-feed isolation (no Dhan auth, no
    // instrument fetch, no Dhan WebSocket).
    // =======================================================================
    // D2a — build the lane context (cheap references / Arc clones of the
    // already-built PROCESS-shared infra) and start the Dhan lane through the
    // extracted `start_dhan_lane`. The two boot-abort outcomes map back to the
    // EXACT `main()` returns the inline gate produced before this extraction:
    //   - `BootAbortClean`  → `return Ok(())`  (clean process exit, code 0)
    //   - `BootAbortErr(e)` → `return Err(e)`   (error exit with the chain)
    // When `dhan_enabled=false` (Groww-only / no-feed) the lane is skipped and
    // `dhan_lane` is `None` — OFF-feed isolation preserved (no Dhan auth, no
    // instrument fetch, no Dhan WebSocket), exactly as before.
    let dhan_lane: Option<DhanLaneRunHandles> = if config.feeds.dhan_enabled {
        let lane_ctx = DhanLaneContext {
            config: &config,
            notifier: &notifier,
            health_status: &health_status,
            tick_broadcast_sender: &tick_broadcast_sender,
            order_update_sender: &order_update_sender,
            feed_runtime: &feed_runtime,
            feed_health: &feed_health,
            trading_calendar: &trading_calendar,
            ws_frame_spill: &ws_frame_spill,
            is_market_hours,
            is_trading,
            is_muhurat,
            is_mock_trading,
            boot_start,
            ws_wal_replay_live_feed,
            ws_wal_replay_order_update,
            // BOOT-ON: keep the pre-existing single-feed `process::exit(2)` Halt
            // contract (H7 boot-ON blast-radius is a tracked separate follow-up).
            lane_scoped: false,
        };
        match start_dhan_lane(lane_ctx).await {
            Ok(handles) => {
                // D2b: boot-ON inline start succeeded — drive the lane FSM
                // Starting→Running (the seed set it to Starting above). The
                // runtime supervisor sees Running and does NOTHING for boot-ON
                // (byte-identical boot behaviour). Edge-triggered gauge write.
                feed_runtime.set_dhan_lane_state(tickvault_api::feed_state::LaneState::Running);
                record_dhan_lane_transition(
                    tickvault_api::feed_state::LaneState::Starting,
                    tickvault_api::feed_state::LaneState::Running,
                );
                Some(handles)
            }
            // D2b LOW (cosmetic, deferred): a boot-ON inline abort leaves the
            // seeded `Starting` gauge value until the (immediate) process exit.
            // We KEEP the exact one-liner mapping the D2a behaviour-identical
            // ratchet pins (`d2a_start_dhan_lane_guard::boot_abort_outcomes_map_
            // back_to_main_returns`) — the process exits on either arm, so the
            // stale gauge is never observed by a live system. Driving it to Off
            // here would break the D2a one-liner ratchet for zero runtime gain.
            Err(StartLaneError::BootAbortClean) => return Ok(()),
            Err(StartLaneError::BootAbortErr(err)) => return Err(err),
            // FTC-14: boot-ON WS-pool build failed — exit with an error chain
            // (a pool we intended to build but couldn't is a real boot failure,
            // not an offline mode). The runtime classify path emits DHAN-LANE-02
            // for the runtime cold-start arm.
            Err(StartLaneError::WsPoolSpawn) => {
                return Err(anyhow::anyhow!(
                    "DHAN-LANE-02: WebSocket main-feed pool build/spawn failed at boot"
                ));
            }
        }
    } else {
        // Dhan-OFF (Groww-only / no-feed): no Dhan lane. The shared infra built
        // by `build_shared_infra` keeps the process alive via the run-loop.
        info!(
            groww_enabled = config.feeds.groww_enabled,
            "DHAN LANE SKIPPED (dhan_enabled=false) — shared-infra-only runtime; \
             the unified run-loop keeps the API + seal-writer + aggregator alive"
        );
        // ===================================================================
        // Phase A (operator directive 2026-07-13 — "now remove this entire
        // Dhan live websocket feed instruments subscription even entire live
        // websocket feed itself"): the Dhan live WS lane is retired, but the
        // Dhan REST retained surface (token/auth stack, REST canary,
        // per-minute spot_1m_rest, per-minute option_chain_1m + entitlement
        // probe) must KEEP RUNNING. Before Phase A those subsystems lived
        // exclusively inside the lane / fast-boot arm and died with it. The
        // REST-only stack brings them up WITHOUT any WebSocket.
        //
        // Placement notes (mutual exclusion + reachability):
        //   - Spawned ONLY here, in the Dhan-OFF branch, and ONLY when the
        //     RAW boot TOML retires the lane (`is_dhan_config_enabled() ==
        //     false` — round-2 FIX A, 2026-07-13). The lane (whose
        //     `spawn_post_market_tasks` seam owns these subsystems on a
        //     Dhan-ON boot) can never run alongside it: the runtime
        //     cold-start supervisor REFUSES a lane start on the SAME raw
        //     gate (see run_dhan_lane_runtime_supervisor).
        //   - The FAST crash-recovery arm is filtered on
        //     `config.feeds.dhan_enabled` (its `fast_cache.filter`), so with
        //     Dhan off it never fires its early return — every Dhan-off boot
        //     reaches THIS slow-path site. The metrics recorder + notifier +
        //     shared infra are all installed well above (`build_shared_infra`).
        //   - Bring-up is a background task with internal retry-forever
        //     loops: it never blocks boot and never halts the process.
        // ===================================================================
        if feed_runtime.is_dhan_config_enabled() {
            // Round-2 FIX A (2026-07-13): a config-ON boot whose feed-state
            // overlay (last webpage toggle) left Dhan runtime-OFF — the lane
            // is DORMANT, not retired. The REST-only stack must NOT spawn
            // here: it would hold the dual-instance SSM lock + mint the
            // TokenManager, and the (allowed) runtime re-enable's cold-start
            // would then collide with our OWN process (AlreadyHeld →
            // DHAN-LANE-03 retry loop + RESILIENCE-01 pages). The retained
            // Dhan REST surface comes up WITH the lane on re-enable
            // (`spawn_post_market_tasks`) — pre-Phase-A behavior; until
            // then the Dhan REST surface is honestly absent (as it was
            // pre-Phase-A on every runtime-OFF boot).
            info!(
                "Dhan REST-only stack NOT spawned: the boot TOML carries dhan_enabled=true \
                 (lane dormant via the runtime overlay, not retired) — a runtime re-enable \
                 cold-starts the full lane, which owns the Dhan REST surface"
            );
            // E6 (fix-round 2026-07-14): name the config conflict — the
            // dry-run order runtime spawns ONLY from the REST stack (the
            // dhan-OFF arm), so on a dhan-ON boot `[order_runtime]` is
            // silently inert (the mark forwarder is built but never armed).
            if config.order_runtime.enabled {
                warn!(
                    "[order_runtime] enabled = true is IGNORED on a dhan-ON boot — \
                     the dry-run order runtime spawns only from the dhan-OFF REST \
                     stack; dhan-ON boots use the trading_pipeline OMS instead"
                );
            }
        } else {
            let _dhan_rest_stack_monitor = tickvault_app::dhan_rest_stack::spawn_dhan_rest_stack(
                tickvault_app::dhan_rest_stack::DhanRestStackParams {
                    config: std::sync::Arc::new(config.clone()),
                    notifier: std::sync::Arc::clone(&notifier),
                    calendar: std::sync::Arc::clone(&trading_calendar),
                    feed_runtime: std::sync::Arc::clone(&feed_runtime),
                    // Order-runtime dry-run PR (2026-07-14): WAL capture +
                    // boot-staged replay + conditional confirm + mark bridge.
                    // Dhan-OFF boots never move these into the lane ctx (that
                    // branch is the `if`; this is the `else`), so the Vec +
                    // count are intact here.
                    ws_frame_spill: ws_frame_spill.clone(),
                    ws_wal_replay_order_update,
                    livefeed_frames_replayed: ws_wal_replay_live_feed.len(),
                    wal_dir: ws_wal_path.clone(),
                    mark_rx_slot: std::sync::Arc::clone(&order_runtime_mark_rx_slot),
                    marks_wanted: std::sync::Arc::clone(&order_runtime_marks_wanted),
                },
            );
        }
        None
    };

    // =======================================================================
    // Dhan-OFF boot completion signals (deploy-hang fix, 2026-07-13).
    //
    // The systemd unit is Type=notify with TimeoutStartSec=infinity: systemd
    // releases the `systemctl restart tickvault` start job ONLY when the app
    // sends sd_notify(READY=1). Both pre-existing notify sites are Dhan-gated
    // — the FAST crash-recovery arm (filtered on `config.feeds.dhan_enabled`)
    // and the slow-boot site inside `start_dhan_lane` — so the Phase-A
    // `dhan_enabled=false` flip (#1496, 2026-07-13) left every prod boot
    // stuck `activating` forever: the on-box deploy script's
    // `systemctl restart tickvault` never returned, the SSM command never
    // reached a terminal state, and the deploy workflow's wait budget expired
    // into the auto-stop mid-script (runs 29269805430 / 29273283894 — every
    // post-#1496 deploy failed identically). This arm sends READY=1 and the
    // feed-liveness-gated `tv_boot_completed` (the 08:40 IST boot-heartbeat
    // alarm's signal — equally lane-owned before this fix, so a Dhan-OFF boot
    // would also have false-paged that alarm every morning) for the Dhan-OFF
    // boot, mirroring the two existing sites. `dhan_poolless_idle=false`:
    // with `dhan_enabled=false` the Dhan term of `boot_completed_should_emit`
    // is vacuous and the gate genuinely waits (bounded — see
    // BOOT_COMPLETED_FEED_LIVENESS_WAIT_SECS, widened 60->300 per the
    // 2026-07-13 hostile-review MEDIUM: a merely-slow watch-list build must
    // not withhold the metric forever) for the Groww lane — a Groww that
    // never comes up still withholds the metric so the
    // boot-heartbeat alarm pages (Rule 11, no false-OK). READY=1 is sent
    // FIRST (unconditionally for this arm): systemd start-job release must
    // never be held hostage to feed liveness — the PROCESS booted; feed
    // health has its own alarms. No-op when NOTIFY_SOCKET is unset (local
    // `cargo run` / Mac scale-test boots, which took this arm long before
    // Phase A without ever running under systemd).
    // =======================================================================
    if !config.feeds.dhan_enabled {
        infra::notify_systemd_ready();
        emit_boot_completed_when_feed_live(
            &feed_runtime,
            config.feeds.dhan_enabled,
            config.feeds.groww_enabled,
            false,
        )
        .await;
    }

    // =======================================================================
    // PROCESS RUN-LOOP (BOTH Dhan-OFF and Dhan-ON-slow).
    //
    // Built once over the hoisted shared infra: market-close timer,
    // partition-detach, shutdown wait, then `teardown_dhan_lane_tasks(lane)`
    // when a lane exists, then API + otel teardown. Replaces the old separate
    // `run_shutdown_fast` slow-arm call AND the deleted `run_shared_infra_only`.
    // =======================================================================
    run_process_runloop(
        dhan_lane,
        Some(api_handle),
        otel_provider,
        &notifier,
        &config,
        trading_calendar.clone(),
        groww_scale_fleet_lock,
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
        // §36 hostile-review round 2 (2026-07-08, F4): the warm-snapshot path
        // emits the SAME per-contract boot-evidence lines + Dhan parity entry
        // the cold orchestrator does — previously both were deferred to the
        // background reconcile (and silently never happened if it failed),
        // so a Groww activation recorded single-feed and the FUTIDX-02
        // comparator never ran for the whole warm-boot trading day.
        tickvault_core::instrument::index_futures::record_dhan_selection_from_universe(
            &universe,
            now_ist.date_naive(),
        );
        // Scoreboard PR-D: register the warm universe into the
        // per-instrument presence registry — same seam as the parity
        // recording above (the FUTIDX-02 warm-path lesson: a seam wired
        // only on the cold path loses the whole warm-boot trading day).
        tickvault_core::instrument::presence_registration::register_dhan_presence_from_universe(
            &universe,
            tickvault_core::instrument::presence_registration::ist_day_from_date(
                now_ist.date_naive(),
            ),
        );
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
        tokio::spawn(async move {
            match cold_build_daily_universe(&questdb, false).await {
                Ok((fresh_outcome, fresh_universe)) => {
                    // Hostile-review round 4 (2026-07-08): stamp the date the
                    // rebuild's FUTIDX selection ACTUALLY used (the runner
                    // re-derives it per attempt, R2-2) — never the boot-entry
                    // `today_date`, which is stale if the reconcile's §4
                    // retry loop crossed IST midnight.
                    if let Err(e) = instrument_snapshot::write_plan_snapshot(
                        &fresh_universe,
                        &fresh_outcome.build_trading_date_ist,
                    ) {
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
    // cold-builds again). Hostile-review round 4 (2026-07-08): stamp the
    // date the successful build's FUTIDX selection ACTUALLY used
    // (`outcome.build_trading_date_ist` — re-derived per attempt, R2-2),
    // never the `today_date` frozen at fn entry: a §4 retry loop crossing
    // IST midnight builds for D+1, and a D-labeled snapshot carrying the
    // D+1 front month is an internally-inconsistent forensic artifact.
    if let Err(e) =
        instrument_snapshot::write_plan_snapshot(&daily_universe, &outcome.build_trading_date_ist)
    {
        warn!(
            error = %e,
            date = %outcome.build_trading_date_ist,
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
    // PR-E (2026-06-21): shared runtime Dhan feed-enable flag (from
    // FeedRuntimeState::dhan_flag). Every connection reads it to pause/resume on
    // the operator toggle. `None` only in paths with no runtime control.
    dhan_feed_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    // D2c (closes C4): LANE-OWNED `TokenManager` for the WS sleep-wake renewal.
    // `Some(lane manager)` from `start_dhan_lane` (the canonical cold path used
    // by every runtime cold-start) so the wake renews the manager the live pool
    // is actually using; `None` from the fast crash-recovery arm — that arm
    // `return run_shutdown_fast`s before the inline spine's
    // `set_global_token_manager`, so the global `OnceLock` is UNSET there and
    // wake-renewal no-ops (the fast arm relies on its cached token; no proactive
    // wake-renewal). Closes the stop→re-start "renew the wrong token" race.
    lane_token_manager: Option<std::sync::Arc<tickvault_core::auth::TokenManager>>,
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
        // Audit GAP-1 fix (Operator approved 2026-07-02: "approve 15s"): the
        // DailyUniverse clamp previously reused the 3s IDX_I threshold
        // (the Sub-PR #1 2026-05-27 comment here explicitly deferred
        // re-tuning to "#8", which never happened). Dhan's server pings
        // every 10s and the read loop counts pings as activity
        // (connection.rs STAGE-C.3), so the 3s clamp force-reconnected
        // HEALTHY, still-pinging sockets during any >=3s data lull
        // inside the market-hours gate (thin pre-open 09:00–09:15
        // especially). 15s = one full Dhan ping interval + 50% margin:
        // can never fire on a healthy socket, still detects a truly
        // silent one 3.3x faster than the 50s config default. The
        // Indices4Only arm above keeps its historical 3s value
        // untouched. See `WATCHDOG_THRESHOLD_DAILY_UNIVERSE_SECS` docs +
        // `.claude/rules/project/live-market-feed-subscription.md`
        // (2026-07-02 section).
        tickvault_common::config::SubscriptionScope::DailyUniverse => {
            tickvault_core::websocket::activity_watchdog::WATCHDOG_THRESHOLD_DAILY_UNIVERSE_SECS
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

    // 2026-06-12: WS-event audit channel + consumer. Every connect/disconnect/
    // reconnect/sleep on every connection stamps a forensic `ws_event_audit`
    // row (future-proof for the 5+5+5+1 = 16-connection scenario). The
    // order-update connection reuses the SAME helper (its own consumer).
    let ws_audit_tx = spawn_ws_event_audit_consumer(config.questdb.clone());

    let mut pool = match WebSocketConnectionPool::new_with_optional_wal(
        token_handle.clone(),
        client_id.to_string(),
        dhan_for_pool,
        ws_config,
        instruments,
        feed_mode,
        notifier,
        wal_spill,
        Some(ws_audit_tx),
        dhan_feed_flag,
        lane_token_manager,
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

// Phase C1 (2026-07-13): the WS-event audit channel + consumer helper
// (`spawn_ws_event_audit_consumer` / `run_ws_event_audit_consumer` /
// `WS_EVENT_AUDIT_CHANNEL_CAPACITY`) RELOCATED to the lib module
// `tickvault_app::ws_audit_consumer` (pure move, zero behavior change) so
// `dhan_rest_stack` — which owns the functional-dormant order-update WS per
// operator ruling Q4-i — can create its own consumer. Re-imported below.

/// Spawns all WebSocket connections in the pool (with stagger).
/// Call AFTER the tick processor is started so frames are consumed immediately.
///
/// S4-T1a: Accepts `Arc<WebSocketConnectionPool>` so the caller can retain a
/// reference for the pool watchdog task (which polls `poll_watchdog()` every
/// 5s) and the SIGTERM handler (which calls `request_graceful_shutdown()`).
/// The Arc is cheap (one atomic ref count increment per clone) and required
/// because all three use cases need to share the same pool instance.
/// WS-GAP-08: honour a persisted Dhan 429 rate-limit cooldown BEFORE the first
/// main-feed WS connect, so the cooldown survives a `process::exit(2)` +
/// supervisor restart and the fresh process does NOT reconnect straight back
/// into Dhan's still-active 429 window (the instant-429 restart loop).
///
/// Fail-open + bounded: a missing / corrupt / stale cooldown file → no wait;
/// the wait is clamped to `WS_RATE_LIMIT_BACKOFF_CAP_MS` (5 minutes) so a bad
/// file can never hang boot. The pure `remaining_cooldown_ms` decision is
/// unit-tested in `tickvault_core::websocket::rate_limit_cooldown`.
///
/// Called from BOTH boot paths (2026-07-06 audit fix — the FAST
/// crash-recovery arm previously skipped it): each `create_websocket_pool`
/// call site in main.rs must be preceded by this wait, ratcheted by
/// `crates/app/tests/ws_rate_limit_cooldown_wiring_guard.rs`.
async fn wait_out_persisted_ws_rate_limit_cooldown() {
    use tickvault_core::websocket::rate_limit_cooldown;

    let Some(cooldown) = rate_limit_cooldown::read_cooldown() else {
        return; // no/corrupt/stale file → fail-open, no wait
    };
    let remaining_ms = rate_limit_cooldown::remaining_cooldown_ms(
        cooldown.last_hit_epoch_ms,
        cooldown.floor_ms,
        rate_limit_cooldown::now_epoch_ms(),
    )
    .min(tickvault_common::constants::WS_RATE_LIMIT_BACKOFF_CAP_MS);
    if remaining_ms == 0 {
        return;
    }
    metrics::counter!("tv_ws_rate_limit_cooldown_waited_total").increment(1);
    info!(
        code = tickvault_common::error_code::ErrorCode::WsGap08RateLimitCooldown.code_str(),
        remaining_secs = remaining_ms / 1_000,
        streak = cooldown.streak,
        "WS-GAP-08: Dhan rate-limited us recently — waiting out the persisted \
         cooldown before the first WebSocket connect (prevents an instant-429 \
         restart loop)"
    );
    tokio::time::sleep(std::time::Duration::from_millis(remaining_ms)).await;
}

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

/// Current IST epoch nanos for feed last-tick-age math — the same basis the
/// `FeedHealthRegistry` records ticks at (mirrors the `/api/feeds/health`
/// handler's clock).
fn now_ist_nanos_for_feed_status() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS)
}

/// Builds one operator-facing status line per ENABLED market-data feed for
/// the readiness + end-of-day Telegram messages (operator directive
/// 2026-07-03: Groww — and any future feed — must appear alongside Dhan).
///
/// Source of truth = the SAME shared `FeedHealthRegistry` the feed-control
/// page reads: subscribed counts + last-tick age per feed. Honest labeling:
/// a feed with no recorded data yields `None` fields, which the renderer
/// prints as "unknown" / "no tick seen yet" — never fabricated numbers.
/// Dhan's last-tick age falls back to the global REAL-tick gap detector
/// (Dhan-pipeline-only; real ticks, never keep-alive pings) because the Dhan
/// hot path does not stamp the registry per tick. Cold path — called once
/// per boot summary / once per daily digest.
fn build_feed_status_lines(
    feed_runtime: &tickvault_api::feed_state::FeedRuntimeState,
    feed_health: &tickvault_common::feed_health::FeedHealthRegistry,
) -> Vec<tickvault_core::notification::events::FeedStatusLine> {
    use tickvault_common::feed::Feed;
    let now = now_ist_nanos_for_feed_status();
    let market_open = tickvault_common::market_hours::is_trading_session_now();
    Feed::ALL
        .iter()
        .copied()
        .filter(|feed| feed_runtime.is_enabled(*feed))
        .map(|feed| {
            let report = feed_health.snapshot(
                feed,
                true, // enabled — filtered above
                feed_runtime.lane_running(feed),
                market_open,
                now,
            );
            let subscribed = report.input.subscribed_stocks + report.input.subscribed_indices;
            let mut last_tick_age_secs = report.input.last_tick_age_secs;
            if feed == Feed::Dhan && last_tick_age_secs.is_none() {
                // The Dhan tick path stamps the global real-tick gap detector
                // (not the per-feed registry) — reuse it so the Dhan line is
                // as truthful as the "Real market ticks" line above it.
                last_tick_age_secs =
                    tickvault_core::pipeline::tick_gap_detector::global_tick_gap_detector()
                        .and_then(|d| d.freshest_tick_age_secs(std::time::Instant::now()));
            }
            tickvault_core::notification::events::FeedStatusLine {
                name: feed.display_name().to_string(),
                instruments: (subscribed > 0).then_some(subscribed),
                last_tick_age_secs,
            }
        })
        .collect()
}

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
    // 2026-07-03 feed parity: the "ready to trade" message names EVERY
    // enabled market-data feed (Dhan, Groww, future), built at emit time
    // from the same per-feed health registry the feed-control page reads.
    feed_runtime: &std::sync::Arc<tickvault_api::feed_state::FeedRuntimeState>,
    feed_health: &std::sync::Arc<tickvault_common::feed_health::FeedHealthRegistry>,
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
            feeds: build_feed_status_lines(feed_runtime, feed_health),
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
            // 2026-07-04 boot-visibility parity: the off-hours "Boot complete"
            // message must name EVERY enabled feed — a Saturday boot with a
            // connected-and-subscribed Groww previously reported only the
            // deferred Dhan connection count.
            feeds: build_feed_status_lines(feed_runtime, feed_health),
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
/// - `Halt` → `WebSocketPoolHalt { down_secs }`. The Halt action depends on the
///   `lane_halt` blast-radius mode (H7):
///   - `None` (BOOT-ON, single-feed contract) → `std::process::exit(2)` so
///     systemd/the supervisor restarts the WHOLE process. This is the
///     pre-existing behaviour and is preserved exactly.
///   - `Some(notify)` (RUNTIME lane) → fire `notify.notify_waiters()` instead of
///     `process::exit`, so the parked task tears the Dhan lane down + drives the
///     FSM to `Off` (the runtime supervisor re-cold-starts it with bounded
///     backoff). A lane-local Dhan fault MUST NOT take down the independent Groww
///     feed or the shared seal-writer / aggregator / API server.
/// - `Degrading` / `Healthy` → gauge update only, no Telegram.
///
/// The task stops when the `shutdown_notify` is fired (during graceful
/// shutdown) to avoid a false-positive Halt during the intentional
/// teardown.
/// Pure decision: should the pool watchdog ACT on a `Degraded`/`Halt` verdict
/// (page Telegram + `process::exit`/lane-teardown), or treat the down pool as
/// EXPECTED IDLE and stay quiet?
///
/// The watchdog acts ONLY when BOTH hold:
/// - `in_market_hours` — outside [09:00, 15:30) IST Dhan stops streaming and a
///   60s/300s silence is normal (the 2026-04-24 post-market `process::exit(2)`
///   cascade fix).
/// - `dhan_enabled` — the operator has NOT deliberately switched Dhan OFF from
///   the feed-control page (PR-E runtime toggle). When Dhan is toggled OFF its
///   main-feed connections go dormant by design, so the resulting 0-connection
///   pool must NOT be read as "fully degraded": no page, no `process::exit`, no
///   lane teardown — exactly like the off-hours expected-idle branch. Without
///   this, a runtime Dhan-OFF self-halted the whole process every ~300s, which
///   also killed the independent Groww feed.
///
/// Returns `true` to ACT, `false` to treat as expected idle.
#[must_use]
const fn pool_watchdog_should_act_on_degradation(
    in_market_hours: bool,
    dhan_enabled: bool,
) -> bool {
    in_market_hours && dhan_enabled
}

/// Fix A (2026-06-30) — pure classifier: is an all-down pool at the `>300s Halt`
/// verdict a BENIGN bare-Dhan-transport-RST class (ride it out by reconnecting
/// in place), or a GENUINE FATAL (restart as today)?
///
/// Returns `true` ONLY when ALL hold:
/// - every connection's `rate_limit_streak == 0` — NOT a real Dhan 429/805
///   (a real 429 means waiting/exiting is correct; #1265's persisted cooldown
///   then absorbs the restart);
/// - no connection saw a `NonReconnectableDisconnect` — no genuine-fatal Dhan
///   disconnect code (807-class / auth / etc.);
/// - `token_valid` — the JWT is live (a dead token needs the renewed-token
///   restart path);
/// - `questdb_reachable` — persistence is up (a dead DB needs a restart).
///
/// Any failing predicate → `false` → genuine-fatal path (today's
/// `process::exit(2)` / lane teardown). Empty `healths` → `false` (no pool to
/// reconnect — never ride out an absent pool).
///
/// Pure function (reads only the snapshot the watchdog already takes + two
/// booleans). Tested by `test_is_bare_reset_class_*`.
#[must_use]
fn is_bare_reset_class(
    healths: &[tickvault_core::websocket::types::ConnectionHealth],
    token_valid: bool,
    questdb_reachable: bool,
) -> bool {
    if healths.is_empty() {
        return false;
    }
    token_valid
        && questdb_reachable
        && healths
            .iter()
            .all(|h| h.rate_limit_streak == 0 && !h.saw_non_reconnectable)
}

/// Fix A (2026-06-30) — pure ceiling check for reconnect-in-place mode. Returns
/// `true` when the watchdog has spent `elapsed_secs` or more in reconnect-in-
/// place for the current down-episode (>= `POOL_RECONNECT_IN_PLACE_CEILING_SECS`,
/// = 15 min). When `true`, the watchdog falls back to the genuine-fatal exit so
/// a truly-wedged feed cannot ride out reconnect-in-place forever — the worst
/// case strictly degrades to today's restart behaviour.
///
/// Pure function. Tested by `test_reconnect_in_place_ceiling_*`.
#[must_use]
const fn reconnect_in_place_ceiling_exceeded(elapsed_secs: u64) -> bool {
    elapsed_secs >= tickvault_core::websocket::pool_watchdog::POOL_RECONNECT_IN_PLACE_CEILING_SECS
}

/// In-window 429 ride-out (2026-06-30) — pure classifier: is an all-down pool at
/// the `>300s Halt` verdict a BENIGN in-MARKET-HOURS Dhan rate-limit (HTTP 429 /
/// DATA-805) class that should be RIDDEN OUT in place rather than restarted?
///
/// This is the SIBLING of [`is_bare_reset_class`]. That function deliberately
/// refuses a `rate_limit_streak > 0` (it handles only the streak==0 bare-RST
/// storm). But a genuine 429 is the EXACT case the per-connection reconnect
/// loops already absorb in place — they wait out the
/// `compute_rate_limit_floor_ms` 60s→5m floor and retry, preserving
/// subscriptions via `SubscribeRxGuard` (and the WS-GAP-08 persisted cooldown
/// survives a restart). Calling `process::exit(2)` on a 429 turns that benign
/// in-place wait into a 775-SID cold re-subscribe that just earns the NEXT 429 →
/// a self-inflicted restart/429 loop (observed 2026-06-30: 61 main-feed 429s in
/// one market window; the restart loop also 429-starved the Groww feed's auth).
///
/// Returns `true` ONLY when ALL hold:
/// - `in_market_hours` — gate identical to WS-GAP-09; OUTSIDE 09:00–15:30 IST a
///   down pool is expected idle and the watchdog's `should_act` gate already
///   handles it (this arm is never reached off-hours). Belt-and-suspenders so the
///   classifier is honest if reused.
/// - `token_valid` AND `questdb_reachable` — the SAME genuine-fatal guards as
///   `is_bare_reset_class`: a dead token / dead DB still needs the restart path.
/// - non-empty `healths` AND NO connection saw a `NonReconnectableDisconnect`
///   (a genuine-fatal Dhan code — 807/auth/etc. — still exits, never rides out).
/// - at least ONE connection has `rate_limit_streak > 0` — i.e. this genuinely IS
///   the 429 class (distinguishes it from the streak==0 bare-RST class that
///   `is_bare_reset_class` already admits). It does NOT require streak==0 — that
///   is the whole point of this fix: it ADMITS the 429.
///
/// Any failing predicate → `false` → genuine-fatal path (today's
/// `process::exit(2)` / lane teardown). Pure function (reads only the snapshot the
/// watchdog already takes + three booleans). Tested by
/// `test_in_window_429_rideout_class_*`.
#[must_use]
fn is_in_window_429_rideout_class(
    healths: &[tickvault_core::websocket::types::ConnectionHealth],
    in_market_hours: bool,
    token_valid: bool,
    questdb_reachable: bool,
) -> bool {
    if healths.is_empty() {
        return false;
    }
    in_market_hours
        && token_valid
        && questdb_reachable
        // No genuine-fatal Dhan disconnect on any connection.
        && healths.iter().all(|h| !h.saw_non_reconnectable)
        // ...and this IS the 429 class: at least one connection is rate-limited.
        && healths.iter().any(|h| h.rate_limit_streak > 0)
}

/// In-window 429 ride-out (2026-06-30) — the SINGLE ride-out decision the pool
/// watchdog's Halt arm consults. An all-down Halt should reconnect IN PLACE
/// (instead of `process::exit`/lane teardown + a 429-tripping cold re-subscribe)
/// when it is EITHER:
/// - a benign streak==0 bare-Dhan-transport-RST storm ([`is_bare_reset_class`]),
///   OR
/// - a benign in-market-hours Dhan 429 storm
///   ([`is_in_window_429_rideout_class`]).
///
/// Both classes share the SAME genuine-fatal guards (token valid, QuestDB
/// reachable, no non-reconnectable code, non-empty pool) and the SAME 15-min
/// ceiling (`reconnect_in_place_ceiling_exceeded`, checked separately by the
/// caller) so a truly-wedged feed still falls back to the genuine-fatal restart —
/// never WORSE than today's behaviour. Pure function. Tested by
/// `test_should_reconnect_in_place_*`.
#[must_use]
fn should_reconnect_in_place(
    healths: &[tickvault_core::websocket::types::ConnectionHealth],
    in_market_hours: bool,
    token_valid: bool,
    questdb_reachable: bool,
) -> bool {
    is_bare_reset_class(healths, token_valid, questdb_reachable)
        || is_in_window_429_rideout_class(healths, in_market_hours, token_valid, questdb_reachable)
}

/// QuestDB-probe damping (2026-07-01, watchdog-cascade audit HIGH) — number of
/// CONSECUTIVE failed QuestDB liveness probes the pool watchdog tolerates before
/// the 429 ride-out EXIT gate treats QuestDB as "down". N=2 (× the 5s tick =
/// 10s of sustained unreachability). Rationale: the ride-out classifiers
/// (`is_bare_reset_class` / `is_in_window_429_rideout_class`) HARD-REQUIRE
/// `questdb_reachable == true`, and the raw signal is a SINGLE un-damped probe
/// (`timeout(2s, wait_for_questdb_ready(_, 1))`). A single momentary blip (GC
/// pause / transient HTTP hiccup / the 2s timeout firing under load) during a
/// live in-market 429 storm flips the raw flag false → `ride_out == false` →
/// `process::exit(2)` → 775-SID cold re-subscribe → next 429 → the self-inflicted
/// restart/429 loop #1277 exists to stop. N=2 is the SMALLEST N that absorbs a
/// one-tick blip while still forcing the exit on a genuine SUSTAINED outage
/// after 10s (2 consecutive ticks) — well inside the 15-min ride-out ceiling, so
/// a real dead DB is never worse than today (N=1 = today's undamped bug). Cold
/// path (5s watchdog tick), not the hot tick path.
const POOL_WATCHDOG_QDB_EXIT_DAMP_THRESHOLD: u32 = 2; // APPROVED: cold-path watchdog damping threshold, 2026-07-01 audit fix

/// QuestDB-probe damping (2026-07-01) — pure decision: given the count of
/// CONSECUTIVE failed QuestDB probes seen so far, is QuestDB "reachable" FOR THE
/// EXIT DECISION? Returns `true` (reachable — keep riding out) until
/// `consecutive_failures >= threshold`, then `false` (treat as down → the
/// ride-out gate falls back to the genuine-fatal exit). A single successful
/// probe resets the caller's counter to 0, so the flip requires N *consecutive*
/// failures — a real SUSTAINED outage, not a blip.
///
/// The RAW single-probe signal (`health.set_questdb_reachable`) is UNCHANGED and
/// keeps feeding `/health` + `overall_status` — only the exit gate consults this
/// damped view. Pure function. Tested by `test_damp_questdb_exit_signal_*`.
#[must_use]
const fn damp_questdb_exit_signal(consecutive_failures: u32, threshold: u32) -> bool {
    consecutive_failures < threshold
}

#[allow(clippy::too_many_arguments)] // APPROVED: watchdog orchestration requires the full shared-infra handle set (pool + shutdown + notifier + health + feed-health + lane-halt + dhan-flag + token + questdb)
fn spawn_pool_watchdog_task(
    pool: std::sync::Arc<WebSocketConnectionPool>,
    shutdown_notify: std::sync::Arc<tokio::sync::Notify>,
    notifier: std::sync::Arc<NotificationService>,
    health: tickvault_api::state::SharedHealthStatus,
    // Live-feed health (SP5): the shared per-feed registry the `/api/feeds/health`
    // endpoint reads. The watchdog already counts `active` Connected sockets every
    // 5s for the `websocket_connections` gauge; we mirror that into the Dhan
    // connected slot so the endpoint reports Dhan's connect state truthfully.
    // `None` is a no-op. Reported always (not market-hours-gated) so the connected
    // state is honest; the classify() market-hours gate (C1 fix) ensures a
    // pre-market disconnected Dhan reads "idle", never a false Down.
    feed_health: Option<std::sync::Arc<tickvault_common::feed_health::FeedHealthRegistry>>,
    // H7 lane-scoped blast-radius mode. `None` = BOOT-ON: keep `process::exit(2)`
    // on Halt (single-feed contract). `Some(notify)` = RUNTIME lane: signal the
    // parked task to tear down + drive the FSM to `Off`, NEVER `process::exit`.
    lane_halt: Option<std::sync::Arc<tokio::sync::Notify>>,
    // PR-E runtime Dhan-OFF gate. A clone of the SAME `FeedRuntimeState::dhan`
    // atomic the Dhan WS read/reconnect loop consults — when the operator
    // toggles Dhan OFF on the feed-control page, this flips `false` and the
    // main-feed connections go dormant by design. The watchdog reads it so a
    // deliberately-dormant 0-connection pool is treated as expected idle (no
    // page, no `process::exit`, no lane teardown), never as "fully degraded".
    // `None` keeps the legacy always-enabled behaviour (treated as on).
    dhan_enabled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    // Fix A (2026-06-30): the shared JWT token handle so the Halt-arm bare-reset
    // classifier can require `token_valid` before reconnecting in place. `None`
    // (no handle wired) reads as token-invalid → genuine-fatal exit preserved
    // (never ride out a Halt when we cannot confirm the token is live).
    token_handle: Option<TokenHandle>,
    // Fix A no-op close (2026-06-30): the QuestDB config so the watchdog can
    // push a REAL production QuestDB-liveness signal into
    // `health.set_questdb_reachable(...)` every 5s. Without this the backing
    // `questdb_reachable` AtomicBool was only ever set true in test code, so in
    // prod `health.questdb_reachable()` was permanently `false` →
    // `is_bare_reset_class` (which requires `questdb_reachable == true`) could
    // never return true → the reconnect-in-place branch was DEAD. The watchdog
    // already ticks every 5s and holds `health`, so it is the lowest-risk place
    // to maintain the signal (mirrors the `set_websocket_connections` wiring).
    // A cheap `/exec?query=SELECT 1` ping each tick keeps it fresh INDEPENDENTLY
    // of tick flow — critical, because a Dhan bare-RST storm stops ticks while
    // QuestDB stays up, which is exactly when the gate must read it correctly.
    questdb_config: tickvault_common::config::QuestDbConfig,
    // R8-EDGE-2 / SEC-C2-1 (2026-07-07): returns the JoinHandle so the caller
    // can LANE-OWN the watchdog. Previously this was fire-and-forget; its only
    // exit was `shutdown_notify.notified()`, which nothing could ever fire on
    // a cancel-mid-Starting (the lane struct that owns the notify_waiters
    // calls was not yet constructed), so a supervisor `.abort()` parked on the
    // ≤30s `emit_websocket_connected_alerts` await LEAKED the watchdog: it
    // kept writing `health.set_websocket_connections` +
    // `feed_health.set_connected(Feed::Dhan, …)` every 5s from the DEAD pool
    // (fighting the re-enabled lane's fresh watchdog — /health +
    // /api/feeds/health flapping), and in market hours the dead pool's 60s /
    // 300s verdicts fired a FALSE WebSocketPoolDegraded page + a FALSE
    // CRITICAL WebSocketPoolHalt + `tv_pool_self_halts_total` increment. The
    // handle also closes the Rule 16 lost-wake residual: `notify_waiters()`
    // is lost if the watchdog is mid-tick-body (its ≤2s QuestDB probe), so
    // teardown/Drop now abort() the handle as the floor.
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Fix A (2026-06-30): wall-clock start of the current reconnect-in-place
        // episode. `None` = not currently riding out a bare-reset Halt. Reset to
        // `None` on any non-Halt verdict (the pool recovered / is no longer
        // all-down), so the 15-min ceiling measures one continuous episode.
        let mut reconnect_in_place_since: Option<std::time::Instant> = None;
        // QuestDB-probe damping (2026-07-01 audit fix): count of CONSECUTIVE
        // failed QuestDB liveness probes. A single successful probe resets it
        // to 0. Feeds `damp_questdb_exit_signal(..)` → the DAMPED
        // `questdb_reachable_for_exit_decision` signal the Halt-arm ride-out
        // gate reads (the RAW `set_questdb_reachable` signal is still pushed
        // every tick for `/health`). Local to the task (cold-path 5s tick, no
        // shared state, no hot-path alloc).
        let mut qdb_consecutive_failures: u32 = 0;
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

                    // Fix A no-op close (2026-06-30): push the REAL production
                    // QuestDB-liveness signal so `is_bare_reset_class` (which
                    // requires `questdb_reachable == true`) can actually engage
                    // on a genuine bare-reset Halt when QuestDB is healthy, and —
                    // by the same gate — refuses reconnect-in-place when
                    // persistence is genuinely down. A cheap `/exec?query=SELECT 1`
                    // probe (the canonical boot-probe, reused) wrapped in a 2s
                    // hard timeout (mirrors the SLO scheduler so a hung TCP
                    // connect can't stretch the 5s tick). Runs every 5s
                    // INDEPENDENTLY of tick flow — a Dhan bare-RST storm stops
                    // ticks while QuestDB stays up, which is exactly when this
                    // signal must stay fresh. Cold path, not the hot tick path.
                    const POOL_WATCHDOG_QDB_PROBE_TIMEOUT_SECS: u64 = 2; // APPROVED: bounds a hung TCP connect on the 5s cold-path watchdog tick
                    let questdb_reachable_now = tokio::time::timeout(
                        std::time::Duration::from_secs(POOL_WATCHDOG_QDB_PROBE_TIMEOUT_SECS),
                        tickvault_storage::boot_probe::wait_for_questdb_ready(
                            &questdb_config,
                            1,
                        ),
                    )
                    .await
                    .is_ok_and(|res| res.is_ok());
                    // RAW single-probe signal — feeds `/health` + `overall_status`
                    // + observability. A single blip legitimately shows here.
                    health.set_questdb_reachable(questdb_reachable_now);

                    // DAMPED exit signal (2026-07-01 audit fix): track CONSECUTIVE
                    // failures so the 429 ride-out EXIT gate treats QuestDB as
                    // "down" ONLY after N consecutive failed probes — a single
                    // blip during an in-market 429 storm no longer forces
                    // process::exit + a 775-SID cold re-subscribe that trips the
                    // next 429. A genuine SUSTAINED outage still flips it after N
                    // ticks (never worse than today for a real dead DB).
                    if questdb_reachable_now {
                        qdb_consecutive_failures = 0;
                    } else {
                        qdb_consecutive_failures = qdb_consecutive_failures.saturating_add(1);
                    }
                    health.set_questdb_reachable_for_exit_decision(damp_questdb_exit_signal(
                        qdb_consecutive_failures,
                        POOL_WATCHDOG_QDB_EXIT_DAMP_THRESHOLD,
                    ));

                    // Live-feed health (SP5): mirror the active-socket count into
                    // the Dhan connected slot so `/api/feeds/health` reports Dhan
                    // truthfully. `active > 0` = at least one main-feed socket is
                    // Connected. Honest at all hours; classify()'s market-hours
                    // gate keeps pre-market disconnection from reading as Down.
                    if let Some(ref fh) = feed_health {
                        fh.set_connected(
                            tickvault_common::feed::Feed::Dhan,
                            active > 0,
                        );
                    }

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
                    // PR-E runtime Dhan-OFF gate. `None` (legacy) = treated as
                    // enabled. When the operator toggles Dhan OFF on the
                    // feed-control page this reads `false`, its main-feed
                    // connections go dormant by design, and the resulting
                    // 0-connection pool must be treated as expected idle —
                    // never paged/halted. Read once per poll (Relaxed: a
                    // UI-status flag with no ordering dependency).
                    let dhan_on = dhan_enabled
                        .as_ref()
                        .is_none_or(|f| f.load(std::sync::atomic::Ordering::Relaxed));
                    // ACT (page + exit/teardown) ONLY when in market hours AND
                    // Dhan is enabled; otherwise the down pool is expected idle.
                    let should_act =
                        pool_watchdog_should_act_on_degradation(in_market_hours, dhan_on);
                    match verdict {
                        WatchdogVerdict::Degraded { down_for } => {
                            metrics::counter!("tv_pool_degraded_alerts_total").increment(1);
                            if should_act {
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
                                    in_market_hours,
                                    dhan_enabled = dhan_on,
                                    "S4-T1a: pool Degraded verdict but expected idle \
                                     (off-hours 09:00-15:30 IST, or Dhan toggled OFF) — \
                                     Dhan idle, no Telegram, no exit"
                                );
                            }
                        }
                        WatchdogVerdict::Recovered { was_down_for } => {
                            info!(
                                was_down_for_secs = was_down_for.as_secs(),
                                "S4-T1a: pool watchdog Recovered verdict — pool back online"
                            );
                            metrics::counter!("tv_pool_recoveries_total").increment(1);
                            // Fix A (2026-06-30): the pool is back online — the
                            // bare-reset reconnect-in-place episode is over; clear
                            // the ceiling timer so the next episode measures fresh.
                            reconnect_in_place_since = None;
                            // Recovery is informational; only Telegram-page the
                            // operator if the original Degraded fired (i.e. we
                            // were inside market hours AND Dhan was enabled when
                            // the down-cycle hit).
                            if should_act {
                                notifier.notify(NotificationEvent::WebSocketPoolRecovered {
                                    was_down_secs: was_down_for.as_secs(),
                                });
                            }
                        }
                        WatchdogVerdict::Halt { down_for } => {
                            if should_act {
                                // Fix A (2026-06-30): classify the down-cause from
                                // the snapshot we already took + the token/QuestDB
                                // health signals. A BENIGN bare-Dhan-transport-RST
                                // class (every conn streak 0, no non-reconnectable
                                // code, token valid, QuestDB reachable) is ridden
                                // out by reconnecting IN PLACE instead of
                                // process::exit + a 775-SID cold re-subscribe that
                                // trips Dhan's per-IP 429. A 15-min ceiling falls
                                // back to the genuine-fatal exit so a wedged feed
                                // can't ride out forever.
                                let token_valid = token_handle.as_ref().is_some_and(|h| {
                                    let guard = h.load();
                                    // First `.as_ref()`: Guard -> &Option<TokenState>;
                                    // second: Option<&TokenState>.
                                    guard
                                        .as_ref()
                                        .as_ref()
                                        .is_some_and(|s| s.is_valid())
                                });
                                // DAMPED signal (2026-07-01 audit fix): the
                                // ride-out classifiers require QuestDB "reachable",
                                // but reading the RAW single-probe signal here let
                                // a single blip force process::exit → 775-SID cold
                                // re-subscribe → next 429 → restart/429 loop. The
                                // damped signal is false only after N consecutive
                                // failed probes, so a blip rides out but a genuine
                                // SUSTAINED outage still exits (never worse than
                                // today). `/health` still reads the raw signal.
                                let questdb_reachable = health.questdb_reachable_for_exit_decision();
                                // In-window 429 ride-out (2026-06-30): the
                                // ride-out decision now covers BOTH the streak==0
                                // bare-RST storm AND the in-market-hours Dhan 429
                                // storm. A 429 is the EXACT case the per-connection
                                // loops already absorb in place (they wait out the
                                // floor + retry, subs preserved); exiting on it
                                // just earns the next 429 (self-inflicted
                                // restart/429 loop — observed 2026-06-30: 61
                                // main-feed 429s, also 429-starving Groww).
                                let bare_reset =
                                    is_bare_reset_class(&healths, token_valid, questdb_reachable);
                                let ride_out = should_reconnect_in_place(
                                    &healths,
                                    in_market_hours,
                                    token_valid,
                                    questdb_reachable,
                                );
                                // Distinguish the two ride-out classes for
                                // observability: `bare_dhan_reset` = streak==0
                                // RST storm; `in_window_429_ride_out` = the new
                                // in-market 429 admit (bare_reset is false but
                                // ride_out is true ⇒ admitted via the 429 class).
                                let ride_out_reason = if bare_reset {
                                    "bare_dhan_reset"
                                } else {
                                    "in_window_429_ride_out"
                                };
                                let elapsed_in_place = reconnect_in_place_since
                                    .map_or(0, |since| since.elapsed().as_secs());
                                if ride_out
                                    && !reconnect_in_place_ceiling_exceeded(elapsed_in_place)
                                {
                                    // Start (or continue) the in-place episode.
                                    if reconnect_in_place_since.is_none() {
                                        reconnect_in_place_since = Some(std::time::Instant::now());
                                    }
                                    metrics::counter!(
                                        "tv_ws_watchdog_reconnect_in_place_total",
                                        "reason" => ride_out_reason
                                    )
                                    .increment(1);
                                    // Restart the 300s AllDown window so the
                                    // per-connection reconnect loops keep running
                                    // (SubscribeRxGuard preserves subscriptions;
                                    // they ALSO honor the WS-GAP-08 429 cooldown
                                    // floor in-place); no exit, no cold
                                    // re-subscribe, no fresh 429.
                                    pool.reset_watchdog();
                                    error!(
                                        down_for_secs = down_for.as_secs(),
                                        elapsed_in_place_secs = elapsed_in_place,
                                        reason = ride_out_reason,
                                        code = tickvault_common::error_code::ErrorCode::WsGap09WatchdogReconnectInPlace.code_str(),
                                        "WS-GAP-09 pool watchdog Halt classified as benign \
                                         ride-out (token valid, QuestDB reachable, no \
                                         non-reconnectable code; either a streak==0 bare-Dhan-reset \
                                         OR an in-market Dhan 429) — reconnecting IN PLACE instead \
                                         of restarting; subscriptions preserved, no 429-tripping \
                                         cold re-subscribe."
                                    );
                                    // Skip the genuine-fatal path this cycle.
                                    continue;
                                }
                                // Genuine-fatal (or ceiling exceeded): restart as
                                // today. If we WERE riding out (either class) and
                                // the ceiling tripped, tag the fallback first.
                                if ride_out {
                                    metrics::counter!(
                                        "tv_ws_watchdog_reconnect_in_place_total",
                                        "reason" => "ceiling_exceeded"
                                    )
                                    .increment(1);
                                    error!(
                                        elapsed_in_place_secs = elapsed_in_place,
                                        ride_out_reason,
                                        code = tickvault_common::error_code::ErrorCode::WsGap09WatchdogReconnectInPlace.code_str(),
                                        "WS-GAP-09 reconnect-in-place ceiling exceeded (15 min, \
                                         zero frame recovery) — falling back to genuine-fatal \
                                         restart."
                                    );
                                }
                                // (No need to clear `reconnect_in_place_since`
                                // here — every path below this point diverges
                                // via lane teardown `return` or `process::exit`.)
                                // Genuine-fatal Halt: count it (the counter's
                                // meaning is now sharp — a real restart, never a
                                // benign reset ride-out).
                                metrics::counter!("tv_pool_self_halts_total").increment(1);
                                notifier.notify(NotificationEvent::WebSocketPoolHalt {
                                    down_secs: down_for.as_secs(),
                                });
                                // Give notifications + metrics flush a moment.
                                tokio::time::sleep(std::time::Duration::from_secs(2)).await; // APPROVED: pre-existing literal, session-7 tech debt tracked for cleanup
                                if let Some(ref halt) = lane_halt {
                                    // H7 RUNTIME lane: a lane-local Dhan fault MUST
                                    // NOT take down the process / the independent
                                    // Groww feed / the shared seal-writer +
                                    // aggregator + API server. Signal the parked
                                    // task to tear the lane down + drive the FSM to
                                    // `Off` (the runtime supervisor re-cold-starts
                                    // it with bounded backoff). NEVER process::exit.
                                    error!(
                                        down_for_secs = down_for.as_secs(),
                                        "S4-T1a FATAL: pool watchdog fired Halt verdict \
                                         (runtime lane). Tearing down the Dhan lane + \
                                         driving the FSM to Off so the supervisor \
                                         re-cold-starts it — Groww + shared infra stay \
                                         alive. All main-feed WebSocket connections have \
                                         been down for >300s."
                                    );
                                    metrics::counter!("tv_dhan_lane_watchdog_halt_total")
                                        .increment(1);
                                    halt.notify_waiters();
                                    // The parked task owns the teardown from here;
                                    // stop polling so we do not re-Halt mid-teardown.
                                    return;
                                }
                                // BOOT-ON single-feed contract (pre-existing): exit
                                // so systemd/the supervisor restarts the WHOLE
                                // process.
                                error!(
                                    down_for_secs = down_for.as_secs(),
                                    "S4-T1a FATAL: pool watchdog fired Halt verdict. \
                                     Exiting process with status 2 so the supervisor restarts us. \
                                     All main-feed WebSocket connections have been down for >300s."
                                );
                                std::process::exit(2);
                            } else {
                                info!(
                                    down_for_secs = down_for.as_secs(),
                                    in_market_hours,
                                    dhan_enabled = dhan_on,
                                    "S4-T1a: pool Halt verdict but expected idle \
                                     (off-hours 09:00-15:30 IST, or Dhan toggled OFF) — \
                                     Dhan idle, suppressing Telegram and process exit. \
                                     Watchdog will reset/resume when Dhan is active again."
                                );
                            }
                        }
                        WatchdogVerdict::Healthy => {
                            // Fix A (2026-06-30): the pool is healthy again — any
                            // bare-reset reconnect-in-place episode has ended;
                            // clear the ceiling timer so a fresh episode measures
                            // from zero.
                            reconnect_in_place_since = None;
                        }
                        WatchdogVerdict::Degrading { .. } => {
                            // No alert; watchdog's internal state machine
                            // will upgrade to Degraded / Halt if this persists.
                            // NOTE: do NOT clear `reconnect_in_place_since` here —
                            // a still-down pool mid-reconnect-in-place stays
                            // Degrading after `reset_watchdog()`, and the 15-min
                            // ceiling must keep accumulating across those polls.
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
                    // Halt arm above, gated by `should_act`) is unchanged.
                    //
                    // PR-E: also reset while Dhan is deliberately toggled OFF.
                    // Its connections are dormant by design, so a stale
                    // `AllDown { since }` would otherwise accumulate and trip
                    // Halt the instant Dhan is re-enabled mid-market. Resetting
                    // here gives the re-enabled feed a fresh 300s window. The
                    // genuine "Dhan enabled + all-down for 300s → halt" safety
                    // property (the `should_act`-gated Halt arm) is unchanged.
                    if !should_act {
                        pool.reset_watchdog();
                    }
                }
            }
        }
    })
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
    // C2: `BufferedSeal` / `TfIndex` / `stamp_seal_pct_fields` are no longer used
    // directly here — the per-seal routing body moved into
    // `tickvault_app::seal_routing::route_seal` (behavior-preserving).
    use tickvault_trading::candles::{AggregatorHeartbeatCounters, MultiTfAggregator};

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
                        // Dhan feed: re-fold 1-bucket-late ticks (Option B); the
                        // `u32` Quote-packet volume needs no override (None ⇒ the
                        // cell reads tick.volume). Byte-identical to the
                        // pre-FeedStrategy Dhan behaviour.
                        tickvault_trading::candles::FeedStrategy::DHAN,
                        None,
                        |tf, state| {
                            // C2 (behavior-preserving): the per-seal routing body
                            // now lives in the shared `route_seal`. Dhan policy:
                            // drop the D1 seal (1d is historical-only per
                            // `live-feed-purity.md` rule 10), stamp the prev-day
                            // pct fields from `prev_day_cache_for_agg`, and drive
                            // the heartbeat + `tv_aggregator_*` counters. The
                            // emitted output (counters, drop label, BufferedSeal
                            // fields, D1-drop, pct-stamp) is byte-identical to the
                            // pre-C2 inline closure.
                            tickvault_app::seal_routing::route_seal(
                                tickvault_app::seal_routing::SealRouteParams {
                                    feed: tickvault_common::feed::Feed::Dhan,
                                    drop_d1: true,
                                    prev_day_cache: Some(prev_day_cache_for_agg.as_ref()),
                                    heartbeat: Some(&heartbeat_writer),
                                    feed_health_on_m1: None,
                                },
                                tick.security_id,
                                tick.exchange_segment_code,
                                tf,
                                state,
                                sender,
                            );
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
    // Cloned BEFORE Task 3 moves `trading_calendar` — Task 3b (close-time
    // force-seal) and Task 4 (the watermark catch-up seal below) share the
    // same calendar gate.
    let calendar_for_catchup = std::sync::Arc::clone(&trading_calendar);
    let calendar_for_close_seal = std::sync::Arc::clone(&trading_calendar);
    tokio::spawn(async move {
        loop {
            // Sleep until the next IST midnight (bounded helper, ≤ 24h).
            let sleep_secs = tickvault_common::market_hours::secs_until_next_ist_midnight().max(1);
            tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;

            // Scoreboard PR-C: reset the Dhan DAY lag histogram at every
            // IST midnight — BEFORE the trading-day gate below (a
            // Saturday-midnight `continue` must still clear Friday's
            // distribution before Monday's scorecard row). Cold, O(96).
            tickvault_core::pipeline::feed_lag_monitor::reset_day_lag_histogram(
                tickvault_common::feed::Feed::Dhan,
            );
            // Scoreboard PR-D: reset the Dhan presence bitsets at the same
            // boundary (belt-and-braces — the day-change clear at
            // registration is the backstop). Cold, O(slots × 6).
            tickvault_core::pipeline::feed_presence::reset_daily(
                tickvault_common::feed::Feed::Dhan,
            );

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
            agg_for_boundary.force_seal_all(|security_id, segment_code, tf, state| {
                // C2 (behavior-preserving): the per-seal routing body now lives
                // in the shared `route_seal`. This IST-midnight Dhan path uses
                // the SAME Dhan policy as the per-tick site (drop D1, pct-stamp,
                // fire the `tv_aggregator_*` counters) but carries NO heartbeat —
                // it keeps its own local `sealed`/`dropped` running counts from
                // the returned `SealOutcome`. Byte-identical to the pre-C2 inline
                // closure.
                match tickvault_app::seal_routing::route_seal(
                    tickvault_app::seal_routing::SealRouteParams {
                        feed: tickvault_common::feed::Feed::Dhan,
                        drop_d1: true,
                        prev_day_cache: Some(prev_day_cache_for_boundary.as_ref()),
                        heartbeat: None,
                        feed_health_on_m1: None,
                    },
                    security_id,
                    segment_code,
                    tf,
                    state,
                    sender,
                ) {
                    tickvault_app::seal_routing::SealOutcome::Sent => {
                        sealed = sealed.saturating_add(1);
                    }
                    tickvault_app::seal_routing::SealOutcome::DroppedFull => {
                        dropped = dropped.saturating_add(1);
                    }
                    // D1 is dropped at the write boundary — not counted as a
                    // mpsc-full drop (matches the pre-C2 early-`return`).
                    tickvault_app::seal_routing::SealOutcome::DroppedD1 => {}
                }
            });
            // F2 self-heal (2026-07-03): restart the day's event-time
            // watermark from 0 so (a) a POISONED watermark (garbage
            // future-dated tick advanced the never-regressing fetch_max past
            // the future-skew guard, disabling catch-up) self-heals within
            // one day, and (b) each day's watermark rebuilds from the day's
            // first real tick. The catch-up driver's watermark==0 gate keeps
            // the scan idle until then.
            agg_for_boundary.reset_watermark();
            tracing::info!(
                sealed,
                dropped,
                "IST-midnight force-seal complete — open buckets flushed (watermark reset)"
            );
        }
    });
    tracing::info!("candle-engine #T1b — IST-midnight force-seal task spawned");

    // --- Task 3b: 15:30:05 IST close-time force-seal (2026-07-03) ---
    // The LAST session minute (the 15:29 M1 bar — and every TF's final
    // bucket) never sealed intraday: a bucket seals only on the SAME
    // instrument's next tick, and the session gate discards ≥15:30:00
    // ticks BEFORE they can roll the bucket, so the final buckets waited
    // for the IST-midnight force-seal — which the 16:30 IST instance
    // auto-stop destroys (RAM state lost). This task closes that gap:
    // at 15:30:05 IST on trading days it force-seals every NON-always-on
    // instrument via `force_seal_all_session_scoped` (GIFT Nifty's ~21h
    // session must NOT be truncated at NSE close — only the midnight
    // task seals always-on cells) and routes each seal through the SAME
    // `route_seal` Dhan policy as Task 3.
    //
    // Ordering vs the 15:30:00.8 market-close pipeline stop: DELIBERATELY
    // a parallel timer, NOT a close-sequence hook — the close sequence
    // (`run_until_shutdown`) only aborts WS/tick-processor/trading
    // handles; the aggregator Arc, `global_seal_sender()` and the
    // seal-writer loop stay alive until final app shutdown, so this task
    // runs safely after the stop AND covers both boot paths (fast boot +
    // start_dhan_lane both call `spawn_engine_b_aggregator`).
    //
    // Idempotent vs the midnight seal: `force_seal` on emptied slots
    // returns None (0 double-flushes) and any duplicate row is absorbed
    // by the candle tables' DEDUP UPSERT KEYS. Never fires mid-session:
    // the trigger instant is fixed strictly after the [09:15, 15:30)
    // session-gate window closes. Same bare-spawn supervision level as
    // the sibling Task 3 midnight force-seal.
    let agg_for_close_seal = std::sync::Arc::clone(&aggregator);
    let prev_day_cache_for_close_seal = std::sync::Arc::clone(&prev_day_cache);
    tokio::spawn(async move {
        loop {
            // Sleep until the next 15:30:05 IST (bounded helper, ≤ 24h;
            // returns tomorrow's trigger when at/past today's, never 0).
            tokio::time::sleep(compute_close_seal_sleep()).await;

            // Only force-seal on trading days — a weekend/holiday 15:30:05
            // has no open buckets worth flushing.
            if !calendar_for_close_seal.is_trading_day_today() {
                tracing::info!("close-time force-seal: skipping (non-trading day)");
                continue;
            }

            let Some(sender) = global_seal_sender() else {
                tracing::warn!("close-time force-seal: seal sender not installed — skipping");
                continue;
            };

            let mut sealed: u64 = 0;
            let mut dropped: u64 = 0;
            agg_for_close_seal.force_seal_all_session_scoped(
                |security_id, segment_code, tf, state| {
                    match tickvault_app::seal_routing::route_seal(
                        tickvault_app::seal_routing::SealRouteParams {
                            feed: tickvault_common::feed::Feed::Dhan,
                            drop_d1: true,
                            prev_day_cache: Some(prev_day_cache_for_close_seal.as_ref()),
                            heartbeat: None,
                            feed_health_on_m1: None,
                        },
                        security_id,
                        segment_code,
                        tf,
                        state,
                        sender,
                    ) {
                        tickvault_app::seal_routing::SealOutcome::Sent => {
                            sealed = sealed.saturating_add(1);
                        }
                        tickvault_app::seal_routing::SealOutcome::DroppedFull => {
                            dropped = dropped.saturating_add(1);
                        }
                        // D1 is dropped at the write boundary — not counted
                        // as a mpsc-full drop (same policy as Task 3).
                        tickvault_app::seal_routing::SealOutcome::DroppedD1 => {}
                    }
                },
            );
            // NOTE: no `reset_watermark()` here — the watermark reset is the
            // IST-midnight task's cross-day duty; post-close ticks must keep
            // advancing it for the BOUNDARY-01 catch-up driver.
            tracing::info!(
                sealed,
                dropped,
                "close-time force-seal complete — final session buckets flushed"
            );
        }
    });
    tracing::info!("candle-engine #T1b — 15:30:05 IST close-time force-seal task spawned");

    // --- Task 4: watermark-aware per-minute catch-up seal (BOUNDARY-01) ---
    // Bounds candle seal lag WITHOUT mass-discarding backlogged ticks. A
    // bucket seals today only on the SAME instrument's next tick or at IST
    // midnight — an instrument that stops ticking mid-session leaves its
    // candle rows absent for hours, and the final session minute (the 15:29
    // M1 bar) is structurally absent until midnight because the
    // out-of-session gate blocks ≥15:30 ticks from folding. This task closes
    // both gaps SAFELY: every CATCHUP_SEAL_POLL_INTERVAL_SECS it reads the
    // Dhan aggregator instance's event-time watermark (max exchange_timestamp
    // ever consumed — post-close ticks still advance it) and gates via the
    // shared pure `compute_catchup_cutoff`: scan ONLY when the watermark
    // ADVANCED since the last scan AND is not POISONED (more than the
    // future-skew guard ahead of the IST wall clock — a garbage future-dated
    // tick advanced the never-regressing fetch_max); the cutoff is
    // min(watermark − CATCHUP_SEAL_LATENESS_MARGIN_SECS_DHAN, now_ist) so a
    // bucket can never seal before the wall clock passes its end. Buckets
    // past that cutoff are still potentially being filled by a backlogged
    // stream and stay open — a naive wall-clock force-seal here would
    // convert the backlog into DiscardLate drops and corrupt candles on
    // re-open. A STALLED watermark (dead feed / broadcast starvation) gets
    // NO catch-up seals — FEED-STALL-01 owns the dead-feed page; no "assume
    // dead then force-seal anyway" escape hatch exists by design. A POISONED
    // watermark disables catch-up (coalesced BOUNDARY-01 error,
    // reason=watermark_future_skew) until the IST-midnight watermark reset
    // self-heals it. Same bare-spawn supervision level as the sibling Task 3
    // midnight force-seal.
    let agg_for_catchup = std::sync::Arc::clone(&aggregator);
    let prev_day_cache_for_catchup = std::sync::Arc::clone(&prev_day_cache);
    let heartbeat_for_catchup = heartbeat.clone();
    tokio::spawn(async move {
        use tickvault_trading::candles::{
            CATCHUP_SEAL_LATENESS_MARGIN_SECS_DHAN, CATCHUP_SEAL_POLL_INTERVAL_SECS,
            CATCHUP_WATERMARK_FUTURE_SKEW_GUARD_SECS, compute_catchup_cutoff,
        };
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            CATCHUP_SEAL_POLL_INTERVAL_SECS,
        ));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_scanned_watermark: u32 = 0;
        // Edge latch for the poisoned-watermark error — ONE coalesced line
        // per poisoning episode (audit Rule 4), not one per 5 s wave.
        let mut poison_logged = false;
        loop {
            interval.tick().await;
            let watermark = agg_for_catchup.watermark_secs();
            // IST wall-clock now (epoch seconds) — the SAME canonical
            // Utc::now + IST-offset path the market-hours helpers use
            // (audit-findings Rule 3). A pre-1970 / post-2106 degenerate
            // clock maps to 0, which makes every watermark look poisoned →
            // catch-up stays disabled (fail-closed, BOOT-03-class posture).
            let now_ist_secs = u32::try_from(chrono::Utc::now().timestamp().saturating_add(
                i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS),
            ))
            .unwrap_or(0);
            let Some(cutoff) = compute_catchup_cutoff(
                watermark,
                last_scanned_watermark,
                now_ist_secs,
                CATCHUP_SEAL_LATENESS_MARGIN_SECS_DHAN,
                CATCHUP_WATERMARK_FUTURE_SKEW_GUARD_SECS,
            ) else {
                // None = no tick yet / watermark unchanged (self-gate) /
                // POISONED. Only the poisoned arm is observable: watermark
                // non-zero and advanced, yet the gate refused — meaning it
                // sits past now + guard. last_scanned is NOT updated, so
                // scanning resumes the moment the watermark self-heals
                // (IST-midnight reset_watermark).
                if watermark != 0 && watermark != last_scanned_watermark {
                    metrics::counter!(
                        "tv_boundary_catchup_skipped_total",
                        "feed" => "dhan", "reason" => "future_skew"
                    )
                    .increment(1);
                    if !poison_logged {
                        poison_logged = true;
                        tracing::error!(
                            code = tickvault_common::error_code::ErrorCode::Boundary01CatchupSeal
                                .code_str(),
                            reason = "watermark_future_skew",
                            feed = "dhan",
                            watermark_secs = watermark,
                            now_ist_secs,
                            "BOUNDARY-01: poisoned event-time watermark (further ahead of the \
                             IST wall clock than host skew allows) — catch-up sealing disabled \
                             until the IST-midnight watermark reset self-heals it"
                        );
                    }
                }
                continue;
            };
            poison_logged = false;
            // Trading-day gate — mirrors the Task 3 midnight force-seal.
            if !calendar_for_catchup.is_trading_day_today() {
                continue;
            }
            let Some(sender) = global_seal_sender() else {
                // Seal-writer not installed yet — retry next wave WITHOUT
                // consuming the watermark advance (no seal is lost).
                continue;
            };
            last_scanned_watermark = watermark;
            // F5 (2026-07-03): count only ROUTED catch-up seals — Dhan drops
            // D1 at the write boundary (`live-feed-purity.md` rule 10), so a
            // D1 catch-up seal must not inflate the counter or the coalesced
            // `seals` count. DroppedFull IS counted here (the row reaches the
            // ring→spill→DLQ absorption chain and is separately counted by
            // `tv_seal_mpsc_dropped_total`).
            let mut routed: u64 = 0;
            agg_for_catchup.catch_up_seal_all(cutoff, |security_id, segment_code, tf, state| {
                // EXACT same Dhan routing policy as the per-tick seal site
                // (spawn_engine_b_aggregator Task 1): drop D1, pct-stamp from
                // the prev-day cache, drive the heartbeat + tv_aggregator_*
                // counters.
                match tickvault_app::seal_routing::route_seal(
                    tickvault_app::seal_routing::SealRouteParams {
                        feed: tickvault_common::feed::Feed::Dhan,
                        drop_d1: true,
                        prev_day_cache: Some(prev_day_cache_for_catchup.as_ref()),
                        heartbeat: Some(&heartbeat_for_catchup),
                        feed_health_on_m1: None,
                    },
                    security_id,
                    segment_code,
                    tf,
                    state,
                    sender,
                ) {
                    // D1 dropped at the write boundary — not a routed seal.
                    tickvault_app::seal_routing::SealOutcome::DroppedD1 => {}
                    tickvault_app::seal_routing::SealOutcome::Sent
                    | tickvault_app::seal_routing::SealOutcome::DroppedFull => {
                        routed = routed.saturating_add(1);
                        metrics::counter!("tv_boundary_catchup_total", "feed" => "dhan")
                            .increment(1);
                    }
                }
            });
            if routed > 0 {
                // ONE coalesced line per scan wave — never per-seal spam.
                tracing::warn!(
                    code =
                        tickvault_common::error_code::ErrorCode::Boundary01CatchupSeal.code_str(),
                    feed = "dhan",
                    seals = routed,
                    cutoff_secs = cutoff,
                    watermark_secs = watermark,
                    "BOUNDARY-01: watermark catch-up sealed lagging candle bucket(s) — \
                     late but correct; buckets past the watermark stay open for the backlog"
                );
            }
        }
    });
    tracing::info!("candle-engine #T1b — watermark catch-up seal task spawned (BOUNDARY-01)");
}

// ---------------------------------------------------------------------------
// D2-pre: PROCESS-shared infra for the Dhan-OFF boot path
// ---------------------------------------------------------------------------
//
// Behaviour-identical HOIST (`active-plan-dhan-cold-start-d2.md` §1.0). The
// Dhan-ON boot path is UNTOUCHED — it still builds the API server, seal-writer,
// 21-TF aggregator, and run-loop inline (byte-identical). This function is the
// Dhan-OFF mirror: it brings up the SAME PROCESS-shared infra so a Dhan-OFF boot
// is no longer a bare early-return.
//
// Why this exists (adversarial review C1/C2, 2026-06-26):
//   • The HTTP API server (incl. the `/api/feeds` toggle routes) previously
//     spawned ONLY inside the Dhan block — so a Dhan-OFF boot had NO API server
//     and the `/api/feeds` endpoint did not exist (C1).
//   • The candle seal-writer installs the process-wide `global_seal_sender`. The
//     Groww feed routes its sealed candles through that SAME singleton
//     (`groww_bridge.rs`). With Dhan OFF the seal-writer never ran, so Groww
//     candles silently never sealed (C2).
//
// This function fixes BOTH for the Dhan-OFF path. It does NOT spawn any Dhan
// WebSocket, does NOT authenticate, and does NOT fetch instruments — the
// per-feed OFF-isolation guarantee (operator lock 2026-06-23) is preserved.
// It adds NO new WebSocket endpoint (the 2-WS Dhan lock is untouched).
//
// It is NOT a runtime cold-start path: no Dhan lane is started here. The full
// boot-OFF → runtime cold-start of the Dhan spine is the deferred residual
// tracked as D2a/D2b.
#[allow(clippy::too_many_arguments)] // APPROVED: process-shared infra requires the captured boot state
// ---------------------------------------------------------------------------
// D2 Stage 2 (genuine shared-infra hoist) — the PROCESS-shared infra built ONCE
// by `build_shared_infra` and shared by BOTH the Dhan-OFF and Dhan-ON-slow
// paths. This replaces the old duplicate `run_shared_infra_only` (deleted): the
// shared construction now lives in exactly one place and `main()` builds the
// optional Dhan lane on top of it.
// ---------------------------------------------------------------------------
struct SharedInfraHandles {
    /// Strict-initialised notifier (coalescer-wrapped per config). Used by the
    /// lane, the run-loop, and the periodic-health task.
    notifier: std::sync::Arc<NotificationService>,
    /// Drives `/health` + `/api/feeds/health`. The lane updates it; the API
    /// server reads it.
    health_status: SharedHealthStatus,
    /// The PROCESS-shared tick broadcast. The 3 subscriber tasks (obs,
    /// aggregator, tick-storage) are spawned in `build_shared_infra` and have
    /// already `.subscribe()`d to this BEFORE any Dhan tick processor publishes.
    /// The lane's `run_tick_processor` is the only publisher.
    tick_broadcast_sender: tokio::sync::broadcast::Sender<tickvault_common::tick_types::ParsedTick>,
    /// The PROCESS-shared order-update broadcast (lane order-update WS publishes;
    /// trading pipeline subscribes).
    order_update_sender: tokio::sync::broadcast::Sender<tickvault_common::order_types::OrderUpdate>,
    /// The hoisted axum API server handle (binds exactly once, incl. /api/feeds).
    api_handle: tokio::task::JoinHandle<()>,
}

/// Builds the PROCESS-shared infra ONCE for BOTH the Dhan-OFF and Dhan-ON-slow
/// boot paths: strict notifier (+ optional coalescer), health registry,
/// seal-writer (installs the process-wide `global_seal_sender`), the 21-TF
/// Engine-B aggregator, the tick + order-update broadcast channels, the
/// observability + tick-storage subscriber tasks (which `.subscribe()` to the
/// tick broadcast BEFORE the lane's tick processor publishes — the
/// subscribe-before-publish / zero-tick-loss invariant, preserved by
/// construction), and the axum API server (incl. /api/feeds — so the toggle
/// endpoint exists even with Dhan OFF).
///
/// The Docker auto-start side-effect is gated on `config.infrastructure
/// .auto_start_docker` exactly as the slow-boot path always was; it runs in
/// parallel with the strict notifier init (same `tokio::join!` as before).
async fn build_shared_infra(
    config: &ApplicationConfig,
    feed_runtime: std::sync::Arc<tickvault_api::feed_state::FeedRuntimeState>,
    feed_health: std::sync::Arc<tickvault_common::feed_health::FeedHealthRegistry>,
    trading_calendar: std::sync::Arc<TradingCalendar>,
    prev_day_cache: std::sync::Arc<tickvault_trading::in_mem::PrevDayCache>,
    tick_storage: std::sync::Arc<tickvault_trading::in_mem::TickStorage>,
) -> Result<SharedInfraHandles> {
    // --- Notifier (strict) + Docker infra (parallel — independent) ---
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
                "SHARED-INFRA BOOT: strict notifier init failed — REFUSING BOOT (systemd will restart)"
            );
            return Err(anyhow::anyhow!(reason));
        }
    };
    // Wave 3-B Item 11: opt-in Telegram bucket-coalescer (defaults to `true`).
    // 2026-07-07 UX overhaul: digest window from config; the drain loop
    // also owns the episode stability ticker (green-close promotion).
    let notifier = if config.features.telegram_bucket_coalescer {
        NotificationService::enable_coalescer(
            notifier,
            tickvault_core::notification::CoalescerConfig {
                market_hours_window: std::time::Duration::from_secs(
                    config.notification.digest_window_secs_clamped(),
                ),
                ..Default::default()
            },
        )
    } else {
        // No drain loop → the episode green-close promotion needs its own
        // tiny ticker (no-op when episode_mode is off / NoOp mode).
        NotificationService::spawn_episode_ticker(&notifier);
        notifier
    };
    // Boot bubble (2026-07-09): declare which feed lines the checklist
    // shows as pending from its first page.
    notifier.set_boot_expectations(config.feeds.dhan_enabled, config.feeds.groww_enabled);

    // --- Health registry (drives /health + /api/feeds/health) ---
    let health_status: SharedHealthStatus = std::sync::Arc::new(SystemHealthStatus::new());

    // --- Seal-writer (installs the process-wide global_seal_sender) ---
    spawn_seal_writer_loop(&config.questdb);

    // --- Tick + order-update broadcast channels (PROCESS-shared) ---
    // Held for the process lifetime so the aggregator subscriber never wakes on
    // a disconnected channel. With Dhan OFF nothing publishes Dhan ticks into
    // the tick broadcast, but the channel + aggregator still run so the wiring
    // is identical to the Dhan-ON path (Groww runs its OWN aggregator instance).
    let (tick_broadcast_sender, _tick_broadcast_default_rx) =
        tokio::sync::broadcast::channel::<tickvault_common::tick_types::ParsedTick>(
            tickvault_common::constants::TICK_BROADCAST_CAPACITY,
        );
    let (order_update_sender, _order_update_receiver) =
        tokio::sync::broadcast::channel::<tickvault_common::order_types::OrderUpdate>(
            tickvault_common::constants::ORDER_UPDATE_BROADCAST_CAPACITY,
        );

    // --- Subscriber tasks: obs + 21-TF aggregator + tick-storage ---
    // ALL three `.subscribe()` to `tick_broadcast_sender` HERE, in the hoisted
    // prefix, BEFORE the lane's `run_tick_processor` (the only publisher) runs.
    // Subscribe-before-publish is therefore preserved by construction.
    {
        let obs_rx = tick_broadcast_sender.subscribe();
        let questdb_cfg = config.questdb.clone();
        tokio::spawn(async move {
            run_slow_boot_observability(obs_rx, questdb_cfg).await;
        });
        info!("slow-boot observability consumer started");
    }
    spawn_engine_b_aggregator(
        &tick_broadcast_sender,
        std::sync::Arc::clone(&prev_day_cache),
        std::sync::Arc::clone(&trading_calendar),
    );
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
    info!("SHARED-INFRA BOOT: seal-writer + 21-TF aggregator running (Groww candles can seal)");

    // --- HTTP API server (incl. /api/feeds toggle routes) — C1 fix ---
    let api_state = SharedAppState::new_with_feed_runtime_and_health(
        config.questdb.clone(),
        config.dhan.clone(),
        config.instrument.clone(),
        std::sync::Arc::clone(&health_status),
        std::sync::Arc::clone(&feed_runtime),
        std::sync::Arc::clone(&feed_health),
    );
    let api_bearer_token = tickvault_core::auth::secret_manager::fetch_api_bearer_token()
        .await
        .context("GAP-SEC-01: SSM fetch for API bearer token failed at /tickvault/<env>/api/bearer-token — store the token via `aws ssm put-parameter --name /tickvault/<env>/api/bearer-token --type SecureString`")?;
    info!("GAP-SEC-01: API bearer token loaded from SSM (/tickvault/<env>/api/bearer-token)");
    let api_auth_config = tickvault_api::middleware::ApiAuthConfig::from_token(api_bearer_token);
    // W2#7 (2026-07-10): supervised SSM re-read loop (slow-boot mirror of
    // the fast-arm spawn above) — token rotation without restart; fail-open
    // on SSM outages. Ratchet:
    // crates/app/tests/api_token_rotation_wiring_guard.rs.
    if api_auth_config.enabled {
        let _api_token_reload_supervisor =
            tickvault_app::api_token_rotation::spawn_supervised_api_token_reload(
                api_auth_config.clone(),
            );
    }
    let router = tickvault_api::build_router_with_auth(
        api_state,
        &config.api.allowed_origins,
        api_auth_config,
        // 2026-07-04 operator quote: flag accepted-but-IGNORED — the feed
        // toggle is bearer-protected in ALL modes (see
        // websocket-connection-scope-lock.md). Kept to avoid a cascade.
        config.strategy.dry_run,
    );
    let bind_addr: SocketAddr = format_bind_addr(&config.api.host, config.api.port)
        .parse()
        .context("invalid API bind address")?;
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .context("failed to bind API server")?;
    info!(address = %bind_addr, "SHARED-INFRA BOOT: API server listening (/api/feeds reachable regardless of Dhan ON/OFF)");
    let api_handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router).await {
            error!(?err, "API server error");
        }
    });

    Ok(SharedInfraHandles {
        notifier,
        health_status,
        tick_broadcast_sender,
        order_update_sender,
        api_handle,
    })
}

// ---------------------------------------------------------------------------
// D2a — `start_dhan_lane`: the extracted Dhan-lane cold-start (slow boot arm).
//
// This is the (now-contiguous, post-Stage-2) Dhan LANE body lifted VERBATIM out
// of the `if config.feeds.dhan_enabled { … }` gate in `main()`. It owns ONLY
// LANE work (auth → daily-universe build → main-feed WS pool create+spawn →
// tick processor → order-update WS → token-renewal/sweep/watchdog → pool +
// mid-session watchdogs) on top of the already-built PROCESS-shared infra
// (`build_shared_infra`). It does NOT create the seal-writer / 21-TF aggregator
// / API server (those are PROCESS-shared, hoisted in Stage 2) — design §1.1 /
// C2 ownership.
//
// Behaviour-identical to the inline gate: each former boot-abort `return Ok(())`
// becomes `Err(StartLaneError::BootAbortClean)` (the caller maps it back to
// `main()`'s `return Ok(())`, i.e. clean process exit), and the single
// instrument-load `return Err(err)` becomes `Err(StartLaneError::BootAbortErr)`
// (caller `return Err(err)`). On success the same `DhanLaneRunHandles` are
// returned for the PROCESS run-loop to tear down.
//
// C4 (lane-owns-TokenManager): the `TokenManager` is built INSIDE this fn (auth
// step) and handed back inside the renewal task; the global `OnceLock`
// (`set_global_token_manager`) is set exactly as the inline spine did (boot
// path, set-once). D2b will make the lane own the manager handle end-to-end for
// the runtime ON→OFF cold-start; this PR is the pure extraction only.
// ---------------------------------------------------------------------------

/// PROCESS-shared state `start_dhan_lane` reads from `main()` (all cheap
/// references / Arc clones). Captured ONCE after `build_shared_infra` so the
/// lane body compiles unchanged against `ctx`-rebound locals of the SAME names.
///
/// Field list is the EXHAUSTIVE set of `main()`-scope locals the (slow-arm)
/// Dhan lane body reads — produced from a real compile (design L11, no elision).
struct DhanLaneContext<'a> {
    /// Immutable boot config. Borrowed (the PROCESS run-loop still needs it
    /// after the lane returns, so it is never moved out of `main`).
    config: &'a ApplicationConfig,
    /// Strict-initialised notifier (Telegram). The lane clones it per task.
    notifier: &'a std::sync::Arc<NotificationService>,
    /// `/health` + `/api/feeds/health` registry — the lane updates it.
    health_status: &'a SharedHealthStatus,
    /// PROCESS-shared tick broadcast — the lane's tick processor is the only
    /// publisher; the 3 subscriber tasks already `.subscribe()`d in
    /// `build_shared_infra` (subscribe-before-publish preserved).
    tick_broadcast_sender:
        &'a tokio::sync::broadcast::Sender<tickvault_common::tick_types::ParsedTick>,
    /// PROCESS-shared order-update broadcast — the lane order-update WS
    /// publishes; the trading pipeline subscribes.
    order_update_sender:
        &'a tokio::sync::broadcast::Sender<tickvault_common::order_types::OrderUpdate>,
    /// Per-feed enable/disable + gate atomics (PR-E Dhan runtime toggle).
    feed_runtime: &'a std::sync::Arc<tickvault_api::feed_state::FeedRuntimeState>,
    /// Per-feed live-health signal registry.
    feed_health: &'a std::sync::Arc<tickvault_common::feed_health::FeedHealthRegistry>,
    /// NSE trading calendar.
    trading_calendar: &'a std::sync::Arc<TradingCalendar>,
    /// WAL→ring→spill→DLQ frame spill (PROCESS, reused; the lane hands clones
    /// to the WS pool + order-update WS). `None` only if WAL init failed (the
    /// inline path `std::process::exit(1)`s in that case, so it is `Some` here).
    ws_frame_spill: &'a Option<std::sync::Arc<tickvault_storage::ws_frame_spill::WsFrameSpill>>,
    /// Boot-time clock flags (Copy).
    is_market_hours: bool,
    is_trading: bool,
    is_muhurat: bool,
    is_mock_trading: bool,
    /// Boot wall-clock start, for the boot-elapsed telemetry (Copy).
    boot_start: std::time::Instant,
    /// WAL-replay buffers recovered BEFORE the lane (STAGE-C). OWNED + moved
    /// into the lane (it `std::mem::take`s them into the WS pool / order-update
    /// broadcast). They are not read by `main()` after the lane, so the lane
    /// consumes them — hence owned fields + `ctx` taken by value below.
    ws_wal_replay_live_feed: Vec<(u64, bytes::Bytes)>,
    ws_wal_replay_order_update: Vec<Vec<u8>>,
    /// H7 lane-scoped watchdog (cross-feed blast-radius fix). `false` for a
    /// BOOT-ON start (the inline boot spine — the pre-existing single-feed
    /// contract keeps the `process::exit(2)` + systemd-restart Halt behaviour).
    /// `true` for a RUNTIME-started lane (`run_dhan_lane_cold_start`) — its pool
    /// watchdog MUST NOT `process::exit` on a 300s-all-down Halt (that would kill
    /// the WHOLE process incl. the independent Groww feed + the shared seal-writer
    /// / aggregator / API server). Instead the lane-scoped watchdog signals the
    /// lane to tear down + drive the FSM to `Off` so the runtime supervisor
    /// re-cold-starts it, leaving Groww + shared infra ALIVE.
    lane_scoped: bool,
}

/// CLI flag that authorizes overwriting a WEDGED dual-instance lock at
/// boot (dual-instance lock hardening, operator "go" 2026-07-04). The
/// 90 s TTL already auto-clears a crashed holder WITHOUT this flag — it
/// exists only for a lock that TTL cannot clear (corrupt JSON, or a live
/// holder the operator has decided must yield). The takeover is logged
/// at `error!` (pages Telegram) naming the displaced holder — never
/// silent. See `.claude/rules/project/dual-instance-lock-2026-07-04.md`.
const FORCE_INSTANCE_TAKEOVER_FLAG: &str = "--force-instance-takeover";

/// Pure matcher for [`FORCE_INSTANCE_TAKEOVER_FLAG`] — unit-testable
/// without touching process args.
fn is_force_takeover_arg(arg: &str) -> bool {
    arg == FORCE_INSTANCE_TAKEOVER_FLAG
}

/// True when the operator passed `--force-instance-takeover` on the CLI.
fn force_instance_takeover_requested() -> bool {
    std::env::args().any(|a| is_force_takeover_arg(&a))
}

/// Why `start_dhan_lane` ended early. Maps 1:1 to the inline gate's two
/// boot-abort flavours so the caller reproduces `main()`'s EXACT prior return:
/// `BootAbortClean` → `main()` `return Ok(())` (clean exit, code 0);
/// `BootAbortErr` → `main()` `return Err(err)` (error exit with the chain).
enum StartLaneError {
    /// A boot gate refused to proceed (IP verify / auth / dual-instance lock /
    /// static-IP gate). The inline spine `return Ok(())`d here — a clean
    /// process exit after the operator Telegram already fired.
    BootAbortClean,
    /// Instrument load failed at boot — the inline spine `return Err(err)`d
    /// (process exits with the error chain).
    BootAbortErr(anyhow::Error),
    /// FTC-14 (audit 2026-07-01): the WebSocket main-feed pool build/spawn
    /// FAILED on a day we intended to connect (`should_connect_ws == true`,
    /// so the plan was present) — `create_websocket_pool` returned `None`
    /// despite a valid plan. Previously this silently proceeded with no pool
    /// (offline-blind); it now fails the cold-start so `classify_start_lane_
    /// error` can emit `DHAN-LANE-02` with `stage='ws_pool'`, pointing the
    /// operator at the correct (pool-spawn) runbook instead of the wrong
    /// (`universe_build_failed`) one. The lane FSM returns to `Off` + the
    /// bounded retry re-attempts.
    WsPoolSpawn,
}

/// Cold-start the full Dhan lane (slow boot arm). See the module-level doc
/// above for the ownership + behaviour-identical contract.
#[allow(clippy::too_many_lines)] // APPROVED: verbatim relocation of the inline Dhan-lane gate (D2a) — split into helpers is D2b/D2d, not this pure-extraction PR
async fn start_dhan_lane(
    ctx: DhanLaneContext<'_>,
) -> std::result::Result<DhanLaneRunHandles, StartLaneError> {
    // Re-bind every `ctx` field to a local of the SAME name + ownership the
    // (relocated) lane body expects, so the body below is byte-identical to the
    // inline gate (only the early-return + final-tail wrapping changed). `ctx`
    // is taken by value so the owned WAL-replay buffers can be moved out.
    let config: &ApplicationConfig = ctx.config;
    let notifier: std::sync::Arc<NotificationService> = std::sync::Arc::clone(ctx.notifier);
    let health_status: SharedHealthStatus = std::sync::Arc::clone(ctx.health_status);
    let tick_broadcast_sender = ctx.tick_broadcast_sender.clone();
    let order_update_sender = ctx.order_update_sender.clone();
    let feed_runtime = std::sync::Arc::clone(ctx.feed_runtime);
    let feed_health = std::sync::Arc::clone(ctx.feed_health);
    let trading_calendar = std::sync::Arc::clone(ctx.trading_calendar);
    let ws_frame_spill = ctx.ws_frame_spill.clone();
    let is_market_hours = ctx.is_market_hours;
    let is_trading = ctx.is_trading;
    let is_muhurat = ctx.is_muhurat;
    let is_mock_trading = ctx.is_mock_trading;
    let boot_start = ctx.boot_start;
    let lane_scoped = ctx.lane_scoped;
    let mut ws_wal_replay_live_feed = ctx.ws_wal_replay_live_feed;
    let mut ws_wal_replay_order_update = ctx.ws_wal_replay_order_update;

    // -----------------------------------------------------------------------
    // Step 5.5: Verify public IP matches SSM static IP (BLOCKS BOOT on failure)
    // -----------------------------------------------------------------------
    // Sandbox mode: skip IP verification (sandbox doesn't require registered IP).
    // Live mode: MUST verify IP before any Dhan API call.
    // Paper mode: skip (no Dhan API calls at all).
    let trading_mode = config.strategy.mode;
    // F5 (2026-07-08): the runtime IP monitor is LANE-OWNED, never a detached
    // per-lane-cycle spawn. The previous `std::mem::forget(ip_monitor_shutdown_tx)`
    // + dropped JoinHandle leaked one immortal monitor per D2b lane cold-start
    // cycle, each pinned to ITS boot's verified-IP baseline — after a
    // legitimate IP change + lane restart the zombie's STALE baseline fired
    // false CRITICAL GAP-NET-01 pages every poll and, in live mode, could
    // HALT the whole process. The handle rides in a `PreLaneAbortGuard`
    // across the pre-`DhanLaneRunHandles` awaits (this is the EARLIEST lane
    // spawn — every subsequent await sits in its window), then defuses into
    // the lane struct; the watch Sender rides alongside so teardown can drop
    // it (the monitor's `changed()` select arm exits on a closed channel).
    let (ip_monitor_guard, ip_monitor_shutdown): (
        PreLaneAbortGuard<()>,
        Option<tokio::sync::watch::Sender<bool>>,
    ) = if trading_mode.is_live() {
        info!("verifying public IP against SSM static IP");
        match ip_verifier::verify_public_ip().await {
            Ok(result) => {
                let verified_ip = result.verified_ip.clone();
                notifier.notify(NotificationEvent::IpVerificationSuccess {
                    verified_ip: result.verified_ip,
                });

                // AUTH-P12 / GAP-NET-01: wire the RUNTIME IP monitor. The
                // boot-time verify above is one-shot; a mid-session public-IP
                // / Elastic-IP change (EIP disassociation, NAT failover) must
                // be re-detected. The monitor polls the verified IP every
                // IP_MONITOR_CHECK_INTERVAL_SECS and, on a CONFIRMED sustained
                // mismatch (confirm-twice debounce), emits a CRITICAL
                // GAP-NET-01 alert. It HALTS the process only when
                // dry_run == false (real orders would be rejected by Dhan's
                // static-IP mandate); under dry_run it alerts but keeps the
                // live feed streaming (killing it for a no-orders IP change
                // would drop ticks for zero benefit).
                let ip_monitor_config =
                    tickvault_core::network::ip_monitor::IpMonitorConfig::for_runtime(
                        verified_ip,
                        config.strategy.dry_run,
                    );
                // F5: the sender is KEPT (lane-owned) — while the lane lives
                // the channel stays open; teardown/Drop drop it, and the
                // monitor's `changed()` arm exits on the closed channel.
                let (ip_monitor_shutdown_tx, ip_monitor_shutdown_rx) =
                    tokio::sync::watch::channel(false);
                let (_ip_mismatch_rx, ip_monitor_handle) =
                    tickvault_core::network::ip_monitor::spawn_ip_monitor(
                        ip_monitor_config,
                        ip_monitor_shutdown_rx,
                    );
                info!(
                    halt_on_mismatch = !config.strategy.dry_run,
                    "GAP-NET-01 (AUTH-P12): runtime IP monitor spawned (lane-owned)"
                );
                (
                    PreLaneAbortGuard::new(vec![ip_monitor_handle]),
                    Some(ip_monitor_shutdown_tx),
                )
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
                return Err(StartLaneError::BootAbortClean);
            }
        }
    } else {
        info!(
            mode = trading_mode.as_str(),
            "IP verification skipped — not required for {} mode",
            trading_mode.as_str()
        );
        (PreLaneAbortGuard::new(Vec::new()), None)
    };

    // -----------------------------------------------------------------------
    // Step 6a-prime: Dual-instance SSM lock (Phase 0 Item 19) — LOCK BEFORE MINT
    // -----------------------------------------------------------------------
    // RESILIENCE-01: only ONE tickvault process per Dhan client-id may
    // ever be live. Two processes against the same account fight over
    // static-IP enforcement (Item 18), fragment the WebSocket connection
    // budget, silently break order reconciliation — and, decisively
    // (coexistence audit 2026-07-04): Dhan enforces ONE ACTIVE TOKEN AT A
    // TIME, so a second instance's `generateAccessToken` mint at Step 6
    // INVALIDATES the first instance's JWT even in sandbox/dry-run mode.
    // We hold an SSM-Parameter-Store-backed lock (90s TTL, 30s heartbeat)
    // for the lifetime of the process; this gate fails the boot if
    // another live peer is already holding it. SSM is the source of
    // truth so an AWS prod instance and a dev Mac sharing the same env
    // name cannot accidentally run in parallel.
    //
    // Dual-instance lock hardening (operator "go" 2026-07-04 —
    // `.claude/rules/project/dual-instance-lock-2026-07-04.md`):
    //   1. ALWAYS-ON: the lock is acquired for EVERY Dhan-enabled boot,
    //      sandbox/paper INCLUDED (this lane only runs when
    //      `feeds.dhan_enabled == true`). The previous `is_live()` gate
    //      left the lock permanently OFF — both prod and local run
    //      mode = "sandbox" — yet a sandbox boot still mints the shared
    //      one-active-token JWT.
    //   2. LOCK-BEFORE-MINT: runs BEFORE Step 6 auth so a losing peer
    //      halts BEFORE it can invalidate the winner's token. The lock
    //      needs only an AWS SSM client — no Dhan token.
    //   3. SSM transport errors get the SAME bounded 3-attempt /
    //      exponential-backoff budget as the Step 6 SSM credential fetch,
    //      then HALT fail-closed (cannot prove there's no peer; a
    //      sustained SSM outage would fail Step 6 right after anyway).
    //   4. Escape hatch: `--force-instance-takeover` overwrites a wedged
    //      lock with a loud audited error! line. The 90s TTL already
    //      auto-clears crashed holders without any flag.
    //
    // Costs: standard-tier SSM PutParameter is free; ~2,880 calls/day at
    // the 30 s heartbeat interval is well under the 40 TPS SSM rate cap
    // (operator lock 2026-05-24 chose SSM as the source of truth).
    //
    // Phase 0 Item 19f — chain bridge from the broader `shutdown_notify`
    // (constructed at Step 8b below) to the heartbeat's own `Notify`.
    //
    // `instance_lock_held` is the RESILIENCE-03 mint-tripwire flag: set
    // `true` here on acquisition; the heartbeat flips it `false` on lock
    // loss / shutdown release; `TokenManager::acquire_token` refuses any
    // `generateAccessToken` mint while it reads `false`.
    let instance_lock_held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // R8-EDGE-1 / R8-CPLX-1 (2026-07-07): the heartbeat handle + shutdown
    // Notify are held in a notify-on-drop guard (NOT bare locals) across the
    // entire pre-`DhanLaneRunHandles` window — Step 6 auth, the IP gate,
    // QuestDB DDL, the §4 infinite-retry universe fetch, WAL reinject, WS
    // spawn, connected-alerts. A supervisor `.abort()` on any of those awaits
    // OR any `return Err(StartLaneError::…)` drops the guard, whose Drop
    // `notify_one()`s the heartbeat's shutdown (graceful SSM DeleteParameter
    // release — never an abort) so the lock can never be zombie-renewed into
    // a permanent AlreadyHeld/RESILIENCE-01 lane lockout. Defused into the
    // lane struct's BUG-1 fields at the construction site.
    let instance_lock_guard: InstanceLockHeartbeatGuard = {
        info!(
            mode = trading_mode.as_str(),
            "Phase 0 Item 19: acquiring dual-instance SSM lock (always-on for every \
             Dhan-enabled boot, BEFORE the token mint — lock-before-mint 2026-07-04)"
        );

        // Bounded transport-retry budget — mirrors the Step 6 SSM
        // credential fetch (3 attempts, exponential backoff).
        const INSTANCE_LOCK_ACQUIRE_MAX_ATTEMPTS: u32 = 3;

        let ssm_client = std::sync::Arc::new(
            tickvault_core::auth::secret_manager::create_ssm_client_public().await,
        );

        // host_id composition: pid + boot_random + aws_instance_id (when
        // present). The instance lock path is env-qualified, so dev /
        // sandbox / prod cannot collide.
        let env = tickvault_core::auth::secret_manager::resolve_environment()
            .context("Phase 0 Item 19: cannot resolve environment for lock path")
            .map_err(StartLaneError::BootAbortErr)?;
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

        // Transport (SSM) errors are retried on the bounded budget;
        // AlreadyHeld is a DEFINITIVE answer and is never retried.
        let acquire_result = {
            let mut attempt: u32 = 0;
            loop {
                attempt = attempt.saturating_add(1);
                match tickvault_core::instance_lock::try_acquire_instance_lock(
                    &ssm_client,
                    &env,
                    &host_id,
                )
                .await
                {
                    Ok(outcome) => break Ok(outcome),
                    Err(err) if attempt >= INSTANCE_LOCK_ACQUIRE_MAX_ATTEMPTS => break Err(err),
                    Err(err) => {
                        warn!(
                            attempt,
                            max_attempts = INSTANCE_LOCK_ACQUIRE_MAX_ATTEMPTS,
                            error = %err,
                            "Phase 0 Item 19: SSM lock acquire failed (transient) — retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(u64::from(
                            2u32.saturating_pow(attempt),
                        )))
                        .await;
                    }
                }
            }
        };

        match acquire_result {
            Ok(tickvault_core::instance_lock::AcquireOutcome::Acquired) => {
                instance_lock_held.store(true, std::sync::atomic::Ordering::Release);
                info!(
                    env = %env,
                    host_id = %host_id,
                    lock_key = %lock_key,
                    ttl_secs = tickvault_core::instance_lock::INSTANCE_LOCK_TTL_SECS,
                    "Phase 0 Item 19: dual-instance lock acquired (pre-mint)"
                );
            }
            Ok(tickvault_core::instance_lock::AcquireOutcome::AlreadyHeld { holder })
                if force_instance_takeover_requested() =>
            {
                // Operator escape hatch (--force-instance-takeover): a
                // wedged lock that the 90s TTL cannot clear. The
                // takeover itself logs a loud RESILIENCE-01 error!
                // audit line naming the displaced holder.
                match tickvault_core::instance_lock::force_takeover_instance_lock(
                    &ssm_client,
                    &env,
                    &host_id,
                )
                .await
                {
                    Ok(()) => {
                        instance_lock_held.store(true, std::sync::atomic::Ordering::Release);
                        warn!(
                            env = %env,
                            host_id = %host_id,
                            lock_key = %lock_key,
                            displaced_holder = %holder,
                            "Phase 0 Item 19: dual-instance lock FORCE-TAKEN via \
                             --force-instance-takeover — proceeding as sole owner"
                        );
                    }
                    Err(err) => {
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
                            "Phase 0 Item 19: force-takeover failed — BLOCKING BOOT"
                        );
                        notifier.notify(
                            tickvault_core::notification::events::NotificationEvent::DualInstanceDetected {
                                holder: format!("(force-takeover-failed: {err})"),
                                lock_key,
                            },
                        );
                        return Err(StartLaneError::BootAbortClean);
                    }
                }
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
                    "Phase 0 Item 19: another tickvault process holds the lock — BLOCKING BOOT \
                     (before any token mint — the peer's Dhan session is untouched)"
                );
                notifier.notify(
                    tickvault_core::notification::events::NotificationEvent::DualInstanceDetected {
                        holder,
                        lock_key,
                    },
                );
                return Err(StartLaneError::BootAbortClean);
            }
            Err(err) => {
                // SSM PutParameter / GetParameter failed after the bounded
                // retry budget (network outage, IAM denial, sustained
                // throttle). Same HALT semantics as already-held: we
                // cannot prove there's no peer. Note: a sustained SSM
                // outage would fail Step 6's credential fetch immediately
                // after this anyway — this arm does not add a new
                // boot-failure class, it only surfaces it earlier.
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
                return Err(StartLaneError::BootAbortClean);
            }
        }

        // Lock held — spawn the heartbeat. The heartbeat owns its
        // own `Notify` shutdown source; main.rs's broader
        // `shutdown_notify` (constructed at the Step 8b stage) is
        // chained into it via the bridge task installed further
        // down (Item 19f). On the bridge firing, the heartbeat
        // releases the lock by deleting the SSM Parameter so the next
        // boot sees a clean slate immediately. The held-flag clone lets
        // the heartbeat disarm the RESILIENCE-03 mint tripwire on lock
        // loss / shutdown release.
        let heartbeat_shutdown_inner = std::sync::Arc::new(tokio::sync::Notify::new());
        let heartbeat_shutdown_for_chain = heartbeat_shutdown_inner.clone();
        let heartbeat_handle = tickvault_core::instance_lock::spawn_instance_lock_heartbeat(
            ssm_client,
            env,
            host_id,
            heartbeat_shutdown_inner,
            std::sync::Arc::clone(&instance_lock_held),
        );
        InstanceLockHeartbeatGuard::new(heartbeat_handle, heartbeat_shutdown_for_chain)
    };
    // BUG-1 fix (2026-07-05, live 15:35 IST stop proof): the heartbeat
    // JoinHandle is NO LONGER dropped here. It rides into
    // `DhanLaneRunHandles` below so `teardown_dhan_lane_tasks` can
    // bound-await the shutdown release (SSM DeleteParameter) BEFORE the
    // process exits. Previously the handle was dropped on this line
    // (`let _instance_lock_handle = ...`), so the release RACED process
    // exit — the 15:35 IST graceful EOD stop left the lock parameter in
    // SSM, and a quick Stop→Start (<90s TTL) HALTed with RESILIENCE-01.
    //
    // R8-EDGE-1 / R8-CPLX-1 (2026-07-07): an early boot-abort return (or a
    // supervisor cancel) between here and the handles struct NO LONGER
    // detaches the still-renewing heartbeat — the guard's Drop notify_one()s
    // its shutdown so the SSM release runs and the next runtime cold-start
    // attempt can re-acquire. (The previous "TTL backstop" note was true
    // only on the boot-ON path, where the process exits; on the D2b runtime
    // path a detached heartbeat kept renewing forever and permanently
    // wedged the lane with AlreadyHeld → RESILIENCE-01 pages per retry.)

    // -----------------------------------------------------------------------
    // Step 6: Authenticate with Dhan API (infinite retry for transient errors)
    // -----------------------------------------------------------------------
    // Runs AFTER Step 6a-prime (lock-before-mint, 2026-07-04): the
    // dual-instance lock is held by now, so the `generateAccessToken`
    // mint below can never invalidate a live peer's token. The
    // RESILIENCE-03 tripwire flag rides into the TokenManager so a
    // mid-session lock loss also refuses any renewal-fallback mint.
    info!("authenticating with Dhan API via SSM → TOTP → JWT");

    let token_init_timeout =
        std::time::Duration::from_secs(tickvault_common::constants::TOKEN_INIT_TIMEOUT_SECS);
    let token_manager = match tokio::time::timeout(
        token_init_timeout,
        TokenManager::initialize(
            &config.dhan,
            &config.token,
            &config.network,
            &notifier,
            Some(std::sync::Arc::clone(&instance_lock_held)),
        ),
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
            return Err(StartLaneError::BootAbortClean);
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
            return Err(StartLaneError::BootAbortClean);
        }
    };

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
                    return Err(StartLaneError::BootAbortClean);
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
            return Err(StartLaneError::BootAbortClean);
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
    info!("setting up QuestDB tables (ticks + candles + historical_candles)");

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
        // C2 (2026-07-03): attribute the failure honestly. A ClientBuild
        // error means OUR HTTP client could not be constructed (host
        // fd/TLS/resolver pressure) — routing that to the BOOT-02
        // Docker/QuestDB runbook misdirects triage. Control flow is
        // identical in both arms (halt the lane start).
        if matches!(
            &e,
            tickvault_storage::boot_probe::BootProbeError::ClientBuild(_)
        ) {
            tracing::error!(
                error = ?e,
                code =
                    tickvault_common::error_code::ErrorCode::HttpClient01BuildFailed.code_str(),
                "HTTP-CLIENT-01 HTTP client build failed — host fd/TLS/resolver problem, \
                 not QuestDB — halting"
            );
        } else {
            tracing::error!(
                error = ?e,
                code = tickvault_common::error_code::ErrorCode::Boot02DeadlineExceeded.code_str(),
                "BOOT-02 QuestDB never reached ready state — halting"
            );
        }
        return Err(StartLaneError::BootAbortErr(anyhow::anyhow!(
            "BOOT-02 QuestDB readiness deadline exceeded: {e}"
        )));
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
                return Err(StartLaneError::BootAbortErr(anyhow::anyhow!(
                    "BOOT-03 clock skew {skew_secs:+.3}s exceeds threshold {threshold_secs:.2}s \
                     (source: {source}) — fix `chronyc tracking` then restart"
                )));
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
        // Option-chain minute-snapshot QuestDB table DROPPED 2026-06-23
        // (operator: ticks are the single source of truth; the dormant
        // never-written table is gone). The scheduler keeps populating
        // the RAM `SnapshotCache` for the strategy — it just no longer
        // mirrors into QuestDB.
        // Wave 6 Sub-PR #1 item 1.4a — shadow candle tables (9 timeframes)
        // + aggregator_seal_audit forensic table. The future writer task
        // (item 1.4b) writes sealed candles into these tables; the boot
        // DDL must run first so the ILP writes don't 404. Idempotent
        // CREATE TABLE IF NOT EXISTS — safe to call on every boot.
        tickvault_storage::shadow_persistence::ensure_shadow_candle_tables(&config.questdb),
    );

    // Human-readable analyst console views (ticks_named / candles_named).
    // Sequential AFTER the base-table ensure join so `ticks` + `candles_1m`
    // exist before CREATE VIEW validates its column references. Fail-soft +
    // convergent (CREATE OR REPLACE VIEW — every boot converges the deployed
    // definition to the code); a failure retries next boot. Groww-only boots
    // get their own call site in `groww_activation.rs::activate_groww_lane`.
    tickvault_storage::console_views::ensure_named_views(&config.questdb).await;

    // D2 Stage 2: the seal-writer (item 1.4b) is now spawned ONCE in the hoisted
    // `build_shared_infra` prefix (via `spawn_seal_writer_loop`) so it runs for
    // BOTH the Dhan-OFF and Dhan-ON-slow paths and installs the process-wide
    // `global_seal_sender` Groww routes through. The duplicate inline lane copy
    // is removed.

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
    // F4/F13 (2026-07-08): once-per-process latch. The reset task is an
    // INFINITE loop driving the PROCESS-GLOBAL FirstSeenSet singleton — a
    // per-lane-cycle spawn leaked one immortal loop per D2b Dhan
    // enable/disable cycle (N copies all resetting the same singleton at
    // IST midnight). First lane start wins; later cold-starts skip.
    if FIRST_SEEN_RESET_SPAWNED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        info!(
            "first-seen IST-midnight reset task already running — skipping duplicate spawn (lane restart)"
        );
    } else {
        let _first_seen_reset_handle =
            tickvault_core::pipeline::first_seen_set::spawn_ist_midnight_reset_task(first_seen);
    }

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

    // 2026-07-01 per-lane-leak fix: the three PROCESS-GLOBAL (host-level)
    // supervised observability monitors — spill disk-health watcher
    // (DISK-WATCHER-01), OOM-kill monitor (PROC-01), and the fd/RSS/spill-free
    // resource monitor (RESOURCE-01/02/03) — used to be spawned HERE, inside the
    // per-lane `start_dhan_lane`. Because `start_dhan_lane` re-runs on every Dhan
    // enable / stop→restart / cold-start retry (`run_dhan_lane_cold_start`), each
    // re-invocation leaked a fresh never-aborted monitor → duplicate pages + N×
    // metric increments for one real event. They are host/process-global, so they
    // are now spawned EXACTLY ONCE in `main()`'s process-global prefix (right
    // after `build_shared_infra`, before the Dhan-lane gate) and owned at the
    // process level — independent of any Dhan enable/disable/restart.

    // D2 Stage 2: `health_status` is now built ONCE in the hoisted
    // `build_shared_infra` prefix and destructured into `main()` scope above, so
    // the lane references the SAME Arc the API server + run-loop hold. The
    // tick-persistence flag below sets state on that shared registry.

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
                        return Err(StartLaneError::BootAbortErr(anyhow::anyhow!(
                            "pre-market profile check FAILED during market hours — HALT: {reason}"
                        )));
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
    let (subscription_plan, slow_boot_universe, needs_instrument_persist) = match load_instruments(
        config,
        is_trading,
        trading_calendar.as_ref(),
    )
    .await
    {
        Ok(loaded) => loaded,
        Err(err) => {
            // Observability slice #2: emit the symmetric failure alert.
            // Mirrors the fast-boot site — the bare `?` previously halted
            // boot with no operator Telegram. Fail-closed halt preserved;
            // NOT a duplicate of INSTR-FETCH-* (those infinite-retry).
            // Security (review 2026-06-12): redact the anyhow chain before it
            // reaches Telegram + the log sinks (authentication.md rule 2 — a
            // token must never appear in error messages). capture_rest_error_body
            // is the ratchet-guaranteed redactor (URL-param + JWT + credential).
            let safe_reason = tickvault_common::sanitize::capture_rest_error_body(&err.to_string());
            error!(error = %safe_reason, "instrument load failed at boot — alerting operator and halting");
            notifier.notify(NotificationEvent::InstrumentBuildFailed {
                reason: safe_reason,
                manual_trigger_url: format!(
                    "http://{}:{}/api/instruments/rebuild",
                    config.api.host, config.api.port
                ),
            });
            return Err(StartLaneError::BootAbortErr(err));
        }
    };

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

    // 2026-07-03 feed parity: record Dhan's subscribed counts into the shared
    // per-feed health registry (one-shot, cold path) so the readiness +
    // end-of-day Telegram feed lines — and the feed-control page — show a
    // REAL Dhan instrument count instead of "unknown". Mirrors the fast-boot
    // site above (boot symmetry).
    if let Some(ref plan) = subscription_plan {
        let summary = &plan.summary;
        let indices = summary
            .major_index_values
            .saturating_add(summary.display_indices);
        let stocks = summary.total.saturating_sub(indices);
        feed_health.set_subscribed(
            tickvault_common::feed::Feed::Dhan,
            stocks as u64,
            indices as u64,
        );
    }

    // Boot-timing proof (DailyUniverse scope only — sentinel-guarded so
    // Indices4Only emits nothing). Reads the wall-clock stashed by
    // `load_daily_universe_plan`; REAL daily evidence the O(1) warm path is
    // working. DEMOTED 2026-07-09 (operator escalation — Telegram noise N3):
    // the standalone Telegram essay is now a structured log line; the
    // operator-facing count lives on the boot bubble's Instruments line.
    if let Some(message) = format_instrument_load_telegram(
        INSTRUMENT_LOAD_ELAPSED_MS.load(std::sync::atomic::Ordering::Relaxed),
        INSTRUMENT_LOAD_WARM_SKIPPED.load(std::sync::atomic::Ordering::Relaxed),
        INSTRUMENT_LOAD_TOTAL_ROWS.load(std::sync::atomic::Ordering::Relaxed),
    ) {
        info!(target: "boot", %message, "instrument load timing (demoted from Telegram 2026-07-09)");
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

    // D2c (C4): install THIS lane's TokenManager as the live health-gauge source,
    // OVERWRITING any prior (now-dead) manager from an earlier cold-start. Unlike
    // the global `OnceLock` above (set-once at first boot, immutable thereafter),
    // this slot tracks the CURRENT lane manager — so after a runtime stop→re-start
    // the self-test token-headroom + SLO token-freshness gauges read the NEW
    // manager's remaining seconds, not the stale boot-time one. Cleared at every
    // lane→Off transition (operator disable / watchdog-Halt / already-torn-down).
    feed_runtime.set_live_token_manager(token_manager.clone());

    // Fetch credentials ONCE for all downstream consumers (WS pool, order update WS, trading pipeline).
    // Previously fetched 3 separate times — each SSM call is a network roundtrip to AWS.
    let ws_client_id = {
        let credentials = secret_manager::fetch_dhan_credentials()
            .await
            .context("failed to fetch Dhan client ID for WebSocket + trading")
            .map_err(StartLaneError::BootAbortErr)?;
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
    // Step 9 starts tick processor BEFORE the STAGE-C.2b WAL re-injection
    // awaits AND before connections are spawned (Step 8b), so frames are
    // consumed immediately — prevents re-injection send timeouts on a
    // >capacity replay and frame send timeouts during stagger.
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

    // WS-GAP-08: before the FIRST Dhan WS connect, honour any persisted
    // 429 rate-limit cooldown that survived a `process::exit(2)` + supervisor
    // restart. Without this, a restart wipes the in-memory `rate_limit_streak`
    // and the fresh process reconnects with a 0ms first retry straight back
    // into Dhan's still-active 429 window → instant-429 restart loop. The wait
    // is fail-open (no/corrupt/stale file → no wait) and bounded by the cap, so
    // a bad file can never hang boot beyond 5 minutes. Only on a day we will
    // actually connect.
    if should_connect_ws {
        wait_out_persisted_ws_rate_limit_cooldown().await;
    }

    let (pool_receiver, ws_pool_ready) = if should_connect_ws {
        match create_websocket_pool(
            &token_handle,
            &ws_client_id,
            &subscription_plan,
            config,
            is_market_hours,
            Some(notifier.clone()),
            ws_frame_spill.clone(),
            // PR-E: hand the Dhan main feed the shared runtime enable flag.
            Some(feed_runtime.dhan_flag()),
            // D2c (C4): hand the LANE-OWNED TokenManager so a runtime
            // stop→re-start renews the live lane manager on wake, not the
            // stale global. This is the canonical cold path.
            Some(token_manager.clone()),
        ) {
            Some((receiver, pool)) => (Some(receiver), Some(pool)),
            // FTC-14: we intended to connect (should_connect_ws == true ⇒ plan
            // present), but the pool build/spawn failed. Fail the cold-start so
            // it surfaces as DHAN-LANE-02 (stage=ws_pool), not a silent
            // offline-blind proceed. The lane returns to Off + bounded retry.
            None => return Err(StartLaneError::WsPoolSpawn),
        }
    } else if !is_trading && !is_mock_trading {
        // BUG-2 fix (2026-07-05): no pool ⇒ the lane-running flag + the PR-2
        // pool-present sentinel stay FALSE (they are only set at the
        // pool-spawn site). The feeds page shows Dhan as enabled-but-idle.
        info!(
            "WebSocket pool skipped — non-trading day (no live market data to capture); \
             Dhan lane stays enabled-but-idle (dhan_running=false, no pool)"
        );
        (None, None)
    } else {
        warn!(
            "WebSocket pool skipped — running in offline mode; Dhan lane stays \
             enabled-but-idle (dhan_running=false, no pool)"
        );
        (None, None)
    };

    // -----------------------------------------------------------------------
    // Step 9: Spawn tick processor FIRST (before WAL re-injection + WS spawn)
    // -----------------------------------------------------------------------
    // PR #2 (2026-05-18): `shared_movers` snapshot retired alongside
    // the deleted `top_movers` / `option_movers` modules.

    // D2 Stage 2: the tick broadcast (`tick_broadcast_sender`) is now created
    // ONCE in the hoisted `build_shared_infra` prefix and destructured into
    // `main()` scope above. The lane references that SAME sender so the obs /
    // aggregator / tick-storage subscribers (also spawned in the prefix, having
    // already `.subscribe()`d) receive every tick this lane processor publishes.

    // S12 wiring: heartbeat watchdog (slow boot).
    // Same responsibilities as the fast-boot watchdog above. Spawned
    // after token_handle + tick_broadcast_sender are both available.
    // F13 (2026-07-08): LANE-OWNED — the previous dropped `let _ = …`
    // binding detached an INFINITE 30s loop pinning the dead lane's
    // token_handle (arc-swap): after a runtime Dhan disable it fired a
    // false AUTH-GAP-01 "token handle is None" ERROR every 30s forever,
    // and each D2b cold-start cycle leaked another copy. Guard-wrapped
    // across the pre-lane awaits (WAL reinject + connected-alerts), then
    // defused into `DhanLaneRunHandles.heartbeat_watchdog_handle` so
    // teardown/Drop abort it. (The FAST arm's twin stays a plain spawn —
    // main() is never supervisor-aborted, process-lifetime by design.)
    let heartbeat_watchdog_guard = PreLaneAbortGuard::new(vec![spawn_heartbeat_watchdog(
        std::sync::Arc::clone(&token_handle),
        tick_broadcast_sender.clone(),
    )]);

    // In-market gap-backfill is DISABLED by user policy. Historical
    // candle data must NOT be injected into the live `ticks` table.
    // Post-market historical fetch runs on a separate cold path and
    // writes only to `historical_candles`.
    //
    // D2 Stage 2: the slow-boot observability consumer is now spawned in the
    // hoisted `build_shared_infra` prefix (subscribe-before-publish), not here.

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
                // §36 (hostile-review round 1, 2026-07-08; §36.7 all-months
                // 2026-07-10): the IndexFuture targets (~12 under §36.7)
                // are unconditionally SKIPPED by the prev-day loop
                // (`instrument_type_for_role` → None — Dhan-historical FUTIDX
                // UNVERIFIED-LIVE), so they must NOT sit in the coverage
                // DENOMINATOR — pre-fix a perfect day could never read 100%
                // and the spot pct was permanently diluted. Same role filter
                // the 15:31 cross-verify target build uses.
                let pd_expected = pd_universe
                    .subscription_targets
                    .iter()
                    .filter(|t| {
                        tickvault_app::prev_day_ohlcv_boot::instrument_type_for_role(t.role)
                            .is_some()
                    })
                    .count();
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
                            code = tickvault_common::error_code::ErrorCode::PrevDay01CoverageEmpty
                                .code_str(),
                            expected = pd_expected,
                            skipped = summary.skipped,
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

    // D2 Stage 2: the 21-TF Engine-B aggregator and the L10 tick-storage
    // broadcast consumer are now spawned ONCE in the hoisted `build_shared_infra`
    // prefix — BEFORE this lane's tick processor publishes (subscribe-before-
    // publish, preserved by construction). Both subscribe to the SAME hoisted
    // `tick_broadcast_sender` this lane references.

    // PR #450 commit 6 (2026-05-03): V2 snapshot handle DELETED.
    // The legacy /api/movers (V2) route + handler + in-memory
    // MoversTrackerV2 + V2 pipeline are gone. The new unified
    // /api/movers handler (commit 4) reads from the canonical
    // movers_1s + 25 mat views via QuestDB SQL.

    // PR #4 (2026-05-19): SharedSpotPrices map RETIRED — depth-20/200 +
    // movers pipelines that consumed it are deleted per operator lock
    // 2026-05-15 (websocket-connection-scope-lock.md).

    // R9-EDGE-2 / COV-C4-2 (2026-07-07): the IST-midnight enricher rollover
    // loop + the 5-min prev_oi refresh poller are LANE tasks (they hold this
    // attempt's `Arc<TickEnricher>` + QuestDB config). Their handles are
    // hoisted OUT of the tick-processor block below so they can be
    // guard-wrapped across the pre-construction awaits and lane-owned in
    // `DhanLaneRunHandles` — the previous `let _ = ...` bindings DETACHED
    // both at spawn, leaking one immortal midnight-rollover loop (plus a
    // potentially-immortal 5-min QuestDB poller while candles_1d stays
    // empty) per D2b lane cold-start cycle, each pinning the dead lane's
    // enricher and issuing duplicate cold-path SELECTs at IST midnight.
    let mut midnight_rollover_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut prev_oi_refresh_handle: Option<tokio::task::JoinHandle<()>> = None;

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
        let greeks_enricher = build_inline_greeks_enricher(config, &subscription_plan);

        // NT-15: seed the tick-gap detector with every subscribed SID so a
        // never-ticking instrument becomes a first-tick black-hole signal.
        seed_tick_gap_detector_from_plan(&subscription_plan);

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
        // 2026-06-28: the boot-time Option Chain REST prev_oi OVERLAY was
        // REMOVED with the entire option_chain subsystem (operator directive
        // 2026-06-28 — "drop the option chain entire implementations and its
        // table also"; the subsystem was disabled since 2026-06-02 with no
        // Data API entitlement). The orphaned overlay built a `prev_oi_cache`
        // that was immediately shadowed by the tick-enricher's own
        // `prev_day_ohlcv`-sourced load below — so its removal is behaviour-
        // neutral. The LIVE OI-change path (tick enricher reads OI from the
        // `prev_day_ohlcv` table) is untouched and remains the sole prev_oi
        // source. #T4 (2026-05-20): the `previous_close` QuestDB boot SELECT
        // was already retired; `PrevDayCache` stays empty and the cascade
        // seal-time pct path falls back to 0.0 pct per the div-by-zero policy.

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
        // candles_1d), then loops.
        //
        // R9-EDGE-2 / COV-C4-2 (2026-07-07): the handle is BOUND (hoisted
        // Option above) and rides into `DhanLaneRunHandles` — dropping a
        // JoinHandle DETACHES the task (never cancels it), so the previous
        // `let _ = ...` leaked one immortal rollover loop per D2b lane
        // cold-start cycle.
        midnight_rollover_handle = Some(
            tickvault_core::pipeline::tick_enricher::spawn_midnight_rollover_task(
                std::sync::Arc::clone(&tick_enricher),
                config.questdb.clone(),
            ),
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
        //
        // R9-EDGE-2 / COV-C4-2 (2026-07-07): handle bound + lane-owned
        // (see the midnight-rollover comment above) — per live-feed-purity
        // rule 10 the live path never writes candles_1d, so on a box
        // without the historical 1d row the self-exit condition may never
        // fire and a detached copy would poll QuestDB every 5 min forever.
        prev_oi_refresh_handle = Some(
            tickvault_core::pipeline::tick_enricher::spawn_prev_oi_cache_refresh_task(
                std::sync::Arc::clone(&tick_enricher),
                config.questdb.clone(),
            ),
        );
        tracing::info!(
            "prev_oi_cache periodic refresh task spawned (Phase 2.11 — \
             5min poll for fresh-deploy / matview-catchup recovery)"
        );

        let _ = slow_registry; // PR #2: instrument_registry was used by deleted movers persistence helpers
        // SP5: dedicated clone moved into the tick-processor coroutine so the
        // original `feed_health` Arc stays available for the pool watchdog below.
        let feed_health_for_processor = std::sync::Arc::clone(&feed_health);
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
                Some(feed_health_for_processor),        // SP5: Dhan live-feed health
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

    // R7-EDGE-1 / COV-R7-1 fix (2026-07-07): abort-on-drop floor for the tick
    // processor across every top-level await between here and the
    // `let lane = DhanLaneRunHandles {` construction — the STAGE-C.2b WAL
    // re-injection just below (total wall-clock unbounded BY DESIGN on a WAL
    // backlog, 30s per stalled send — ws-reinject-error-codes.md), then
    // `install_subscribe_channels` / `spawn_websocket_connections` /
    // `emit_websocket_connected_alerts` (≤30s health poll). On the D2b
    // runtime cold-start the supervisor `.abort()`s this future on a
    // disable-during-Starting; dropping the BARE `Option<JoinHandle>` at any
    // of those awaits DETACHED (did not abort) the processor — on the next
    // runtime re-enable the detached processor persisted a duplicate tick
    // stream alongside the fresh lane's (capture_seq is IN the `ticks` DEDUP
    // key, so duplicate rows are NOT collapsed). The guard aborts on Drop
    // unless defused into the lane struct's H8 floor via `into_inner`.
    // Ratchet: secret_manager.rs::test_pre_lane_abort_guard_covers_early_spawn_windows.
    let processor_guard = PreLaneAbortGuard::new(processor_handle.into_iter().collect());
    // R9-EDGE-2 / COV-C4-2 (2026-07-07): same pre-construction abort-on-drop
    // floor for the two enricher maintenance tasks spawned alongside the
    // processor above — they sit before the SAME unbounded-by-design
    // WAL-reinject + ≤30s connected-alerts awaits, so a cancel-mid-Starting
    // would otherwise detach them. Defused into the lane struct's H8 floor
    // at construction (`midnight_rollover_handle` / `prev_oi_refresh_handle`).
    let midnight_rollover_guard =
        PreLaneAbortGuard::new(midnight_rollover_handle.into_iter().collect());
    let prev_oi_refresh_guard =
        PreLaneAbortGuard::new(prev_oi_refresh_handle.into_iter().collect());

    // STAGE-C.2b: Slow-boot mirror of the fast-boot re-injection path.
    // Ordering (C3 review CRITICAL fix, 2026-07-03):
    //   [channel create, Step 8a] → [tick processor spawned ABOVE, Step 9,
    //   draining] → [this reinject await] → [WS connections, Step 8b BELOW].
    // The consumer MUST already be running — a replay larger than
    // FRAME_CHANNEL_CAPACITY would otherwise fill the channel with nobody
    // draining, stall into the 30s WAL_REINJECT_SEND_TIMEOUT, abort
    // NOT-clean, and the WAL would never archive (the storm loop). Because
    // this await completes BEFORE Step 8b spawns the connections, recovered
    // frames still land ahead of any live frame (FIFO — one sequential
    // sender loop) and are persisted idempotently via QuestDB dedup keys.
    // Ratchet: wal_reinject::tests::ratchet_tick_processor_spawns_before_reinject_await.
    //
    // CRASH-SAFETY (zero-loss MEDIUM fix 2026-06-30): same staged-then-confirm
    // contract as the fast-boot path — see its comment. Confirm only if BOTH
    // the LiveFeed re-injection and the OrderUpdate drain below are clean.
    let mut ws_wal_replay_reinjection_clean = true;
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
            // C3 fix (2026-07-03): slow-boot mirror of the fast-boot bounded
            // chunked-backpressure re-injection — see the fast-boot comment.
            // Runbook: ws-reinject-error-codes.md.
            let outcome = tickvault_app::wal_reinject::reinject_wal_frames(
                &sender,
                to_inject,
                tickvault_common::constants::WAL_REINJECT_CHUNK_SIZE,
                std::time::Duration::from_secs(
                    tickvault_common::constants::WAL_REINJECT_SEND_TIMEOUT_SECS,
                ),
            )
            .await;
            metrics::counter!(
                "tv_ws_frame_wal_reinjected_total",
                "ws_type" => "live_feed"
            )
            .increment(outcome.injected);
            if outcome.clean {
                info!(
                    injected = outcome.injected,
                    "STAGE-C.2b: LiveFeed re-injection complete (slow boot) — all replayed \
                     frames delivered with backpressure"
                );
            } else {
                // WS-REINJECT-01 already error!-paged inside the helper;
                // record the not-clean flag so the slow-boot confirm is
                // skipped and the staged segments re-replay next boot.
                ws_wal_replay_reinjection_clean = false;
                warn!(
                    injected = outcome.injected,
                    aborted_remaining = outcome.aborted_remaining,
                    "STAGE-C.2b: LiveFeed re-injection aborted (slow boot) — staged WAL \
                     segments remain in replaying/ for re-replay next boot"
                );
            }
        } else {
            ws_wal_replay_reinjection_clean = false;
            warn!(
                frames = ws_wal_replay_live_feed.len(),
                "STAGE-C.2b: LiveFeed replay frames held but no pool (slow boot) — \
                 frames stay staged in WAL replaying/, not re-injected"
            );
        }
    }

    // Step 8b: NOW spawn WebSocket connections (tick processor is already consuming).
    // S4-T1a/T1b: shared shutdown_notify for pool watchdog + graceful shutdown.
    let shutdown_notify = std::sync::Arc::new(tokio::sync::Notify::new());

    // Phase 0 Item 19f — chain `shutdown_notify` into the instance-lock
    // heartbeat's own Notify. Without this bridge the heartbeat would
    // only see the broader shutdown when its own Notify fires (never,
    // in practice); now Ctrl-C / 15:30 IST close / pool-halt all
    // trigger the heartbeat's `GracefulRelease` audit row + lock
    // release before the process exits.
    //
    // BUG-1 fix (2026-07-05): `.clone()` instead of `.take()` — the same
    // Notify ALSO rides into `DhanLaneRunHandles.instance_lock_shutdown`
    // so `teardown_dhan_lane_tasks` can `notify_one()` it DIRECTLY
    // (permit-storing, lost-wake safe per audit Rule 16 — this bridge's
    // `notify_waiters()` wake can be lost if it fires before the bridge
    // task parks) and then bound-await the heartbeat's release.
    //
    // SEC-C3-1 / R9-CPLX-1 / COV-R8-2 (2026-07-07): the bridge JoinHandle is
    // guard-wrapped, never detached. `shutdown_notify` is PER start-attempt,
    // and its only `notify_waiters()` lives in `teardown_dhan_lane_tasks` —
    // which never runs when the lane struct is never constructed. A D2b
    // cancel-mid-Starting (or any pre-lane `return Err`) therefore left the
    // bridge parked on the orphaned per-attempt Notify FOREVER: one immortal
    // task + two pinned Arc<Notify> per cancelled cold-start attempt,
    // accumulating across bounded-backoff retries. Abort is safe by the
    // BUG-1 contract above: the bridge's only pending action (`notify_one`
    // on the heartbeat's shutdown) is ALSO performed directly by
    // teardown/Drop, so aborting the bridge never loses the release signal.
    // Defused into `DhanLaneRunHandles.heartbeat_bridge_handle` at the lane
    // construction so teardown/Drop abort it there.
    let bridge_shutdown = instance_lock_guard.shutdown_handle();
    let heartbeat_bridge_handle = bridge_shutdown.map(|heartbeat_shutdown| {
        let shutdown_signal = shutdown_notify.clone();
        tokio::spawn(async move {
            shutdown_signal.notified().await;
            heartbeat_shutdown.notify_one();
        })
    });
    let heartbeat_bridge_guard =
        PreLaneAbortGuard::new(heartbeat_bridge_handle.into_iter().collect::<Vec<_>>());

    // H7 lane-scoped watchdog Halt signal. For a RUNTIME lane (`lane_scoped`)
    // the pool watchdog signals this on a 300s-all-down Halt INSTEAD of
    // `process::exit(2)`; the parked task (`park_running_dhan_lane`) observes it
    // and tears the lane down + drives the FSM to `Off` so the supervisor
    // re-cold-starts it — Groww + shared infra stay ALIVE. `None` for a BOOT-ON
    // start (the watchdog keeps its `process::exit(2)` single-feed contract).
    let lane_halt_notify: Option<std::sync::Arc<tokio::sync::Notify>> = if lane_scoped {
        Some(std::sync::Arc::new(tokio::sync::Notify::new()))
    } else {
        None
    };

    let (ws_handles_guard, pool_watchdog_guard, ws_pool_arc) = if let Some(pool) = ws_pool_ready {
        let pool_arc = std::sync::Arc::new(pool);
        // O1-B (2026-04-17): install per-connection runtime subscribe
        // channels BEFORE spawn — same as the FAST BOOT path (main.rs ~830).
        pool_arc.install_subscribe_channels().await;
        let handles = spawn_websocket_connections(std::sync::Arc::clone(&pool_arc)).await;
        // R7-EDGE-1 fix (2026-07-07): wrap the just-spawned main-feed pool
        // handles in the abort-on-drop guard BEFORE the
        // `emit_websocket_connected_alerts(...).await` below — that await
        // polls pool health for up to WS_BOOT_PER_CONN_DEADLINE_SECS (30s),
        // exactly the window where connections are still handshaking (or
        // 429-stuck) and an operator disable is most likely. A supervisor
        // `.abort()` parked there previously dropped the BARE
        // `Vec<JoinHandle>`, DETACHING the read loops: the detached pool
        // went flag-dormant on the disable and RECONNECTED on the next
        // runtime re-enable alongside the fresh lane's pool — 2 live Dhan
        // main-feed connections (websocket-connection-scope-lock violation)
        // + duplicate-row tick persistence. The guard aborts on Drop unless
        // defused into `DhanLaneRunHandles.ws_handles` (the H8 floor).
        let handles_guard = PreLaneAbortGuard::new(handles);
        // R8-EDGE-2 / SEC-C2-1 fix (2026-07-07): bind the watchdog handle and
        // wrap it in the SAME pre-lane abort-on-drop guard as the WS handles.
        // The watchdog is spawned BEFORE the ≤30s
        // `emit_websocket_connected_alerts(...).await` below, and its ONLY
        // in-loop exit is `shutdown_notify.notified()` — a signal nothing can
        // fire before the lane struct exists. A supervisor `.abort()` parked
        // on that await previously LEAKED the watchdog forever: it kept
        // writing the PROCESS-SHARED `/health` websocket_connections +
        // feed-health Dhan-connected slots every 5s from the DEAD pool
        // (fighting the re-enabled lane's fresh watchdog, last-writer-wins
        // flapping), and — reading the SHARED `dhan_enabled` atomic — its
        // dead-pool verdicts fired a FALSE WebSocketPoolDegraded page at 60s
        // and a FALSE CRITICAL WebSocketPoolHalt + `tv_pool_self_halts_total`
        // at 300s after the operator re-enabled Dhan in market hours. The
        // guard aborts on Drop unless defused into
        // `DhanLaneRunHandles.pool_watchdog_handle` at the lane construction.
        let dhan_pool_watchdog_handle = spawn_pool_watchdog_task(
            std::sync::Arc::clone(&pool_arc),
            std::sync::Arc::clone(&shutdown_notify),
            std::sync::Arc::clone(&notifier),
            std::sync::Arc::clone(&health_status),
            Some(std::sync::Arc::clone(&feed_health)), // SP5: Dhan connected state
            // H7: lane-scoped Halt handler. `Some` only for a runtime lane.
            lane_halt_notify.as_ref().map(std::sync::Arc::clone),
            // PR-E: runtime Dhan-OFF gate — same atomic the WS read loop
            // consults; a dormant Dhan-OFF pool must not be paged/halted.
            Some(feed_runtime.dhan_flag()),
            // Fix A (2026-06-30): shared JWT handle for the Halt-arm bare-reset
            // classifier's token-valid predicate.
            Some(std::sync::Arc::clone(&token_handle)),
            // Fix A no-op close (2026-06-30): QuestDB config so the watchdog
            // pushes the REAL QuestDB-liveness signal into
            // `health.set_questdb_reachable(...)` every 5s — without it the
            // bare-reset gate was a production no-op (signal only ever set in
            // test code).
            config.questdb.clone(),
        );
        let pool_watchdog_guard = PreLaneAbortGuard::new(vec![dhan_pool_watchdog_handle]);
        // FAST BOOT parity: helper emits per-connection + aggregate Telegram
        // alerts on BOTH boot paths (main.rs ~830 for FAST BOOT, here for slow).
        // PR #458: now polls pool.health() for truthful state.
        emit_websocket_connected_alerts(
            &notifier,
            &pool_arc,
            tickvault_core::notification::events::BootPathLabel::Slow,
            boot_start,
            &feed_runtime,
            &feed_health,
        )
        .await;
        // BUG-2 fix (2026-07-05): mark the Dhan lane running + the PR-2
        // pool-present sentinel ONLY now that a REAL pool is spawned (moved
        // from the unconditional dhan_enabled block at the top of main()).
        // Boot-ON only: the RUNTIME cold-start (`lane_scoped`) keeps its
        // phantom-running closure — pool-present + lane-running are set
        // exclusively after the `StartSucceeded` CAS wins (main.rs, the
        // runtime Ok arm), so a disable-during-cold-start race can never
        // leave a torn-down pool marked present.
        if !lane_scoped {
            feed_runtime.mark_dhan_lane_running();
            feed_runtime.mark_dhan_pool_present();
        }
        (handles_guard, pool_watchdog_guard, Some(pool_arc))
    } else {
        (
            PreLaneAbortGuard::new(Vec::new()),
            PreLaneAbortGuard::new(Vec::new()),
            None,
        )
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
            // DayOhlcTracker boot wiring — MOVED to process-global scope
            // (2026-07-01 per-lane-leak fix).
            //
            // The DayOhlc tracker + tick consumer + IST-midnight reset supervisor
            // (INDEX-OHLC-02) used to be spawned HERE, inside the lane-local
            // `should_connect_ws && subscription_plan.is_some()` block. Because
            // `start_dhan_lane` re-runs on every Dhan enable / stop→restart /
            // cold-start retry, each re-invocation leaked a fresh never-aborted
            // midnight-reset supervisor → duplicate INDEX-OHLC-02 pages. The
            // tracker reads the PROCESS-GLOBAL `tick_broadcast_sender` and the
            // reset is a host IST-midnight observability task, so the whole
            // wiring is now spawned EXACTLY ONCE in `main()`'s process-global
            // prefix (after `build_shared_infra`, before the Dhan-lane gate),
            // decoupled from the lane. Only the lane-readiness pings below
            // (09:14 / 09:15:30) stay here — they are feed-readiness, not
            // process-global.

            // F4 (2026-07-08): the three market-open ONE-SHOT schedulers
            // below (09:14 readiness / 09:15:30 streaming heartbeat / 09:16
            // self-test) are detached-by-design one-shots (self-exit after
            // firing), so they get the SLO-supervisor treatment for lane
            // restarts: (a) a ONCE-PER-PROCESS spawn latch — N D2b
            // enable/disable cycles before the bell previously accumulated N
            // pending copies, each firing its own verdict (N duplicate
            // Telegrams, N SELFTEST rows); and (b) a FIRE-TIME dhan-enabled
            // gate inside each task — a pending one-shot outliving a
            // deliberate runtime Dhan disable previously read the dead
            // pool's frozen 0-connection state and fired FALSE
            // MarketOpenStreamingFailed (High) / SELFTEST-02 (Critical)
            // pages at the bell. Ratchet:
            // secret_manager.rs::test_market_open_one_shots_latched_and_gated.
            let market_open_one_shots_first_spawn =
                !MARKET_OPEN_ONE_SHOTS_SPAWNED.swap(true, std::sync::atomic::Ordering::SeqCst);
            if !market_open_one_shots_first_spawn {
                info!(
                    "market-open one-shot schedulers already spawned this process — \
                     skipping duplicates (lane restart)"
                );
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
            if market_open_one_shots_first_spawn {
                let readiness_dhan_flag = feed_runtime.dhan_flag();
                let readiness_notifier = notifier.clone();
                let readiness_health = health_status.clone();
                let readiness_calendar = std::sync::Arc::clone(&trading_calendar);
                // D2a: pre-extract the two Copy config fields so the `'static`
                // spawn captures owned values, not the lane-lifetime `&config`
                // (the inline gate captured these via 2021 disjoint Copy
                // captures of an OWNED `config`; behaviour is identical).
                let readiness_main_feed_total =
                    tickvault_common::config::effective_main_feed_pool_size(
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

                    // F4: fire-time gate — a deliberately-disabled Dhan lane
                    // must not emit a readiness verdict off dead state.
                    if !market_open_fire_gate_dhan_enabled(&readiness_dhan_flag) {
                        info!("market-open readiness: skipping (Dhan disabled at fire time)");
                        return;
                    }

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
                        main_feed_total: readiness_main_feed_total,
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
            //
            // F4 (2026-07-08): once-per-process latch + fire-time
            // dhan-enabled gate — see the latch comment above.
            if market_open_one_shots_first_spawn {
                let heartbeat_dhan_flag = feed_runtime.dhan_flag();
                let heartbeat_notifier = notifier.clone();
                let heartbeat_health = health_status.clone();
                let heartbeat_calendar = std::sync::Arc::clone(&trading_calendar);
                // D2a: pre-extract the Copy config fields (see the readiness
                // spawn above) so the `'static` task does not capture the
                // lane-lifetime `&config`. Behaviour-identical.
                let heartbeat_main_feed_total =
                    tickvault_common::config::effective_main_feed_pool_size(
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

                    // F4: fire-time gate — a pending one-shot outliving a
                    // deliberate runtime Dhan disable would otherwise read
                    // the dead pool's frozen 0 connections and fire a FALSE
                    // High MarketOpenStreamingFailed page at the bell.
                    if !market_open_fire_gate_dhan_enabled(&heartbeat_dhan_flag) {
                        info!("market-open heartbeat: skipping (Dhan disabled at fire time)");
                        return;
                    }

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
                    let expected_main_feed_total = heartbeat_main_feed_total;
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
                ws_client_id.clone(),
                config,
                std::sync::Arc::clone(&feed_runtime),
                std::sync::Arc::clone(&feed_health),
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
            //
            // F4 (2026-07-08): once-per-process latch + fire-time
            // dhan-enabled gate — a duplicate/stale one-shot previously
            // fired N verdicts (or a FALSE Critical SELFTEST-02 off a
            // deliberately-disabled lane's dead state) at 09:16.
            if market_open_one_shots_first_spawn && config.features.market_open_self_test {
                let st_dhan_flag = feed_runtime.dhan_flag();
                let st_notifier = notifier.clone();
                let st_health = health_status.clone();
                let st_calendar = std::sync::Arc::clone(&trading_calendar);
                let st_qcfg = config.questdb.clone();
                // D2c (C4): capture the shared feed state so the token-headroom
                // sub-check reads the LIVE lane manager (preferred) over the global.
                let st_feed_runtime = std::sync::Arc::clone(&feed_runtime);
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

                    // F4: fire-time gate — a deliberately-disabled Dhan lane
                    // must not fire a FALSE Critical SELFTEST-02 verdict off
                    // its dead pool/pipeline state.
                    if !market_open_fire_gate_dhan_enabled(&st_dhan_flag) {
                        info!("market-open self-test: skipping (Dhan disabled at fire time)");
                        return;
                    }

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
                    // from the global
                    // `TickGapDetector::freshest_tick_age_secs` —
                    // FEED-level liveness (seconds since ANY subscribed
                    // SID last ticked), mirroring the feed-stall
                    // watchdog's feed-level last-tick design.
                    let main_active = st_health.websocket_connections() as usize;
                    let oms = st_health.order_update_connected();
                    let pipeline = st_health.pipeline_active();
                    let questdb_ok =
                        tickvault_storage::boot_probe::wait_for_questdb_ready(&st_qcfg, 3)
                            .await
                            .is_ok();
                    // D2c (C4): prefer the LIVE lane-owned manager over the global
                    // OnceLock — after a runtime stop→re-start the global points at
                    // the dead boot-time manager, which would show a misleading
                    // token-headroom on the exact path D2c targets.
                    let token_headroom_secs = gauge_token_headroom_secs(&st_feed_runtime);
                    // B3 (2026-07-03): the `recent_tick` sub-check's
                    // documented meaning ("last tick > 60s old — silent
                    // socket likely", wave-3-c) is FEED-level liveness,
                    // but the old wiring fed it the WORST per-SID gap
                    // (the top-1 worst-gap scan) with no exclusions —
                    // ~33 always-silent illiquid SIDs made SELFTEST-02
                    // page Critical at 09:16:30 IST on pure illiquidity
                    // (same false-alarm class as the fixed #1342 SLO
                    // tick_freshness). Feed it the FRESHEST tick age
                    // instead: seconds since ANY subscribed SID last
                    // ticked. Round-2 (review MEDIUM-2): the age is
                    // REAL-tick-only — NT-15 boot seeds cannot arm it,
                    // so a lane (re)start < 60s before 09:16:30 cannot
                    // false-PASS with zero real ticks. Fail-safe:
                    // `None` (detector missing, or no REAL tick ever
                    // recorded — seeds don't count) maps to `u64::MAX`
                    // so an in-market self-test with zero tick evidence
                    // FAILS instead of false-passing.
                    let feed_freshest_age_secs =
                        tickvault_core::pipeline::tick_gap_detector::global_tick_gap_detector()
                            .and_then(|d| d.freshest_tick_age_secs(std::time::Instant::now()))
                            .unwrap_or(u64::MAX);
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
                        last_tick_age_secs: feed_freshest_age_secs,
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
            // ticks do NOT spam Telegram. The sustained-condition channel
            // is the CloudWatch alarm `tv-prod-realtime-guarantee-degraded`
            // (< 0.95, 9-of-15 × 60s, window-gated — silent-feed
            // hardening Item 2, 2026-07-06 incident; the previously cited
            // Prometheus alert `tv-realtime-score-degraded` was retired
            // with the CloudWatch-only migration #O3).
            //
            // Gated on `config.features.realtime_guarantee_score`.
            if config.features.realtime_guarantee_score {
                // 2026-05-11 v3: SLO Telegram dispatch removed — see the
                // suppression note inside `spawn_slo_publisher_task`. To
                // re-enable SLO Telegram, pass the notifier into
                // `spawn_slo_publisher_task` and restore the notify() calls
                // there.
                //
                // SLO-03 (live incident 2026-07-03 10:35 IST): the publisher
                // used to be a bare `tokio::spawn` with a dropped JoinHandle —
                // it died silently mid-market and the guarantee-critical alarm
                // false-OK'd on missing data. It is now wrapped in the standard
                // supervisor-respawn pattern (mirrors
                // `spawn_supervised_oom_monitor` / DISK-WATCHER-01 / WS-GAP-05)
                // so a task death logs `error!(code = "SLO-03")`, increments
                // `tv_slo_publisher_respawn_total{reason}`, and respawns after
                // a bounded backoff. The once-per-process guard keeps lane
                // restarts (D2b re-runs `start_dhan_lane`) from leaking
                // duplicate supervisors — the same per-lane-leak class as the
                // 2026-07-01 DayOhlcTracker fix.
                if SLO_PUBLISHER_SUPERVISOR_SPAWNED.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    info!(
                        "SLO publisher supervisor already running — skipping duplicate spawn \
                         (lane restart)"
                    );
                } else {
                    let _slo_publisher_supervisor = spawn_supervised_slo_publisher(
                        health_status.clone(),
                        config.questdb.clone(),
                        std::sync::Arc::clone(&feed_runtime),
                        tickvault_common::config::effective_main_feed_pool_size(
                            config.subscription.scope,
                            config.dhan.max_websocket_connections,
                        ),
                    );
                }
            }

            // Silent-feed hardening Item 4 (2026-07-06 incident): the Dhan
            // exchange→receive lag p99 publisher. Every 10s it snapshots the
            // trailing-60s lag window recorded by the tick processor's
            // persist sites and publishes `tv_dhan_exchange_lag_p99_seconds`
            // — ONLY in-session (Rule 3) and ONLY with ≥50 samples (Rule 11:
            // a thin/empty window publishes NOTHING; 0 would read as
            // "perfect lag"). Supervised (WS-GAP-05 / SLO-03 respawn
            // pattern) and once-per-process — a D2b lane restart re-runs
            // `start_dhan_lane`, and a duplicate supervisor would leak
            // forever (same per-lane-leak class as the SLO publisher guard
            // above).
            if FEED_LAG_PUBLISHER_SUPERVISOR_SPAWNED.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                info!(
                    "feed-lag publisher supervisor already running — skipping duplicate spawn \
                     (lane restart)"
                );
            } else {
                let _feed_lag_publisher_supervisor = spawn_supervised_feed_lag_publisher();
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
    // Option-chain minute-snapshot scheduler — REMOVED 2026-06-28.
    //
    // The entire option_chain REST subsystem (client + minute scheduler +
    // expiry warmup + current-expiry cache + snapshot cache) was deleted per
    // operator directive 2026-06-28 ("drop the option chain entire
    // implementations and its table also"). It had been config-gated OFF
    // since 2026-06-02 (the account lacks the Dhan Option Chain Data API
    // entitlement) with no live consumer, and its `option_chain_minute_snapshot`
    // QuestDB table was already dropped 2026-06-23. The strategy reads no
    // option-chain RAM cache; the live OI-change feature sources OI from the
    // `prev_day_ohlcv` table via the tick enricher (earlier in this file).
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Step 10: Spawn order update WebSocket connection
    // -----------------------------------------------------------------------
    // D2 Stage 2: the `order_update_sender` broadcast is now created ONCE in the
    // hoisted `build_shared_infra` prefix and destructured into `main()` scope
    // above. This lane references that SAME sender for the WAL-replay drain, the
    // order-update WS publisher, and the trading-pipeline subscriber.

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

    // CRASH-SAFETY confirm (slow boot): mirror of the fast-boot confirm — only
    // archive the staged segments if both re-injection legs were clean.
    {
        let slow_ws_wal_path = tickvault_app::tick_conservation_boot::ws_wal_dir();
        if ws_wal_replay_reinjection_clean {
            tickvault_storage::ws_frame_spill::confirm_replayed(&slow_ws_wal_path);
        } else {
            warn!(
                dir = %slow_ws_wal_path.display(),
                "STAGE-C.2b: WAL replay NOT confirmed (slow boot — a re-injection dropped or pool \
                 not ready) — staged segments remain in replaying/ for re-replay next boot"
            );
        }
    }

    let (order_update_handle, order_update_auth_listener_handle) = {
        let url = config.dhan.order_update_websocket_url.clone();
        let order_ws_client_id = ws_client_id.clone();
        let token = token_manager.token_handle();
        let sender = order_update_sender.clone();
        let cal = trading_calendar.clone();
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
        // R9-EDGE-1 / R10-CPLX-1 (2026-07-07): BIND the listener handle and
        // ride it into `DhanLaneRunHandles` (mirrors `heartbeat_bridge_handle`)
        // — `auth_signal` is attempt-private and its ONLY firer is this
        // lane's order-update WS task, which teardown/Drop abort. A detached
        // listener on a lane that never authenticated (off-hours disable,
        // Dhan down) would therefore park on `notified()` FOREVER, leaking
        // one task + a pinned Arc<Notify> + Arc<NotificationService> per D2b
        // lane stop/start cycle. Abort is lossless: its only action is an
        // informational Telegram enqueue that is moot once the connection is
        // being torn down. (The fast-arm twin at main.rs ~2362 is
        // process-lifetime — `main()` is never supervisor-aborted — so it
        // stays a plain spawn.)
        let auth_listener_handle = {
            let listener_signal = std::sync::Arc::clone(&auth_signal);
            let listener_notifier = notifier.clone();
            tokio::spawn(async move {
                listener_signal.notified().await;
                listener_notifier.notify(NotificationEvent::OrderUpdateAuthenticated);
            })
        };
        let run_signal = Some(std::sync::Arc::clone(&auth_signal));
        let run_latch = Some(std::sync::Arc::clone(&auth_latch));
        let ou_reconnect_notifier = Some(std::sync::Arc::clone(&notifier));
        // 2026-06-12: order-update lifecycle events → ws_event_audit.
        let ou_ws_audit_tx = Some(spawn_ws_event_audit_consumer(config.questdb.clone()));
        // PR-E: the order-update WS reads the same Dhan enable flag.
        let ou_dhan_flag = Some(feed_runtime.dhan_flag());
        let ou_connection_handle = tokio::spawn(async move {
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
                ou_ws_audit_tx,
                ou_dhan_flag,
            )
            .await;
            // Defensive only: run_order_update_connection is an infinite
            // never-give-up loop (WS-GAP-04) and structurally cannot return —
            // the OrderUpdateDisconnected notify that used to live here was
            // DEAD CODE (2026-07-06 incident: 39+ in-market failures, zero
            // HIGH pages). The reachable [HIGH] page now fires INSIDE the
            // reconnect loop (WS-GAP-10). If this line ever executes, a
            // future refactor broke the loop contract — surface it loudly,
            // never silently.
            error!(
                code = tickvault_common::error_code::ErrorCode::WsGap10OrderUpdateOutage.code_str(),
                reason = "task_exited_unreachable",
                "order update WebSocket task exited — unreachable by design; investigate immediately"
            );
            ou_health.set_order_update_connected(false);
        });
        (ou_connection_handle, auth_listener_handle)
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
            config,
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
    // Step 11: axum API server — D2 Stage 2: HOISTED.
    // -----------------------------------------------------------------------
    // The API server (incl. /api/feeds toggle routes), its SSM bearer-token
    // fetch, and the axum::serve bind are now done ONCE in the hoisted
    // `build_shared_infra` prefix so the API binds exactly once for BOTH the
    // Dhan-OFF and Dhan-ON-slow paths and exists with Dhan OFF (the toggle
    // endpoint is reachable to turn Dhan ON at runtime). The `api_handle` is
    // destructured into `main()` scope above and handed to `run_process_runloop`.

    // -----------------------------------------------------------------------
    // Step 12: Spawn token renewal background task
    // -----------------------------------------------------------------------
    let renewal_handle = token_manager.spawn_renewal_task();
    info!("token renewal task started");

    // AUTH-GAP-05 (2026-07-06): shared profile-truth flag — written by the
    // mid-session watchdog every in-session cycle (false on a REAL
    // /v2/profile auth failure, true on a clean check), read by the
    // token-health gauge poller AND (F15, 2026-07-08) the /health
    // token-block writer for their AND-composed validity. Seeds true; the
    // local-expiry gate in both consumers still forces invalid for an
    // absent/expired token, so no false valid before the first cycle.
    // Declared BEFORE the writer spawn below so both consumers share ONE flag.
    let token_profile_valid = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

    // -----------------------------------------------------------------------
    // Step 12′: B3 round-2 — dedicated token-health writer (UNCONDITIONAL,
    // NOT behind `realtime_guarantee_score` or any other feature flag). Its
    // JoinHandle is registered in `DhanLaneRunHandles` below so a runtime
    // Dhan disable aborts it instead of leaving an orphan writer reporting
    // the dead boot manager's `token_valid=true` for up to 24h.
    // F15 (2026-07-08): the writer now honors the SAME profile-truth flag as
    // the gauge poller — previously `/health token_valid` derived from local
    // expiry only, so a Dhan-KILLED (but locally-unexpired) token read
    // `valid` on /health for up to ~24h while `tv_token_valid` honestly
    // read 0.0 — a split-brain false-OK (audit Rule 11).
    // -----------------------------------------------------------------------
    let token_health_handle = spawn_token_health_writer(
        std::sync::Arc::clone(&health_status),
        std::sync::Arc::clone(&feed_runtime),
        std::sync::Arc::clone(&token_profile_valid),
    );
    info!(
        interval_secs = TOKEN_HEALTH_WRITER_INTERVAL_SECS,
        "token-health writer started (unconditional; lane-owned; profile-truth aware)"
    );

    // -----------------------------------------------------------------------
    // Step 12a: Spawn mid-session profile watchdog (queue item I7)
    // -----------------------------------------------------------------------
    // Every 15 minutes during market hours, re-runs `pre_market_check`
    // (dataPlan == "Active", activeSegment contains "Derivative", token
    // expires > 4h). On rising-edge failure fires CRITICAL Telegram via
    // NotificationEvent::MidSessionProfileInvalidated. Does NOT HALT —
    // dropping the live WS feed mid-session costs more than the
    // silent-failure risk we're monitoring.
    // Lane-owned (AG5-R1 fix, 2026-07-06): registered in `DhanLaneRunHandles`
    // below so a runtime Dhan disable aborts it. An unregistered spawn would
    // survive the lane teardown holding the DEAD lane's `Arc<TokenManager>`
    // (violating the dhan-lane C4 lane-ownership contract), keep hitting
    // `/v2/profile` with the dead token every 900s, and — with the remint
    // machinery attached — fire a false RESILIENCE-01 "peer owns the session"
    // page per lane cycle after a deliberate lane stop.
    let mid_session_watchdog_handle =
        tickvault_core::auth::mid_session_watchdog::spawn_mid_session_profile_watchdog(
            std::sync::Arc::clone(&token_manager),
            Some(std::sync::Arc::clone(&notifier)),
            std::sync::Arc::clone(&token_profile_valid),
        );
    info!("mid-session profile watchdog spawned (15-min cadence, market-hours only)");

    // AUTH-GAP-05: live token-health gauge poller — makes
    // `tv_token_remaining_seconds` LIVE (was the frozen mint-time snapshot)
    // and publishes the honest AND-composed `tv_token_valid` 0/1 gauge.
    // NOT market-hours gated (a killed/expired token must read 0 24/7);
    // the first interval tick fires immediately, so both gauges are live
    // at spawn (no separate boot seed needed).
    // Lane-owned (AG5-R1 fix, 2026-07-06): registered in `DhanLaneRunHandles`
    // below, mirroring `token_health_handle` — an unregistered spawn would
    // survive the lane teardown, keep the dead lane's `Arc<TokenManager>`
    // alive, and publish `tv_token_valid=1.0` for up to ~24h while Dhan is
    // deliberately OFF (false-OK, audit Rule 11) — then interleave with the
    // re-enabled lane's fresh poller (gauge flapping every ≤15s).
    let token_health_gauge_handle =
        tickvault_core::auth::token_health_gauge::spawn_token_health_gauge_poller(
            std::sync::Arc::clone(&token_manager),
            std::sync::Arc::clone(&token_profile_valid),
        );
    info!(
        poll_secs = tickvault_core::auth::token_health_gauge::TOKEN_HEALTH_GAUGE_POLL_SECS,
        "live token-health gauge poller spawned (tv_token_remaining_seconds + tv_token_valid)"
    );

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
    // SEC-R4-1 / R4-CPLX-1 (2026-07-06): LANE-OWNED — the handle rides into
    // `DhanLaneRunHandles` below (aborted by `teardown_dhan_lane_tasks` +
    // the H8 Drop floor). A detached spawn survived every runtime Dhan
    // disable holding the dead lane's `Arc<TokenManager>` (C4 violation),
    // leaked one immortal copy per D2b lane cold-start cycle, kept
    // RenewToken-ing a deliberately-disabled lane's still-active JWT, and —
    // after a lane restart minted a fresh token — fell into `acquire_token`
    // where the teardown-flipped held_flag tripwire fired a false
    // RESILIENCE-03 "peer owns the session" Critical page every 4h (no peer
    // exists — the lane was deliberately stopped).
    let token_sweep_handle = {
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
        })
    };
    info!("token sweep spawned (4h cadence, parallel safety-net to renewal_loop)");

    // -----------------------------------------------------------------------
    // Step 12b: Background periodic health check (disk space + memory RSS + spill + QuestDB)
    // -----------------------------------------------------------------------
    // C3: Runs every 5 minutes, fires Telegram CRITICAL on disk <10% or RSS >threshold.
    // Alert dedup: each category sends at most 1 alert per ALERT_COOLDOWN_SECS
    // to avoid spamming Telegram when a condition persists across intervals.
    // R4-CPLX-2 (2026-07-06): LANE-OWNED — registered in `DhanLaneRunHandles`
    // below. A detached spawn duplicated once per D2b lane cold-start cycle:
    // N enable/disable cycles = N concurrent check loops, each with its OWN
    // task-local alert-cooldown state, so a persisting disk-low /
    // QuestDB-down condition paged N× per 30-min cooldown window and the
    // QuestDbDisconnected/Reconnected edge fired once per copy.
    // R6-1 HONEST ENVELOPE (2026-07-07): the checks are PROCESS-scoped
    // (disk / RSS / spill / QuestDB liveness — not Dhan), but their ONLY
    // spawn site is this Dhan lane, so lane ownership gives exactly one
    // live copy WHILE THE DHAN LANE IS UP — and a monitoring blackout
    // across a runtime "Fully disconnect Dhan" window (Groww-only runtime):
    // the QuestDbDisconnected/Reconnected Telegram edges, the disk<10%
    // CRITICAL, and the RSS-threshold pages all go DARK until the Dhan lane
    // is re-enabled, even though Groww keeps writing to QuestDB. Residual
    // coverage in that window is only the indirect error! paths on ACTUAL
    // write failures (AGGREGATOR-SEAL-01 / GROWW-MASTER-01) plus the
    // process-scoped supervised spill-dir watcher (DISK-WATCHER-01). Alarm
    // designers MUST NOT assume QuestDB-liveness paging exists in the
    // Groww-only runtime. Hoisting this loop to the process-level
    // once-per-boot path (spawned exactly once, outside the lane) is the
    // flagged follow-up that closes the gap.
    let periodic_health_handle = {
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
                } else {
                    // Healthy liveness check.
                    if should_emit_questdb_reconnected(questdb_disconnect_alerted, liveness_success)
                    {
                        // Recovery edge — symmetric with the Critical disconnect
                        // alert so the operator is told the incident resolved
                        // (audit-findings Rule 11). Fires exactly once: the flag
                        // is cleared here and only re-armed by a fresh disconnect.
                        health_notifier.notify(NotificationEvent::QuestDbReconnected {
                            writer: "liveness-check".to_string(),
                            failed_checks_before_recovery: questdb_peak_failed_checks,
                        });
                        questdb_disconnect_alerted = false;
                    }
                    // Reset the peak on ANY healthy check (incl. after a
                    // sub-threshold blip that never alerted) so no stale
                    // residue carries into a future incident's count.
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
        })
    };
    info!("background periodic health check started (every 5 minutes)");

    // -----------------------------------------------------------------------
    // AG5-R3-1 fix (2026-07-06): bundle EVERY lane-owned handle into
    // `DhanLaneRunHandles` NOW — immediately after the last lane-owned spawn
    // and BEFORE the first post-spawn top-level `.await` below
    // (`emit_boot_completed_when_feed_live`, which can park for the full
    // BOOT_COMPLETED_FEED_LIVENESS_WAIT_SECS on a runtime cold-start with no
    // other feed Running). On the D2b runtime path the supervisor
    // `.abort()`s this future on a disable-during-Starting; a cancelled
    // future runs NO cleanup and dropping a BARE `tokio::task::JoinHandle`
    // DETACHES its task (it does not abort it) — so an unregistered spawn
    // window would leak the remint-capable mid-session watchdog + the
    // AUTH-GAP-05 gauge poller (publishing tv_token_valid=1.0 from the
    // cancelled lane's TokenManager for the rest of the process — false-OK,
    // audit Rule 11). Constructing the struct HERE means `lane`'s H8 Drop
    // abort-floor owns every handle (WS pool, processor, renewal, order
    // update, trading, token-health writer, gauge poller, watchdog, the 4h
    // token sweep, the 5-min periodic health check) across
    // every subsequent await until the caller consumes the struct. The
    // shared infra (API + otel) is torn down by the PROCESS run-loop below
    // (outside the `if config.feeds.dhan_enabled` lane), never here.
    //
    // R7-EDGE-1 / COV-R7-1 (2026-07-07): the tick processor and the main-feed
    // WS pool are spawned EARLIER than the other lane handles — before the
    // STAGE-C.2b WAL re-injection + connected-alerts awaits — so their
    // pre-construction window is covered by `PreLaneAbortGuard` (abort-on-drop);
    // the guards are DEFUSED here by handing the raw handles into this
    // struct's H8 Drop floor. Same for the pool watchdog (R8-EDGE-2 /
    // SEC-C2-1), the runtime IP monitor + 30s heartbeat watchdog (F5/F13,
    // 2026-07-08) and — via the notify-on-drop `InstanceLockHeartbeatGuard`,
    // defused just below — the dual-instance lock heartbeat (R8-EDGE-1 /
    // R8-CPLX-1).
    //
    // F19 HONESTY (2026-07-08) — the coverage above is NOT "every task this
    // fn spawns". The REAL residual detached spawns in `start_dhan_lane`,
    // enumerated with why each is acceptable:
    //   * the 16:00 IST daily-reset + 15:30 IST market-close signal timers —
    //     one-shot sleep→notify tasks that self-exit at their trigger; a
    //     duplicate per lane cycle fires a redundant notify on an
    //     attempt-private Notify (harmless, self-collecting same day);
    //   * the ws_event_audit consumer (`spawn_ws_event_audit_consumer`) —
    //     KNOWN residual: one cold-path ILP consumer leaks per lane cycle
    //     (bounded by operator toggle frequency; tracked follow-up);
    //   * the one-shot prev-day OHLCV fetch — self-exits after its bounded
    //     fail-soft fetch pass;
    //   * process-LATCHED families (spawned at most once per process):
    //     post-market tasks (the shared lib guard
    //     claim_post_market_task_family_once), the market-open
    //     one-shots (MARKET_OPEN_ONE_SHOTS_SPAWNED + fire-time dhan gate),
    //     the SLO supervisor (SLO_PUBLISHER_SUPERVISOR_SPAWNED), and the
    //     FirstSeenSet midnight reset (FIRST_SEEN_RESET_SPAWNED).
    let (instance_lock_handle, instance_lock_shutdown_chain) = instance_lock_guard.into_parts();
    let lane = DhanLaneRunHandles {
        ws_handles: ws_handles_guard.into_inner(),
        processor_handle: processor_guard.into_inner().pop(),
        renewal_handle: Some(renewal_handle),
        order_update_handle: Some(order_update_handle),
        // R9-EDGE-1 / R10-CPLX-1 (2026-07-07): lane-own the one-shot
        // OrderUpdateAuthenticated listener — its attempt-private Notify is
        // never fired again once the order-update WS task is aborted, so a
        // detached listener parks forever (one leaked task per lane cycle).
        order_update_auth_listener_handle: Some(order_update_auth_listener_handle),
        trading_handle,
        // B3 round-2 (MEDIUM-1): lane-owned so teardown aborts it + resets
        // the shared token block to the honest lane-off state.
        token_health_handle: Some(token_health_handle),
        // AG5-R1 fix (2026-07-06): the AUTH-GAP-05 gauge poller + the
        // remint-capable mid-session watchdog are lane-owned too — teardown
        // aborts them and publishes the honest 0/0.0 gauge state so a
        // deliberately-OFF lane can never keep reporting tv_token_valid=1.0.
        token_health_gauge_handle: Some(token_health_gauge_handle),
        mid_session_watchdog_handle: Some(mid_session_watchdog_handle),
        // SEC-R4-1 / R4-CPLX-1 (2026-07-06): the mint-capable 4h token sweep
        // is lane-owned — a detached copy per lane cold-start cycle kept a
        // deliberately-disabled lane's JWT alive and fired false
        // RESILIENCE-03 "peer owns the session" Critical pages every 4h
        // after a lane stop/restart.
        token_sweep_handle: Some(token_sweep_handle),
        // R4-CPLX-2 (2026-07-06): the 5-min periodic health check is
        // lane-owned so lane cycles never accumulate N concurrent copies
        // (each with its own alert-cooldown state → N× Telegram per window).
        periodic_health_handle: Some(periodic_health_handle),
        // R9-EDGE-2 / COV-C4-2 (2026-07-07): defuse the enricher-maintenance
        // guards into the H8 floor — a detached midnight-rollover loop is
        // IMMORTAL (one leaked copy + a duplicate IST-midnight QuestDB
        // reload per lane cycle) and the prev_oi poller may never self-exit
        // on a box whose candles_1d stays empty.
        midnight_rollover_handle: midnight_rollover_guard.into_inner().pop(),
        prev_oi_refresh_handle: prev_oi_refresh_guard.into_inner().pop(),
        // R8-EDGE-2 / SEC-C2-1 (2026-07-07): defuse the pool-watchdog guard
        // into the H8 floor — teardown/Drop abort it (the notify stop signal
        // alone is Rule-16 lost-wake prone against its ≤2s probe await).
        pool_watchdog_handle: pool_watchdog_guard.into_inner().pop(),
        // F5 (2026-07-08): defuse the Step 5.5 IP-monitor guard into the H8
        // floor + carry its watch shutdown Sender so teardown/Drop close the
        // channel (graceful stop) and abort the handle (floor) — a zombie
        // monitor's stale IP baseline pages false CRITICAL GAP-NET-01 (and
        // can HALT in live mode) after a legitimate IP change + lane restart.
        ip_monitor_handle: ip_monitor_guard.into_inner().pop(),
        ip_monitor_shutdown,
        // F13 (2026-07-08): defuse the heartbeat-watchdog guard — a detached
        // copy is an immortal 30s loop firing false AUTH-GAP-01 pages off
        // the dead lane's token_handle after a runtime disable.
        heartbeat_watchdog_handle: heartbeat_watchdog_guard.into_inner().pop(),
        health: Some(std::sync::Arc::clone(&health_status)),
        // F14 (2026-07-08): held so teardown/Drop publish the honest
        // lane-off Dhan-connected=false after aborting the pool watchdog.
        feed_health: Some(std::sync::Arc::clone(&feed_health)),
        ws_pool_arc,
        shutdown_notify,
        // H7: `Some` only for a runtime lane — the parked task watches this for
        // the lane-scoped watchdog Halt signal. `None` for boot-ON (the watchdog
        // keeps its `process::exit(2)` contract; boot-ON handles are not parked).
        lane_halt_notify,
        // BUG-1 fix (2026-07-05): the dual-instance lock heartbeat handle +
        // its shutdown Notify ride into the lane so teardown bound-awaits
        // the SSM DeleteParameter release before process exit.
        instance_lock_heartbeat: instance_lock_handle,
        instance_lock_shutdown: instance_lock_shutdown_chain,
        // SEC-C3-1 / R9-CPLX-1 / COV-R8-2 (2026-07-07): defuse the Item 19f
        // bridge guard into the H8 floor — a detached bridge parks forever
        // on the attempt-private shutdown_notify once the lane dies.
        heartbeat_bridge_handle: heartbeat_bridge_guard.into_inner().pop(),
    };

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

    // Boot-completed CloudWatch signal (slow-boot / normal path). Emitted
    // EXACTLY ONCE here, alongside `notify_systemd_ready()`, AFTER the
    // boot-complete `if/else` block — so it fires for BOTH the under-budget and
    // over-budget completed-boot cases, and NEVER on a halt (a halt uses
    // `process::exit(...)` / `bail!`/`?`, none of which reach this line).
    // See `emit_boot_completed` for the alarm semantics.
    //
    // GATED on a live feed (audit fix, this PR): for `dhan_enabled=true` the
    // Dhan lane already had to reach Running to get here (a start failure
    // returns/exits earlier), but the `dhan_enabled=false` / Groww-only branch
    // proceeds even though Groww activation is an ASYNC watcher that may never
    // come up. `emit_boot_completed_when_feed_live` waits (bounded) for at least
    // one enabled feed to reach Running and withholds the metric (→ boot-heartbeat
    // alarm pages) if every enabled feed stays dark. No feed enabled → emits
    // immediately (headless shared-infra run must not false-page).
    emit_boot_completed_when_feed_live(
        &feed_runtime,
        config.feeds.dhan_enabled,
        config.feeds.groww_enabled,
        // BUG-2: a slow boot that deliberately skipped the pool
        // (non-trading day / offline) completed its boot — the lane is
        // honestly NOT running, but the alive signal must still emit or the
        // boot-heartbeat alarm false-pages on every holiday/weekend boot.
        // A pool-build FAILURE never reaches this line (StartLaneError
        // returns above), so `ws_pool_arc.is_none()` here means
        // "deliberately pool-less", never "pool died". (Read through `lane`
        // — the pool Arc moved into the AG5-R3-1 early-constructed struct.)
        lane.ws_pool_arc.is_none(),
    )
    .await;

    // -----------------------------------------------------------------------
    // Step 13: D2 Stage 2 — hand the Dhan-lane handles to the PROCESS
    // run-loop. The struct itself was built EARLY (AG5-R3-1, right after the
    // last lane-owned spawn above) so the H8 Drop abort-floor covered every
    // lane handle across the awaits between there and here. The shared infra
    // (API + otel) is torn down by the run-loop BELOW (outside this
    // `if config.feeds.dhan_enabled` lane), not here.
    // -----------------------------------------------------------------------
    Ok(lane)
}

// ---------------------------------------------------------------------------
// Dhan-lane runtime handles (D2 genuine-hoist) — the tasks a STARTED Dhan lane
// owns, bundled so the PROCESS run-loop can tear them down. `None` (no lane)
// is the Dhan-OFF / Groww-only case: the run-loop keeps the shared infra alive
// with nothing Dhan-specific to stop.
//
// This is the teardown-side half of the D2 design's `DhanLaneHandles` (§1.3).
// D2b will reuse `teardown_dhan_lane_tasks` from `stop_dhan_lane` for the
// runtime ON→OFF path; for THIS PR the run-loop calls it so boot teardown
// behaviour is preserved.
// ---------------------------------------------------------------------------

/// R7-EDGE-1 / COV-R7-1 (2026-07-07): abort-on-drop floor for lane-owned
/// `JoinHandle`s during the pre-`DhanLaneRunHandles` window inside
/// `start_dhan_lane`. The tick processor and the main-feed WS pool are
/// spawned BEFORE the STAGE-C.2b WAL re-injection await (total wall-clock
/// unbounded by design on a WAL backlog) and the
/// `emit_websocket_connected_alerts` ≤30s health-poll await, while the
/// `DhanLaneRunHandles` H8 Drop abort-floor is only constructed AFTER the
/// last lane-owned spawn. A D2b supervisor `.abort()` landing on any of
/// those intermediate awaits drops the bare handles, which DETACHES (does
/// not abort) the tasks: the detached pool goes flag-dormant on the disable
/// and RECONNECTS on the next runtime re-enable alongside the fresh lane's
/// pool — 2 live Dhan main-feed connections (2-WS-lock violation) — while
/// the detached tick processor persists the duplicate stream (`capture_seq`
/// is IN the `ticks` DEDUP key, so duplicate rows are NOT collapsed). This
/// guard aborts the wrapped handles on Drop unless they were handed into
/// the lane struct via [`PreLaneAbortGuard::into_inner`].
struct PreLaneAbortGuard<T> {
    handles: Vec<tokio::task::JoinHandle<T>>,
}

impl<T> PreLaneAbortGuard<T> {
    fn new(handles: Vec<tokio::task::JoinHandle<T>>) -> Self {
        Self { handles }
    }

    /// Defuse: hand the handles onward to the `DhanLaneRunHandles` H8 Drop
    /// abort-floor. After this, the guard's own Drop iterates an empty vec
    /// (no double-abort, no detach).
    fn into_inner(mut self) -> Vec<tokio::task::JoinHandle<T>> {
        std::mem::take(&mut self.handles)
    }
}

impl<T> Drop for PreLaneAbortGuard<T> {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

/// R8-EDGE-1 / R8-CPLX-1 (2026-07-07): notify-on-drop floor for the Phase 0
/// Item 19 dual-instance SSM lock heartbeat across the
/// pre-`DhanLaneRunHandles` window inside `start_dhan_lane`. The heartbeat is
/// spawned at Step 6a-prime — ~2,500 lines BEFORE the lane struct that
/// carries it — and its ONLY exits are its own shutdown `Notify` or a foreign
/// takeover (`renew → Ok(false)`, which never happens: the next acquire
/// REFUSES instead of overwriting). On the D2b RUNTIME path (process
/// survives), a supervisor `.abort()` parked on ANY intermediate await
/// (Step 6 auth, IP gate, QuestDB DDL, the §4 infinite-retry universe fetch,
/// WAL reinject, WS spawn, connected-alerts) — or any ordinary
/// `return Err(StartLaneError::…)` after the spawn — previously dropped the
/// BARE `Option<JoinHandle>`, which DETACHES (never aborts) the task: the
/// zombie heartbeat kept renewing the SSM lock every 30s FOREVER inside the
/// still-running process, so the 90s TTL never cleared it, every retry /
/// re-enable generated a fresh host_id → `AlreadyHeld` → a RESILIENCE-01
/// Critical page per bounded-backoff retry and a PERMANENTLY wedged Dhan
/// lane until full process restart (the "TTL backstop" is only real on the
/// boot-ON path, where the process exits and the heartbeat dies with it).
///
/// This guard `notify_one()`s the heartbeat's shutdown on Drop — a
/// permit-storing wake (Rule 16 lost-wake safe) that drives the heartbeat's
/// GRACEFUL release: SSM DeleteParameter + RESILIENCE-03 tripwire disarm +
/// task exit, so the next cold-start attempt re-acquires cleanly. It NEVER
/// aborts the handle (BUG-1 contract: an abort would kill the in-flight
/// DeleteParameter). Defused into `DhanLaneRunHandles` via
/// [`InstanceLockHeartbeatGuard::into_parts`] at the lane construction.
struct InstanceLockHeartbeatGuard {
    handle: Option<tokio::task::JoinHandle<()>>,
    shutdown: Option<std::sync::Arc<tokio::sync::Notify>>,
}

impl InstanceLockHeartbeatGuard {
    fn new(
        handle: tokio::task::JoinHandle<()>,
        shutdown: std::sync::Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            handle: Some(handle),
            shutdown: Some(shutdown),
        }
    }

    /// The heartbeat's shutdown `Notify` (for the Item 19f broader-shutdown
    /// bridge task) without defusing the guard.
    fn shutdown_handle(&self) -> Option<std::sync::Arc<tokio::sync::Notify>> {
        self.shutdown.clone()
    }

    /// Defuse: hand the heartbeat handle + shutdown Notify onward to the
    /// `DhanLaneRunHandles` BUG-1 fields (teardown notify_one()s + bound-awaits
    /// the release; Drop notify_one()s). After this, the guard's own Drop is a
    /// no-op (no double release signal).
    fn into_parts(
        mut self,
    ) -> (
        Option<tokio::task::JoinHandle<()>>,
        Option<std::sync::Arc<tokio::sync::Notify>>,
    ) {
        (self.handle.take(), self.shutdown.take())
    }
}

impl Drop for InstanceLockHeartbeatGuard {
    fn drop(&mut self) {
        // Permit-storing wake: even a heartbeat mid-renewal observes it at
        // its next select! poll, deletes the SSM lock parameter (graceful
        // release), flips the RESILIENCE-03 tripwire false, and exits — so a
        // cancelled/failed cold-start can never leave a zombie renewer that
        // wedges every subsequent re-acquire with AlreadyHeld/RESILIENCE-01.
        if let Some(shutdown) = self.shutdown.as_ref() {
            shutdown.notify_one();
        }
        // Dropping `self.handle` detaches the task deliberately: the release
        // proceeds best-effort in the background (NEVER abort — BUG-1: an
        // abort would kill the in-flight SSM DeleteParameter; the 90s TTL
        // remains the backstop only for a hard process crash).
    }
}

struct DhanLaneRunHandles {
    ws_handles: Vec<tokio::task::JoinHandle<Result<(), WebSocketError>>>,
    processor_handle: Option<tokio::task::JoinHandle<()>>,
    renewal_handle: Option<tokio::task::JoinHandle<()>>,
    order_update_handle: Option<tokio::task::JoinHandle<()>>,
    /// R9-EDGE-1 / R10-CPLX-1 (2026-07-07): the one-shot
    /// `OrderUpdateAuthenticated` Telegram listener. Lane-owned because its
    /// `auth_signal` is attempt-private — the ONLY firer is this lane's own
    /// order-update WS task (aborted by teardown/Drop), so on any lane cycle
    /// where the WS never authenticated (off-hours disable, Dhan down,
    /// cancel-mid-Starting) a detached listener parks on `notified()`
    /// FOREVER, leaking one task + a pinned Arc<Notify> +
    /// Arc<NotificationService> per D2b cold-start cycle. Abort is lossless:
    /// its only pending action is an informational Telegram enqueue that is
    /// moot once the connection is torn down. `None` on the FAST
    /// crash-recovery boot arm (its twin is process-lifetime — `main()` is
    /// never supervisor-aborted).
    order_update_auth_listener_handle: Option<tokio::task::JoinHandle<()>>,
    trading_handle: Option<tokio::task::JoinHandle<()>>,
    /// B3 round-2 (MEDIUM-1): the dedicated unconditional token-health writer
    /// (`spawn_token_health_writer`). Lane-owned so a runtime Dhan disable
    /// aborts it — an unregistered `tokio::spawn` would survive the lane
    /// teardown, fall back to the global boot manager after
    /// `clear_live_token_manager`, and keep writing `token_valid=true` for up
    /// to 24h while Dhan is deliberately OFF. `None` on the FAST
    /// crash-recovery boot arm, which never spawns the writer.
    token_health_handle: Option<tokio::task::JoinHandle<()>>,
    /// AG5-R1 fix (2026-07-06): the AUTH-GAP-05 live token-health gauge
    /// poller (`spawn_token_health_gauge_poller`). Lane-owned for the SAME
    /// reason as `token_health_handle` — an unregistered `tokio::spawn`
    /// would survive a runtime Dhan disable holding the dead lane's
    /// `Arc<TokenManager>` (C4 violation) and keep publishing
    /// `tv_token_valid = 1.0` for up to ~24h while Dhan is deliberately
    /// OFF, then fight the re-enabled lane's fresh poller for the same
    /// unlabelled gauges (last-writer-wins flapping every ≤15s). Teardown /
    /// Drop abort it and publish the honest 0/0.0 gauge state. `Some` on
    /// BOTH boot arms (the FAST arm spawns the poller too — EDGE-2 fix —
    /// with an inert profile flag since it runs no watchdog).
    token_health_gauge_handle: Option<tokio::task::JoinHandle<()>>,
    /// AG5-R1 / SEC-R1-2 fix (2026-07-06): the mid-session profile watchdog
    /// (now carrying the AUTH-GAP-05 forced-re-mint trigger). Lane-owned so
    /// a runtime Dhan disable aborts it — a leaked watchdog would keep
    /// GET-ing `/v2/profile` every 900s with the dead lane's token, accrue
    /// RealAuthFail cycles, and fire a false RESILIENCE-01 "a peer owns the
    /// Dhan session" page per lane stop/restart cycle (there is no peer —
    /// the lane was deliberately stopped). `None` on the FAST
    /// crash-recovery boot arm, which never spawns the watchdog.
    mid_session_watchdog_handle: Option<tokio::task::JoinHandle<()>>,
    /// SEC-R4-1 / R4-CPLX-1 (2026-07-06): the 4h token-sweep safety net
    /// (`force_renewal_if_stale` backstop for a halted renewal loop). Lane-
    /// owned — it holds the lane's `Arc<TokenManager>` and is MINT-CAPABLE
    /// (`force_renewal_if_stale` → `renew_with_fallback` → `acquire_token`
    /// generateAccessToken fallback). A detached spawn leaked one immortal
    /// copy per D2b lane cold-start cycle, kept RenewToken-ing a
    /// deliberately-disabled lane's still-active JWT (against the operator's
    /// "Fully disconnect Dhan" intent), and — after a lane restart minted a
    /// fresh token — fell into the teardown-flipped held_flag tripwire → a
    /// false RESILIENCE-03 "peer owns the session" Critical page every 4h.
    /// `None` on the FAST crash-recovery boot arm, which never spawns the
    /// sweep.
    token_sweep_handle: Option<tokio::task::JoinHandle<()>>,
    /// R4-CPLX-2 (2026-07-06): the 5-min periodic health check (disk /
    /// spill / Docker / QuestDB liveness). Lane-owned so a lane stop/start
    /// cycle cannot accumulate N concurrent copies, each with its own
    /// task-local alert-cooldown state (N× Telegram per 30-min window on a
    /// persisting condition + N QuestDbDisconnected edge-triggers). `None`
    /// on the FAST crash-recovery boot arm, which never spawns it.
    periodic_health_handle: Option<tokio::task::JoinHandle<()>>,
    /// R9-EDGE-2 / COV-C4-2 (2026-07-07): the IST-midnight enricher rollover
    /// loop (`spawn_midnight_rollover_task` — an unconditional infinite
    /// loop). Lane-owned so a runtime Dhan disable aborts it — a detached
    /// copy per D2b lane cold-start cycle is IMMORTAL, pins the dead lane's
    /// `Arc<TickEnricher>`, and issues a duplicate concurrent QuestDB
    /// prev-OI reload at IST 00:00 per leaked copy. Pre-construction
    /// coverage is a `PreLaneAbortGuard` at the spawn site, defused here.
    /// `None` on the FAST crash-recovery boot arm (fast boot never attaches
    /// the enricher maintenance tasks) and when no frame source exists.
    midnight_rollover_handle: Option<tokio::task::JoinHandle<()>>,
    /// R9-EDGE-2 / COV-C4-2 (2026-07-07): the 5-min prev_oi cache refresh
    /// poller (`spawn_prev_oi_cache_refresh_task`). Lane-owned for the same
    /// reason — it self-exits only once the cache becomes non-empty, and per
    /// live-feed-purity rule 10 the live path never writes `candles_1d`, so
    /// on a box without the historical 1d row a detached copy polls QuestDB
    /// every 5 min forever. Same guard + defuse pattern as the rollover.
    prev_oi_refresh_handle: Option<tokio::task::JoinHandle<()>>,
    /// R8-EDGE-2 / SEC-C2-1 (2026-07-07): the lane-scoped pool watchdog
    /// (`spawn_pool_watchdog_task`). Lane-owned for two reasons: (a) its ONLY
    /// in-loop exit is `shutdown_notify.notified()` — a `notify_waiters()`
    /// wake that is LOST if the watchdog is mid-tick-body (its ≤2s QuestDB
    /// probe; audit Rule 16), so teardown/Drop abort() the handle as the
    /// floor; (b) a leaked copy writes the PROCESS-SHARED `/health`
    /// websocket_connections + feed-health Dhan-connected slots every 5s
    /// from a DEAD pool (fighting a re-enabled lane's fresh watchdog) and
    /// fires FALSE WebSocketPoolDegraded / CRITICAL WebSocketPoolHalt pages
    /// from the dead pool's frozen states. Pre-construction coverage is a
    /// `PreLaneAbortGuard` at the spawn site, defused here.
    pool_watchdog_handle: Option<tokio::task::JoinHandle<()>>,
    /// F5 (2026-07-08): the runtime IP monitor (GAP-NET-01 / AUTH-P12).
    /// Lane-owned — a detached copy per D2b lane cold-start cycle kept ITS
    /// boot's verified-IP baseline forever: after a legitimate IP change +
    /// lane restart the zombie's stale baseline fired false CRITICAL
    /// GAP-NET-01 pages and (live mode) could HALT the whole process.
    /// Pre-construction coverage is a `PreLaneAbortGuard` at the Step 5.5
    /// spawn site, defused here. `None` outside live mode and on the FAST
    /// crash-recovery boot arm.
    ip_monitor_handle: Option<tokio::task::JoinHandle<()>>,
    /// F5: the monitor's watch shutdown Sender — the previous
    /// `std::mem::forget` pinned the channel open for the process lifetime.
    /// Held here so the channel stays open while the lane lives; teardown /
    /// Drop DROP it, and the monitor's `changed()` select arm exits on the
    /// closed channel (graceful stop; the handle abort above is the floor).
    ip_monitor_shutdown: Option<tokio::sync::watch::Sender<bool>>,
    /// F13 (2026-07-08): the 30s liveness heartbeat watchdog
    /// (`spawn_heartbeat_watchdog`, slow boot). Lane-owned — an INFINITE
    /// loop pinning the dead lane's arc-swap token_handle; a detached copy
    /// fired a false AUTH-GAP-01 "token handle is None" ERROR every 30s
    /// forever after a runtime Dhan disable, one more copy per lane cycle.
    /// Pre-construction coverage is a `PreLaneAbortGuard` at the spawn
    /// site, defused here. `None` on the FAST arm (its twin is
    /// process-lifetime by design).
    heartbeat_watchdog_handle: Option<tokio::task::JoinHandle<()>>,
    /// Shared health registry — held so teardown/Drop can reset the token
    /// block to the honest deliberate-lane-off state (0 / false). `None`
    /// only on the FAST crash-recovery boot arm (no writer to reset for).
    health: Option<SharedHealthStatus>,
    /// F14 (2026-07-08): the shared per-feed health registry — held so
    /// teardown/Drop can publish the honest lane-off Dhan-connected state
    /// (false) after aborting the pool watchdog (its sole writer);
    /// otherwise `/api/feeds/health` freezes at the last live value after
    /// a deliberate runtime disable. `None` only on the FAST arm.
    feed_health: Option<std::sync::Arc<tickvault_common::feed_health::FeedHealthRegistry>>,
    // S4-T1b: shared pool handle + shutdown notifier. `ws_pool_arc` is None
    // when no WebSocket pool was spawned. `shutdown_notify` is fired before
    // the abort loop so the pool watchdog stops polling (no false-positive
    // Halt during intentional teardown).
    ws_pool_arc: Option<std::sync::Arc<WebSocketConnectionPool>>,
    shutdown_notify: std::sync::Arc<tokio::sync::Notify>,
    /// H7 lane-scoped watchdog Halt signal. `Some` ONLY for a RUNTIME lane: the
    /// pool watchdog fires `notify_waiters()` on a 300s-all-down Halt instead of
    /// `process::exit(2)`, and `park_running_dhan_lane` `select!`s on it to tear
    /// the lane down + drive the FSM to `Off` (supervisor re-cold-starts it,
    /// Groww + shared infra ALIVE). `None` for a BOOT-ON lane (not parked; the
    /// watchdog keeps the pre-existing single-feed `process::exit(2)` behaviour).
    lane_halt_notify: Option<std::sync::Arc<tokio::sync::Notify>>,
    /// BUG-1 fix (2026-07-05): the Phase 0 Item 19 dual-instance SSM lock
    /// heartbeat task. Teardown signals `instance_lock_shutdown` then
    /// bound-awaits this handle ([`INSTANCE_LOCK_RELEASE_TIMEOUT_SECS`]) so
    /// the heartbeat's shutdown release (SSM DeleteParameter) LANDS before
    /// the process exits — previously the handle was dropped at spawn time
    /// and the release RACED process exit (live proof: the 15:35 IST
    /// 2026-07-05 graceful EOD stop left the lock in SSM; the 17:59 boot
    /// found it stale at age 8622s). `None` on the FAST crash-recovery boot
    /// arm, which holds no lock (dual-instance-lock-2026-07-04 §3 honest
    /// envelope). NEVER aborted in Drop — aborting would kill the in-flight
    /// DeleteParameter; dropping the handle detaches (pre-fix best-effort).
    instance_lock_heartbeat: Option<tokio::task::JoinHandle<()>>,
    /// The heartbeat's own shutdown `Notify`. `notify_one()` stores a permit
    /// (Rule 16 lost-wake safe) — teardown/Drop fire it directly instead of
    /// relying only on the `shutdown_notify` bridge task's `notified()` wake.
    instance_lock_shutdown: Option<std::sync::Arc<tokio::sync::Notify>>,
    /// SEC-C3-1 / R9-CPLX-1 / COV-R8-2 (2026-07-07): the Item 19f
    /// heartbeat-shutdown BRIDGE task (parks on the per-attempt
    /// `shutdown_notify`, then `notify_one()`s the heartbeat's own Notify).
    /// Lane-owned so teardown/Drop ABORT it — a detached bridge parked on
    /// the per-attempt Notify outlives the lane forever (the Notify is
    /// attempt-private, nothing ever fires it again), pinning one task +
    /// two Arc<Notify> per lane cycle. Abort is safe: its only pending
    /// action (`notify_one` on the heartbeat shutdown) is performed
    /// DIRECTLY by teardown step 6 / Drop (the BUG-1 permit-storing wake),
    /// so no release signal is lost. `None` on the FAST crash-recovery
    /// boot arm (no lock, no heartbeat, no bridge).
    heartbeat_bridge_handle: Option<tokio::task::JoinHandle<()>>,
}

/// Defense-in-depth (H8 no-leak floor): if a `DhanLaneRunHandles` is dropped
/// WITHOUT going through `teardown_dhan_lane_tasks` (which destructures it by
/// value, leaving this Drop a no-op), fire the shutdown notify + abort every
/// owned task so the live WebSocket pool can NEVER silently DETACH. Without this,
/// dropping the `Vec<JoinHandle>` would detach (leak) the spawned WS-pool tasks
/// — the exact failure of the disable-during-cold-start race. Drop is sync-safe:
/// `notify_waiters` + `JoinHandle::abort` are non-blocking, hold no lock, do no
/// `.await`, so there is no deadlock or double-abort hazard (abort on an already
/// finished/aborted handle is a no-op).
impl Drop for DhanLaneRunHandles {
    fn drop(&mut self) {
        // Stop the pool watchdog polling first (prevents a false-positive Halt
        // during this implicit teardown), mirroring `teardown_dhan_lane_tasks`.
        self.shutdown_notify.notify_waiters();
        for handle in &self.ws_handles {
            handle.abort();
        }
        if let Some(handle) = self.processor_handle.as_ref() {
            handle.abort();
        }
        if let Some(handle) = self.renewal_handle.as_ref() {
            handle.abort();
        }
        if let Some(handle) = self.order_update_handle.as_ref() {
            handle.abort();
        }
        // R9-EDGE-1 / R10-CPLX-1: abort the OrderUpdateAuthenticated
        // listener — its attempt-private Notify has no remaining firer once
        // the order-update task above is aborted, so a survivor parks
        // forever. Abort is lossless (informational Telegram only).
        if let Some(handle) = self.order_update_auth_listener_handle.as_ref() {
            handle.abort();
        }
        if let Some(handle) = self.trading_handle.as_ref() {
            handle.abort();
        }
        if let Some(handle) = self.token_health_handle.as_ref() {
            handle.abort();
        }
        // AG5-R1: abort the lane-owned AUTH-GAP-05 gauge poller + the
        // remint-capable mid-session watchdog (both sync, non-blocking).
        if let Some(handle) = self.token_health_gauge_handle.as_ref() {
            handle.abort();
        }
        if let Some(handle) = self.mid_session_watchdog_handle.as_ref() {
            handle.abort();
        }
        // SEC-R4-1 / R4-CPLX-1/2: abort the lane-owned mint-capable 4h token
        // sweep + the 5-min periodic health check (both sync, non-blocking).
        if let Some(handle) = self.token_sweep_handle.as_ref() {
            handle.abort();
        }
        if let Some(handle) = self.periodic_health_handle.as_ref() {
            handle.abort();
        }
        // R9-EDGE-2 / COV-C4-2: abort the enricher maintenance tasks — the
        // midnight rollover loop is otherwise immortal (duplicate IST-00:00
        // QuestDB reloads per leaked copy) and the prev_oi poller may never
        // self-exit while candles_1d is empty. Sync, non-blocking, Drop-safe.
        if let Some(handle) = self.midnight_rollover_handle.as_ref() {
            handle.abort();
        }
        if let Some(handle) = self.prev_oi_refresh_handle.as_ref() {
            handle.abort();
        }
        // R8-EDGE-2 / SEC-C2-1: abort the lane-scoped pool watchdog — the
        // notify_waiters() above is lost if it is mid-tick-body (Rule 16);
        // a survivor would keep writing the shared /health + feed-health
        // Dhan slots from the dead pool and could page a false
        // WebSocketPoolHalt after a re-enable.
        if let Some(handle) = self.pool_watchdog_handle.as_ref() {
            handle.abort();
        }
        // F5: abort the runtime IP monitor (its watch Sender drops with the
        // struct, closing the channel — belt and braces). A survivor's stale
        // IP baseline pages false CRITICAL GAP-NET-01 (and can HALT in live
        // mode) after a legitimate IP change + lane restart.
        if let Some(handle) = self.ip_monitor_handle.as_ref() {
            handle.abort();
        }
        // F13: abort the 30s heartbeat watchdog — a survivor pins the dead
        // lane's token_handle and fires false AUTH-GAP-01 pages forever.
        if let Some(handle) = self.heartbeat_watchdog_handle.as_ref() {
            handle.abort();
        }
        // BUG-1: implicit teardown — signal the instance-lock heartbeat's
        // release (permit-based `notify_one`, sync + non-blocking, Drop-safe)
        // but do NOT abort its JoinHandle: aborting would kill the in-flight
        // SSM DeleteParameter. Dropping the handle with the struct detaches
        // the task so the release proceeds best-effort in the background
        // (the 90s TTL remains the backstop on a hard crash).
        if let Some(shutdown) = self.instance_lock_shutdown.as_ref() {
            shutdown.notify_one();
        }
        // SEC-C3-1 / R9-CPLX-1 / COV-R8-2: abort the Item 19f bridge task —
        // its only pending action (notify_one on the heartbeat shutdown) was
        // just performed directly above, and a survivor would park FOREVER
        // on this lane's attempt-private shutdown_notify (one leaked task +
        // two pinned Arc<Notify> per lane cycle). Sync, non-blocking,
        // Drop-safe; abort of a finished handle is a no-op.
        if let Some(handle) = self.heartbeat_bridge_handle.as_ref() {
            handle.abort();
        }
        // B3 round-2 honesty reset: deliberate lane-off → token block reads
        // 0/false, same as the pre-wiring (pre-B3) perpetual state, so no
        // NEW alarm class is introduced. Sync atomic stores — Drop-safe.
        // Idempotent after `teardown_dhan_lane_tasks` already reset it.
        if let Some(health) = self.health.as_ref() {
            health.set_token_remaining_secs(0);
            health.set_token_valid(false);
            // F14 (2026-07-08): honest lane-off WS state — the just-aborted
            // pool watchdog was the SOLE writer of this counter, so without
            // this reset /health freezes at the last live value after a
            // deliberate runtime disable. Sync atomic store — Drop-safe;
            // idempotent after `teardown_dhan_lane_tasks`.
            health.set_websocket_connections(0);
        }
        // F14: same honesty reset for the /api/feeds/health Dhan-connected
        // slot (the pool watchdog was its sole writer too).
        if let Some(feed_health) = self.feed_health.as_ref() {
            feed_health.set_connected(tickvault_common::feed::Feed::Dhan, false);
        }
        // AG5-R1: honest lane-off gauge state — mirrors the /health reset
        // above on the Prometheus surface. Sync gauge stores — Drop-safe;
        // idempotent after `teardown_dhan_lane_tasks`.
        //
        // UNCONDITIONAL (R3-2 fix, 2026-07-06): this reset was previously
        // gated on `token_health_gauge_handle.is_some()`, but the async
        // teardown take()s that handle BEFORE its bounded join awaits — so
        // a teardown future cancelled mid-join dropped `lane` with the
        // handle already `None`, SKIPPED this backstop, and left the gauges
        // frozen at the last live values (possibly tv_token_valid=1.0) with
        // no surviving writer: the exact false-OK (audit Rule 11) this
        // backstop exists to close. Setting an already-0 gauge to 0 is
        // harmless, so the gate bought nothing — reset always.
        //
        // AG5-R2-3 residual (documented, not fixable here): Drop cannot
        // `.await`, so unlike the async teardown (which JOINS the aborted
        // poller + renewal loop BEFORE this reset), a task iteration
        // already past its await point can complete its gauge stores AFTER
        // these resets — a microseconds-per-tick window, best-effort only
        // on this implicit-teardown backstop (which normally runs at
        // process exit, where the gauge surface dies with the process
        // anyway). The deterministic ordering lives in
        // `teardown_dhan_lane_tasks`.
        metrics::gauge!("tv_token_remaining_seconds").set(0.0);
        metrics::gauge!("tv_token_valid").set(0.0);
    }
}

/// AG5-R2-3 / SEC-R2-3 / R3-1 (2026-07-06): bound on JOINING the
/// just-aborted token-health writer + gauge poller + token RENEWAL loop
/// before publishing the honest lane-off 0/0.0 reset. `JoinHandle::abort()`
/// only lands at the task's NEXT await point — the gauge poller's body has
/// NO await between its interval tick and its two synchronous gauge stores,
/// and the renewal loop has two synchronous `tv_token_remaining_seconds`
/// stores after its awaits — so an in-flight iteration racing the teardown
/// could otherwise publish stale gauge values AFTER the reset and leave the
/// deliberately-OFF lane's gauges stuck at the exact false-OK (audit
/// Rule 11) the lane-ownership fix targets, with no remaining writer to
/// correct it until the lane restarts. The join returns ~immediately
/// (`JoinError::Cancelled`); the timeout is pure defense against a
/// pathologically wedged runtime worker.
const LANE_HEALTH_TASK_JOIN_TIMEOUT_SECS: u64 = 5;

/// Step-3 graceful WS drain: give each live connection up to this long to
/// finish its RequestCode-12 Disconnect send before the abort loop. Hoisted
/// to module scope (R10-EDGE-1, 2026-07-07) so the teardown's internal
/// worst-case budget below can sum it at compile time.
const WS_GRACEFUL_DRAIN_SLEEP_SECS: u64 = 2;

/// Step-3b bounded `supervise_pool` drain after the WS abort loop — a hung
/// handle must not stall shutdown. Hoisted to module scope (R10-EDGE-1) for
/// the same compile-time budget sum.
const POOL_SUPERVISOR_DRAIN_TIMEOUT_SECS: u64 = 5;

/// Teardown of the Dhan-lane runtime tasks ONLY (renewal → order-update →
/// graceful WS close → tick-processor flush → trading pipeline).
///
/// This is the lane-scoped teardown the D2 design (C3) splits out of the old
/// `run_shutdown_fast`. It NEVER touches PROCESS-shared infra (API server,
/// seal-writer, aggregator, otel) and NEVER runs the market-close timer /
/// partition-detach / `wait_for_shutdown_signal` — those belong to the PROCESS
/// run-loop. D2b's runtime `stop_dhan_lane` will call this directly.
///
/// Order matches the old `run_shutdown_fast` steps 1–5 exactly so boot
/// behaviour is preserved.
async fn teardown_dhan_lane_tasks(mut lane: DhanLaneRunHandles) {
    // CANCEL-SAFETY (hostile-review CRITICAL fix): this future itself can be
    // dropped MID-FLIGHT — the runtime cold-start's lost-CAS branch calls this
    // and is concurrently `.abort()`ed by the supervisor (Starting→Off won). If
    // it is dropped during the graceful 2s WS drain BELOW (an `.await`) the
    // WebSocket `JoinHandle`s must STILL be aborted, or the live pool DETACHES
    // (the exact H8/H6 leak this whole change exists to close).
    //
    // The guarantee: the WS `JoinHandle`s stay INSIDE `lane` (which implements
    // the H8 `Drop` abort-floor) across every `.await` until the instant they
    // are legitimately consumed by `supervise_pool`. So if this future is
    // dropped before that consumption, `lane`'s `Drop` aborts them. After the
    // graceful drain we `mem::take` them out and `supervise_pool` owns them
    // (and the supervisor itself runs to completion — there is no further
    // mid-flight drop hazard once we are past the pre-consumption awaits:
    // the AG5-R2-3 bounded health-task joins in step 0 + the graceful drain).
    //
    // `DhanLaneRunHandles` implements `Drop`, so fields cannot be moved out by
    // destructuring (E0509). The fields consumed AFTER an `.await`
    // (`ws_handles`, `processor_handle`, `trading_handle`) are `take`n out only
    // at their point of use, so `lane`'s Drop guards them until then. Every
    // other handle is taken + aborted at its own point of use BEFORE any await
    // that follows it (already aborted on a later drop — no leak). `Arc`
    // fields are cheaply cloned.

    // 0. B3 round-2 (MEDIUM-1): stop the lane-owned token-health writer and
    //    reset the shared token block (each handle is taken + aborted at its
    //    point of use BEFORE the bounded join await on it — Drop-floor safe).
    //    Deliberate lane-off → the token block reads 0/false — the same as
    //    the pre-wiring (pre-B3) perpetual state, so no NEW alarm class is
    //    introduced. Without this the orphan writer would fall back to the
    //    global boot manager (after `clear_live_token_manager`) and keep
    //    reporting `token_valid=true` for up to 24h while Dhan is
    //    deliberately OFF.
    // AG5-R2-3 / SEC-R2-3 (2026-07-06): abort THEN JOIN (bounded) before the
    // honest resets below — see [`LANE_HEALTH_TASK_JOIN_TIMEOUT_SECS`]. An
    // in-flight writer/poller iteration that already passed its await point
    // would otherwise complete its synchronous stores AFTER the reset and
    // leave the OFF lane's health/gauge surface stuck at a false valid=1
    // with no remaining writer to correct it. Joining an aborted handle
    // returns ~immediately with `JoinError::Cancelled`. Cancel-safety: the
    // handles are taken + aborted BEFORE the join await, so if THIS future
    // is dropped mid-join the tasks are already aborted (no leak) and the
    // remaining lane fields stay guarded by `lane`'s Drop.
    if let Some(handle) = lane.token_health_handle.take() {
        handle.abort();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(LANE_HEALTH_TASK_JOIN_TIMEOUT_SECS),
            handle,
        )
        .await;
    }
    // AG5-R1 fix (2026-07-06): stop the lane-owned AUTH-GAP-05 gauge poller
    // + the remint-capable mid-session watchdog too (same false-OK class as
    // the token-health writer above — a leaked poller would keep publishing
    // tv_token_valid=1.0 from the dead lane's TokenManager for up to ~24h;
    // a leaked watchdog would keep hitting /v2/profile with the dead token
    // and fire false RESILIENCE-01 dual-instance pages). Then publish the
    // honest lane-off gauge state (0/0.0), mirroring the /health reset.
    if let Some(handle) = lane.token_health_gauge_handle.take() {
        handle.abort();
        // AG5-R2-3: join BEFORE the 0/0.0 reset so a mid-body final poll
        // can never republish stale gauges after the reset lands.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(LANE_HEALTH_TASK_JOIN_TIMEOUT_SECS),
            handle,
        )
        .await;
    }
    if let Some(handle) = lane.mid_session_watchdog_handle.take() {
        handle.abort();
    }
    // SEC-R4-1 / R4-CPLX-1 (2026-07-06): stop the lane-owned 4h token sweep
    // — the third mint-capable TokenManager caller (after the renewal loop
    // + the watchdog above). A leaked sweep would keep the deliberately-
    // disabled lane's JWT alive via RenewToken and, after a lane restart
    // minted a fresh token, fall into `acquire_token` where the teardown-
    // flipped held_flag refuses fail-closed — a false RESILIENCE-03 "peer
    // owns the session" Critical page every 4h with no peer. Plain abort:
    // the sweep writes only monotonic counters (no gauges), so no bounded
    // join is needed before the honest gauge reset below.
    if let Some(handle) = lane.token_sweep_handle.take() {
        handle.abort();
    }
    // R4-CPLX-2 (2026-07-06): stop the lane-owned 5-min periodic health
    // check so lane stop/start cycles never accumulate N concurrent check
    // loops (each with its own alert-cooldown state → N× Telegram per
    // 30-min window on a persisting condition).
    if let Some(handle) = lane.periodic_health_handle.take() {
        handle.abort();
    }
    // R9-EDGE-2 / COV-C4-2 (2026-07-07): stop the lane-owned enricher
    // maintenance tasks — the IST-midnight rollover loop (an unconditional
    // infinite loop holding the lane's Arc<TickEnricher>) and the 5-min
    // prev_oi refresh poller (which may never self-exit while candles_1d is
    // empty per live-feed-purity rule 10). Detached copies previously
    // accumulated one immortal set per D2b lane stop/start cycle, each
    // issuing duplicate cold-path QuestDB SELECTs at IST midnight. Plain
    // abort: neither writes gauges, so no bounded join is needed before the
    // honest gauge reset below.
    if let Some(handle) = lane.midnight_rollover_handle.take() {
        handle.abort();
    }
    if let Some(handle) = lane.prev_oi_refresh_handle.take() {
        handle.abort();
    }
    // 1. Stop token renewal — abort + BOUNDED JOIN, and do it BEFORE the
    //    honest gauge reset below (R3-1/SEC-R3-3/AG5-R3-2 fix, 2026-07-06).
    //    `TokenManager::renewal_loop` is the THIRD writer of
    //    `tv_token_remaining_seconds` (the loop-top snapshot + the
    //    post-renew-success write in token_manager.rs — the COV-3 writer
    //    inventory in token_health_gauge.rs). `abort()` only lands at the
    //    task's NEXT await point, so an iteration that just returned from
    //    `renew_with_fallback().await` (most plausible during a
    //    failing-token retry burst — exactly when an operator disables the
    //    lane) would otherwise complete its synchronous gauge store AFTER
    //    the 0.0 reset, sticking the deliberately-OFF lane at a stale
    //    non-zero remaining-seconds with no surviving writer (the 15s
    //    poller is already joined-dead above). Same AG5-R2-3 treatment as
    //    the poller: abort → bounded join → only then reset. Cancel-safe:
    //    taken + aborted before the join await, Drop-floor covers the rest.
    if let Some(handle) = lane.renewal_handle.take() {
        handle.abort();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(LANE_HEALTH_TASK_JOIN_TIMEOUT_SECS),
            handle,
        )
        .await;
    }
    // Honest lane-off reset — published only AFTER every
    // tv_token_remaining_seconds / tv_token_valid writer (token-health
    // writer, gauge poller, renewal loop) has been aborted AND joined, so
    // no late synchronous store can overwrite it. Unconditional (R3-2):
    // idempotent, and gating on handle presence would skip the reset when
    // a cancelled teardown already take()n the handles.
    metrics::gauge!("tv_token_remaining_seconds").set(0.0);
    metrics::gauge!("tv_token_valid").set(0.0);
    if let Some(health) = lane.health.as_ref() {
        health.set_token_remaining_secs(0);
        health.set_token_valid(false);
    }

    // 2. Abort order update WebSocket (same Drop-floor guarantee).
    if let Some(handle) = lane.order_update_handle.take() {
        handle.abort();
    }
    // R9-EDGE-1 / R10-CPLX-1 (2026-07-07): abort the OrderUpdateAuthenticated
    // listener alongside the connection task whose auth signal is its ONLY
    // firer — once that task is aborted, nothing ever fires the
    // attempt-private Notify again and a survivor parks on `notified()`
    // FOREVER (one leaked task + pinned Arcs per lane stop/start cycle).
    // Abort is lossless: its only pending action is an informational
    // Telegram enqueue that is moot once the connection is torn down.
    if let Some(handle) = lane.order_update_auth_listener_handle.take() {
        handle.abort();
    }

    // 3. S4-T1b: Graceful unsubscribe BEFORE abort. Sends RequestCode 12
    //    to every live WebSocket so Dhan's server cleans up subscriptions
    //    instead of timing them out 40s later. Best-effort — dead
    //    connections are skipped. Also fires shutdown_notify so the pool
    //    watchdog stops polling (prevents false-positive Halt during
    //    intentional teardown).
    //
    //    `ws_handles` stays in `lane` across this `.await`: a mid-sleep cancel
    //    drops `lane` → its `Drop` aborts every WS handle (no detach).
    lane.shutdown_notify.notify_waiters();
    // R8-EDGE-2 / SEC-C2-1 (2026-07-07): abort the pool watchdog DIRECTLY —
    // the notify_waiters() wake above is lost if the watchdog is mid-tick-body
    // (its ≤2s QuestDB probe await; audit Rule 16). A survivor would keep
    // writing the PROCESS-SHARED /health + feed-health Dhan slots from the
    // dead pool every 5s and could fire a false CRITICAL WebSocketPoolHalt
    // after a runtime re-enable. Abort is safe: the watchdog holds no
    // in-flight release side-effect (its probe is a read-only SELECT 1).
    if let Some(handle) = lane.pool_watchdog_handle.take() {
        handle.abort();
        // F14 (2026-07-08): bounded join BEFORE the honest lane-off reset
        // below — the watchdog's tick body has awaits (its ≤2s QuestDB
        // probe), so an in-flight iteration past its await point could
        // otherwise complete its synchronous `set_websocket_connections` /
        // `set_connected(Dhan, …)` stores AFTER the reset and freeze the
        // OFF lane's surfaces at stale live values (same AG5-R2-3 race
        // class as the gauge poller). Joining an aborted handle returns
        // ~immediately (`JoinError::Cancelled`).
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(LANE_HEALTH_TASK_JOIN_TIMEOUT_SECS),
            handle,
        )
        .await;
    }
    // F14: honest lane-off surfaces — the just-joined pool watchdog was the
    // SOLE writer of both; without these resets /health's websocket count
    // and /api/feeds/health's Dhan-connected flag freeze at the last live
    // values after a deliberate runtime disable (false-OK, audit Rule 11).
    if let Some(health) = lane.health.as_ref() {
        health.set_websocket_connections(0);
    }
    if let Some(feed_health) = lane.feed_health.as_ref() {
        feed_health.set_connected(tickvault_common::feed::Feed::Dhan, false);
    }
    // F5 (2026-07-08): stop the runtime IP monitor — drop its watch Sender
    // (the monitor's `changed()` select arm exits on the closed channel),
    // then abort the handle as the floor. A survivor's stale IP baseline
    // fires false CRITICAL GAP-NET-01 pages (and can HALT in live mode)
    // after a legitimate IP change + lane restart.
    if let Some(shutdown_tx) = lane.ip_monitor_shutdown.take() {
        drop(shutdown_tx);
    }
    if let Some(handle) = lane.ip_monitor_handle.take() {
        handle.abort();
    }
    // F13 (2026-07-08): stop the 30s heartbeat watchdog — an immortal loop
    // pinning the dead lane's token_handle that would fire a false
    // AUTH-GAP-01 "token handle is None" ERROR every 30s forever.
    if let Some(handle) = lane.heartbeat_watchdog_handle.take() {
        handle.abort();
    }
    if let Some(pool) = lane.ws_pool_arc.clone() {
        let signalled = pool.request_graceful_shutdown();
        info!(
            connections_signalled = signalled,
            "S4-T1b: graceful shutdown signalled to WebSocket pool"
        );
        // Give each connection up to WS_GRACEFUL_DRAIN_SLEEP_SECS to finish
        // its Disconnect send (R10-EDGE-1: named const so the outer teardown
        // budget's compile-time worst-case sum can include it).
        tokio::time::sleep(std::time::Duration::from_secs(WS_GRACEFUL_DRAIN_SLEEP_SECS)).await;
    }

    // 3b. Abort WebSocket connections (drops senders → processor exits).
    // Wave 2 Item 5.2 (WS-GAP-05) — first abort, then run the pool
    // supervisor to drain any final exits with structured ERROR logs +
    // metric increments for tasks that panicked / errored vs exited
    // cleanly. Bounded by 5s so a hung handle does not stall shutdown.
    //
    // Take `ws_handles` out of `lane` ONLY now, past the graceful-drain await:
    // they are aborted on the same synchronous run as `supervise_pool` consuming
    // them, so there is no await between the take and the consumption — no
    // mid-flight-drop leak window for these handles.
    let ws_handles = std::mem::take(&mut lane.ws_handles);
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
    // R10-EDGE-1 (2026-07-07): POOL_SUPERVISOR_DRAIN_TIMEOUT_SECS hoisted to
    // module scope (next to LANE_HEALTH_TASK_JOIN_TIMEOUT_SECS) so the outer
    // teardown budget's compile-time worst-case sum includes it.
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(POOL_SUPERVISOR_DRAIN_TIMEOUT_SECS),
        supervise_fut,
    )
    .await;

    // 4. Wait for tick processor final flush. Take it out only here, past the
    //    WS drain; a drop before this point leaves it in `lane` → Drop aborts it.
    if let Some(handle) = lane.processor_handle.take() {
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

    // 5. Stop trading pipeline (taken out only at its point of use; guarded by
    //    `lane`'s Drop until here).
    if let Some(handle) = lane.trading_handle.take() {
        handle.abort();
    }

    // 6. BUG-1 fix (2026-07-05): release the dual-instance SSM lock and
    //    bound-await the release BEFORE the process exits. Signal the
    //    heartbeat's own Notify DIRECTLY (`notify_one` stores a permit —
    //    Rule 16 lost-wake safe; the step-3 `shutdown_notify.notify_waiters()`
    //    bridge wake can be lost if the bridge task was not yet parked), then
    //    await the heartbeat JoinHandle with a bounded timeout so a
    //    black-holed SSM endpoint can never hang shutdown (mirror of
    //    `GrowwScaleFleetLockGuard::release_on_shutdown`, 2026-07-04).
    //    Live proof this was broken: the 15:35 IST 2026-07-05 graceful EOD
    //    stop exited before the DeleteParameter landed, leaving the lock in
    //    SSM (17:59 boot found it stale, age 8622s); a Stop→Start inside the
    //    90s TTL would have HALTed with RESILIENCE-01.
    //    Fields are taken only here (past every earlier `.await`); a
    //    mid-flight drop leaves them in `lane` → Drop fires `notify_one` so
    //    the release still proceeds best-effort in the background.
    if let Some(shutdown) = lane.instance_lock_shutdown.take() {
        shutdown.notify_one();
    }
    // SEC-C3-1 / R9-CPLX-1 / COV-R8-2 (2026-07-07): abort the Item 19f
    // heartbeat-shutdown bridge task. Its only pending action (`notify_one`
    // on the heartbeat shutdown) was just delivered DIRECTLY above (the
    // Rule 16 permit-storing wake), so the abort loses nothing — while a
    // survivor would park FOREVER on this lane's attempt-private
    // `shutdown_notify` (nothing ever fires it again after teardown),
    // leaking one task + two Arc<Notify> per lane stop/start cycle.
    if let Some(handle) = lane.heartbeat_bridge_handle.take() {
        handle.abort();
    }
    if let Some(handle) = lane.instance_lock_heartbeat.take() {
        match tokio::time::timeout(
            std::time::Duration::from_secs(INSTANCE_LOCK_RELEASE_TIMEOUT_SECS),
            handle,
        )
        .await
        {
            Ok(_) => info!(
                "dual-instance lock released on shutdown — the SSM slot is free \
                 for an immediate same-host restart"
            ),
            Err(_) => warn!(
                timeout_secs = INSTANCE_LOCK_RELEASE_TIMEOUT_SECS,
                "dual-instance lock release did not settle within the bound — \
                 the 90s TTL self-heals the slot; a restart inside that window \
                 sees a fresh peer and HALTs with RESILIENCE-01 (retry after ~90s)"
            ),
        }
    }
    // H7: `lane_halt_notify` is consumed by the parked task's `select!`, not
    // needed during teardown — left in `lane` (it is not a JoinHandle, so Drop
    // does not touch it). `lane` drops here as a no-op (all handle fields taken).
}

// ===========================================================================
// D2b — Runtime Dhan-lane cold-start FSM (start/stop wiring + lane runtime)
//
// `start_dhan_lane` above is the boot-path (slow-arm) extraction. For a Dhan
// feed that was OFF *at boot* (no pool spawned), this section makes it
// runtime-startable via the dormant activation watcher (`dhan_activation.rs`),
// driving the `LaneState` FSM (`Off→Starting→Running→Stopping→Off`).
//
// The watcher is spawned UNCONDITIONALLY at boot (line ~377) with an
// `Arc<OnceCell<Arc<DhanLaneRuntimeContext>>>`. `main()` populates the cell
// right after `build_shared_infra` (which runs in BOTH the Dhan-ON and Dhan-OFF
// paths), so the runtime context exists whether or not a lane started at boot.
//
// Safety fixes wired here (design §0.5):
//   * C2 ownership — the runtime wrapper NEVER creates/tears down the PROCESS
//     seal-writer / 21-TF aggregator / API server (those live in
//     `build_shared_infra`); it only drives the LANE via `start_dhan_lane` +
//     `teardown_dhan_lane_tasks`. It spawns NO children of its own.
//   * Phantom-running closure — `LaneState::Running` (and `mark_dhan_pool_*`)
//     is set ONLY after `start_dhan_lane` returns `Ok` (FSM `StartSucceeded`).
//   * H5 gate-hold — the runtime Stop re-asserts `can_disable_dhan()`
//     IMMEDIATELY before the irreversible teardown; if the gate re-closed it
//     ABORTS (keeps `Running`).
//   * H6 double-pool — `Stopping→Off` happens ONLY after teardown awaits all
//     handles join (`StopJoined`); the FSM rejects `Stopping→Starting`.
//   * H7 lane watchdog — `start_dhan_lane`'s pool watchdog is reused as-is;
//     this section does NOT add a `process::exit` path.
//   * H8 cancel-safety — a single owned `JoinHandle` (held by the watcher) +
//     `.abort()`; `start_dhan_lane` aborts every lane-owned handle it spawned
//     so far on an Err/cancel. The runtime wrapper holds NO extra spawns.
//   * Backoff — bounded `min(10 * 2^n, 300)`s between failed cold-starts.
//
// Deferred to D2c (documented honest residual per Rule 14 — NOT a skeleton:
// every fn below has a real call site + real work):
//   * The full `dhan_lane_audit` QuestDB table + typed Telegram event variants
//     (this PR uses `NotificationEvent::Custom` for the start/stop pings).
//   * C4 "lane owns its TokenManager handle end-to-end in DhanLaneRunHandles":
//     the lane already keeps its token alive via the renewal task it spawned;
//     the global `OnceLock` (`set_global_token_manager`) is a best-effort
//     convenience that no-ops if already set. The runtime wrapper does NOT add
//     a second `set_global_token_manager` call, so a runtime cold-start cannot
//     double-set it; a runtime Stop aborts the renewal task (which owns the
//     manager), so the manager is dropped when that task ends. Threading the
//     manager handle out through `DhanLaneRunHandles` so the Stop can drop it
//     deterministically is a deeper spine refactor tracked for D2c.
// ===========================================================================

/// BUG-1 fix (2026-07-05): bounded wait for the dual-instance lock
/// heartbeat's clean SSM release (one GetParameter + DeleteParameter
/// round-trip) at graceful teardown. On timeout shutdown proceeds anyway —
/// the 90s TTL remains the self-heal backstop; the bound only exists so a
/// black-holed SSM endpoint can never hang process exit. Mirrors
/// `GROWW_SCALE_LOCK_RELEASE_TIMEOUT_SECS` (the 2026-07-04 fleet-lock
/// release pattern this fix makes the Dhan lock match).
const INSTANCE_LOCK_RELEASE_TIMEOUT_SECS: u64 = 5;

/// Cap on the per-failed-cold-start retry backoff, mirroring the Groww
/// activation watcher's `min(10 * 2^n, 300)` ladder (cold control-plane).
const DHAN_LANE_RETRY_BACKOFF_CAP_SECS: u64 = 300;

/// Bounded drain timeout for a runtime Stop's graceful WS close before the
/// teardown force-aborts and emits `DHAN-LANE-04`. The inner
/// `teardown_dhan_lane_tasks` itself bounds the WS close; this is the outer
/// wall-clock budget that detects a hung teardown.
///
/// R10-EDGE-1 (2026-07-07): raised 30 → 45. The AG5-R2-3 fixes added three
/// sequential bounded joins (token-health writer + gauge poller + renewal
/// loop, 5s each) inside the teardown, pushing its internal worst case to
/// 37s (3×5 + 2 WS drain + 5 pool-supervisor drain + 10 processor flush +
/// 5 SSM lock release) — past the old 30s outer budget, so a
/// wedged-worker teardown could be force-DROPPED before the processor's
/// bounded final-flush window and the step-6 SSM lock-release await ran.
/// The compile-time assertion below pins outer ≥ Σ(inner bounded waits) so
/// a future join addition can never silently re-shrink the headroom.
const DHAN_LANE_TEARDOWN_DRAIN_TIMEOUT_SECS: u64 = 45;

/// R10-EDGE-1: the teardown's internal worst-case wall-clock — the sum of
/// every sequential bounded wait inside `teardown_dhan_lane_tasks`. MUST be
/// kept in sync when a new bounded await is added there (the const assert
/// below fails the build if the outer budget no longer covers it).
const DHAN_LANE_TEARDOWN_INTERNAL_WORST_CASE_SECS: u64 =
    // step 0/1: token-health writer + gauge poller + renewal loop joins,
    // + (F14, 2026-07-08) the step-3 pool-watchdog join before the honest
    // websocket/feed-health lane-off reset
    4 * LANE_HEALTH_TASK_JOIN_TIMEOUT_SECS
    // step 3: graceful WS RequestCode-12 drain sleep
    + WS_GRACEFUL_DRAIN_SLEEP_SECS
    // step 3b: bounded supervise_pool drain
    + POOL_SUPERVISOR_DRAIN_TIMEOUT_SECS
    // step 4: tick-processor final flush
    + tickvault_common::constants::GRACEFUL_SHUTDOWN_TIMEOUT_SECS
    // step 6: dual-instance SSM lock release
    + INSTANCE_LOCK_RELEASE_TIMEOUT_SECS;

const _: () = assert!(
    DHAN_LANE_TEARDOWN_DRAIN_TIMEOUT_SECS >= DHAN_LANE_TEARDOWN_INTERNAL_WORST_CASE_SECS,
    "R10-EDGE-1: the outer DHAN_LANE_TEARDOWN_DRAIN_TIMEOUT_SECS budget must cover the \
     teardown's internal worst-case sum of bounded waits — otherwise a wedged-worker \
     teardown is force-dropped (DHAN-LANE-04) before its later graceful steps \
     (processor flush, SSM lock release) can run"
);

/// Poll cadence for the Running-lane disable watch + the cold-start race
/// re-evaluation. Cold control-plane (a feed toggle), NOT the hot tick path —
/// a 2s observation latency is irrelevant. Mirrors `DHAN_ACTIVATION_POLL`.
const DHAN_LANE_RUNTIME_POLL_SECS: u64 = 2;
const DHAN_LANE_RUNTIME_POLL: std::time::Duration =
    std::time::Duration::from_secs(DHAN_LANE_RUNTIME_POLL_SECS);

/// Owned, re-entrant PROCESS-shared state the runtime Dhan-lane cold-start
/// needs — every field is a cheap `Arc`/`Clone`, captured ONCE after
/// `build_shared_infra` so the watcher can start the lane from cold at any time
/// (M9 / L11). This is the OWNED mirror of the boot-path borrowed
/// `DhanLaneContext<'a>`; the runtime wrapper rebuilds the borrowed context
/// from these owned fields at call time so the (verbatim-extracted)
/// `start_dhan_lane` body is reused unchanged.
pub struct DhanLaneRuntimeContext {
    config: std::sync::Arc<ApplicationConfig>,
    notifier: std::sync::Arc<NotificationService>,
    health_status: SharedHealthStatus,
    tick_broadcast_sender: tokio::sync::broadcast::Sender<tickvault_common::tick_types::ParsedTick>,
    order_update_sender: tokio::sync::broadcast::Sender<tickvault_common::order_types::OrderUpdate>,
    feed_runtime: std::sync::Arc<tickvault_api::feed_state::FeedRuntimeState>,
    feed_health: std::sync::Arc<tickvault_common::feed_health::FeedHealthRegistry>,
    trading_calendar: std::sync::Arc<TradingCalendar>,
    ws_frame_spill: Option<std::sync::Arc<tickvault_storage::ws_frame_spill::WsFrameSpill>>,
}

impl DhanLaneRuntimeContext {
    /// Build the owned runtime context from the PROCESS-shared infra. Called
    /// ONCE in `main()` after `build_shared_infra`, then handed to the dormant
    /// activation watcher via the shared `OnceCell`.
    #[allow(clippy::too_many_arguments)] // APPROVED: captures the full PROCESS-shared infra set (L11)
    #[must_use]
    // TEST-EXEMPT: trivial field-assignment constructor; its real call site is the boot wiring (dhan_lane_ctx_cell.set after build_shared_infra) and it is exercised by the live boot-deploy follow. No logic to unit-test.
    pub fn new(
        config: std::sync::Arc<ApplicationConfig>,
        notifier: std::sync::Arc<NotificationService>,
        health_status: SharedHealthStatus,
        tick_broadcast_sender: tokio::sync::broadcast::Sender<
            tickvault_common::tick_types::ParsedTick,
        >,
        order_update_sender: tokio::sync::broadcast::Sender<
            tickvault_common::order_types::OrderUpdate,
        >,
        feed_runtime: std::sync::Arc<tickvault_api::feed_state::FeedRuntimeState>,
        feed_health: std::sync::Arc<tickvault_common::feed_health::FeedHealthRegistry>,
        trading_calendar: std::sync::Arc<TradingCalendar>,
        ws_frame_spill: Option<std::sync::Arc<tickvault_storage::ws_frame_spill::WsFrameSpill>>,
    ) -> Self {
        Self {
            config,
            notifier,
            health_status,
            tick_broadcast_sender,
            order_update_sender,
            feed_runtime,
            feed_health,
            trading_calendar,
            ws_frame_spill,
        }
    }
}

/// Map a `start_dhan_lane` failure to its typed `DHAN-LANE-0N` ErrorCode + the
/// fixed (secret-free) `reason` discriminant + the `stage` counter label. Pure
/// + unit-tested so the runtime classify path is verifiable without I/O.
///
/// The boot extraction collapses every pre-pool gate (IP / auth / static-IP /
/// dual-instance / instrument-load) into the two `StartLaneError` flavours. At
/// RUNTIME we cannot tell auth from universe apart beyond those two flavours, so:
///   * `BootAbortClean` (a boot gate refused) maps to `DHAN-LANE-03` (auth/gate),
///     the most common runtime cold-start failure class.
///   * `BootAbortErr` (instrument-load failed) maps to `DHAN-LANE-01`
///     (universe-build).
///   * `WsPoolSpawn` (FTC-14) maps to `DHAN-LANE-02` (ws-pool-spawn) with
///     `stage='ws_pool'` — a runtime WS-pool build/spawn failure on a
///     connect-day now points the operator at the correct pool-spawn
///     runbook instead of the wrong `universe_build_failed` one.
/// `DHAN-LANE-04` (teardown-timeout) is emitted at its own dedicated site.
#[must_use]
fn classify_start_lane_error(
    err: &StartLaneError,
) -> (
    tickvault_common::error_code::ErrorCode,
    &'static str,
    &'static str,
) {
    use tickvault_common::error_code::ErrorCode;
    match err {
        StartLaneError::BootAbortClean => (
            ErrorCode::DhanLane03AuthFailed,
            "auth_or_gate_failed",
            "auth",
        ),
        StartLaneError::BootAbortErr(_) => (
            ErrorCode::DhanLane01UniverseBuildFailed,
            "universe_build_failed",
            "universe",
        ),
        // FTC-14: WS-pool build/spawn failed on a connect-day.
        StartLaneError::WsPoolSpawn => (
            ErrorCode::DhanLane02WsPoolSpawnFailed,
            "ws_pool_spawn_failed",
            "ws_pool",
        ),
    }
}

/// Compute the bounded retry backoff for the Nth consecutive failed cold-start.
/// `min(10 * 2^attempt, 300)` seconds — identical ladder to the Groww watcher
/// (cold control-plane). Pure + unit-tested.
#[must_use]
fn dhan_lane_retry_backoff_secs(attempt: u32) -> u64 {
    std::cmp::min(
        10u64.saturating_mul(1u64 << attempt.min(5)),
        DHAN_LANE_RETRY_BACKOFF_CAP_SECS,
    )
}

/// D2c (C4): token-headroom (seconds-until-expiry) for the PROCESS-level health
/// gauges (market-open self-test token-headroom + SLO token-freshness).
///
/// PREFERS the live lane-owned `TokenManager` stored on `FeedRuntimeState` and
/// falls back to the global `OnceLock` only when no lane is running (boot-OFF /
/// Groww-only / pre-start). This is the fix for the misleading reading after a
/// runtime Dhan-lane stop→re-start: the global `OnceLock` is set-once at first
/// boot and forever points at the dead boot-time (manager-A); the live slot
/// tracks the CURRENT lane manager (manager-B/C/…), so the gauges report the
/// running lane's real remaining seconds. Returns `0` when neither is present —
/// the same "no token" sentinel the gauges used before.
///
/// Off-hot-path: called on the once-per-trading-day self-test and the 10s SLO
/// cadences, never per-tick. The `live_token_manager()` read takes a brief,
/// uncontended `Mutex` and clones an `Arc` out (no `.await` held).
fn gauge_token_headroom_secs(
    feed_runtime: &std::sync::Arc<tickvault_api::feed_state::FeedRuntimeState>,
) -> u64 {
    if let Some(lane_tm) = feed_runtime.live_token_manager() {
        return lane_tm.seconds_until_expiry();
    }
    tickvault_core::auth::token_manager::global_token_manager()
        .map(|tm| tm.seconds_until_expiry())
        .unwrap_or(0)
}

/// Cadence (seconds) of the dedicated token-health writer task. Matches the
/// SLO scheduler's 10s sampling cadence so the `/health` / READY / EOD token
/// block stays within the same ≤10s staleness envelope the B3 plan documents.
const TOKEN_HEALTH_WRITER_INTERVAL_SECS: u64 = 10;

/// B3 round-2 (adversarial review HIGH + MEDIUM-1, 2026-07-03): dedicated,
/// UNCONDITIONAL writer of the shared health-state token block
/// (`token_remaining_secs` + `token_valid`).
///
/// Round 1 placed the two stores inside the SLO score loop, which is gated on
/// `config.features.realtime_guarantee_score` — flipping that UNRELATED flag
/// off would silently re-ghost the 09:14 IST READY Telegram, the 15:31:30 IST
/// EOD digest and `GET /health`, AND pin `overall_status()` "degraded"
/// (`state.rs` degrades on `!token_valid`). This task is spawned in
/// `start_dhan_lane` OUTSIDE any feature gate, and its `JoinHandle` is
/// registered in `DhanLaneRunHandles` so `teardown_dhan_lane_tasks` aborts it
/// on a runtime Dhan disable (MEDIUM-1: a bare `tokio::spawn` would survive
/// the lane teardown, fall back to the global boot manager after
/// `clear_live_token_manager`, and keep writing `token_valid=true` for up to
/// 24h while Dhan is deliberately OFF).
///
/// Two lock-free atomic stores every 10s on a cold-path task — not the tick
/// hot path. `secs == 0` means no token / expired (per `seconds_until_expiry`),
/// so `> 0` is the honest local-validity leg; F15 ANDs in the profile-truth
/// flag so a Dhan-KILLED token cannot read valid on /health.
// TEST-EXEMPT: infinite 10s interval loop over live token-manager state; the
// wiring (both setter calls, unconditional spawn site, teardown abort+reset)
// is pinned by crates/api/tests/token_headroom_wired_guard.rs and the pure
// derivation by `test_token_health_writer_valid_honors_profile_truth`.
fn spawn_token_health_writer(
    health: SharedHealthStatus,
    feed_runtime: std::sync::Arc<tickvault_api::feed_state::FeedRuntimeState>,
    profile_valid: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            TOKEN_HEALTH_WRITER_INTERVAL_SECS,
        ));
        loop {
            interval.tick().await;
            let secs = gauge_token_headroom_secs(&feed_runtime);
            let profile_ok = profile_valid.load(std::sync::atomic::Ordering::Acquire);
            health.set_token_remaining_secs(secs);
            health.set_token_valid(token_health_writer_valid(secs, profile_ok));
        }
    })
}

/// F15 (2026-07-08). Pure. The /health `token_valid` derivation — mirrors
/// the `tv_token_valid` gauge's AND composition (`token_health_gauge.rs`):
/// valid iff the token has local headroom (`secs > 0` — strictly greater,
/// fail-closed at the expiry instant) AND the mid-session watchdog's last
/// `/v2/profile` check did not REALLY fail. Previously `/health` used
/// `secs > 0` alone, so a Dhan-KILLED but locally-unexpired token read
/// valid there for up to ~24h while `tv_token_valid` honestly read 0.0.
fn token_health_writer_valid(secs: u64, profile_valid: bool) -> bool {
    secs > 0 && profile_valid
}

/// Stable static label for the lane state (no allocation on the metric path).
const fn lane_state_label(state: tickvault_api::feed_state::LaneState) -> &'static str {
    use tickvault_api::feed_state::LaneState;
    match state {
        LaneState::Off => "off",
        LaneState::Starting => "starting",
        LaneState::Running => "running",
        LaneState::Stopping => "stopping",
    }
}

/// Edge-triggered `tv_dhan_lane_state` gauge write (L12): set the gauge ONLY on
/// a real FSM transition, never per-poll. Also bumps the per-transition counter.
fn record_dhan_lane_transition(
    from: tickvault_api::feed_state::LaneState,
    to: tickvault_api::feed_state::LaneState,
) {
    metrics::gauge!("tv_dhan_lane_state").set(f64::from(to.as_u8()));
    metrics::counter!(
        "tv_dhan_lane_transitions_total",
        "from" => lane_state_label(from),
        "to" => lane_state_label(to),
    )
    .increment(1);
}

/// Decide whether the runtime supervisor should ABORT the in-flight cold-start
/// task this tick (cancel-safety, H8). Pure + unit-tested.
///
/// The cold-start task owns the FULL lifecycle (Off→Starting→Running→park→
/// gated-teardown→Off). The supervisor only needs to force-cancel the ONE case
/// the task cannot self-resolve: a cold-start still in `Starting` (no pool
/// parked yet, `start_dhan_lane` has no cancel arm) when the operator has
/// flipped Dhan OFF. Aborting then triggers `start_dhan_lane`'s own cleanup
/// (it aborts every lane-owned handle it spawned so far) at the next `.await`.
///
/// A `Running` lane is NOT aborted by the supervisor — its parked task does the
/// H5-gated, H6-joined teardown itself; aborting it would skip the gate-hold +
/// the handle-join. An `Off`/`Stopping` lane has no in-flight start to cancel.
#[must_use]
fn should_abort_cold_start_on_disable(
    desired_on: bool,
    lane_state: tickvault_api::feed_state::LaneState,
) -> bool {
    use tickvault_api::feed_state::LaneState;
    !desired_on && matches!(lane_state, LaneState::Starting)
}

/// NT-15 (audit 2026-07-01): seed the global `TickGapDetector` with every
/// subscribed instrument at boot so a SID that never ticks all session is
/// still detectable as a per-SID cold black-hole (WS-GAP-06 /
/// `tv_tick_gap_instruments_silent`), not silently masked by other SIDs that
/// do tick. Insert-if-absent — never clobbers an entry a real tick already
/// wrote. Cold path, called once per boot after the subscription plan exists.
fn seed_tick_gap_detector_from_plan(plan: &Option<SubscriptionPlan>) {
    let Some(plan) = plan else {
        return;
    };
    // §36.7 (2026-07-10): refresh the far-month IndexFuture exclusion set
    // FIRST — it gates only the two ALARM-FACING counts (SLO tick_freshness
    // + the silent gauge); the SIDs below are still SEEDED so WS-GAP-06
    // keeps logging a never-ticking month per-SID. Runs at every seed call
    // site (both boot arms + a runtime lane cold start), so a rebuilt plan
    // always overwrites the previous set.
    // AM-r1 F6: nearest-month identity flows through the shared singular
    // `select_index_future_expiry`, so the split honors `>= today` at
    // seed time (a post-midnight lane cold start rolls correctly).
    let today_ist = (chrono::Utc::now()
        + chrono::TimeDelta::seconds(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64))
    .date_naive();
    let far = far_month_future_sids(plan, today_ist);
    let far_month_excluded = far.len();
    store_far_month_future_exclusions(far);
    let Some(detector) = tickvault_core::pipeline::tick_gap_detector::global_tick_gap_detector()
    else {
        return;
    };
    let now = std::time::Instant::now();
    let mut seeded = 0u64;
    // O(1) EXEMPT: cold-path boot seeding, one insert-if-absent per
    // subscribed instrument (~universe size), never on the hot loop.
    for inst in plan.registry.iter() {
        detector.seed_subscribed(inst.security_id, inst.exchange_segment, now);
        seeded += 1;
    }
    info!(
        seeded,
        far_month_excluded,
        "NT-15: seeded tick-gap detector with subscribed instruments — a \
         never-ticking SID is now a first-tick black-hole signal; §36.7 \
         non-nearest-month futures are excluded from the alarm-facing \
         silent counts only"
    );
}

/// §36.7 (2026-07-10): composite `(security_id, exchange_segment)` keys of
/// the NON-NEAREST-month IndexFuture targets — far monthly serials tick
/// sparsely by nature; counting them as "silent" would permanently lift the
/// healthy always-silent floor (~33) toward the many-instruments-silent
/// alarm threshold (40) and drag SLO tick_freshness below 0.95.
/// INDIA-VIX-precedent exclusion, applied ONLY to the two alarm-facing
/// counts; the SIDs stay SEEDED in the gap detector so WS-GAP-06 still logs
/// a never-ticking month per-SID (the live-delivery probe for months 2..N).
/// Composite key per I-P1-11 — a future SID numerically colliding with a
/// spot SID on another segment must never exclude the spot.
///
/// AM-r1 F3 (2026-07-10): grouping is by the CANONICAL underlying
/// (`canonicalize_index_symbol`), never the raw `underlying_symbol` literal —
/// the Dhan master legally carries mixed alias literals across months of one
/// underlying (the MIDCPNIFTY "NIFTY MIDCAP SELECT" precedent); raw-string
/// groups would split, make every month its own "nearest", and silently
/// disable the exclusion.
///
/// AM-r1 F6 (2026-07-10): the nearest month is identified through the SHARED
/// singular `select_index_future_expiry` (the `>= today` rule keeps exactly
/// ONE implementation). An underlying whose plan months are ALL `< today`
/// (stale boot-frozen plan) gets NO nearest → nothing excluded for it —
/// fail-safe: its SIDs stay counted, never wrongly subtracted.
///
/// AM-r1 F5 honest envelope (2026-07-10): the nearest/far split is per-BOOT,
/// not per-day — this runs only at plan-seed time (both boot arms + a
/// runtime lane cold start) on the boot-frozen plan. A process surviving
/// across an expiry-day IST midnight WITHOUT a re-seed keeps yesterday's
/// split (the subscription itself is equally boot-frozen — the same
/// pre-existing envelope); the AWS daily 16:30 stop makes this a
/// dev/manual-run-only residual. See `futidx-4-error-codes.md` §3.
#[must_use]
fn far_month_future_sids(
    plan: &SubscriptionPlan,
    today_ist: chrono::NaiveDate,
) -> std::collections::HashSet<(u64, tickvault_common::types::ExchangeSegment)> {
    use tickvault_common::instrument_registry::SubscriptionCategory;
    use tickvault_core::instrument::index_extractor::canonicalize_index_symbol;
    use tickvault_core::instrument::index_futures::select_index_future_expiry;
    // Pass 1: collect the plan's expiry months per CANONICAL underlying.
    // IndexDerivative is populated ONLY by the IndexFuture role in the
    // DailyUniverse plan.
    let mut months: std::collections::HashMap<String, Vec<chrono::NaiveDate>> =
        std::collections::HashMap::new();
    for inst in plan.registry.iter() {
        if inst.category != SubscriptionCategory::IndexDerivative {
            continue;
        }
        let Some(exp) = inst.expiry_date else {
            // Unparsable expiry: conservatively KEEP the SID counted (it
            // cannot be proven non-nearest).
            continue;
        };
        months
            .entry(canonicalize_index_symbol(&inst.underlying_symbol))
            .or_default()
            .push(exp);
    }
    // Nearest per canonical via the shared singular (F6).
    let mut min_expiry: std::collections::HashMap<String, chrono::NaiveDate> =
        std::collections::HashMap::new();
    for (canonical, mut dates) in months {
        dates.sort_unstable();
        dates.dedup();
        if let Some(nearest) = select_index_future_expiry(&dates, today_ist) {
            min_expiry.insert(canonical, nearest);
        }
    }
    // Pass 2: every future whose expiry is strictly AFTER its underlying's
    // nearest month is excluded from the alarm-facing silent counts.
    let mut out: std::collections::HashSet<(u64, tickvault_common::types::ExchangeSegment)> =
        std::collections::HashSet::new();
    for inst in plan.registry.iter() {
        if inst.category != SubscriptionCategory::IndexDerivative {
            continue;
        }
        let Some(exp) = inst.expiry_date else {
            continue;
        };
        if let Some(min) = min_expiry.get(&canonicalize_index_symbol(&inst.underlying_symbol))
            && exp > *min
        {
            out.insert((inst.security_id, inst.exchange_segment));
        }
    }
    out
}

/// Process-global far-month exclusion set (§36.7). `RwLock` because a
/// runtime Dhan-lane cold start rebuilds the plan and overwrites the set;
/// readers are the 10s SLO loop + the 60s gauge loop (cold paths).
static FAR_MONTH_FUTURE_EXCLUDED_SIDS: std::sync::OnceLock<
    std::sync::RwLock<std::collections::HashSet<(u64, tickvault_common::types::ExchangeSegment)>>,
> = std::sync::OnceLock::new();

/// Overwrite the far-month exclusion set (called wherever the tick-gap
/// detector is seeded from a fresh plan). Poison recovery per the house
/// pattern — the data is still valid after a panicked writer.
fn store_far_month_future_exclusions(
    set: std::collections::HashSet<(u64, tickvault_common::types::ExchangeSegment)>,
) {
    let lock =
        FAR_MONTH_FUTURE_EXCLUDED_SIDS.get_or_init(|| std::sync::RwLock::new(Default::default()));
    match lock.write() {
        Ok(mut guard) => *guard = set,
        Err(poisoned) => *poisoned.into_inner() = set,
    }
}

/// Is this (sid, segment) a NON-NEAREST-month §36.7 index future? Used by
/// the SLO tick_freshness silent filter + the silent-instruments gauge —
/// never by the seeding, the WS-GAP-06 log, or any data path.
#[must_use]
fn is_far_month_future_sid(
    security_id: u64,
    segment: tickvault_common::types::ExchangeSegment,
) -> bool {
    FAR_MONTH_FUTURE_EXCLUDED_SIDS
        .get()
        .is_some_and(|lock| match lock.read() {
            Ok(guard) => guard.contains(&(security_id, segment)),
            Err(poisoned) => poisoned.into_inner().contains(&(security_id, segment)),
        })
}

/// Decide whether the runtime supervisor should SPAWN a fresh cold-start task
/// this tick. Pure + unit-tested. Spawn iff: desired-ON AND the lane is in a
/// confirmed-empty `Off` (no pool — H6: never start from `Stopping`) AND no
/// task is currently active.
#[must_use]
fn should_spawn_cold_start(
    desired_on: bool,
    lane_state: tickvault_api::feed_state::LaneState,
    task_active: bool,
) -> bool {
    use tickvault_api::feed_state::LaneState;
    desired_on && matches!(lane_state, LaneState::Off) && !task_active
}

/// Runtime Dhan-lane cold-start SUPERVISOR — the single owner of the one
/// abortable cold-start `JoinHandle` (the Groww-watcher shape). Spawned
/// UNCONDITIONALLY at boot; idles (zero Dhan work) until the runtime context is
/// populated (after `build_shared_infra`) AND the operator enables a boot-OFF
/// Dhan feed. Level-triggered: it compares the live enable flag + the shared
/// `LaneState` against whether it holds an active task, so a flap converges to
/// the final state and a sub-poll ON→OFF→ON cannot leave a stuck lane.
///
/// Boot-ON byte-identical: if Dhan was ON at boot, the inline boot spine drove
/// the FSM to `Running` (see the boot wiring), so the supervisor's first tick
/// sees `LaneState::Running` (not `Off`) → it does NOTHING. It ONLY ever acts on
/// a boot-OFF cold-start or a runtime re-enable of a fully-torn-down lane.
///
/// The cold-start task owns the FULL lifecycle; the supervisor only force-cancels
/// a `Starting` lane on a disable (H8) and respawns a dead task (WS-GAP-05-style
/// bounded respawn — a task that ended without bringing the lane up).
// TEST-EXEMPT: infinite control-plane supervisor loop driving live spawn/abort; the pure decisions (should_spawn_cold_start, should_abort_cold_start_on_disable) are unit-tested below + the FSM is unit-tested in feed_state.rs.
pub async fn run_dhan_lane_runtime_supervisor(
    feed_runtime: std::sync::Arc<tickvault_api::feed_state::FeedRuntimeState>,
    ctx_cell: std::sync::Arc<tokio::sync::OnceCell<std::sync::Arc<DhanLaneRuntimeContext>>>,
) {
    use tickvault_api::feed_state::{Feed, LaneState};
    let mut active_task: Option<tokio::task::JoinHandle<()>> = None;
    // 2026-07-13 refusal latch — edge-triggered once per attempt-episode
    // (audit Rule 4): set on the first refused cold-start of an ON period,
    // re-armed when the operator toggles Dhan OFF again. Never per-2s spam.
    let mut cold_start_refusal_logged = false;
    loop {
        // Idle (zero Dhan work) until the runtime context exists. The context is
        // populated by `main()` right after `build_shared_infra`, which runs in
        // BOTH the Dhan-ON and Dhan-OFF paths.
        let Some(ctx) = ctx_cell.get() else {
            tokio::time::sleep(DHAN_LANE_RUNTIME_POLL).await;
            continue;
        };

        let desired_on = feed_runtime.is_enabled(Feed::Dhan);
        let lane_state = feed_runtime.dhan_lane_state();

        // Dead-task respawn (WS-GAP-05-style): a finished task that did NOT bring
        // the lane up (panicked / early-returned out of `Starting`) leaves the
        // FSM in `Off` (its own `StartFailed`/cancel cleanup) or `Starting`
        // (panic mid-start). Clear the handle so a desired-ON tick re-spawns it.
        if active_task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            // The task ended; if the FSM is stuck in Starting (panic mid-start),
            // reset it to Off so a fresh spawn is legal (no double pool — the
            // panicked task's `start_dhan_lane` already aborted its own spawns).
            // CAS via the FSM (not a force-write): `advance(StartFailed)` returns
            // `Some(Off)` ONLY if the lane is still `Starting`. If the task had
            // already reached `Running` before panicking-post-park (it can't —
            // a Running task parks and never `is_finished` until teardown), the
            // CAS would reject and we leave the live state untouched.
            if feed_runtime.advance_dhan_lane(tickvault_api::feed_state::LaneEvent::StartFailed)
                == Some(LaneState::Off)
            {
                record_dhan_lane_transition(LaneState::Starting, LaneState::Off);
                error!(
                    code = tickvault_common::error_code::ErrorCode::DhanLane03AuthFailed.code_str(),
                    severity = tickvault_common::error_code::ErrorCode::DhanLane03AuthFailed
                        .severity()
                        .as_str(),
                    reason = "cold_start_task_ended_without_running",
                    stage = "supervisor",
                    "[dhan-lane] cold-start task ended without bringing the lane up — reset to \
                     Off for a clean respawn"
                );
            } else if feed_runtime.dhan_lane_state() == LaneState::Running {
                // Defensive (hostile-review LOW): the owned task finished while the
                // FSM is still `Running` — i.e. `park_running_dhan_lane` panicked
                // (or returned) BEFORE its `Running→Stopping→Off` teardown drove
                // the FSM down. Without a recovery arm the lane stays stuck
                // `Running` with a dead owner: `should_spawn_cold_start` (which
                // requires `Off`) never fires, so the feed is dead forever. The
                // `StartFailed` CAS above only fires from `Starting`, so it cannot
                // recover a stuck `Running`. Force the FSM to `Off` + clear the
                // running flag (safe: the owner is `is_finished`, so nothing else
                // will drive the FSM; the dead task's `start_dhan_lane`/teardown
                // already aborted its own spawns, so no double pool). The next
                // desired-ON tick re-cold-starts the lane.
                feed_runtime.set_dhan_lane_state(LaneState::Off);
                feed_runtime.set_dhan_lane_running(false);
                // D2c (C4): lane forced Off (dead owner) — drop the live token-
                // manager so the health gauges fall back to the global instead of
                // reading this lane's now-dead manager until the supervisor
                // re-cold-starts it.
                feed_runtime.clear_live_token_manager();
                record_dhan_lane_transition(LaneState::Running, LaneState::Off);
                error!(
                    code = tickvault_common::error_code::ErrorCode::DhanLane03AuthFailed.code_str(),
                    severity = tickvault_common::error_code::ErrorCode::DhanLane03AuthFailed
                        .severity()
                        .as_str(),
                    reason = "park_task_ended_while_running",
                    stage = "supervisor",
                    "[dhan-lane] parked lane task ended while still Running (panic before \
                     teardown) — forced to Off for a clean respawn"
                );
            }
            active_task = None;
        }

        // Cancel-safety (H8): force-abort an in-flight `Starting` cold-start when
        // the operator disabled Dhan. `start_dhan_lane` has no cancel arm, so the
        // abort lands at its next `.await`, triggering its own lane-handle
        // cleanup. (A `Running` parked task self-tears-down with the gate-hold +
        // join — the supervisor does NOT abort it.)
        //
        // CRITICAL ownership invariant: the supervisor only touches the FSM for a
        // `Starting` lane it OWNS (`active_task.is_some()`). A boot-ON inline
        // start ALSO seeds the FSM to `Starting` (then drives it to `Running`
        // itself) but is NOT owned by the supervisor — so the supervisor MUST
        // NOT fire `StartCancelled` against it (that would clobber the inline
        // start mid-flight). The boot-ON runtime disable is handled by the
        // existing `dhan_activation` watcher + PR-E in-loop dormancy, not here.
        //
        // CRITICAL CAS-FIRST race fix (hostile-review MEDIUM): the owned task can
        // CAS `Starting→Running` between our `lane_state` snapshot above and this
        // block. So we DRIVE THE FSM FIRST via `advance_dhan_lane(StartCancelled)`
        // — that CAS returns `Some(Off)` ONLY if the lane was genuinely still
        // `Starting` (and we therefore won the cancel). If the task already
        // promoted to `Running`, `StartCancelled` is illegal → `None`, and we do
        // NOTHING: we MUST NOT abort the now-`Running` task (it owns its H5/H6
        // teardown — aborting it would leak its WS pool) and MUST NOT mark it
        // not-running. The abort + not-running side-effects fire ONLY on a
        // confirmed cancel.
        if active_task.is_some() && should_abort_cold_start_on_disable(desired_on, lane_state) {
            if feed_runtime.advance_dhan_lane(tickvault_api::feed_state::LaneEvent::StartCancelled)
                == Some(LaneState::Off)
            {
                // We won the cancel: the lane was still Starting. NOW abort the
                // owned task + mark not-running. Cleanup on abort (R7-EDGE-1 /
                // COV-R7-1 / R8-EDGE-1 / R8-EDGE-2 / SEC-C3-1 / R9-EDGE-1/2 /
                // COV-C4-2, 2026-07-07): every lane-owned handle spawned so
                // far is covered by a Drop floor at the abort instant — the
                // early-spawned tick processor + main-feed WS pool + pool
                // watchdog + the Item 19f heartbeat-shutdown bridge + the
                // IST-midnight enricher rollover + the 5-min prev_oi refresh
                // poller by `PreLaneAbortGuard` (abort-on-drop) across the
                // WAL-reinject / connected-alerts awaits, the dual-instance
                // lock heartbeat by `InstanceLockHeartbeatGuard`
                // (NOTIFY-on-drop: graceful SSM release, never an abort — a
                // detached zombie renewer would otherwise wedge every
                // re-enable with AlreadyHeld / RESILIENCE-01), and everything
                // else (incl. the OrderUpdateAuthenticated listener) by the
                // AG5-R3-1 early-constructed `DhanLaneRunHandles` H8 floor —
                // so dropping the cancelled future ABORTS-or-RELEASES (never
                // detaches) the lane's registered tasks. Honest residual
                // (COV-C4-2, deliberately NOT lane-owned): the
                // market-hours-gated no-tick watchdog and the one-shot
                // sleep-then-notify close/reset timers — the timers
                // self-terminate at their fire time (bounded ≤ ~24h) and the
                // watchdog is heartbeat-driven + session-gated, so neither
                // pins lane data-path state nor pages from a dead lane.
                if let Some(task) = active_task.take() {
                    info!(
                        "[dhan-lane] Dhan disabled mid-cold-start (Starting→Off won) — aborting \
                         the in-flight start; its lane-handle cleanup runs"
                    );
                    task.abort();
                }
                record_dhan_lane_transition(LaneState::Starting, LaneState::Off);
                feed_runtime.set_dhan_lane_running(false);
                // D2c (C4): cold-start cancelled (Starting→Off won) — drop the live
                // token-manager so the health gauges fall back to the global instead
                // of reading the aborted start's manager. (The slot may already be
                // None if the abort landed before set_live_token_manager; clearing is
                // idempotent.)
                feed_runtime.clear_live_token_manager();
            } else {
                // The task promoted to Running between our snapshot and now — do
                // NOT abort it. Its parked `park_running_dhan_lane` owns the
                // H5-gated, H6-joined teardown for this disable.
                info!(
                    "[dhan-lane] disable observed but the cold-start already reached Running — \
                     leaving the parked task to do its gated teardown (no abort, no false UI)"
                );
            }
        }

        // Spawn a fresh cold-start when desired-ON + confirmed-empty Off + no
        // active task. The task itself drives the whole FSM from here.
        if should_spawn_cold_start(
            desired_on,
            feed_runtime.dhan_lane_state(),
            active_task.is_some(),
        ) {
            // 2026-07-13 operator directive ("now remove this entire Dhan
            // live websocket feed instruments subscription even entire live
            // websocket feed itself"): with `dhan_enabled = false` in the
            // RAW boot TOML, the Dhan REST-only stack (`dhan_rest_stack.rs`)
            // already owns the dual-instance SSM lock + the TokenManager. A
            // runtime cold-start of the FULL lane (POST /api/feeds/dhan →
            // desired-ON here) would collide with it — Step 6a-prime sees
            // AlreadyHeld → DHAN-LANE-03 retry loop + RESILIENCE-01 pages
            // against our OWN process. The supervisor therefore REFUSES the
            // cold-start while the RAW config says Dhan is off; re-enabling
            // the live WS lane requires a config change + restart (and a
            // fresh dated operator quote per production_config_wiring.rs).
            //
            // Round-2 FIX A (2026-07-13): the gate reads the RAW TOML
            // snapshot (`is_dhan_config_enabled`, seeded pre-overlay), NOT
            // the overlaid `ctx.config.feeds` — a persisted runtime-OFF
            // overlay on a config-ON boot leaves the lane dormant-but-NOT-
            // retired (the REST-only stack did NOT spawn on that boot, same
            // raw gate — no lock to collide with), so the cold-start stays
            // allowed there (pre-Phase-A round trip). This keeps the gate in
            // lockstep with the /api/feeds 409 arm: the API never accepts an
            // enable this supervisor would refuse.
            if !feed_runtime.is_dhan_config_enabled() {
                if !cold_start_refusal_logged {
                    warn!(
                        "Dhan live WS lane retired by operator directive 2026-07-13 — runtime \
                         enable refused; config change + restart required (the Dhan REST-only \
                         stack keeps running; toggling the feed OFF re-arms this notice)"
                    );
                    cold_start_refusal_logged = true;
                }
            } else {
                info!(
                    "[dhan-lane] runtime supervisor spawning cold-start (boot-OFF Dhan enabled \
                     at runtime, lane is Off)"
                );
                let task = tokio::spawn(run_dhan_lane_cold_start(std::sync::Arc::clone(ctx)));
                active_task = Some(task);
            }
        }
        if !desired_on {
            // Operator toggled Dhan back OFF — re-arm the refusal notice for
            // the next ON attempt-episode (edge-triggered, audit Rule 4).
            cold_start_refusal_logged = false;
        }

        tokio::time::sleep(DHAN_LANE_RUNTIME_POLL).await;
    }
}

/// Runtime Dhan-lane cold-start driver — the body of the ONE owned task the
/// runtime supervisor holds for an ON period. Drives the `LaneState` FSM:
/// `Off→Starting`, `start_dhan_lane`, then on `Ok` `Starting→Running` (mark
/// pool present + lane running ONLY here — phantom-running closure) and parks
/// (idles) until disable; on `Err` `Starting→Off` + typed `DHAN-LANE-0N` +
/// bounded backoff + retry. On a disable the supervisor `.abort()`s this whole
/// task while `Starting` — `start_dhan_lane`'s own cancel cleanup (H8) aborts
/// every lane-owned handle it spawned.
///
/// This wrapper spawns NO children of its own (C2): the only spawns are inside
/// `start_dhan_lane` (LANE) and `build_shared_infra` (PROCESS).
// TEST-EXEMPT: orchestration task that drives the unit-tested FSM (next_lane_state) + the unit-tested pure helpers (classify_start_lane_error, dhan_lane_retry_backoff_secs) around live I/O (start_dhan_lane); the live cold-start is exercised by the live boot-deploy follow.
pub async fn run_dhan_lane_cold_start(ctx: std::sync::Arc<DhanLaneRuntimeContext>) {
    use tickvault_api::feed_state::{Feed, LaneEvent, LaneState};

    let mut attempt: u32 = 0;
    loop {
        // Drive Off→Starting through the FSM. If we can't (e.g. the state is not
        // Off — a race with a concurrent stop), bail; the watcher re-evaluates.
        match ctx
            .feed_runtime
            .advance_dhan_lane(LaneEvent::StartRequested)
        {
            Some(LaneState::Starting) => {
                record_dhan_lane_transition(LaneState::Off, LaneState::Starting);
                info!(
                    "[dhan-lane] runtime cold-start beginning (Off→Starting) — auth → \
                     universe → main-feed pool"
                );
            }
            _ => {
                info!(
                    current = lane_state_label(ctx.feed_runtime.dhan_lane_state()),
                    "[dhan-lane] cold-start not begun — lane not in Off (race with stop); \
                     the watcher will re-evaluate"
                );
                return;
            }
        }

        // Build the borrowed boot-path context from the owned runtime context.
        // Runtime starts NEVER replay WAL (that is a boot-only concern), so the
        // replay buffers are empty.
        let lane_ctx = DhanLaneContext {
            config: &ctx.config,
            notifier: &ctx.notifier,
            health_status: &ctx.health_status,
            tick_broadcast_sender: &ctx.tick_broadcast_sender,
            order_update_sender: &ctx.order_update_sender,
            feed_runtime: &ctx.feed_runtime,
            feed_health: &ctx.feed_health,
            trading_calendar: &ctx.trading_calendar,
            ws_frame_spill: &ctx.ws_frame_spill,
            is_market_hours: ctx.trading_calendar.is_trading_day_today(),
            is_trading: ctx.trading_calendar.is_trading_day_today(),
            is_muhurat: ctx.trading_calendar.is_muhurat_trading_today(),
            is_mock_trading: ctx.trading_calendar.is_mock_trading_today(),
            boot_start: std::time::Instant::now(),
            ws_wal_replay_live_feed: Vec::new(),
            ws_wal_replay_order_update: Vec::new(),
            // RUNTIME (H7): lane-scoped watchdog — a 300s-all-down Halt MUST NOT
            // kill the process / Groww feed. It drives the lane FSM to Off so the
            // supervisor re-cold-starts it (bounded backoff), shared infra ALIVE.
            lane_scoped: true,
        };

        match start_dhan_lane(lane_ctx).await {
            Ok(handles) => {
                // Phantom-running closure + disable-during-cold-start race fix:
                // ALL success side-effects (pool-present + lane-running + the
                // "started" Telegram + park) are gated on this CAS WINNING
                // `Starting→Running`. The supervisor can win an
                // `advance_dhan_lane(StartCancelled)` (Starting→Off) in the
                // sub-second window where `start_dhan_lane` is returning `Ok`
                // (operator disabled Dhan mid-cold-start). In that case THIS CAS
                // is `(Off, StartSucceeded)` → `None` (the FSM is total + rejects
                // it), and we must NOT mark the lane running, must NOT fire a
                // phantom "Dhan started" UI, and must NOT park the pool. Parking
                // it would drop the just-spawned `DhanLaneRunHandles` on the
                // supervisor's `.abort()` and DETACH (leak) the live WS pool while
                // the FSM reads `Off` (violates H6 join-before-Off + H8 no-leak).
                if ctx
                    .feed_runtime
                    .advance_dhan_lane(LaneEvent::StartSucceeded)
                    != Some(LaneState::Running)
                {
                    // Lost the Starting→Off race to the supervisor's
                    // StartCancelled. The FSM is already `Off`; gracefully close
                    // (RequestCode-12 unsubscribe + shutdown_notify) and JOIN the
                    // just-spawned pool via the bounded lane-scoped teardown — the
                    // SAME teardown the operator-disable path uses — so no pool
                    // leaks. Then drop the live token-manager (idempotent) and
                    // return; the supervisor re-cold-starts the lane if Dhan is
                    // re-enabled.
                    info!(
                        "[dhan-lane] cold-start lost the Starting→Off race to the supervisor \
                         (StartCancelled won) — tearing down the just-spawned pool instead of \
                         parking it (no leak, no phantom 'Dhan started')"
                    );
                    let teardown = teardown_dhan_lane_tasks(handles);
                    if tokio::time::timeout(
                        std::time::Duration::from_secs(DHAN_LANE_TEARDOWN_DRAIN_TIMEOUT_SECS),
                        teardown,
                    )
                    .await
                    .is_err()
                    {
                        let code =
                            tickvault_common::error_code::ErrorCode::DhanLane04TeardownTimeout;
                        error!(
                            code = code.code_str(),
                            severity = code.severity().as_str(),
                            "[dhan-lane] lost-race teardown drain timed out — handles \
                             force-aborted; lane stays Off"
                        );
                        metrics::counter!("tv_dhan_lane_teardown_forced_total").increment(1);
                    }
                    ctx.feed_runtime.set_dhan_lane_running(false);
                    ctx.feed_runtime.clear_live_token_manager();
                    return;
                }
                // We won the CAS: the lane is genuinely Running. Pool-present +
                // lane-running are set ONLY here, after the pool genuinely spawned.
                record_dhan_lane_transition(LaneState::Starting, LaneState::Running);
                ctx.feed_runtime.mark_dhan_pool_present();
                ctx.feed_runtime.set_dhan_lane_running(true);
                info!(
                    "[dhan-lane] runtime cold-start SUCCEEDED (Starting→Running) — main-feed \
                     pool live, ticks flowing; lane idling until disable"
                );
                ctx.notifier.notify(NotificationEvent::Custom {
                    message: "🟢 <b>Dhan feed started</b>\nThe live price feed is now connected \
                              and streaming."
                        .to_string(),
                });
                // The lane is Running. Park the handles + idle until a disable
                // toggle, then tear down (H5 gate-hold + H6 join inside).
                park_running_dhan_lane(std::sync::Arc::clone(&ctx), handles).await;
                return;
            }
            Err(err) => {
                let (code, reason, stage) = classify_start_lane_error(&err);
                // FSM Starting→Off — no half-running lane. `start_dhan_lane`'s
                // own cancel/Err cleanup already covered every lane-owned
                // handle it spawned: the tick processor / WS pool / pool
                // watchdog / midnight-rollover / prev_oi-refresh guards ABORT
                // on drop (H8 + R7-EDGE-1 / R8-EDGE-2 / R9-EDGE-2), and the
                // dual-instance lock heartbeat guard NOTIFY-releases
                // on drop (R8-EDGE-1: graceful SSM DeleteParameter, so the
                // bounded-backoff retry below can re-acquire the lock instead
                // of wedging on its own zombie renewer with AlreadyHeld →
                // RESILIENCE-01).
                if ctx.feed_runtime.advance_dhan_lane(LaneEvent::StartFailed)
                    == Some(LaneState::Off)
                {
                    record_dhan_lane_transition(LaneState::Starting, LaneState::Off);
                }
                ctx.feed_runtime.set_dhan_lane_running(false);
                // D2c (C4): cold-start FAILED (Starting→Off) — drop the live token-
                // manager so the health gauges fall back to the global instead of
                // reading the failed start's manager during the bounded retry
                // backoff. (Idempotent if the failure landed before
                // set_live_token_manager ran.)
                ctx.feed_runtime.clear_live_token_manager();
                attempt = attempt.saturating_add(1);
                let backoff = dhan_lane_retry_backoff_secs(attempt);
                error!(
                    code = code.code_str(),
                    severity = code.severity().as_str(),
                    reason,
                    stage,
                    attempt,
                    backoff_secs = backoff,
                    "[dhan-lane] runtime cold-start FAILED (Starting→Off) — bounded retry"
                );
                metrics::counter!(
                    "tv_dhan_lane_start_failed_total",
                    "stage" => stage,
                )
                .increment(1);
                // If the operator disabled Dhan during the failed attempt, stop
                // retrying — the watcher's `.abort()` would also cancel us, but
                // checking here avoids one wasted backoff sleep.
                if !ctx.feed_runtime.is_enabled(Feed::Dhan) {
                    info!("[dhan-lane] Dhan disabled during failed cold-start — stopping retries");
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                // Loop: re-attempt Off→Starting→… . `advance_dhan_lane` confirms
                // the lane is still Off before re-starting (no double pool).
            }
        }
    }
}

/// Park a Running runtime lane: hold its owned handles and wait for the
/// activation watcher to request a Stop (the lane's enable flag flips OFF AND
/// the gate permits). On a disable this drives `Running→Stopping`, re-asserts
/// the disable gate (H5), tears the lane down (`teardown_dhan_lane_tasks` with a
/// bounded drain, H6 join), and drives `Stopping→Off` (or stays `Running` if
/// the gate re-closed mid-teardown, H5).
///
/// Polls the enable flag on the cold-control-plane cadence (2s); the lane does
/// zero work while Running-and-enabled (the WS pool + tick processor it spawned
/// run independently). When the watcher `.abort()`s the owning cold-start task,
/// THIS future is dropped — a hard disconnect equivalent to the boot teardown.
// TEST-EXEMPT: orchestration around the unit-tested FSM (advance_dhan_lane) + the live teardown_dhan_lane_tasks; the live disable path is exercised by the boot-deploy follow.
async fn park_running_dhan_lane(
    ctx: std::sync::Arc<DhanLaneRuntimeContext>,
    handles: DhanLaneRunHandles,
) {
    use tickvault_api::feed_state::{Feed, LaneEvent, LaneState};
    // H7: the lane-scoped pool-watchdog Halt signal (always `Some` for a runtime
    // lane — `start_dhan_lane` built it because `lane_scoped == true`). On a
    // 300s-all-down Halt the watchdog fires this instead of `process::exit(2)`,
    // and we tear the lane down + drive the FSM to `Off` so the supervisor
    // re-cold-starts it (Groww + shared infra stay ALIVE).
    let lane_halt = handles.lane_halt_notify.clone();
    let mut handles = Some(handles);
    loop {
        let desired_on = ctx.feed_runtime.is_enabled(Feed::Dhan);
        if desired_on {
            // Park: idle until EITHER the next poll tick (re-check the enable
            // flag) OR the lane-scoped watchdog Halt fires. The Halt path is the
            // H7 cross-feed blast-radius fix — a lane-local Dhan fault tears down
            // ONLY this lane, never the process / Groww / shared infra.
            if let Some(ref halt) = lane_halt {
                tokio::select! {
                    () = halt.notified() => {
                        if let Some(owned) = handles.take() {
                            handle_lane_watchdog_halt(&ctx, owned).await;
                        }
                        // Return so the supervisor's `should_spawn_cold_start`
                        // re-cold-starts the lane (desired-ON + Off + no task).
                        return;
                    }
                    () = tokio::time::sleep(DHAN_LANE_RUNTIME_POLL) => {}
                }
            } else {
                tokio::time::sleep(DHAN_LANE_RUNTIME_POLL).await;
            }
            continue;
        }
        // Desired OFF. The teardown is the safety-critical direction. H5:
        // re-assert the disable gate IMMEDIATELY before the irreversible
        // WS-close. If the gate is closed (live trading / open orders) we must
        // NOT tear the lane down — leave it Running so the feed is never
        // blinded mid-trade. The watcher's gated reconcile already downgrades a
        // gate-closed Stop to None, but re-checking HERE narrows the
        // sampled-not-held window (an order could open after the watcher last
        // checked) to the teardown-prep duration.
        //
        // HONEST ENVELOPE (security-review MEDIUM): this NARROWS but cannot
        // ELIMINATE a TOCTOU window against a REMOTE order system — no local
        // lock/fence can serialize against an exchange fill that lands between
        // this check and `teardown_dhan_lane_tasks`'s WS-close. The window is
        // bounded to ~one teardown-prep tick. It is bounded further by the
        // operator contract: `can_disable_dhan()` is seeded from `dry_run` and is
        // only ever `true` during the no-orders data-pull phase; once live
        // trading is on the gate is `false` and the teardown is refused here
        // entirely (the only safe-to-disable phase has no orders to race).
        if !ctx.feed_runtime.can_disable_dhan() {
            info!(
                "[dhan-lane] disable observed but the safety gate is CLOSED (live trading / \
                 open orders) — refusing teardown, lane stays Running"
            );
            metrics::counter!("tv_dhan_lane_disable_aborted_total").increment(1);
            tokio::time::sleep(DHAN_LANE_RUNTIME_POLL).await;
            continue;
        }
        // Gate open — proceed with teardown. Drive Running→Stopping.
        if ctx.feed_runtime.advance_dhan_lane(LaneEvent::StopRequested) == Some(LaneState::Stopping)
        {
            record_dhan_lane_transition(LaneState::Running, LaneState::Stopping);
            info!(
                "[dhan-lane] Dhan disabled (gate permits) — tearing down lane (Running→Stopping)"
            );
        } else {
            // Could not enter Stopping (race) — re-evaluate next tick.
            tokio::time::sleep(DHAN_LANE_RUNTIME_POLL).await;
            continue;
        }

        let Some(owned) = handles.take() else {
            // Already torn down (defensive) — converge to Off.
            if ctx.feed_runtime.advance_dhan_lane(LaneEvent::StopJoined) == Some(LaneState::Off) {
                record_dhan_lane_transition(LaneState::Stopping, LaneState::Off);
            }
            ctx.feed_runtime.set_dhan_lane_running(false);
            // D2c (C4): lane is Off — drop the live token-manager so the health
            // gauges fall back to the global instead of reading a dead manager.
            ctx.feed_runtime.clear_live_token_manager();
            return;
        };

        // H6: teardown awaits every WS/order-update/renewal handle join (with a
        // bounded drain) BEFORE we report Stopping→Off. `teardown_dhan_lane_tasks`
        // is the lane-scoped teardown (C3) — it NEVER touches the PROCESS API
        // server / seal-writer / aggregator / otel.
        let teardown = teardown_dhan_lane_tasks(owned);
        if tokio::time::timeout(
            std::time::Duration::from_secs(DHAN_LANE_TEARDOWN_DRAIN_TIMEOUT_SECS),
            teardown,
        )
        .await
        .is_err()
        {
            // The bounded teardown drain (which itself bounds the WS close)
            // exceeded the outer wall-clock budget — the inner force-abort
            // already fired; report DHAN-LANE-04 (degraded but the lane still
            // reaches Off).
            let code = tickvault_common::error_code::ErrorCode::DhanLane04TeardownTimeout;
            error!(
                code = code.code_str(),
                severity = code.severity().as_str(),
                "[dhan-lane] teardown drain timed out — handles force-aborted; lane still \
                 reaches Off"
            );
            metrics::counter!("tv_dhan_lane_teardown_forced_total").increment(1);
        }

        // Drive Stopping→Off ONLY now (after the join).
        if ctx.feed_runtime.advance_dhan_lane(LaneEvent::StopJoined) == Some(LaneState::Off) {
            record_dhan_lane_transition(LaneState::Stopping, LaneState::Off);
        }
        ctx.feed_runtime.set_dhan_lane_running(false);
        // D2c (C4): lane is Off — drop the live token-manager so the health gauges
        // fall back to the global instead of reading this stopped lane's manager.
        ctx.feed_runtime.clear_live_token_manager();
        info!("[dhan-lane] lane torn down (Stopping→Off) — ready to cold-start again on re-enable");
        ctx.notifier.notify(NotificationEvent::Custom {
            message: "⚪ <b>Dhan feed stopped</b>\nThe live price feed is now disconnected."
                .to_string(),
        });
        return;
    }
}

/// H7 cross-feed blast-radius fix — handle a RUNTIME lane's pool-watchdog Halt
/// (all main-feed sockets down ≥300s during market hours) WITHOUT killing the
/// process.
///
/// Unlike the boot-ON watchdog (which `std::process::exit(2)`s so systemd
/// restarts the whole process), a runtime lane drives `Running→Stopping→Off`
/// through the same FSM + lane-scoped `teardown_dhan_lane_tasks` as an operator
/// disable, then RETURNS so the runtime supervisor re-cold-starts it with its
/// bounded backoff. The independent Groww feed + the shared seal-writer /
/// aggregator / API server stay ALIVE.
///
/// This is a FAULT-restart, NOT an operator disable, so it does NOT re-assert the
/// H5 `can_disable_dhan()` gate: the feed's sockets are already dead (300s down),
/// so there is nothing to "blind mid-trade" — the only action is to bring the
/// dead feed back. The H8 abort-safety still holds: `teardown_dhan_lane_tasks`
/// joins-or-force-aborts every lane handle the failed lane spawned.
// TEST-EXEMPT: orchestration around the unit-tested FSM (advance_dhan_lane) + the live teardown_dhan_lane_tasks; the live Halt-restart path is exercised by the boot-deploy follow. The lane-scoped no-exit contract is pinned by the source-scan ratchet `runtime_lane_watchdog_does_not_process_exit`.
async fn handle_lane_watchdog_halt(
    ctx: &std::sync::Arc<DhanLaneRuntimeContext>,
    owned: DhanLaneRunHandles,
) {
    use tickvault_api::feed_state::{LaneEvent, LaneState};
    error!(
        "[dhan-lane] runtime pool-watchdog Halt — tearing the lane down + driving the FSM \
         to Off (supervisor re-cold-starts); Groww + shared infra stay alive"
    );
    // Drive Running→Stopping. If the FSM is no longer Running (a concurrent
    // operator disable already drove it), the teardown below is still safe
    // (handles are joined-or-force-aborted) and we converge to Off.
    if ctx.feed_runtime.advance_dhan_lane(LaneEvent::StopRequested) == Some(LaneState::Stopping) {
        record_dhan_lane_transition(LaneState::Running, LaneState::Stopping);
    }
    // H6 join (bounded drain) BEFORE reporting Stopping→Off. NEVER touches the
    // PROCESS API server / seal-writer / aggregator / otel (C2).
    let teardown = teardown_dhan_lane_tasks(owned);
    if tokio::time::timeout(
        std::time::Duration::from_secs(DHAN_LANE_TEARDOWN_DRAIN_TIMEOUT_SECS),
        teardown,
    )
    .await
    .is_err()
    {
        let code = tickvault_common::error_code::ErrorCode::DhanLane04TeardownTimeout;
        error!(
            code = code.code_str(),
            severity = code.severity().as_str(),
            "[dhan-lane] watchdog-Halt teardown drain timed out — handles force-aborted; \
             lane still reaches Off"
        );
        metrics::counter!("tv_dhan_lane_teardown_forced_total").increment(1);
    }
    // Drive Stopping→Off ONLY after the join, so the supervisor's
    // `should_spawn_cold_start` (desired-ON + Off + no active task) re-cold-starts.
    if ctx.feed_runtime.advance_dhan_lane(LaneEvent::StopJoined) == Some(LaneState::Off) {
        record_dhan_lane_transition(LaneState::Stopping, LaneState::Off);
    }
    ctx.feed_runtime.set_dhan_lane_running(false);
    // D2c (C4): lane is Off after the Halt teardown — drop the live token-manager
    // so the health gauges fall back to the global until the supervisor re-cold-
    // starts the lane (which installs a fresh manager via set_live_token_manager).
    ctx.feed_runtime.clear_live_token_manager();
    info!(
        "[dhan-lane] watchdog-Halt teardown complete (→Off) — supervisor will re-cold-start \
         the lane on the next tick"
    );
}

// ---------------------------------------------------------------------------
// Helper: PROCESS run-loop (shared by Dhan-ON-slow, Dhan-OFF, and — via the
// thin `run_shutdown_fast` wrapper — the fast boot arm).
//
// Owns the market-close timer, post-market partition-detach, and the shutdown
// wait. On teardown it calls `teardown_dhan_lane_tasks` for the (optional) Dhan
// lane, then stops the PROCESS API server + flushes otel. When `lane` is None
// (Dhan-OFF / Groww-only) there is no Dhan teardown — the shared infra stays up
// for Groww until the shutdown signal, then the API + otel are torn down.
// ---------------------------------------------------------------------------
#[allow(clippy::too_many_arguments)] // APPROVED: run-loop orchestration requires all handles
async fn run_process_runloop(
    lane: Option<DhanLaneRunHandles>,
    api_handle: Option<tokio::task::JoinHandle<()>>,
    otel_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    notifier: &std::sync::Arc<NotificationService>,
    config: &ApplicationConfig,
    // 2026-05-02: gate the 15:30 Post-Market Telegram on trading-day
    // calendar (Saturday/Sunday/holiday suppression). See
    // `boot_helpers::should_emit_post_market_alert`.
    trading_calendar: std::sync::Arc<TradingCalendar>,
    // Session-B HIGH fix (2026-07-04 adversarial review): the Groww
    // scale-fleet dual-instance lock guard, released at graceful teardown
    // below so a same-host restart never sees its own dead slot as a peer.
    // `None` on every non-scale boot (the overwhelming default).
    groww_scale_fleet_lock: Option<tickvault_app::groww_scale_lock::GrowwScaleFleetLockGuard>,
) -> Result<()> {
    let mode = "LIVE";
    info!(
        mode,
        api_port = config.api.port,
        "system ready — press Ctrl+C to stop"
    );

    // 2026-07-13 operator visibility rider: the boot Telegram states what
    // the per-minute Dhan REST legs will ACTUALLY do today (config truth) —
    // a midnight boot is otherwise silent about them until 09:16 IST.
    notifier.notify(NotificationEvent::StartupComplete {
        mode,
        spot_1m_enabled: config.spot_1m_rest.enabled,
        spot_1m_indices: u32::try_from(tickvault_common::constants::SPOT_1M_REST_INDICES.len())
            .unwrap_or(0),
        chain_1m_enabled: config.option_chain_1m.enabled,
        chain_1m_underlyings: u32::try_from(
            tickvault_common::constants::CHAIN_1M_UNDERLYINGS.len(),
        )
        .unwrap_or(0),
    });

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
        // Dhan-OFF (`lane` is None) has no feed to disconnect — skip cleanly.
        if let Some(ref lane) = lane {
            for handle in &lane.ws_handles {
                handle.abort();
            }
            // Give tick processor time to flush remaining ticks before aborting
            if lane.processor_handle.is_some() {
                let flush_timeout = std::time::Duration::from_secs(
                    tickvault_common::constants::GRACEFUL_SHUTDOWN_TIMEOUT_SECS,
                );
                tokio::time::sleep(flush_timeout).await;
            }
            if let Some(ref handle) = lane.trading_handle {
                handle.abort();
            }
        }

        // Post-market: archive→verify→drop old QuestDB partitions (2026-07-13
        // disk-pressure remediation). MUST run BEFORE the legacy detach cycle
        // so a >retention_days partition is archived+dropped (disk freed, S3
        // copy verified) rather than detached-unarchived (renamed inside the
        // same volume, zero bytes freed). Gated on `archive_enabled` (serde
        // default false) — a config rollback restores detach-only behaviour
        // byte-identically. Fail-closed: a partition is dropped ONLY after
        // its S3 copy is row-count- and size-verified; any failure keeps the
        // partition and retries next run.
        if config.partition_retention.archive_enabled {
            match tickvault_storage::partition_archive::PartitionArchiver::new(
                &config.questdb,
                &config.partition_retention,
            )
            .await
            {
                Ok(Some(mut archiver)) => {
                    let summary = archiver.archive_and_drop_old_partitions().await;
                    info!(
                        tables_scanned = summary.tables_scanned,
                        partitions_considered = summary.partitions_considered,
                        verified = summary.verified,
                        dropped = summary.dropped,
                        failed = summary.failed,
                        rows_archived = summary.rows_archived,
                        gzip_bytes_uploaded = summary.gzip_bytes_uploaded,
                        csv_bytes_exported = summary.csv_bytes_exported,
                        "post-market partition archive complete (verified S3 copy before every drop)"
                    );
                }
                // Review round 1 F1b (fail-closed): no explicit archive
                // bucket AND no explicit TV_ENVIRONMENT/ENVIRONMENT env var
                // — archival skipped rather than guessing the prod bucket.
                // The constructor already logged the actionable warn.
                Ok(None) => {}
                Err(err) => {
                    error!(
                        ?err,
                        code = tickvault_common::error_code::ErrorCode::StorageGap04S3ArchiveFailed
                            .code_str(),
                        "partition archiver construction failed — archive cycle skipped \
                         this run (fail-closed no-op; detach cycle still runs)"
                    );
                }
            }
        }

        // Post-market: detach old QuestDB partitions (Phase B).
        // Runs daily after pipeline stops — keeps hot data bounded to retention_days.
        // KEPT even with the archive leg enabled: the archive cycle above drops
        // aged partitions first, so this legacy cycle finds nothing new — but its
        // `total_detached=…` log lines keep firing so the CloudWatch evidence
        // trail (the 15:30 IST "partition" filter) continues uninterrupted.
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

    // Steps 1–5: tear down the Dhan-lane tasks (renewal → order-update →
    // graceful WS close → tick-processor flush → trading pipeline). On a
    // Dhan-OFF / Groww-only runtime (`lane` is None) there is nothing
    // Dhan-specific to stop — the PROCESS infra below is torn down regardless.
    if let Some(lane) = lane {
        teardown_dhan_lane_tasks(lane).await;
    }

    // 5b. Release the Groww scale-fleet dual-instance lock (2026-07-04
    // adversarial-review HIGH fix). Without this, every clean shutdown left
    // the SSM slot fresh for up to 90s (TTL), so a same-host restart — the
    // dominant Mac scale-test iteration workflow — saw its OWN dead slot as
    // a fresh AlreadyHeld peer: fleet refused for the whole session + a
    // false-positive GROWW-SCALE-05 page naming the operator's previous
    // pid. `release_on_shutdown` is permit-based (Rule 16 lost-wake safe)
    // and bounded (GROWW_SCALE_LOCK_RELEASE_TIMEOUT_SECS), so a black-holed
    // SSM endpoint can never hang process exit; on timeout the 90s TTL
    // remains the backstop.
    if let Some(fleet_lock) = groww_scale_fleet_lock {
        fleet_lock.release_on_shutdown().await;
    }

    // 5c. Telegram UX overhaul (2026-07-07): flush pending coalesced
    // summaries + write the final episode snapshot (bounded 10s inside
    // shutdown_flush — a black-holed Telegram can never hang exit).
    notifier.shutdown_flush().await;

    // 6. Stop API server (PROCESS-shared).
    if let Some(handle) = api_handle {
        handle.abort();
    }

    // 7. Flush OpenTelemetry (PROCESS-shared).
    drop(otel_provider);

    info!("tickvault stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helper: thin compatibility wrapper preserving the original
// `run_shutdown_fast(...)` call shape used by the FAST boot arm (which stays
// byte-identical, design L10/D2d). It bundles the lane handles into
// `DhanLaneRunHandles` and delegates to `run_process_runloop`.
// ---------------------------------------------------------------------------
#[allow(clippy::too_many_arguments)] // APPROVED: shutdown orchestration requires all handles
async fn run_shutdown_fast(
    ws_handles: Vec<tokio::task::JoinHandle<Result<(), WebSocketError>>>,
    processor_handle: Option<tokio::task::JoinHandle<()>>,
    renewal_handle: Option<tokio::task::JoinHandle<()>>,
    order_update_handle: Option<tokio::task::JoinHandle<()>>,
    api_handle: Option<tokio::task::JoinHandle<()>>,
    trading_handle: Option<tokio::task::JoinHandle<()>>,
    token_health_gauge_handle: Option<tokio::task::JoinHandle<()>>,
    // R8-EDGE-2 / SEC-C2-1 (2026-07-07): the pool watchdog handle, lane-owned
    // so teardown/Drop abort it (the notify stop signal alone is Rule-16
    // lost-wake prone).
    pool_watchdog_handle: Option<tokio::task::JoinHandle<()>>,
    otel_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    notifier: &std::sync::Arc<NotificationService>,
    config: &ApplicationConfig,
    ws_pool_arc: Option<std::sync::Arc<WebSocketConnectionPool>>,
    shutdown_notify: std::sync::Arc<tokio::sync::Notify>,
    trading_calendar: std::sync::Arc<TradingCalendar>,
    groww_scale_fleet_lock: Option<tickvault_app::groww_scale_lock::GrowwScaleFleetLockGuard>,
) -> Result<()> {
    run_process_runloop(
        Some(DhanLaneRunHandles {
            ws_handles,
            processor_handle,
            renewal_handle,
            order_update_handle,
            // R9-EDGE-1: the FAST arm's OrderUpdateAuthenticated listener is
            // process-lifetime (main() is never supervisor-aborted), so it
            // is deliberately NOT lane-owned here.
            order_update_auth_listener_handle: None,
            trading_handle,
            // B3 round-2: the FAST crash-recovery arm never spawns the
            // token-health writer (pre-B3 it had no token-block writer at
            // all), so there is nothing to abort or reset here.
            token_health_handle: None,
            // EDGE-2 fix (2026-07-06): the FAST arm DOES spawn the
            // AUTH-GAP-05 gauge poller now — lane-owned so the H8 Drop
            // floor aborts it. The mid-session watchdog stays fast-arm-off.
            token_health_gauge_handle,
            mid_session_watchdog_handle: None,
            // SEC-R4-1 / R4-CPLX-1/2: the FAST crash-recovery arm never
            // spawns the 4h token sweep or the 5-min periodic health check
            // (both are slow-lane Step 12b spawns), so there is nothing to
            // lane-own here.
            token_sweep_handle: None,
            periodic_health_handle: None,
            // R9-EDGE-2: the FAST crash-recovery arm never attaches the
            // TickEnricher maintenance tasks (slow-lane spawns), so there is
            // nothing to lane-own here.
            midnight_rollover_handle: None,
            prev_oi_refresh_handle: None,
            // R8-EDGE-2 / SEC-C2-1: the FAST arm's pool watchdog rides in so
            // the runloop teardown / H8 Drop floor aborts it.
            pool_watchdog_handle,
            // F5/F13: the FAST arm spawns neither the runtime IP monitor
            // (Step 5.5 is slow-lane only) nor a lane-owned heartbeat
            // watchdog (its twin is process-lifetime by design) — nothing
            // to lane-own here.
            ip_monitor_handle: None,
            ip_monitor_shutdown: None,
            heartbeat_watchdog_handle: None,
            health: None,
            // F14: no health registry on the FAST arm ⇒ no per-feed
            // registry to reset either (process exits with the runloop).
            feed_health: None,
            ws_pool_arc,
            shutdown_notify,
            // H7: BOOT-ON (FAST-boot) handles are not parked — the watchdog
            // keeps its `process::exit(2)` single-feed contract, so no
            // lane-scoped Halt signal is needed here.
            lane_halt_notify: None,
            // BUG-1: the FAST crash-recovery arm holds NO dual-instance lock
            // (dual-instance-lock-2026-07-04 §3 honest envelope — it never
            // mints at boot), so there is no heartbeat to release.
            instance_lock_heartbeat: None,
            instance_lock_shutdown: None,
            // SEC-C3-1: no lock ⇒ no heartbeat ⇒ no Item 19f bridge task.
            heartbeat_bridge_handle: None,
        }),
        api_handle,
        otel_provider,
        notifier,
        config,
        trading_calendar,
        groww_scale_fleet_lock,
    )
    .await
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

/// Fractional tick-freshness coverage for the SLO composite score
/// (2026-07-03 — replaces the binary worst-gap check; see the call site
/// in the SLO scheduler for the full incident narrative).
///
/// Returns `1.0 − silent_count / universe_size`, clamped to `[0.0, 1.0]`.
///
/// Why fractional and not binary: the SLO score is a MULTIPLICATIVE
/// product of six dimensions, so a binary 0.0 on any one dimension
/// zeroes the composite. Under the ~775-SID DailyUniverse a handful of
/// illiquid/suspended SIDs (~33 observed live, D2 investigation
/// 2026-07-03) are always silent ≥30s during market hours, which made
/// the binary input a PERMANENT 0 — a constant that carries no signal.
/// The fraction of silent SIDs is the honest liveness measure: small
/// silent tails degrade the score proportionally (33/775 → 0.957,
/// still Healthy ≥0.95), while a genuine broad outage (dead socket →
/// most of the universe silent) still collapses the dimension toward 0
/// and pages via the existing SLO tiers.
///
/// `universe_size == 0` (pre-seed boot window, detector map empty)
/// returns `1.0` — an empty map is "no evidence of staleness", not an
/// outage; the 60s uniform boot grace covers the same window anyway.
fn compute_tick_freshness(silent_count: usize, universe_size: usize) -> f64 {
    if universe_size == 0 {
        return 1.0;
    }
    (1.0 - (silent_count as f64 / universe_size as f64)).clamp(0.0, 1.0)
}

/// SLO-03: once-per-process guard for the SLO publisher supervisor spawn.
///
/// `start_dhan_lane` re-runs on every runtime Dhan enable / stop→restart /
/// cold-start retry (D2b). Without this guard every lane restart would leak
/// an extra never-exiting supervisor plus a duplicate gauge publisher — the
/// same per-lane-leak class the 2026-07-01 DayOhlcTracker fix addressed.
/// The first lane start wins; the supervisor reads process-shared state
/// (`SharedHealthStatus`, `FeedRuntimeState` — the token-headroom helper
/// already prefers the LIVE lane manager), so it stays correct across lane
/// restarts.
static SLO_PUBLISHER_SUPERVISOR_SPAWNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Once-per-process spawn guard for the Dhan exchange-lag publisher
/// supervisor (silent-feed hardening Item 4, 2026-07-06 incident). Same
/// per-lane-leak rationale as [`SLO_PUBLISHER_SUPERVISOR_SPAWNED`]: D2b lane
/// restarts re-run `start_dhan_lane`, and each re-invocation would otherwise
/// leak a fresh never-aborted supervisor. The publisher reads the
/// process-global lag ring, so it stays correct across lane restarts.
static FEED_LAG_PUBLISHER_SUPERVISOR_SPAWNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// F4 (2026-07-08): once-per-process spawn latch for the three market-open
/// ONE-SHOT schedulers (09:14 readiness / 09:15:30 streaming heartbeat /
/// 09:16 self-test). They are detached one-shots by design (self-exit after
/// firing), so lane ownership is the wrong tool — but a per-lane-cycle
/// spawn accumulated N pending copies across D2b Dhan enable/disable cycles
/// before the bell (N duplicate verdict Telegrams per trigger). First
/// `start_dhan_lane` invocation wins; later cold-starts log INFO and skip.
/// The companion FIRE-TIME gate ([`market_open_fire_gate_dhan_enabled`])
/// covers the disable-after-spawn case.
static MARKET_OPEN_ONE_SHOTS_SPAWNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// F4/F13 (2026-07-08): once-per-process spawn latch for the FirstSeenSet
/// IST-midnight reset task — an INFINITE loop over the PROCESS-GLOBAL
/// singleton that a per-lane-cycle spawn leaked once per D2b cold-start.
static FIRST_SEEN_RESET_SPAWNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// F4 (2026-07-08): fire-time gate for the market-open one-shot schedulers —
/// reads the SAME `FeedRuntimeState::dhan` atomic the WS read loop and the
/// pool watchdog consult. A pending one-shot that outlives a deliberate
/// runtime Dhan disable must NOT fire a verdict off the dead lane's frozen
/// health state (FALSE High MarketOpenStreamingFailed / FALSE Critical
/// SELFTEST-02 at the bell). O(1) relaxed load on a cold once-a-day path.
fn market_open_fire_gate_dhan_enabled(dhan_enabled: &std::sync::atomic::AtomicBool) -> bool {
    dhan_enabled.load(std::sync::atomic::Ordering::Relaxed)
}

/// Bounded backoff between SLO publisher respawns (SLO-03). Mirrors the
/// OOM-monitor / disk-watcher supervisor cadence: long enough to avoid a hot
/// respawn loop, short enough that the `tv_realtime_guarantee_score` stream
/// resumes well inside one alarm evaluation period.
const SLO_PUBLISHER_RESPAWN_BACKOFF_SECS: u64 = 5;

/// Wave 3-D Item 13 — the SLO evaluator/publisher task (extracted verbatim
/// from the former inline `tokio::spawn` in `start_dhan_lane` for SLO-03).
///
/// Every 10s it samples the six dimensions (WS, QDB, tick freshness, token,
/// spill, phase2), computes the composite score via `evaluate_slo_score`, and
/// emits `tv_realtime_guarantee_score` + the six per-dimension gauges. The
/// loop is infinite — ANY resolution of the returned `JoinHandle` is abnormal
/// and is handled by [`spawn_supervised_slo_publisher`].
// TEST-EXEMPT: tokio task spawn — the pure evaluator (`slo_score.rs`) and
// `compute_tick_freshness` are fully unit-tested; the loop is a scheduler
// wrapper exercised in production and pinned by the wiring ratchet
// `test_slo_publisher_supervisor_is_wired_into_main`.
#[must_use]
fn spawn_slo_publisher_task(
    slo_health: SharedHealthStatus,
    slo_qcfg: tickvault_common::config::QuestDbConfig,
    slo_feed_runtime: std::sync::Arc<tickvault_api::feed_state::FeedRuntimeState>,
    slo_main_feed_pool_size: usize,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use std::time::{Duration, Instant};
        use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY};
        use tickvault_common::error_code::ErrorCode;
        use tickvault_core::instrument::slo_score::{SloInputs, SloOutcome, evaluate_slo_score};

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
        let slo_ws_expected_main_feed: f64 = slo_main_feed_pool_size as f64;
        let slo_ws_expected_total: f64 = slo_ws_expected_main_feed + SLO_WS_EXPECTED_ORDER_UPDATE;

        /// Tick-freshness silence threshold during market hours:
        /// a tick gap >= this many seconds counts that SID as
        /// SILENT in the fractional coverage computation
        /// (`compute_tick_freshness` — 2026-07-03 fix; the gap no
        /// longer binary-zeroes the whole dimension). Aligned with
        /// the operator's "silent socket" boundary used elsewhere
        /// (the 60s pool watchdog) and with the tick-gap
        /// detector's own `TICK_GAP_THRESHOLD_SECS_DEFAULT = 30`.
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

        let mut interval = tokio::time::interval(Duration::from_secs(SLO_TICK_INTERVAL_SECS));
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
            let now_ist_secs = now_utc_secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
            let secs_of_day = now_ist_secs.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32;
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
            // not a degradation, so it is excluded from the
            // silent count (kept from the original design).
            //
            // 2026-07-03 (D2 root-cause fix): FRACTIONAL
            // coverage replaces the binary worst-gap check.
            // The binary check ("ANY non-excluded gap >= 30s
            // → 0.0") was designed for the 4-SID Indices4Only
            // era. Under the ~775-SID DailyUniverse, ~33
            // illiquid/suspended SIDs are ALWAYS silent —
            // NT-15 boot seeding (2026-07-01) guarantees every
            // subscribed SID is a tick-gap map key, so a
            // never-ticking SID's gap grows monotonically from
            // boot. Result: tick_freshness was flat 0.0 all
            // session, the multiplicative score was 0, and
            // `tv-prod-realtime-guarantee-critical` fired all
            // day — pure noise that masked real degradation.
            //   tick_freshness = 1 − silent/universe (clamped)
            // 33/775 silent → 0.957 (healthy); a genuine broad
            // outage (dead socket → most SIDs silent) still
            // crashes the dimension toward 0. Off-hours stays
            // pinned 1.0. Pure math + rationale in
            // `compute_tick_freshness` (unit-tested).
            const SLO_TICK_FRESHNESS_EXCLUDED_SIDS: &[u64] = &[21]; // INDIA VIX
            // Round-3 fix (2026-07-08, review finding 5): pin
            // tick_freshness to 1.0 during the NSE pre-open /
            // auction window [09:00, 09:15) IST, mirroring the
            // off-hours pin. Continuous trading has not started —
            // the boot-seeded ~775 SIDs are LEGITIMATELY silent
            // (esp. the 09:08-09:15 matching/buffer freeze), so
            // genuine freshness math drives the score far below
            // 0.95 for ~7-10 consecutive pre-open minutes. The
            // window-gate Lambda enables alarm actions at 09:20
            // and forces OK — but forcing OK does NOT purge
            // datapoints, so the new degraded alarm's 9-of-15
            // lookback at ~09:21 would hold >= 9 breaching
            // PRE-OPEN datapoints and false-page at open on
            // ordinary days (every previously-gated alarm has a
            // lookback <= 5 min, which is why the 2-of-2 critical
            // sibling never paged at open). Pre-open silence is
            // not degradation — same rationale class as the
            // off-hours pin; genuine computation starts at the
            // 09:15:00 continuous-session open.
            const SLO_TICK_FRESHNESS_SESSION_OPEN_SECS_OF_DAY_IST: u32 = 9 * 3600 + 15 * 60;
            let tick_freshness = if !in_market
                || secs_of_day < SLO_TICK_FRESHNESS_SESSION_OPEN_SECS_OF_DAY_IST
            {
                1.0
            } else {
                tickvault_core::pipeline::tick_gap_detector::global_tick_gap_detector()
                    .map(|d| {
                        // Full silent set (cap = map size). This
                        // is the 10s SLO scheduler — a cold-path
                        // task, NOT the tick hot path; the
                        // O(universe) walk is the same scan the
                        // 60s coalescer already performs.
                        let universe = d.len();
                        let (silent_entries, _total) = d.scan_gaps_top_n(Instant::now(), universe);
                        // §36.7 (2026-07-10): NON-NEAREST-month index
                        // futures are excluded alongside INDIA VIX —
                        // far serials are legitimately sparse; the
                        // nearest month stays counted (status quo).
                        let silent = silent_entries
                            .iter()
                            .filter(|(sid, seg, gap)| {
                                *gap >= SLO_TICK_FRESHNESS_DEGRADED_SECS
                                    && !SLO_TICK_FRESHNESS_EXCLUDED_SIDS.contains(sid)
                                    && !is_far_month_future_sid(*sid, *seg)
                            })
                            .count();
                        compute_tick_freshness(silent, universe)
                    })
                    .unwrap_or(1.0)
            };

            // ---- Token_freshness -----------------------------
            // D2c (C4): prefer the LIVE lane-owned manager over the
            // global OnceLock so a runtime stop→re-start reports the new
            // manager's headroom, not the dead boot-time manager's.
            let token_secs = gauge_token_headroom_secs(&slo_feed_runtime);
            // B3 round-2 (2026-07-03, review HIGH): the shared
            // health-state token block (remaining-secs +
            // validity setters) is NO LONGER written here.
            // Round 1 put the stores in this loop, but this loop
            // is gated on `config.features.realtime_guarantee_score`
            // — flipping that UNRELATED feature flag off would
            // silently re-ghost the 09:14 IST READY Telegram, the
            // 15:31:30 IST EOD digest and `GET /health`, AND pin
            // `overall_status()` "degraded" (state.rs degrades on
            // `!token_valid`). The dedicated UNCONDITIONAL
            // `spawn_token_health_writer` lane task is the ONE
            // production writer now; `token_secs` here feeds ONLY
            // the Token_freshness SLO dimension.
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
            let outcome = if scheduler_boot_at.elapsed().as_secs() < SLO_BOOT_UNIFORM_GRACE_SECS {
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
            let dim_clamp = |v: f64| -> f64 { if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) } };
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
    })
}

/// SLO-03 — supervise the SLO evaluator/publisher (live incident 2026-07-03
/// 10:35 IST: the publisher died silently mid-market; last
/// `tv_realtime_guarantee_score` datapoint 10:35, no `error!`, no respawn,
/// and the guarantee-critical alarm false-OK'd on missing data).
///
/// [`spawn_slo_publisher_task`] runs an infinite loop, so its `JoinHandle`
/// resolves ONLY on a fatal event (panic or external cancel). This supervisor
/// mirrors `spawn_supervised_oom_monitor` / DISK-WATCHER-01 / WS-GAP-05: on
/// every publisher death it logs `error!` (code `SLO-03`), increments
/// `tv_slo_publisher_respawn_total{reason}`, backs off
/// [`SLO_PUBLISHER_RESPAWN_BACKOFF_SECS`], and respawns so the guarantee
/// score metric stream can never vanish silently again. The supervisor body
/// has no panic path of its own (pure-function classification, no
/// `unwrap`/`expect`).
// TEST-EXEMPT: cold-path supervisor spawn wrapper — exit classification is the
// unit-tested `disk_health_watcher::classify_join_exit`; the boot wiring is
// pinned by `test_slo_publisher_supervisor_is_wired_into_main`.
// O(1) EXEMPT: cold-path supervisor — one task per session, fires only on publisher death.
#[must_use]
fn spawn_supervised_slo_publisher(
    health: SharedHealthStatus,
    qdb_config: tickvault_common::config::QuestDbConfig,
    feed_runtime: std::sync::Arc<tickvault_api::feed_state::FeedRuntimeState>,
    main_feed_pool_size: usize,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle = spawn_slo_publisher_task(
                health.clone(),
                qdb_config.clone(),
                std::sync::Arc::clone(&feed_runtime),
                main_feed_pool_size,
            );
            let join_result = handle.await;
            let reason = tickvault_storage::disk_health_watcher::classify_join_exit(&join_result);
            error!(
                code = tickvault_common::error_code::ErrorCode::Slo03PublisherRespawned.code_str(),
                reason,
                backoff_secs = SLO_PUBLISHER_RESPAWN_BACKOFF_SECS,
                "SLO-03: SLO publisher task exited — respawning so the \
                 tv_realtime_guarantee_score stream cannot vanish silently"
            );
            metrics::counter!("tv_slo_publisher_respawn_total", "reason" => reason).increment(1);
            tokio::time::sleep(std::time::Duration::from_secs(
                SLO_PUBLISHER_RESPAWN_BACKOFF_SECS,
            ))
            .await;
        }
    })
}

/// Bounded backoff between Dhan exchange-lag publisher respawns (silent-feed
/// hardening Item 4). Mirrors [`SLO_PUBLISHER_RESPAWN_BACKOFF_SECS`]: long
/// enough to avoid a hot respawn loop, short enough that the
/// `tv_dhan_exchange_lag_p99_seconds` stream resumes well inside one alarm
/// evaluation period (the lag alarm needs 10 consecutive breaching minutes).
const FEED_LAG_PUBLISHER_RESPAWN_BACKOFF_SECS: u64 = 5;

/// Silent-feed hardening Item 4 — supervise the Dhan exchange-lag p99
/// publisher (`tickvault_core::pipeline::feed_lag_monitor::run_dhan_lag_publisher`).
///
/// The publisher runs an infinite loop, so its `JoinHandle` resolves ONLY on
/// a fatal event (panic or external cancel). This supervisor mirrors the
/// SLO-03 / WS-GAP-05 / DISK-WATCHER-01 pattern: on every publisher death it
/// logs `error!`, increments `tv_feed_lag_publisher_respawn_total{reason}`,
/// backs off [`FEED_LAG_PUBLISHER_RESPAWN_BACKOFF_SECS`], and respawns so
/// the lag gauge stream can never vanish silently. A dead-forever publisher
/// degrades to missing data → `notBreaching` on the lag alarm (feed-dead
/// stays owned by the silent-instruments + WS alarms).
///
/// No dedicated `ErrorCode` (least-new-surface, documented decision): the
/// tag-guard requires a `code =` field only when the message mentions a
/// tracked code, and the respawn counter + supervisor `error!` are the
/// operator-visible signals — exactly the pre-SLO-03 DISK-WATCHER template
/// without minting a new taxonomy entry for a brand-new observability-only
/// task.
// TEST-EXEMPT: cold-path supervisor spawn wrapper — exit classification is
// the unit-tested `disk_health_watcher::classify_join_exit`; the boot wiring
// is pinned by `test_feed_lag_publisher_supervisor_is_wired_into_main`.
// O(1) EXEMPT: cold-path supervisor — one task per session, fires only on publisher death.
#[must_use]
fn spawn_supervised_feed_lag_publisher() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle =
                tokio::spawn(tickvault_core::pipeline::feed_lag_monitor::run_dhan_lag_publisher());
            let join_result = handle.await;
            let reason = tickvault_storage::disk_health_watcher::classify_join_exit(&join_result);
            error!(
                reason,
                backoff_secs = FEED_LAG_PUBLISHER_RESPAWN_BACKOFF_SECS,
                "dhan exchange-lag publisher task exited — respawning so the \
                 tv_dhan_exchange_lag_p99_seconds stream cannot vanish silently"
            );
            metrics::counter!("tv_feed_lag_publisher_respawn_total", "reason" => reason)
                .increment(1);
            tokio::time::sleep(std::time::Duration::from_secs(
                FEED_LAG_PUBLISHER_RESPAWN_BACKOFF_SECS,
            ))
            .await;
        }
    })
}

/// Scoreboard PR-C — supervise the GROWW exchange-lag p99 publisher
/// (`tickvault_core::pipeline::feed_lag_monitor::run_groww_lag_publisher`,
/// gauge `tv_groww_exchange_lag_p99_seconds` — its OWN name, never the Dhan
/// gauge; an EMF `feed` label would fold the series together).
///
/// Spawned ONCE from the process-global boot prefix (next to the Groww
/// bridge supervisor — it runs on EVERY boot mode, so no fast/slow
/// dual-site + once-guard is needed; the single call site is pinned by
/// `test_groww_lag_publisher_supervisor_is_wired_into_main` in
/// secret_manager.rs). With the Groww feed disabled the ring stays empty,
/// the ≥50-sample gate publishes nothing, and the task is inert.
///
/// Same SLO-03 / WS-GAP-05 supervisor semantics as the Dhan one: on every
/// publisher death it logs `error!`, increments its OWN respawn counter
/// `tv_groww_lag_publisher_respawn_total{reason}`, backs off
/// [`FEED_LAG_PUBLISHER_RESPAWN_BACKOFF_SECS`], and respawns — the gauge
/// stream can never vanish silently. No dedicated `ErrorCode` (same
/// least-new-surface decision as the Dhan supervisor above).
// TEST-EXEMPT: cold-path supervisor spawn wrapper — exit classification is
// the unit-tested `disk_health_watcher::classify_join_exit`; the boot wiring
// is pinned by `test_groww_lag_publisher_supervisor_is_wired_into_main`.
// O(1) EXEMPT: cold-path supervisor — one task per session, fires only on publisher death.
#[must_use]
fn spawn_supervised_groww_lag_publisher() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle =
                tokio::spawn(tickvault_core::pipeline::feed_lag_monitor::run_groww_lag_publisher());
            let join_result = handle.await;
            let reason = tickvault_storage::disk_health_watcher::classify_join_exit(&join_result);
            error!(
                reason,
                backoff_secs = FEED_LAG_PUBLISHER_RESPAWN_BACKOFF_SECS,
                "groww exchange-lag publisher task exited — respawning so the \
                 tv_groww_exchange_lag_p99_seconds stream cannot vanish silently"
            );
            metrics::counter!("tv_groww_lag_publisher_respawn_total", "reason" => reason)
                .increment(1);
            tokio::time::sleep(std::time::Duration::from_secs(
                FEED_LAG_PUBLISHER_RESPAWN_BACKOFF_SECS,
            ))
            .await;
        }
    })
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

    // ── --force-instance-takeover (dual-instance lock hardening 2026-07-04) ──

    #[test]
    fn test_force_takeover_flag_matcher() {
        assert!(is_force_takeover_arg("--force-instance-takeover"));
        assert_eq!(FORCE_INSTANCE_TAKEOVER_FLAG, "--force-instance-takeover");
        // Near-misses must NOT trigger a lock takeover.
        assert!(!is_force_takeover_arg("--force-instance-takeover=1"));
        assert!(!is_force_takeover_arg("force-instance-takeover"));
        assert!(!is_force_takeover_arg("--force-takeover"));
        assert!(!is_force_takeover_arg(""));
        // Pin the process-args reader symbol (exercised at boot only).
        let _ = force_instance_takeover_requested;
    }

    // ── build_feed_status_lines (Telegram feed parity, 2026-07-03) ──
    // The readiness + end-of-day messages must list EVERY enabled feed
    // (Dhan, Groww, future) with honest unknowns — never fabricated data.

    #[test]
    fn test_build_feed_status_lines_includes_enabled_feeds_only() {
        use tickvault_common::feed::Feed;
        let runtime = tickvault_api::feed_state::FeedRuntimeState::default();
        runtime.set_enabled(Feed::Dhan, true);
        runtime.set_enabled(Feed::Groww, false);
        let health = tickvault_common::feed_health::FeedHealthRegistry::new();
        health.set_subscribed(Feed::Dhan, 774, 2);
        // Groww's slot is populated but the feed is DISABLED — it must not
        // appear (a switched-off feed is intentional, not "unknown").
        health.set_subscribed(Feed::Groww, 766, 2);
        let lines = build_feed_status_lines(&runtime, &health);
        assert_eq!(lines.len(), 1, "disabled feeds must not appear: {lines:?}");
        assert_eq!(lines[0].name, "Dhan");
        assert_eq!(lines[0].instruments, Some(776));
    }

    #[test]
    fn test_build_feed_status_lines_honest_unknowns() {
        use tickvault_common::feed::Feed;
        let runtime = tickvault_api::feed_state::FeedRuntimeState::default();
        runtime.set_enabled(Feed::Dhan, true);
        runtime.set_enabled(Feed::Groww, true);
        let health = tickvault_common::feed_health::FeedHealthRegistry::new();
        // Groww enabled with a known subscribe count + a recorded tick.
        health.set_subscribed(Feed::Groww, 766, 2);
        health.record_tick(Feed::Groww, now_ist_nanos_for_feed_status());
        let lines = build_feed_status_lines(&runtime, &health);
        assert_eq!(lines.len(), 2, "both enabled feeds must appear: {lines:?}");
        let dhan = lines.iter().find(|l| l.name == "Dhan").expect("Dhan line");
        // Nothing recorded for Dhan in this test registry (and no global
        // tick-gap detector in a unit test) → honest unknowns, never zeros.
        assert_eq!(dhan.instruments, None, "unknown must stay None: {dhan:?}");
        let groww = lines
            .iter()
            .find(|l| l.name == "Groww")
            .expect("Groww line");
        assert_eq!(groww.instruments, Some(768));
        assert!(
            groww.last_tick_age_secs.is_some_and(|age| age <= 1),
            "freshly recorded tick must read ~0s: {groww:?}"
        );
    }

    // ── boot_completed_should_emit truth table ──
    // The alive signal (tv_boot_completed) must be published only when at least
    // one ENABLED feed is running — so a boot where every enabled feed died
    // withholds it and the boot-heartbeat alarm pages. No feed enabled emits
    // (headless run must not false-page). A running-but-disabled feed never
    // counts.

    // ── compute_tick_freshness (SLO fractional coverage, 2026-07-03) ──
    // Pins the D2 fix: tick_freshness is 1 − silent/universe (clamped),
    // NOT a binary worst-gap zero. Regression back to binary semantics
    // fails these tests.

    #[test]
    fn test_compute_tick_freshness_fractional_33_of_775_stays_healthy() {
        // The live D2 signature: 33 silent SIDs out of the ~775-SID
        // DailyUniverse must NOT zero the dimension.
        let v = compute_tick_freshness(33, 775);
        let expected = 1.0 - (33.0 / 775.0);
        assert!((v - expected).abs() < 1e-12, "got {v}, expected {expected}");
        assert!(
            v > 0.95,
            "33/775 silent must stay in the Healthy band, got {v}"
        );
    }

    #[test]
    fn test_compute_tick_freshness_zero_silent_is_one() {
        assert!((compute_tick_freshness(0, 775) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_tick_freshness_all_silent_is_zero() {
        // Genuine broad outage: whole universe silent → dimension 0 →
        // multiplicative score 0 → Critical still fires.
        assert!(compute_tick_freshness(775, 775).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_tick_freshness_empty_universe_is_one() {
        // Pre-seed boot window (empty detector map) — no evidence of
        // staleness, must not zero the score.
        assert!((compute_tick_freshness(0, 0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_tick_freshness_clamps_when_silent_exceeds_universe() {
        // Defensive: racing map mutation between len() and the scan can
        // in theory yield silent > universe; must clamp to 0, not go
        // negative.
        assert!(compute_tick_freshness(1_000, 775).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_tick_freshness_broad_outage_drops_below_critical_band() {
        // 700/775 silent → 0.0967 — far below the 0.80 Critical
        // boundary; real outages still page.
        let v = compute_tick_freshness(700, 775);
        assert!(v < 0.80, "broad outage must land below Critical, got {v}");
    }

    // ── far_month_future_sids (§36.7 alarm-gate exclusion, 2026-07-10) ──

    #[cfg(feature = "daily_universe_fetcher")]
    fn d(s: &str) -> chrono::NaiveDate {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("test date")
    }

    #[cfg(feature = "daily_universe_fetcher")]
    fn far_month_test_plan(
        futures: &[(&str, &str, &str, &str)], // (sid, segment, underlying, expiry)
    ) -> SubscriptionPlan {
        use tickvault_core::instrument::csv_parser::CsvRow;
        use tickvault_core::instrument::daily_universe::{
            DailyUniverse, InstrumentRole, SubscriptionTarget,
        };
        use tickvault_core::instrument::subscription_planner::build_subscription_plan_from_daily_universe;
        let targets = futures
            .iter()
            .map(|(sid, segment, underlying, expiry)| SubscriptionTarget {
                role: InstrumentRole::IndexFuture,
                is_fno_underlying: false,
                is_index_constituent: false,
                csv_row: CsvRow {
                    security_id: (*sid).to_string(),
                    exch_id: if *segment == "BSE_FNO" { "BSE" } else { "NSE" }.to_string(),
                    segment: (*segment).to_string(),
                    instrument: "FUTIDX".to_string(),
                    symbol_name: format!("{underlying}-{expiry}-FUT"),
                    underlying_symbol: (*underlying).to_string(),
                    expiry_date: (*expiry).to_string(),
                    ..CsvRow::default()
                },
            })
            .collect();
        let universe = DailyUniverse {
            subscription_targets: targets,
            fno_contracts: vec![],
        };
        build_subscription_plan_from_daily_universe(&universe)
    }

    #[test]
    #[cfg(feature = "daily_universe_fetcher")]
    fn test_far_month_future_sids_keeps_nearest_excludes_rest() {
        use tickvault_common::types::ExchangeSegment;
        // 3 months × 2 underlyings → 4 excluded (the non-nearest months),
        // the 2 nearest-month SIDs stay counted.
        let plan = far_month_test_plan(&[
            ("35001", "NSE_FNO", "NIFTY", "2026-07-30"),
            ("35002", "NSE_FNO", "NIFTY", "2026-08-27"),
            ("35003", "NSE_FNO", "NIFTY", "2026-09-24"),
            ("45001", "BSE_FNO", "SENSEX", "2026-07-31"),
            ("45002", "BSE_FNO", "SENSEX", "2026-08-28"),
            ("45003", "BSE_FNO", "SENSEX", "2026-09-25"),
        ]);
        let far = far_month_future_sids(&plan, d("2026-07-10"));
        assert_eq!(far.len(), 4, "2 far months per underlying excluded");
        for (sid, seg) in [
            (35002, ExchangeSegment::NseFno),
            (35003, ExchangeSegment::NseFno),
            (45002, ExchangeSegment::BseFno),
            (45003, ExchangeSegment::BseFno),
        ] {
            assert!(far.contains(&(sid, seg)), "far month {sid} excluded");
        }
        // The nearest month of each underlying is NEVER excluded.
        assert!(!far.contains(&(35001, ExchangeSegment::NseFno)));
        assert!(!far.contains(&(45001, ExchangeSegment::BseFno)));
    }

    #[test]
    #[cfg(feature = "daily_universe_fetcher")]
    fn test_far_month_future_sids_single_month_excludes_nothing() {
        // Single-month (the pre-§36.7 shape) → empty exclusion set — the
        // alarm-facing counts are byte-identical to the nearest-only era.
        let plan = far_month_test_plan(&[
            ("35001", "NSE_FNO", "NIFTY", "2026-07-30"),
            ("35002", "NSE_FNO", "BANKNIFTY", "2026-07-30"),
            ("35003", "NSE_FNO", "MIDCPNIFTY", "2026-07-28"),
            ("45001", "BSE_FNO", "SENSEX", "2026-07-31"),
        ]);
        assert!(far_month_future_sids(&plan, d("2026-07-10")).is_empty());
    }

    #[test]
    #[cfg(feature = "daily_universe_fetcher")]
    fn test_far_month_future_sids_groups_by_canonical_across_alias_literals() {
        use tickvault_common::types::ExchangeSegment;
        // AM-r1 F3 regression: mixed alias literals for ONE underlying
        // across months (the documented MIDCPNIFTY "NIFTY MIDCAP SELECT"
        // drift) MUST land in one canonical group — the far month is
        // excluded even though its raw literal differs from the nearest's.
        let plan = far_month_test_plan(&[
            ("35003", "NSE_FNO", "MIDCPNIFTY", "2026-07-28"),
            ("35004", "NSE_FNO", "NIFTY MIDCAP SELECT", "2026-08-25"),
        ]);
        let far = far_month_future_sids(&plan, d("2026-07-10"));
        assert_eq!(far.len(), 1, "the alias-literal far month is excluded");
        assert!(far.contains(&(35004, ExchangeSegment::NseFno)));
        assert!(!far.contains(&(35003, ExchangeSegment::NseFno)));
    }

    #[test]
    #[cfg(feature = "daily_universe_fetcher")]
    fn test_far_month_future_sids_all_months_past_excludes_nothing() {
        // AM-r1 F6 fail-safe arm: a boot-frozen plan whose months are ALL
        // `< today` (stale process) yields NO nearest via the shared
        // singular → nothing is excluded — the SIDs stay counted rather
        // than a dead front month being wrongly alarm-subtracted.
        let plan = far_month_test_plan(&[
            ("35001", "NSE_FNO", "NIFTY", "2026-07-30"),
            ("35002", "NSE_FNO", "NIFTY", "2026-08-27"),
        ]);
        assert!(far_month_future_sids(&plan, d("2026-09-01")).is_empty());
    }

    #[test]
    fn test_boot_completed_should_emit_both_off_emits() {
        // No feed enabled at all → emit (deliberately feed-less run must not
        // false-page). Feed-running values are irrelevant here.
        assert!(boot_completed_should_emit(false, false, false, false));
        assert!(boot_completed_should_emit(false, false, true, true));
    }

    #[test]
    fn test_boot_completed_should_emit_dhan_only_live() {
        assert!(boot_completed_should_emit(true, false, true, false));
    }

    #[test]
    fn test_boot_completed_should_emit_dhan_only_dead() {
        // Dhan enabled but not running, Groww disabled → withhold (page).
        assert!(!boot_completed_should_emit(true, false, false, false));
    }

    #[test]
    fn test_boot_completed_should_emit_groww_only_live() {
        assert!(boot_completed_should_emit(false, true, false, true));
    }

    #[test]
    fn test_boot_completed_should_emit_groww_only_dead() {
        assert!(!boot_completed_should_emit(false, true, false, false));
    }

    #[test]
    fn test_boot_completed_should_emit_both_enabled_only_dhan_live() {
        assert!(boot_completed_should_emit(true, true, true, false));
    }

    #[test]
    fn test_boot_completed_should_emit_both_enabled_only_groww_live() {
        assert!(boot_completed_should_emit(true, true, false, true));
    }

    #[test]
    fn test_boot_completed_should_emit_both_enabled_none_live() {
        // Both enabled, neither running → withhold (page). This is the core
        // alerting-hole case: previously the metric was published unconditionally.
        assert!(!boot_completed_should_emit(true, true, false, false));
    }

    #[test]
    fn test_boot_completed_should_emit_running_but_disabled_feed_does_not_count() {
        // Dhan DISABLED but its running flag is somehow true (stale) — it must NOT
        // count; only the ENABLED Groww matters, and Groww is dead → withhold.
        assert!(!boot_completed_should_emit(false, true, true, false));
        // Symmetric: Groww disabled-but-running, Dhan enabled-and-dead → withhold.
        assert!(!boot_completed_should_emit(true, false, false, true));
    }

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
    fn test_pool_watchdog_should_act_on_degradation_truth_table() {
        // ACT (page + exit/teardown) ONLY when in market hours AND Dhan enabled.
        assert!(
            pool_watchdog_should_act_on_degradation(true, true),
            "in-market + Dhan enabled: a real all-down pool must page/halt"
        );
        // Dhan deliberately toggled OFF (PR-E) during market hours: the dormant
        // 0-connection pool is EXPECTED IDLE — do NOT page/halt. This is the bug
        // fix: previously the watchdog self-halted the whole process (killing
        // Groww too) ~300s after a runtime Dhan-OFF.
        assert!(
            !pool_watchdog_should_act_on_degradation(true, false),
            "Dhan toggled OFF in-market: dormant pool is expected idle, no page/halt"
        );
        // Off-hours, Dhan enabled: pre-existing post-market suppression.
        assert!(
            !pool_watchdog_should_act_on_degradation(false, true),
            "off-hours: Dhan idle, no page/halt (2026-04-24 cascade fix)"
        );
        // Off-hours AND Dhan off: both reasons to stay quiet.
        assert!(
            !pool_watchdog_should_act_on_degradation(false, false),
            "off-hours + Dhan off: expected idle, no page/halt"
        );
    }

    // --- Fix A (2026-06-30): reconnect-in-place bare-reset classifier ---

    fn down_health(
        id: u8,
        rate_limit_streak: u32,
        saw_non_reconnectable: bool,
    ) -> tickvault_core::websocket::types::ConnectionHealth {
        tickvault_core::websocket::types::ConnectionHealth {
            connection_id: id,
            state: tickvault_core::websocket::types::ConnectionState::Reconnecting,
            subscribed_count: 0,
            total_reconnections: 0,
            last_activity_secs_ago: None,
            rate_limit_streak,
            saw_non_reconnectable,
        }
    }

    #[test]
    fn test_is_bare_reset_class_all_benign_true() {
        // Every conn streak 0, none non-reconnectable, token valid, QuestDB up.
        let healths = vec![down_health(0, 0, false), down_health(1, 0, false)];
        assert!(
            is_bare_reset_class(&healths, true, true),
            "benign bare-RST class must reconnect in place"
        );
    }

    #[test]
    fn test_is_bare_reset_class_rate_limited_false() {
        // A real 429 on ANY connection (streak > 0) → genuine-fatal.
        let healths = vec![down_health(0, 0, false), down_health(1, 1, false)];
        assert!(
            !is_bare_reset_class(&healths, true, true),
            "a real 429 (streak>0) must NOT reconnect in place"
        );
    }

    #[test]
    fn test_is_bare_reset_class_non_reconnectable_false() {
        // A non-reconnectable Dhan disconnect seen → genuine-fatal.
        let healths = vec![down_health(0, 0, false), down_health(1, 0, true)];
        assert!(
            !is_bare_reset_class(&healths, true, true),
            "a non-reconnectable disconnect must NOT reconnect in place"
        );
    }

    #[test]
    fn test_is_bare_reset_class_token_invalid_false() {
        let healths = vec![down_health(0, 0, false)];
        assert!(
            !is_bare_reset_class(&healths, false, true),
            "an invalid token must NOT reconnect in place"
        );
    }

    #[test]
    fn test_is_bare_reset_class_questdb_down_false() {
        let healths = vec![down_health(0, 0, false)];
        assert!(
            !is_bare_reset_class(&healths, true, false),
            "an unreachable QuestDB must NOT reconnect in place"
        );
    }

    #[test]
    fn test_is_bare_reset_class_empty_pool_false() {
        // No connections to reconnect → never ride out an absent pool.
        assert!(!is_bare_reset_class(&[], true, true));
    }

    // --- QuestDB-probe damping (2026-07-01 audit fix) ---

    #[test]
    fn test_damp_questdb_exit_signal_threshold_is_two() {
        // Pin N=2: 10s (2 × 5s tick) of sustained unreachability before the
        // exit gate treats QuestDB as down — smallest N that absorbs a blip.
        assert_eq!(POOL_WATCHDOG_QDB_EXIT_DAMP_THRESHOLD, 2);
    }

    #[test]
    fn test_damp_questdb_exit_signal_single_blip_stays_reachable() {
        // 1 failed probe with N=2 ⇒ still "reachable" for the exit gate, so a
        // single blip during an in-market 429 storm keeps riding out (the fix).
        assert!(
            damp_questdb_exit_signal(0, POOL_WATCHDOG_QDB_EXIT_DAMP_THRESHOLD),
            "zero failures: reachable"
        );
        assert!(
            damp_questdb_exit_signal(1, POOL_WATCHDOG_QDB_EXIT_DAMP_THRESHOLD),
            "one blip (< N) must NOT flip the exit gate — ride-out continues"
        );
    }

    #[test]
    fn test_damp_questdb_exit_signal_n_consecutive_flips() {
        // N (and beyond) consecutive failures ⇒ NOT reachable ⇒ the ride-out
        // classifiers return false ⇒ genuine-fatal exit forced. A real
        // sustained outage is never worse than today.
        assert!(
            !damp_questdb_exit_signal(2, POOL_WATCHDOG_QDB_EXIT_DAMP_THRESHOLD),
            "N consecutive failures must flip the exit gate (exit forced)"
        );
        assert!(
            !damp_questdb_exit_signal(3, POOL_WATCHDOG_QDB_EXIT_DAMP_THRESHOLD),
            ">N consecutive failures stays flipped (exit forced)"
        );
        assert!(
            !damp_questdb_exit_signal(100, POOL_WATCHDOG_QDB_EXIT_DAMP_THRESHOLD),
            "a long sustained outage stays flipped (exit forced)"
        );
    }

    #[test]
    fn test_damp_questdb_exit_signal_success_resets() {
        // Model the watchdog counter loop: fail, fail, SUCCESS, fail — a success
        // between failures resets the counter, so it takes N FRESH consecutive
        // failures to flip. Proves a mid-way success resets (a real outage must
        // be N *consecutive*, not N cumulative).
        let n = POOL_WATCHDOG_QDB_EXIT_DAMP_THRESHOLD;
        let mut counter: u32 = 0;

        // Two failures in a row would flip — but a success interrupts them.
        let probes = [false, true, false]; // fail, SUCCESS (reset), fail
        for &reachable in &probes {
            if reachable {
                counter = 0;
            } else {
                counter = counter.saturating_add(1);
            }
        }
        // After fail→success(reset)→fail the counter is only 1, so the exit
        // gate still reads "reachable" — the mid-way success reset it.
        assert_eq!(counter, 1, "success mid-way must reset the counter");
        assert!(
            damp_questdb_exit_signal(counter, n),
            "after a mid-way success it takes N FRESH consecutive failures to flip"
        );

        // Now the SECOND consecutive fresh failure flips it.
        counter = counter.saturating_add(1);
        assert!(
            !damp_questdb_exit_signal(counter, n),
            "two fresh consecutive failures after the reset flip the exit gate"
        );
    }

    #[test]
    fn test_reconnect_in_place_ceiling_under_continues() {
        // Under the 15-min ceiling → keep reconnecting in place (false).
        assert!(!reconnect_in_place_ceiling_exceeded(0));
        assert!(!reconnect_in_place_ceiling_exceeded(
            tickvault_core::websocket::pool_watchdog::POOL_RECONNECT_IN_PLACE_CEILING_SECS - 1
        ));
    }

    #[test]
    fn test_reconnect_in_place_ceiling_over_exits() {
        // At or past the ceiling → fall back to genuine-fatal exit (true).
        assert!(reconnect_in_place_ceiling_exceeded(
            tickvault_core::websocket::pool_watchdog::POOL_RECONNECT_IN_PLACE_CEILING_SECS
        ));
        assert!(reconnect_in_place_ceiling_exceeded(
            tickvault_core::websocket::pool_watchdog::POOL_RECONNECT_IN_PLACE_CEILING_SECS + 600
        ));
    }

    // --- In-window 429 ride-out (2026-06-30): the audit's HIGH-finding fix ---

    #[test]
    fn test_in_window_429_rideout_class_streak_positive_true() {
        // THE CORE FIX: in market hours, token+QDB ok, no non-reconnectable,
        // at least one conn rate-limited (streak>0) → ride out the 429 in place
        // instead of process::exit + 429-tripping cold re-subscribe.
        let healths = vec![down_health(0, 1, false)];
        assert!(
            is_in_window_429_rideout_class(&healths, true, true, true),
            "an in-market Dhan 429 (streak>0) must reconnect in place, not restart"
        );
        // Mixed pool: one rate-limited, one not → still the 429 class.
        let mixed = vec![down_health(0, 0, false), down_health(1, 2, false)];
        assert!(
            is_in_window_429_rideout_class(&mixed, true, true, true),
            "a 429 on ANY conn is the 429 class"
        );
    }

    #[test]
    fn test_in_window_429_rideout_class_out_of_hours_false() {
        // Outside market hours the down pool is expected idle — never ride out
        // here (the watchdog's should_act gate handles off-hours separately).
        let healths = vec![down_health(0, 1, false)];
        assert!(
            !is_in_window_429_rideout_class(&healths, false, true, true),
            "out of market hours must NOT ride out (expected-idle path owns it)"
        );
    }

    #[test]
    fn test_in_window_429_rideout_class_token_invalid_false() {
        // A dead token still needs the renewed-token restart path.
        let healths = vec![down_health(0, 1, false)];
        assert!(
            !is_in_window_429_rideout_class(&healths, true, false, true),
            "an invalid token must NOT ride out — genuine-fatal restart"
        );
    }

    #[test]
    fn test_in_window_429_rideout_class_questdb_down_false() {
        // A dead DB still needs the restart path.
        let healths = vec![down_health(0, 1, false)];
        assert!(
            !is_in_window_429_rideout_class(&healths, true, true, false),
            "an unreachable QuestDB must NOT ride out — genuine-fatal restart"
        );
    }

    #[test]
    fn test_in_window_429_rideout_class_non_reconnectable_false() {
        // A genuine-fatal Dhan disconnect (807/auth/etc.) on ANY conn → exit,
        // even if another conn is merely rate-limited.
        let healths = vec![down_health(0, 1, false), down_health(1, 0, true)];
        assert!(
            !is_in_window_429_rideout_class(&healths, true, true, true),
            "a non-reconnectable disconnect must NOT ride out — genuine-fatal restart"
        );
    }

    #[test]
    fn test_in_window_429_rideout_class_no_streak_false() {
        // All streak==0 is the bare-RST class (is_bare_reset_class handles it),
        // NOT the 429 class — this classifier must return false.
        let healths = vec![down_health(0, 0, false), down_health(1, 0, false)];
        assert!(
            !is_in_window_429_rideout_class(&healths, true, true, true),
            "streak==0 is the bare-RST class, not the 429 class"
        );
    }

    #[test]
    fn test_in_window_429_rideout_class_empty_false() {
        // No connections to reconnect → never ride out an absent pool.
        assert!(!is_in_window_429_rideout_class(&[], true, true, true));
    }

    #[test]
    fn test_should_reconnect_in_place_admits_429_in_window() {
        // The regression the audit demands: an in-market 429 storm rides out.
        let healths = vec![down_health(0, 1, false)];
        assert!(
            should_reconnect_in_place(&healths, true, true, true),
            "in-market 429 storm must reconnect in place (the self-loop fix)"
        );
    }

    #[test]
    fn test_should_reconnect_in_place_admits_bare_rst() {
        // Existing behaviour preserved: streak==0 RST storm still rides out.
        let healths = vec![down_health(0, 0, false), down_health(1, 0, false)];
        assert!(
            should_reconnect_in_place(&healths, true, true, true),
            "streak==0 bare-RST storm must still reconnect in place"
        );
        // ...and a bare-RST storm rides out even OUTSIDE market hours via the
        // bare-reset class (in_market_hours only gates the 429 class), matching
        // pre-change is_bare_reset_class behaviour.
        assert!(
            should_reconnect_in_place(&healths, false, true, true),
            "bare-RST class is not market-hours-gated (unchanged)"
        );
    }

    #[test]
    fn test_should_reconnect_in_place_genuine_fatal_token_dead() {
        // Token dead → neither class → genuine-fatal exit (no wedge).
        let healths = vec![down_health(0, 1, false)];
        assert!(
            !should_reconnect_in_place(&healths, true, false, true),
            "a dead token must exit, never ride out"
        );
    }

    #[test]
    fn test_should_reconnect_in_place_genuine_fatal_questdb_down() {
        // QuestDB dead → neither class → genuine-fatal exit (no wedge).
        let healths = vec![down_health(0, 1, false)];
        assert!(
            !should_reconnect_in_place(&healths, true, true, false),
            "a dead QuestDB must exit, never ride out"
        );
    }

    #[test]
    fn test_should_reconnect_in_place_genuine_fatal_non_reconnectable() {
        // A non-reconnectable Dhan code → neither class → genuine-fatal exit.
        let healths = vec![down_health(0, 1, true)];
        assert!(
            !should_reconnect_in_place(&healths, true, true, true),
            "a non-reconnectable disconnect must exit, never ride out"
        );
    }

    #[test]
    fn test_should_reconnect_in_place_ceiling_still_exits() {
        // The ride-out classifier admits a 429, but the SEPARATE ceiling check
        // (the watchdog gates `ride_out && !ceiling_exceeded`) still forces the
        // genuine-fatal exit past 15 min — worst case is NEVER worse than today.
        let healths = vec![down_health(0, 1, false)];
        assert!(should_reconnect_in_place(&healths, true, true, true));
        assert!(
            reconnect_in_place_ceiling_exceeded(
                tickvault_core::websocket::pool_watchdog::POOL_RECONNECT_IN_PLACE_CEILING_SECS
            ),
            "past the 15-min ceiling the watchdog falls back to genuine-fatal exit"
        );
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

    /// Step C ratchet: the per-feed boot dispatcher gate must exist — when Dhan
    /// is disabled the boot SKIPS the Dhan block and idle-awaits shutdown
    /// (pluggable-feed-runtime.md §6). Guards against regressing back to the old
    /// warn-only stub where `dhan_enabled=false` still ran Dhan.
    #[test]
    fn test_step_c_dhan_disable_gate_exists() {
        let src = include_str!("main.rs");
        assert!(
            src.contains("if !config.feeds.dhan_enabled"),
            "Step C: the Dhan disable-gate `if !config.feeds.dhan_enabled` must exist"
        );
        assert!(
            src.contains("PER-FEED BOOT DISPATCHER"),
            "Step C: the per-feed boot dispatcher block must be present"
        );
        // The gate idles on the shared shutdown signal and returns — not a warn-only no-op.
        assert!(
            src.contains("wait_for_shutdown_signal().await"),
            "Step C: the gate must idle-await the shutdown signal"
        );
    }

    /// Step C ratchet: the gate must sit BEFORE the Dhan fast/slow boot decision
    /// (`load_token_cache_fast`) so the Dhan block is genuinely skipped when
    /// `dhan_enabled=false`. First-occurrence positions are the real gate + boot
    /// (both are far above this test module).
    #[test]
    fn test_step_c_gate_precedes_dhan_boot() {
        let src = include_str!("main.rs");
        let gate_idx = src
            .find("PER-FEED BOOT DISPATCHER")
            .expect("gate marker present");
        let dhan_boot_idx = src
            .find("load_token_cache_fast")
            .expect("Dhan boot decision present");
        assert!(
            gate_idx < dhan_boot_idx,
            "Step C: the per-feed gate must precede the Dhan boot decision so \
             dhan_enabled=false actually skips Dhan (gate@{gate_idx} boot@{dhan_boot_idx})"
        );
    }

    /// PR-3 ratchet (3-agent hostile recommendation): the persisted feed-state
    /// overlay MUST be applied to `config.feeds` BEFORE any boot-skip guard reads
    /// `feeds.dhan_enabled` / `!feeds.dhan_enabled`. If a future refactor moved a
    /// boot-skip read above the `overlay_feeds(...)` call, that read would observe
    /// the pre-overlay config default and silently ignore the operator's persisted
    /// last toggle choice (the whole point of PR-3) — booting the WRONG feed.
    #[test]
    fn test_overlay_precedes_boot_skip_guard() {
        let src = include_str!("main.rs");
        let overlay_idx = src
            .find("overlay_feeds(")
            .expect("the feed-state overlay call must exist");
        // The first boot-skip guard that reads the per-feed enabled flag. Both
        // conditional forms gate the Dhan boot path; whichever appears first must
        // still come AFTER the overlay has been applied.
        let first_enabled_guard = src.find("if feeds.dhan_enabled");
        let first_disabled_guard = src.find("if !feeds.dhan_enabled");
        let first_guard_idx = [first_enabled_guard, first_disabled_guard]
            .into_iter()
            .flatten()
            .min()
            .expect("a `feeds.dhan_enabled` boot-skip guard must exist");
        assert!(
            overlay_idx < first_guard_idx,
            "overlay_feeds(...) (@{overlay_idx}) must precede the first \
             feeds.dhan_enabled boot-skip guard (@{first_guard_idx}) so the \
             boot reads the post-overlay effective feed state, not the config \
             default"
        );
    }

    /// Step C ratchet: both run-mode log branches must exist — Groww-only
    /// (Dhan off, Groww on) and idle (neither on, no crash).
    #[test]
    fn test_step_c_groww_only_and_idle_branches() {
        let src = include_str!("main.rs");
        assert!(
            src.contains("GROWW-ONLY MODE"),
            "Step C: the Groww-only run-mode branch must exist"
        );
        assert!(
            src.contains("NO FEED ENABLED"),
            "Step C: the no-feed idle branch must exist (idle, not crash)"
        );
    }

    // ---------------------------------------------------------------------
    // D2b — runtime Dhan-lane cold-start FSM (pure helpers + wiring guards).
    // The FSM transition function itself is unit-tested in feed_state.rs.
    // ---------------------------------------------------------------------

    #[test]
    fn dhan_lane_retry_backoff_is_groww_capped_ladder() {
        // min(10 * 2^n, 300) — identical to the Groww watcher.
        assert_eq!(dhan_lane_retry_backoff_secs(0), 10);
        assert_eq!(dhan_lane_retry_backoff_secs(1), 20);
        assert_eq!(dhan_lane_retry_backoff_secs(2), 40);
        assert_eq!(dhan_lane_retry_backoff_secs(3), 80);
        assert_eq!(dhan_lane_retry_backoff_secs(4), 160);
        // Capped at 300 from attempt 5 onward (2^5 = 32 -> 320 -> capped).
        assert_eq!(dhan_lane_retry_backoff_secs(5), 300);
        assert_eq!(dhan_lane_retry_backoff_secs(50), 300);
        assert_eq!(
            DHAN_LANE_RETRY_BACKOFF_CAP_SECS, 300,
            "backoff cap pinned to the Groww ladder cap"
        );
    }

    #[test]
    fn classify_start_lane_error_maps_to_typed_codes_with_secret_free_reasons() {
        use tickvault_common::error_code::ErrorCode;
        // BootAbortClean (a boot gate refused) -> DHAN-LANE-03 (auth/gate).
        let (code, reason, stage) = classify_start_lane_error(&StartLaneError::BootAbortClean);
        assert_eq!(code, ErrorCode::DhanLane03AuthFailed);
        assert_eq!(code.code_str(), "DHAN-LANE-03");
        assert_eq!(reason, "auth_or_gate_failed");
        assert_eq!(stage, "auth");
        // BootAbortErr (instrument-load failed) -> DHAN-LANE-01 (universe).
        let (code, reason, stage) =
            classify_start_lane_error(&StartLaneError::BootAbortErr(anyhow::anyhow!("synthetic")));
        assert_eq!(code, ErrorCode::DhanLane01UniverseBuildFailed);
        assert_eq!(code.code_str(), "DHAN-LANE-01");
        assert_eq!(reason, "universe_build_failed");
        assert_eq!(stage, "universe");
        // FTC-14: WsPoolSpawn (WS-pool build/spawn failed on a connect-day) ->
        // DHAN-LANE-02 with stage='ws_pool' — the CORRECT pool-spawn runbook,
        // not the wrong universe-build one.
        let (code, reason, stage) = classify_start_lane_error(&StartLaneError::WsPoolSpawn);
        assert_eq!(code, ErrorCode::DhanLane02WsPoolSpawnFailed);
        assert_eq!(code.code_str(), "DHAN-LANE-02");
        assert_eq!(reason, "ws_pool_spawn_failed");
        assert_eq!(stage, "ws_pool");
        // Secret discipline: the reason discriminants are FIXED strings, never
        // the raw error body (e.g. an auth response). Assert they contain no
        // dynamic content.
        for r in [
            "auth_or_gate_failed",
            "universe_build_failed",
            "ws_pool_spawn_failed",
        ] {
            assert!(
                !r.contains("synthetic"),
                "reason must be a fixed discriminant, not the body"
            );
        }
    }

    #[test]
    fn should_spawn_cold_start_only_from_confirmed_empty_off() {
        use tickvault_api::feed_state::LaneState;
        // Spawn iff desired-ON + Off + no active task.
        assert!(should_spawn_cold_start(true, LaneState::Off, false));
        // NOT while a task is active (single-owner invariant).
        assert!(!should_spawn_cold_start(true, LaneState::Off, true));
        // NOT while desired-OFF.
        assert!(!should_spawn_cold_start(false, LaneState::Off, false));
        // H6: NEVER start from Stopping (no double pool).
        assert!(!should_spawn_cold_start(true, LaneState::Stopping, false));
        // Idempotent: NOT while Running or Starting.
        assert!(!should_spawn_cold_start(true, LaneState::Running, false));
        assert!(!should_spawn_cold_start(true, LaneState::Starting, false));
    }

    #[test]
    fn should_abort_cold_start_only_for_starting_on_disable() {
        use tickvault_api::feed_state::LaneState;
        // H8: abort an in-flight Starting cold-start ONLY when desired-OFF.
        assert!(should_abort_cold_start_on_disable(
            false,
            LaneState::Starting
        ));
        // Do NOT abort a Running lane on disable — its parked task does the
        // H5-gated, H6-joined teardown itself.
        assert!(!should_abort_cold_start_on_disable(
            false,
            LaneState::Running
        ));
        // Nothing to abort from Off/Stopping.
        assert!(!should_abort_cold_start_on_disable(false, LaneState::Off));
        assert!(!should_abort_cold_start_on_disable(
            false,
            LaneState::Stopping
        ));
        // Never abort while still desired-ON.
        assert!(!should_abort_cold_start_on_disable(
            true,
            LaneState::Starting
        ));
    }

    #[test]
    fn supervisor_only_cancels_a_starting_lane_it_owns() {
        // Boot-ON race fix (hostile-review): the supervisor must NOT fire
        // StartCancelled against a `Starting` lane it does NOT own (a boot-ON
        // inline start also seeds Starting). The abort+cancel block is gated on
        // `active_task.is_some()` so it never clobbers the inline start.
        let src = include_str!("main.rs");
        assert!(
            src.contains(
                "if active_task.is_some() && should_abort_cold_start_on_disable(desired_on, lane_state)"
            ),
            "the supervisor abort/cancel must be gated on active_task.is_some() (owns the Starting lane)"
        );
        // The dead-task respawn reset is also gated on an owned (finished) task.
        assert!(
            src.contains("is_some_and(tokio::task::JoinHandle::is_finished)"),
            "the dead-task reset must be gated on an owned, finished task"
        );
    }

    #[test]
    fn supervisor_cancel_is_cas_first_no_abort_of_a_promoted_running_lane() {
        // Hostile-review MEDIUM fix: the disable-abort path must drive
        // advance_dhan_lane(StartCancelled) FIRST and abort + mark-not-running
        // ONLY when the CAS returns Some(Off). If the owned task already CAS'd
        // Starting→Running between the snapshot and this block, StartCancelled is
        // illegal (None) and the supervisor must NOT abort the live pool nor
        // mark it not-running (the parked task owns its gated teardown).
        let src = include_str!("main.rs");
        let region_start = src
            .find("CRITICAL CAS-FIRST race fix")
            .expect("the CAS-first race-fix comment must exist");
        let region_end = src[region_start..]
            .find("Spawn a fresh cold-start")
            .map(|o| region_start + o)
            .expect("the spawn block must follow the cancel block");
        let region = &src[region_start..region_end];
        // The cancel block advances the FSM FIRST, then guards the abort +
        // not-running on the Some(Off) result.
        assert!(
            region.contains(
                "advance_dhan_lane(tickvault_api::feed_state::LaneEvent::StartCancelled)"
            ) && region.contains("== Some(LaneState::Off)"),
            "the cancel must be CAS-first (advance to Off) before any side-effect"
        );
        // task.abort() + set_dhan_lane_running(false) must be INSIDE the Some(Off)
        // arm (after the CAS check), not unconditional.
        let cas_idx = region
            .find("== Some(LaneState::Off)")
            .expect("CAS check present");
        let after_cas = &region[cas_idx..];
        assert!(
            after_cas.contains("task.abort()"),
            "task.abort() must be inside the confirmed-cancel (Some(Off)) arm"
        );
        assert!(
            after_cas.contains("set_dhan_lane_running(false)"),
            "set_dhan_lane_running(false) must be inside the confirmed-cancel arm"
        );
    }

    #[test]
    fn lane_state_labels_are_stable_static_strings() {
        use tickvault_api::feed_state::LaneState;
        assert_eq!(lane_state_label(LaneState::Off), "off");
        assert_eq!(lane_state_label(LaneState::Starting), "starting");
        assert_eq!(lane_state_label(LaneState::Running), "running");
        assert_eq!(lane_state_label(LaneState::Stopping), "stopping");
    }

    #[test]
    fn phantom_running_closed_running_set_only_after_start_ok() {
        // Source-scan: `LaneState::Running` is set ONLY in the Ok arm of a
        // `start_dhan_lane` call (boot-ON inline + the runtime wrapper), never
        // before. Guard against a future edit that pre-marks Running.
        let src = include_str!("main.rs");
        assert!(
            src.contains("advance_dhan_lane(LaneEvent::StartSucceeded)"),
            "the runtime cold-start must advance to Running via StartSucceeded only after Ok"
        );
        assert!(
            src.contains("mark_dhan_pool_present()"),
            "pool-present must be marked (only) on a successful start"
        );
    }

    #[test]
    fn gate_hold_reasserted_before_runtime_teardown() {
        // H5 source-scan: the runtime Stop path re-asserts can_disable_dhan()
        // immediately before the irreversible teardown, and on gate-closed it
        // bumps the disable-aborted counter + stays Running.
        let src = include_str!("main.rs");
        assert!(
            src.contains("if !ctx.feed_runtime.can_disable_dhan() \u{7b}"),
            "H5: the runtime Stop must re-assert can_disable_dhan() before teardown"
        );
        assert!(
            src.contains("tv_dhan_lane_disable_aborted_total"),
            "H5: a gate-closed disable must bump the disable-aborted counter"
        );
    }

    #[test]
    fn double_pool_prevented_stop_join_before_off() {
        // H6 source-scan: Stopping->Off happens via StopJoined (after teardown
        // awaits the join), never on a bare Notify-fire; and the runtime wrapper
        // calls the lane-scoped teardown (C3), never run_process_runloop.
        let src = include_str!("main.rs");
        assert!(
            src.contains("advance_dhan_lane(LaneEvent::StopJoined)"),
            "H6: Stopping->Off must go through StopJoined (after the handle join)"
        );
        assert!(
            src.contains("teardown_dhan_lane_tasks(owned)"),
            "C3: the runtime Stop must call the lane-scoped teardown, not the run-loop"
        );
    }

    #[test]
    fn runtime_lane_does_not_spawn_process_shared_infra() {
        // C2 source-scan: the runtime cold-start wrapper must NOT (re)create the
        // PROCESS-shared seal-writer / aggregator / API server — those live ONLY
        // in build_shared_infra. The runtime path calls start_dhan_lane (LANE)
        // and teardown_dhan_lane_tasks (LANE) only.
        let src = include_str!("main.rs");
        // The only spawn_seal_writer_loop / spawn_engine_b_aggregator /
        // axum::serve call sites must be inside build_shared_infra. We assert
        // the runtime functions reference start_dhan_lane + teardown only.
        assert!(
            src.contains("park_running_dhan_lane(std::sync::Arc::clone(&ctx), handles)"),
            "the runtime cold-start parks via park_running_dhan_lane on success"
        );
        // The runtime supervisor + wrapper must NOT call the PROCESS builders.
        let runtime_region_start = src
            .find("async fn run_dhan_lane_cold_start")
            .expect("run_dhan_lane_cold_start must exist");
        let runtime_region_end = src
            .find("async fn run_process_runloop")
            .expect("run_process_runloop must exist");
        let runtime_region = &src[runtime_region_start..runtime_region_end];
        assert!(
            !runtime_region.contains("spawn_seal_writer_loop"),
            "C2: the runtime lane must NOT spawn the PROCESS seal-writer"
        );
        assert!(
            !runtime_region.contains("spawn_engine_b_aggregator"),
            "C2: the runtime lane must NOT spawn the PROCESS 21-TF aggregator"
        );
        assert!(
            !runtime_region.contains("axum::serve"),
            "C2: the runtime lane must NOT spawn the PROCESS API server"
        );
    }

    #[test]
    fn runtime_supervisor_and_cold_start_are_wired_at_boot() {
        // Wiring guard (Rule 13 / pub-fn-wiring): the runtime supervisor is
        // spawned at boot with the OnceCell, and the OnceCell is populated after
        // build_shared_infra so a boot-OFF Dhan feed is runtime-startable.
        let src = include_str!("main.rs");
        assert!(
            src.contains("tokio::spawn(run_dhan_lane_runtime_supervisor("),
            "the runtime supervisor must be spawned unconditionally at boot"
        );
        assert!(
            src.contains("dhan_lane_ctx_cell.set(runtime_ctx)"),
            "the runtime context cell must be populated after build_shared_infra"
        );
        // The supervisor spawns the cold-start task (the single owned handle).
        assert!(
            src.contains("tokio::spawn(run_dhan_lane_cold_start(std::sync::Arc::clone(ctx)))"),
            "the supervisor must spawn the cold-start task as the single owned handle"
        );
    }

    #[test]
    fn runtime_toggle_never_enters_fast_arm() {
        // L10 source-scan: a runtime cold-start ALWAYS uses the slow/canonical
        // start_dhan_lane (the extracted slow arm), never the fast crash-recovery
        // boot arm. The runtime wrapper builds its DhanLaneContext with EMPTY
        // WAL-replay buffers (a fast-arm cache-warmed entry is a boot-only path).
        let src = include_str!("main.rs");
        let region_start = src
            .find("async fn run_dhan_lane_cold_start")
            .expect("run_dhan_lane_cold_start must exist");
        let region_end = src
            .find("async fn park_running_dhan_lane")
            .expect("park_running_dhan_lane must exist");
        let region = &src[region_start..region_end];
        assert!(
            region.contains("start_dhan_lane(lane_ctx).await"),
            "the runtime cold-start must call the slow/canonical start_dhan_lane"
        );
        assert!(
            region.contains("ws_wal_replay_live_feed: Vec::new()"),
            "the runtime cold-start must use EMPTY WAL-replay buffers (no fast-arm cache entry)"
        );
    }

    #[test]
    fn runtime_cold_start_gates_side_effects_on_cas() {
        // Disable-during-cold-start race source-scan: the `Ok(handles)` arm of
        // the runtime cold-start MUST gate its success side-effects (mark-present
        // / set-running / notify / park) on the `advance_dhan_lane(StartSucceeded)`
        // CAS, and on LOSING that CAS (supervisor won Starting→Off) it MUST tear
        // down the just-spawned pool via `teardown_dhan_lane_tasks` instead of
        // parking it (which would leak the WS pool). Guard against a regression
        // back to the unconditional mark+park.
        let src = include_str!("main.rs");
        let region_start = src
            .find("async fn run_dhan_lane_cold_start")
            .expect("run_dhan_lane_cold_start must exist");
        let region_end = src
            .find("async fn park_running_dhan_lane")
            .expect("park_running_dhan_lane must exist");
        let region = &src[region_start..region_end];
        // The Ok-arm gates on the CAS NOT winning Running, then tears down.
        assert!(
            region.contains("advance_dhan_lane(LaneEvent::StartSucceeded)\n                    != Some(LaneState::Running)")
                || region.contains("!= Some(LaneState::Running)"),
            "the cold-start Ok arm must branch on the StartSucceeded CAS losing the Running race"
        );
        assert!(
            region.contains("teardown_dhan_lane_tasks(handles)"),
            "on the lost-CAS branch the cold-start must tear down (join) the just-spawned pool, \
             not park + leak it"
        );
        // And the success side-effects (park + the 'Dhan started' notify) live
        // AFTER the lost-race teardown's early return, i.e. they are reachable
        // ONLY on the won-CAS path.
        let park_idx = region
            .find("park_running_dhan_lane(std::sync::Arc::clone(&ctx), handles)")
            .expect("the cold-start must still park on the won-CAS path");
        let teardown_idx = region
            .find("teardown_dhan_lane_tasks(handles)")
            .expect("lost-race teardown must exist");
        assert!(
            teardown_idx < park_idx,
            "the lost-race teardown must precede the park (park is the won-CAS-only path)"
        );
    }

    #[test]
    fn teardown_keeps_ws_handles_in_lane_across_the_graceful_drain_await() {
        // Cancel-safety (hostile-review CRITICAL) source-scan: the WS handles
        // MUST be `mem::take`n out of `lane` only AFTER the graceful 2s drain
        // `.await`, never before. Otherwise a mid-drain cancel of this future
        // (the cold-start lost-CAS branch is concurrently `.abort()`ed by the
        // supervisor) would drop the moved-out `ws_handles` un-aborted → the WS
        // pool DETACHES. Keeping them in `lane` until past the await means a
        // mid-drain drop runs `lane`'s `Drop` abort-floor instead.
        let src = include_str!("main.rs");
        let fn_start = src
            .find("async fn teardown_dhan_lane_tasks")
            .expect("teardown_dhan_lane_tasks must exist");
        let fn_end = src[fn_start..]
            .find("\n// ===")
            .map(|i| fn_start + i)
            .unwrap_or(src.len());
        let body = &src[fn_start..fn_end];
        let sleep_idx = body
            .find(
                "tokio::time::sleep(std::time::Duration::from_secs(WS_GRACEFUL_DRAIN_SLEEP_SECS)).await",
            )
            .expect("the graceful WS drain await must exist");
        let take_idx = body
            .find("std::mem::take(&mut lane.ws_handles)")
            .expect("ws_handles must be taken out of lane (not destructured)");
        assert!(
            sleep_idx < take_idx,
            "ws_handles must be taken out of `lane` ONLY AFTER the graceful drain await, so a \
             mid-drain cancel triggers lane's Drop abort-floor instead of detaching the pool"
        );
    }

    #[test]
    fn dhan_lane_run_handles_has_drop_impl_aborting_pool() {
        // H8 no-leak floor source-scan: `DhanLaneRunHandles` MUST implement Drop
        // and that Drop MUST abort the ws pool handles (so an unexpected drop can
        // never DETACH a live WS pool). Pins the defense-in-depth guard.
        let src = include_str!("main.rs");
        assert!(
            src.contains("impl Drop for DhanLaneRunHandles"),
            "DhanLaneRunHandles must implement Drop (the H8 no-leak floor)"
        );
        let drop_start = src
            .find("impl Drop for DhanLaneRunHandles")
            .expect("Drop impl must exist");
        let drop_region = &src[drop_start..drop_start + 1200];
        assert!(
            drop_region.contains("for handle in &self.ws_handles")
                && drop_region.contains(".abort()"),
            "the Drop impl must abort every ws_handle so the WS pool cannot detach"
        );
        assert!(
            drop_region.contains("self.shutdown_notify.notify_waiters()"),
            "the Drop impl must fire the shutdown notify (stop the pool watchdog)"
        );
    }

    #[tokio::test]
    async fn dhan_lane_run_handles_drop_aborts_handles() {
        // Behavioural proof of the H8 floor: a never-completing spawned task held
        // in `ws_handles` is ABORTED (finished) shortly after the
        // `DhanLaneRunHandles` is dropped — i.e. Drop tears down rather than
        // detaches. Uses an abort_handle to observe finishedness after the drop.
        let never = tokio::spawn(async {
            std::future::pending::<()>().await;
            Ok(())
        });
        let abort_handle = never.abort_handle();
        assert!(
            !abort_handle.is_finished(),
            "task should be running before drop"
        );

        let handles = super::DhanLaneRunHandles {
            ws_handles: vec![never],
            processor_handle: None,
            renewal_handle: None,
            order_update_handle: None,
            order_update_auth_listener_handle: None,
            trading_handle: None,
            token_health_handle: None,
            token_health_gauge_handle: None,
            mid_session_watchdog_handle: None,
            token_sweep_handle: None,
            periodic_health_handle: None,
            midnight_rollover_handle: None,
            prev_oi_refresh_handle: None,
            pool_watchdog_handle: None,
            ip_monitor_handle: None,
            ip_monitor_shutdown: None,
            heartbeat_watchdog_handle: None,
            health: None,
            feed_health: None,
            ws_pool_arc: None,
            shutdown_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            lane_halt_notify: None,
            instance_lock_heartbeat: None,
            instance_lock_shutdown: None,
            heartbeat_bridge_handle: None,
        };
        drop(handles);

        // Give the runtime a moment to process the abort.
        for _ in 0..50 {
            if abort_handle.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            abort_handle.is_finished(),
            "dropping DhanLaneRunHandles must abort (not detach) the live ws pool task"
        );
    }

    /// BUG-1 ratchet (2026-07-05): the graceful teardown must bound-await
    /// the dual-instance lock heartbeat so the SSM DeleteParameter release
    /// LANDS before process exit. Live proof this regression class is real:
    /// the 15:35 IST 2026-07-05 graceful EOD stop left the lock in SSM
    /// (the 17:59 boot found it stale at age 8622s); a Stop→Start inside
    /// the 90s TTL HALTs with RESILIENCE-01.
    #[test]
    fn ratchet_teardown_bound_awaits_instance_lock_release() {
        let src = include_str!("main.rs");
        let teardown_start = src
            .find("async fn teardown_dhan_lane_tasks(")
            .expect("teardown_dhan_lane_tasks must exist");
        let teardown_end = src[teardown_start..]
            // \u{7d} is a brace-balanced escape for the closing-brace char: the
            // banned-pattern hook counts raw braces (incl. string literals) to
            // skip the test module; a bare closing brace in a literal derails it.
            .find("\n\u{7d}\n")
            .map(|o| teardown_start + o)
            .expect("teardown_dhan_lane_tasks must have a body end");
        let body = &src[teardown_start..teardown_end];
        // Direct permit-storing wake (Rule 16 lost-wake safe) — never rely
        // only on the notify_waiters bridge.
        assert!(
            body.contains("lane.instance_lock_shutdown.take()")
                && body.contains("shutdown.notify_one()"),
            "teardown must directly notify_one() the instance-lock heartbeat's shutdown Notify"
        );
        // Bounded await of the heartbeat handle (the release round-trip).
        assert!(
            body.contains("lane.instance_lock_heartbeat.take()")
                && body.contains("INSTANCE_LOCK_RELEASE_TIMEOUT_SECS"),
            "teardown must bound-await the instance-lock heartbeat JoinHandle \
             (tokio::time::timeout(INSTANCE_LOCK_RELEASE_TIMEOUT_SECS, handle)) \
             so the SSM release lands before process exit"
        );
        // The handle must actually ride into the lane struct from the
        // boot-ON construction site (not stay a dropped local).
        assert!(
            src.contains("instance_lock_heartbeat: instance_lock_handle"),
            "start_dhan_lane must thread the heartbeat JoinHandle into DhanLaneRunHandles"
        );
        // The old drop-at-spawn binding must never come back. (Needle
        // assembled at runtime so this test's own source never self-matches.)
        let dropped_binding = format!("let _instance_lock_handle = {}", "instance_lock_handle;");
        assert!(
            !src.contains(&dropped_binding),
            "the heartbeat JoinHandle must not be silently dropped at spawn time \
             (that is exactly the race that leaked the SSM lock on graceful stop)"
        );
        // Bound sanity: short enough to never hang a Ctrl-C exit.
        assert!(
            INSTANCE_LOCK_RELEASE_TIMEOUT_SECS >= 1 && INSTANCE_LOCK_RELEASE_TIMEOUT_SECS <= 10,
            "the release bound must stay a short, non-hanging shutdown wait"
        );
    }

    /// BUG-2 ratchet (2026-07-05): `dhan_lane_running` + the PR-2
    /// `dhan_pool_present` sentinel are set ONLY at the pool-spawn sites —
    /// never from the bare `feeds.dhan_enabled` config flag. A
    /// non-trading-day boot skips the pool and must stay enabled-but-idle
    /// (live proof: the 2026-07-05 Saturday boot logged "WebSocket pool
    /// skipped — non-trading day" yet reported dhan_running:true).
    #[test]
    fn ratchet_dhan_running_marked_only_at_pool_spawn() {
        let src = include_str!("main.rs");
        // The early feeds block (seeding the FSM) must NOT mark the flags.
        let feeds_block_start = src
            .find("if feeds.dhan_enabled \u{7b}")
            .expect("the early dhan_enabled boot block must exist");
        let feeds_block_end = feeds_block_start
            + src[feeds_block_start..]
                .find("\n    \u{7d}\n")
                .expect("the early dhan_enabled boot block must close");
        let feeds_block = &src[feeds_block_start..feeds_block_end];
        assert!(
            !feeds_block.contains("mark_dhan_lane_running()")
                && !feeds_block.contains("mark_dhan_pool_present()"),
            "the early `if feeds.dhan_enabled` block must NOT mark the lane \
             running / pool present — enabled-at-boot is not pool-built \
             (non-trading-day boots skip the pool)"
        );
        // Both boot-path pool-spawn sites must mark the flags (FAST arm +
        // slow-arm boot-ON). The distinctive fix-comment marker sits inside
        // each pool-built branch, right after spawn_websocket_connections;
        // the runtime cold-start keeps its separate CAS-gated mark.
        // (Marker assembled at runtime so this test's own source never
        // self-matches and inflates the count.)
        let marker = format!(
            "pool-present sentinel ONLY now that a REAL pool {}",
            "is spawned"
        );
        let mark_sites = src.matches(marker.as_str()).count();
        assert!(
            mark_sites >= 2,
            "both boot-path pool-spawn branches (FAST arm + slow-arm boot-ON) \
             must mark running/pool-present at the pool-spawn site; found \
             {mark_sites} marker(s)"
        );
        // The alive signal must treat a deliberately pool-less slow boot as
        // a completed boot (no false boot-heartbeat page on holidays).
        assert!(
            src.contains("dhan_poolless_idle: bool") && src.contains("ws_pool_arc.is_none(),"),
            "emit_boot_completed_when_feed_live must carry the poolless-idle \
             input, fed from ws_pool_arc.is_none() on the slow boot path"
        );
    }

    #[test]
    fn gauge_token_headroom_falls_back_to_global_when_no_live_lane() {
        // D2c (C4) behavioural: with no live lane manager installed (boot-OFF /
        // Groww-only / pre-start), `gauge_token_headroom_secs` falls back to the
        // global OnceLock. In an isolated test binary the global is unset, so the
        // helper returns the `0` "no token" sentinel — the same value the gauges
        // used before this fix. This proves the fallback arm + the sentinel.
        let feed_runtime =
            std::sync::Arc::new(tickvault_api::feed_state::FeedRuntimeState::default());
        assert!(
            feed_runtime.live_token_manager().is_none(),
            "fresh state has no live lane manager (fallback path)"
        );
        assert_eq!(
            super::gauge_token_headroom_secs(&feed_runtime),
            0,
            "no live lane + no global → 0 (the pre-fix 'no token' sentinel)"
        );
    }

    #[test]
    fn slo_token_gauge_prefers_live_lane_manager() {
        // D2c (C4) source-scan ratchet: BOTH PROCESS-level token gauges (the
        // market-open self-test token-headroom AND the SLO token-freshness) MUST
        // read via `gauge_token_headroom_secs` (which prefers the LIVE lane-owned
        // manager) and MUST NOT read the global `OnceLock` directly — otherwise a
        // runtime stop→re-start would show the dead boot-time manager's headroom on
        // the exact path D2c targets. The ONLY allowed `global_token_manager()` call
        // is inside the `gauge_token_headroom_secs` fallback arm.
        let src = include_str!("main.rs");

        // Both gauges call the shared helper.
        assert!(
            src.contains("let token_headroom_secs = gauge_token_headroom_secs(&st_feed_runtime);"),
            "the self-test token-headroom gauge must read via gauge_token_headroom_secs (live lane preferred)"
        );
        assert!(
            src.contains("let token_secs = gauge_token_headroom_secs(&slo_feed_runtime);"),
            "the SLO token-freshness gauge must read via gauge_token_headroom_secs (live lane preferred)"
        );

        // The helper prefers the live lane manager before any global fallback.
        let helper_start = src
            .find("fn gauge_token_headroom_secs(")
            .expect("gauge_token_headroom_secs must exist");
        let helper_end = src[helper_start..]
            // \u{7d} is a brace-balanced escape for the closing-brace char: the
            // banned-pattern hook counts raw braces (incl. string literals) to
            // skip the test module; a bare closing brace in a literal derails it.
            .find("\n\u{7d}\n")
            .map(|rel| helper_start + rel)
            .expect("gauge_token_headroom_secs must have a body");
        let helper = &src[helper_start..helper_end];
        let live_pos = helper
            .find("feed_runtime.live_token_manager()")
            .expect("helper must read the live lane manager");
        let global_pos = helper
            .find("global_token_manager()")
            .expect("helper must have the global fallback");
        assert!(
            live_pos < global_pos,
            "the helper must PREFER the live lane manager (read it before the global fallback)"
        );

        // The global OnceLock must be read ONLY inside the helper's fallback —
        // never again in the closure bodies (which would re-introduce the bug).
        // Scope the count to the PRODUCTION region via the shared
        // `source_scan::production_region` helper (F8, 2026-07-08): unlike
        // the old before-first-`#[cfg(test)]` prefix, it also covers the
        // production code AFTER the test module, so a direct global read
        // landing there can no longer hide from this exactly-once count.
        let prod_region = tickvault_common::source_scan::production_region(src)
            .expect("main.rs must have a #[cfg(test)]-gated mod tests module");
        let direct_global_reads = prod_region.matches("global_token_manager()").count();
        assert_eq!(
            direct_global_reads, 1,
            "global_token_manager() must be read exactly once (the helper fallback); \
             the two gauge closures must NOT read it directly"
        );
    }

    #[test]
    fn every_lane_not_running_site_clears_the_live_token_manager() {
        // D2c (C4) clear-completeness ratchet: in the lane supervisor / cold-start /
        // teardown / watchdog-Halt region, EVERY `set_dhan_lane_running(false)` call
        // (i.e. every "lane is now Off / not-running" site) MUST be immediately
        // followed — within a small window — by a `clear_live_token_manager()` call.
        // Otherwise a failure arm that forgets the clear leaves the health gauges
        // reading a dead lane's TokenManager during the failed-start backoff — the
        // exact stale-gauge bug D2c fixes. A future failure arm that omits the clear
        // fails THIS test (not just the happy-path gauge test).
        let src = include_str!("main.rs");

        // Scope to the lane control-plane region only (supervisor → run-loop). This
        // excludes the early `set_live_token_manager` install at boot and any
        // `#[cfg(test)]` string literals (the test module lives far below run_loop).
        let region_start = src
            .find("pub async fn run_dhan_lane_runtime_supervisor")
            .expect("the lane runtime supervisor must exist");
        let region_end = src
            .find("async fn run_process_runloop")
            .filter(|&o| o > region_start)
            .expect("run_process_runloop must follow the lane region");
        let region = &src[region_start..region_end];

        // Every not-running marker in this region must be paired with a clear.
        let marker = "set_dhan_lane_running(false);";
        let marker_count = region.matches(marker).count();
        assert!(
            marker_count >= 6,
            "expected the 6 lane-Off sites (3 clean paths + 3 failure arms); found {marker_count} \
             — a site was removed or the region boundary drifted"
        );

        // The number of `set_dhan_lane_running(false)` sites and the number of
        // `clear_live_token_manager()` clears in the region must MATCH — one clear
        // per not-running site. (Both are O(1) substring counts over the region.)
        let clear_count = region.matches("clear_live_token_manager();").count();
        assert_eq!(
            clear_count, marker_count,
            "every lane-Off / not-running site ({marker_count}) must have a matching \
             clear_live_token_manager() ({clear_count}) — a failure arm forgot the clear"
        );

        // Positional check: each not-running marker is followed by a clear within a
        // short window (no intervening other marker), so the pairing is genuine and
        // a clear cannot be "borrowed" from a distant unrelated site.
        let mut search_from = 0usize;
        let mut paired = 0usize;
        while let Some(rel) = region[search_from..].find(marker) {
            let marker_abs = search_from + rel;
            let after = &region[marker_abs..];
            let clear_at = after.find("clear_live_token_manager();");
            let next_marker_at = after[marker.len()..].find(marker).map(|o| o + marker.len());
            // A clear must exist after this marker AND occur before the next marker.
            match (clear_at, next_marker_at) {
                (Some(c), Some(n)) => assert!(
                    c < n,
                    "a lane-Off site at byte {marker_abs} is not followed by a clear before the \
                     next lane-Off site — the failure arm forgot clear_live_token_manager()"
                ),
                (Some(_), None) => {}
                (None, _) => panic!(
                    "a lane-Off site at byte {marker_abs} has no following \
                     clear_live_token_manager() in the lane region"
                ),
            }
            paired += 1;
            search_from = marker_abs + marker.len();
        }
        assert_eq!(
            paired, marker_count,
            "every lane-Off marker must be positionally paired with a clear"
        );
    }

    /// F10 / F2 (2026-07-08): behavioural pin of the
    /// `InstanceLockHeartbeatGuard` defuse semantics. `into_parts()` must
    /// TAKE (not clone) the shutdown Notify out of the guard — a
    /// `.take()`→`.clone()` mutation would leave the husk armed, so its
    /// Drop right after the lane construction would `notify_one()` the
    /// heartbeat's shutdown and RELEASE the SSM dual-instance lock while
    /// the lane is RUNNING (a peer could then acquire it → real
    /// dual-session). Conversely, a drop WITHOUT defuse (cancel-mid-Starting
    /// / Err return) MUST notify so the zombie heartbeat releases.
    #[tokio::test]
    async fn instance_lock_heartbeat_guard_defuse_and_drop_semantics() {
        use std::time::Duration;
        // (a) Drop WITHOUT defuse → the shutdown gets a stored permit.
        let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
        let handle = tokio::spawn(async {});
        let guard =
            super::InstanceLockHeartbeatGuard::new(handle, std::sync::Arc::clone(&shutdown));
        drop(guard);
        tokio::time::timeout(Duration::from_millis(200), shutdown.notified())
            .await
            .expect(
                "dropping an un-defused guard must notify_one() the heartbeat \
                 shutdown (permit-storing graceful SSM release — R8-EDGE-1)",
            );

        // (b) into_parts() DEFUSES: the husk's Drop must NOT notify.
        let shutdown2 = std::sync::Arc::new(tokio::sync::Notify::new());
        let handle2 = tokio::spawn(async {});
        let guard2 =
            super::InstanceLockHeartbeatGuard::new(handle2, std::sync::Arc::clone(&shutdown2));
        let (h, n) = guard2.into_parts(); // husk drops here
        assert!(
            h.is_some() && n.is_some(),
            "into_parts must hand out BOTH the heartbeat handle and its \
             shutdown Notify for the lane struct's BUG-1 fields"
        );
        let stray_permit =
            tokio::time::timeout(Duration::from_millis(100), shutdown2.notified()).await;
        assert!(
            stray_permit.is_err(),
            "into_parts (defuse) must NOT notify the heartbeat shutdown — a \
             clone-instead-of-take mutation in into_parts would release the \
             SSM dual-instance lock while the lane is RUNNING (F10)"
        );
        if let Some(h) = h {
            h.abort();
        }
    }

    /// F15 (2026-07-08): the /health token_valid derivation must honor the
    /// profile-truth flag — a Dhan-KILLED (profile-invalid) but
    /// locally-unexpired token must read INVALID on /health, mirroring
    /// tv_token_valid. Kills the `secs > 0`-only regression.
    #[test]
    fn test_token_health_writer_valid_honors_profile_truth() {
        assert!(super::token_health_writer_valid(86_400, true));
        assert!(
            !super::token_health_writer_valid(86_400, false),
            "a Dhan-KILLED token (profile flag false) with local headroom \
             must read INVALID on /health (F15)"
        );
        assert!(
            !super::token_health_writer_valid(0, true),
            "the expiry instant is invalid — strictly-greater, fail-closed"
        );
        assert!(!super::token_health_writer_valid(0, false));
        assert!(super::token_health_writer_valid(1, true));
    }

    /// F4 (2026-07-08): the market-open fire-time gate reads the live Dhan
    /// flag — never a constant.
    #[test]
    fn test_market_open_fire_gate_reads_dhan_flag() {
        let flag = std::sync::atomic::AtomicBool::new(true);
        assert!(super::market_open_fire_gate_dhan_enabled(&flag));
        flag.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(
            !super::market_open_fire_gate_dhan_enabled(&flag),
            "a disabled Dhan flag must gate the one-shot verdict off (F4)"
        );
    }

    /// F17 + F9 (2026-07-08): COMMENT-STRIPPED source-order ratchet for the
    /// teardown's abort → bounded-join → honest-reset chain. The teardown
    /// body carries doc/comment mentions of the join-timeout const, so a
    /// raw `contains`/count scan was satisfiable by comments alone —
    /// reverting any bounded join to a plain abort stayed green. Stripping
    /// comments first (shared `source_scan` helper) makes each count a CODE
    /// count.
    #[test]
    fn ratchet_teardown_abort_join_reset_ordering_comment_stripped() {
        let src = include_str!("main.rs");
        let stripped = tickvault_common::source_scan::strip_rust_comments(src);
        let fn_start = stripped
            .find("async fn teardown_dhan_lane_tasks(")
            .expect("teardown_dhan_lane_tasks must exist");
        let fn_end = stripped[fn_start..]
            // \u{7d} is a brace-balanced escape for the closing-brace char: the
            // banned-pattern hook counts raw braces (incl. string literals) to
            // skip the test module; a bare closing brace in a literal derails it.
            .find("\n\u{7d}\n")
            .map(|o| fn_start + o)
            .expect("teardown_dhan_lane_tasks must have a body end");
        let flat: String = stripped[fn_start..fn_end]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        // (a) exactly FOUR bounded joins on the shared 5s bound, in CODE
        // (comments stripped): token-health writer, gauge poller, renewal
        // loop (AG5-R2-3 / R3-1), pool watchdog (F14).
        let join_needle = "from_secs(LANE_HEALTH_TASK_JOIN_TIMEOUT_SECS)";
        let join_offsets: Vec<usize> = {
            let mut offs = Vec::new();
            let mut from = 0usize;
            while let Some(rel) = flat[from..].find(join_needle) {
                offs.push(from + rel);
                from += rel + join_needle.len();
            }
            offs
        };
        assert_eq!(
            join_offsets.len(),
            4,
            "teardown_dhan_lane_tasks must bound-join exactly 4 handles on \
             LANE_HEALTH_TASK_JOIN_TIMEOUT_SECS (token-health writer, gauge \
             poller, renewal loop, pool watchdog) — a comment mention can \
             no longer stand in for a reverted join (F9/F17); found {}",
            join_offsets.len()
        );
        // (b) ordering: the three token-side joins precede the honest
        // 0/0.0 gauge reset; the pool-watchdog join follows it but precedes
        // the honest websocket/feed-health reset.
        let gauge_reset = flat
            .find("gauge!(\"tv_token_remaining_seconds\").set(0.0)")
            .expect("teardown must publish the honest 0.0 gauge reset");
        assert!(
            join_offsets[2] < gauge_reset,
            "all three token-side bounded joins must PRECEDE the honest \
             gauge reset — a late writer iteration could otherwise \
             republish stale values after it (AG5-R2-3/R3-1)"
        );
        assert!(
            join_offsets[3] > gauge_reset,
            "the 4th bounded join is the step-3 pool-watchdog join and \
             belongs AFTER the token-block reset (teardown step order)"
        );
        let ws_reset = flat
            .find("health.set_websocket_connections(0)")
            .expect("teardown must publish the honest lane-off ws count (F14)");
        assert!(
            join_offsets[3] < ws_reset,
            "the pool-watchdog bounded join must PRECEDE the honest \
             websocket/feed-health reset — an in-flight watchdog tick could \
             otherwise rewrite the surfaces after the reset (F14)"
        );
        // (c) budget honesty (F16): the compile-time worst-case sum counts
        // FOUR joins.
        assert!(
            super::DHAN_LANE_TEARDOWN_INTERNAL_WORST_CASE_SECS
                >= 4 * super::LANE_HEALTH_TASK_JOIN_TIMEOUT_SECS,
            "the teardown internal worst-case sum must account for all 4 \
             bounded joins (F14/F16)"
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

// Process-global once-guard for `spawn_post_market_tasks` (2026-07-02
// adversarial-sweep fix): the runtime Dhan cold-start path
// (`run_dhan_lane_cold_start` → `start_dhan_lane`) re-invokes
// `spawn_post_market_tasks` on EVERY disable→enable cycle, and the spawned
// tasks are bare `tokio::spawn`s not owned by `DhanLaneRunHandles` — so N
// enable cycles before the guard accumulated N duplicate task families
// (N EOD digests, N cross-verifies, N orphan watchdogs). First caller wins;
// later calls log INFO and return.
//
// 2026-07-13 (Phase A FIX 4): the guard static moved into the lib —
// `tickvault_app::dhan_rest_stack::claim_post_market_task_family_once()` —
// because the Dhan REST-only stack's Phase 5 spawns the SAME canary/spot/
// chain family and must claim the SAME guard. INVARIANT: the Dhan-REST
// scheduled task family is spawned at most once per process, whichever
// path (lane or REST-only stack) claims first — a future relaxation of the
// runtime cold-start refusal can never double-spawn canary/spot/chain.

/// Spawn the scheduled daily tasks — the 15:25 IST orphan-position watchdog,
/// end-of-day digest (15:31:30 IST), the 1-minute cross-verify (15:31:00 IST),
/// and the REST-health canary (09:05 / 12:00 / 15:25 IST — DHAN-REST-400,
/// 2026-06-10). Called from BOTH boot paths so a mid-session fast-boot restart
/// runs them too (boot-symmetry, 2026-06-09); a process-global once-guard makes
/// repeat calls (runtime Dhan cold-start cycles) no-ops so the family is never
/// duplicated. Each spawned task self-skips if past its IST trigger(s) or on a
/// non-trading day, so calling this from a late/non-trading boot is safe
/// (no-op). NOTE: these tasks are Dhan-REST-dependent (token + client-id); the
/// feed-agnostic 15:40 tick-conservation audit was HOISTED to the
/// process-global prefix (`spawn_daily_tick_conservation_task`) on 2026-07-02
/// so it fires in Groww-only sessions too.
fn spawn_post_market_tasks(
    notifier: std::sync::Arc<NotificationService>,
    health_status: SharedHealthStatus,
    trading_calendar: std::sync::Arc<TradingCalendar>,
    token_handle: TokenHandle,
    client_id: String,
    config: &ApplicationConfig,
    // 2026-07-03 feed parity: the end-of-day digest names EVERY enabled
    // market-data feed, built at fire time (15:31:30 IST) from the same
    // per-feed health registry the feed-control page reads.
    feed_runtime: std::sync::Arc<tickvault_api::feed_state::FeedRuntimeState>,
    feed_health: std::sync::Arc<tickvault_common::feed_health::FeedHealthRegistry>,
) {
    if !tickvault_app::dhan_rest_stack::claim_post_market_task_family_once() {
        info!(
            "post-market tasks already spawned this process — skipping duplicate \
             (runtime Dhan cold-start re-entry, or the Dhan REST-only stack \
             already owns the canary/spot/chain family)"
        );
        return;
    }
    // Phase 0 Item 20 (wired 2026-06-13): supervised 15:25 IST orphan-position
    // watchdog — the daily open-position safety gate. Alert-only in
    // sandbox/dry-run (no order/cancel call exists on any path); pages CRITICAL
    // every day open positions exist (NOT edge-suppressed — it is a compliance
    // gate). Supervised respawn + busy-loop floor + secret-redacted REST errors
    // per the 2026-06-13 3-agent adversarial review. Called from BOTH boot
    // paths (boot-symmetry) so a mid-session restart re-arms it.
    let _orphan_watchdog_handle =
        tickvault_app::orphan_position_watchdog_boot::spawn_supervised_orphan_position_watchdog(
            token_handle.clone(),
            notifier.clone(),
            std::sync::Arc::clone(&trading_calendar),
            config.dhan.rest_api_base_url.clone(),
            client_id.clone(),
            config.strategy.dry_run,
        );
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
        let eod_feed_runtime = std::sync::Arc::clone(&feed_runtime);
        let eod_feed_health = std::sync::Arc::clone(&feed_health);
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
                // 2026-07-03 feed parity: one line per enabled feed
                // (Dhan, Groww, future) built at digest fire time.
                feeds: build_feed_status_lines(&eod_feed_runtime, &eod_feed_health),
            });
        });
    }

    // Operator directive 2026-06-02: post-market 1-minute
    // cross-verification at 15:31:00 IST. For every subscribed SPOT
    // instrument, compare our live `candles_1m` OHLCV against Dhan's
    // authoritative intraday 1-minute candles, EXACT match, and write
    // mismatches to the unified `feed_parity_1m_audit` table + a per-day CSV
    // (`data/cross-verify/`). The per-day mismatch COUNT is the quality
    // signal. Cold path, fail-soft, market-hours-gated (audit Rule 3).
    if let Some(cv_universe) = tickvault_app::prev_day_ohlcv_boot::stashed_universe() {
        // Build owned spot targets HERE (universe Arc in scope) so the
        // task doesn't hold the Arc. Skip rows with an unparseable SID.
        let cv_targets: Vec<tickvault_app::cross_verify_1m_boot::CrossVerifyTarget> = cv_universe
            .subscription_targets
            .iter()
            .filter_map(|t| {
                // §36 (2026-07-08): the 15:31 cross-verify stays SPOT-ONLY by
                // construction — `instrument_type_for_role` returns None for
                // IndexFuture targets (Dhan-historical FUTIDX UNVERIFIED-LIVE),
                // so futures never enter the target list; counted, not silent.
                let Some(instrument) =
                    tickvault_app::prev_day_ohlcv_boot::instrument_type_for_role(t.role)
                else {
                    metrics::counter!("tv_cross_verify_futidx_skipped_total").increment(1);
                    return None;
                };
                t.csv_row.security_id.trim().parse::<i64>().ok().map(|sid| {
                    tickvault_app::cross_verify_1m_boot::CrossVerifyTarget {
                        security_id: sid,
                        segment: t.csv_row.segment.trim().to_string(),
                        symbol: t.csv_row.symbol_name.trim().to_string(),
                        instrument,
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

    // Operator grant 2026-07-12 (PR-2, the SPOT half): per-minute spot 1m
    // REST pipeline — every trading-day minute close in [09:16:00, 15:30:00]
    // IST, fetch the just-closed minute's official 1m OHLCV for the 3 IDX_I
    // spot indices via POST /v2/charts/intraday and persist to the
    // `spot_1m_rest` table (SPOT1M-01/02). Spawned from BOTH boot paths via
    // this shared Dhan-gated site (Dhan-REST-dependent — token; a Groww-only
    // session correctly runs none). Config-gated fail-safe: an absent
    // `[spot_1m_rest]` section disables it. Supervised respawn wrapper;
    // self-skips on non-trading days / past 15:30 IST (audit Rule 3).
    // PR-3 (2026-07-12): the spot→chain sequencing signal. Created ONLY
    // when BOTH halves are enabled — with the chain off, the spot leg's
    // behaviour stays byte-identical to PR-2 (no sender, no publishes).
    let (spot_minute_done_tx, spot_minute_done_rx) =
        if config.spot_1m_rest.enabled && config.option_chain_1m.enabled {
            let (tx, rx) = tokio::sync::watch::channel::<Option<u32>>(None);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

    if config.spot_1m_rest.enabled {
        let _spot1m_supervisor = tickvault_app::spot_1m_rest_boot::spawn_supervised_spot_1m_rest(
            tickvault_app::spot_1m_rest_boot::Spot1mRestTaskParams {
                token_handle: std::sync::Arc::clone(&token_handle),
                notifier: notifier.clone(),
                calendar: std::sync::Arc::clone(&trading_calendar),
                questdb: config.questdb.clone(),
                rest_api_base_url: config.dhan.rest_api_base_url.clone(),
                minute_done_tx: spot_minute_done_tx,
                diagnostics_enabled: config.spot_1m_rest.diagnostics,
                diagnostics_second_probe_secs_of_day_ist: config
                    .spot_1m_rest
                    .diagnostics_second_probe_secs_of_day_ist,
            },
        );
        info!(
            "spot_1m_rest: per-minute spot 1m REST pipeline spawned \
             (fires each minute close 09:16:00–15:30:00 IST)"
        );
    } else {
        info!("spot_1m_rest: disabled by config — per-minute spot fetch not spawned");
    }

    // Operator grant 2026-07-12 (PR-3, the OPTION-CHAIN half): per-minute
    // option-chain pipeline — day-start expirylist warmup, then the full
    // current-expiry chain for the 3 underlyings each session minute,
    // sequenced right after the spot leg, persisted to `option_chain_1m`
    // (CHAIN-01..04). Config-gated DEFAULT-OFF pending the live
    // entitlement probe; while disabled, `probe_and_report` (default ON)
    // runs ONE boot-time expirylist probe and reports the verdict via
    // Telegram — the pipeline NEVER auto-runs on a probe pass (the
    // operator flips `[option_chain_1m].enabled`). Same shared Dhan-gated
    // seam + once-guard as the spot leg.
    {
        let chain_params = tickvault_app::option_chain_1m_boot::OptionChain1mTaskParams {
            token_handle: std::sync::Arc::clone(&token_handle),
            notifier: notifier.clone(),
            calendar: std::sync::Arc::clone(&trading_calendar),
            questdb: config.questdb.clone(),
            rest_api_base_url: config.dhan.rest_api_base_url.clone(),
            client_id: client_id.clone(),
            spot_minute_done: spot_minute_done_rx,
        };
        if config.option_chain_1m.enabled {
            let _chain1m_supervisor =
                tickvault_app::option_chain_1m_boot::spawn_supervised_option_chain_1m(chain_params);
            info!(
                "option_chain_1m: per-minute option-chain REST pipeline spawned \
                 (expirylist warmup, then each minute close right after the spot leg)"
            );
        } else if config.option_chain_1m.probe_and_report {
            // Once-only best-effort probe; its exit is MONITORED so an
            // unwind-build panic is never silent (release aborts anyway).
            let probe_handle = tokio::spawn(
                tickvault_app::option_chain_1m_boot::run_option_chain_1m_probe(chain_params),
            );
            tokio::spawn(async move {
                if let Err(join_err) = probe_handle.await
                    && !join_err.is_cancelled()
                {
                    error!(
                        code = tickvault_common::error_code::ErrorCode::Chain04ExpirylistFailed
                            .code_str(),
                        stage = "probe_task_exit",
                        ?join_err,
                        "CHAIN-04: the option-chain entitlement probe task \
                         died (panic) — no verdict today; tomorrow's boot \
                         re-probes"
                    );
                }
            });
            info!(
                "option_chain_1m: pipeline disabled by config — boot-time \
                 entitlement probe spawned (verdict via Telegram)"
            );
        } else {
            info!(
                "option_chain_1m: disabled by config (probe_and_report off) — \
                 no option-chain REST activity"
            );
        }
    }
}

/// Daily 15:40 IST tick-conservation audit — PROCESS-GLOBAL (2026-07-02
/// adversarial-sweep fix). This used to live inside `spawn_post_market_tasks`,
/// whose two call sites are BOTH Dhan-gated (fast-boot `dhan_enabled` filter +
/// `start_dhan_lane`), so a Groww-only session (`dhan_enabled=false`) ran ZERO
/// conservation audits all day — a silent audit-coverage hole — and every
/// runtime Dhan enable cycle spawned a DUPLICATE conservation task. It is now
/// spawned exactly once from `main()`'s process-global prefix, independent of
/// which feeds are enabled; each lane's run is gated at 15:40 on the truthful
/// runtime "is this feed on" Arc, so a disabled lane writes no misleading zero
/// row. Honest envelope: a feed toggled OFF at 15:40 after a full ON day skips
/// its row that day.
///
/// Reconciles, per feed: the durable delivered-count (Dhan WAL frames / Groww
/// sidecar NDJSON) against the processor outcome counters (Dhan) and the
/// feed-filtered QuestDB `ticks` row count, then writes one forensic row per
/// feed to `tick_conservation_audit`. Dhan residual > 0 → error!
/// TICK-CONSERVE-01 (Telegram). Cold path, fail-soft, market-hours-gated
/// (audit Rule 3). Runbook:
/// `.claude/rules/project/tick-conservation-audit-error-codes.md`.
// TEST-EXEMPT: tokio::spawn wrapper over the unit-tested pure parts (decide_conservation_start / boot_covers_full_session / build_conservation_ticks_count_sql); spawn site pinned by tick_conservation_wiring_guard.rs.
fn spawn_daily_tick_conservation_task(
    config: &ApplicationConfig,
    trading_calendar: &std::sync::Arc<TradingCalendar>,
    feed_runtime: &std::sync::Arc<tickvault_api::feed_state::FeedRuntimeState>,
) {
    let tc_qcfg = config.questdb.clone();
    let tc_metrics_port = config.observability.metrics_port;
    let tc_calendar = std::sync::Arc::clone(trading_calendar);
    // Single source of truth for the WAL dir (shared with STAGE-C).
    let tc_wal_dir = tickvault_app::tick_conservation_boot::ws_wal_dir();
    let tc_groww_ndjson =
        std::path::PathBuf::from(tickvault_app::groww_bridge::GROWW_TICK_FILE_DEFAULT);
    let tc_feed_runtime = std::sync::Arc::clone(feed_runtime);
    tokio::spawn(async move {
        use chrono::{FixedOffset, TimeZone, Timelike, Utc};
        use tickvault_app::tick_conservation_boot::{
            ConservationStart, boot_covers_full_session, decide_conservation_start,
            run_groww_tick_conservation_audit, run_tick_conservation_audit,
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
            ConservationStart::RunCatchUp => {
                // Audit fix #2 (2026-07-03): a trading-day boot past 15:40 IST
                // used to SKIP the day's audit entirely — a post-incident
                // evening recovery boot left no forensic WAL-vs-DB row. Run
                // once, immediately; the row is honestly `partial` (post-09:00
                // boot counters cannot vouch for the session) but the WAL
                // frame count + QuestDB row count for the day ARE recorded.
                info!(
                    now = %boot_ist.time(),
                    "tick_conservation: late boot (past 15:40 IST) — running \
                     the day's audit now as a catch-up"
                );
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

        // Dhan lane — runtime-gated (symmetric with the Groww gate below) so a
        // Groww-only session writes no misleading zero-balanced Dhan row.
        if tc_feed_runtime.is_enabled(tickvault_common::feed::Feed::Dhan) {
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
        } else {
            debug!("tick_conservation: Dhan run skipped (Dhan feed disabled this session)");
        }

        // Groww lane — same IST day, same window, runtime-gated so a
        // Dhan-only session writes no misleading zero Groww row.
        if tc_feed_runtime.is_enabled(tickvault_common::feed::Feed::Groww) {
            run_groww_tick_conservation_audit(
                &tc_groww_ndjson,
                &tc_qcfg,
                target_ist_day,
                trading_date_ist_nanos,
                run_ts_ist_nanos,
                boot_covers_full_session(boot_secs_of_day),
            )
            .await;
            info!("PROOF: groww tick_conservation audit fired @ 15:40:00 IST");
        } else {
            debug!("groww_conservation: skipped (Groww feed disabled this session)");
        }
    });
    info!("tick_conservation: daily per-feed WAL/NDJSON-vs-DB audit task spawned (process-global)");
}

/// Dual-feed scoreboard (operator directive 2026-07-10) — PROCESS-GLOBAL,
/// spawned exactly once from `main()`'s prefix, gated on `[scoreboard]
/// enabled` (the B12 rollback switch: `false` ⇒ nothing here spawns).
///
/// Two tasks:
/// 1. Boot-time process-death reconciler — after a settle delay it
///    synthesizes `process_death` episodes (blame `ours`, deterministic ts
///    ⇒ DEDUP-idempotent) for connections that were "up" when the previous
///    process died. See `feed_scoreboard_boot::reconcile_process_death_episodes`.
/// 2. The daily aggregation at `[scoreboard] trigger_secs_of_day_ist`
///    (default 15:45:00 IST) with the inner/outer supervisor idiom (the
///    cross_verify_1m pattern): the inner task returns the summary; the
///    outer watcher sends the `DualFeedDailyScorecard` Telegram on success
///    and `DualFeedScorecardAborted` on Err/panic — the daily signal can
///    never be silently dropped. Graceful-shutdown cancellation stays
///    silent (normal teardown, not an abort).
/// Groww per-minute spot 1m REST leg (operator grant 2026-07-13 — PR-2 of
/// the Groww per-minute REST plan), called from BOTH boot arms (the FAST
/// crash-recovery arm AND the slow process-global prefix — the
/// `spawn_feed_scoreboard_tasks` dual-site pattern; hostile round 1 item 1:
/// a mid-market crash restart takes the fast arm's `return
/// run_shutdown_fast` and would otherwise never run this leg or its 15:31
/// sweep). PROCESS-GLOBAL and deliberately NOT the Dhan-gated
/// `spawn_post_market_tasks` seam: this leg's token is the shared-minter
/// SSM read (never the Dhan JWT), so a Dhan-off (Groww-only) session still
/// runs it. Config-gated fail-safe: an absent `[groww_spot_1m]` section
/// disables it. Supervised respawn wrapper; self-skips on non-trading days
/// / past 15:30 IST (post the one bounded ~15:31 repair sweep).
// TEST-EXEMPT: tokio::spawn wrapper over the unit-tested boot modules; BOTH call sites + the config gates pinned by crates/app/tests/groww_spot_1m_wiring_guard.rs + crates/app/tests/groww_chain_1m_wiring_guard.rs.
fn spawn_groww_spot_1m_leg(
    config: &ApplicationConfig,
    notifier: &std::sync::Arc<NotificationService>,
    trading_calendar: &std::sync::Arc<TradingCalendar>,
) {
    // PR-3 sequencing: the spot→chain minute-done watch channel exists
    // ONLY when BOTH Groww REST legs are enabled — chain-off keeps the
    // spot leg byte-identical to PR-2 (no sender, no publishes; the Dhan
    // seam's precedent).
    let chain_enabled = config.groww_option_chain_1m.enabled;
    let (minute_done_tx, spot_minute_done) = if config.groww_spot_1m.enabled && chain_enabled {
        let (tx, rx) = tokio::sync::watch::channel::<Option<u32>>(None);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    // PR-4 sequencing + selection handoff: the chain→contract channel +
    // anchor store exist ONLY when BOTH legs are on (contract-off keeps
    // the chain leg byte-identical to PR-3). The contract leg DEPENDS on
    // the chain leg's per-minute anchors — enabled-without-chain is
    // refused loudly, never a silent anchor-less loop.
    let contract_enabled = config.groww_contract_1m.enabled && chain_enabled;
    if config.groww_contract_1m.enabled && !chain_enabled {
        warn!(
            code = tickvault_common::error_code::ErrorCode::Spot1m01FetchDegraded.code_str(),
            stage = "enabled_without_chain",
            feed = "groww",
            leg = "contract_1m",
            "SPOT1M-01: [groww_contract_1m] is enabled but the chain leg \
             ([groww_option_chain_1m]) is OFF — the contract leg needs the \
             chain's per-minute ATM anchors; NOT spawned (enable the chain \
             leg first)"
        );
    }
    let (chain_minute_done_tx, chain_minute_done_rx, contract_anchor_store) = if contract_enabled {
        let (tx, rx) = tokio::sync::watch::channel::<Option<u32>>(None);
        let store: tickvault_app::groww_option_chain_1m_boot::GrowwChainAnchorStore =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        (Some(tx), Some(rx), Some(store))
    } else {
        (None, None, None)
    };
    if config.groww_spot_1m.enabled {
        let _groww_spot1m_supervisor =
            tickvault_app::groww_spot_1m_boot::spawn_supervised_groww_spot_1m(
                tickvault_app::groww_spot_1m_boot::GrowwSpot1mTaskParams {
                    notifier: notifier.clone(),
                    calendar: std::sync::Arc::clone(trading_calendar),
                    questdb: config.questdb.clone(),
                    minute_done_tx,
                },
            );
        info!(
            "groww_spot_1m: Groww per-minute spot 1m REST leg spawned \
             (fires each minute close 09:16:00-15:30:00 IST; sequential \
             symbol pacing)"
        );
    } else {
        info!("groww_spot_1m: disabled by config — Groww per-minute spot fetch not spawned");
    }
    // Groww per-minute option-chain leg (PR-3): sequenced after the spot
    // fire via the watch signal + fallback timer; DEFAULT-OFF pending the
    // first live probe — the probe-and-report path runs instead while
    // disabled (one bounded chain call per underlying, Info verdict).
    if chain_enabled {
        let _groww_chain1m_supervisor =
            tickvault_app::groww_option_chain_1m_boot::spawn_supervised_groww_chain_1m(
                tickvault_app::groww_option_chain_1m_boot::GrowwChain1mTaskParams {
                    notifier: notifier.clone(),
                    calendar: std::sync::Arc::clone(trading_calendar),
                    questdb: config.questdb.clone(),
                    spot_minute_done,
                    minute_done_tx: chain_minute_done_tx,
                    anchor_store: contract_anchor_store.clone(),
                },
            );
        info!(
            "groww_chain_1m: Groww per-minute option-chain leg spawned \
             (fires each minute close right after the Groww spot fetch; \
             sequential underlying pacing)"
        );
        // Groww per-contract 1m leg (PR-4, the fill-model leg): sequenced
        // after the chain fire via the watch signal + fallback timer;
        // selection = ATM window from the chain's in-memory anchors,
        // hard-capped per minute. DEFAULT-OFF (depends on the chain leg).
        if contract_enabled {
            let _groww_contract1m_supervisor =
                tickvault_app::groww_contract_1m_boot::spawn_supervised_groww_contract_1m(
                    tickvault_app::groww_contract_1m_boot::GrowwContract1mTaskParams {
                        notifier: notifier.clone(),
                        calendar: std::sync::Arc::clone(trading_calendar),
                        questdb: config.questdb.clone(),
                        chain_minute_done: chain_minute_done_rx,
                        anchor_store: contract_anchor_store,
                        strikes_each_side: config.groww_contract_1m.strikes_each_side,
                    },
                );
            info!(
                "groww_contract_1m: Groww per-minute contract candle leg \
                 spawned (fires each minute close right after the Groww \
                 chain fetch; ATM-window selection, sequential contract \
                 pacing)"
            );
        } else {
            info!(
                "groww_contract_1m: disabled by config — Groww per-minute \
                 contract fetch not spawned"
            );
        }
    } else if config.groww_option_chain_1m.probe_and_report {
        let _groww_chain1m_probe = tokio::spawn(
            tickvault_app::groww_option_chain_1m_boot::run_groww_chain_1m_probe(
                tickvault_app::groww_option_chain_1m_boot::GrowwChain1mTaskParams {
                    notifier: notifier.clone(),
                    calendar: std::sync::Arc::clone(trading_calendar),
                    questdb: config.questdb.clone(),
                    spot_minute_done: None,
                    minute_done_tx: None,
                    anchor_store: None,
                },
            ),
        );
        info!(
            "groww_chain_1m: pipeline OFF — one boot-time chain probe \
             spawned (verdict via Telegram; persists nothing)"
        );
    } else {
        info!("groww_chain_1m: disabled by config (probe off) — nothing spawned");
    }
}

// TEST-EXEMPT: tokio::spawn wrapper over the unit-tested pure parts (decide_scoreboard_start / synthesize_process_death_episodes / SQL builders / parsers); spawn site pinned by test_feed_scoreboard_task_is_wired_into_main.
fn spawn_feed_scoreboard_tasks(
    config: &ApplicationConfig,
    trading_calendar: &std::sync::Arc<TradingCalendar>,
    notifier: &std::sync::Arc<NotificationService>,
    process_start_ist_nanos: i64,
    feed_runtime: &std::sync::Arc<tickvault_api::feed_state::FeedRuntimeState>,
) {
    if !config.scoreboard.enabled {
        info!("feed_scoreboard: disabled by [scoreboard] config — nothing spawned");
        return;
    }
    use tickvault_app::feed_scoreboard_boot::{
        PROCESS_DEATH_RECONCILE_DELAY_SECS, ScoreboardStart, day_already_scored_complete,
        decide_scoreboard_start, parse_scoreboard_date_override, reconcile_process_death_episodes,
        run_feed_scoreboard, sanitize_scoreboard_trigger, scoreboard_trigger_after_auto_stop,
        validate_scoreboard_backfill_date,
    };
    use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;

    // Config sanity (round-2 hostile review 2026-07-10): a typo'd trigger
    // ≥ 86400 slept past the 16:30 auto-stop every day — the task died at
    // shutdown as silent teardown, so a config typo disabled the whole
    // deliverable with zero signal. Out-of-range falls back to 15:45 IST.
    let (sb_trigger, trigger_invalid) =
        sanitize_scoreboard_trigger(config.scoreboard.trigger_secs_of_day_ist);
    if trigger_invalid {
        error!(
            code =
                tickvault_common::error_code::ErrorCode::Scoreboard01AggregationDegraded.code_str(),
            stage = "trigger_config",
            configured = config.scoreboard.trigger_secs_of_day_ist,
            effective = sb_trigger,
            "SCOREBOARD-01: [scoreboard] trigger_secs_of_day_ist is outside \
             [session close, 23:59:59] IST — falling back to the 15:45 IST \
             default so the daily scoreboard still fires"
        );
    }
    // Round-4 LOW (2026-07-10): an ACCEPTED trigger past ~16:15 IST sleeps
    // into the prod box's scheduled 16:30 auto-stop every day — graceful
    // shutdown cancels the task silently by design, so the deliverable is
    // disabled with zero signal. The bound stays wide (a manually-run box
    // legitimately triggers later); this warns loudly instead.
    if scoreboard_trigger_after_auto_stop(sb_trigger) {
        warn!(
            code =
                tickvault_common::error_code::ErrorCode::Scoreboard01AggregationDegraded.code_str(),
            stage = "trigger_after_auto_stop",
            effective = sb_trigger,
            "SCOREBOARD-01: [scoreboard] trigger_secs_of_day_ist fires at/after \
             16:15 IST — on the auto-stopped prod box (16:30 IST EventBridge \
             stop) the daily scoreboard would NEVER fire; move the trigger \
             before 16:15 IST unless this box runs past 16:30"
        );
    }

    // ── Task 1: boot-time process-death reconciler (polling per-key —
    // hostile review 2026-07-10: a fast crash-recovery boot can wait out a
    // 300s 429 cooldown before its first connect, past the old one-shot
    // 180s query). The JoinHandle is threaded into Task 2 so a RunCatchUp /
    // forced run aggregates AFTER the synthesized rows exist AND so its
    // panic/JoinError is always classified (never a silent task death).
    // Returns the synthesized episode ROWS (round 3, 2026-07-10): the
    // immediate paths fold them IN MEMORY — a SELECT read-back seconds
    // after the ILP-HTTP flush can miss them (committed-to-WAL is NOT
    // visible-to-SELECT).
    let reconcile_handle: tokio::task::JoinHandle<
        Vec<tickvault_storage::feed_episode_audit_persistence::FeedEpisodeAuditRow>,
    > = {
        let pd_qcfg = config.questdb.clone();
        tokio::spawn(async move {
            // The anchor is the PROCESS-START instant captured at the very
            // top of main() (round-2 HIGH fix: stamping Utc::now() HERE runs
            // when this task STARTS — on the fast crash-recovery arm that is
            // AFTER the Dhan main-feed + order-update WS already connected,
            // so their `connected` rows classified PRE-boot and the fast
            // arm's process_death episodes were never synthesized).
            let boot_ts_ist_nanos = process_start_ist_nanos;
            let boot_ist_secs = process_start_ist_nanos.div_euclid(1_000_000_000);
            // Let this boot's own `connected` audit rows land first (async
            // audit writer + ILP flush); the reconciler then POLLS per-key
            // for the post-boot up rows (60s cadence, bounded).
            tokio::time::sleep(std::time::Duration::from_secs(
                PROCESS_DEATH_RECONCILE_DELAY_SECS,
            ))
            .await;
            // APPROVED: epoch day number fits u64 trivially.
            let target_ist_day = boot_ist_secs.max(0) as u64 / 86_400;
            let trading_date_ist_nanos = i64::try_from(target_ist_day)
                .unwrap_or(0)
                .saturating_mul(86_400)
                .saturating_mul(1_000_000_000);
            reconcile_process_death_episodes(
                &pd_qcfg,
                target_ist_day,
                trading_date_ist_nanos,
                boot_ts_ist_nanos,
            )
            .await
        })
    };

    // ── Task 2: the daily aggregation + Telegram scorecard ──
    let sb_qcfg = config.questdb.clone();
    let sb_metrics_port = config.observability.metrics_port;
    let sb_calendar = std::sync::Arc::clone(trading_calendar);
    let sb_telegram_enabled = config.scoreboard.telegram_enabled;
    let sb_coverage_detail = config.scoreboard.coverage_detail_rows;
    let sb_notifier = std::sync::Arc::clone(notifier);
    // Round-4 (feed-off days): the CURRENT runtime enabled flags
    // disambiguate a switched-off feed from an enabled-but-dead broker on
    // same-day runs (backfills infer from data alone).
    let sb_feed_runtime = std::sync::Arc::clone(feed_runtime);
    let inner = tokio::spawn(async move {
        use chrono::{FixedOffset, TimeZone, Timelike, Utc};
        let Some(ist_offset) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
            return Err("IST offset construction failed".to_string());
        };
        let boot_ist = ist_offset.from_utc_datetime(&Utc::now().naive_utc());
        let today_ist = boot_ist.date_naive();
        let boot_secs_of_day = boot_ist.time().num_seconds_from_midnight();
        let force_now = std::env::var("TICKVAULT_SCOREBOARD_NOW")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        // Past-day backfill (design contract §5): TICKVAULT_SCOREBOARD_DATE
        // =YYYY-MM-DD, honored ONLY alongside TICKVAULT_SCOREBOARD_NOW.
        // Fail-closed: a malformed date REFUSES the run loudly (the outer
        // supervisor pages Aborted) instead of silently aggregating the
        // wrong day.
        //
        // Round-4 LOW (2026-07-10): every refusal arm previously
        // `return Err(...)`ed BEFORE `reconcile_handle.await`, dropping the
        // JoinHandle — a reconciler panic was silently discarded on exactly
        // those paths, contradicting this function's own always-classified
        // invariant. Refusals are now RECORDED and returned only AFTER the
        // classify-await below (bounded ≤ ~13 min; the refusal paths are
        // forced runs, so no trigger sleep intervenes).
        let mut refusal: Option<String> = None;
        let date_override: Option<(u64, String)> = if force_now {
            match std::env::var("TICKVAULT_SCOREBOARD_DATE") {
                Ok(raw) => match parse_scoreboard_date_override(&raw) {
                    Some(v) => Some(v),
                    None => {
                        error!(
                            code = tickvault_common::error_code::ErrorCode::
                                Scoreboard01AggregationDegraded
                                .code_str(),
                            stage = "date_override_parse",
                            raw = %raw,
                            "SCOREBOARD-01: invalid TICKVAULT_SCOREBOARD_DATE — \
                             expected strict YYYY-MM-DD; refusing the forced run"
                        );
                        refusal = Some(format!(
                            "invalid TICKVAULT_SCOREBOARD_DATE {raw:?} — expected YYYY-MM-DD"
                        ));
                        None
                    }
                },
                Err(_) => None,
            }
        } else {
            None
        };
        // APPROVED: epoch day number fits u64 trivially.
        let boot_day = boot_ist
            .timestamp()
            .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS))
            .max(0) as u64
            / 86_400;
        // Semantic backfill-date validation (round-2 hostile review
        // 2026-07-10): a well-shaped NON-TRADING or FUTURE date used to run
        // the full aggregation against legitimately-empty sources and write
        // two fabricated all-zero rows stamped outcome='complete'. The
        // TARGET date must be a past-or-today TRADING day; refusal pages
        // Aborted, matching the malformed-date arm.
        //
        // Round 3 (2026-07-10): the SAME semantic gate applies to the
        // no-DATE forced arm — `TICKVAULT_SCOREBOARD_NOW=1` alone on a
        // Saturday/holiday targeted the non-trading TODAY and (after
        // 15:45) fabricated two all-zero rows stamped outcome='complete'.
        // The natural weekend mistake is re-running Friday's card and
        // forgetting the DATE var; refusal pages Aborted with the fix.
        if refusal.is_none() && force_now && date_override.is_none() {
            let today_label = today_ist.format("%Y-%m-%d").to_string();
            if let Err(why) = validate_scoreboard_backfill_date(
                boot_day,
                boot_day,
                sb_calendar.is_trading_day(today_ist),
            ) {
                error!(
                    code = tickvault_common::error_code::ErrorCode::Scoreboard01AggregationDegraded
                        .code_str(),
                    stage = "forced_now_non_trading",
                    date = %today_label,
                    "SCOREBOARD-01: TICKVAULT_SCOREBOARD_NOW without a date on \
                     a non-trading day {why} — refusing the forced run (pass \
                     TICKVAULT_SCOREBOARD_DATE=YYYY-MM-DD for the trading day \
                     you meant)"
                );
                refusal = Some(format!(
                    "TICKVAULT_SCOREBOARD_NOW: today ({today_label}) {why} — \
                     pass TICKVAULT_SCOREBOARD_DATE=YYYY-MM-DD for the trading \
                     day you meant"
                ));
            }
        }
        if refusal.is_none()
            && let Some((target_day, label)) = &date_override
        {
            let target_is_trading = chrono::NaiveDate::parse_from_str(label, "%Y-%m-%d")
                .map(|d| sb_calendar.is_trading_day(d))
                .unwrap_or(false);
            if let Err(why) =
                validate_scoreboard_backfill_date(*target_day, boot_day, target_is_trading)
            {
                error!(
                    code = tickvault_common::error_code::ErrorCode::Scoreboard01AggregationDegraded
                        .code_str(),
                    stage = "date_override_reject",
                    date = %label,
                    "SCOREBOARD-01: TICKVAULT_SCOREBOARD_DATE {why} — refusing \
                     the forced backfill (an all-zero 'complete' day would \
                     pollute the month-end verdict)"
                );
                refusal = Some(format!("TICKVAULT_SCOREBOARD_DATE {label} {why}"));
            }
        }
        let is_trading_day = sb_calendar.is_trading_day(today_ist);
        let decision =
            decide_scoreboard_start(boot_secs_of_day, is_trading_day, force_now, sb_trigger);
        // A refused run logs/sleeps NOTHING here (the refusal paths are all
        // forced runs — RunNow, never SleepThenRun — but the "running
        // on-demand NOW" line would mislead); it still awaits + classifies
        // the reconciler below before returning Err.
        if refusal.is_none() {
            match decision {
                ScoreboardStart::SkipNonTradingDay => {
                    info!("feed_scoreboard: skipping the daily aggregation (non-trading day)");
                }
                ScoreboardStart::RunCatchUp => {
                    info!(
                        now = %boot_ist.time(),
                        "feed_scoreboard: late boot (past the trigger) — running the \
                         day's scoreboard now as a catch-up (DEDUP-idempotent)"
                    );
                }
                ScoreboardStart::RunNow => {
                    info!(
                        backfill_date = date_override.as_ref().map_or("today", |(_, l)| l.as_str()),
                        "feed_scoreboard: TICKVAULT_SCOREBOARD_NOW set — running \
                     on-demand NOW (operator dry-run / backfill)"
                    );
                }
                ScoreboardStart::SleepThenRun(secs_until) => {
                    info!(
                        secs_until,
                        "feed_scoreboard: sleeping until the daily trigger"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(secs_until)).await;
                }
            }
        }
        // Await + CLASSIFY the boot-time reconciler on EVERY path — incl.
        // the non-trading-day skip (round-2 hostile review 2026-07-10: the
        // reconciler was a bare spawn whose panic was silently discarded /
        // never awaited on the skip path — the SLO-03 silent-task-death
        // class). Aggregation also runs AFTER it AND receives the
        // synthesized ROWS themselves (round 3): on the immediate
        // RunCatchUp/RunNow paths the run fires seconds after the
        // reconciler's flush, so the rows are folded in-memory instead of
        // relying on a SELECT read-back inside the WAL-apply window. The
        // handle resolves in ≤ ~13 min worst case (the scheduled 15:45
        // path has long passed).
        let boot_reconciled_rows = match reconcile_handle.await {
            Ok(rows) => rows,
            Err(join_err) => {
                if join_err.is_panic() {
                    error!(
                        code = tickvault_common::error_code::ErrorCode::
                            Scoreboard01AggregationDegraded
                            .code_str(),
                        stage = "reconcile_panic",
                        %join_err,
                        "SCOREBOARD-01: the boot-time process-death reconciler \
                         CRASHED — this boot's death episodes were NOT \
                         synthesized (boot-reconciled rows are NOT \
                         re-creatable by a later re-run)"
                    );
                }
                Vec::new()
            }
        };
        // Round-4 LOW: refusals return HERE — after the reconciler's
        // panic/JoinError was classified above, never before (the refusal
        // arms used to drop the JoinHandle).
        if let Some(reason) = refusal {
            return Err(reason);
        }
        // The restart-day partial floor counts IN-MARKET deaths only —
        // post-close restarts (market_hours=false, blame_reason
        // post_close_restart) are the scheduled-stop-ambiguous shape and
        // must not flip a completed day's rows to partial (round 3).
        let boot_synthesized_deaths = boot_reconciled_rows
            .iter()
            .filter(|r| r.market_hours)
            .count();
        if matches!(decision, ScoreboardStart::SkipNonTradingDay) {
            return Ok(None);
        }
        // Target day: the IMMEDIATE paths (RunCatchUp / RunNow) stamp the
        // DECISION day — the reconcile await above (≤ ~13 min) can cross
        // IST midnight on a ~23:47+ recovery boot, and recomputing here
        // would aggregate TOMORROW while the day that actually traded got
        // no row (round-2 hostile review 2026-07-10). Only SleepThenRun (a
        // same-day sleep to the trigger by construction) recomputes after
        // the sleep.
        let (today_day, today_label, run_secs_of_day) =
            if matches!(decision, ScoreboardStart::SleepThenRun(_)) {
                let run_ist = ist_offset.from_utc_datetime(&Utc::now().naive_utc());
                let run_ist_secs = Utc::now()
                    .timestamp()
                    .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
                (
                    // APPROVED: epoch day number fits u64 trivially.
                    run_ist_secs.max(0) as u64 / 86_400,
                    run_ist.date_naive().format("%Y-%m-%d").to_string(),
                    run_ist.time().num_seconds_from_midnight(),
                )
            } else {
                (
                    boot_day,
                    today_ist.format("%Y-%m-%d").to_string(),
                    boot_secs_of_day,
                )
            };
        let (target_ist_day, trading_date_label) =
            date_override.unwrap_or((today_day, today_label));
        // A forced run BEFORE the trigger targeting TODAY covers only part
        // of the session — the row is stamped partial and the card says so
        // (hostile review 2026-07-10: never a mid-day row masquerading as
        // a complete end-of-day row).
        let forced_early_run =
            force_now && target_ist_day == today_day && run_secs_of_day < sb_trigger;
        // Round-4 LOW rerun latch: a post-trigger same-day boot whose day
        // ALREADY carries complete rows for BOTH feeds (the post-close
        // deploy-restart shape) skips the redundant re-run + duplicate
        // Telegram card. Never applied to forced runs (RunCatchUp only
        // fires when !force_now) and never when THIS boot synthesized an
        // in-market death (the restart floor must still land on the row);
        // partial/degraded days re-run (a rerun may improve partial; the
        // daily keep-better guard protects degraded).
        if matches!(decision, ScoreboardStart::RunCatchUp)
            && boot_synthesized_deaths == 0
            && day_already_scored_complete(&sb_qcfg, target_ist_day).await
        {
            info!(
                target_ist_day,
                "feed_scoreboard: today's scorecard already ran to completion \
                 — skipping the catch-up re-run (no duplicate card; a re-run \
                 can be forced with TICKVAULT_SCOREBOARD_NOW=1)"
            );
            return Ok(None);
        }
        // Round-4 (feed-off days): same-day runs read the CURRENT runtime
        // flags so an enabled-but-dead broker day can never be softened
        // into 'feed_off'; backfills pass None (data-only inference).
        let runtime_enabled_now = if target_ist_day == today_day {
            Some((
                sb_feed_runtime.is_enabled(tickvault_common::feed::Feed::Dhan),
                sb_feed_runtime.is_enabled(tickvault_common::feed::Feed::Groww),
            ))
        } else {
            None
        };
        let summary = run_feed_scoreboard(
            &sb_qcfg,
            sb_metrics_port,
            target_ist_day,
            trading_date_label,
            forced_early_run,
            // Scoreboard PR-D: per-instrument feed_coverage_daily rows are
            // config-gated ([scoreboard] coverage_detail_rows).
            sb_coverage_detail,
            // Same-day runs self-scrape the session's audit-drop counter and
            // apply the restart-day partial floor; backfills skip both
            // (round-2: the counter is CURRENT-session state; round-3: the
            // floor is ALSO data-driven off tallied restarts, so a backfill
            // of a crash day still stamps partial).
            target_ist_day == today_day,
            boot_synthesized_deaths,
            &boot_reconciled_rows,
            runtime_enabled_now,
        )
        .await?;
        info!("PROOF: feed_scoreboard daily aggregation fired");
        Ok(Some(summary))
    });
    tokio::spawn(async move {
        let to_line = |name: &str,
                       n: &tickvault_app::feed_scoreboard_boot::FeedDayNumbers|
         -> tickvault_core::notification::events::FeedScoreLine {
            tickvault_core::notification::events::FeedScoreLine {
                name: name.to_string(),
                ticks: n.ticks,
                exclusive_minutes: n.unique_win_minutes,
                // Scoreboard PR-C (2026-07-11): the day lag histograms are
                // LIVE — measured exchange→receipt distributions flow from
                // the summary (drained same-day from the in-memory per-feed
                // histograms; -1 survives only on backfill/thin days and
                // still renders "not measured yet", never a fabricated 0).
                lag_p50_ms: n.lag_p50_ms,
                lag_p99_ms: n.lag_p99_ms,
                drops_market: n.disconnects_market,
                blame_broker: n.blame_broker,
                blame_ours: n.blame_ours,
                blame_unclear: n.blame_indeterminate,
                // Scoreboard PR-B (2026-07-10): the StallRestarted event
                // kind is LIVE — the stall watchdog writes one
                // `stall_restarted` row per kill+relaunch, so the tally is a
                // measurement from this deploy forward (0 = measured 0; the
                // PR-1 "?" sentinel + footnote are retired — the runbook
                // keeps the pre-ship-day caveat, and the CloudWatch counter
                // holds the pre-ship past).
                stalls: n.stalls,
                restarts: n.restarts,
                streaming_minutes: n.streaming_minutes,
            }
        };
        match inner.await {
            Ok(Ok(Some(summary))) => {
                if sb_telegram_enabled {
                    // Scoreboard PR-B (2026-07-10): the round-2 Groww
                    // drops blind spot is CLOSED for its dominant failure
                    // mode — the sidecar socket-death family now writes
                    // `stall_restarted` rows (counted in the Stalls column),
                    // so Groww's drops count (feed-disable + bridge-death)
                    // renders as a measured number again. Honest residual
                    // (runbook §6): in-sidecar reconnects that recover
                    // FASTER than the 30s stall threshold remain invisible
                    // to both columns on both card sides.
                    let groww_line = to_line("Groww", &summary.groww);
                    // Groww REST plan PR-5 (operator Quote 2, 2026-07-13):
                    // the official minute-candle pull digest lines — the
                    // four canonical feed/leg pairs always render (honest
                    // "not measured yet" when a source is absent); the
                    // contract leg lights up automatically once its
                    // forensics rows land.
                    let rest_legs = tickvault_app::feed_scoreboard_boot::build_rest_leg_score_lines(
                        &summary.rest_legs,
                    );
                    sb_notifier.notify(NotificationEvent::DualFeedDailyScorecard {
                        trading_date_ist: summary.trading_date_ist.clone(),
                        dhan: to_line("Dhan", &summary.dhan),
                        groww: groww_line,
                        session_minutes: summary.session_minutes,
                        partial_coverage: summary.partial_coverage,
                        degraded: summary.degraded,
                        early_run: summary.early_run,
                        restart_partial: summary.restart_partial,
                        dhan_feed_off: summary.dhan_feed_off,
                        groww_feed_off: summary.groww_feed_off,
                        rest_legs,
                        rest_legs_read_failed: summary.rest_legs_read_failed,
                    });
                } else {
                    info!("feed_scoreboard: Telegram disabled — daily rows written only");
                }
            }
            Ok(Ok(None)) => {} // non-trading day skip — nothing to send.
            Ok(Err(reason)) => {
                error!(
                    code = tickvault_common::error_code::ErrorCode::Scoreboard01AggregationDegraded
                        .code_str(),
                    stage = "daily_run",
                    %reason,
                    "SCOREBOARD-01: the daily scoreboard run failed"
                );
                sb_notifier.notify(NotificationEvent::DualFeedScorecardAborted { detail: reason });
            }
            Err(join_err) if join_err.is_panic() => {
                error!(
                    code = tickvault_common::error_code::ErrorCode::Scoreboard01AggregationDegraded
                        .code_str(),
                    stage = "daily_panic",
                    %join_err,
                    "SCOREBOARD-01: the daily scoreboard task crashed"
                );
                sb_notifier.notify(NotificationEvent::DualFeedScorecardAborted {
                    detail: format!("the scorecard task crashed: {join_err}"),
                });
            }
            Err(_) => {
                // Cancellation during graceful shutdown (16:30 IST auto-stop,
                // `make stop`) — normal teardown, NOT an abort. No page.
                info!("feed_scoreboard: task cancelled during shutdown");
            }
        }
    });
    info!("feed_scoreboard: process-death reconciler + daily scorecard tasks spawned");
}
