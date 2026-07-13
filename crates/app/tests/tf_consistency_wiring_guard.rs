//! Source-scan ratchet (Z+ L4 PREVENT / L5 AUDIT) pinning that the daily
//! 15:40 IST timeframe-consistency verifier (operator 2026-07-13;
//! TF-VERIFY-01/02) is wired into BOTH main.rs boot paths and that the
//! boot module is a real implementation, not a stub.
//!
//! The dual-spawn topology mirrors `spawn_feed_scoreboard_tasks` (the
//! 2026-07-10 hostile-review CRITICAL): the FAST crash-recovery arm
//! `return run_shutdown_fast(...)`s and never reaches the process-global
//! prefix, so the verifier must be spawned on BOTH arms (a once-per-process
//! AtomicBool inside `spawn_tf_consistency_tasks` makes the dual spawn safe).
//!
//! Mirrors the codebase's `*_wiring_guard` pattern
//! (`tick_conservation_wiring_guard.rs`, `spot_1m_rest_wiring_guard.rs`).
//! Reads SOURCE text, so it runs on the default build independent of any
//! feature flag.

use std::fs;
use std::path::PathBuf;

fn app_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Byte offset of the CODE-form fast-arm return. The literal
/// `return run_shutdown_fast(` ALSO appears inside a doc comment earlier in
/// main.rs (as `return run_shutdown_fast(...)`), so the code form is anchored
/// by the trailing newline — the real call site is a multi-line call whose
/// opening paren ends the line.
fn fast_arm_return_offset(src: &str) -> usize {
    src.find("return run_shutdown_fast(\n")
        .expect("main.rs must contain the FAST-arm `return run_shutdown_fast(` call")
}

#[test]
fn test_spawn_tf_consistency_tasks_is_wired_into_both_main_boot_paths() {
    let src = app_src("src/main.rs");
    // The fn is DEFINED in tf_consistency_boot.rs, so every main.rs mention
    // is a call site. Exactly two: fast arm + process-global prefix.
    let call_count = src.matches("spawn_tf_consistency_tasks(").count();
    assert_eq!(
        call_count, 2,
        "main.rs must call spawn_tf_consistency_tasks(...) EXACTLY twice \
         (fast crash-recovery arm + process-global prefix — the scoreboard \
         dual-spawn precedent); found {call_count}."
    );

    // Ordering: one call BEFORE the fast arm's `return run_shutdown_fast(`
    // (else a mid-market crash-restart session never runs the 15:40 check)
    // and one AFTER it (the process-global prefix for normal boots).
    let fast_return = fast_arm_return_offset(&src);
    let first = src
        .find("spawn_tf_consistency_tasks(")
        .expect("first call site must exist");
    let last = src
        .rfind("spawn_tf_consistency_tasks(")
        .expect("second call site must exist");
    assert!(
        first < fast_return,
        "one spawn_tf_consistency_tasks call must precede the FAST-arm \
         `return run_shutdown_fast(` (byte {fast_return}), else a crash-restart \
         boot never spawns the verifier; first call at byte {first}."
    );
    assert!(
        last > fast_return,
        "one spawn_tf_consistency_tasks call must follow the FAST-arm return \
         (the process-global prefix site); last call at byte {last}."
    );
}

#[test]
fn test_spawn_tf_consistency_tasks_threads_config_calendar_notifier() {
    let src = app_src("src/main.rs");
    // Both call sites must thread (&config, &trading_calendar, &notifier) —
    // scan a bounded window after each call for the three argument tokens.
    let mut from = 0;
    let mut checked = 0;
    while let Some(rel) = src[from..].find("spawn_tf_consistency_tasks(") {
        let abs = from + rel;
        let window = &src[abs..(abs + 400).min(src.len())];
        for needle in ["&config", "&trading_calendar", "&notifier"] {
            assert!(
                window.contains(needle),
                "spawn_tf_consistency_tasks call at byte {abs} must pass \
                 {needle} within its argument window."
            );
        }
        checked += 1;
        from = abs + 1;
    }
    assert_eq!(checked, 2, "expected exactly 2 call sites to check");
}

#[test]
fn test_tf_consistency_boot_module_is_not_a_stub() {
    // Rule-14 skeleton-PR defense: the boot module must contain the REAL
    // verification machinery, not inert pub fns. Each needle pins one leg of
    // the design (discovery → 1m baseline → 19-way TF UNION → recompute →
    // compare → audit rows → budget → schedule → env overrides).
    let src = app_src("src/tf_consistency_boot.rs");
    for needle in [
        "FROM candles_1m",           // Query A — the 1m baseline
        "UNION ALL",                 // Query B — the 19-way TF union
        "append_finding",            // audit rows reach the storage writer
        "LIMIT",                     // explicit row caps (truncation tripwire)
        "TICKVAULT_TF_VERIFY_NOW",   // env force-run override
        "TICKVAULT_TF_VERIFY_DATE",  // env backfill-date override
        "decide_tf_verify_start",    // pure scheduling decision fn
        "TF_VERIFY_RUN_BUDGET_SECS", // wall-clock run budget
        "previous_trading_day",      // the Groww D-1 walk-back
        "flush",                     // final audit flush
        "discard",                   // poisoned-buffer discard defense
        "bucket_grid",               // the independent grid reimplementation
        "TfVerify01MismatchFound",   // TF-VERIFY-01 emit site
        "TfVerify02RunDegraded",     // TF-VERIFY-02 emit site
        "TfConsistencySummary",      // the one-per-run Telegram summary
        "TfConsistencyAborted",      // the supervisor abort page
    ] {
        assert!(
            src.contains(needle),
            "tf_consistency_boot.rs must contain `{needle}` — a missing \
             needle means the verifier lost that leg of the design (or was \
             hollowed into a stub)."
        );
    }
}

#[test]
fn test_tf_consistency_boot_uses_once_per_process_guard() {
    // The dual spawn (fast arm + prefix) is only safe because the module
    // self-guards with a once-per-process AtomicBool — a lost guard would
    // double-run the verifier on a crash-restart boot.
    let src = app_src("src/tf_consistency_boot.rs");
    assert!(
        src.contains("AtomicBool"),
        "spawn_tf_consistency_tasks must keep its once-per-process AtomicBool \
         guard (the dual-spawn safety)."
    );
    assert!(
        src.contains("tf_consistency.enabled"),
        "spawn_tf_consistency_tasks must gate on [tf_consistency] enabled."
    );
}
