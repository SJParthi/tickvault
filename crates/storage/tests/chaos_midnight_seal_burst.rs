//! Chaos test (zero-tick-loss PR-6, G5) — IST-midnight seal-burst absorption.
//!
//! At IST midnight the multi-TF aggregator force-seals every open bucket
//! across the whole universe at once — the largest single seal burst of the
//! day. The locked design (Wave 6 L-C1) absorbs it with an infallible
//! 3-tier cascade in [`SealAbsorptionPipeline`]:
//!
//!   tier 1  SealRing        (in-memory FIFO, `SEAL_BUFFER_CAPACITY = 200_000`)
//!   tier 2  SealSpillWriter (128-byte binary records on disk) — catches ring overflow
//!   tier 3  SealDlqWriter   (NDJSON recoverable text)         — catches spill failure
//!
//! `submit()` is infallible: every seal lands in exactly one tier and the
//! ONLY loss path is `SubmitOutcome::Dropped`, which requires ALL THREE
//! tiers to fail at once (host out of RAM AND out of disk AND `data/dlq/`
//! unwritable). This test proves, with deterministic counts (not a sampled
//! metric), that:
//!
//!   1. the full ~99K midnight burst fits inside the real 200K ring with
//!      ZERO overflow and ZERO drop;
//!   2. when the ring is deliberately undersized, the ring→spill cascade
//!      absorbs every overflowing seal with ZERO drop;
//!   3. conservation holds — every submitted seal is accounted for in
//!      exactly one tier (buffered + spilled + dlq + dropped == N).
//!
//! Honest envelope: this proves bounded zero-loss INSIDE the tested
//! envelope (RAM ring + a writable spill/dlq dir). The tier-3-fails →
//! `Dropped` case (disk-full + DLQ-unwritable) is the AGGREGATOR-DROP-01
//! Critical path, exercised separately by PR-7's compound-failure chaos.

use std::path::PathBuf;

use tickvault_common::feed::Feed;
use tickvault_storage::seal_absorption::{SealAbsorptionPipeline, SubmitOutcome};
use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

/// Realistic IST-midnight burst magnitude: ~11K instruments × 9 active TFs
/// ≈ 99K seals in one batch. `SEAL_BUFFER_CAPACITY = 200_000` gives ~2×
/// headroom, which test 1 verifies directly.
const MIDNIGHT_BURST_SEALS: usize = 99_000;

/// A fixed valid UNIX timestamp (2026-01-01 ~noon UTC). Only used to name
/// the per-day spill/dlq files; the exact value is irrelevant.
const NOW_UNIX_SECS: i64 = 1_767_268_800;

fn temp_pair(tag: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!(
        "tv-pr6-seal-burst-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let spill = base.join("spill");
    let dlq = base.join("dlq");
    (spill, dlq)
}

fn cleanup(spill: &PathBuf, dlq: &PathBuf) {
    if let Some(parent) = spill.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
    let _ = std::fs::remove_dir_all(spill);
    let _ = std::fs::remove_dir_all(dlq);
}

/// Build a distinct-ish seal for index `i` so the burst resembles a real
/// universe spread (the ring is uniqueness-agnostic — pure FIFO capacity —
/// but varied keys keep the test honest about realistic input).
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
    // security_id varies across ~11K instruments; segment IDX_I=0; TF M1.
    let sid = (i % 11_000) as u64 + 1;
    BufferedSeal::new(sid, 0, TfIndex::M1, state, Feed::Dhan)
}

/// (buffered, spilled, dlq, dropped) tally of a stream of outcomes.
#[derive(Default, Debug, Clone, Copy)]
struct Tally {
    buffered: usize,
    spilled: usize,
    dlq: usize,
    dropped: usize,
}

impl Tally {
    fn record(&mut self, outcome: SubmitOutcome) {
        match outcome {
            SubmitOutcome::Buffered => self.buffered += 1,
            SubmitOutcome::Spilled => self.spilled += 1,
            SubmitOutcome::DlqWritten => self.dlq += 1,
            SubmitOutcome::Dropped(_) => self.dropped += 1,
        }
    }

    fn total(&self) -> usize {
        self.buffered + self.spilled + self.dlq + self.dropped
    }
}

