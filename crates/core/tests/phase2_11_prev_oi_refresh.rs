//! Phase 2.11 — hostile bug-hunt M2 + M4 fixes: periodic prev_oi_cache
//! refresh task wired into slow-boot main.rs.
//!
//! Closes two scenarios that boot-time load + midnight rollover do NOT
//! cover:
//!
//! - **M2** — boot at 23:50 IST window: boot-time load reads "today's"
//!   `prev_day_ohlcv` row briefly before midnight; minutes later that row
//!   becomes "yesterday's" without an immediate reload. The midnight
//!   task fires at 00:00 IST and reloads, but the cache was correct
//!   anyway in this case.
//!
//! - **M4** — fresh deploy with empty `prev_day_ohlcv`: boot-time load
//!   returns `Ok(count=0)`, cache stays empty until the next IST
//!   midnight. OI Change panels read 0% for the entire first day.
//!   Periodic refresh closes that gap once the morning historical pull
//!   populates `prev_day_ohlcv`.
//!
//! - **M4** — `prev_day_ohlcv` still being written post-boot (the daily
//!   historical pull runs as a cold-path task): refresh covers the gap
//!   if the table catches up after boot.
//!
//! **Source note 2026-06-02:** the OI source moved from the tick-sealed
//! `candles_1d` matview to the historical-only `prev_day_ohlcv` table
//! (1d historical-only directive). These scenarios are unchanged in shape.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    let me = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    me.parent()
        .expect("tickvault root")
        .parent()
        .expect("workspace root")
        .to_path_buf()
        .join("crates")
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|err| panic!("read {p:?}: {err}"))
}

#[test]
fn phase2_11_spawn_helper_exists_in_tick_enricher() {
    let src = read("core/src/pipeline/tick_enricher.rs");
    assert!(
        src.contains("pub fn spawn_prev_oi_cache_refresh_task"),
        "tick_enricher.rs must export pub fn spawn_prev_oi_cache_refresh_task"
    );
}

#[test]
fn phase2_11_refresh_interval_constant_is_pinned() {
    let src = read("core/src/pipeline/tick_enricher.rs");
    assert!(
        src.contains("pub const PREV_OI_CACHE_REFRESH_INTERVAL_SECS: u64 = 300"),
        "PREV_OI_CACHE_REFRESH_INTERVAL_SECS must be exactly 300 (5 min)"
    );
}

#[test]
fn phase2_11_main_rs_spawns_refresh_task_in_slow_boot() {
    let src = read("app/src/main.rs");
    assert!(
        src.contains("spawn_prev_oi_cache_refresh_task"),
        "main.rs slow boot must call spawn_prev_oi_cache_refresh_task — \
         without it, fresh-deploy OI Change panels read 0% for the entire \
         first day (M2/M4 fix)"
    );
}

#[test]
fn phase2_11_refresh_task_runs_alongside_midnight_rollover() {
    let src = read("app/src/main.rs");
    // Both spawn calls must coexist — they cover complementary scenarios.
    assert!(
        src.contains("spawn_midnight_rollover_task"),
        "midnight rollover task must still be spawned (Phase 2.7)"
    );
    assert!(
        src.contains("spawn_prev_oi_cache_refresh_task"),
        "refresh task must also be spawned (Phase 2.11)"
    );
}

#[test]
fn phase2_11_refresh_task_self_exits_on_population() {
    let src = read("core/src/pipeline/tick_enricher.rs");
    // The refresh task should `return` from the loop on Ok with non-zero
    // count — the midnight task takes over from there.
    assert!(
        src.contains("populated") && src.contains("midnight rollover task takes over"),
        "refresh task must self-exit once cache is populated, with a log line \
         noting that the midnight task takes over (no double-handling)"
    );
}

#[test]
fn phase2_11_emits_three_outcome_labels_for_observability() {
    let src = read("core/src/pipeline/tick_enricher.rs");
    // The refresh task emits tv_prev_oi_cache_refresh_total with three
    // distinct outcome labels so dashboards can distinguish:
    //   - "still_empty" — prev_day_ohlcv not yet populated, will retry
    //   - "populated"   — task succeeded, exiting
    //   - "err"         — load failed, will retry
    //   - "external"    — cache populated by another path (midnight)
    assert!(src.contains("\"outcome\" => \"still_empty\""));
    assert!(src.contains("\"outcome\" => \"populated\""));
    assert!(src.contains("\"outcome\" => \"err\""));
    assert!(src.contains("\"outcome\" => \"external\""));
}
