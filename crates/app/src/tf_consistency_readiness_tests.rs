//! Tests for `tf_consistency_readiness` (plan item M3, 2026-10-26). The two
//! `test_regression_*_live_*` cases are the exact rows read back from the
//! 2026-10-23 S3 archive of `candles_1m`, `candles_3m` and `candles_5m`.

use super::*;
use crate::tf_consistency_boot::{
    FindingDraft, bucket_grid, compare_tf, recompute_window, tf_verify_targets,
};
use tickvault_storage::tf_consistency_audit_persistence::FindingCategory;
use tickvault_trading::candles::TfIndex;

const NANOS_PER_SEC: i64 = 1_000_000_000;
/// A whole IST day number, so bucket arithmetic is exact.
const DAY_START: i64 = 20_000 * 86_400 * NANOS_PER_SEC;

fn hm(h: i64, m: i64) -> i64 {
    h * 3600 + m * 60
}

fn row(sod: i64, o: f64, h: f64, l: f64, c: f64, v: i64, tc: i64) -> CandleRow {
    CandleRow {
        ts_nanos: DAY_START + sod * NANOS_PER_SEC,
        open: o,
        high: h,
        low: l,
        close: c,
        volume: v,
        tick_count: tc,
    }
}

fn mismatches(findings: &[FindingDraft]) -> Vec<&'static str> {
    findings
        .iter()
        .filter(|f| f.category == FindingCategory::Mismatch)
        .map(|f| f.field)
        .collect()
}

/// 2026-10-23, `IDX_I` id 3 (NIFTY SMALLCAP 250), 3-minute 12:51 bar. The
/// exchange day low moved 16232.25 -> 16232.05 between the packet received
/// 12:51:59.95 and the one at 12:52:00.46. Only the 3-minute bar can hold it.
/// Before M3 this paged as `low stored 16232.05 recomputed 16232.25`.
#[test]
fn test_regression_extreme_straddle_live_2026_10_23_idx3_is_not_a_mismatch() {
    let ones = [
        row(hm(12, 51), 16249.7, 16249.7, 16232.25, 16232.5, 0, 119),
        row(hm(12, 52), 16232.9, 16247.15, 16232.9, 16247.15, 0, 112),
        row(hm(12, 53), 16248.1, 16251.3, 16246.25, 16249.9, 0, 112),
    ];
    let stored = [row(hm(12, 51), 16249.7, 16251.3, 16232.05, 16249.9, 0, 343)];
    let (findings, counts) = compare_tf(TfIndex::M3, &ones, &stored, DAY_START, None);
    assert!(mismatches(&findings).is_empty(), "{findings:?}");
    assert_eq!(counts.attributed_extreme, 1);
    assert_eq!(counts.attributed_first_trade, 0);
    assert_eq!(counts.buckets_compared, 1, "the bucket is still compared");
}

/// 2026-10-23, `NSE_FNO` id 35633 (DIVISLAB 27 Oct 9400 CE), 5-minute 09:15
/// bar. First packet of the day after 09:16: the 5-minute bar holds the
/// day's first volume and open, the 1-minute 09:16 bar holds volume 0.
/// Before M3 this paged as `open` and `volume` mismatches.
#[test]
fn test_regression_first_trade_live_2026_10_23_divislab_is_not_a_mismatch() {
    let ones = [
        row(hm(9, 16), 22.45, 22.45, 22.45, 22.45, 0, 1),
        row(hm(9, 17), 18.6, 18.75, 18.6, 18.75, -700, 2),
        row(hm(9, 18), 18.0, 18.0, 18.0, 18.0, -800, 1),
        row(hm(9, 19), 15.1, 15.15, 14.0, 14.75, -10_200, 3),
    ];
    let stored = [row(hm(9, 15), 22.0, 22.45, 14.0, 14.75, -12_900, 7)];
    let (findings, counts) = compare_tf(TfIndex::M5, &ones, &stored, DAY_START, None);
    assert!(mismatches(&findings).is_empty(), "{findings:?}");
    assert_eq!(counts.attributed_first_trade, 1);
    assert_eq!(counts.attributed_extreme, 0);
}

/// The same first-trade numbers in a bucket that is NOT the session's first
/// still page: the fold cannot attribute a first trade there.
#[test]
fn test_first_trade_shape_outside_the_0915_bar_still_pages() {
    let ones = [
        row(hm(10, 1), 22.45, 22.45, 22.45, 22.45, 0, 1),
        row(hm(10, 2), 18.6, 18.75, 18.6, 18.75, -700, 2),
    ];
    let stored = [row(hm(10, 0), 22.0, 22.45, 18.6, 18.75, -1_500, 3)];
    let (findings, counts) = compare_tf(TfIndex::M5, &ones, &stored, DAY_START, None);
    assert_eq!(mismatches(&findings), vec!["open", "volume"]);
    assert_eq!(counts.attributed_first_trade, 0);
}

