//! Per-feed boot-isolation guarantee (operator lock 2026-06-23).
//!
//! Operator requirement (verbatim intent): *"until or unless the feed is ON,
//! the entire architecture related to that particular feed should never ever
//! be touched."* i.e. a feed that is OFF must trigger ZERO work — no auth, no
//! instrument fetch, no connection.
//!
//! This is a SOURCE-SCAN ratchet (same pattern as the existing
//! `secret_manager.rs::test_*_is_wired_into_main` guards): it reads the literal
//! `crates/app/src/main.rs` and asserts the per-feed boot gates are present, so
//! the build FAILS if a future edit removes the gate and lets an OFF feed start
//! authenticating / fetching instruments / connecting.
//!
//! It does NOT (and cannot) prove runtime behaviour by itself — the live boot
//! is exercised by the app's own boot path — but it mechanically prevents the
//! regression class "someone deleted the `if feeds.<x>_enabled` guard".

/// Read `crates/app/src/main.rs` regardless of the test's working directory.
fn read_main_rs() -> String {
    std::fs::read_to_string("src/main.rs")
        .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
        .expect("main.rs must be readable from the app crate test working dir")
}

/// End index (relative to `src`) of the top-level item that starts at
/// byte `item_start`, i.e. the position of the NEXT top-level item after it.
///
/// The earlier heuristic bounded a function body at the next `"\nasync fn "`
/// only. That leaks any intervening NON-async top-level item (a `struct` or
/// `enum` and its doc comment) into the body slice. FTC-14 (audit 2026-07-01)
/// added the `enum StartLaneError { … WsPoolSpawn … }` block — whose doc
/// comment legitimately mentions `create_websocket_pool` (it describes the
/// LANE, not shared infra) — BETWEEN `build_shared_infra` and the next
/// `async fn`, tripping the negative isolation assertion falsely. Bounding on
/// the earliest of the next top-level `fn`/`async fn`/`struct`/`enum`/`impl`
/// keeps each assertion scoped to the actual function body it names.
fn next_top_level_item(src: &str, item_start: usize) -> usize {
    let tail = &src[item_start + 1..];
    ["\nasync fn ", "\nfn ", "\nstruct ", "\nenum ", "\nimpl "]
        .iter()
        .filter_map(|marker| tail.find(marker))
        .min()
        .map_or(src.len(), |rel| item_start + 1 + rel)
}

#[test]
fn dhan_boot_is_gated_on_dhan_enabled() {
    let src = read_main_rs();
    // The Dhan lane (auth + daily-universe instruments + WS) must be wrapped by
    // a `dhan_enabled` gate, AND there must be an explicit skip branch for the
    // OFF case. Either spelling of the config access is accepted.
    let has_positive_gate =
        src.contains("feeds.dhan_enabled") || src.contains("config.feeds.dhan_enabled");
    assert!(
        has_positive_gate,
        "main.rs MUST gate the Dhan lane on `feeds.dhan_enabled` — removing it \
         would let an OFF Dhan feed authenticate / fetch instruments (operator \
         lock 2026-06-23: OFF feed = nothing touched)."
    );
    let has_skip_branch =
        src.contains("!config.feeds.dhan_enabled") || src.contains("!feeds.dhan_enabled");
    assert!(
        has_skip_branch,
        "main.rs MUST have an explicit `if !feeds.dhan_enabled` skip branch so \
         Dhan-OFF takes ZERO boot action (no auth, no instruments, no WS)."
    );
}

/// Read a Groww lane-module source regardless of the test's working directory.
fn read_app_src(file: &str) -> String {
    std::fs::read_to_string(format!("src/{file}"))
        .or_else(|_| std::fs::read_to_string(format!("crates/app/src/{file}")))
        .unwrap_or_else(|_| panic!("{file} must be readable from the app crate test working dir"))
}

// RETIRED (2026-07-15 — Groww live-feed deletion, operator directive per
// the [groww_universe] rider re-home): groww_lanes_spawn_dormant_and_self_idle
// and off_feed_reconciler_never_emits_start died with the machinery they
// pinned — groww_bridge.rs / groww_sidecar_supervisor.rs / groww_activation.rs
// (the dormant lanes + the LaneAction reconciler) were deleted; the surviving
// Groww surface is the per-minute REST legs (config-gated, feed-flag
// independent) + the [groww_universe] daily watch-set rider.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md "2026-07-13
// Amendment"): dhan_lanes_spawn_dormant_and_self_idle died with the
// machinery it pinned — `run_dhan_activation_watcher` / dhan_activation.rs /
// the D2b runtime cold-start were deleted with the lane; a runtime Dhan
// enable is now REFUSED API-side with 409 (the PR-E ON-half is revoked) and
// the surviving Dhan surface (dhan_rest_stack) is config+restart only.

#[test]
fn both_feeds_off_is_handled_explicitly() {
    let src = read_main_rs();
    // When NEITHER feed is enabled the app must NOT silently do feed work — it
    // halts / no-feed-path. `any_enabled()` is the shared helper for that gate.
    assert!(
        src.contains("any_enabled()"),
        "main.rs MUST consult `feeds.any_enabled()` so the both-OFF case takes \
         no feed action (no auth, no instruments for either feed)."
    );
}

#[test]
fn dhan_auth_and_universe_exist_so_the_gate_actually_wraps_work() {
    // Sanity: the very work the isolation guards protect must still exist —
    // otherwise the isolation claim is vacuous. RE-POINTED PR-C2 (2026-07-13):
    // the Dhan auth (TokenManager::initialize) moved from the deleted
    // `start_dhan_lane` into dhan_rest_stack.rs; the instrument download
    // chain was deleted outright (Q3 — hardcoded SIDs feed the REST pulls),
    // so only the auth half remains pinned.
    let stack = read_app_src("dhan_rest_stack.rs");
    assert!(
        stack.contains("TokenManager::initialize("),
        "Dhan auth (TokenManager::initialize) must exist in dhan_rest_stack.rs \
         for the Dhan isolation contract to be meaningful."
    );
}

