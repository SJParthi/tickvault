//! Source-scan ratchet (Z+ L5 AUDIT / L4 PREVENT) pinning that the
//! `InstrumentBuildFailed` boot-failure alert is wired at BOTH
//! `load_instruments(...)` sites in `main.rs` (fast-boot + standard-boot),
//! and that neither site has regressed to a silent bare `?`.
//!
//! Background: `InstrumentBuildSuccess` is emitted on the `Ok` arm of both
//! boot paths, but the failure side used a bare `?` — so an instrument-load
//! `Err` exited `main()` with no operator Telegram. This guard fails the
//! build if a future refactor reverts either site to the silent `?`.
//!
//! Mirrors the codebase's `*_is_wired` guard pattern
//! (e.g. `daily_universe_boot_wiring_guard.rs`). Reads `main.rs` SOURCE text,
//! so it runs on the default build independent of any feature flag.

use std::fs;
use std::path::PathBuf;

fn main_rs_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_instrument_build_failed_is_wired_into_main() {
    let src = main_rs_source();
    assert!(
        src.contains("NotificationEvent::InstrumentBuildFailed"),
        "main.rs must emit NotificationEvent::InstrumentBuildFailed on the \
         instrument-load failure path — otherwise a boot-time load Err exits \
         main() with no operator Telegram (the asymmetry this slice fixes)."
    );
}

#[test]
fn test_instrument_build_failed_emitted_at_both_boot_paths() {
    let src = main_rs_source();
    let emit_sites = src
        .matches("NotificationEvent::InstrumentBuildFailed")
        .count();
    assert!(
        emit_sites >= 2,
        "InstrumentBuildFailed must be emitted at BOTH load_instruments sites \
         (fast-boot + standard-boot); found {emit_sites}. A single site means \
         one boot path still fails silently."
    );
}

#[test]
fn test_no_silent_bare_question_mark_on_load_instruments() {
    let src = main_rs_source();
    // The pre-fix silent form. If this string reappears, a site has regressed
    // to a bare `?` that exits boot with no operator alert.
    assert!(
        !src.contains("trading_calendar.as_ref()).await?;"),
        "A load_instruments(...) call regressed to a bare `.await?;` — wrap it \
         in a match that emits InstrumentBuildFailed before returning Err \
         (fail-closed boot-halt preserved, but the operator is alerted)."
    );
}

#[test]
fn test_instrument_build_failed_preserves_fail_closed_halt() {
    let src = main_rs_source();
    // The failure arm must still return the error (boot halts) — the added
    // alert must NOT swallow the Err and continue booting on a broken universe.
    assert!(
        src.contains("return Err(err);"),
        "The InstrumentBuildFailed arm must `return Err(err)` — the alert is \
         additive; the fail-closed boot-halt must be preserved."
    );
}

#[test]
fn test_instrument_build_failed_reason_is_secret_redacted() {
    // Security (review 2026-06-12): the anyhow chain MUST be routed through
    // `capture_rest_error_body` (ratchet-guaranteed URL-param + JWT +
    // credential-field redaction) before it reaches `reason` (Telegram) and
    // the `error!` log sinks — authentication.md rule 2: a token must never
    // appear in error messages. Both boot arms must emit `reason: safe_reason`.
    let src = main_rs_source();
    assert!(
        src.contains("capture_rest_error_body(&err.to_string())"),
        "InstrumentBuildFailed `reason` must be redacted via \
         capture_rest_error_body(&err.to_string()) before reaching Telegram/logs."
    );
    let redacted_emits = src.matches("reason: safe_reason").count();
    assert!(
        redacted_emits >= 2,
        "Both boot arms must emit `reason: safe_reason` (redacted); found \
         {redacted_emits}. A raw `err.to_string()` in reason can leak a token."
    );
}
