//! Source-scan ratchet (Z+ L4 PREVENT / L5 AUDIT) pinning that the
//! judge-locked cadence scheduler (2026-07-14; CADENCE-01/02/03) is wired
//! into the main.rs boot path and that the boot module keeps its
//! config gate + once-per-process guard + dry-run executors.
//!
//! PR-C2 re-shape (2026-07-13, operator retirement directive —
//! websocket-connection-scope-lock.md "2026-07-13 Amendment", adopted at
//! the 2026-07-16 rebase onto post-#1540 main): the FAST crash-recovery
//! arm (and its `return run_shutdown_fast(` exit) DIED with the Dhan
//! live-WS lane, so the tf_consistency-precedent dual-spawn shape
//! collapses to the single process-global prefix call. The
//! dual-spawn-safe once-guard inside `spawn_cadence_scheduler` is
//! unchanged (defense-in-depth, exactly like the tf_consistency guard).
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

#[test]
fn test_spawn_cadence_scheduler_is_wired_into_main_boot_path() {
    let src = app_src("src/main.rs");
    // The fn is DEFINED in cadence_boot.rs, so every main.rs mention is a
    // call site. Exactly one on the single boot path (PR-C2 — the FAST
    // crash-recovery arm is deleted; the tf_consistency guard precedent).
    let call_count = src.matches("spawn_cadence_scheduler(").count();
    assert_eq!(
        call_count, 1,
        "main.rs must call spawn_cadence_scheduler(...) EXACTLY once \
         (the single process-global boot prefix since PR-C2); found \
         {call_count} — zero kills the scheduler; two would rely on the \
         once-guard instead of the boot shape."
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
    // PR-C2 (2026-07-13): single call site on the single boot path.
    assert_eq!(checked, 1, "expected exactly 1 call site to check");
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
