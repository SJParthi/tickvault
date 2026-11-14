//! Unit tests of the Quote 61 repeat filter's decision.

use super::*;
use crate::feed_aux_persistence::AuxPacketKind;

/// 2026-11-11 10:00:00 IST as IST nanoseconds.
const DAY_10AM: i64 = (20_768 * 86_400 + 10 * 3_600) * 1_000_000_000;

fn row(ltp: f64, exchange_ts: i64, oi: i64, close: f64, ts: i64) -> AuxPacketRow {
    let mut r =
        AuxPacketRow::from_header(AuxPacketKind::NoTradeToday, 66_422, 2, 0, 1).expect("fits");
    r.ts_ist_nanos = ts;
    r.ltp = Some(ltp);
    r.exchange_ts_ist_nanos = Some(exchange_ts);
    r.oi = Some(oi);
    r.day_close = Some(close);
    r
}

fn base(ts: i64) -> AuxPacketRow {
    row(142.5, 1_000, 75, 140.0, ts)
}

/// Runs `check` and, on `Write`, `note_written`; returns whether it wrote.
fn feed(f: &mut AuxRepeatFilter, key: (u64, u8), r: &AuxPacketRow) -> bool {
    match f.check(key, r) {
        AuxRepeatVerdict::Write(v) => {
            f.note_written(key, v);
            true
        }
        AuxRepeatVerdict::Skip => false,
    }
}

const KEY: (u64, u8) = (66_422, 2);

/// A gap epoch that never moves: the filter judges values only.
fn no_gap() -> u64 {
    0
}

fn filter() -> AuxRepeatFilter {
    AuxRepeatFilter::with_gap_epoch(no_gap)
}

