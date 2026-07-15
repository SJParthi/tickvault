//! Phase 2.7 — perf + correctness fixes from the 3-agent adversarial
//! review on the Phase 2 production diff.
//!
//! Pins the four fixes:
//!  - Hot-path C1: `volume_delta_tracker::record_tick` uses single
//!    papaya `insert` (returns prior value), not get + insert (2
//!    probes → 1 probe).
//!  - Hot-path C2: `now_ist_secs_of_day()` is hoisted OUT of the
//!    per-tick branch into a per-frame cache (`frame_now_ist_secs`).
//!  - Hostile C1: midnight rollover task is spawned in slow boot;
//!    `spawn_midnight_rollover_task` exists.
//!  - Hostile C2: VOLUME-MONO-01 suppression is wired —
//!    `tick_processor.rs` gates `volume_monotonicity_guard.observe`
//!    on `!volume_is_first_seen && phase == Phase::Open`.

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

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|err| panic!("read {p:?}: {err}"))
}

/// Hot-path C1 ratchet: record_tick uses single insert (returns prior),
/// NOT the legacy get + insert pair. The string scan looks for the new
/// pattern's signature ("self.inner.insert(key, current_volume," followed
/// by the match arm) and asserts the legacy `self.inner.get(&key, &guard).copied()`
/// pattern has been removed.
#[test]
fn hot_path_c1_record_tick_uses_single_papaya_insert() {
    let src = read("core/src/pipeline/volume_delta_tracker.rs");
    assert!(
        src.contains("self.inner.insert(key, current_volume, &guard)"),
        "record_tick must call self.inner.insert(key, current_volume, &guard) \
         (returns Option<&V> with prior value — single hash probe)"
    );
    // The legacy pattern was: get -> copied -> match -> then insert.
    // After C1 fix, the get+copied call should be gone from record_tick
    // (only insert should appear in that function).
    assert!(
        !src.contains("let prev = self.inner.get(&key, &guard).copied();"),
        "record_tick must NOT use the legacy get+insert pair (C1 fix: \
         single insert returns prior value)"
    );
}

/// Hot-path C2 ratchet: no per-tick wall-clock syscall on the enrich path.
///
/// **Updated 2026-06-02:** the original C2 fix hoisted the wall-clock read to
/// a frame-level `frame_now_ist_secs` cache. The production code was since
/// improved to derive the seconds-of-day from the TICK'S OWN
/// `exchange_timestamp` (`tick.exchange_timestamp % 86_400`) — zero wall-clock
/// syscalls per tick, and the correct semantic (exchange time, not arrival
/// time). This ratchet now pins that stronger invariant. The old ratchet
/// asserted `frame_now_ist_secs`, which no longer exists — it was stale
/// against the refactor and failing on `main`.
#[test]
fn hot_path_c2_now_ist_secs_hoisted_to_frame_level() {
    let src = read("core/src/pipeline/tick_processor.rs");
    // The per-tick seconds-of-day is derived from the tick's exchange
    // timestamp — no syscall, no shared frame cache needed.
    assert!(
        src.contains("tick.exchange_timestamp % 86_400"),
        "tick_processor must derive the per-tick seconds-of-day from \
         `tick.exchange_timestamp % 86_400` (hot-path C2: no per-tick \
         wall-clock syscall on the enrich path)"
    );
    assert!(
        src.contains("enricher.enrich_tick(&tick, tick_secs_of_day)"),
        "tick_processor must pass the tick-derived `tick_secs_of_day` to \
         enricher.enrich_tick — not a per-tick wall-clock read"
    );
    // Per-tick re-reads of the wall clock inside the persistence
    // branch must be gone (the C2 invariant, unchanged).
    assert!(
        !src.contains("let now_ist_secs = now_ist_secs_of_day();"),
        "tick_processor must NOT re-read the wall clock per tick — derive \
         from the tick's exchange timestamp instead (hot-path C2)"
    );
}

/// Regression: 2026-06-02 — zero-tick-loss WAL must be fail-closed. The WAL
/// (`WsFrameSpill`) is the durable floor of the ring → spill → WAL chain; if it
/// can't init at boot, running without it admits SILENT frame loss under
/// backpressure. The boot MUST `std::process::exit(1)` rather than proceed with
/// `wal_spill = None`. Pins fail-closed so it can't regress to degraded mode.
#[test]
fn test_regression_ws_frame_wal_init_is_fail_closed() {
    let src = read("app/src/main.rs");
    assert!(
        src.contains("HALTING boot (fail-closed)") && src.contains("WsFrameSpill"),
        "main.rs must HALT boot (fail-closed) when WsFrameSpill init fails — \
         the WAL is the zero-tick-loss durability floor"
    );
    assert!(
        !src.contains("proceeding WITHOUT durable WAL"),
        "main.rs must NOT proceed without the durable WAL (degraded mode removed \
         2026-06-02 — would admit silent tick loss)"
    );
}

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): test_regression_prev_day_ohlcv_table_ensured_before_prev_oi_load died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): hostile_c1_midnight_rollover_task_spawned_in_slow_boot died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

