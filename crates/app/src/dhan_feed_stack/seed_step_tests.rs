//! Plan item DT-A1 (2026-10-28): the late-attach seed queue
//! (`LiveIngest::queue_seed_batch`, `LiveIngest::step_idle_work`,
//! `SEED_STEP_INSTRUMENTS`, `SEED_QUEUE_BATCHES`). Step boundaries, FIFO
//! order across batches, detector refusals, ticks folded between steps, the
//! queue-room transition, seed time, catch-up priority and the silence scan
//! on a part-seeded batch. A sibling file, to keep `dhan_feed_stack.rs` under
//! its size pin.

use super::*;
use proptest::prelude::*;
use std::collections::VecDeque;
use tickvault_common::feed::Feed;
use tickvault_core::pipeline::tick_gap_detector::{DEFAULT_SILENCE_FLOOR_MILLIS, SilenceReport};

/// An exchange time inside a session (epoch seconds), as `catch_up_book`
/// in the main test module uses.
const TS: u32 = 1_779_355_000;

fn ingest_with_detector(capacity: usize) -> LiveIngest {
    LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4).with_detector_capacity(capacity)
}

/// `n` distinct instruments `first..first + n` on `segment`.
fn batch(first: u64, n: usize, segment: ExchangeSegment) -> Vec<SubscribeInstrument> {
    (0..n as u64)
        .map(|i| SubscribeInstrument {
            security_id: first + i,
            segment,
        })
        .collect()
}

/// One IDX_I ticker packet.
fn ticker_packet(security_id: u32, ltp: f32, ltt: u32) -> [u8; 16] {
    let mut p = [0u8; 16];
    p[0] = 2; // ticker
    p[1] = 16;
    p[3] = 0; // IDX_I
    p[4..8].copy_from_slice(&security_id.to_le_bytes());
    p[8..12].copy_from_slice(&ltp.to_le_bytes());
    p[12..16].copy_from_slice(&ltt.to_le_bytes());
    p
}

/// Folds one IDX_I ticker tick for `security_id` received at `recv_millis`.
fn tick(ingest: &mut LiveIngest, security_id: u32, ltt: u32, recv_millis: u64, seq: u64) {
    let packet = ticker_packet(security_id, 100.0, ltt);
    let ParsedFrame::Tick(t) =
        dispatch_frame(&packet, i64::from(ltt) * 1_000_000_000).expect("parse")
    else {
        panic!("expected a tick");
    };
    let _ = ingest.ingest_tick(&t, seq, recv_millis);
}

/// The detector's report for one key at `now`.
fn report(ingest: &LiveIngest, key: (u64, ExchangeSegment), now: u64) -> Option<SilenceReport> {
    let mut out = None;
    ingest.detector.scan_silence(now, |r| {
        if r.key == key {
            out = Some(r);
        }
    });
    out
}

/// Steps the idle work until nothing is pending; returns every seed_done in
/// order and the step count.
fn drain_seed(ingest: &mut LiveIngest) -> (Vec<SeedBatchDone>, usize) {
    let mut done = Vec::new();
    let mut steps = 0usize;
    while ingest.idle_work_pending() {
        steps += 1;
        assert!(steps < 100_000, "the seed must finish");
        if let Some(d) = ingest.step_idle_work().seed_done {
            done.push(d);
        }
    }
    (done, steps)
}

/// Steps a batch of `n` costs: one even when empty.
fn steps_for(n: usize) -> usize {
    n.div_ceil(SEED_STEP_INSTRUMENTS).max(1)
}