std::thread_local! {
    /// A per-test-thread gap epoch the test moves by hand.
    static EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn thread_epoch() -> u64 {
    EPOCH.with(std::cell::Cell::get)
}

#[test]
fn test_check_writes_the_first_row_and_skips_an_identical_repeat() {
    let mut f = filter();
    assert!(feed(&mut f, KEY, &base(DAY_10AM)), "first row is written");
    // Only the receipt time and capture sequence differ.
    let mut again = base(DAY_10AM + 1_000_000_000);
    again.capture_seq = 99;
    again.received_at_ist_nanos = Some(DAY_10AM + 1_000_000_000);
    assert_eq!(f.check(KEY, &again), AuxRepeatVerdict::Skip);
}

#[test]
fn test_check_writes_when_any_one_value_column_changes() {
    let t = DAY_10AM + 5;
    let changes: [fn(&mut AuxPacketRow); 13] = [
        |r| r.oi = Some(76),
        |r| r.prev_close = Some(1.0),
        |r| r.prev_oi = Some(1),
        |r| r.ltp = Some(142.55),
        |r| r.exchange_ts_ist_nanos = Some(1_001),
        |r| r.volume = Some(1),
        |r| r.day_open = Some(1.0),
        |r| r.day_high = Some(1.0),
        |r| r.day_low = Some(1.0),
        |r| r.day_close = Some(140.05),
        |r| r.oi_day_high = Some(1),
        |r| r.oi_day_low = Some(1),
        |r| r.reason_code = Some(805),
    ];
    for (n, change) in changes.iter().enumerate() {
        let mut f = filter();
        assert!(feed(&mut f, KEY, &base(DAY_10AM)));
        let mut r = base(t);
        change(&mut r);
        assert!(
            matches!(f.check(KEY, &r), AuxRepeatVerdict::Write(_)),
            "column {n} changed, the row must be written"
        );
    }
}

#[test]
fn test_check_tells_null_from_zero_and_negative_zero_from_zero() {
    let mut f = filter();
    let mut a = base(DAY_10AM);
    a.oi = None;
    assert!(feed(&mut f, KEY, &a));
    let mut b = base(DAY_10AM + 1);
    b.oi = Some(0);
    assert!(feed(&mut f, KEY, &b), "NULL then 0 is a change");
    let mut c = base(DAY_10AM + 2);
    c.oi = Some(0);
    c.ltp = Some(-0.0);
    let mut d = base(DAY_10AM + 3);
    d.oi = Some(0);
    d.ltp = Some(0.0);
    assert!(feed(&mut f, KEY, &c));
    assert!(feed(&mut f, KEY, &d), "-0.0 and 0.0 differ in bits");
}

#[test]
fn test_check_keeps_the_same_id_in_another_segment_independent() {
    let mut f = filter();
    assert!(feed(&mut f, (66_422, 2), &base(DAY_10AM)));
    assert!(
        feed(&mut f, (66_422, 1), &base(DAY_10AM + 1)),
        "same id, other segment: its own first row"
    );
    assert!(!feed(&mut f, (66_422, 1), &base(DAY_10AM + 2)));
    assert!(!feed(&mut f, (66_422, 2), &base(DAY_10AM + 3)));
}

#[test]
fn test_check_clears_at_the_ist_day_roll() {
    let mut f = filter();
    assert!(feed(&mut f, KEY, &base(DAY_10AM)));
    assert!(!feed(&mut f, KEY, &base(DAY_10AM + 1)));
    assert!(
        feed(&mut f, KEY, &base(DAY_10AM + NANOS_PER_DAY)),
        "next IST day: the first row is written again"
    );
    assert_eq!(f.last.len(), 1, "the old day's keys are gone");
}

#[test]
fn test_note_written_past_the_cap_writes_and_never_skips() {
    let mut f = filter();
    for id in 0..FEED_AUX_REPEAT_MAX_TRACKED as u64 {
        assert!(feed(&mut f, (id, 2), &base(DAY_10AM)));
    }
    assert_eq!(f.last.len(), FEED_AUX_REPEAT_MAX_TRACKED);
    let capacity = f.last.capacity();
    let over = (u64::MAX, 2);
    assert!(feed(&mut f, over, &base(DAY_10AM)));
    assert!(
        feed(&mut f, over, &base(DAY_10AM + 1)),
        "an untracked key is never skipped"
    );
    assert_eq!(f.last.len(), FEED_AUX_REPEAT_MAX_TRACKED);
    assert_eq!(f.last.capacity(), capacity, "the map never grows");
    // A tracked key still updates in place at the cap: its NEW value is
    // remembered (a repeat of it is skipped) and its OLD value no longer is
    // (going back to it is a change and is written).
    let changed = row(150.0, 1_000, 75, 140.0, DAY_10AM + 2);
    assert!(feed(&mut f, (7, 2), &changed), "a changed value is written");
    let mut changed_again = changed;
    changed_again.ts_ist_nanos = DAY_10AM + 3;
    assert!(
        !feed(&mut f, (7, 2), &changed_again),
        "a repeat of the new value is skipped: the tracked key was updated at the cap"
    );
    assert!(
        feed(&mut f, (7, 2), &base(DAY_10AM + 4)),
        "going back to the old value is a change and is written"
    );
}

#[test]
fn test_with_gap_epoch_a_moved_epoch_writes_the_next_repeat() {
    EPOCH.with(|e| e.set(10));
    let mut f = AuxRepeatFilter::with_gap_epoch(thread_epoch);
    assert!(feed(&mut f, KEY, &base(DAY_10AM)));
    assert!(
        !feed(&mut f, KEY, &base(DAY_10AM + 1)),
        "same epoch: skipped"
    );
    // A frame was shed before the drain, or a staged row did not land: the
    // epoch moves. Values A, then B (never seen live), then A again: the
    // second A must be written, or the replayed B stays the last stored row.
    EPOCH.with(|e| e.set(11));
    assert!(
        feed(&mut f, KEY, &base(DAY_10AM + 2)),
        "the first packet after a gap is written, even if it repeats"
    );
    assert!(
        !feed(&mut f, KEY, &base(DAY_10AM + 3)),
        "after that, repeats are skipped again"
    );
}

#[test]
fn test_note_written_is_what_arms_the_skip() {
    let mut f = filter();
    // A row that was never noted (never staged) arms no skip.
    assert!(matches!(
        f.check(KEY, &base(DAY_10AM)),
        AuxRepeatVerdict::Write(_)
    ));
    assert!(matches!(
        f.check(KEY, &base(DAY_10AM + 1)),
        AuxRepeatVerdict::Write(_)
    ));
}

#[test]
fn test_clear_forgets_every_key() {
    let mut f = filter();
    assert!(feed(&mut f, KEY, &base(DAY_10AM)));
    f.clear();
    assert!(
        feed(&mut f, KEY, &base(DAY_10AM + 1)),
        "after a clear the first row is written again"
    );
}

#[test]
fn test_with_gap_epoch_is_pre_sized_at_the_cap() {
    let f = filter();
    assert!(f.last.capacity() >= FEED_AUX_REPEAT_MAX_TRACKED);
    assert!(f.last.is_empty());
}

/// Review finding (2026-11-14): a WAL replay gap inside one pass. Frames
/// between two replayed frames were not re-read, and live may have stored a
/// different row for the key in them, so the first row after the gap is
/// written even when it repeats the row before the gap.
#[test]
fn test_mark_gap_writes_the_next_identical_row() {
    let mut f = filter();
    assert!(feed(&mut f, KEY, &base(DAY_10AM)));
    assert!(!feed(&mut f, KEY, &base(DAY_10AM + 1)), "no gap: skipped");
    f.mark_gap();
    assert!(
        feed(&mut f, KEY, &base(DAY_10AM + 2)),
        "the first row after a replay gap is written"
    );
    assert!(
        !feed(&mut f, KEY, &base(DAY_10AM + 3)),
        "after that, repeats are skipped again"
    );
    // Two gaps in a row still move the epoch past the earlier one.
    f.mark_gap();
    f.mark_gap();
    assert!(feed(&mut f, KEY, &base(DAY_10AM + 4)));
}

/// Review finding (2026-11-14): `kind` is a stored column (and in the DEDUP
/// key), so a row of another kind is never a repeat, even with identical
/// values (the receipt-day label cutover).
#[test]
fn test_check_writes_a_row_of_another_kind() {
    let mut f = filter();
    let mut old = base(DAY_10AM);
    old.kind = AuxPacketKind::ConnectSnapshot;
    assert!(feed(&mut f, KEY, &old));
    assert!(
        feed(&mut f, KEY, &base(DAY_10AM + 1)),
        "same values, other kind: written"
    );
    assert!(!feed(&mut f, KEY, &base(DAY_10AM + 2)));
}

/// Review finding (2026-11-14): a contract the main feed carries on two
/// sockets is judged with `check_never_skip`. A ring shed on one socket is not
/// ordered against the other socket's frames, so a repeat from the second
/// socket could be judged before the shed is counted. Its rows are always
/// written, and the key stays unskippable for the day, even when later rows
/// are judged with `check` (a copy still in flight after it left the set).
#[test]
fn test_check_never_skip_writes_every_row_and_pins_the_key_for_the_day() {
    let mut f = filter();
    let run = |f: &mut AuxRepeatFilter, r: &AuxPacketRow| match f.check_never_skip(KEY, r) {
        AuxRepeatVerdict::Write(v) => {
            f.note_written(KEY, v);
            true
        }
        AuxRepeatVerdict::Skip => false,
    };
    assert!(run(&mut f, &base(DAY_10AM)));
    assert!(
        run(&mut f, &base(DAY_10AM + 1)),
        "an identical row of a backed-up contract is written"
    );
    assert!(
        feed(&mut f, KEY, &base(DAY_10AM + 2)),
        "once judged as backed up, the key is never skipped that day"
    );
    assert!(feed(&mut f, KEY, &base(DAY_10AM + 3)));
    // Another key on the same filter is unaffected.
    assert!(feed(&mut f, (1, 2), &base(DAY_10AM + 4)));
    assert!(!feed(&mut f, (1, 2), &base(DAY_10AM + 5)));
    // The next IST day starts over: the pin is gone.
    assert!(feed(&mut f, KEY, &base(DAY_10AM + NANOS_PER_DAY)));
    assert!(!feed(&mut f, KEY, &base(DAY_10AM + NANOS_PER_DAY + 1)));
    // A replay-pass clear also drops the pin.
    assert!(run(&mut f, &base(DAY_10AM + NANOS_PER_DAY + 2)));
    f.clear();
    assert!(feed(&mut f, KEY, &base(DAY_10AM + NANOS_PER_DAY + 3)));
    assert!(!feed(&mut f, KEY, &base(DAY_10AM + NANOS_PER_DAY + 4)));
}
