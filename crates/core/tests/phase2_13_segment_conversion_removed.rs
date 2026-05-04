//! Phase 2.13 — hot-path agent M2 fix: dead `ExchangeSegment::from_byte`
//! conversion removed from the per-tick enrichment path.
//!
//! What was wasted: `compute_phase` ignores its `_segment` parameter
//! today (NSE/BSE share boundaries; reserved for future MCX support).
//! The enricher converted `tick.exchange_segment_code: u8` →
//! `ExchangeSegment` enum (via `from_byte`) → passed it to
//! `compute_phase` which immediately threw it away. At 25K tps that's
//! 25,000 pointless conversions per second.
//!
//! The fix: new `compute_phase_by_segment_code(secs, code: u8)`
//! variant takes the binary code directly. Behaviour identical to
//! `compute_phase` today (via shared `compute_phase_inner`). Drift
//! between the two entry points is mechanically impossible because
//! they share the same private inner function.
//!
//! ## Hot-path impact
//!
//! Removes one match-on-int + one Option::unwrap_or per tick.
//! ~3-5 ns saved per tick × 25,000 tps = ~100µs/sec total.
//! Modest but free — the conversion was producing nothing.

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

#[test]
fn phase2_13_compute_phase_by_segment_code_exists() {
    let src = read("common/src/phase.rs");
    assert!(
        src.contains("pub fn compute_phase_by_segment_code"),
        "phase.rs must export pub fn compute_phase_by_segment_code (Phase 2.13 hot-path M2 fix)"
    );
}

#[test]
fn phase2_13_shared_inner_function_avoids_drift() {
    let src = read("common/src/phase.rs");
    assert!(
        src.contains("fn compute_phase_inner"),
        "phase.rs must use a shared `compute_phase_inner` so both public entry points cannot drift"
    );
}

#[test]
fn phase2_13_tick_enricher_no_longer_calls_from_byte() {
    let src = read("core/src/pipeline/tick_enricher.rs");
    assert!(
        !src.contains("ExchangeSegment::from_byte(seg)"),
        "tick_enricher must NOT call ExchangeSegment::from_byte(seg) on the hot path \
         after Phase 2.13 (dead conversion — compute_phase ignores the result)"
    );
}

#[test]
fn phase2_13_tick_enricher_uses_segment_code_variant() {
    let src = read("core/src/pipeline/tick_enricher.rs");
    assert!(
        src.contains("compute_phase_by_segment_code(now_ist_secs_of_day, seg)"),
        "tick_enricher must call compute_phase_by_segment_code(secs, seg) directly \
         (Phase 2.13 hot-path M2 fix)"
    );
}

#[test]
fn phase2_13_exchange_segment_import_removed_from_enricher() {
    let src = read("core/src/pipeline/tick_enricher.rs");
    // The enricher no longer needs `ExchangeSegment` — confirm the
    // import is gone (defence-in-depth: an unused import would still
    // compile but signals dead code).
    assert!(
        !src.contains("use tickvault_common::types::ExchangeSegment;"),
        "tick_enricher must NOT import ExchangeSegment after Phase 2.13 — unused"
    );
}

/// Behaviour parity: the new `compute_phase_by_segment_code` produces
/// the same Phase as the legacy `compute_phase` for every minute of
/// the IST day. Defence-in-depth against future drift.
#[test]
fn phase2_13_segment_code_variant_matches_legacy_compute_phase() {
    use tickvault_common::phase::{compute_phase, compute_phase_by_segment_code};
    use tickvault_common::types::ExchangeSegment;

    // Sweep every IST minute. For each segment-code/enum pair, the
    // two functions MUST return identical Phase.
    for minute in 0..(24 * 60_u32) {
        let secs = minute * 60;
        let legacy = compute_phase(secs, ExchangeSegment::NseEquity);
        let by_code = compute_phase_by_segment_code(secs, 1); // NSE_EQ = 1
        assert_eq!(
            legacy, by_code,
            "compute_phase vs compute_phase_by_segment_code mismatch at minute {minute}"
        );
    }
}

/// Edge case: passing an unknown segment code (e.g. 99, the gap at 6,
/// 255) to `compute_phase_by_segment_code` must NOT panic and must
/// return the same Phase as the segment-agnostic logic.
#[test]
fn phase2_13_unknown_segment_code_does_not_panic() {
    use tickvault_common::phase::{Phase, compute_phase_by_segment_code};

    let secs_open = 9 * 3600 + 30 * 60; // 09:30 IST = OPEN
    for unknown_code in [6_u8, 99, 100, 255] {
        let phase = compute_phase_by_segment_code(secs_open, unknown_code);
        assert_eq!(
            phase,
            Phase::Open,
            "unknown segment_code {unknown_code} must not affect phase classification"
        );
    }
}
