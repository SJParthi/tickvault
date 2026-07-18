//! Wave-5 PR #509 — static-universe cleanup ratchet guards.
//!
//! Asserts that dead modules and helpers retired under the indices-only
//! scope (per `.claude/plans/active-plan-in-memory-store-aws-instance.md`
//! §R.1) do NOT come back. Each test is a single-purpose source-tree
//! scan — fast, no compile, no runtime state.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").exists())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            manifest_dir
                .parent()
                .and_then(|p| p.parent())
                .map(PathBuf::from)
                .unwrap_or(manifest_dir)
        })
}

#[test]
fn test_live_tick_atm_resolver_file_does_not_exist() {
    let path = workspace_root().join("crates/core/src/instrument/live_tick_atm_resolver.rs");
    assert!(
        !path.exists(),
        "live_tick_atm_resolver.rs MUST be deleted under indices-only scope (PR #509). \
         Found at {}. Stock F&O ATM resolution is dead — see plan §R.1.",
        path.display()
    );
}

#[test]
fn test_no_pub_mod_live_tick_atm_resolver_in_instrument_mod() {
    let path = workspace_root().join("crates/core/src/instrument/mod.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        !src.contains("pub mod live_tick_atm_resolver"),
        "instrument/mod.rs MUST NOT declare `pub mod live_tick_atm_resolver` \
         (file was deleted in PR #509)."
    );
}

#[test]
fn test_preopen_rest_fallback_file_does_not_exist() {
    let path = workspace_root().join("crates/core/src/instrument/preopen_rest_fallback.rs");
    assert!(
        !path.exists(),
        "preopen_rest_fallback.rs MUST be deleted under indices-only scope (PR #509). \
         Found at {}. Stock F&O REST fallback is dead — see plan §R.1.",
        path.display()
    );
}

#[test]
fn test_no_pub_mod_preopen_rest_fallback_in_instrument_mod() {
    let path = workspace_root().join("crates/core/src/instrument/mod.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        !src.contains("pub mod preopen_rest_fallback"),
        "instrument/mod.rs MUST NOT declare `pub mod preopen_rest_fallback` \
         (file was deleted in PR #509)."
    );
}

#[test]
fn test_boot_mode_file_does_not_exist() {
    let path = workspace_root().join("crates/core/src/instrument/boot_mode.rs");
    assert!(
        !path.exists(),
        "boot_mode.rs MUST be deleted under indices-only scope (PR #509). \
         Found at {}. The 4-mode resolver (PreMarket/MidPreMarket/MidMarket/PostMarket) \
         is dead — see plan §R.1 row 4. Market-hours gating now lives in \
         the g1_exchange_gate_accepts / market-hours constants in common \
         (the tick_processor persist-window helpers died with the tick \
         chain, stage-2 dead-WS sweep 2026-07-17).",
        path.display()
    );
}

#[test]
fn test_no_pub_mod_boot_mode_in_instrument_mod() {
    let path = workspace_root().join("crates/core/src/instrument/mod.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        !src.contains("pub mod boot_mode"),
        "instrument/mod.rs MUST NOT declare `pub mod boot_mode` (deleted in PR #509)."
    );
}

#[test]
fn test_mid_market_boot_complete_event_retired() {
    let path = workspace_root().join("crates/core/src/notification/events.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    // Only the retirement-comment marker should remain; the variant body,
    // match arms, and emit sites must all be gone.
    let occurrences = src.matches("MidMarketBootComplete").count();
    assert_eq!(
        occurrences, 1,
        "MidMarketBootComplete should appear exactly once (the retirement-comment marker). \
         Found {occurrences} occurrences — variant body/match arms/tests likely re-introduced."
    );
}

#[test]
fn test_phase2_scheduler_file_does_not_exist() {
    let path = workspace_root().join("crates/core/src/instrument/phase2_scheduler.rs");
    assert!(
        !path.exists(),
        "phase2_scheduler.rs MUST be deleted (PR #509d). The 09:13 IST stock-F&O \
         dispatcher is retired under indices-only scope. See plan §R.1 row 5."
    );
}

#[test]
fn test_phase2_delta_file_does_not_exist() {
    let path = workspace_root().join("crates/core/src/instrument/phase2_delta.rs");
    assert!(
        !path.exists(),
        "phase2_delta.rs MUST be deleted (PR #509d). Stock-F&O ATM-delta \
         computation is dead under indices-only scope."
    );
}

#[test]
fn test_phase2_emit_guard_file_does_not_exist() {
    let path = workspace_root().join("crates/core/src/instrument/phase2_emit_guard.rs");
    assert!(
        !path.exists(),
        "phase2_emit_guard.rs MUST be deleted (PR #509d). The Phase 2 \
         emit-guard belonged to the retired dispatcher chain."
    );
}

#[test]
fn test_no_pub_mod_phase2_dispatcher_in_instrument_mod() {
    let path = workspace_root().join("crates/core/src/instrument/mod.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        !src.contains("pub mod phase2_scheduler"),
        "instrument/mod.rs MUST NOT declare `pub mod phase2_scheduler` (deleted in PR #509d)."
    );
    assert!(
        !src.contains("pub mod phase2_delta"),
        "instrument/mod.rs MUST NOT declare `pub mod phase2_delta` (deleted in PR #509d)."
    );
    assert!(
        !src.contains("pub mod phase2_emit_guard"),
        "instrument/mod.rs MUST NOT declare `pub mod phase2_emit_guard` (deleted in PR #509d)."
    );
}

#[test]
fn test_phase2_readiness_check_file_does_not_exist() {
    let path = workspace_root().join("crates/core/src/instrument/phase2_readiness_check.rs");
    assert!(
        !path.exists(),
        "phase2_readiness_check.rs MUST be deleted (PR #5, AWS-lifecycle 2026-05-19). \
         The 09:13:01 IST pre-flight readiness check is retired under the 4-IDX_I LOCKED_UNIVERSE: \
         Phase 2 stock-F&O dispatcher is dead weight (operator lock 2026-05-15)."
    );
}

#[test]
fn test_run_phase2_scheduler_not_called_in_main() {
    let path = workspace_root().join("crates/app/src/main.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        !src.contains("run_phase2_scheduler("),
        "main.rs MUST NOT call `run_phase2_scheduler(` — dispatcher retired in PR #509d."
    );
}
