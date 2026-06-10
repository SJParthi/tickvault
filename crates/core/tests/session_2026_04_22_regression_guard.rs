//! Regression guard for the 2026-04-22 session hotfixes.
//!
//! This test file pins the source-level invariants of the 11 fixes shipped
//! on branch `claude/market-feed-depth-explanation-RynUx` (PR #324). If any
//! of these guards fail, a fix has been silently reverted — DO NOT just
//! delete the test, investigate the change.
//!
//! Each guard cites the commit SHA so future Claude sessions can find the
//! original change quickly via `git show <sha>`.

use std::fs;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or(manifest)
}

fn read_file(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

// `guard_phase2_trigger_minute_is_13_not_12` retired in PR #509d
// (Wave-5 §R.1) — `phase2_scheduler.rs` deleted along with the entire
// dispatcher chain. Trigger-minute invariant is moot.

#[test]
fn guard_order_update_watchdog_is_14400_not_1800() {
    // Commit 55452c2 — bumped watchdog 1800s -> 14400s to stop 30-min flap
    // observed in production at 09:10/09:40/10:10/10:40/11:10/11:41 IST.
    let src = read_file("crates/core/src/websocket/activity_watchdog.rs");
    assert!(
        src.contains("WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS: u64 = 14400"),
        "Order-update watchdog MUST be 14400s. If you see 1800 or 660, the 55452c2 fix was \
         reverted — Telegram will spam every 30 min during idle."
    );
}

#[test]
fn guard_offhours_ws_disconnected_variant_exists() {
    // Commit 996b0cc — split disconnects into in-market (HIGH) vs off-hours (LOW)
    // to stop pre-market 8:32 IST Telegram spam from Dhan idle TCP-RST.
    let src = read_file("crates/core/src/notification/events.rs");
    assert!(
        src.contains("WebSocketDisconnectedOffHours"),
        "WebSocketDisconnectedOffHours variant MUST exist. If gone, the 996b0cc fix was \
         reverted — pre-market Telegram spam returns."
    );
}

// `guard_depth_rebalance_label_uses_zero_disconnect` retired in PR
// #509d — the depth-rebalance Telegram wording moved from inline
// `Custom { message }` strings in main.rs to the typed
// `DepthRebalanced` notification variant per the 2026-04-24 PR #337
// migration. The variant + its `test_depth_rebalance_*` ratchets were
// deleted 2026-06-10 (Phase B batch 2 — zero production emitters since
// the depth feeds were removed in AWS-lifecycle PR #4).
//
// `guard_phase2_empty_plan_fires_phase2_failed` retired in PR #509d
// — `phase2_scheduler.rs` deleted along with the dispatcher chain.

// `guard_preopen_buffer_captures_index_underlyings` retired 2026-05-26 —
// per operator directive, the Dhan pre-market buffer module was deleted
// alongside Dhan historical fetch. `day_open` now derives from the first
// observed live tick LTP via `DayOhlcTracker::update_tick` (auto-arm).

#[test]
fn guard_streaming_heartbeat_task_exists_in_main() {
    // Commit de1784a — once-per-trading-day heartbeat at 09:15:30 IST.
    let src = read_file("crates/app/src/main.rs");
    assert!(
        src.contains("MarketOpenStreamingConfirmation"),
        "Streaming-live heartbeat task MUST be wired in main.rs (commit de1784a)."
    );
    assert!(
        src.contains("09:15:30"),
        "Heartbeat task must reference the 09:15:30 IST trigger time."
    );
}

// `guard_depth_command_initial_subscribe_variants_exist` retired —
// `crates/core/src/websocket/depth_connection.rs` was deleted in PR #4
// (#707) under the LOCKED 2-WS-connections-forever rule (operator-charter §I).
// Depth feeds are forbidden in any future phase without operator re-approval.

// PR #5 (2026-05-19): guard_phase2_complete_includes_depth_counts retired.
// Phase2Complete event removed alongside the Phase 2 dispatcher chain
// under operator-locked 4-IDX_I LOCKED_UNIVERSE.

// `guard_phase2_emits_trigger_latency_histogram` and
// `guard_phase2_emits_preopen_buffer_entries_gauge` retired in PR
// #509d — `phase2_scheduler.rs` deleted along with the dispatcher
// chain; the metrics they pinned no longer exist.

// PR #2 (2026-05-18): `guard_movers_v2_emits_snapshot_duration_histogram`
// and `guard_movers_v2_emits_tracked_total_gauge_per_bucket` retired
// alongside the deleted `crates/core/src/pipeline/top_movers.rs`
// module. Under the 4-IDX_I-only universe, top-N movers are
// meaningless and the tracker types are gone.
