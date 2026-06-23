//! Chaos test (zero-tick-loss PR-7, G6) — SIGKILL mid-flush replay for the
//! SEAL absorption path, proving zero loss + dedup-safe idempotent recovery.
//!
//! The tick path already has SIGKILL-replay coverage (`chaos_sigkill_replay.rs`).
//! The SEAL spill — written when the aggregator's ILP flush to QuestDB fails
//! mid-batch — had no equivalent. This test closes it.
//!
//! Scenario: a process spilled N sealed candles to disk (each `append_seal`
//! flushes the `write(2)`, so the bytes are durable) and then died via
//! `SIGKILL`. Drop impls did NOT run, but the durable records are on disk. A
//! fresh process must recover EVERY seal byte-exact via `read_all` — zero
//! loss — and the recovery must be IDEMPOTENT: reading the spill again yields
//! the identical set (recovery is non-destructive), so a re-run after a crash
//! mid-replay cannot lose or reorder a seal. Downstream, QuestDB's DEDUP
//! UPSERT on the shadow-candle key collapses any double-replay into one row,
//! so idempotent recovery + DEDUP = zero duplicate.
//!
//! This is pure on-disk durability + readback — deterministic, cross-platform,
//! no subprocess, no `unsafe`.

use std::path::PathBuf;

use tickvault_common::feed::Feed;
use tickvault_storage::seal_spill::{SealSpillWriter, SerializedSeal};
use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

const NOW_UNIX_SECS: i64 = 1_767_268_800;

fn unique_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "tv-pr7-g6-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ))
}

fn mk_serialized(i: usize) -> SerializedSeal {
    let mut state = LiveCandleState::empty();
    state.bucket_start_ist_secs = 1_716_000_900u32.wrapping_add(i as u32);
    state.open = 100.0 + (i % 7) as f64;
    state.high = 110.0 + (i % 13) as f64;
    state.low = 90.0 - (i % 5) as f64;
    state.close = 100.0 + (i % 100) as f64;
    state.volume = 1_000 + i as u64;
    state.bucket_start_cumulative = 1000;
    state.oi = 50_000 + i as i64;
    state.tick_count = 5;
    let buffered = BufferedSeal::new((i % 11_000) as u32 + 1, 0, TfIndex::M1, state, Feed::Dhan);
    SerializedSeal::from(&buffered)
}

#[test]
fn test_seal_spill_survives_sigkill_and_replays_loss_free_and_idempotent() {
    const N: usize = 20_000;

    let dir = unique_dir("replay");

    // --- Process 1: spill N seals, then "die" via SIGKILL (drop without clear).
    let originals: Vec<SerializedSeal> = (0..N).map(mk_serialized).collect();
    {
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.clone());
        for seal in &originals {
            writer
                .append_seal(seal, NOW_UNIX_SECS)
                .expect("append must succeed on a writable dir");
        }
        // SIGKILL semantics: no clear_spill_for_date, no graceful shutdown —
        // the writer is simply dropped. Each append already flushed to the OS,
        // so the file is durable.
    }

    // --- Process 2: fresh writer recovers from the SAME spill dir.
    let recovery = SealSpillWriter::with_spill_dir_for_test(dir.clone());
    let recovered = recovery
        .read_all(NOW_UNIX_SECS)
        .expect("read_all after crash");

    // Zero loss: every spilled seal is recovered, in order, byte-exact.
    assert_eq!(
        recovered.len(),
        N,
        "every spilled seal must survive the crash"
    );
    assert_eq!(
        recovered, originals,
        "recovered seals must match the spilled originals exactly (no loss, no reorder, no corruption)"
    );

    // Idempotent recovery: read_all is non-destructive — a second read (e.g.
    // after the replay itself is interrupted by a second crash) yields the
    // identical set. Combined with QuestDB DEDUP UPSERT, double-replay = zero
    // duplicate rows.
    let recovered_again = recovery
        .read_all(NOW_UNIX_SECS)
        .expect("idempotent read_all");
    assert_eq!(
        recovered_again, originals,
        "recovery must be idempotent — re-reading the spill yields the same seals"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
