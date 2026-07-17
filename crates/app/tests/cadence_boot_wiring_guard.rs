//! Source-scan ratchet (Z+ L4 PREVENT / L5 AUDIT) pinning that the
//! judge-locked cadence scheduler (2026-07-14; CADENCE-01/02/03) is wired
//! into the main.rs boot path and that the boot module keeps its
//! config gate + once-per-process guard + the REAL broker executors
//! (2026-07-17 — the dry-run day-1 scaffolding is superseded).
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

/// The PRODUCTION region of a source file: everything above the first
/// column-0 `#[cfg(test)]` line (the house production-region split — the
/// margin_gate_off_guard / seal-writer ratchet precedent). A test-module
/// mention of a pinned needle can never satisfy or double-count a
/// production pin.
fn production_region(src: &str) -> &str {
    match src.find("\n#[cfg(test)]") {
        Some(at) => &src[..at],
        None => src,
    }
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
        // `&notifier` = the R6 (2026-07-16) typed Telegram sink for the
        // expiry cross-broker disagreement page.
        for needle in ["&config", "&trading_calendar", "&feed_runtime", "&notifier"] {
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
fn test_cadence_boot_module_gate_guard_and_real_executors() {
    let src = app_src("src/cadence_boot.rs");
    for needle in [
        // Config gate (disabled boot = byte-identical to today).
        "config.cadence.enabled",
        // Once-per-process guard (the dual-spawn safety).
        "AtomicBool",
        "CADENCE_SPAWNED",
        // REAL broker executors on BOTH lanes (2026-07-17) — built
        // BEFORE the once-guard so a client-build failure leaves the
        // other boot path able to retry; failures are HTTP-CLIENT-01
        // loud, never a Client::new() panic fallback.
        "DhanCadenceExecutor::new(",
        "GrowwCadenceExecutor::new(",
        // The executors are the SOLE table authors under RS3 — the
        // ensure-DDL duty for their three tables lives HERE now.
        "ensure_spot_1m_rest_table",
        "ensure_option_chain_1m_table",
        "ensure_rest_fetch_audit_table",
        // Level-triggered lane gates: the SAME atomics the API toggle
        // flips (dhan_flag/groww_flag).
        "feed_runtime.dhan_flag()",
        "feed_runtime.groww_flag()",
        // The supervised runner spawn.
        "spawn_supervised_cadence_runner",
        // Real executors = real coded degrade levels (F10 semantics).
        "dry_run: false",
    ] {
        assert!(
            src.contains(needle),
            "cadence_boot.rs must contain `{needle}` — a missing needle \
             means the boot wiring lost that leg of the locked design."
        );
    }
    // The dry-run logging executor must be GONE from the boot wiring —
    // its presence would silently revert a lane to no-REST logging.
    assert!(
        !src.contains("DryRunLoggingExecutor"),
        "cadence_boot.rs must no longer reference DryRunLoggingExecutor \
         (real broker executors both lanes since 2026-07-17)."
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
    //     path, after the ShutdownInitiated notification. Scan the
    //     PRODUCTION region only (split at the first column-0
    //     `#[cfg(test)]`) so a test-module mention can never satisfy or
    //     double-count this pin (2026-07-16, verifier round-4 item 2).
    let whole = app_src("src/main.rs");
    let src = production_region(&whole);
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

/// One section's `enabled = ...` line from base.toml (the FIRST
/// `enabled` key after the section header — every scanned section leads
/// with it).
fn section_enabled_line(toml: &str, section: &str) -> String {
    // Anchor on the REAL section-header LINE — a prose comment can mention
    // the section name earlier in the file (e.g. base.toml's [cadence]
    // module comment names the four legacy leg sections), and a raw
    // `find(section)` would land there and miss the enabled key.
    let mut lines = toml.lines();
    lines
        .by_ref()
        .find(|l| l.trim_start().starts_with(section))
        .unwrap_or_else(|| panic!("config/base.toml must carry the {section} section"));
    lines
        .take(12)
        .find(|l| l.trim_start().starts_with("enabled ="))
        .unwrap_or_else(|| panic!("{section} must lead with an enabled key"))
        .to_string()
}

#[test]
fn test_cadence_base_toml_enabled_and_legacy_legs_stood_down() {
    // 2026-07-17: the cadence scheduler ships ENABLED with the REAL
    // broker executors, and the RS3 mutual exclusion (config.rs) demands
    // the four legacy per-minute legs stand down in the SAME config —
    // cadence-on + any-leg-on is a boot-refusing double demand.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/base.toml");
    let toml = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read base.toml: {e}"));
    assert!(
        section_enabled_line(&toml, "[cadence]").contains("enabled = true"),
        "[cadence] must ship enabled = true (real executors, 2026-07-17)."
    );
    for legacy in [
        "[spot_1m_rest]",
        "[option_chain_1m]",
        "[groww_spot_1m]",
        "[groww_option_chain_1m]",
        "[groww_contract_1m]",
    ] {
        assert!(
            section_enabled_line(&toml, legacy).contains("enabled = false"),
            "{legacy} must ship enabled = false (stood down under the RS3 \
             cadence mutual exclusion, 2026-07-17)."
        );
    }
}
