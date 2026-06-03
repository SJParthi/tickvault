//! Phase 2.6 — production-attach ratchet for the TickEnricher.
//!
//! Source-scan tests that pin the boot-time wiring in
//! `crates/app/src/main.rs`. Slow boot MUST:
//!  (a) construct a `TickEnricher`,
//!  (b) call `prev_oi_cache.load_from_questdb(...)` and handle the
//!      Result (graceful degradation on failure),
//!  (c) mark the three L14 boot phases on a `BootOrderingGate`,
//!  (d) pass `Some(Arc::clone(&tick_enricher))` to `run_tick_processor`
//!      (NOT None — that's the legacy unattached state).
//!
//! Drift in any of these surfaces here as a build-fail rather than
//! silent regression to the legacy zero-default lifecycle path.

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

fn read_main() -> String {
    let p = workspace_root().join("app/src/main.rs");
    std::fs::read_to_string(&p).unwrap_or_else(|err| panic!("read {p:?}: {err}"))
}

#[test]
fn slow_boot_constructs_tick_enricher() {
    let src = read_main();
    assert!(
        src.contains("tickvault_core::pipeline::tick_enricher::TickEnricher::new()"),
        "main.rs must construct a TickEnricher in slow boot (Phase 2.6 production attach)"
    );
}

#[test]
fn slow_boot_loads_prev_oi_cache_from_questdb() {
    let src = read_main();
    assert!(
        src.contains(".prev_oi_cache") && src.contains(".load_from_questdb(&config.questdb)"),
        "main.rs must call prev_oi_cache.load_from_questdb in slow boot before subscribe (L14)"
    );
}

#[test]
fn slow_boot_handles_prev_oi_cache_load_error_fail_closed() {
    let src = read_main();
    // Updated 2026-06-02 (Phase 2.9 H1): the prev_oi_cache load is now
    // FAIL-CLOSED, not graceful. The match arm against load_from_questdb's
    // Result must contain both an Ok branch (info log) and an Err branch
    // that logs at ERROR with the typed PREVCLOSE-01 code and leaves the
    // boot-ordering gate in AwaitingOiCache (→ try_authorize_subscribe()
    // false → process::exit(1)). A transient QuestDB failure HALTS the boot
    // so systemd restarts it rather than subscribing with unhealthy state.
    assert!(
        src.contains("prev_oi_cache load FAILED"),
        "main.rs must log the prev_oi_cache load failure at ERROR (fail-closed per L14 / PREVCLOSE-01)"
    );
    assert!(
        src.contains("tv_prev_oi_cache_load_total"),
        "main.rs must emit the tv_prev_oi_cache_load_total counter (outcome label) on both load branches"
    );
    assert!(
        src.contains("prev_oi_cache loaded for tick enricher"),
        "main.rs must log the prev_oi_cache load success at info level"
    );
}

#[test]
fn slow_boot_marks_boot_ordering_gate_phases() {
    let src = read_main();
    for needle in &[
        "BootOrderingGate::new()",
        "mark_oi_cache_loaded()",
        "mark_engines_ready()",
        "mark_replay_completed()",
        "try_authorize_subscribe()",
    ] {
        assert!(
            src.contains(needle),
            "main.rs slow boot must call `{needle}` on the BootOrderingGate (L14)"
        );
    }
}

#[test]
fn slow_boot_passes_some_tick_enricher_to_run_tick_processor() {
    let src = read_main();
    // The slow-boot call site MUST pass `Some(Arc::clone(&tick_enricher))`,
    // not the legacy `None`. We assert the Some-form appears at least
    // once; the fast-boot call site continues to pass None.
    assert!(
        src.contains("Some(std::sync::Arc::clone(&tick_enricher))"),
        "slow boot run_tick_processor call site must pass Some(Arc::clone(&tick_enricher)) (Phase 2.6 production attach)"
    );
}

#[test]
fn fast_boot_still_passes_none_tick_enricher() {
    let src = read_main();
    // Fast boot is a debug/recovery path that doesn't need enrichment.
    // Pinning that it stays None preserves the explicit production-vs-debug
    // split from Phase 2.5 — drift would silently start enriching during
    // recovery boots and could mask real regressions.
    let count_none_for_enricher = src.matches("None, // tick_enricher").count();
    assert!(
        count_none_for_enricher >= 1,
        "fast boot run_tick_processor call site must keep `None, // tick_enricher` for the recovery path"
    );
}

#[test]
fn slow_boot_logs_l14_authorization() {
    let src = read_main();
    assert!(
        src.contains("L14 boot-ordering gate authorized subscribe"),
        "main.rs must emit a positive log line confirming L14 authorization (operator visibility)"
    );
}
