use super::*;
use crate::hot_path_telemetry::{bucket_index, fine_bucket_index};

const MINUTE: i64 = 1_793_577_600_000_000_000;

fn observed(stage: Stage, samples: &[u64]) -> SecondWindow {
    let mut second = SecondWindow::empty();
    second.stage_strict = [Some(StrictSampleCounts::default()); STAGES_PER_MINUTE];
    let mut exact = StrictSampleCounts::default();
    for &ns in samples {
        second.stage_buckets[stage as usize][bucket_index(ns)] += 1;
        second.stage_max_nanos[stage as usize] = second.stage_max_nanos[stage as usize].max(ns);
        exact.samples += 1;
        exact.samples_at_or_above_1000ns += u64::from(ns >= 1_000);
        if stage == Stage::ReceiptToFold {
            second.receipt_to_fold_fine[fine_bucket_index(ns)] += 1;
        }
    }
    second.stage_strict[stage as usize] = Some(exact);
    second
}

fn one_row(stage: Stage, samples: &[u64]) -> LatencyMinuteRow {
    let mut accumulator = MinuteAccumulator::new();
    let _ = accumulator.absorb(MINUTE, &observed(stage, samples));
    accumulator.rows().expect("rows")[stage as usize]
}

#[test]
fn test_minute_strict_counts_aggregate_exact_windows_without_reinterpreting_1024() {
    let stage = Stage::SocketToWal;
    let mut accumulator = MinuteAccumulator::new();
    let _ = accumulator.absorb(MINUTE, &observed(stage, &[999; 99]));
    let _ = accumulator.absorb(MINUTE + 1, &observed(stage, &[1_000]));
    let row = accumulator.rows().expect("rows")[stage as usize];
    assert_eq!(row.strict_version, Some(1));
    assert_eq!(row.strict_samples, Some(100));
    assert_eq!(row.strict_samples_at_or_above_1000ns, Some(1));
    assert_eq!(
        row.strict_counts().expect("exact").p99_below_1000ns(),
        Some(true)
    );
    assert_eq!(
        row.p99_ns,
        Some(1_024),
        "historical upper bound is unchanged"
    );
    assert_eq!(row.samples_over_1024ns, Some(0));
    assert_eq!(row.histogram, HISTOGRAM_LOG2);
    for ns in [1_000, 1_001, 1_024] {
        let row = one_row(stage, &[ns; 100]);
        assert_eq!(
            row.strict_counts().expect("exact").p99_below_1000ns(),
            Some(false)
        );
        assert_eq!(row.p99_ns, Some(1_024));
    }
}

#[test]
fn test_minute_strict_counts_never_mix_receipt_fine_and_raw_populations() {
    let stage = Stage::ReceiptToFold;
    let mut second = observed(stage, &[1_000; 100]);
    // Fine and coarse atomics can be observed at different boundaries.
    second.receipt_to_fold_fine[0] = 301;
    let mut accumulator = MinuteAccumulator::new();
    let _ = accumulator.absorb(MINUTE, &second);
    let row = accumulator.rows().expect("rows")[stage as usize];
    assert_eq!(row.samples, 301);
    assert_eq!(row.strict_samples, Some(100));
    assert_eq!(row.strict_samples_at_or_above_1000ns, Some(100));
    assert_eq!(
        row.strict_counts().expect("raw pair").p99_below_1000ns(),
        Some(false)
    );
}

#[test]
fn test_minute_missing_or_overflowed_strict_window_stays_unknown_until_rollover() {
    let stage = Stage::SocketToWal;
    for invalid in [
        None,
        Some(StrictSampleCounts {
            samples: u64::MAX,
            samples_at_or_above_1000ns: 0,
        }),
    ] {
        let mut accumulator = MinuteAccumulator::new();
        let good = observed(stage, &[999]);
        let _ = accumulator.absorb(MINUTE, &good);
        let mut bad = good;
        bad.stage_strict[stage as usize] = invalid;
        let _ = accumulator.absorb(MINUTE + 1, &bad);
        let _ = accumulator.absorb(MINUTE + 2, &good);
        let old = accumulator
            .absorb(MINUTE + NANOS_PER_MINUTE, &good)
            .expect("close");
        assert!(old[stage as usize].strict_counts().is_none());
        assert_eq!(old[stage as usize].strict_version, None);
        assert_eq!(old[stage as usize].strict_samples, None);
        assert_eq!(old[stage as usize].strict_samples_at_or_above_1000ns, None);
        assert_eq!(
            accumulator.rows().expect("new minute")[stage as usize].strict_samples,
            Some(1)
        );
    }
    let mut too_large = observed(stage, &[]);
    too_large.stage_strict[stage as usize] = Some(StrictSampleCounts {
        samples: i64::MAX as u64 + 1,
        samples_at_or_above_1000ns: 1,
    });
    let mut accumulator = MinuteAccumulator::new();
    let _ = accumulator.absorb(MINUTE, &too_large);
    assert!(
        accumulator.rows().expect("rows")[stage as usize]
            .strict_counts()
            .is_none()
    );
}

#[test]
fn test_from_columns_and_strict_counts_reject_legacy_partial_and_bad_counts() {
    let original = one_row(Stage::DrainMainFrame, &[999]);
    for (version, samples, violations) in [
        (None, None, None),
        (None, Some(1), Some(0)),
        (Some(0), Some(1), Some(0)),
        (Some(2), Some(1), Some(0)),
        (Some(1), None, Some(0)),
        (Some(1), Some(1), None),
        (Some(1), Some(-1), Some(0)),
        (Some(1), Some(1), Some(-1)),
        (Some(1), Some(1), Some(2)),
    ] {
        let mut row = original;
        row.strict_version = version;
        row.strict_samples = samples;
        row.strict_samples_at_or_above_1000ns = violations;
        assert!(row.strict_counts().is_none());
        let mut writer = HotPathLatencyMinuteWriter::for_test();
        writer.append_row(&row).expect("legacy-compatible row");
        assert!(!String::from_utf8_lossy(writer.buffer.as_bytes()).contains("strict_"));
    }
    let zero = one_row(Stage::SocketToWal, &[]);
    assert_eq!(
        zero.strict_counts()
            .expect("measured zero")
            .p99_below_1000ns(),
        None
    );
}

#[test]
fn test_append_row_strict_version_and_counts_are_v1_integers_and_jointly_present() {
    let row = one_row(Stage::DrainMainFrame, &[999, 1_000, 1_001]);
    let mut writer = HotPathLatencyMinuteWriter::for_test();
    writer.append_row(&row).expect("strict row");
    let text = std::str::from_utf8(writer.buffer.as_bytes()).expect("protocol V1 UTF-8");
    for field in [
        "strict_version=1i",
        "strict_samples=3i",
        "strict_samples_at_or_above_1000ns=2i",
        "p99_ns=1024i",
    ] {
        assert!(text.contains(field), "{field}: {text}");
    }
    assert!(text.ends_with(&format!(" {MINUTE}\n")));
    let ddl = hot_path_latency_minute_ddl_statements();
    for name in [
        "strict_version",
        "strict_samples",
        "strict_samples_at_or_above_1000ns",
    ] {
        assert!(ddl[0].contains(&format!("{name} LONG")));
        assert!(ddl.contains(&format!(
            "ALTER TABLE hot_path_latency_minute ADD COLUMN IF NOT EXISTS {name} LONG;"
        )));
    }
    assert!(
        ddl.last()
            .expect("dedup")
            .contains("DEDUP ENABLE UPSERT KEYS(ts, stage, feed)")
    );
}