/// Every batch size 0..=1537 (each residue mod 512 three times): the step
/// count is `max(1, ceil(n / 512))` (an empty batch costs one step, never
/// zero), each step seeds exactly `min(512, left)`, the batch reports once on
/// its last step, and the idle arm sleeps right after it.
#[test]
fn test_step_idle_work_seed_steps_at_every_batch_size_boundary() {
    assert_eq!(SEED_STEP_INSTRUMENTS, 512, "the enumeration covers 3 × 512");
    let max = SEED_STEP_INSTRUMENTS * 3 + 1;
    for n in 0..=max {
        let mut ingest = ingest_with_detector(max + 1);
        assert_eq!(
            ingest.queue_seed_batch(batch(40_000, n, ExchangeSegment::NseFno), 1_000),
            None,
            "n={n}: queued, not inline"
        );
        let mut steps = 0usize;
        let mut seeded = 0usize;
        loop {
            let before = ingest.tracked_instruments();
            let step = ingest.step_idle_work();
            steps += 1;
            let delta = ingest.tracked_instruments() - before;
            assert_eq!(
                delta,
                SEED_STEP_INSTRUMENTS.min(n - seeded),
                "n={n} step {steps}"
            );
            seeded += delta;
            assert!(step.catch_up_done.is_none(), "n={n}: no sweep queued");
            if seeded == n && step.seed_done.is_some() {
                assert_eq!(step.seed_done, Some((n, n)), "n={n}");
                assert!(
                    !ingest.idle_work_pending(),
                    "n={n}: nothing pending right after the last step"
                );
                break;
            }
            assert_eq!(step.seed_done, None, "n={n}: only the last step reports");
            assert!(ingest.idle_work_pending(), "n={n} step {steps}");
            assert!(steps <= steps_for(n), "n={n}: an extra empty step");
        }
        assert_eq!(steps, steps_for(n), "n={n}");
        assert_eq!(ingest.tracked_instruments(), n);
        assert_eq!(ingest.detector_refused(), 0);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Several queued batches of different sizes and disjoint ids: each
    /// reports its own `(n, n)` oldest first, the step count is the sum of
    /// each batch's own, and no step carries into the next batch (the
    /// detector count after every step is exactly the model's).
    #[test]
    fn proptest_step_seed_reports_each_batch_fifo_without_carry_over(
        sizes in prop::collection::vec(0usize..=1600, 1..=SEED_QUEUE_BATCHES),
    ) {
        let mut ingest = ingest_with_detector(SEED_QUEUE_BATCHES * 1601);
        for (b, &n) in sizes.iter().enumerate() {
            let first = 100_000 + (b as u64) * 10_000;
            prop_assert_eq!(
                ingest.queue_seed_batch(batch(first, n, ExchangeSegment::NseFno), 1_000),
                None
            );
        }
        let mut done = Vec::new();
        let mut steps = 0usize;
        let mut front = 0usize; // index of the batch at the front
        let mut seeded_in_front = 0usize;
        let mut finished_total = 0usize;
        while ingest.idle_work_pending() {
            let before = ingest.tracked_instruments();
            let step = ingest.step_idle_work();
            steps += 1;
            let delta = ingest.tracked_instruments() - before;
            let n = sizes[front];
            let want = SEED_STEP_INSTRUMENTS.min(n - seeded_in_front);
            prop_assert_eq!(delta, want, "step {} of batch {}", steps, front);
            seeded_in_front += delta;
            if seeded_in_front == n {
                prop_assert_eq!(step.seed_done, Some((n, n)));
                done.push((n, n));
                finished_total += n;
                front += 1;
                seeded_in_front = 0;
            } else {
                prop_assert_eq!(step.seed_done, None);
            }
            prop_assert_eq!(
                ingest.tracked_instruments(),
                finished_total + seeded_in_front,
                "the tracked count is exactly the finished batches plus the front's part"
            );
        }
        let want: Vec<SeedBatchDone> = sizes.iter().map(|&n| (n, n)).collect();
        prop_assert_eq!(done, want);
        prop_assert_eq!(steps, sizes.iter().map(|&n| steps_for(n)).sum::<usize>());
        prop_assert_eq!(ingest.detector_refused(), 0);
    }
}

/// A detector smaller than the batch: `(requested, seeded)` reports the
/// acceptances, the batch keeps stepping past the first refusal, the next
/// batch still pops, and `detector_refused` is Σ(requested − seeded). The
/// same tuples through the full-queue inline path report the same.
#[test]
fn test_step_seed_and_inline_path_report_detector_refusals() {
    // (capacity, batch sizes, expected seed_done per batch, steps)
    let cases: [(usize, &[usize], &[SeedBatchDone], usize); 4] = [
        (700, &[1_000], &[(1_000, 700)], 2),
        (512, &[512, 1], &[(512, 512), (1, 0)], 2),
        (1_024, &[513, 513], &[(513, 513), (513, 511)], 4),
        (1, &[0, 3, 0], &[(0, 0), (3, 1), (0, 0)], 3),
    ];
    for (cap, sizes, want, want_steps) in cases {
        // Queued path.
        let mut ingest = ingest_with_detector(cap);
        for (b, &n) in sizes.iter().enumerate() {
            let first = 200_000 + (b as u64) * 10_000;
            assert_eq!(
                ingest.queue_seed_batch(batch(first, n, ExchangeSegment::NseFno), 1_000),
                None
            );
        }
        let (done, steps) = drain_seed(&mut ingest);
        assert_eq!(done, want, "cap={cap} sizes={sizes:?}");
        assert_eq!(steps, want_steps, "cap={cap} sizes={sizes:?}");
        let lost: usize = want.iter().map(|(r, s)| r - s).sum();
        assert_eq!(ingest.detector_refused(), lost as u64, "cap={cap}");
        assert_eq!(ingest.tracked_instruments(), cap.min(sizes.iter().sum()));

        // Inline path: the queue is full of empty batches first.
        let mut ingest = ingest_with_detector(cap);
        for _ in 0..SEED_QUEUE_BATCHES {
            assert_eq!(ingest.queue_seed_batch(Vec::new(), 1_000), None);
        }
        assert!(!ingest.seed_queue_has_room());
        for ((b, &n), &expect) in sizes.iter().enumerate().zip(want) {
            let first = 200_000 + (b as u64) * 10_000;
            assert_eq!(
                ingest.queue_seed_batch(batch(first, n, ExchangeSegment::NseFno), 1_000),
                Some(expect),
                "inline cap={cap} batch {b}"
            );
        }
        assert_eq!(ingest.detector_refused(), lost as u64, "inline cap={cap}");
        let (empties, _) = drain_seed(&mut ingest);
        assert_eq!(empties, vec![(0, 0); SEED_QUEUE_BATCHES]);
    }
}

/// A tick for X folded before, after, or both before and after the step
/// that seeds X: X takes one slot, still counts as seeded, and the seed never
/// resets its ticked state or rewinds its last-seen time.
#[test]
fn test_step_seed_never_resets_an_instrument_that_ticked_between_steps() {
    const N: usize = 1_100;
    const FIRST: u64 = 10_000;
    const SEED_AT: u64 = 1_000;
    const TICK_AT: u64 = 50_000;
    const NOW: u64 = 150_000;
    #[derive(Debug, Clone, Copy)]
    enum When {
        Before,
        After,
        Both,
    }
    for pos in [0usize, 511, 512, 513, 1_099] {
        for when in [When::Before, When::After, When::Both] {
            let mut ingest = ingest_with_detector(AGGREGATOR_MAX_SLOTS);
            let x = FIRST + pos as u64;
            let x_sid = u32::try_from(x).expect("fits");
            assert_eq!(
                ingest.queue_seed_batch(batch(FIRST, N, ExchangeSegment::IdxI), SEED_AT),
                None
            );
            let x_step = pos / SEED_STEP_INSTRUMENTS + 1;
            let mut last_tick = 0u64;
            if matches!(when, When::Before | When::Both) {
                tick(&mut ingest, x_sid, TS, TICK_AT, 7);
                last_tick = TICK_AT;
            }
            let mut done = None;
            for _ in 0..x_step {
                if let Some(d) = ingest.step_idle_work().seed_done {
                    done = Some(d);
                }
            }
            if matches!(when, When::After | When::Both) {
                tick(&mut ingest, x_sid, TS + 1, TICK_AT + 1_000, 8);
                last_tick = TICK_AT + 1_000;
            }
            let (rest, _) = drain_seed(&mut ingest);
            let done = done.or(rest.first().copied());
            assert_eq!(done, Some((N, N)), "pos={pos} {when:?}");
            assert_eq!(ingest.tracked_instruments(), N, "pos={pos} {when:?}");
            assert_eq!(ingest.detector_refused(), 0, "pos={pos} {when:?}");

            let r = report(&ingest, (x, ExchangeSegment::IdxI), NOW).expect("X tracked");
            assert_ne!(
                r.verdict,
                tickvault_core::pipeline::tick_gap_detector::SilenceVerdict::NeverTicked,
                "pos={pos} {when:?}: the seed reset X's tick"
            );
            assert_eq!(
                r.silent_millis,
                NOW - last_tick,
                "pos={pos} {when:?}: last-seen must stay at the tick"
            );
            let samples = u32::from(matches!(when, When::Both));
            assert_eq!(r.samples, samples, "pos={pos} {when:?}");
            assert_eq!(
                ingest.scan_silence(NOW),
                (N as u64 - 1, N as u64 - 1),
                "pos={pos} {when:?}: every other seeded instrument never ticked"
            );
        }
    }
}

/// At a detector exactly the batch size, a tick from an instrument outside
/// the batch takes the slot one batch member needed: that member is refused
/// by ordering alone. Pinned so the shape is visible, not hidden.
#[test]
fn test_step_seed_outside_tick_takes_a_slot_the_batch_needed_at_exact_capacity() {
    const N: usize = 1_100;
    let total_steps = steps_for(N);
    // Tick the outsider before step k (k = total_steps means after the last).
    for before_step in 0..=total_steps {
        let mut ingest = ingest_with_detector(N);
        assert_eq!(
            ingest.queue_seed_batch(batch(10_000, N, ExchangeSegment::IdxI), 1_000),
            None
        );
        let mut done = None;
        for step in 0..total_steps {
            if step == before_step {
                tick(&mut ingest, 99_999, TS, 5_000, 7);
            }
            if let Some(d) = ingest.step_idle_work().seed_done {
                done = Some(d);
            }
        }
        if before_step == total_steps {
            tick(&mut ingest, 99_999, TS, 5_000, 7);
        }
        assert!(!ingest.idle_work_pending());
        let expect = if before_step == total_steps {
            (N, N)
        } else {
            (N, N - 1)
        };
        assert_eq!(done, Some(expect), "outside tick before step {before_step}");
        assert_eq!(ingest.detector_refused(), 1, "before step {before_step}");
        assert_eq!(ingest.tracked_instruments(), N);
    }
}

/// Eight 600-instrument batches (two steps each): `seed_queue_has_room`
/// stays false while the front batch is part-seeded, turns true on exactly
/// the step that finishes a batch, and a ninth batch pushed then is the last
/// one reported. The queue never grows.
#[test]
fn test_seed_queue_has_room_turns_true_on_the_step_that_finishes_a_batch() {
    let mut ingest = ingest_with_detector(AGGREGATOR_MAX_SLOTS);
    let cap = ingest.seed_queue.batches.capacity();
    for b in 0..SEED_QUEUE_BATCHES {
        let first = 300_000 + (b as u64) * 1_000;
        assert_eq!(
            ingest.queue_seed_batch(batch(first, 600, ExchangeSegment::NseFno), 1_000),
            None
        );
    }
    assert!(!ingest.seed_queue_has_room());
    let mut pushed_ninth = false;
    let mut done = Vec::new();
    let mut steps = 0usize;
    while ingest.idle_work_pending() {
        let room_before = ingest.seed_queue_has_room();
        let step = ingest.step_idle_work();
        steps += 1;
        assert_eq!(
            ingest.seed_queue_has_room(),
            room_before || step.seed_done.is_some(),
            "step {steps}: room must appear on the finishing step, not before"
        );
        if steps == 1 {
            assert!(
                !ingest.seed_queue_has_room(),
                "a part-seeded front batch is still queued"
            );
        }
        if let Some(d) = step.seed_done {
            done.push(d);
        }
        if ingest.seed_queue_has_room() && !pushed_ninth {
            assert_eq!(steps, 2, "room appears on the second step");
            assert_eq!(
                ingest.queue_seed_batch(batch(900_000, 7, ExchangeSegment::NseFno), 1_000),
                None,
                "the ninth batch is queued, never inline"
            );
            pushed_ninth = true;
            assert!(!ingest.seed_queue_has_room(), "full again");
        }
        assert_eq!(ingest.seed_queue.batches.capacity(), cap, "never grew");
    }
    let mut want = vec![(600, 600); SEED_QUEUE_BATCHES];
    want.push((7, 7));
    assert_eq!(done, want, "the ninth batch is reported last");
    assert_eq!(steps, SEED_QUEUE_BATCHES * 2 + 1);
}

/// The seed time is the batch's ARRIVAL time, not the step time: a batch
/// queued at T and stepped 10 s later reads never-ticked from T + floor + 1,
/// not from T + 10 s + floor + 1. An instrument that ticked is untouched by
/// any later seed.
#[test]
fn test_queue_seed_batch_seed_time_is_the_arrival_time_not_the_step_time() {
    const T: u64 = 1_000_000;
    let floor = DEFAULT_SILENCE_FLOOR_MILLIS;
    let mut ingest = ingest_with_detector(16);
    assert_eq!(
        ingest.queue_seed_batch(batch(13, 1, ExchangeSegment::IdxI), T),
        None
    );
    // The step happens "10 s later": steps take no clock, so the seed time
    // can only be the batch's own stamp.
    let (done, _) = drain_seed(&mut ingest);
    assert_eq!(done, vec![(1, 1)]);
    assert_eq!(ingest.scan_silence(T + floor), (0, 0));
    assert_eq!(ingest.scan_silence(T + floor + 1), (1, 1));
    assert_eq!(ingest.scan_silence(T + 10_000 + floor), (1, 1));

    // Ticked between an inline seed and a queued seed: both seeds leave it.
    let mut ingest = ingest_with_detector(16);
    assert_eq!(
        ingest.queue_seed_batch(batch(13, 1, ExchangeSegment::IdxI), 1_000),
        None
    );
    for _ in 1..SEED_QUEUE_BATCHES {
        assert_eq!(ingest.queue_seed_batch(Vec::new(), 1_000), None);
    }
    assert_eq!(
        ingest.queue_seed_batch(batch(13, 1, ExchangeSegment::IdxI), 2_000),
        Some((1, 1)),
        "inline seed on a full queue"
    );
    tick(&mut ingest, 13, TS, 3_000, 7);
    let _ = drain_seed(&mut ingest);
    let r = report(&ingest, (13, ExchangeSegment::IdxI), 10_000).expect("tracked");
    assert_eq!(r.silent_millis, 7_000, "last-seen stays at the tick");
}

/// BUG (DT-A1 ordering): X sits in a queued batch stamped 1000; a call on a
/// full queue seeds X inline at 2000; the queued step later re-seeds X at
/// 1000, rewinding its last-seen time while it has not ticked. The silence
/// scan then judges X silent up to one queue-depth of time early (a false
/// RISK-GAP-03 episode). Before DT-A1 seeds ran in arrival order.
#[test]
fn test_queue_seed_batch_inline_then_queued_seed_never_rewinds_seed_time() {
    let mut ingest = ingest_with_detector(16);
    assert_eq!(
        ingest.queue_seed_batch(batch(13, 1, ExchangeSegment::IdxI), 1_000),
        None
    );
    for _ in 1..SEED_QUEUE_BATCHES {
        assert_eq!(ingest.queue_seed_batch(Vec::new(), 1_000), None);
    }
    assert_eq!(
        ingest.queue_seed_batch(batch(13, 1, ExchangeSegment::IdxI), 2_000),
        Some((1, 1))
    );
    let _ = drain_seed(&mut ingest);
    let r = report(&ingest, (13, ExchangeSegment::IdxI), 10_000).expect("tracked");
    assert_eq!(
        r.silent_millis, 8_000,
        "last-seen must be max(1000, 2000) = 2000, the newer seed"
    );
}

/// Folds one IDX_I ticker per instrument at `TS`, then one far later on a
/// separate instrument, so the catch-up cutoff clears every early bucket
/// (the main test module's `catch_up_book`, repeated here).
fn catch_up_book(instruments: u32) -> LiveIngest {
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), instruments as usize + 1);
    for sid in 1..=instruments {
        tick(
            &mut ingest,
            1_000 + sid,
            TS,
            u64::from(TS) * 1_000,
            u64::from(sid),
        );
    }
    let later = TS + CATCHUP_LATENESS_MARGIN_SECS + 180;
    tick(&mut ingest, 999, later, u64::from(later) * 1_000, 7);
    ingest
}

