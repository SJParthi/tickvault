//! Source-scan ratchet (Z+ L5 AUDIT / L4 PREVENT) pinning that the
//! daily-universe boot orchestrator is wired into `main.rs` Step-6c,
//! fail-closed, and feature-gated. Mirrors the codebase's `*_is_wired`
//! guard pattern (e.g. `secret_manager.rs::test_orphan_position_watchdog_is_wired_into_main`).
//!
//! These tests read the `main.rs` SOURCE text, so they run on the default
//! build (feature OFF) — the ratchet is always active in CI, independent
//! of the `daily_universe_fetcher` feature.

use std::fs;
use std::path::PathBuf;

fn main_rs_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_daily_universe_boot_is_wired_into_main() {
    let src = main_rs_source();
    assert!(
        src.contains("run_daily_universe_boot("),
        "Step-6c must call run_daily_universe_boot — the daily-universe fetch \
         is otherwise dead code (pub-fn-wiring + go-live regression)."
    );
}

#[test]
fn test_step_6c_ensures_three_lifecycle_tables() {
    let src = main_rs_source();
    for ensure_fn in [
        "ensure_instrument_lifecycle_table",
        "ensure_instrument_lifecycle_audit_table",
        "ensure_instrument_fetch_audit_table",
    ] {
        assert!(
            src.contains(ensure_fn),
            "Step-6c must call {ensure_fn} before run_daily_universe_boot — the \
             downstream writers assume the table exists."
        );
    }
}

#[test]
fn test_step_6c_is_feature_gated_and_fail_closed() {
    let src = main_rs_source();
    assert!(
        src.contains("#[cfg(feature = \"daily_universe_fetcher\")]"),
        "Step-6c must stay feature-gated (default-OFF until the go-live flip)."
    );
    // Fail-closed §4: the orchestrator result is `?`-propagated with a
    // halting context (NOT swallowed / fail-open). Pinning the halt context
    // string keeps a future edit from silently downgrading to fail-open.
    assert!(
        src.contains("daily-universe boot failed (fail-closed") && src.contains("halting"),
        "Step-6c must be fail-closed per §4 — propagate the orchestrator Err \
         with a halting context, never log-and-continue."
    );
}