/// A longer bar NARROWER than its 1-minute bars, or wider with a volume or
/// close disagreement mid-day, is not a by-design shape and still pages.
#[test]
fn test_non_attributable_differences_still_page() {
    let ones = [
        row(hm(12, 0), 100.0, 101.0, 99.0, 100.5, 10, 2),
        row(hm(12, 1), 100.5, 102.0, 99.5, 101.0, 20, 2),
    ];
    // Narrower high.
    let stored = [row(hm(12, 0), 100.0, 101.5, 99.0, 101.0, 30, 4)];
    let (f, c) = compare_tf(TfIndex::M5, &ones, &stored, DAY_START, None);
    assert_eq!(mismatches(&f), vec!["high"]);
    assert_eq!(c.attributed_extreme + c.attributed_first_trade, 0);
    // Wider low but volume off.
    let stored = [row(hm(12, 0), 100.0, 102.0, 98.0, 101.0, 31, 4)];
    let (f, _) = compare_tf(TfIndex::M5, &ones, &stored, DAY_START, None);
    assert_eq!(mismatches(&f), vec!["low", "volume"]);
    // Wider low but close off.
    let stored = [row(hm(12, 0), 100.0, 102.0, 98.0, 100.0, 30, 4)];
    let (f, _) = compare_tf(TfIndex::M5, &ones, &stored, DAY_START, None);
    assert_eq!(mismatches(&f), vec!["low", "close"]);
}

/// 2026-10-26 15:41 IST, `IDX_I` id 1: "15m 3:30 PM: candle missing (1-minute
/// data present)". The 15:30 bucket ends 15:45 and seals about 15:49; the
/// check read at 15:40. Read with the floor it had (15:36), the bucket is
/// skipped and counted, not missing; read after it sealed, a missing row
/// still pages.
#[test]
fn test_regression_bucket_unsealed_at_read_is_skipped_not_missing() {
    let ones = [
        row(hm(15, 30), 100.0, 101.0, 99.0, 100.5, 0, 50),
        row(hm(15, 39), 100.5, 101.0, 100.0, 100.8, 0, 50),
    ];
    let floor_at_1540 = hm(15, 36);
    let (findings, counts) = compare_tf(TfIndex::M15, &ones, &[], DAY_START, Some(floor_at_1540));
    assert!(
        findings
            .iter()
            .all(|f| f.category != FindingCategory::MissingTfRow),
        "{findings:?}"
    );
    assert_eq!(counts.unsealed_at_read, 1);

    let floor_after_seal = hm(15, 49);
    let (findings, counts) =
        compare_tf(TfIndex::M15, &ones, &[], DAY_START, Some(floor_after_seal));
    assert_eq!(
        findings
            .iter()
            .filter(|f| f.category == FindingCategory::MissingTfRow)
            .count(),
        1
    );
    assert_eq!(counts.unsealed_at_read, 0);

    // No floor (a past day): judged as before.
    let (findings, _) = compare_tf(TfIndex::M15, &ones, &[], DAY_START, None);
    assert_eq!(findings.len(), 1);
}

#[test]
fn test_unsealed_bucket_with_nothing_on_either_side_is_not_counted() {
    let (_, counts) = compare_tf(TfIndex::M15, &[], &[], DAY_START, Some(hm(9, 0)));
    assert_eq!(counts.unsealed_at_read, 0);
}

#[test]
fn test_attributed_wider_shape_rules() {
    let rec = recompute_window(&[row(hm(12, 0), 100.0, 101.0, 99.0, 100.5, 10, 2)])
        .ok()
        .flatten()
        .expect("a one-row window recomputes");
    let start = hm(12, 0);
    let wider_low = row(start, 100.0, 101.0, 98.5, 100.5, 10, 2);
    assert_eq!(
        attributed_wider_shape(&wider_low, &rec, start, Some(hm(9, 15))),
        Some(AttributedShape::ExtremeStraddle)
    );
    let wider_high_signed = row(start, 100.0, 101.5, 99.0, 100.5, -10, 2);
    assert_eq!(
        attributed_wider_shape(&wider_high_signed, &rec, start, Some(hm(9, 15))),
        Some(AttributedShape::ExtremeStraddle),
        "volume compares by magnitude"
    );
    let open_off = row(start, 100.2, 101.5, 99.0, 100.5, 10, 2);
    assert_eq!(attributed_wider_shape(&open_off, &rec, start, None), None);
    let min_volume = row(start, 100.0, 101.5, 99.0, 100.5, i64::MIN, 2);
    assert_eq!(attributed_wider_shape(&min_volume, &rec, start, None), None);
    // First trade: 09:15 bar, first 1m row after it, more volume.
    let open = hm(9, 15);
    let first = row(open, 99.0, 101.0, 99.0, 100.5, 15, 2);
    assert_eq!(
        attributed_wider_shape(&first, &rec, open, Some(hm(9, 16))),
        Some(AttributedShape::FirstTrade)
    );
    assert_eq!(attributed_wider_shape(&first, &rec, open, Some(open)), None);
    let less = row(open, 99.0, 101.0, 99.0, 100.5, 5, 2);
    assert_eq!(
        attributed_wider_shape(&less, &rec, open, Some(hm(9, 16))),
        None
    );
    assert_eq!(
        AttributedShape::ExtremeStraddle.as_str(),
        "extreme_straddle"
    );
    assert_eq!(AttributedShape::FirstTrade.as_str(), "first_trade");
}