/// A catch-up fire while a 22,000-instrument seed is still stepping: the
/// overrun is counted once, the sweep keeps cursor 0 and takes the newer
/// cutoff, every seed step runs first, and the sweep then seals exactly the
/// bars a whole sweep at the newer cutoff seals.
#[test]
fn test_step_idle_work_catch_up_overrun_while_seeding_keeps_cursor_and_loses_no_bar() {
    let instruments = (CATCHUP_SEAL_STEP_SLOTS * 2 + 3) as u32;
    let even_later = TS + CATCHUP_LATENESS_MARGIN_SECS + 900;

    let mut twin = catch_up_book(instruments);
    tick(&mut twin, 999, even_later, u64::from(even_later) * 1_000, 9);
    let (e, d) = twin.catch_up_seal();
    assert!(e + d >= u64::from(instruments), "the fixture seals bars");

    let mut ingest = catch_up_book(instruments).with_detector_capacity(AGGREGATOR_MAX_SLOTS);
    ingest.begin_catch_up_seal();
    let old_cutoff = ingest.catch_up.expect("sweep queued").cutoff;
    assert_eq!(ingest.queue_seed_batch(seed_contracts(22_000), 1_000), None);
    for i in 0..20 {
        let step = ingest.step_idle_work();
        assert!(
            step.catch_up_done.is_none() && step.seed_done.is_none(),
            "{i}"
        );
    }
    tick(
        &mut ingest,
        999,
        even_later,
        u64::from(even_later) * 1_000,
        9,
    );
    let new_cutoff = ingest.catch_up_cutoff();
    assert!(new_cutoff > old_cutoff);
    ingest.begin_catch_up_seal();
    assert_eq!(
        ingest.catch_up_overruns, 1,
        "one fire found the sweep running"
    );
    let sweep = ingest.catch_up.expect("still queued");
    assert_eq!(sweep.next_slot, 0, "seeding ran no sweep step");
    assert_eq!(sweep.cutoff, new_cutoff, "the sweep took the newer cutoff");

    let mut steps = 20usize;
    let mut seed_done_at = None;
    let mut sweep_done = None;
    while ingest.idle_work_pending() {
        let step = ingest.step_idle_work();
        steps += 1;
        if step.seed_done.is_some() {
            assert_eq!(step.seed_done, Some((22_000, 22_000)));
            seed_done_at = Some(steps);
        }
        if let Some(done) = step.catch_up_done {
            assert!(seed_done_at.is_some(), "the sweep finishes after the seed");
            sweep_done = Some(done);
        }
    }
    assert_eq!(seed_done_at, Some(steps_for(22_000)));
    let slots = ingest.aggregator.len();
    assert_eq!(
        steps,
        steps_for(22_000) + slots.div_ceil(CATCHUP_SEAL_STEP_SLOTS)
    );
    let (se, sd) = sweep_done.expect("the sweep finished");
    assert_eq!(se + sd, e + d, "no bar lost to the overrun");
}

