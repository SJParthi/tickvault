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
    CONFIG_BASE_PATH, CONFIG_LOCAL_PATH, IstTimer, check_clock_drift, compute_close_seal_sleep,
    compute_market_close_sleep, create_error_log_writer, format_bind_addr,
    should_emit_post_market_alert,
};
use tickvault_app::{infra, observability, subsystem_memory};

use std::net::SocketAddr;

use anyhow::{Context, Result};
use figment::Figment;
use figment::providers::{Format, Toml};
use tracing::{debug, error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use tickvault_common::config::ApplicationConfig;
use tickvault_common::trading_calendar::TradingCalendar;
// PR-C2 (2026-07-13): the Dhan live-WS lane imports (subscription planner,
// WS pool, tick processor, token cache/manager, IP verifier) retired with
// the lane deletion.
use tickvault_core::notification::{NotificationEvent, NotificationService};
// Phase C1 (2026-07-13): relocated from this binary to the lib so
// dhan_rest_stack can create its own ws_event_audit consumer (Q4-i rewire).
use tickvault_app::ws_audit_consumer::spawn_ws_event_audit_consumer;

// PR-E (2026-05-26): ensure_candle_table_dedup_keys retired alongside
// the deleted candle_persistence module + historical_candles table.
// PR #3 (2026-05-19): `greeks_persistence` retired. Migration SQL
// `scripts/migrate-drop-greeks-tables.sql` drops the option_greeks /
// pcr_snapshots / dhan_option_chain_raw / greeks_verification tables.

// PR #3 (2026-05-19): `InlineGreeksComputer` retired alongside the
// trading::greeks module. `build_inline_greeks_enricher` below now
// returns `Option<NoopGreeksEnricher>::None` so the tick processor's
// generic over `GreeksEnricher` keeps compiling without computing.

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
/// Called from the single boot-completion point (PR-C2, 2026-07-13 — the
/// fast crash-recovery arm is deleted), reached ONLY after every boot gate
/// has passed — a halt uses
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
    // 2026-07-10; PR-C2 2026-07-13: the FAST crash-recovery arm + the Dhan
    // WS pool are deleted, so the anchor now simply precedes the single
    // `spawn_feed_scoreboard_tasks` call in the process-global prefix).
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
    // PR-C2 (2026-07-13): the D2b `LaneState` FSM seed that lived here is
    // deleted with the Dhan live-WS lane — no pool, no lane FSM, no runtime
    // cold-start supervisor. `mark_dhan_lane_running()` /
    // `mark_dhan_pool_present()` stay permanently unset (no pool-spawn site
    // exists anymore).
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
    // PR-C2 (2026-07-13): the Dhan dormant activation watcher
    // (`dhan_activation.rs`) and the D2b runtime cold-start supervisor
    // (`run_dhan_lane_runtime_supervisor` + `dhan_lane_ctx_cell`) that were
    // spawned here are DELETED with the Dhan live-WS lane. The /api/feeds
    // Dhan enable arm permanently 409-refuses (the lane no longer exists);
    // Dhan runs REST-only via `dhan_rest_stack` spawned further down.

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

    // (PR-C2, 2026-07-13: the Wave-2 `websocket::connection::set_market_calendar`
    // global install retired with the deleted main-feed sleep path — the
    // surviving order-update WS takes the calendar as an explicit parameter.)

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
                // §36.7 far-month exclusion RETIRED in PR-C2 (2026-07-13):
                // the exclusion set was seeded ONLY from the deleted Dhan
                // lane's subscription plan (`seed_tick_gap_detector_from_plan`
                // / `store_far_month_future_exclusions`), so post-retirement
                // it was permanently empty — the filter is now the honest
                // constant 0. The whole detector (this scan loop included)
                // is deleted in PR-C3 with WS-GAP-06 (operator Q4-ii).
                let excluded_silent = 0usize;
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
    // PR-C2 (2026-07-13): with the Dhan live WS retired there is no frame
    // APPEND site left in this process (the dhan_rest_stack order-update WS
    // deliberately runs wal_spill=None while dormant — C1 dormancy honesty),
    // but the WAL WRITER + fail-closed init are KEPT: the WAL dir remains the
    // replay/archive floor consumed above and by the 15:40 conservation audit.
    let _ws_frame_spill = match tickvault_storage::ws_frame_spill::WsFrameSpill::new(&ws_wal_path) {
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

    // =======================================================================
    // Step C — PER-FEED BOOT DISPATCHER (pluggable-feed-runtime.md §6),
    // SINGLE-PATH since PR-C2 (2026-07-14, Dhan live-WS lane deletion).
    //
    // "A feed's code runs IFF its enable flag is true." All shared infra
    // (observability, WAL replay, trading calendar, errors.jsonl) is already
    // up, and the Groww lane (auth + bridge) was already spawned above gated
    // on `groww_enabled`. This block is now a LOGGING dispatcher only: there
    // is no Dhan fast/slow boot to skip anymore — the two-phase
    // (fast-crash-recovery vs slow) Dhan boot arms, the lane gate, and the
    // D2b runtime cold-start were all DELETED with the lane (operator
    // retirement directive 2026-07-13; websocket-connection-scope-lock.md
    // "2026-07-13 Amendment" §B). Every boot flows through ONE path:
    // shared prefix → build_shared_infra → WAL settlement → REST stack
    // (unconditional) → READY → run_process_runloop. `dhan_enabled=true` is
    // an ILLEGAL post-retirement config (logged loudly + ignored at the
    // REST-stack spawn below — dhan_enabled=false is the prod reality).
    // The OFF-feed-isolation guarantee is now BY CONSTRUCTION: no Dhan auth
    // beyond the REST stack's own, no instrument fetch, no Dhan WebSocket
    // (except the stack's functional-dormant order-update WS) exists on any
    // path.
    // =======================================================================
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
    // hostile-review round 1 CRITICAL). The unit sets WatchdogSec=60. This
    // thin, FEED-AGNOSTIC, process-lifetime task pings every
    // WATCHDOG_INTERVAL_SECS (30s) from the shared boot prefix, BEFORE the
    // boot path sends READY=1 (the READY site sits below this line in
    // source order; a source-scan ratchet pins that ordering + the absence
    // of any feed gate on this spawn). Sending READY=1 with NOBODY pinging
    // would ARM the watchdog -> systemd SIGABRTs the process at t+60s ->
    // Restart=always loop -> StartLimitBurst hard-fail minutes after a
    // deploy that reported success (Rule-11 false-OK). PR-C2 (2026-07-13,
    // Dhan live-WS retirement): the Dhan-gated lane/fast-arm
    // `spawn_heartbeat_watchdog` twins are DELETED with the lane — this
    // process-global pinger is now the SOLE WATCHDOG=1 source. No-op
    // outside systemd (`notify_systemd_watchdog` requires NOTIFY_SOCKET).
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

    // =====================================================================
    // BOOT (single path since PR-C2, 2026-07-13 — the Dhan live-WS lane and
    // its FAST crash-recovery arm are DELETED per the operator's retirement
    // directive; Groww is the sole live feed, Dhan is REST-only).
    // =====================================================================
    info!("standard boot — shared infra, then per-feed lanes (Groww live; Dhan REST-only)");

    // =======================================================================
    // D2 Stage 2 — HOISTED PROCESS-SHARED INFRA (the single boot path)
    //
    // Build the PROCESS-shared infra ONCE here: notifier (+ Docker auto-start),
    // health registry, seal-writer (installs the process-wide global_seal_sender),
    // the tick + order-update broadcasts, the obs / 21-TF aggregator / tick-storage
    // subscriber tasks, and the axum API server (incl. /api/feeds, so the
    // toggle endpoint exists regardless of feed state). The single
    // `run_process_runloop` below keeps the process alive.
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
    //
    // DORMANT SINCE PR-C2 (2026-07-14, Dhan live-WS lane deletion): the tick
    // broadcast this consumer drains is PUBLISHER-LESS (the lane's
    // `run_tick_processor` was the only publisher; Groww's IDX ticks flow
    // through its OWN bridge, never this channel) — so the tracker can never
    // arm and the INDEX-OHLC machinery can never fire. Retained un-deleted
    // because the wiring is cheap, self-contained, and publisher-ready; its
    // retain-vs-delete decision is a C3 call alongside the rest of the idle
    // tick-broadcast consumers (see the plan's Observability note).
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
    // STAGE-C.2b (PR-C2, 2026-07-13): WAL replay settlement — single path.
    //
    // The Dhan live-WS lane (and with it the pool frame channel the LiveFeed
    // re-injection targeted) is DELETED per the operator's 2026-07-13
    // retirement directive. Two legs remain:
    //   - OrderUpdate frames drain into the PROCESS-shared broadcast created
    //     by `build_shared_infra` (closing the C1 "boot-staged order-update
    //     WAL segments remain undrained on dhan-off boots" residual — the
    //     dhan_rest_stack's order-update WS + dormant drain consume the same
    //     broadcast).
    //   - Residual LiveFeed frames (possible only from a PRE-retirement
    //     session's WAL) have NO live consumer anymore: they are counted +
    //     logged loudly, then archived WITH the segments below — the raw
    //     frames stay on disk in the WAL archive (forensic), and NOT
    //     confirming would re-stage them every boot forever (the
    //     WS-REINJECT-01 growth-storm class).
    // =======================================================================
    if !ws_wal_replay_order_update.is_empty() {
        let frames = std::mem::take(&mut ws_wal_replay_order_update);
        let (parsed, broadcast_count, parse_errors) =
            tickvault_app::boot_helpers::drain_replayed_order_updates_to_broadcast(
                frames,
                &order_update_sender,
            );
        info!(
            parsed,
            broadcast_count, parse_errors, "STAGE-C.2b: OrderUpdate WAL replay drain complete"
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
    if !ws_wal_replay_live_feed.is_empty() {
        let dropped = ws_wal_replay_live_feed.len() as u64;
        warn!(
            frames = dropped,
            "STAGE-C.2b: residual LiveFeed WAL frames from a pre-retirement session have no \
             re-injection target (the Dhan live WS was retired 2026-07-13) — counted and \
             archived with the WAL segments; the raw frames remain on disk in the archive"
        );
        metrics::counter!(
            "tv_ws_frame_wal_reinjected_dropped_total",
            "ws_type" => "live_feed"
        )
        .increment(dropped);
        ws_wal_replay_live_feed.clear();
    }
    {
        // Both legs settled (drained or loudly archived) — archive the staged
        // segments so they never re-stage. `confirm_replayed` MOVES segments
        // into the WAL archive dir (never deletes).
        let confirm_ws_wal_path = tickvault_app::tick_conservation_boot::ws_wal_dir();
        tickvault_storage::ws_frame_spill::confirm_replayed(&confirm_ws_wal_path);
    }

    // =======================================================================
    // DHAN REST-ONLY STACK (PR-C2, 2026-07-13 — the only Dhan surface).
    //
    // The Dhan live-WS lane (`start_dhan_lane` + the FAST crash-recovery arm
    // + the D2b runtime cold-start supervisor) is DELETED per the operator's
    // 2026-07-13 retirement directive ("now remove this entire Dhan live
    // websocket feed instruments subscription even entire live websocket
    // feed itself"). The retained Dhan surface — token/auth stack, REST
    // canary, per-minute spot_1m_rest, per-minute option_chain_1m +
    // entitlement probe, and the functional-dormant order-update WS (Q4-i)
    // — is the REST-only stack, spawned unconditionally on every boot.
    // Bring-up is a background task with internal retry-forever loops: it
    // never blocks boot and never halts the process.
    //
    // A raw boot TOML still carrying `dhan_enabled = true` is an ILLEGAL
    // config post-retirement (re-enabling the live WS requires a fresh dated
    // operator quote in websocket-connection-scope-lock.md FIRST): it is
    // logged loudly and otherwise IGNORED — no live lane exists to start,
    // and the REST stack (the only Dhan surface) spawns regardless. The
    // collision the pre-C2 gate guarded against (REST stack vs a runtime
    // lane cold-start fighting over the dual-instance SSM lock) is
    // structurally impossible now — no cold-start path exists.
    // =======================================================================
    if feed_runtime.is_dhan_config_enabled() {
        error!(
            "boot TOML carries dhan_enabled=true but the Dhan live WS lane is RETIRED \
             (operator directive 2026-07-13, deleted in PR-C2) — the flag is IGNORED; \
             fix the config to dhan_enabled=false. The Dhan REST-only stack runs either way."
        );
    }
    let _dhan_rest_stack_monitor = tickvault_app::dhan_rest_stack::spawn_dhan_rest_stack(
        tickvault_app::dhan_rest_stack::DhanRestStackParams {
            config: std::sync::Arc::new(config.clone()),
            notifier: std::sync::Arc::clone(&notifier),
            calendar: std::sync::Arc::clone(&trading_calendar),
            feed_runtime: std::sync::Arc::clone(&feed_runtime),
            // PR-C2: the stack owns the /health token-block writer.
            health: health_status.clone(),
        },
    );

    // =======================================================================
    // Boot completion signals (deploy-hang fix 2026-07-13; unconditional +
    // non-blocking since PR-C2).
    //
    // The systemd unit is Type=notify with TimeoutStartSec=infinity: systemd
    // releases the `systemctl restart tickvault` start job ONLY when the app
    // sends sd_notify(READY=1). READY=1 is sent FIRST (unconditionally):
    // systemd start-job release must never be held hostage to feed liveness
    // — the PROCESS booted; feed health has its own alarms. The
    // feed-liveness-gated `tv_boot_completed` (the 08:40 IST boot-heartbeat
    // alarm's signal) is then emitted from a SPAWNED task (PR-C2 — the C1
    // review MEDIUM: the inline `.await` here delayed `run_process_runloop`,
    // i.e. the StartupComplete Telegram + market-close timer, by up to the
    // bounded BOOT_COMPLETED_FEED_LIVENESS_WAIT_SECS=300 while a slow Groww
    // watch-list build converged). Semantics preserved: the gate genuinely
    // waits (bounded) for the enabled feeds — a Groww that never comes up
    // still withholds the metric so the boot-heartbeat alarm pages (Rule 11,
    // no false-OK). `dhan_poolless_idle=false`: with the lane retired the
    // Dhan term of `boot_completed_should_emit` is vacuous on a legal
    // (dhan_enabled=false) config. No-op when NOTIFY_SOCKET is unset (local
    // `cargo run` boots, which never run under systemd).
    // =======================================================================
    infra::notify_systemd_ready();
    {
        let bc_feed_runtime = std::sync::Arc::clone(&feed_runtime);
        let bc_dhan_enabled = config.feeds.dhan_enabled;
        let bc_groww_enabled = config.feeds.groww_enabled;
        tokio::spawn(async move {
            emit_boot_completed_when_feed_live(
                &bc_feed_runtime,
                bc_dhan_enabled,
                bc_groww_enabled,
                false,
            )
            .await;
        });
    }

    // =======================================================================
    // PROCESS RUN-LOOP (the single boot path).
    //
    // Built once over the hoisted shared infra: market-close timer,
    // partition-detach, shutdown wait, then API + otel teardown.
    // =======================================================================
    run_process_runloop(
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
// Helper: S4-T1d — Slow-boot observability-only consumer
// ---------------------------------------------------------------------------

/// S4-T1d: Gap tracker + QuestDB HTTP health observer (the process-shared
/// obs subscriber). PR-C2 truth-sync (2026-07-14): the "slow-boot" naming
/// and the fast/slow parity prose are historical — there is ONE boot path
/// now, and the tick broadcast this task subscribes to is PUBLISHER-LESS
/// (the lane's `run_tick_processor`, the only Dhan tick publisher, was
/// deleted; Groww persists via its own writer). The task is ADDITIVE —
/// observability only, no writes:
///
/// 1. Subscribes to the tick broadcast (idle post-C2 — kept so the wiring
///    is publisher-ready and the channel never closes under the aggregator)
/// 2. Feeds any tick into `TickGapTracker::record_tick` (no producer today;
///    the detector itself deletes in C3)
/// 3. Every 2 seconds, HTTP-pings QuestDB's `/exec` endpoint and feeds
///    the result into `QuestDbHealthPoller` — the LIVE, load-bearing half:
///    it owns the /health `questdb_reachable` flag write (see the param
///    note below).
async fn run_slow_boot_observability(
    mut tick_rx: tokio::sync::broadcast::Receiver<tickvault_common::tick_types::ParsedTick>,
    questdb_config: tickvault_common::config::QuestDbConfig,
    // PR-C2 (2026-07-13): the deleted Dhan pool watchdog was the ONLY
    // production writer of the /health `questdb_reachable` flag — without a
    // replacement, GET /health (and overall_status) would report QuestDB
    // down FOREVER on the lane-less runtime (Rule-11 false-degraded). This
    // task's 2s /exec ping is the surviving reachability probe, so it now
    // owns the flag write.
    health: tickvault_api::state::SharedHealthStatus,
) {
    info!("S4-T1d: slow-boot observability task started");

    let tick_gap_tracker_capacity =
        tickvault_common::constants::MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION
            .saturating_mul(tickvault_common::constants::MAX_WEBSOCKET_CONNECTIONS);
    let mut tick_gap_tracker =
        tickvault_trading::risk::tick_gap_tracker::TickGapTracker::new(tick_gap_tracker_capacity);

    let mut qdb_health_poller = tickvault_storage::questdb_health::QuestDbHealthPoller::new();
    let qdb_health_interval = std::time::Duration::from_secs(2); // APPROVED: pre-existing literal, session-7 tech debt tracked for cleanup

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

    // PR-C2 (2026-07-13): the QuestDB ping + the 30s stale scan are now
    // CADENCE-driven (a select! interval arm) instead of tick-driven. With
    // the Dhan live WS retired, this broadcast has no publisher — a
    // recv()-gated ping would never fire and /health would stay blind (the
    // same starvation also affected total-silence incidents pre-C2).
    let mut qdb_ping_ticker = tokio::time::interval(qdb_health_interval);
    qdb_ping_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            recv = tick_rx.recv() => match recv {
                Ok(tick) => {
                    // Gap detection — the tracker fires its own log/metric on
                    // ERROR thresholds; no backfill request is published
                    // (in-market backfill disabled by user policy).
                    let _ =
                        tick_gap_tracker.record_tick(tick.security_id, tick.exchange_timestamp);
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
            },
            _ = qdb_ping_ticker.tick() => {
                // Audit finding #2 (2026-04-24): periodic per-instrument stall
                // scan every 30 s — cadence-tracked so the scan runs even when
                // no ticks are flowing (total-silence incidents).
                if last_stale_check.elapsed() >= stale_check_interval {
                    let newly_stale = tick_gap_tracker.detect_stale_instruments();
                    if newly_stale > 0 {
                        debug!(newly_stale, "per-instrument stall scan: newly-stale count");
                    }
                    last_stale_check = std::time::Instant::now();
                }

                // QuestDB HTTP health ping every 2 seconds — feeds the
                // metrics verdict AND the /health `questdb_reachable` flag
                // (the PR-C2 re-home; see the `health` parameter doc above).
                let connected = match http_client.get(&questdb_ping_url).send().await {
                    Ok(resp) => resp.status().is_success(),
                    Err(_) => false,
                };
                health.set_questdb_reachable(connected);
                let verdict = qdb_health_poller.tick(connected, std::time::Instant::now());
                tickvault_storage::questdb_health::emit_metrics_for_verdict(
                    verdict,
                    &qdb_health_poller,
                );
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
/// The single boot path calls this from `build_shared_infra` (PR-C2,
/// 2026-07-13 — the fast crash-recovery arm is deleted).
// WIRING-EXEMPT: call site is `build_shared_infra` below.
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
    // runs safely after the stop (single boot path since PR-C2).
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
    /// already `.subscribe()`d to this before anything could publish.
    /// PUBLISHER-LESS since PR-C2 (2026-07-14): the lane's
    /// `run_tick_processor` — the only publisher — was deleted with the Dhan
    /// live-WS lane, and Groww persists via its own writer + owns its own
    /// aggregator instance. Kept (with its subscribers) so the seal-writer
    /// install + channel wiring stay publisher-ready; the C3 universe-chain
    /// deletion decides whether the idle consumers go too.
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

    // Positive boot signal (audit-findings Rule 11): the once-per-boot
    // BootHealthCheck ping with the real healthy/total container counts —
    // fires even at 0/0 (honest "nothing healthy") so the operator always
    // learns the boot outcome. RE-HOMED here in PR-C2 (2026-07-13, operator
    // retirement directive per websocket-connection-scope-lock.md
    // "2026-07-13 Amendment"): the emit previously lived in the deleted
    // fast/slow Dhan boot arms; it is PROCESS-shared infra (Docker health,
    // not a Dhan surface), so it survives in the hoisted shared-infra build.
    // Spawned so the `docker compose ps` shell-out never blocks boot.
    {
        let boot_health_notifier = notifier.clone();
        tokio::spawn(async move {
            let (services_healthy, services_total) = infra::container_health_counts().await;
            boot_health_notifier.notify(NotificationEvent::BootHealthCheck {
                services_healthy,
                services_total,
            });
        });
    }

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
    // prefix, before anything could publish — subscribe-before-publish is
    // preserved by construction. PR-C2 (2026-07-14): the broadcast is
    // PUBLISHER-LESS (the lane's `run_tick_processor` is deleted; Groww runs
    // its own writer + aggregator instance), so these consumers idle. The
    // wiring stays: the `spawn_seal_writer_loop` above installed the
    // process-wide `global_seal_sender` that Groww's OWN aggregator drains
    // through (load-bearing), and this Dhan aggregator instance's force-seal
    // tasks are idempotent no-ops on its empty state.
    {
        let obs_rx = tick_broadcast_sender.subscribe();
        let questdb_cfg = config.questdb.clone();
        let obs_health = health_status.clone();
        tokio::spawn(async move {
            run_slow_boot_observability(obs_rx, questdb_cfg, obs_health).await;
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
// Helper: PROCESS run-loop (the single boot path since PR-C2, 2026-07-13 —
// the Dhan live-WS lane + the fast crash-recovery arm are deleted).
//
// Owns the market-close timer, post-market partition-detach, and the shutdown
// wait, then stops the PROCESS API server + flushes otel. The shared infra
// stays up for Groww until the shutdown signal.
// ---------------------------------------------------------------------------
async fn run_process_runloop(
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
        info!("market close reached — post-market housekeeping, API stays alive");
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
            notifier.notify(NotificationEvent::CustomStatus {
                message: "<b>Market closed</b>\nThe live price feed has disconnected for the \
                          day; the system stays running."
                    .to_string(),
            });
        } else {
            info!(
                date = %today_ist,
                "non-trading day — suppressing Post-Market Telegram emission"
            );
        }

        // Drain buffer: let in-flight ticks (last 15:29 candle) settle in the
        // Groww bridge / seal writer before post-market housekeeping. (PR-C2,
        // 2026-07-13: the Dhan lane abort block that followed this sleep is
        // deleted with the lane — the Groww lane self-manages via its own
        // dormancy gates; the order-update WS stays alive until app shutdown.)
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

        info!("post-market: housekeeping complete — scheduled tasks continue");

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

    // (PR-C2, 2026-07-13: the Dhan-lane teardown steps 1–5 are deleted with
    // the lane — the PROCESS infra below is torn down regardless.)

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

/// Bounded backoff between Dhan exchange-lag publisher respawns (silent-feed
/// hardening Item 4). Mirrors [`SLO_PUBLISHER_RESPAWN_BACKOFF_SECS`]: long
/// enough to avoid a hot respawn loop, short enough that the
/// `tv_dhan_exchange_lag_p99_seconds` stream resumes well inside one alarm
/// evaluation period (the lag alarm needs 10 consecutive breaching minutes).
const FEED_LAG_PUBLISHER_RESPAWN_BACKOFF_SECS: u64 = 5;

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
    use tickvault_app::boot_helpers::create_log_file_writer;

    // All pure helper tests moved to boot_helpers.rs in the lib target.
    // Tests below verify main.rs-specific smoke behavior.

    // ── --force-instance-takeover (dual-instance lock hardening 2026-07-04) ──

    // ── build_feed_status_lines (Telegram feed parity, 2026-07-03) ──
    // The readiness + end-of-day messages must list EVERY enabled feed
    // (Dhan, Groww, future) with honest unknowns — never fabricated data.

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

    // PR-C (2026-05-26): historical-fetch source-scan guards removed —
    // the entire spawn_historical_candle_fetch chain is deleted.

    #[test]
    fn test_boot_helper_functions_callable() {
        let _ = compute_market_close_sleep("15:30:00");
        let _ = create_log_file_writer();
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

    /// Step C ratchet (single-path since PR-C2, 2026-07-14): the per-feed
    /// boot dispatcher marker + the `!dhan_enabled` logging branch must
    /// exist, and the run-loop must idle on the shared shutdown signal.
    /// Pre-C2 this pinned "Dhan disabled ⇒ SKIP the Dhan fast/slow boot
    /// block"; with the lane deleted there is no Dhan block to skip — the
    /// pins now guard the surviving Groww-only / no-feed logging dispatcher
    /// and the shutdown idle-await.
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
        // The run-loop idles on the shared shutdown signal — not a warn-only no-op.
        assert!(
            src.contains("wait_for_shutdown_signal().await"),
            "Step C: the boot must idle-await the shutdown signal"
        );
    }

    // RETIRED 2026-07-14 (PR-C2 — Dhan live-WS lane deletion):
    // `test_step_c_gate_precedes_dhan_boot` ordered the dispatcher gate
    // BEFORE the Dhan fast/slow boot decision (`load_token_cache_fast`).
    // That decision was DELETED with the lane, which made the test VACUOUS —
    // its `src.find("load_token_cache_fast")` needle matched only the test's
    // own doc comment (a Rule-11 false-OK), so it is retired rather than
    // left pinning nothing. The surviving ordering contracts are
    // `test_overlay_precedes_boot_skip_guard` (below) and the
    // systemd_boot_notify_guard pinger-before-READY scan.

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
/// the Groww per-minute REST plan), called from the single process-global
/// boot prefix (PR-C2, 2026-07-13: the FAST crash-recovery arm and its
/// second call site are deleted). PROCESS-GLOBAL and deliberately NOT the
/// deleted Dhan-gated `spawn_post_market_tasks` seam: this leg's token is
/// the shared-minter SSM read (never the Dhan JWT), so a Dhan-off
/// (Groww-only) session still runs it. Config-gated fail-safe: an absent
/// `[groww_spot_1m]` section disables it. Supervised respawn wrapper;
/// self-skips on non-trading days / past 15:30 IST (post the one bounded
/// ~15:31 repair sweep).
// TEST-EXEMPT: tokio::spawn wrapper over the unit-tested boot modules; the call site + the config gates pinned by crates/app/tests/groww_spot_1m_wiring_guard.rs + crates/app/tests/groww_chain_1m_wiring_guard.rs.
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
