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
// B.2 ratchets — main.rs boot code
// -----------------------------------------------------------------------
