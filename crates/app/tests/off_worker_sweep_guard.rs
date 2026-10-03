//! Source-scan ratchet — sweep S5 (2026-10-03): the cold loops that keep
//! running through the trading session do their disk, child-process and ILP
//! steps through `tickvault_storage::off_worker::off_worker`, never bare on a
//! tokio worker. The runtime has two workers, shared with the frame drain and
//! the socket readers, so one slow `df`, directory walk or HTTP flush on a
//! worker delays the live feed.
//!
//! Each row names a file and a call that must stay wrapped. The match is made
//! on whitespace-collapsed source so a rustfmt reflow does not trip it.

use std::fs;
use std::path::PathBuf;

/// `(file under the workspace root, wrapped call that must be present)`.
const WRAPPED: &[(&str, &str)] = &[
    // Every 60 s: `df` is a child process and can hang on a wedged disk.
    (
        "crates/app/src/disk_pressure_boot.rs",
        "tokio::task::spawn_blocking(move || probe_disk_free_bytes(&probe_dir))",
    ),
    // Attach retry loop (15 s / 60 s) while the main feed is live.
    (
        "crates/app/src/dhan_contract_universe.rs",
        "off_worker(|| read_contract_artifact_on_this_thread(date_ist))",
    ),
    (
        "crates/app/src/dhan_contract_universe.rs",
        "off_worker(|| read_symbol_map_on_this_thread(date_ist))",
    ),
    (
        "crates/app/src/depth20_static.rs",
        "off_worker(|| read_fno_spot_instruments_on_this_thread(date_ist))",
    ),
    (
        "crates/app/src/depth_seed.rs",
        "off_worker(|| read_depth_seed_on_this_thread(path))",
    ),
    // Per minute on the steering task, when the held set grows.
    (
        "crates/app/src/depth_subscription_view.rs",
        "off_worker(|| write_held_today_file(path, day, keys))",
    ),
    // Close of session and the once-a-day probe latch.
    (
        "crates/app/src/depth_rebalance.rs",
        "off_worker(|| { crate::depth_seed::write_depth_seed(&path, &seed)",
    ),
    (
        "crates/app/src/depth_rebalance.rs",
        "crate::depth_unsubscribe_probe::mark_probe_ran_today(&probe_latch, probe_day)",
    ),
    // Hourly log retention loops.
    (
        "crates/app/src/main.rs",
        "observability::sweep_errors_jsonl_retention(dir, RETENTION_HOURS)",
    ),
    (
        "crates/app/src/main.rs",
        "observability::sweep_app_log_retention(dir, RETENTION_HOURS)",
    ),
    // Daily build (also on a mid-session restart).
    (
        "crates/app/src/dhan_universe.rs",
        "off_worker(|| sweep_stale_artifacts(date))",
    ),
    // Post-close ILP flushes.
    (
        "crates/app/src/tf_consistency_boot.rs",
        "off_worker(|| state.writer.flush())",
    ),
    (
        "crates/app/src/ws_connection_rollup.rs",
        "off_worker(|| writer.flush())",
    ),
    (
        "crates/app/src/table_storage_rollup.rs",
        "off_worker(|| writer.flush())",
    ),
    (
        "crates/app/src/feed_scoreboard_boot.rs",
        "off_worker(|| writer.flush())",
    ),
    (
        "crates/app/src/dhan_live_crossverify_boot.rs",
        "off_worker(|| w.flush())",
    ),
    // Storage loops: audit spill drain (60 s per table), OOM probe (60 s),
    // partition export under disk pressure.
    (
        "crates/storage/src/audit_spill.rs",
        "crate::off_worker::off_worker(|| list_spill_files(&dir))",
    ),
    (
        "crates/storage/src/audit_spill.rs",
        "crate::off_worker::off_worker(|| oldest_spill_age_secs(&dir, now_nanos))",
    ),
    (
        "crates/storage/src/oom_monitor.rs",
        "crate::off_worker::off_worker(|| probe_oom_kill_count(&memory_events_path))",
    ),
    (
        "crates/storage/src/partition_archive.rs",
        "crate::off_worker::off_worker(|| encoder.write_all(&chunk))",
    ),
    (
        "crates/storage/src/partition_archive.rs",
        "crate::off_worker::off_worker(|| sender.flush(buffer))",
    ),
    // Per uncached `/board` request.
    (
        "crates/api/src/handlers/board.rs",
        "tickvault_storage::off_worker::off_worker(read_proc_rss_bytes)",
    ),
];

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn workspace_file(rel: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root.join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn every_session_loop_step_runs_off_the_worker() {
    let mut missing = Vec::new();
    for (file, call) in WRAPPED {
        let src = collapse(&workspace_file(file));
        if !src.contains(&collapse(call)) {
            missing.push(format!("{file}: {call}"));
        }
    }
    assert!(
        missing.is_empty(),
        "these blocking steps are no longer wrapped in off_worker / spawn_blocking; \
         a bare call holds a tokio worker the frame drain and the socket readers share:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn the_guard_bites_on_a_bare_call() {
    let bare = collapse("let probe = match probe_disk_free_bytes(&data_dir) {");
    assert!(
        !bare.contains(&collapse(WRAPPED[0].1)),
        "a bare probe must not satisfy the wrapped pattern"
    );
}