/// Test 1 — the full ~99K midnight burst fits inside the REAL 200K ring with
/// zero overflow and zero drop. Proves the `SEAL_BUFFER_CAPACITY` headroom is
/// sufficient for the worst single batch of the day.
#[test]
fn test_midnight_burst_99k_fits_within_ring_cap_zero_drop() {
    let (spill, dlq) = temp_pair("fits");
    // with_dirs_for_test uses the REAL SEAL_BUFFER_CAPACITY (200_000).
    let mut pipeline = SealAbsorptionPipeline::with_dirs_for_test(spill.clone(), dlq.clone());

    let mut tally = Tally::default();
    for i in 0..MIDNIGHT_BURST_SEALS {
        tally.record(pipeline.submit(mk_seal(i), NOW_UNIX_SECS));
    }

    // The operator's #1 invariant: not a single seal lost.
    assert_eq!(
        tally.dropped, 0,
        "zero seals may be dropped in the midnight burst"
    );
    // 99K < 200K cap → everything stays in RAM; nothing even spills.
    assert_eq!(
        tally.spilled, 0,
        "99K burst must fit in the 200K ring without spilling"
    );
    assert_eq!(
        tally.dlq, 0,
        "no DLQ fallback needed when the ring has headroom"
    );
    assert_eq!(
        tally.buffered, MIDNIGHT_BURST_SEALS,
        "every seal must be buffered in-memory"
    );
    assert_eq!(
        pipeline.ring_len(),
        MIDNIGHT_BURST_SEALS,
        "ring must hold the whole burst"
    );
    // Conservation: every submitted seal accounted for in exactly one tier.
    assert_eq!(tally.total(), MIDNIGHT_BURST_SEALS);

    cleanup(&spill, &dlq);
}

/// Test 2 — when the ring is deliberately undersized, the ring→spill cascade
/// absorbs every overflowing seal with ZERO drop. Proves tier-2 catches what
/// tier-1 cannot hold (the real-world case where the burst exceeds the ring).
#[test]
fn test_midnight_burst_overflow_cascades_to_spill_zero_drop() {
    const SMALL_RING: usize = 1_000;
    const BURST: usize = 6_000; // 5_000 overflow into the spill tier

    let (spill, dlq) = temp_pair("cascade");
    let mut pipeline = SealAbsorptionPipeline::with_capacity_and_dirs_for_test(
        SMALL_RING,
        spill.clone(),
        dlq.clone(),
    );

    let mut tally = Tally::default();
    for i in 0..BURST {
        tally.record(pipeline.submit(mk_seal(i), NOW_UNIX_SECS));
    }

    // Zero loss is the headline guarantee even when the ring overflows.
    assert_eq!(
        tally.dropped, 0,
        "spill tier must catch every ring overflow — zero drop"
    );
    // The cascade was actually exercised (not a no-op pass).
    assert!(
        tally.spilled > 0,
        "undersized ring MUST force overflow into the spill tier"
    );
    // First SMALL_RING submits buffer; the rest evict-oldest → spill.
    assert_eq!(tally.buffered, SMALL_RING, "ring fills to its capacity");
    assert_eq!(
        tally.spilled,
        BURST - SMALL_RING,
        "every overflow seal lands in the spill tier"
    );
    assert_eq!(tally.dlq, 0, "writable spill dir → DLQ tier not needed");
    assert_eq!(pipeline.ring_len(), SMALL_RING, "ring stays at capacity");
    // Conservation across tiers.
    assert_eq!(tally.total(), BURST);

    cleanup(&spill, &dlq);
}

/// Test 3 — conservation invariant under the full real-ring burst: every
/// single submitted seal is accounted for in exactly one tier, with the
/// loss tier empty. A standalone guard so a future refactor that silently
/// drops a seal (or double-counts) fails the build.
#[test]
fn test_midnight_burst_conservation_every_seal_accounted_for() {
    let (spill, dlq) = temp_pair("conservation");
    let mut pipeline = SealAbsorptionPipeline::with_dirs_for_test(spill.clone(), dlq.clone());

    let mut tally = Tally::default();
    for i in 0..MIDNIGHT_BURST_SEALS {
        tally.record(pipeline.submit(mk_seal(i), NOW_UNIX_SECS));
    }

    assert_eq!(
        tally.total(),
        MIDNIGHT_BURST_SEALS,
        "every submitted seal must be accounted for in exactly one tier"
    );
    assert_eq!(
        tally.dropped, 0,
        "the loss tier must be empty inside the envelope"
    );

    cleanup(&spill, &dlq);
}