#[test]
fn test_sealed_through_secs_of_day_is_inside_the_target_day_only() {
    let day = 1_000 * 86_400;
    let at = |sod: i64| u32::try_from(day + sod).unwrap_or(u32::MAX);
    assert_eq!(sealed_through_secs_of_day(at(56_700), day), Some(56_700));
    assert_eq!(sealed_through_secs_of_day(at(0), day), Some(0));
    assert_eq!(
        sealed_through_secs_of_day(at(86_400), day),
        None,
        "next day"
    );
    assert_eq!(
        sealed_through_secs_of_day(at(-1), day),
        None,
        "previous day"
    );
}

#[test]
fn test_classify_seal_barriers_in_order() {
    let staged = |n| -> std::io::Result<SpillStaged> {
        Ok(SpillStaged {
            staged: n,
            parked: 0,
        })
    };
    let target = TF_VERIFY_SEALED_TARGET_SECS_OF_DAY_IST;
    assert_eq!(
        classify_seal_barriers(target - 1, 100, Some(200), &staged(0)),
        Err(TfNotReady::NotSealed)
    );
    assert_eq!(
        classify_seal_barriers(target, 100, Some(100), &staged(0)),
        Err(TfNotReady::SealsPending),
        "a drain in the sweep's own second may predate its seals"
    );
    assert_eq!(
        classify_seal_barriers(target, 100, None, &staged(0)),
        Err(TfNotReady::SealsPending)
    );
    assert_eq!(
        classify_seal_barriers(target, 100, Some(101), &staged(1)),
        Err(TfNotReady::SealSpillStaged)
    );
    assert_eq!(
        classify_seal_barriers(target, 100, Some(101), &Err(std::io::Error::other("x"))),
        Err(TfNotReady::SealSpillStaged),
        "an unlistable folder fails closed"
    );
    assert_eq!(
        classify_seal_barriers(target, 100, Some(101), &staged(0)),
        Ok(())
    );
    for r in [
        TfNotReady::NotSealed,
        TfNotReady::SealsPending,
        TfNotReady::SealSpillStaged,
        TfNotReady::NotApplied,
    ] {
        assert!(!r.as_str().is_empty());
    }
}

fn wal(name: &str, writer: i64, seq: i64, suspended: bool) -> WalTableRow {
    WalTableRow {
        name: name.to_string(),
        suspended,
        writer_txn: Some(writer),
        sequencer_txn: Some(seq),
        error_tag: None,
        error_message: None,
    }
}

#[test]
fn test_snapshot_and_tables_applied() {
    let rows = [
        wal("candles_1m", 5, 9, false),
        wal("candles_5m", 3, 3, false),
    ];
    let snap = snapshot_sequencer_txns(&["candles_1m", "candles_5m", "candles_absent"], &rows);
    assert_eq!(
        snap,
        vec![("candles_1m".to_string(), 9), ("candles_5m".to_string(), 3)],
        "a table QuestDB does not list has nothing pending"
    );
    assert!(!tables_applied(&snap, &rows), "candles_1m writer 5 < 9");
    let later = [
        wal("candles_1m", 9, 12, false),
        wal("candles_5m", 3, 4, false),
    ];
    assert!(
        tables_applied(&snap, &later),
        "measured against the snapshot"
    );
    let suspended = [
        wal("candles_1m", 9, 9, true),
        wal("candles_5m", 3, 3, false),
    ];
    assert!(!tables_applied(&snap, &suspended));
    assert!(
        !tables_applied(&snap, &later[..1]),
        "a vanished table is not applied"
    );
}

/// The target is the latest natural end of any target's last bucket other
/// than the hourly bar, whose 15:15 bucket ends 16:15 and is skipped by the
/// in-process run.
#[test]
fn test_sealed_target_is_the_latest_natural_end_below_the_hourly_bar() {
    let mut latest_other = 0_i64;
    for tf in tf_verify_targets() {
        let secs = tf.seconds_per_bucket();
        let last = bucket_grid(secs)
            .last()
            .copied()
            .expect("every target has a grid");
        let natural_end = i64::from(last.start_secs_of_day) + i64::from(secs);
        if tf == TfIndex::M60 {
            assert_eq!(natural_end, hm(16, 15));
            assert!(natural_end > TF_VERIFY_READY_DEADLINE_SECS_OF_DAY_IST);
        } else {
            latest_other = latest_other.max(natural_end);
        }
    }
    assert_eq!(latest_other, TF_VERIFY_SEALED_TARGET_SECS_OF_DAY_IST);
    assert_eq!(TF_VERIFY_SEALED_TARGET_SECS_OF_DAY_IST, hm(15, 45));
}

#[test]
fn test_read_tables_are_candles_1m_and_every_target() {
    let tables = read_tables();
    assert_eq!(tables.first().copied(), Some("candles_1m"));
    assert_eq!(tables.len(), tf_verify_targets().len() + 1);
    for tf in tf_verify_targets() {
        assert!(tables.contains(&tf.table_name()));
    }
}