/// Hostile C1 ratchet: the spawn helper exists in the public API of
/// the tick_enricher module.
#[test]
fn hostile_c1_spawn_midnight_rollover_task_helper_exists() {
    let src = read("core/src/pipeline/tick_enricher.rs");
    assert!(
        src.contains("pub fn spawn_midnight_rollover_task"),
        "tick_enricher.rs must export pub fn spawn_midnight_rollover_task"
    );
    assert!(
        src.contains("secs_until_next_ist_midnight"),
        "the spawn helper must use secs_until_next_ist_midnight to schedule the loop"
    );
    assert!(
        src.contains("rollover_for_new_day"),
        "the spawn helper must call enricher.rollover_for_new_day inside the loop"
    );
    assert!(
        src.contains("prev_oi_cache.load_from_questdb"),
        "the spawn helper must reload prev_oi_cache from QuestDB at each midnight (L13 step 2)"
    );
}

/// Hostile C2 ratchet: VOLUME-MONO-01 is suppressed on first-seen
/// ticks and on non-OPEN phases. The string scan asserts the gate
/// is present at the volume_monotonicity_guard.observe call site.
#[test]
fn hostile_c2_volume_monotonicity_suppression_wired() {
    let src = read("core/src/pipeline/tick_processor.rs");
    assert!(
        src.contains("suppress_monotonicity"),
        "tick_processor must declare a `suppress_monotonicity` gate at the \
         volume_monotonicity_guard call site"
    );
    assert!(
        src.contains("flags.volume_is_first_seen"),
        "the suppression gate must read `flags.volume_is_first_seen` from the \
         enricher's lifecycle output (L13 step 5)"
    );
    assert!(
        src.contains("flags.phase != tickvault_common::phase::Phase::Open"),
        "the suppression gate must skip when phase != OPEN (PREMARKET/PREOPEN/\
         POSTAUCTION/CLOSED ticks are not subject to live-trading invariants)"
    );
    assert!(
        src.contains("if !suppress_monotonicity {"),
        "the volume_monotonicity_guard.observe call must be gated by `if !suppress_monotonicity`"
    );
}

/// Hostile C2 follow-up ratchet: when no enricher is attached (legacy
/// path), the guard runs unconditionally. This preserves backward
/// compatibility for the fast-boot recovery path.
#[test]
fn hostile_c2_no_enricher_means_no_suppression() {
    let src = read("core/src/pipeline/tick_processor.rs");
    // The match arm for `lifecycle_for_tick` should fall back to false
    // (no suppression) when None.
    assert!(
        src.contains("None => false,"),
        "the suppression gate must fall back to `false` (run guard unconditionally) \
         when no enricher attached — preserves fast-boot path behavior"
    );
}

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): test_spawn_midnight_rollover_task_is_referenced_in_slow_boot died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

/// Phase 2.7 explicit name-match for the pub-fn-test guard:
/// `secs_until_next_ist_midnight` is the scheduler primitive used by
/// the rollover task. Bounds covered by market_hours unit tests; this
/// test pins that the helper is referenced from production code.
#[test]
fn test_secs_until_next_ist_midnight_is_referenced_by_rollover_task() {
    let src = read("core/src/pipeline/tick_enricher.rs");
    assert!(
        src.contains("secs_until_next_ist_midnight"),
        "tick_enricher must use secs_until_next_ist_midnight to schedule the midnight rollover loop"
    );
}

/// Phase 2.7 ratchet: the EnrichedTickFlags helper bundle is exported
/// from tick_enricher so the tick processor can carry the suppression
/// signals across the lifecycle_for_tick mapping without holding the
/// `'a` borrow on ParsedTick.
#[test]
fn phase2_7_enriched_tick_flags_helper_exists() {
    let src = read("core/src/pipeline/tick_enricher.rs");
    assert!(
        src.contains("pub struct EnrichedTickFlags"),
        "tick_enricher must export `pub struct EnrichedTickFlags` so the tick \
         processor can suppress VOLUME-MONO-01 without lifetime gymnastics"
    );
    assert!(
        src.contains("pub volume_is_first_seen: bool")
            && src.contains("pub volume_is_regression: bool")
            && src.contains("pub phase: Phase"),
        "EnrichedTickFlags must carry the three fields the suppression gate reads"
    );
}
