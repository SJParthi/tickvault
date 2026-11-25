use super::*;
use proptest::prelude::*;

#[test]
fn test_record_stage_nanos_strict_boundaries_keep_legacy_projection() {
    let stats = StageStats::new();
    for nanos in [0, 999, 1_000, 1_001, 1_024, 1_025] {
        stats.record(Stage::SocketToWal, nanos);
    }
    let snap = stats.snapshot();
    assert_eq!(&snap.raw_buckets[..3], &[2, 3, 1]);
    assert_eq!(&snap.buckets[..2], &[5, 1]);
    assert_eq!(snap.count, 6);
    let strict = strict_window(&snap.raw_buckets, &[0; RAW_STAGE_BUCKETS]).expect("exact");
    assert_eq!(strict.samples, 6);
    assert_eq!(strict.samples_at_or_above_1000ns, 4);
    assert_eq!(strict.p99_below_1000ns(), Some(false));
    assert_eq!(
        snap.over_budget, 0,
        "strict target does not redefine a stall"
    );
    assert_eq!(STAGE_BUCKETS, 28);
    assert_eq!(BUCKET_LABELS[0], "1024");
    assert_eq!(bucket_upper_bound_nanos(0), Some(1_024));
}

proptest! {
    #[test]
    fn proptest_raw_bucket_index_projects_to_the_historical_bucket(nanos in any::<u64>()) {
        let mut raw = [0; RAW_STAGE_BUCKETS];
        raw[raw_bucket_index(nanos)] = 1;
        let legacy = legacy_buckets(&raw);
        prop_assert_eq!(legacy.iter().sum::<u64>(), 1);
        prop_assert_eq!(legacy[bucket_index(nanos)], 1);
        let counts = strict_window(&raw, &[0; RAW_STAGE_BUCKETS]).expect("one sample");
        prop_assert_eq!(counts.samples_at_or_above_1000ns, u64::from(nanos >= 1_000));
    }
}

#[test]
fn test_raw_bucket_index_all_historical_edges_keep_their_bounds() {
    for index in 0..STAGE_BUCKETS {
        if let Some(bound) = bucket_upper_bound_nanos(index) {
            for nanos in [bound - 1, bound, bound + 1] {
                let mut raw = [0; RAW_STAGE_BUCKETS];
                raw[raw_bucket_index(nanos)] = 1;
                assert_eq!(legacy_buckets(&raw)[bucket_index(nanos)], 1);
            }
        }
    }
    assert_eq!(raw_bucket_index(u64::MAX), RAW_STAGE_BUCKETS - 1);
}

#[test]
fn test_strict_sample_counts_p99_below_1000ns_uses_exact_nearest_rank() {
    for samples in [1, 99, 100, 101, 199, 200, 1_000, i64::MAX as u64, u64::MAX] {
        let allowed = samples / 100;
        assert_eq!(
            StrictSampleCounts {
                samples,
                samples_at_or_above_1000ns: allowed,
            }
            .p99_below_1000ns(),
            Some(true),
            "N={samples}"
        );
        assert_eq!(
            StrictSampleCounts {
                samples,
                samples_at_or_above_1000ns: allowed + 1,
            }
            .p99_below_1000ns(),
            Some(false),
            "N={samples}"
        );
    }
    for nanos in [999, 1_000, 1_001, 1_024] {
        let stats = StageStats::new();
        for _ in 0..101 {
            stats.record(Stage::DrainMainFrame, nanos);
        }
        let counts = strict_window(&stats.snapshot().raw_buckets, &[0; RAW_STAGE_BUCKETS])
            .expect("recorded samples");
        assert_eq!(counts.p99_below_1000ns(), Some(nanos < 1_000));
    }
    assert_eq!(StrictSampleCounts::default().p99_below_1000ns(), None);
    assert_eq!(
        StrictSampleCounts {
            samples: 1,
            samples_at_or_above_1000ns: 2
        }
        .p99_below_1000ns(),
        None
    );
}

#[test]
fn test_strict_window_regression_is_unknown_and_next_window_recovers() {
    let stats = StageStats::new();
    let mut windows = QuantileWindows::new();
    stats.record(Stage::SocketToWal, 999);
    assert_eq!(
        windows
            .advance(Stage::SocketToWal, &stats.snapshot())
            .strict
            .expect("first")
            .p99_below_1000ns(),
        Some(true)
    );
    stats.buckets[0].store(0, Ordering::Relaxed); // Simulated reset/wrap.
    assert!(
        windows
            .advance(Stage::SocketToWal, &stats.snapshot())
            .strict
            .is_none()
    );
    stats.record(Stage::SocketToWal, 1_000);
    let next = windows
        .advance(Stage::SocketToWal, &stats.snapshot())
        .strict
        .expect("new baseline");
    assert_eq!(
        next,
        StrictSampleCounts {
            samples: 1,
            samples_at_or_above_1000ns: 1
        }
    );
    assert_eq!(next.p99_below_1000ns(), Some(false));
}

#[test]
fn test_checked_add_and_ilp_counts_enforce_overflow_and_signed_limits() {
    let mut raw = [0; RAW_STAGE_BUCKETS];
    raw[0] = u64::MAX;
    raw[1] = 1;
    assert!(strict_window(&raw, &[0; RAW_STAGE_BUCKETS]).is_none());
    assert_eq!(
        legacy_buckets(&raw)[0],
        u64::MAX,
        "legacy saturation retained"
    );
    let huge = StrictSampleCounts {
        samples: u64::MAX,
        samples_at_or_above_1000ns: 0,
    };
    assert!(
        huge.checked_add(StrictSampleCounts {
            samples: 1,
            samples_at_or_above_1000ns: 0
        })
        .is_none()
    );
    assert!(huge.ilp_counts().is_none());
    let signed_max = StrictSampleCounts {
        samples: i64::MAX as u64,
        samples_at_or_above_1000ns: 1,
    };
    assert_eq!(signed_max.ilp_counts(), Some((i64::MAX, 1)));
    assert!(
        StrictSampleCounts {
            samples: 0,
            samples_at_or_above_1000ns: 1
        }
        .ilp_counts()
        .is_none()
    );
}

#[test]
fn test_strict_window_concurrent_real_recording_conserves_the_same_population() {
    const THREADS: u64 = 4;
    const PER_THREAD: u64 = 10_000;
    let stats = StageStats::new();
    let mut windows = QuantileWindows::new();
    let mut observed = StrictSampleCounts::default();
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                for i in 0..PER_THREAD {
                    stats.record(
                        Stage::SocketToWal,
                        [999, 1_000, 1_024, 1_025][(i % 4) as usize],
                    );
                }
            });
        }
        for _ in 0..100 {
            let window = windows.advance(Stage::SocketToWal, &stats.snapshot());
            let counts = window.strict.expect("monotonic bins");
            assert!(counts.samples_at_or_above_1000ns <= counts.samples);
            assert_eq!(counts.samples, window.window.iter().sum::<u64>());
            observed = observed.checked_add(counts).expect("bounded fixture");
        }
    });
    let final_window = windows
        .advance(Stage::SocketToWal, &stats.snapshot())
        .strict
        .expect("final");
    observed = observed.checked_add(final_window).expect("bounded fixture");
    assert_eq!(observed.samples, THREADS * PER_THREAD);
    assert_eq!(
        observed.samples_at_or_above_1000ns,
        THREADS * PER_THREAD * 3 / 4
    );
    assert_eq!(stats.snapshot().count, observed.samples);
}
