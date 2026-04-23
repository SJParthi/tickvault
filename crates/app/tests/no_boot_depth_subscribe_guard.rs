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

#[test]
fn boot_does_not_skip_spawn_when_instruments_empty() {
    let src = read_main_rs();

    // Negative ratchet — the legacy early-`continue` shape must NOT
    // appear. Old behaviour was:
    //
    //     if instruments_for_underlying.is_empty() {
    //         info!(..., "20-level depth: no instruments selected — skipping");
    //         continue;
    //     }
    //
    // New behaviour logs "spawning DEFERRED connection" and falls
    // through to spawn.
    assert!(
        !src.contains(
            "20-level depth: no instruments selected — skipping\"\n                    );\n                    continue;"
        ),
        "Item B.2 regression: main.rs re-added the `continue` that \
         skipped depth-connection spawn when no boot-time ATM was \
         available. This means pre-market deploys will NOT start \
         depth sockets until next restart. Must fall through to \
         spawn with an empty instrument list (DEFERRED mode)."
    );

    // Positive ratchet — the new DEFERRED log line MUST be present.
    assert!(
        src.contains("spawning DEFERRED connection"),
        "Item B.2 regression: main.rs is missing the 'spawning \
         DEFERRED connection' log. Boot must announce it is spawning \
         an idle connection that will be activated by the 09:13 \
         dispatcher."
    );
}

#[test]
fn boot_does_not_error_on_missing_atm_ce_pe() {
    let src = read_main_rs();

    // Negative ratchet — the legacy ERROR + `continue` on missing
    // atm_ce/atm_pe must NOT appear. Old behaviour:
    //
    //     let Some((opt_label, depth200_sid, depth200_label)) = opt_entry else {
    //         error!(
    //             underlying,
    //             "200-level depth: no ATM CE/PE found — skipping (check subscription planner)"
    //         );
    //         continue;
    //     };
    //
    // New behaviour synthesises a "{UL}-{CE|PE}-deferred" label and
    // passes `None` as the security_id so the 09:13 dispatcher can
    // InitialSubscribe200 it later.
    assert!(
        !src.contains("no ATM CE/PE found — skipping (check subscription planner)"),
        "Item B.2 regression: main.rs re-added the ERROR-level skip \
         when boot-time ATM CE/PE is missing. Pre-market missing ATM \
         is EXPECTED, not a planner bug — must use deferred-mode \
         placeholder sid=None + '{{UL}}-{{CE|PE}}-deferred' label so \
         the 09:13 dispatcher can complete the subscribe."
    );

    // Positive ratchet — the deferred-label pattern MUST appear.
    assert!(
        src.contains("-CE-deferred") || src.contains("-PE-deferred"),
        "Item B.2 regression: main.rs is missing the deferred-label \
         synthesis for 200-level. When atm_ce/atm_pe is None, boot \
         must build a '{{UL}}-CE-deferred' / '{{UL}}-PE-deferred' \
         label so the depth connection + cmd_senders map keys \
         remain consistent."
    );
}

// -----------------------------------------------------------------------
// real-C ratchets — 09:13 dispatcher
// -----------------------------------------------------------------------

#[test]
fn dispatcher_at_0913_sends_initial_subscribe20() {
    let src = read_main_rs();

    assert!(
        src.contains("DepthCommand::InitialSubscribe20"),
        "Item real-C regression: main.rs no longer references \
         `DepthCommand::InitialSubscribe20`. The 09:13 dispatcher MUST \
         send this command to each deferred 20-level depth connection, \
         otherwise they stay idle forever."
    );
}

#[test]
fn dispatcher_at_0913_sends_initial_subscribe200() {
    let src = read_main_rs();

    assert!(
        src.contains("DepthCommand::InitialSubscribe200"),
        "Item real-C regression: main.rs no longer references \
         `DepthCommand::InitialSubscribe200`. The 09:13 dispatcher MUST \
         send this command to the NIFTY + BANKNIFTY CE/PE deferred \
         200-level connections, otherwise those sockets stay idle \
         forever and `MarketOpenDepthAnchor` becomes a lie."
    );
}

#[test]
fn dispatcher_at_0913_calls_select_depth_instruments() {
    let src = read_main_rs();

    // The dispatcher pre-commit path was a Telegram-only task. After
    // real-C it must call the canonical ATM selector to build the
    // 20-level subscribe list.
    assert!(
        src.contains("select_depth_instruments"),
        "Item real-C regression: main.rs no longer calls \
         `select_depth_instruments` at 09:13. Without it the dispatcher \
         cannot compute ATM ± 24 for 20-level InitialSubscribe."
    );
}

#[test]
fn dispatcher_at_0913_emits_dispatch_counter() {
    let src = read_main_rs();

    // Grafana + alerting rely on this counter to verify the dispatch
    // actually fired today. Removing the counter would make the
    // dispatcher silent in ops.
    assert!(
        src.contains("tv_depth_initial_subscribe_dispatched_total"),
        "Item real-C regression: main.rs no longer emits the \
         `tv_depth_initial_subscribe_dispatched_total` counter from \
         the 09:13 dispatcher. Without it ops has no way to confirm \
         that InitialSubscribe actually fired today."
    );
}

#[test]
fn dispatcher_uses_preopen_buffer_and_shared_spot_prices() {
    let src = read_main_rs();

    // Dual spot-price source — 09:12 close for NIFTY + BANKNIFTY,
    // live LTP for FINNIFTY + MIDCPNIFTY.
    assert!(
        src.contains("anchor_buffer") && src.contains("anchor_shared_spot_prices"),
        "Item real-C regression: the 09:13 dispatcher must read BOTH \
         the preopen_buffer (`anchor_buffer`) and SharedSpotPrices \
         (`anchor_shared_spot_prices`). NIFTY/BANKNIFTY get 09:12 \
         close from the buffer; FINNIFTY/MIDCPNIFTY get live LTP \
         from SharedSpotPrices."
    );
}
