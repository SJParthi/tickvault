//! D2a — `start_dhan_lane` extraction guard (source-scan ratchet).
//!
//! D2a lifts the now-contiguous Dhan-lane body (auth → daily-universe build →
//! main-feed WS pool → tick processor → order-update → token-renewal) VERBATIM
//! out of the inline `if config.feeds.dhan_enabled { … }` gate in `main()` into
//! a callable `async fn start_dhan_lane(ctx: DhanLaneContext<'_>) ->
//! Result<DhanLaneRunHandles, StartLaneError>`. The gate now BUILDS a
//! `DhanLaneContext` and CALLS `start_dhan_lane(lane_ctx)`; the Dhan-OFF branch
//! is still `None`. Behaviour for Dhan-ON is IDENTICAL.
//!
//! This SOURCE-SCAN ratchet (same pattern as the other boot-isolation guards)
//! reads the literal `crates/app/src/main.rs` and fails the build if a future
//! edit removes the extracted fn, inlines the body back, drops the
//! `dhan_enabled` gate, or weakens the boot-abort → `main()`-return mapping that
//! keeps the extraction behaviour-identical.

/// Read `crates/app/src/main.rs` regardless of the test's working directory.
fn read_main_rs() -> String {
    std::fs::read_to_string("src/main.rs")
        .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
        .expect("main.rs must be readable from the app crate test working dir")
}

/// The extracted fn + its support types must exist.
#[test]
fn start_dhan_lane_fn_and_context_types_exist() {
    let src = read_main_rs();
    assert!(
        src.contains("async fn start_dhan_lane("),
        "D2a: the Dhan lane MUST be extracted into a callable \
         `async fn start_dhan_lane(...)`."
    );
    assert!(
        src.contains("struct DhanLaneContext<'a>"),
        "D2a: `start_dhan_lane` MUST take a `DhanLaneContext` of the PROCESS-shared \
         state it reads from `main()`."
    );
    assert!(
        src.contains("enum StartLaneError"),
        "D2a: `start_dhan_lane` MUST return a typed `StartLaneError` so the caller \
         can reproduce `main()`'s exact prior boot-abort returns."
    );
    assert!(
        src.contains("-> std::result::Result<DhanLaneRunHandles, StartLaneError>"),
        "D2a: `start_dhan_lane` MUST return \
         `Result<DhanLaneRunHandles, StartLaneError>`."
    );
}

/// The Dhan lane is STILL gated by `dhan_enabled`, and the gate's value is built
/// by CALLING the extracted fn (never inlined back).
#[test]
fn dhan_lane_is_start_dhan_lane_extracted_and_still_gated_by_dhan_enabled() {
    let src = read_main_rs();

    // (1) the lane gate still exists and produces an Option<DhanLaneRunHandles>.
    let gate = src
        .find("let dhan_lane: Option<DhanLaneRunHandles> = if config.feeds.dhan_enabled")
        .expect(
            "D2a: the Dhan lane MUST stay wrapped in \
             `let dhan_lane: Option<DhanLaneRunHandles> = if config.feeds.dhan_enabled {…}`",
        );

    // (2) the gate CALLS the extracted fn — the body is NOT inlined back.
    let call = src
        .find("match start_dhan_lane(lane_ctx).await")
        .expect("D2a: the gate MUST call the extracted `start_dhan_lane(lane_ctx)`");
    assert!(
        gate < call,
        "D2a: the `start_dhan_lane(lane_ctx)` call MUST live INSIDE the \
         `if config.feeds.dhan_enabled` gate — so a Dhan-OFF boot never starts it."
    );

    // (3) the OFF branch is still a plain `None` (no lane work). The OFF arm is
    // the `else { … "DHAN LANE SKIPPED…" … None };` directly after the gate's
    // `start_dhan_lane` call; scan from the gate to the `};` that closes the
    // `let dhan_lane = …` binding.
    let bind_end = src[gate..]
        .find("\n    };")
        .map(|rel| gate + rel)
        .expect("D2a: the `let dhan_lane = …;` binding MUST close with `};`");
    let gate_block = &src[gate..bind_end];
    assert!(
        gate_block.contains("DHAN LANE SKIPPED") && gate_block.contains("None"),
        "D2a: the Dhan-OFF branch MUST stay `None` (skip the lane) — no Dhan lane \
         when disabled."
    );
}

/// The boot-abort → `main()`-return mapping that makes the extraction
/// behaviour-identical must be present: `BootAbortClean` → `return Ok(())`,
/// `BootAbortErr(err)` → `return Err(err)`.
#[test]
fn boot_abort_outcomes_map_back_to_main_returns() {
    let src = read_main_rs();
    assert!(
        src.contains("Err(StartLaneError::BootAbortClean) => return Ok(())"),
        "D2a: a clean boot-abort (the inline gate's `return Ok(())`) MUST map back \
         to `main()` `return Ok(())` — same clean process exit."
    );
    assert!(
        src.contains("Err(StartLaneError::BootAbortErr(err)) => return Err(err)"),
        "D2a: an error boot-abort (the inline gate's `return Err(err)`) MUST map \
         back to `main()` `return Err(err)` — same error exit with the chain."
    );
    // both typed variants exist for the mapping.
    assert!(
        src.contains("BootAbortClean") && src.contains("BootAbortErr(anyhow::Error)"),
        "D2a: `StartLaneError` MUST carry both boot-abort flavours."
    );
}

/// The 2-WS Dhan lock holds: the extracted lane still spawns the SAME two Dhan
/// WebSockets via the SAME helpers — no new endpoint, no 2nd main-feed conn.
#[test]
fn extraction_preserves_the_2_ws_dhan_lock() {
    let src = read_main_rs();
    let lane = src
        .find("async fn start_dhan_lane(")
        .expect("start_dhan_lane must exist");
    let lane_end = src[lane + 1..]
        .find("\nasync fn ")
        .map(|rel| lane + 1 + rel)
        .unwrap_or(src.len());
    let body = &src[lane..lane_end];
    assert!(
        body.contains("create_websocket_pool("),
        "D2a: the lane MUST still create the main-feed WS pool via \
         `create_websocket_pool` (1 main-feed conn — the 2-WS lock)."
    );
    assert!(
        body.contains("run_order_update_connection("),
        "D2a: the lane MUST still run the order-update WS via \
         `run_order_update_connection` (the 2nd of the 2 Dhan WebSockets)."
    );
    // The 2-WS Dhan lock (exactly 1 main-feed + 1 order-update, no new endpoint,
    // no 2nd main-feed conn) is itself ratcheted by
    // `indices4only_scope_lock_guard.rs`; this guard only asserts the extraction
    // preserved BOTH WS helper call sites (above).
}
