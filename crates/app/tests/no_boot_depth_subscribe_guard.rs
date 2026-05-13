//! Plan item I (2026-04-23, follow-up to PR #324) — source-scan guard
//! for the "no boot-time depth subscribe" invariant.
//!
//! This test ratchets the architectural change shipped by items B.1,
//! B.2 and real-C on branch `claude/continue-work-se9hG`:
//!
//! * **B.1** (`crates/core/src/websocket/depth_connection.rs`) —
//!   `run_twenty_depth_connection` no longer short-circuits on empty
//!   instruments; `run_two_hundred_depth_connection` takes
//!   `security_id: Option<u32>`.
//! * **B.2** (`crates/app/src/main.rs`) — boot no longer `continue`s
//!   when `instruments_for_underlying.is_empty()`; 200-level no
//!   longer ERRORs + skips when `atm_ce`/`atm_pe` is `None`.
//! * **real-C** (`crates/app/src/main.rs`) — the 09:13 IST dispatcher
//!   sends `InitialSubscribe20` / `InitialSubscribe200` via
//!   `depth_cmd_senders`.
//!
//! Source-scanning is the right tool because the invariants live in
//! `main.rs` (binary entry point) and `depth_connection.rs` (a long
//! file with async lifetime plumbing). Behavioural assertions would
//! need the full boot sequence (Docker, secrets, calendar) or a
//! test harness that mocks mpsc channels + tokio clock; a mechanical
//! scan is deterministic, cheap and immune to flake.
//!
//! If any assertion here fails, the PR that caused it is regressing
//! the "depth connections stay idle until 09:13" invariant — fix the
//! source change, not this test.

use std::path::PathBuf;

fn read_source(relative_path: &str) -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // `crates/app` — walk up to `crates/` to reach sibling crates.
    path.pop(); // -> crates/
    path.push(relative_path);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read {}: {err}", path.display()))
}

fn read_main_rs() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("src/main.rs");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read {}: {err}", path.display()))
}

// -----------------------------------------------------------------------
// B.1 ratchets — depth_connection.rs
// -----------------------------------------------------------------------

#[test]
fn run_twenty_depth_does_not_early_exit_on_empty_instruments() {
    let src = read_source("core/src/websocket/depth_connection.rs");

    // Negative ratchet — the legacy early-exit shape must NOT appear.
    let legacy_pattern = "if instruments.is_empty() {\n        info!(\"20-level depth: no instruments to subscribe — skipping\");\n        return Ok(());\n    }";
    assert!(
        !src.contains(legacy_pattern),
        "Item B.1 regression: `run_twenty_depth_connection` re-added the \
         old early-exit on empty instruments. This defeats the 09:13 \
         dispatcher — connections never stay idle waiting for \
         InitialSubscribe20. See commit 0611a60."
    );

    // Positive ratchet — the DEFERRED-mode log line MUST be present.
    assert!(
        src.contains("starting 20-level depth in DEFERRED mode") || src.contains("DEFERRED mode"),
        "Item B.1 regression: `run_twenty_depth_connection` is missing \
         the DEFERRED-mode log. When boot passes an empty Vec, the \
         connection must announce it is waiting for InitialSubscribe20."
    );
}

#[test]
fn run_two_hundred_depth_takes_optional_security_id() {
    let src = read_source("core/src/websocket/depth_connection.rs");

    // Positive ratchet — the parameter MUST be Option<u32>.
    assert!(
        src.contains("security_id: Option<u32>"),
        "Item B.1 regression: `run_two_hundred_depth_connection` lost \
         its `security_id: Option<u32>` parameter. Without `None` as \
         the deferred sentinel, boot cannot spawn 200-level connections \
         before the 09:13 dispatcher has picked an ATM."
    );

    // Negative ratchet — the outer function signature MUST NOT regress
    // to the old `security_id: u32,` form.
    assert!(
        !src.contains("pub async fn run_two_hundred_depth_connection(\n    token_handle: TokenHandle,\n    client_id: String,\n    exchange_segment: ExchangeSegment,\n    security_id: u32,"),
        "Item B.1 regression: `run_two_hundred_depth_connection` \
         regressed to the old `security_id: u32` signature. Must be \
         `security_id: Option<u32>`."
    );
}

// -----------------------------------------------------------------------
// Phase 0 Item 7 (operator-locked 2026-05-13) — scope-gate ratchets
//
// The deferred-mode ratchets above pin the "depth conns stay idle until
// the 09:13 dispatcher fires Initial Subscribe" invariant for legacy
// scopes. Phase 0 LEAN MVP goes one step further: depth-20 + depth-200
// pipelines are PARKED ENTIRELY under `IndicesUnderlyingsOnly` scope.
// PR-3 wired the scope gate via `should_spawn_depth_dynamic_pipeline`.
// These ratchets pin that main.rs ACTUALLY CALLS the helper at the
// depth-spawn site (Rule 13 — defined-but-never-called is a bug).
// -----------------------------------------------------------------------

#[test]
fn main_rs_consults_should_spawn_depth_dynamic_pipeline_helper() {
    let src = read_main_rs();

    // Positive ratchet — main.rs MUST call the scope helper before the
    // depth pipeline_v2 spawn block.
    let helper_call = "should_spawn_depth_dynamic_pipeline";
    assert!(
        src.contains(helper_call),
        "Phase 0 Item 7 regression: main.rs no longer consults \
         `{helper_call}` before spawning the depth-20 + depth-200 \
         pipelines. Under `SubscriptionScope::IndicesUnderlyingsOnly` \
         the depth pipelines MUST stay parked. See PR-3 + \
         phase2_recovery.rs::should_spawn_depth_dynamic_pipeline.",
    );

    // Positive ratchet — the legacy raw-flag read MUST NOT be the gate.
    // (The flag may still appear as an ARG to the helper; what we
    // forbid is `let pipeline_v2_active = config.features.depth_dynamic_pipeline_v2;`
    // as the standalone gate condition.)
    let legacy_gate = "let pipeline_v2_active = config.features.depth_dynamic_pipeline_v2;";
    assert!(
        !src.contains(legacy_gate),
        "Phase 0 Item 7 regression: main.rs reverted to the legacy \
         direct flag read `{legacy_gate}` — this bypasses the Phase 0 \
         scope gate. Use `should_spawn_depth_dynamic_pipeline(scope, \
         flag)` so `IndicesUnderlyingsOnly` parks depth pipelines \
         regardless of flag state.",
    );
}

#[test]
fn main_rs_consults_should_spawn_greeks_pipeline_helper() {
    let src = read_main_rs();

    // Positive ratchet — main.rs MUST call the scope helper before the
    // greeks pipeline spawn.
    let helper_call = "should_spawn_greeks_pipeline";
    assert!(
        src.contains(helper_call),
        "Phase 0 Item 7 regression (companion): main.rs no longer \
         consults `{helper_call}` before spawning the greeks pipeline. \
         Under Phase 0 the greeks pipeline MUST stay parked (operator's \
         strategy uses underlying spot ticks only).",
    );

    // Negative ratchet — the legacy raw `if config.greeks.enabled` gate
    // MUST NOT be the sole condition.
    let legacy_gate = "if config.greeks.enabled {";
    assert!(
        !src.contains(legacy_gate),
        "Phase 0 Item 7 regression: main.rs reverted to the legacy \
         direct flag read `{legacy_gate}` — this bypasses the Phase 0 \
         scope gate. Use `should_spawn_greeks_pipeline(scope, \
         greeks_enabled)`.",
    );
}

// -----------------------------------------------------------------------
// B.2 ratchets — main.rs boot code
// -----------------------------------------------------------------------
