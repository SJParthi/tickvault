//! Mid-session fresh-boot regression guard (2026-04-24).
//!
//! Locks in the invariants that keep a mid-session fresh clone + `make run`
//! behaviour clean. Before this guard existed, the 12:07 IST mid-session
//! boot on 2026-04-24 surfaced:
//!
//! 1. `market-open heartbeat: skipping (past 09:15:30 — late start)` at INFO
//!    → confusingly pager-like for operators who saw the logs during normal
//!    mid-session boots. Now DEBUG — the real streaming confirmation comes
//!    from the boot-time spot-wait + ATM-selection path, not this task.
//! 2. `depth-anchor: skipping (past 09:13:00 — late start)` at INFO → same
//!    story. Real dispatch happens via `run_depth_init_sync` at boot time.
//! 3. `tick_gap_tracker` backlog-tick state corruption → 988 false-positive
//!    gap ERRORs in 15 min right after Phase 2 dispatch. Fixed at
//!    `crates/trading/src/risk/tick_gap_tracker.rs`.
//! 4. `run_depth_rebalancer` must retain the market-hours gate (regression
//!    guard against accidental removal).
//!
//! This file is a source-scan guard — it does NOT run the app. It checks
//! that specific string literals and control-flow invariants survive
//! future edits. If any assertion here fires, read the comment above the
//! `assert!` call: it points at the exact production incident that
//! motivated the check.

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/app has parent")
        .parent()
        .expect("crates has parent")
        .to_path_buf()
}

fn read_file(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("failed to read {}: {e}", path.display());
    })
}

#[test]
fn test_market_open_heartbeat_skip_is_debug_not_info() {
    let src = read_file("crates/app/src/main.rs");
    // Pre-fix: `info!(... "market-open heartbeat: skipping (past 09:15:30 — late start)"`
    // Post-fix: `debug!(... "market-open heartbeat: skipping (past 09:15:30 — expected on mid-session boot)"`
    assert!(
        !src.contains("info!(\n                            now = %now_time,\n                            \"market-open heartbeat: skipping (past 09:15:30"),
        "2026-04-24 regression: market-open heartbeat skip log re-promoted to INFO. Should be DEBUG on mid-session boot."
    );
    assert!(
        src.contains(
            "market-open heartbeat: skipping (past 09:15:30 — expected on mid-session boot)"
        ),
        "2026-04-24 regression: market-open heartbeat skip message missing or rewritten. Expected 'expected on mid-session boot' wording so operators don't treat it as a bug."
    );
}

// `test_depth_anchor_skip_is_debug_not_info` retired with the legacy
// 09:13 anchor task in the v2-only refactor (PR <TBD>). The pinned
// literal "depth-anchor: skipping (past 09:13:00..." lived inside
// the deleted anchor task body; under v2 the anchor is structurally
// absent so the skip log no longer exists.

// `test_depth_rebalancer_market_hours_gate_present` RETIRED with the
// PR #4 (2026-05-19) deletion of depth_rebalancer.rs per operator lock
// 2026-05-15 (websocket-connection-scope-lock.md).

#[test]
fn test_tick_gap_tracker_has_backlog_before_state_mutation() {
    let src = read_file("crates/trading/src/risk/tick_gap_tracker.rs");
    // The backlog-tick check MUST appear BEFORE `states.entry(...).or_insert(...)`.
    // If someone moves the `or_insert` back above the backlog filter, the
    // 2026-04-24 988-false-positive bug returns.
    let or_insert_idx = src
        .find("self.states.entry(security_id).or_insert(")
        .expect("expected or_insert call in tick_gap_tracker");
    let backlog_idx = src
        .find("let is_backlog_tick = tick_age_secs > BACKLOG_TICK_AGE_THRESHOLD_SECS")
        .expect("expected backlog_tick detection in tick_gap_tracker");
    assert!(
        backlog_idx < or_insert_idx,
        "2026-04-24 regression: backlog-tick check moved after or_insert. \
         Must run BEFORE any state mutation or the false-positive gap-ERROR bug returns."
    );
}

#[test]
fn test_tick_gap_error_threshold_is_raised_to_300s() {
    let src = read_file("crates/common/src/constants.rs");
    // Exact token for the raised threshold.
    assert!(
        src.contains("pub const TICK_GAP_ERROR_THRESHOLD_SECS: u32 = 300"),
        "2026-04-24 regression: TICK_GAP_ERROR_THRESHOLD_SECS reverted below 300s. \
         120s was too aggressive for illiquid F&O and produced 988 false ERRORs in 15min. \
         Real disconnects are caught by WS ping/pong within 40s + the feed-level stall watchdogs."
    );
}

// `test_depth_200_has_initial_stagger_constant` RETIRED with the
// PR #4 (2026-05-19) deletion of depth_connection.rs per operator lock
// 2026-05-15 (websocket-connection-scope-lock.md).

