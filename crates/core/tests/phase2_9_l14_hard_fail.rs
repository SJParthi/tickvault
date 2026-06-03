//! Phase 2.9 — H1 fix: BootOrderingGate authorization failure now
//! HARD-FAILS the boot via `process::exit(1)` instead of logging
//! ERROR + continuing. The H4 fix from Phase 2.8 already arranged
//! for `mark_oi_cache_loaded` to NOT fire on a `prev_oi_cache.load_from_questdb`
//! error; this PR turns that signal into a binary-level L14
//! enforcement: the process refuses to subscribe with unhealthy state.
//!
//! systemd / docker restart policy handles the recovery loop. The
//! `TICKVAULT_BOOT_DRY_RUN` env var lets tests exercise the branch
//! without killing the runner.

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
fn h1_l14_refusal_calls_process_exit() {
    let src = read_main();
    assert!(
        src.contains("std::process::exit(1)"),
        "main.rs must call std::process::exit(1) when boot_ordering_gate.try_authorize_subscribe \
         returns false (Phase 2.9 H1 fix — binary-level L14 enforcement)"
    );
}

#[test]
fn h1_l14_refusal_logs_typed_error_code() {
    let src = read_main();
    // PR #5 (2026-05-19) retagged the L14 refusal from
    // `Phase2Ready01PreflightFailed` (retired with the Phase 2 dispatcher)
    // to `PrevClose01IlpFailed`, which matches the "PREVCLOSE-01" message
    // reference per the error_code_tag_guard meta-test invariant.
    assert!(
        src.contains("ErrorCode::PrevClose01IlpFailed"),
        "main.rs must tag the L14 refusal with a typed ErrorCode (Phase 2.9 H1 fix)"
    );
}

#[test]
fn h1_l14_refusal_emits_counter() {
    let src = read_main();
    assert!(
        src.contains("tv_l14_boot_authorization_refused_total"),
        "main.rs must emit tv_l14_boot_authorization_refused_total when L14 refused"
    );
    assert!(
        src.contains("tv_l14_boot_authorization_total"),
        "main.rs must emit tv_l14_boot_authorization_total when L14 authorized (positive signal)"
    );
}

#[test]
fn h1_dry_run_env_var_gate_lets_tests_exercise_path() {
    let src = read_main();
    assert!(
        src.contains("TICKVAULT_BOOT_DRY_RUN"),
        "main.rs must gate the process::exit on TICKVAULT_BOOT_DRY_RUN env var \
         so tests can exercise the path without killing the runner"
    );
}

#[test]
fn h1_log_level_is_error_not_warn() {
    let src = read_main();
    // The block immediately preceding the process::exit must be a
    // tracing::error!(...) — Loki routes ERROR to Telegram. WARN would
    // not page the operator.
    let needle = "L14 boot-ordering gate refused to authorize subscribe";
    let idx = src.find(needle).expect("L14 refusal log line must exist");
    let preceding = &src[..idx];
    let last_error_idx = preceding.rfind("tracing::error!").unwrap_or(0);
    let last_warn_idx = preceding.rfind("tracing::warn!").unwrap_or(0);
    assert!(
        last_error_idx > last_warn_idx,
        "the L14 refusal log line must use tracing::error! (not warn!) so Loki → Telegram fires"
    );
}
