//! Source-scan ratchet (Z+ L4 PREVENT / L5 AUDIT) pinning that the
//! judge-locked cadence scheduler (2026-07-14; CADENCE-01/02/03) is wired
//! into BOTH main.rs boot paths and that the boot module keeps its
//! config gate + once-per-process guard + dry-run executors.
//!
//! The dual-spawn topology mirrors `spawn_tf_consistency_tasks` (the
//! 2026-07-10 hostile-review CRITICAL): the FAST crash-recovery arm
//! `return run_shutdown_fast(...)`s and never reaches the process-global
//! prefix, so the scheduler must be spawned on BOTH arms (the
//! once-per-process AtomicBool inside `spawn_cadence_scheduler` makes the
//! dual spawn safe).
//!
//! Mirrors the codebase's `*_wiring_guard` pattern
//! (`tf_consistency_wiring_guard.rs`, `spot_1m_rest_wiring_guard.rs`).
//! Reads SOURCE text, so it runs on the default build independent of any
//! feature flag.

use std::fs;
use std::path::PathBuf;

fn app_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Byte offset of the CODE-form fast-arm return (see
/// `tf_consistency_wiring_guard.rs` for the doc-comment anchor rationale).
fn fast_arm_return_offset(src: &str) -> usize {
    src.find("return run_shutdown_fast(\n")
        .expect("main.rs must contain the FAST-arm `return run_shutdown_fast(` call")
}

#[test]
fn test_spawn_cadence_scheduler_is_wired_into_both_main_boot_paths() {
    let src = app_src("src/main.rs");
    // The fn is DEFINED in cadence_boot.rs, so every main.rs mention is a
    // call site. Exactly two: fast arm + process-global prefix.
    let call_count = src.matches("spawn_cadence_scheduler(").count();
    assert_eq!(
        call_count, 2,
        "main.rs must call spawn_cadence_scheduler(...) EXACTLY twice \
         (fast crash-recovery arm + process-global prefix — the \
         tf_consistency dual-spawn precedent); found {call_count}."
    );

    let fast_return = fast_arm_return_offset(&src);
    let first = src
        .find("spawn_cadence_scheduler(")
        .expect("first call site must exist");
    let last = src
        .rfind("spawn_cadence_scheduler(")
        .expect("second call site must exist");
    assert!(
        first < fast_return,
        "one spawn_cadence_scheduler call must precede the FAST-arm \
         `return run_shutdown_fast(` (byte {fast_return}), else a \
         crash-restart boot never spawns the scheduler; first call at \
         byte {first}."
    );
    assert!(
        last > fast_return,
        "one spawn_cadence_scheduler call must follow the FAST-arm return \
         (the process-global prefix site); last call at byte {last}."
    );
}

#[test]
fn test_spawn_cadence_scheduler_threads_config_calendar_feed_runtime() {
    let src = app_src("src/main.rs");
    let mut from = 0;
    let mut checked = 0;
    while let Some(rel) = src[from..].find("spawn_cadence_scheduler(") {
        let abs = from + rel;
        let window = &src[abs..(abs + 400).min(src.len())];
        for needle in ["&config", "&trading_calendar", "&feed_runtime"] {
            assert!(
                window.contains(needle),
                "spawn_cadence_scheduler call at byte {abs} must pass \
                 {needle} within its argument window."
            );
        }
        checked += 1;
        from = abs + 1;
    }
    assert_eq!(checked, 2, "expected exactly 2 call sites to check");
}

#[test]
fn test_cadence_boot_module_gate_guard_and_dry_run_executors() {
    let src = app_src("src/cadence_boot.rs");
    for needle in [
        // Config gate (disabled boot = byte-identical to today).
        "config.cadence.enabled",
        // Once-per-process guard (the dual-spawn safety).
        "AtomicBool",
        "CADENCE_SPAWNED",
        // Day-1 dry-run executors on BOTH lanes — NO REST caller in this
        // PR (the real broker executors land in a LATER PR with the dated
        // rule-file re-authorization).
        "DryRunLoggingExecutor",
        // Level-triggered lane gates: the SAME atomics the API toggle
        // flips (dhan_flag/groww_flag).
        "feed_runtime.dhan_flag()",
        "feed_runtime.groww_flag()",
        // The supervised runner spawn.
        "spawn_supervised_cadence_runner",
    ] {
        assert!(
            src.contains(needle),
            "cadence_boot.rs must contain `{needle}` — a missing needle \
             means the boot wiring lost that leg of the locked design."
        );
    }
    // Both lanes must be the dry-run executor (exactly two constructor
    // mentions — one per lane).
    let dry_run_count = src.matches("Arc::new(DryRunLoggingExecutor)").count();
    assert_eq!(
        dry_run_count, 2,
        "BOTH lanes must run DryRunLoggingExecutor day 1 (found \
         {dry_run_count} constructor sites)."
    );
}

#[test]
fn test_cadence_graceful_shutdown_chain_is_wired() {
    // TRH-3 (hostile-review round 1, 2026-07-15): the F2 fix exists
    // precisely because the pre-fix spawn returned the Notify into a
    // never-notified `_cadence_shutdown` binding — graceful teardown
    // never reached the runner. Deleting the single production notify
    // call (or the parked-handle set) silently reverts to that exact
    // defect, so BOTH legs of the chain are pinned here:
    //
    // (a) cadence_boot.rs parks the handle process-globally and exposes
    //     the notifier.
    let boot = app_src("src/cadence_boot.rs");
    for needle in ["CADENCE_SHUTDOWN.set(", "pub fn notify_cadence_shutdown()"] {
        assert!(
            boot.contains(needle),
            "cadence_boot.rs must contain `{needle}` — the F2 parked \
             shutdown handle / notifier leg is missing."
        );
    }
    // (b) main.rs fires the notifier from run_process_runloop's teardown
    //     path, after the ShutdownInitiated notification.
    let src = app_src("src/main.rs");
    let call = "cadence_boot::notify_cadence_shutdown();";
    let call_count = src.matches(call).count();
    assert_eq!(
        call_count, 1,
        "main.rs must call `{call}` exactly once (the run_process_runloop \
         teardown site); found {call_count}."
    );
    let runloop_at = src
        .find("async fn run_process_runloop(")
        .expect("main.rs must define run_process_runloop");
    let shutdown_initiated_at = src[runloop_at..]
        .find("NotificationEvent::ShutdownInitiated")
        .map(|rel| runloop_at + rel)
        .expect("run_process_runloop must emit ShutdownInitiated");
    let call_at = src.find(call).expect("notify call site must exist");
    assert!(
        call_at > shutdown_initiated_at,
        "notify_cadence_shutdown() (byte {call_at}) must sit in the \
         teardown path AFTER the ShutdownInitiated notification (byte \
         {shutdown_initiated_at}) inside run_process_runloop."
    );
}

#[test]
fn test_cadence_base_toml_ships_disabled() {
    // The [cadence] section must exist AND ship enabled = false — a
    // default-ON flip needs a fresh dated operator quote in the rule file
    // first (`.claude/rules/project/cadence-error-codes.md`).
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/base.toml");
    let toml = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read base.toml: {e}"));
    let section_at = toml
        .find("[cadence]")
        .expect("config/base.toml must carry the [cadence] section");
    let window = &toml[section_at..(section_at + 400).min(toml.len())];
    assert!(
        window.contains("enabled = false"),
        "[cadence] must ship enabled = false (dry-run scaffolding only; \
         the enable flip is a later, operator-quoted PR)."
    );
}
