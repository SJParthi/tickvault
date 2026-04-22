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

#[test]
fn guard_phase2_trigger_minute_is_13_not_12() {
    // Commit 0340a7c — moved trigger from 09:12 to 09:13 so the 09:12 close
    // bucket is fully captured before we read the preopen buffer.
    let src = read_file("crates/core/src/instrument/phase2_scheduler.rs");
    assert!(
        src.contains("PHASE2_TRIGGER_MIN: u32 = 13"),
        "Phase 2 trigger minute MUST be 13. If you see 12, the 0340a7c fix was reverted — \
         the 09:12 close bucket will be empty when read at 09:12:00."
    );
}

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

#[test]
fn guard_depth_rebalance_label_uses_zero_disconnect() {
    // Commit 6f6edc5 — replaced scary "aborting old 200-level → spawning new ATM"
    // wording with the accurate "zero-disconnect swap" label. The actual code
    // path is still Swap20/Swap200 — only the operator-facing text changed.
    let main_src = read_file("crates/app/src/main.rs");
    assert!(
        !main_src.contains("aborting old 200-level"),
        "Depth-rebalance Telegram MUST NOT contain the misleading 'aborting old 200-level' \
         text — it describes a disconnect-and-respawn, but the code does Swap20/Swap200 \
         (zero-disconnect). The 6f6edc5 fix was reverted."
    );
    assert!(
        main_src.contains("zero-disconnect swap"),
        "Depth-rebalance Telegram MUST say 'zero-disconnect swap' (commit 6f6edc5)."
    );
}

#[test]
fn guard_phase2_empty_plan_fires_phase2_failed() {
    // Commit 4aaa0fb — when the plan is empty, scheduler MUST fire
    // Phase2Failed with diagnostic, NOT Phase2Complete { added_count: 0 }.
    let src = read_file("crates/core/src/instrument/phase2_scheduler.rs");
    assert!(
        src.contains("Empty plan at trigger"),
        "Phase 2 empty-plan diagnostic message MUST exist (commit 4aaa0fb). \
         If gone, Phase 2 silently lies that it succeeded with 0 instruments."
    );
    assert!(
        src.contains("buffer_entries"),
        "Phase 2 diagnostic must include 'buffer_entries' field for root-cause triage."
    );
}

#[test]
fn guard_preopen_buffer_captures_index_underlyings() {
    // Commit f641315 — preopen buffer now records NIFTY (id=13) + BANKNIFTY
    // (id=25) on IDX_I segment for depth ATM selection.
    let src = read_file("crates/core/src/instrument/preopen_price_buffer.rs");
    assert!(
        src.contains("PREOPEN_INDEX_UNDERLYINGS"),
        "PREOPEN_INDEX_UNDERLYINGS const MUST exist (commit f641315)."
    );
    assert!(
        src.contains("(\"NIFTY\", 13)"),
        "PREOPEN_INDEX_UNDERLYINGS must include NIFTY=13."
    );
    assert!(
        src.contains("(\"BANKNIFTY\", 25)"),
        "PREOPEN_INDEX_UNDERLYINGS must include BANKNIFTY=25."
    );
}

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

#[test]
fn guard_depth_anchor_task_exists_in_main() {
    // Commit 427bf2d — once-per-trading-day depth anchor at 09:13:00 IST.
    let src = read_file("crates/app/src/main.rs");
    assert!(
        src.contains("MarketOpenDepthAnchor"),
        "Depth-anchor task MUST be wired in main.rs (commit 427bf2d)."
    );
}

#[test]
fn guard_depth_command_initial_subscribe_variants_exist() {
    // Commit 6fd9c2a — DepthCommand::InitialSubscribe20 / InitialSubscribe200
    // variants exist for the future unified 09:13 dispatch (Items B+C).
    let src = read_file("crates/core/src/websocket/depth_connection.rs");
    assert!(
        src.contains("InitialSubscribe20"),
        "DepthCommand::InitialSubscribe20 MUST exist (commit 6fd9c2a)."
    );
    assert!(
        src.contains("InitialSubscribe200"),
        "DepthCommand::InitialSubscribe200 MUST exist (commit 6fd9c2a)."
    );
}

#[test]
fn guard_phase2_complete_includes_depth_counts() {
    // Commit 6fd9c2a — Phase2Complete event extended with depth_20_underlyings
    // + depth_200_contracts fields for the unified dispatch flow.
    let src = read_file("crates/core/src/notification/events.rs");
    assert!(
        src.contains("depth_20_underlyings"),
        "Phase2Complete must include depth_20_underlyings field (commit 6fd9c2a)."
    );
    assert!(
        src.contains("depth_200_contracts"),
        "Phase2Complete must include depth_200_contracts field (commit 6fd9c2a)."
    );
}

#[test]
fn guard_phase2_emits_trigger_latency_histogram() {
    // Plan item K (2026-04-22) — Phase 2 emits tv_phase2_trigger_latency_ms
    // so the operator can see how late the RunImmediate crash-recovery path
    // woke up vs the 09:13 target. Zero for SleepUntil, minutes_late * 60_000
    // for RunImmediate.
    let src = read_file("crates/core/src/instrument/phase2_scheduler.rs");
    assert!(
        src.contains("tv_phase2_trigger_latency_ms"),
        "Phase 2 must emit tv_phase2_trigger_latency_ms (plan item K)."
    );
}

#[test]
fn guard_phase2_emits_preopen_buffer_entries_gauge() {
    // Plan item K (2026-04-22) — Phase 2 emits tv_phase2_preopen_buffer_entries
    // as a gauge at trigger time. Low value = upstream tick capture problem;
    // the phase2-empty-plan runbook references this metric.
    let src = read_file("crates/core/src/instrument/phase2_scheduler.rs");
    assert!(
        src.contains("tv_phase2_preopen_buffer_entries"),
        "Phase 2 must emit tv_phase2_preopen_buffer_entries gauge (plan item K)."
    );
}