#[test]
fn test_instrument_build_success_event_is_emitted_on_both_boot_paths() {
    // 2026-04-24 audit finding #6: InstrumentBuildSuccess was defined and
    // unit-tested but NEVER emitted in production. Only the FAILURE path
    // (InstrumentBuildFailed) fired, so operators had no positive Telegram
    // signal that the daily instrument rebuild succeeded.
    //
    // This guard ensures InstrumentBuildSuccess is fired from BOTH the
    // fast-boot and slow-boot load_instruments call sites.
    let src = read_file("crates/app/src/main.rs");
    let emissions = src
        .matches("NotificationEvent::InstrumentBuildSuccess")
        .count();
    assert!(
        emissions >= 2,
        "2026-04-24 regression: InstrumentBuildSuccess must be emitted from \
         BOTH boot paths (fast-boot + slow-boot). Found only {emissions} emission(s). \
         Without both, one boot path regresses to silent-success behaviour."
    );
    // The source tag must be one of the two expected values — not a blank
    // or generic string.
    assert!(
        src.contains("\"fresh_csv_build\""),
        "2026-04-24 regression: source tag for FreshBuild must be \
         'fresh_csv_build' (distinguishes from cache-hit path)."
    );
    assert!(
        src.contains("\"rkyv_cache\""),
        "2026-04-24 regression: source tag for CachedPlan must be \
         'rkyv_cache' (distinguishes from fresh-csv path)."
    );
}

// PR #4 (2026-05-19): legacy `test_depth_200_main_rs_increments_spawn_counter`
// and the v2 depth-200 spawn counter test are both retired alongside the
// deleted 20/200-level depth WebSocket pipelines. Under the 4-IDX_I
// LOCKED_UNIVERSE only 1 main-feed + 1 order-update conn run forever
// (.claude/rules/project/websocket-connection-scope-lock.md).

#[test]
fn test_per_instrument_stall_poller_is_wired() {
    // 2026-04-24 audit finding #2: TickGapTracker::detect_stale_instruments()
    // existed in the tracker but was NEVER called in production. Per-instrument
    // stall (Dhan silently drops a subscription OR an ATM strike stops trading
    // mid-session) stayed invisible until the (since-retired 2026-07-14) global no-tick watchdog fired
    // on TOTAL silence. This guard ensures the 30s periodic poller stays
    // wired in run_slow_boot_observability.
    let src = read_file("crates/app/src/main.rs");
    assert!(
        src.contains("tick_gap_tracker.detect_stale_instruments()"),
        "2026-04-24 regression: detect_stale_instruments() call missing from \
         main.rs. Per-instrument stall detection reverts to the 120s global \
         watchdog — catastrophic for mid-session individual-underlying stalls."
    );
    let constants = read_file("crates/common/src/constants.rs");
    assert!(
        constants.contains("pub const STALE_LTP_SCAN_INTERVAL_SECS: u64 = 30;"),
        "2026-04-24 regression: STALE_LTP_SCAN_INTERVAL_SECS must stay at 30 \
         in common/constants.rs. Longer cadence delays stall detection; \
         shorter wastes CPU on the O(n) scan."
    );
    assert!(
        src.contains("STALE_LTP_SCAN_INTERVAL_SECS"),
        "2026-04-24 regression: main.rs must reference the named constant \
         STALE_LTP_SCAN_INTERVAL_SECS, not a hardcoded Duration literal \
         (banned-pattern category 3)."
    );
    assert!(
        src.contains("last_stale_check.elapsed() >= stale_check_interval"),
        "2026-04-24 regression: stale-check cadence gate missing. Without it, \
         detect_stale_instruments() would run on every tick (O(n) per tick = \
         O(n^2) per session) or not at all."
    );
}

#[test]
fn test_market_open_streaming_routes_to_failed_when_main_feed_is_zero() {
    // 2026-04-24 audit finding #8: when main_feed_active == 0 at the
    // 09:15:30 heartbeat, the event MUST route to MarketOpenStreamingFailed
    // (Severity::High), NOT to MarketOpenStreamingConfirmation
    // (Severity::Info). Without this branch, a catastrophic "no connections
    // at market open" scenario shows up as "Streaming live / Main feed: 0/5"
    // in Telegram — Info severity, wakes nobody up.
    let src = read_file("crates/app/src/main.rs");
    assert!(
        src.contains("NotificationEvent::MarketOpenStreamingFailed"),
        "2026-04-24 regression: MarketOpenStreamingFailed routing missing. \
         When main_feed_active == 0 at 09:15:30 IST, operator must page via \
         the High-severity Failed variant, not the Info-severity Confirmation."
    );
    // Branch predicate must be `main_active == 0` — if someone relaxes it
    // to `main_active < 5` or similar, the normal degraded-but-still-streaming
    // case flips to High-severity noise.
    assert!(
        src.contains("if main_active == 0 {"),
        "2026-04-24 regression: MarketOpenStreamingFailed gate must be \
         `main_active == 0` exactly — relaxing to `< 5` etc. produces \
         false pages for degraded-but-streaming pool states."
    );
}

// PR-C (2026-05-26): 2 source-scan guards for the deleted
// `spawn_historical_candle_fetch` routing tree are retired.
//
// PR-D (2026-05-26): HistoricalFetchAlreadyAvailable variant guard
// retired — the variant itself is deleted alongside candle_fetcher.
