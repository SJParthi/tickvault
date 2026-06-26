//! D2 Stage 2 — genuine shared-infra hoist guard (source-scan ratchet).
//!
//! Stage 2 hoists the 9 PROCESS-shared blocks (notifier, health, seal-writer,
//! the tick + order-update broadcasts, the obs / 21-TF aggregator / tick-storage
//! subscriber tasks, the API server) OUT of the Dhan-ON slow arm and into a
//! single `build_shared_infra(...)` prefix shared by BOTH the Dhan-OFF and
//! Dhan-ON-slow paths, deletes the D2-pre duplicate `run_shared_infra_only`, and
//! wraps the Dhan lane in a single `if config.feeds.dhan_enabled { … }` whose
//! value is `Option<DhanLaneRunHandles>`, then runs ONE `run_process_runloop`.
//!
//! This SOURCE-SCAN ratchet (same pattern as the existing boot-isolation guards)
//! reads the literal `crates/app/src/main.rs` and fails the build if a future
//! edit re-introduces a duplicate shared-infra construction, removes the lane
//! gate, double-binds the API, or breaks the subscribe-before-publish ordering
//! (the zero-tick-loss invariant).

/// Read `crates/app/src/main.rs` regardless of the test's working directory.
fn read_main_rs() -> String {
    std::fs::read_to_string("src/main.rs")
        .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
        .expect("main.rs must be readable from the app crate test working dir")
}

/// Slice of `main()` *after the fast crash-recovery arm* — i.e. the unified slow
/// path: the hoisted `build_shared_infra` prefix + the Dhan lane + the run-loop.
/// Bounded `[SLOW BOOT PATH .. first top-level fn after main]`. The fast arm
/// (Dhan-ON-only, byte-identical by design L10/D2d) and the helper fn
/// DEFINITIONS at the end of the file are both excluded, so counts reflect the
/// slow-path CALL sites only.
fn slow_path_of_main(src: &str) -> String {
    let slow_marker = src.find("SLOW BOOT PATH").expect("slow-boot marker");
    // `main()` ends at the first top-level `fn ` after it (column-0).
    let main_end = src[slow_marker..]
        .find("\nfn ")
        .or_else(|| src[slow_marker..].find("\nasync fn "))
        .map(|rel| slow_marker + rel)
        .unwrap_or(src.len());
    src[slow_marker..main_end].to_string()
}

#[test]
fn run_shared_infra_only_is_deleted() {
    // The D2-pre duplicate OFF-only shared-infra mirror FUNCTION is gone — Stage 2
    // routes BOTH paths through the single `build_shared_infra` construction.
    // (Comments may still REFERENCE the old name; we assert the definition + call
    // are gone, not the mere string.)
    let src = read_main_rs();
    assert!(
        !src.contains("fn run_shared_infra_only"),
        "D2 Stage 2: the `run_shared_infra_only` function (the D2-pre OFF-only \
         duplicate) MUST be deleted — both the Dhan-OFF and Dhan-ON-slow paths now \
         share the ONE `build_shared_infra` construction."
    );
    assert!(
        !src.contains("run_shared_infra_only("),
        "D2 Stage 2: there must be NO call to `run_shared_infra_only(...)` — the \
         Dhan-OFF path falls through to the unified `build_shared_infra` prefix."
    );
    assert!(
        src.contains("async fn build_shared_infra("),
        "D2 Stage 2: the single shared-infra builder `build_shared_infra` MUST exist."
    );
}

#[test]
fn single_shared_infra_construction() {
    // Exactly ONE shared-infra construction reachable from the slow boot path:
    // the `axum::serve` API bind happens once (in build_shared_infra, CALLED once
    // from the slow path), and `spawn_engine_b_aggregator` is CALLED once from the
    // slow path (via build_shared_infra). (The fast arm has its own byte-identical
    // copy by design L10/D2d — excluded; helper fn DEFINITIONS are excluded.)
    let src = read_main_rs();
    let slow = slow_path_of_main(&src);

    // Exactly one `build_shared_infra(` CALL on the slow path.
    let build_calls = slow.matches("build_shared_infra(").count();
    assert_eq!(
        build_calls, 1,
        "D2 Stage 2: exactly ONE `build_shared_infra(...)` call must exist on the slow \
         path. Found {build_calls}."
    );

    // The old slow-arm `axum::serve` bind is removed — the API now binds ONLY in
    // build_shared_infra, so there is NO `axum::serve` call left in main()'s slow
    // path (it moved into the helper fn).
    let serve_calls = slow.matches("axum::serve(").count();
    assert_eq!(
        serve_calls, 0,
        "D2 Stage 2: the slow-arm `axum::serve` bind MUST be removed (the API now \
         binds once inside `build_shared_infra`). Found {serve_calls} stray binds — a \
         second bind would double-bind the port and break boot."
    );

    // Likewise the old slow-arm `spawn_engine_b_aggregator` call is removed (it
    // moved into build_shared_infra).
    let aggregator_calls = slow.matches("spawn_engine_b_aggregator(").count();
    assert_eq!(
        aggregator_calls, 0,
        "D2 Stage 2: the slow-arm `spawn_engine_b_aggregator` call MUST be removed (it \
         now runs once inside `build_shared_infra`). Found {aggregator_calls} stray \
         calls — a second would double-spawn the 21-TF aggregator."
    );
}

