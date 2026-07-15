//! Phase 2.5 — `run_tick_processor` enricher param ratchet.
//!
//! Pins the contract that `run_tick_processor` accepts an
//! `Option<Arc<TickEnricher>>` parameter and routes ticks through
//! `append_tick_enriched` when `Some`. Tested at the source-scan level
//! (no live QuestDB needed) so the ratchet runs deterministically in
//! CI on every PR.
//!
//! Why source-scan rather than runtime exercise: the enricher branch
//! needs both a hot ILP writer AND a live tick stream to produce
//! observable output, neither of which we want to spin up in a unit
//! test. Source-scan is sufficient because the failure mode we're
//! guarding against is "future commit accidentally drops the enricher
//! branch" — which a string-match catches reliably.

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

fn read(path: &str) -> String {
    let p = workspace_root().join(path);
    std::fs::read_to_string(&p).unwrap_or_else(|err| panic!("read {p:?}: {err}"))
}

#[test]
fn run_tick_processor_signature_carries_tick_enricher_param() {
    let src = read("core/src/pipeline/tick_processor.rs");
    assert!(
        src.contains("tick_enricher: Option<std::sync::Arc<TickEnricher>>"),
        "run_tick_processor must declare `tick_enricher: Option<std::sync::Arc<TickEnricher>>` parameter"
    );
}

#[test]
fn run_tick_processor_branches_to_append_tick_enriched_on_some_path() {
    let src = read("core/src/pipeline/tick_processor.rs");
    assert!(
        src.contains("writer.append_tick_enriched_with_seq(&tick, life, capture_seq)"),
        "run_tick_processor must call append_tick_enriched_with_seq (replay-stable \
         capture_seq, TICK-SEQ-01 PR-2b) in the enricher Some branch"
    );
    // Phase 2.10: phase classification now uses `tick.exchange_timestamp`
    // (per-tick Dhan-source IST seconds-of-day) instead of `now_ist_secs`
    // wall-clock to fix the boundary-jitter misclassification (hostile
    // bug-hunt M1). Either variable name is acceptable — pin `enrich_tick`
    // is called on the hot path with the second param being one of these.
    assert!(
        src.contains("enricher.enrich_tick(&tick, tick_secs_of_day)")
            || src.contains("enricher.enrich_tick(&tick, frame_now_ist_secs)")
            || src.contains("enricher.enrich_tick(&tick, now_ist_secs)"),
        "run_tick_processor must call enricher.enrich_tick(&tick, <secs>) — accepts \
         tick_secs_of_day (Phase 2.10), frame_now_ist_secs (Phase 2.7), or now_ist_secs (legacy)"
    );
}

#[test]
fn run_tick_processor_falls_back_to_append_tick_on_none_path() {
    let src = read("core/src/pipeline/tick_processor.rs");
    // TICK-SEQ-01 PR-2b: the no-enricher branch persists via the replay-stable
    // `append_tick_with_seq` (threaded capture_seq), NOT the seq-less `append_tick`
    // — else WAL replay duplicates the row under the (ts,sid,segment,capture_seq) key.
    assert!(
        src.contains("writer.append_tick_with_seq(&tick, capture_seq)"),
        "run_tick_processor must use append_tick_with_seq (replay-stable capture_seq) \
         in the no-enricher None branch"
    );
}

#[test]
fn run_tick_processor_uses_canonical_secs_of_day_source() {
    let src = read("core/src/pipeline/tick_processor.rs");
    // Phase 2.10 (hostile M1 fix): tick_secs_of_day is computed from
    // `tick.exchange_timestamp % 86_400` — Dhan's IST-source timestamp,
    // not the receive-time wall clock. Pre-2.10 code used
    // `now_ist_secs_of_day()` which misclassified ~10-30 ticks/day at
    // phase boundaries due to network arrival jitter.
    assert!(
        src.contains("tick.exchange_timestamp % 86_400") || src.contains("now_ist_secs_of_day"),
        "run_tick_processor must derive secs_of_day from either the per-tick \
         exchange_timestamp (Phase 2.10 fix) or the wall-clock helper (legacy) — \
         not inline IST offset arithmetic"
    );
}

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): main_rs_call_sites_pass_none_for_tick_enricher died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

#[test]
fn tick_lifecycle_is_built_from_enriched_tick_in_processor() {
    let src = read("core/src/pipeline/tick_processor.rs");
    // The mapping from EnrichedTick to TickLifecycle must remain 1:1 by
    // field name. If a future commit adds a 5th lifecycle column, this
    // test fails until the call site is updated to forward it.
    assert!(src.contains("volume_delta: enriched.volume_delta"));
    assert!(src.contains("prev_day_close: enriched.prev_day_close"));
    assert!(src.contains("prev_day_oi: enriched.prev_day_oi"));
    assert!(src.contains("phase: enriched.phase as u8"));
}
