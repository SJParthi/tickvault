//! The 30 s silence arm's slot-cell path (2026-11-17) against a frozen copy of
//! the arm as it stood before it (one reconcile hash probe per report).
//!
//! Two identical lanes are driven through the same random seeds, ticks,
//! session resets, subscribe records on up to three sockets, forgets and
//! sweeps at clocks moving both ways; one sweeps with the production
//! `scan_silence_named_at` (slot cells), its twin with the frozen copy below
//! (`scan_silence` + `note`). Every return value, every named instrument,
//! the dead-class latch, every reconcile book field and every round handed to
//! the executor must agree. Both lanes end each sweep at a GENERATED reconcile
//! clock (IST second of day, mostly inside the 09:17-15:15 window, and IST day,
//! moving both ways), never the wall clock, so rounds are reserved whatever
//! time CI runs and the two lanes can never straddle a gate second (2026-11-17
//! review). `test_frozen_oracle_and_slot_arm_agree_after_a_reserved_round` is
//! the deterministic proof that a round is reached.

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::subscribe_reconcile::{RECONCILE_DELTA_SECS, SubscribeReconcile};
    use proptest::prelude::*;
    use std::time::Duration;
    use tickvault_core::pipeline::tick_gap_detector::TickObservation;
    use tickvault_core::websocket::pool_supervisor::LiveSubscriptionCommand;
    use tokio::sync::mpsc;

    /// The arm exactly as it stood on 2026-11-17 before the slot cells
    /// (`5e1bc0158`), comments trimmed. Frozen: do not edit to follow the
    /// production function; it is the oracle. The one edit: the reconcile
    /// ends at the injected clock (`finish_at`) instead of `finish_now`, the
    /// same seam the production arm takes in this test.
    fn scan_silence_named_frozen(
        lane: &LiveIngest,
        now_millis: u64,
        worst: &mut [SilentInstrument; WORST_SILENT_NAMED],
        reconcile_now: Instant,
        reconcile_clock: (u32, i64),
    ) -> (u64, u64, usize) {
        let mut silent = 0u64;
        let mut never = 0u64;
        let mut named = 0usize;
        let mut classes = ClassLiveness::default();
        let mut reconcile = lane
            .subscribe_reconcile
            .as_ref()
            .map(|book| book.sweep_at(reconcile_now));
        lane.detector.scan_silence(now_millis, |report| {
            if let Some(sweep) = reconcile.as_mut() {
                sweep.note(&report);
            }
            classes.observe(
                report.key.1,
                report.sparse,
                report.verdict == SilenceVerdict::NeverTicked,
                report.silent_millis > report.expected_millis,
            );
            if !report.counts_toward_alarm() {
                return;
            }
            if report.silent_millis <= report.expected_millis {
                return;
            }
            silent = silent.saturating_add(1);
            if report.verdict == SilenceVerdict::NeverTicked {
                never = never.saturating_add(1);
            }
            named = rank_silent(
                worst,
                named,
                SilentInstrument {
                    security_id: report.key.0,
                    segment: report.key.1,
                    silent_millis: report.silent_millis,
                    expected_millis: report.expected_millis,
                    never_ticked: report.verdict == SilenceVerdict::NeverTicked,
                },
            );
        });
        if let Some(sweep) = reconcile {
            sweep.finish_at(reconcile_clock.0, reconcile_clock.1);
        }
        metrics::gauge!(INSTRUMENTS_SILENT_GAUGE).set(silent as f64);
        metrics::gauge!(INSTRUMENTS_NEVER_TICKED_GAUGE).set(never as f64);
        let scan_secs = ist_secs_of_day_from_millis(now_millis);
        if dead_class_verdict_is_due(
            is_within_market_hours_ist(scan_secs),
            silence_page_allowed_today(),
        ) || muhurat_silence_check_due_now(scan_secs)
        {
            lane.report_dead_classes(&classes);
        } else {
            lane.stand_down_dead_class_report();
        }
        (silent, never, named)
    }

    const SEGMENTS: [ExchangeSegment; 3] = [
        ExchangeSegment::IdxI,
        ExchangeSegment::NseEquity,
        ExchangeSegment::NseFno,
    ];

    #[derive(Debug, Clone)]
    enum Op {
        Seed {
            id: u64,
            seg: usize,
            at: u64,
        },
        Tick {
            id: u64,
            seg: usize,
            ltt: u32,
            vol: u32,
            at: u64,
        },
        ResetSession,
        Record {
            socket: usize,
            keys: Vec<(u64, usize)>,
            at_ms: u64,
        },
        CloseSocket {
            socket: usize,
        },
        Forget {
            keys: Vec<(u64, usize)>,
        },
        Sweep {
            now: u64,
            at_ms: u64,
            secs: u32,
            day: i64,
        },
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        let keys = || prop::collection::vec((0u64..8, 0usize..3), 0..6);
        prop_oneof![
            3 => (0u64..8, 0usize..3, 0u64..2_000_000)
                .prop_map(|(id, seg, at)| Op::Seed { id, seg, at }),
            6 => (0u64..8, 0usize..3, 0u32..60, 0u32..5_000, 0u64..2_000_000)
                .prop_map(|(id, seg, ltt, vol, at)| Op::Tick { id, seg, ltt, vol, at }),
            1 => Just(Op::ResetSession),
            3 => (0usize..3, keys(), 0u64..900_000)
                .prop_map(|(socket, keys, at_ms)| Op::Record { socket, keys, at_ms }),
            1 => (0usize..3).prop_map(|socket| Op::CloseSocket { socket }),
            1 => keys().prop_map(|keys| Op::Forget { keys }),
            4 => (0u64..2_500_000, 0u64..900_000, secs_strategy(), 0i64..3)
                .prop_map(|(now, at_ms, secs, day)| Op::Sweep {
                    now,
                    at_ms,
                    secs,
                    day: DAY + day,
                }),
        ]
    }

    /// An IST day for the generated reconcile clock.
    const DAY: i64 = 20_000;
    /// 10:00 IST, inside the reconcile window.
    const IN_WINDOW: u32 = 10 * 3_600;

    /// Mostly inside the reconcile window (09:17-15:15 IST), sometimes anywhere
    /// in the day, so both the round and the early-return paths are drawn.
    fn secs_strategy() -> impl Strategy<Value = u32> {
        prop_oneof![3 => 33_420u32..54_900, 1 => 0u32..86_400]
    }

    fn list(keys: &[(u64, usize)]) -> Vec<SubscribeInstrument> {
        keys.iter()
            .map(|(id, seg)| SubscribeInstrument {
                security_id: *id,
                segment: SEGMENTS[*seg],
            })
            .collect()
    }

    fn lane(book: &Arc<SubscribeReconcile>) -> LiveIngest {
        // A small detector, so refusals past capacity are exercised too.
        LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
            .with_detector_capacity(12)
            .with_subscribe_reconcile(Arc::clone(book))
    }

    #[test]
    fn test_with_subscribe_reconcile_attaches_the_book_the_arm_judges() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let mut ingest = lane(&book);
        assert!(
            ingest
                .subscribe_reconcile
                .as_ref()
                .is_some_and(|b| Arc::ptr_eq(b, &book))
        );
        let (tx, _rx) = mpsc::channel(1);
        book.record_subscribed(
            &tx,
            &[SubscribeInstrument {
                security_id: 1,
                segment: ExchangeSegment::NseFno,
            }],
        );
        assert!(ingest.seed(1, ExchangeSegment::NseFno, 1_000));
        let mut worst = [SilentInstrument::EMPTY; WORST_SILENT_NAMED];
        let _ = ingest.scan_silence_named(2_000, &mut worst);
        assert_eq!(book.cached_cells(), 1, "the arm judged the attached book");
    }

    /// The deterministic proof that the oracle comparison reaches a reserved
    /// round (2026-11-17 review): in auto mode an expected instrument that
    /// never ticked, beside a live one on the same socket, is resent at an
    /// in-window clock; both lanes hand over the same round, and the next
    /// sweep (a round reserved, so the slot arm must take the probe) still
    /// agrees and counts no fast-path hit.
    #[test]
    fn test_frozen_oracle_and_slot_arm_agree_after_a_reserved_round() {
        let (book_new, mut rounds_new) = SubscribeReconcile::new(true);
        let (book_old, mut rounds_old) = SubscribeReconcile::new(true);
        let mut new = lane(&book_new);
        let mut old = lane(&book_old);
        let (tx, _rx) = mpsc::channel(4);
        let base = Instant::now();
        let both = list(&[(1, 2), (2, 2)]);
        book_new.record_subscribed_at(&tx, &both, base);
        book_old.record_subscribed_at(&tx, &both, base);
        const SEED_MS: u64 = 1_000_000;
        for ingest in [&mut new, &mut old] {
            assert!(ingest.seed(1, ExchangeSegment::NseFno, SEED_MS));
            assert!(ingest.seed(2, ExchangeSegment::NseFno, SEED_MS));
            for k in 0..9u32 {
                let _ = ingest.detector.observe(TickObservation {
                    key: (1, ExchangeSegment::NseFno),
                    ltt_epoch_secs: 1_786_800_000 + k,
                    volume: 10 + k,
                    open_interest: 0,
                    recv_monotonic_millis: SEED_MS + u64::from(k) * 1_000,
                });
            }
        }
        let now = SEED_MS + 9_000;
        let clock = (IN_WINDOW, DAY);
        for (sweep_n, extra) in [(0u32, 1u64), (1, 2)] {
            let at = base + Duration::from_secs(RECONCILE_DELTA_SECS + extra);
            let mut worst_new = [SilentInstrument::EMPTY; WORST_SILENT_NAMED];
            let mut worst_old = [SilentInstrument::EMPTY; WORST_SILENT_NAMED];
            let got = new.scan_silence_named_at(now, &mut worst_new, at, Some(clock));
            let want = scan_silence_named_frozen(&old, now, &mut worst_old, at, clock);
            assert_eq!(got, want, "sweep {sweep_n}");
            assert_eq!(worst_new, worst_old, "sweep {sweep_n}");
            assert_eq!(book_new.test_fingerprint(), book_old.test_fingerprint());
            book_new.assert_cell_invariant();
            if sweep_n == 0 {
                let a = rounds_new
                    .try_recv()
                    .expect("the slot arm reserved a round");
                let b = rounds_old
                    .try_recv()
                    .expect("the frozen arm reserved a round");
                assert_eq!(a.test_view(), b.test_view());
            }
            assert!(rounds_new.try_recv().is_err() && rounds_old.try_recv().is_err());
        }
        assert_eq!(
            book_new.fast_path_hits(),
            0,
            "a reserved round sends every report down the probe"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn proptest_scan_silence_named_matches_frozen_oracle(
            auto in any::<bool>(),
            ops in prop::collection::vec(op_strategy(), 1..80),
        ) {
            let (book_new, mut rounds_new) = SubscribeReconcile::new(auto);
            let (book_old, mut rounds_old) = SubscribeReconcile::new(auto);
            let mut new = lane(&book_new);
            let mut old = lane(&book_old);
            let base = Instant::now();
            let at = |ms: u64| base + Duration::from_millis(ms);
            let mut sockets: Vec<(
                mpsc::Sender<LiveSubscriptionCommand>,
                Option<mpsc::Receiver<LiveSubscriptionCommand>>,
            )> = (0..3)
                .map(|_| {
                    let (tx, rx) = mpsc::channel(4);
                    (tx, Some(rx))
                })
                .collect();
            for op in ops {
                match op {
                    Op::Seed { id, seg, at } => {
                        let key_seg = SEGMENTS[seg];
                        prop_assert_eq!(new.seed(id, key_seg, at), old.seed(id, key_seg, at));
                    }
                    Op::Tick { id, seg, ltt, vol, at } => {
                        let obs = TickObservation {
                            key: (id, SEGMENTS[seg]),
                            ltt_epoch_secs: 1_786_800_000 + ltt,
                            volume: vol,
                            open_interest: 0,
                            recv_monotonic_millis: at,
                        };
                        prop_assert_eq!(new.detector.observe(obs), old.detector.observe(obs));
                    }
                    Op::ResetSession => {
                        // Reassigns every detector slot: cached cells must
                        // then mismatch and fall back to the probe.
                        new.detector.reset_session();
                        old.detector.reset_session();
                    }
                    Op::Record { socket, keys, at_ms } => {
                        let l = list(&keys);
                        book_new.record_subscribed_at(&sockets[socket].0, &l, at(at_ms));
                        book_old.record_subscribed_at(&sockets[socket].0, &l, at(at_ms));
                    }
                    Op::CloseSocket { socket } => {
                        sockets[socket].1 = None;
                        let (tx, rx) = mpsc::channel(4);
                        sockets[socket] = (tx, Some(rx));
                    }
                    Op::Forget { keys } => {
                        let l = list(&keys);
                        book_new.forget(&l);
                        book_old.forget(&l);
                    }
                    Op::Sweep { now, at_ms, secs, day } => {
                        let mut worst_new = [SilentInstrument::EMPTY; WORST_SILENT_NAMED];
                        let mut worst_old = [SilentInstrument::EMPTY; WORST_SILENT_NAMED];
                        let clock = (secs, day);
                        let got =
                            new.scan_silence_named_at(now, &mut worst_new, at(at_ms), Some(clock));
                        let want =
                            scan_silence_named_frozen(&old, now, &mut worst_old, at(at_ms), clock);
                        prop_assert_eq!(got, want);
                        prop_assert_eq!(worst_new, worst_old);
                        prop_assert_eq!(
                            new.dead_class_latch.load(Ordering::Relaxed),
                            old.dead_class_latch.load(Ordering::Relaxed)
                        );
                    }
                }
                book_new.assert_cell_invariant();
                prop_assert_eq!(book_new.test_fingerprint(), book_old.test_fingerprint());
                loop {
                    match (rounds_new.try_recv(), rounds_old.try_recv()) {
                        (Ok(a), Ok(b)) => prop_assert_eq!(a.test_view(), b.test_view()),
                        (Err(_), Err(_)) => break,
                        (a, b) => prop_assert!(
                            false,
                            "rounds differ: {} vs {}",
                            a.is_ok(),
                            b.is_ok()
                        ),
                    }
                }
            }
        }
    }
}
