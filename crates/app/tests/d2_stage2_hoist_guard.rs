//! D2 Stage 2 — genuine shared-infra hoist guard (source-scan ratchet).
//!
//! Stage 2 hoisted the PROCESS-shared blocks (notifier, health, seal-writer,
//! the tick broadcast, the obs / tick-storage subscriber tasks, the API
//! server) into a single `build_shared_infra(...)` prefix and deleted the
//! D2-pre duplicate `run_shared_infra_only`.
//!
//! ## Stage-3 dead-WS sweep (2026-07-17)
//! The 21-TF TICK aggregator driver (`spawn_engine_b_aggregator`) is
//! DELETED (no tick input on the REST-only runtime); its spawned-once pins
//! below are re-pointed at the surviving seal-writer + subscriber shape,
//! with a negative pin that the aggregator driver stays deleted.
//!
//! ## PR-C2 re-shape (2026-07-13 — Dhan live-WS lane deletion, operator
//! retirement directive per websocket-connection-scope-lock.md "2026-07-13
//! Amendment" §B)
//! The fast crash-recovery arm, the "SLOW BOOT PATH" marker, the
//! `let dhan_lane: Option<DhanLaneRunHandles> = if config.feeds.dhan_enabled`
//! gate, and the Dhan `run_tick_processor` publish site all DIED with the
//! lane — main.rs has a SINGLE boot path. Retired tests:
//! - `dhan_lane_is_wrapped_in_feed_gate` — the lane gate no longer exists;
//!   the replacement Dhan shape (dhan_rest_stack spawn + no lane primitives)
//!   is pinned by `per_feed_boot_isolation_guard.rs`.
//! - the Dhan publish-ORDERING half of
//!   `aggregator_subscribe_precedes_dhan_tick_publish` — there is no Dhan
//!   tick publisher; the Groww bridge runs its OWN aggregator instance with
//!   its own wiring guards. The builder-spawns-the-shared-pipeline half
//!   SURVIVES below.
//! The single-construction contract (ONE `build_shared_infra` call, the API
//! bound once, the seal-writer spawned once) survives re-anchored on the
//! whole file instead of the deleted slow-path slice.

/// Read `crates/app/src/main.rs` regardless of the test's working directory.
fn read_main_rs() -> String {
    std::fs::read_to_string("src/main.rs")
        .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
        .expect("main.rs must be readable from the app crate test working dir")
}

/// Body of `build_shared_infra` — from its `async fn` line to the next
/// top-level `async fn` (or EOF).
fn build_shared_infra_body(src: &str) -> &str {
    let builder = src
        .find("async fn build_shared_infra(")
        .expect("build_shared_infra must exist");
    let builder_end = src[builder..]
        .find("\nasync fn ")
        .map(|rel| builder + rel)
        .unwrap_or(src.len());
    &src[builder..builder_end]
}

#[test]
fn run_shared_infra_only_is_deleted() {
    // The D2-pre duplicate OFF-only shared-infra mirror FUNCTION is gone — the
    // single boot path routes through the one `build_shared_infra` construction.
    // (Comments may still REFERENCE the old name; we assert the definition + call
    // are gone, not the mere string.)
    let src = read_main_rs();
    assert!(
        !src.contains("fn run_shared_infra_only"),
        "D2 Stage 2: the `run_shared_infra_only` function (the D2-pre OFF-only \
         duplicate) MUST stay deleted — the boot path shares the ONE \
         `build_shared_infra` construction."
    );
    assert!(
        !src.contains("run_shared_infra_only("),
        "D2 Stage 2: there must be NO call to `run_shared_infra_only(...)`."
    );
    assert!(
        src.contains("async fn build_shared_infra("),
        "D2 Stage 2: the single shared-infra builder `build_shared_infra` MUST exist."
    );
}

#[test]
fn single_shared_infra_construction() {
    // Exactly ONE shared-infra construction: one `build_shared_infra(` call
    // beyond the definition, the API bound once (inside the builder), the
    // seal-writer spawned once (inside the builder). PR-C2: re-anchored
    // on the whole file — the fast/slow split no longer exists.
    let src = read_main_rs();

    let total = src.matches("build_shared_infra(").count();
    let defs = src.matches("async fn build_shared_infra(").count();
    assert_eq!(defs, 1, "exactly one build_shared_infra definition");
    assert_eq!(
        total - defs,
        1,
        "exactly ONE `build_shared_infra(...)` call must exist (found {}) — a \
         second construction would double-spawn the shared pipeline.",
        total - defs
    );

    let builder_body = build_shared_infra_body(&src);

    // The API binds exactly once, and that bind lives inside the builder.
    let serve_total = src.matches("axum::serve(").count();
    assert_eq!(
        serve_total, 1,
        "exactly one `axum::serve` bind may exist (found {serve_total}) — a \
         second bind would double-bind the port and break boot."
    );
    assert_eq!(
        builder_body.matches("axum::serve(").count(),
        1,
        "the single `axum::serve` bind must live inside build_shared_infra."
    );

    // The seal-writer is spawned exactly once, inside the builder (stage-3
    // dead-WS sweep, 2026-07-17: the tick-aggregator spawn pin retired with
    // `spawn_engine_b_aggregator`; the seal chain's single install point is
    // the surviving invariant).
    let sw_defs = src.matches("fn spawn_seal_writer_loop(").count();
    let sw_total = src.matches("spawn_seal_writer_loop(").count();
    assert_eq!(
        sw_total - sw_defs,
        1,
        "exactly one `spawn_seal_writer_loop` call may exist (found {}) — \
         a second would double-install the global seal sender.",
        sw_total - sw_defs
    );
    assert_eq!(
        builder_body.matches("spawn_seal_writer_loop(").count(),
        1,
        "the single seal-writer spawn must live inside build_shared_infra."
    );

    // Stage-3 negative pin: the deleted 21-TF tick-aggregator driver must
    // stay deleted — re-adding it would resurrect a publisher-less consumer.
    // (Paren-suffixed needle so the dated retirement COMMENTS naming the fn
    // do not trip the pin — only a real definition/call site does.)
    assert!(
        !src.contains("spawn_engine_b_aggregator("),
        "stage-3 dead-WS sweep: `spawn_engine_b_aggregator` (the 21-TF tick \
         aggregator driver) must stay deleted from main.rs."
    );
}

#[test]
fn shared_infra_builder_spawns_the_shared_pipeline() {
    // The builder spawns the shared candle pipeline + subscribers for EVERY
    // boot (Groww-only included). PR-C2: the Dhan publish-ordering half of
    // the original `aggregator_subscribe_precedes_dhan_tick_publish` retired
    // with the Dhan tick publisher; this builder-contents half survives —
    // the subscribers still `.subscribe()` to the tick broadcast inside the
    // builder, ahead of any future publisher.
    let src = read_main_rs();
    let body = build_shared_infra_body(&src);
    // `run_tick_storage_consumer` assertion RETIRED 2026-07-19 (BATCH-5):
    // the tick-storage consumer was deleted in the PrevDayCache/TickStorage
    // cleanup, so the builder no longer spawns it. The surviving spawns below
    // still pin the shared candle pipeline + seal-writer for every boot.
    for needle in ["run_slow_boot_observability", "spawn_seal_writer_loop"] {
        assert!(
            body.contains(needle),
            "build_shared_infra MUST spawn `{needle}` so the shared candle \
             pipeline + subscribers run for every boot."
        );
    }
}