fn seed_contracts(n: usize) -> Vec<SubscribeInstrument> {
    batch(40_000, n, ExchangeSegment::NseFno)
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Push(usize),
    Fire,
    Step,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0usize..=1_200).prop_map(Op::Push),
        Just(Op::Fire),
        Just(Op::Step),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Any interleaving of pushes, catch-up fires and idle steps: a seed step
    /// always runs before a sweep step, each batch reports `(n, n)` in push
    /// order, and once pushing stops the sweep is gone within the remaining
    /// seed steps plus one sweep's own steps.
    #[test]
    fn proptest_step_idle_work_seed_priority_never_holds_the_sweep_past_its_bound(
        ops in prop::collection::vec(op(), 0..=500),
    ) {
        let mut ingest = catch_up_book((CATCHUP_SEAL_STEP_SLOTS + 7) as u32)
            .with_detector_capacity(AGGREGATOR_MAX_SLOTS);
        // Model: (size, seeded so far) per queued batch.
        let mut model: VecDeque<(usize, usize)> = VecDeque::new();
        let step_model = |model: &mut VecDeque<(usize, usize)>| -> Option<SeedBatchDone> {
            let (n, s) = model.front_mut()?;
            *s += SEED_STEP_INSTRUMENTS.min(*n - *s);
            if *s == *n {
                let n = *n;
                model.pop_front();
                return Some((n, n));
            }
            None
        };
        for op in ops {
            match op {
                Op::Push(n) => {
                    // Same ids every batch: repeats are accepted no-ops, so
                    // the detector never runs out of room.
                    let got = ingest.queue_seed_batch(seed_contracts(n), 1_000);
                    if model.len() < SEED_QUEUE_BATCHES {
                        prop_assert_eq!(got, None);
                        model.push_back((n, 0));
                    } else {
                        prop_assert_eq!(got, Some((n, n)), "inline on a full queue");
                    }
                    prop_assert_eq!(ingest.seed_queue_has_room(), model.len() < SEED_QUEUE_BATCHES);
                }
                Op::Fire => ingest.begin_catch_up_seal(),
                Op::Step => {
                    let seeding = !model.is_empty();
                    let step = ingest.step_idle_work();
                    prop_assert_eq!(step.seed_done, step_model(&mut model));
                    if seeding {
                        prop_assert!(step.catch_up_done.is_none(), "seed first");
                    }
                }
            }
        }
        let seed_steps: usize = model
            .iter()
            .map(|&(n, s)| (n - s).div_ceil(SEED_STEP_INSTRUMENTS).max(1))
            .sum();
        let sweep_steps = ingest.aggregator.len().div_ceil(CATCHUP_SEAL_STEP_SLOTS);
        let mut steps = 0usize;
        while ingest.idle_work_pending() {
            let seeding = !model.is_empty();
            let step = ingest.step_idle_work();
            prop_assert_eq!(step.seed_done, step_model(&mut model));
            if seeding {
                prop_assert!(step.catch_up_done.is_none());
            }
            steps += 1;
            prop_assert!(steps <= seed_steps + sweep_steps, "{} > {} + {}", steps, seed_steps, sweep_steps);
        }
        prop_assert!(ingest.catch_up.is_none());
        prop_assert!(model.is_empty());
    }
}