#[test]
fn dhan_lane_is_wrapped_in_feed_gate() {
    // The Dhan lane is a single `let dhan_lane: Option<DhanLaneRunHandles> =
    // if config.feeds.dhan_enabled { … } else { None };` wrapper, sitting AFTER
    // the fast arm and BEFORE the shared run-loop.
    let src = read_main_rs();
    let lane_gate = src
        .find("let dhan_lane: Option<DhanLaneRunHandles> = if config.feeds.dhan_enabled")
        .expect(
            "D2 Stage 2: the Dhan lane MUST be wrapped in a single \
                 `let dhan_lane: Option<DhanLaneRunHandles> = if config.feeds.dhan_enabled` \
                 expression",
        );
    let slow_marker = src.find("SLOW BOOT PATH").expect("slow-boot marker");
    assert!(
        slow_marker < lane_gate,
        "the Dhan lane gate MUST come after the slow-boot marker (the fast arm is separate)."
    );
    let runloop = src
        .find("run_process_runloop(\n        dhan_lane,")
        .expect("the shared `run_process_runloop(dhan_lane, …)` must exist");
    assert!(
        lane_gate < runloop,
        "the shared `run_process_runloop(dhan_lane, …)` MUST come AFTER the lane gate."
    );
}

#[test]
fn aggregator_subscribe_precedes_dhan_tick_publish() {
    // The zero-tick-loss invariant: the hoisted subscribers (obs + 21-TF
    // aggregator + tick-storage) MUST `.subscribe()` to the tick broadcast BEFORE
    // the lane's `run_tick_processor` (the only publisher) runs. After the hoist
    // the subscribers live in `build_shared_infra` (the prefix) and the processor
    // lives in the lane below, so the prefix-builder's body must precede the lane
    // publish site in `main()`.
    let src = read_main_rs();

    // The hoisted prefix is built by the `build_shared_infra(...)` CALL in main()'s
    // slow path (the destructuring `let SharedInfraHandles { … } = build_shared_infra(`).
    let shared_call = src
        .find("} = build_shared_infra(")
        .expect("main() must destructure `build_shared_infra(...)` in its slow path");
    // The SLOW lane's tick processor publishes into the broadcast. (The fast arm
    // has its own earlier `run_tick_processor` — search from the slow-path
    // build_shared_infra call so we anchor on the slow lane's publish site.)
    let publish = src[shared_call..]
        .find("run_tick_processor(")
        .map(|rel| shared_call + rel)
        .expect("the slow lane's run_tick_processor publish site must exist");
    assert!(
        shared_call < publish,
        "D2 Stage 2: `build_shared_infra(...)` (which spawns the obs / aggregator / \
         tick-storage subscribers, each `.subscribe()`d to the tick broadcast) MUST be \
         called BEFORE the lane's `run_tick_processor` publishes — subscribe-before-\
         publish, the zero-tick-loss invariant."
    );

    // And the builder itself contains the three subscriber sites + the seal-writer.
    let builder = src
        .find("async fn build_shared_infra(")
        .expect("build_shared_infra must exist");
    let builder_end = src[builder..]
        .find("\nasync fn ")
        .map(|rel| builder + rel)
        .unwrap_or(src.len());
    let body = &src[builder..builder_end];
    for needle in [
        "run_slow_boot_observability",
        "spawn_engine_b_aggregator",
        "run_tick_storage_consumer",
        "spawn_seal_writer_loop",
    ] {
        assert!(
            body.contains(needle),
            "D2 Stage 2: build_shared_infra MUST spawn `{needle}` so the shared candle \
             pipeline + subscribers run for BOTH the Dhan-OFF and Dhan-ON-slow paths."
        );
    }
}
