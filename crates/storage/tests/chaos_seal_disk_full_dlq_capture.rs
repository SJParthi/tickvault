//! Chaos test (zero-tick-loss PR-7, G7) — disk-full + QuestDB-down at once,
//! for the SEAL absorption path.
//!
//! The tick path already has this compound-failure coverage
//! (`chaos_disk_full.rs`). The SEAL absorption pipeline added in PR-6 did NOT
//! — PR-6 deliberately deferred the "tier-2 spill fails → tier-3 DLQ catches
//! it" case to this PR. This test closes it.
//!
//! Scenario: QuestDB is DOWN (so seals can never reach the shadow tables and
//! pile into the absorption pipeline) AND the spill disk is unusable. The
//! locked design escalates: ring overflow → spill (FAILS) → DLQ NDJSON
//! (recoverable text). The ONLY loss path is `SubmitOutcome::Dropped`, which
//! needs the DLQ to ALSO fail. With a writable DLQ dir, every overflowing
//! seal MUST be captured in the DLQ — zero drop.
//!
//! Cross-platform failure injection: instead of a flaky `chmod` read-only
//! directory, we point the spill dir at a path that already exists as a
//! regular FILE. `SealSpillWriter::append_seal` calls `create_dir_all(spill_dir)`
//! first, which deterministically errors on every OS when the path is a file
//! — forcing the tier-2 → tier-3 escalation. This runs in normal CI (no
//! `#[ignore]`, no `unsafe`, no platform gate).
//!
//! Honest envelope: this proves bounded zero-loss when spill is dead but the
//! DLQ is writable. The truly-terminal case (DLQ ALSO unwritable →
//! `Dropped` → AGGREGATOR-DROP-01 Critical) is by design the end of the line
//! and is asserted as the typed `Dropped` outcome elsewhere.

use std::path::PathBuf;

use tickvault_common::feed::Feed;
use tickvault_storage::seal_absorption::{SealAbsorptionPipeline, SubmitOutcome};
use tickvault_storage::seal_dlq::SealDlqWriter;
use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

const NOW_UNIX_SECS: i64 = 1_767_268_800;

fn unique_base(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "tv-pr7-g7-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ))
}

fn mk_seal(i: usize) -> BufferedSeal {
    let mut state = LiveCandleState::empty();
    state.bucket_start_ist_secs = 1_716_000_900u32.wrapping_add(i as u32);
    state.open = 100.0;
    state.high = 105.0;
    state.low = 99.0;
    state.close = 100.0 + (i % 100) as f64;
    state.volume = 1234;
    state.bucket_start_cumulative = 1000;
    state.oi = 50_000;
    state.tick_count = 5;
    BufferedSeal::new((i % 11_000) as u32 + 1, 0, TfIndex::M1, state, Feed::Dhan)
}

#[test]
fn test_seal_spill_dead_dlq_captures_every_overflow_zero_drop() {
    const SMALL_RING: usize = 500;
    const BURST: usize = 3_000; // 2_500 overflow → spill (dead) → DLQ
    const OVERFLOW: usize = BURST - SMALL_RING;

    let base = unique_base("dlq-capture");
    std::fs::create_dir_all(&base).expect("base dir");

    // Make the spill dir UNUSABLE by creating a regular file at its path —
    // `create_dir_all` inside append_seal will error on every OS, forcing the
    // tier-2 → tier-3 (DLQ) escalation. This stands in for "spill disk full".
    let spill_path = base.join("spill_is_a_file");
    std::fs::write(&spill_path, b"not a directory").expect("write spill blocker file");

    // The DLQ lives on a SEPARATE, writable location — the critical invariant
    // (mirrors chaos_disk_full.rs): even when spill is dead, the audit trail
    // survives.
    let dlq_dir = base.join("dlq");

    let mut pipeline = SealAbsorptionPipeline::with_capacity_and_dirs_for_test(
        SMALL_RING,
        spill_path.clone(),
        dlq_dir.clone(),
    );

    let mut buffered = 0usize;
    let mut spilled = 0usize;
    let mut dlq = 0usize;
    let mut dropped = 0usize;
    for i in 0..BURST {
        match pipeline.submit(mk_seal(i), NOW_UNIX_SECS) {
            SubmitOutcome::Buffered => buffered += 1,
            SubmitOutcome::Spilled => spilled += 1,
            SubmitOutcome::DlqWritten => dlq += 1,
            SubmitOutcome::Dropped(_) => dropped += 1,
        }
    }

    // Headline guarantee: not a single seal lost, even with spill dead.
    assert_eq!(
        dropped, 0,
        "DLQ must catch every overflow when spill is dead — zero drop"
    );
    // Spill is dead, so NOTHING may report Spilled.
    assert_eq!(
        spilled, 0,
        "spill dir is unusable — no seal may report Spilled"
    );
    // The ring fills, then every overflow escalates straight to the DLQ.
    assert_eq!(buffered, SMALL_RING, "ring fills to capacity first");
    assert_eq!(dlq, OVERFLOW, "every overflow seal must be DLQ-captured");
    // Conservation.
    assert_eq!(buffered + spilled + dlq + dropped, BURST);

    // The DLQ NDJSON file must actually contain every captured payload as
    // recoverable text — read it back through the real reader.
    let reader = SealDlqWriter::with_dlq_dir_for_test(dlq_dir.clone());
    let recovered = reader.read_all(NOW_UNIX_SECS).expect("DLQ read_all");
    assert_eq!(
        recovered.len(),
        OVERFLOW,
        "DLQ file must hold exactly the {OVERFLOW} captured seals as recoverable text"
    );

    let _ = std::fs::remove_dir_all(&base);
}