/// The stated honest limit: a 30 s scan between seed steps sees only the
/// instruments seeded so far (never more), and the first scan after the
/// batch finishes sees all of them.
#[test]
fn test_scan_silence_on_a_part_seeded_batch_counts_only_seeded_instruments() {
    const N: usize = 1_500;
    const T: u64 = 1_000_000;
    let at = T + DEFAULT_SILENCE_FLOOR_MILLIS + 1;
    for (steps_before, want) in [(0usize, 0u64), (1, 512), (2, 1_024), (3, 1_500)] {
        let mut ingest = ingest_with_detector(N + 1);
        assert_eq!(
            ingest.queue_seed_batch(batch(500_000, N, ExchangeSegment::NseFno), T),
            None
        );
        for _ in 0..steps_before {
            let _ = ingest.step_idle_work();
        }
        assert_eq!(
            ingest.scan_silence(at),
            (want, want),
            "{steps_before} steps before the scan"
        );
        let _ = drain_seed(&mut ingest);
        assert_eq!(
            ingest.scan_silence(at),
            (N as u64, N as u64),
            "after the batch finishes every instrument is counted"
        );
    }
}

/// The idle arm's guard is `idle_work_pending()` alone, never the seed
/// latch: after the attach task drops its sender, the batches already queued
/// are still stepped to the end. The behavioural half: with no more sends,
/// the idle steps alone seed every queued batch.
#[test]
fn test_idle_arm_keeps_seeding_queued_batches_after_the_seed_channel_closes() {
    let src = include_str!("../dhan_feed_stack.rs");
    let drain = src
        .split_once("async fn run_frame_drain(")
        .expect("the drain exists")
        .1;
    let idle: Vec<&str> = drain
        .lines()
        .filter(|l| l.contains("tokio::task::coop::consume_budget()"))
        .collect();
    let work: Vec<&str> = idle
        .iter()
        .copied()
        .filter(|l| l.contains("idle_work_pending"))
        .collect();
    assert_eq!(work.len(), 1, "one idle-work arm: {idle:?}");
    assert_eq!(
        work[0].trim(),
        "() = tokio::task::coop::consume_budget(), if ingest.idle_work_pending() => {"
    );
    assert!(
        idle.iter().all(|l| !l.contains("seed_closed")),
        "no idle arm may be gated on the seed latch: {idle:?}"
    );

    let mut ingest = ingest_with_detector(AGGREGATOR_MAX_SLOTS);
    let sizes = [1_000usize, 1, 513];
    for (b, &n) in sizes.iter().enumerate() {
        let first = 600_000 + (b as u64) * 10_000;
        assert_eq!(
            ingest.queue_seed_batch(batch(first, n, ExchangeSegment::NseFno), 1_000),
            None
        );
    }
    // Sender dropped here: nothing else arrives; only the idle arm runs.
    let (done, _) = drain_seed(&mut ingest);
    assert_eq!(done, vec![(1_000, 1_000), (1, 1), (513, 513)]);
    assert_eq!(ingest.tracked_instruments(), sizes.iter().sum::<usize>());
}