#[test]
fn dhan_off_skips_auth_and_instruments_via_the_lane_gate() {
    // PR-C2 re-shape (2026-07-13, operator retirement directive —
    // websocket-connection-scope-lock.md "2026-07-13 Amendment"): the inline
    // Dhan lane (`let dhan_lane: Option<DhanLaneRunHandles> = if
    // config.feeds.dhan_enabled { … start_dhan_lane … }`) was DELETED with the
    // live-WS retirement. The C2 isolation shape: main.rs itself performs ZERO
    // Dhan auth / instrument fetch / WS spawn — the ONLY Dhan surface is the
    // REST-only stack module, and the deleted-lane primitives must never
    // reappear in main.rs.
    let src = read_main_rs();
    for (banned, why) in [
        (
            "TokenManager::initialize(",
            "Dhan auth lives ONLY in dhan_rest_stack.rs since PR-C2",
        ),
        (
            "create_websocket_pool(",
            "the Dhan main-feed WS pool is retired (scope-lock §D)",
        ),
        (
            "authenticating with Dhan",
            "the slow-arm Dhan auth step retired with start_dhan_lane",
        ),
        (
            "load_instruments(",
            "the Dhan instrument download chain is deleted (Q3 — hardcoded SIDs)",
        ),
    ] {
        assert!(
            !src.contains(banned),
            "main.rs must NOT contain `{banned}` — {why}. The OFF-feed \
             isolation guarantee (operator lock 2026-06-23) holds by \
             construction: main.rs does no Dhan work at all."
        );
    }
    // The Dhan REST surface is brought up by exactly one spawn of the stack
    // module (which owns lock-before-mint + the dormant order-update WS).
    assert!(
        src.contains("dhan_rest_stack::spawn_dhan_rest_stack("),
        "main.rs must spawn the Dhan REST-only stack (the sole surviving Dhan \
         surface — dual-instance-lock-2026-07-04.md §3.5)."
    );

    // The hoisted `build_shared_infra` (runs for every boot) must stay
    // Dhan-free so the work an OFF/retired feed still runs is Dhan-free.
    let shared = src
        .find("async fn build_shared_infra(")
        .expect("build_shared_infra must exist (D2 Stage 2 shared-infra hoist)");
    let shared_body_end = next_top_level_item(&src, shared);
    let shared_body = &src[shared..shared_body_end];
    assert!(
        !shared_body.contains("dhan_rest_stack") && !shared_body.contains("TokenManager"),
        "build_shared_infra MUST NOT bring up any Dhan surface — it builds ONLY \
         the PROCESS-shared infra (notifier, health, seal-writer, broadcasts, \
         API server)."
    );

    // The duplicate `run_shared_infra_only` stays deleted (D2 Stage 2).
    assert!(
        !src.contains("fn run_shared_infra_only"),
        "the D2-pre duplicate `run_shared_infra_only` MUST stay deleted — the \
         single hoisted `build_shared_infra` serves every boot."
    );
}

#[test]
fn api_server_up_in_dhan_off_mode() {
    // A Dhan-OFF (Groww-only) boot must STILL bring up the HTTP API server
    // (so `/api/feeds` is reachable) + the candle seal-writer + the 21-TF
    // aggregator + the single process run-loop. PR-C2 re-shape (2026-07-13):
    // there is only ONE boot path now — `build_shared_infra` runs
    // unconditionally and `run_process_runloop` closes main().
    let src = read_main_rs();

    // (1) build_shared_infra is called before the Dhan REST stack spawn.
    let shared_call = src
        .find("build_shared_infra(")
        .expect("main() must call build_shared_infra(...) to build the shared prefix");
    let stack_spawn = src
        .find("dhan_rest_stack::spawn_dhan_rest_stack(")
        .expect("main() must spawn the Dhan REST-only stack");
    assert!(
        shared_call < stack_spawn,
        "build_shared_infra MUST be called BEFORE the Dhan REST stack spawn so \
         the PROCESS-shared infra (API + seal-writer + aggregator) is up \
         regardless of the Dhan surface (C1/C2 fix lineage)."
    );

    // (2) build_shared_infra must actually build the API server, /api/feeds
    //     routes, the seal-writer, and the aggregator.
    let shared = src
        .find("async fn build_shared_infra(")
        .expect("build_shared_infra must exist (D2 Stage 2)");
    let shared_body_end = next_top_level_item(&src, shared);
    let body = &src[shared..shared_body_end];
    for (needle, what) in [
        ("axum::serve", "the HTTP API server"),
        ("build_router_with_auth", "the /api/feeds toggle routes"),
        (
            "spawn_seal_writer_loop",
            "the candle seal-writer (Groww candles seal — C2)",
        ),
        ("spawn_engine_b_aggregator", "the 21-TF aggregator"),
    ] {
        assert!(
            body.contains(needle),
            "build_shared_infra MUST bring up {what} (`{needle}`) so a Dhan-OFF boot \
             has the PROCESS-shared infra running (C1/C2 fix)."
        );
    }

    // (3) The single PROCESS run-loop closes main(), after the stack spawn.
    let runloop_call = src
        .find("run_process_runloop(")
        .expect("main() must end with the single `run_process_runloop(...)`");
    assert!(
        stack_spawn < runloop_call,
        "the shared `run_process_runloop(...)` MUST come AFTER the Dhan REST \
         stack spawn — it runs the run-loop over the hoisted shared infra for \
         every boot mode."
    );
}
