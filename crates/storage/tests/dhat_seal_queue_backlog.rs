//! DHAT zero-allocation ratchet for the sealed-candle hand-off queue under a
//! backlog (audit N2, 2026-10-04).
//!
//! The frame drain hands every sealed candle to the seal writer with
//! `SealSender::try_send`. The queue used to be a tokio `mpsc`, which grows
//! its buffer in 32-slot blocks as items queue: whenever the writer fell
//! behind (a slow database, the midnight seal burst), the drain allocated on
//! its own task, and no DHAT test saw it because none queued a backlog. The
//! queue is now a pre-sized `crossbeam_channel` array, allocated once when
//! the runner is built.
//!
//! This test fills the REAL runner's queue to capacity with no consumer, then
//! keeps sending into the full queue (each send refused and handed back), and
//! pins the whole window at zero allocations. A second phase runs the same
//! backlog through a tokio `mpsc` and asserts it DOES allocate, so the first
//! phase is measuring a case that matters rather than one that cannot fail.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::path::PathBuf;

use tickvault_common::feed::Feed;
use tickvault_storage::seal_writer_runner::{SealTrySendError, SealWriterRunner};
use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

mod dhat_support;

const BUDGET_BYTES: u64 = 0;
const BUDGET_BLOCKS: u64 = 0;
/// Seals the queue holds; the backlog fills it exactly.
const QUEUE_CAPACITY: usize = 20_000;
/// Sends into the already-full queue, each refused.
const REFUSED_SENDS: usize = 1_000;
/// One runner per measurement attempt, so a re-attempt starts on an empty
/// queue (`dhat_support` retries up to three times).
const ATTEMPTS: usize = 3;

fn seal(sid: u64) -> BufferedSeal {
    let mut state = LiveCandleState::empty();
    state.bucket_start_ist_secs = 1_780_000_000;
    state.open = 100.0;
    state.high = 105.0;
    state.low = 99.0;
    state.close = 101.0;
    state.volume = 1_234;
    state.tick_count = 5;
    BufferedSeal::new(sid, 2, TfIndex::M1, state, Feed::Dhan)
}

fn temp_dir(tag: &str, n: usize) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tv-dhat-seal-queue-{tag}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn dhat_seal_queue_backlog_never_allocates_where_the_tokio_queue_did() {
    // Every runner is built BEFORE the profiler window: building is where the
    // queue allocates its slots, once.
    let runners: Vec<SealWriterRunner> = (0..ATTEMPTS)
        .map(|n| {
            SealWriterRunner::for_test(
                temp_dir("spill", n),
                temp_dir("dlq", n),
                16,
                QUEUE_CAPACITY,
                16,
            )
        })
        .collect();
    let senders: Vec<_> = runners.iter().map(SealWriterRunner::sender).collect();
    let tokio_queues: Vec<_> = (0..ATTEMPTS)
        .map(|_| tokio::sync::mpsc::channel::<BufferedSeal>(QUEUE_CAPACITY))
        .collect();

    let _profiler = dhat::Profiler::builder().testing().build();

    // Phase 1: the real queue, filled and then over-filled, allocates nothing.
    let attempt = std::cell::Cell::new(0usize);
    let mut accepted = 0usize;
    let mut refused = 0usize;
    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || attempt.set(attempt.get() + 1),
        || {
            let tx = &senders[attempt.get().min(ATTEMPTS - 1)];
            accepted = 0;
            refused = 0;
            for i in 0..(QUEUE_CAPACITY + REFUSED_SENDS) {
                match tx.try_send(seal(i as u64)) {
                    Ok(()) => accepted += 1,
                    Err(SealTrySendError::Full(back)) => {
                        std::hint::black_box(back);
                        refused += 1;
                    }
                    Err(SealTrySendError::Disconnected(_)) => {}
                }
            }
        },
    );
    assert!(
        bytes == BUDGET_BYTES && blocks == BUDGET_BLOCKS,
        "the seal hand-off queue allocated {bytes} bytes / {blocks} blocks while a backlog \
         of {QUEUE_CAPACITY} seals queued and {REFUSED_SENDS} more were refused. The frame \
         drain sends on it for every sealed candle; it must stay pre-sized."
    );
    assert_eq!(accepted, QUEUE_CAPACITY, "the backlog filled the queue");
    assert_eq!(
        refused, REFUSED_SENDS,
        "every send past capacity was refused"
    );

    // Phase 2 (the case this guards against): the same backlog through a tokio
    // mpsc allocates as it grows.
    let tokio_attempt = std::cell::Cell::new(0usize);
    let (_, tokio_blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || tokio_attempt.set(tokio_attempt.get() + 1),
        || {
            let (tx, _rx) = &tokio_queues[tokio_attempt.get().min(ATTEMPTS - 1)];
            for i in 0..QUEUE_CAPACITY {
                let _ = tx.try_send(seal(i as u64));
            }
        },
    );
    assert!(
        tokio_blocks > 0,
        "a tokio mpsc backlog of {QUEUE_CAPACITY} seals allocated nothing, so phase 1 \
         would not be measuring the case it exists for"
    );

    drop(runners);
    for n in 0..ATTEMPTS {
        let _ = std::fs::remove_dir_all(temp_dir("spill", n));
        let _ = std::fs::remove_dir_all(temp_dir("dlq", n));
    }
}
