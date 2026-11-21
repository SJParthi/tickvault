//! Tests for [`super`] (scope lock 2026-11-20, Dhan trade time sent as UTC).

use super::*;
use proptest::prelude::*;

const NANOS: i64 = 1_000_000_000;
/// 2026-11-20 11:00:00 IST as a UTC receipt: the scope lock's NIFTY 50 example
/// ("received 11:00:00 IST, raw LTT `1795152600`").
const RECEIPT_1100_UTC_SECS: i64 = 1_795_152_600;
const RAW_UTC_1100: u32 = 1_795_152_600;

fn receipt(utc_secs: i64) -> i64 {
    utc_secs * NANOS
}

/// The raw time Dhan sends for a print made at UTC second `utc_secs` on a
/// normal (IST-epoch) day.
fn raw_normal(utc_secs: i64) -> u32 {
    u32::try_from(utc_secs + IST_OFFSET_SECS).expect("in u32")
}

/// The raw time Dhan sent on 2026-11-20 (plain UTC epoch).
fn raw_utc(utc_secs: i64) -> u32 {
    u32::try_from(utc_secs).expect("in u32")
}

fn d(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("test date")
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "tv-ltt-{tag}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// --- time helpers -----------------------------------------------------------

#[test]
fn test_receipt_ist_day_and_date_round_trip() {
    let day = receipt_ist_day(receipt(RECEIPT_1100_UTC_SECS));
    assert_eq!(ist_day_date(day), Some(d("2026-11-20")));
    assert_eq!(date_ist_day(d("2026-11-20")), day);
    // 18:29:59 UTC is 23:59:59 IST, the same IST day; 18:30:00 UTC is the next.
    let midnight_utc = date_ist_day(d("2026-11-21")) * 86_400 - IST_OFFSET_SECS;
    assert_eq!(receipt_ist_day(receipt(midnight_utc - 1)), day);
    assert_eq!(receipt_ist_day(receipt(midnight_utc)), day + 1);
    assert_eq!(ist_day_date(i64::MAX), None);
}

// --- the guard (DORMANT since 2026-11-21: `correction_verdict`) --------------

#[test]
fn test_regression_2026_11_20_nifty_example_is_corrected_to_receipt_time() {
    // Regression: 2026-11-20 — NIFTY 50 received 11:00:00 IST carried raw
    // 1795152600, which reads as 05:30:00 under the IST rule.
    let got = correction_verdict(RAW_UTC_1100, receipt(RECEIPT_1100_UTC_SECS), true);
    assert_eq!(got, Some(RAW_UTC_1100 + LTT_UTC_OFFSET_SECS));
    assert_eq!(
        got.map(|t| i64::from(t) % 86_400),
        Some(11 * 3600),
        "the corrected time is 11:00:00 IST"
    );
}

#[test]
fn test_a_normal_print_is_never_shifted_into_the_future() {
    let r = receipt(RECEIPT_1100_UTC_SECS);
    let raw = raw_normal(RECEIPT_1100_UTC_SECS);
    assert_eq!(correction_verdict(raw, r, true), None);
    assert_eq!(correction_verdict(raw, r, false), None);
}

#[test]
fn test_the_future_cap_is_receipt_plus_five_seconds() {
    let r = receipt(RECEIPT_1100_UTC_SECS);
    assert!(correction_verdict(raw_utc(RECEIPT_1100_UTC_SECS + 5), r, true).is_some());
    assert_eq!(
        correction_verdict(raw_utc(RECEIPT_1100_UTC_SECS + 6), r, true),
        None
    );
}

#[test]
fn test_zero_out_of_band_and_unstamped_are_never_corrected() {
    let r = receipt(RECEIPT_1100_UTC_SECS);
    assert_eq!(correction_verdict(0, r, true), None);
    assert_eq!(correction_verdict(1_000, r, true), None);
    assert_eq!(correction_verdict(u32::MAX, r, true), None);
    assert_eq!(
        correction_verdict(RAW_UTC_1100, WAL_RECEIPT_UNKNOWN_NANOS, true),
        None
    );
    assert_eq!(correction_verdict(RAW_UTC_1100, -5, true), None);
}

#[test]
fn test_an_unconfirmed_segment_corrects_only_fresh_prints() {
    let r = receipt(RECEIPT_1100_UTC_SECS);
    let fresh = raw_utc(RECEIPT_1100_UTC_SECS - 120);
    let stale = raw_utc(RECEIPT_1100_UTC_SECS - 121);
    assert!(correction_verdict(fresh, r, false).is_some());
    assert_eq!(correction_verdict(stale, r, false), None);
    // A confirmed segment corrects a stale print of the same day.
    assert!(correction_verdict(stale, r, true).is_some());
    let hours_old = raw_utc(RECEIPT_1100_UTC_SECS - 4 * 3600);
    assert!(correction_verdict(hours_old, r, true).is_some());
}

#[test]
fn test_the_previous_day_is_allowed_only_inside_its_session() {
    let r = receipt(RECEIPT_1100_UTC_SECS);
    // Yesterday 15:29 IST, raw sent as UTC.
    let yesterday_1529_ist = RECEIPT_1100_UTC_SECS - 86_400 + (4 * 3600 + 29 * 60);
    let in_session = raw_utc(yesterday_1529_ist);
    assert!(correction_verdict(in_session, r, true).is_some());
    // Yesterday 20:59 IST: outside the session (a pre-open snapshot carrying
    // yesterday's close read as IST, shifted, lands here).
    let evening = raw_utc(yesterday_1529_ist + 5 * 3600 + 30 * 60);
    assert_eq!(correction_verdict(evening, r, true), None);
    // Two days back: never.
    let two_back = raw_utc(RECEIPT_1100_UTC_SECS - 2 * 86_400);
    assert_eq!(correction_verdict(two_back, r, true), None);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// Every correction is exactly +19,800 s, never after receipt + 5 s, and on
    /// the receipt's day or inside the previous day's session.
    #[test]
    fn proptest_correction_obeys_every_guard(
        raw in any::<u32>(),
        receipt_secs in 1_600_000_000i64..2_500_000_000i64,
        confirmed in any::<bool>(),
    ) {
        let r = receipt(receipt_secs);
        if let Some(c) = correction_verdict(raw, r, confirmed) {
            prop_assert_eq!(c, raw + LTT_UTC_OFFSET_SECS);
            let rec = receipt_ist_secs(r);
            prop_assert!(i64::from(c) <= rec + RECEIPT_FUTURE_CAP_SECS);
            let (cd, rd) = (i64::from(c).div_euclid(86_400), rec.div_euclid(86_400));
            if cd != rd {
                prop_assert_eq!(cd, rd - 1);
                let sod = i64::from(c).rem_euclid(86_400) as u32;
                prop_assert!(
                    (TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST)
                        .contains(&sod)
                        || (MUHURAT_PERSIST_START_SECS_OF_DAY_IST
                            ..MUHURAT_PERSIST_END_SECS_OF_DAY_IST)
                            .contains(&sod)
                );
            }
            if !confirmed {
                prop_assert!(rec - i64::from(c) <= UNCONFIRMED_FRESH_SECS);
            }
            prop_assert!(raw != 0);
        }
    }
}

// --- the table ---------------------------------------------------------------

fn entry(id: u8, segment: u8, from: i64, to: i64) -> LttEntry {
    LttEntry {
        id,
        segment,
        from_nanos: from,
        to_nanos: to,
    }
}

#[test]
fn test_table_corrects_only_inside_an_entry_and_on_its_day() {
    let r = receipt(RECEIPT_1100_UTC_SECS);
    let day = receipt_ist_day(r);
    let mut table = LttTable::empty();
    assert_eq!(table.correct(RAW_UTC_1100, 0, r), None);
    assert!(!table.any_entry());
    assert!(table.arm(day, entry(0, SEGMENT_IDX_I, r, i64::MAX)));
    assert!(table.any_entry());
    assert_eq!(
        table.correct(RAW_UTC_1100, SEGMENT_IDX_I, r),
        Some(RAW_UTC_1100 + LTT_UTC_OFFSET_SECS)
    );
    // Before `from`: raw.
    assert_eq!(table.correct(RAW_UTC_1100 - 1, SEGMENT_IDX_I, r - 1), None);
    // The next IST day: the entry never carries over.
    let next = r + 86_400 * NANOS;
    assert_eq!(
        table.correct(RAW_UTC_1100 + 86_400, SEGMENT_IDX_I, next),
        None
    );
    // Closed: raw from `to` on.
    assert!(table.close(day, 0, r + 10 * NANOS));
    assert!(
        table
            .correct(RAW_UTC_1100 + 9, SEGMENT_IDX_I, r + 9 * NANOS)
            .is_some()
    );
    assert_eq!(
        table.correct(RAW_UTC_1100 + 10, SEGMENT_IDX_I, r + 10 * NANOS),
        None
    );
    assert!(!table.close(day, 5, r));
    assert!(!table.close(day + 1, 0, r));
}

#[test]
fn test_another_segment_uses_the_unconfirmed_rule() {
    let r = receipt(RECEIPT_1100_UTC_SECS);
    let day = receipt_ist_day(r);
    let mut table = LttTable::empty();
    assert!(table.arm(day, entry(0, SEGMENT_IDX_I, r, i64::MAX)));
    let stale = raw_utc(RECEIPT_1100_UTC_SECS - 600);
    // NSE_FNO (2): not confirmed, so a stale print stays raw.
    assert_eq!(table.correct(stale, 2, r), None);
    assert!(
        table
            .correct(raw_utc(RECEIPT_1100_UTC_SECS - 10), 2, r)
            .is_some()
    );
    // A second entry confirming NSE_EQ upgrades that segment only.
    assert!(table.arm(day, entry(1, SEGMENT_NSE_EQ, r, i64::MAX)));
    assert!(table.correct(stale, SEGMENT_NSE_EQ, r).is_some());
    assert_eq!(table.correct(stale, 2, r), None);
}

#[test]
fn test_a_day_holds_at_most_eight_entries() {
    let r = receipt(RECEIPT_1100_UTC_SECS);
    let day = receipt_ist_day(r);
    let mut table = LttTable::empty();
    for id in 0..MAX_ENTRIES_PER_DAY as u8 {
        assert!(table.arm(day, entry(id, 0, r, i64::MAX)));
    }
    assert!(!table.arm(day, entry(8, 0, r, i64::MAX)));
    assert_eq!(table.day(day).map(|s| s.entries().len()), Some(8));
}

#[test]
fn test_a_slot_reused_by_a_later_day_drops_the_old_day() {
    let r = receipt(RECEIPT_1100_UTC_SECS);
    let day = receipt_ist_day(r);
    let mut table = LttTable::empty();
    assert!(table.arm(day, entry(0, 0, r, i64::MAX)));
    let later = day + DAY_SLOTS as i64;
    assert!(table.arm(later, entry(0, 0, r, i64::MAX)));
    assert!(table.day(day).is_none());
    assert_eq!(table.correct(RAW_UTC_1100, 0, r), None);
}

#[test]
fn test_held_days_are_read_by_receipt() {
    let r = receipt(RECEIPT_1100_UTC_SECS);
    let day = receipt_ist_day(r);
    let mut record = DayRecord::default();
    record.apply(&RecordLine {
        kind: RecordKind::Entry,
        id: 0,
        segment: 0,
        nanos: r,
    });
    let mut table = LttTable::empty();
    table.install_day(day, &record);
    assert!(table.any_held());
    assert!(table.receipt_day_held(r));
    assert!(!table.receipt_day_held(r + 86_400 * NANOS));
    assert!(!table.any_entry(), "a dangling intent arms nothing");
}

// --- the record --------------------------------------------------------------

#[test]
fn test_record_line_round_trips_and_refuses_damage() {
    for kind in [
        RecordKind::Entry,
        RecordKind::Armed,
        RecordKind::Void,
        RecordKind::Close,
        RecordKind::Closed,
        RecordKind::CloseVoid,
        RecordKind::Notice,
    ] {
        let line = RecordLine {
            kind,
            id: 7,
            segment: 1,
            nanos: -42,
        };
        let text = encode_line(&line);
        assert!(text.ends_with('\n'));
        assert_eq!(decode_line(&text), Some(line));
        // One flipped character fails the CRC.
        let tampered = text.replacen("-42", "-43", 1);
        assert_eq!(decode_line(&tampered), None);
        // A torn line (cut before its CRC) is refused.
        assert_eq!(decode_line(&text[..text.len() / 2]), None);
    }
    assert_eq!(decode_line(""), None);
    assert_eq!(decode_line("TVLTT1 E 8 0 0 00000000"), None);
    assert_eq!(decode_line("garbage line with no crc"), None);
}

fn lines(spec: &[(RecordKind, u8, i64)]) -> String {
    spec.iter()
        .map(|&(kind, id, nanos)| {
            encode_line(&RecordLine {
                kind,
                id,
                segment: 0,
                nanos,
            })
        })
        .collect()
}

#[test]
fn test_day_record_held_semantics() {
    use RecordKind::*;
    // Committed: armed, not held.
    let r = DayRecord::parse(&lines(&[(Entry, 0, 1), (Armed, 0, 100)]));
    assert!(!r.held());
    assert_eq!(r.armed_entries(), vec![entry(0, 0, 100, i64::MAX)]);
    // Dangling intent: held.
    assert!(DayRecord::parse(&lines(&[(Entry, 0, 1)])).held());
    // Voided: neither held nor armed.
    let v = DayRecord::parse(&lines(&[(Entry, 0, 1), (Void, 0, 0)]));
    assert!(!v.held());
    assert!(v.armed_entries().is_empty());
    // Close intent without commit: held.
    assert!(DayRecord::parse(&lines(&[(Entry, 0, 1), (Armed, 0, 100), (Close, 0, 0)])).held());
    // Close committed.
    let k = DayRecord::parse(&lines(&[
        (Entry, 0, 1),
        (Armed, 0, 100),
        (Close, 0, 0),
        (Closed, 0, 200),
    ]));
    assert!(!k.held());
    assert_eq!(k.armed_entries(), vec![entry(0, 0, 100, 200)]);
    // Close voided: the entry stays open.
    let x = DayRecord::parse(&lines(&[
        (Entry, 0, 1),
        (Armed, 0, 100),
        (Close, 0, 0),
        (CloseVoid, 0, 0),
    ]));
    assert!(!x.held());
    assert_eq!(x.armed_entries(), vec![entry(0, 0, 100, i64::MAX)]);
    // A damaged line is counted and ignored.
    let mut text = lines(&[(Entry, 0, 1), (Armed, 0, 100)]);
    text.push_str("TVLTT1 A 0 0 1 deadbeef\n");
    let dmg = DayRecord::parse(&text);
    assert_eq!(dmg.damaged_lines, 1);
    assert_eq!(dmg.armed_entries(), vec![entry(0, 0, 100, i64::MAX)]);
    // Notice and ids used.
    let n = DayRecord::parse(&lines(&[(Entry, 2, 1), (Void, 2, 0), (Notice, 0, 5)]));
    assert!(n.notice_sent);
    assert_eq!(n.ids_used, 3);
}

#[test]
fn test_append_and_load_record_dir() {
    let scratch = Scratch::new("load");
    let r = receipt(RECEIPT_1100_UTC_SECS);
    let day = receipt_ist_day(r);
    for line in [
        RecordLine {
            kind: RecordKind::Entry,
            id: 0,
            segment: 0,
            nanos: r,
        },
        RecordLine {
            kind: RecordKind::Armed,
            id: 0,
            segment: 0,
            nanos: r,
        },
    ] {
        append_line(&scratch.0, day, &line).expect("append");
    }
    // A dangling intent the day before.
    append_line(
        &scratch.0,
        day - 1,
        &RecordLine {
            kind: RecordKind::Entry,
            id: 0,
            segment: 0,
            nanos: r,
        },
    )
    .expect("append");
    let (table, report) = load_record_dir(&scratch.0, day, LttUtcAutocorrect::Act);
    assert_eq!(report.entries, 1);
    assert_eq!(report.held_days, vec![d("2026-11-19")]);
    assert_eq!(
        table.correct(RAW_UTC_1100, 0, r),
        Some(RAW_UTC_1100 + LTT_UTC_OFFSET_SECS)
    );
    // Shadow loads the same records (replay must match what act wrote).
    let (shadow, _) = load_record_dir(&scratch.0, day, LttUtcAutocorrect::Shadow);
    assert_eq!(shadow, table);
    // Off loads nothing: the full rollback.
    let (off, off_report) = load_record_dir(&scratch.0, day, LttUtcAutocorrect::Off);
    assert!(!off.any_entry() && !off.any_held());
    assert_eq!(off_report.mode, "off");
    // A file past the loaded window is not read.
    let (far, _) = load_record_dir(&scratch.0, day + BOOT_LOAD_DAYS + 1, LttUtcAutocorrect::Act);
    assert!(!far.any_entry());
}

#[test]
fn test_an_unreadable_record_holds_its_day() {
    let scratch = Scratch::new("unreadable");
    let r = receipt(RECEIPT_1100_UTC_SECS);
    let day = receipt_ist_day(r);
    let path = record_path(&scratch.0, day).expect("path");
    // A folder where the file should be: reading it fails.
    std::fs::create_dir_all(&path).expect("dir");
    let (table, report) = load_record_dir(&scratch.0, day, LttUtcAutocorrect::Act);
    assert_eq!(report.unreadable_days, vec![d("2026-11-20")]);
    assert!(table.receipt_day_held(r));
    // An oversized file is refused too.
    let scratch2 = Scratch::new("oversized");
    let path2 = record_path(&scratch2.0, day).expect("path");
    std::fs::write(&path2, vec![b'x'; (MAX_RECORD_BYTES + 1) as usize]).expect("write");
    assert!(read_day_record(&scratch2.0, day).is_err());
}

#[test]
fn test_notice_text_is_plain_and_names_the_time() {
    let r = receipt(RECEIPT_1100_UTC_SECS);
    let text = notice_text(SEGMENT_IDX_I, r);
    assert!(text.contains("5 h 30 min early"), "{text}");
    assert!(text.contains("11:00:00 AM"), "{text}");
    assert!(text.contains("index prices"), "{text}");
    assert!(text.contains("Raw data is kept"), "{text}");
    assert!(notice_text(SEGMENT_NSE_EQ, r).contains("NSE stock prices"));
    assert!(notice_text(9, r).contains("(seen on prices)"));
}

#[test]
fn test_record_writer_thread_acks_intents_and_sends_one_notice_a_day() {
    let scratch = Scratch::new("writer");
    let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&sent);
    let notice: LttNoticeFn = Arc::new(move |m| sink.lock().expect("lock").push(m));
    let handle = spawn_record_writer(scratch.0.clone(), Some(notice)).expect("spawn");
    let r = receipt(RECEIPT_1100_UTC_SECS);
    let day = receipt_ist_day(r);
    let line = |kind, id| RecordCmd {
        day,
        line: RecordLine {
            kind,
            id,
            segment: 0,
            nanos: r,
        },
    };
    handle.tx.send(line(RecordKind::Entry, 0)).expect("send");
    let ack = handle
        .acks
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("ack");
    assert_eq!(
        ack,
        RecordAck {
            day,
            id: 0,
            kind: RecordKind::Entry,
            durable: true
        }
    );
    handle.tx.send(line(RecordKind::Armed, 0)).expect("send");
    handle.tx.send(line(RecordKind::Entry, 1)).expect("send");
    let _ = handle
        .acks
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("ack 2");
    handle.tx.send(line(RecordKind::Armed, 1)).expect("send");
    handle.tx.send(line(RecordKind::Entry, 2)).expect("send");
    let _ = handle
        .acks
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("ack 3");
    assert_eq!(sent.lock().expect("lock").len(), 1, "one notice a day");
    let record = read_day_record(&scratch.0, day)
        .expect("read")
        .expect("exists");
    assert!(record.notice_sent);
    assert_eq!(record.armed_entries().len(), 2);
    drop(handle);
    // A new writer for the same day does not send it again.
    let sink2 = Arc::clone(&sent);
    let notice2: LttNoticeFn = Arc::new(move |m| sink2.lock().expect("lock").push(m));
    let handle2 = spawn_record_writer(scratch.0.clone(), Some(notice2)).expect("spawn");
    handle2.tx.send(line(RecordKind::Armed, 2)).expect("send");
    handle2.tx.send(line(RecordKind::Entry, 3)).expect("send");
    let _ = handle2
        .acks
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("ack 4");
    assert_eq!(
        sent.lock().expect("lock").len(),
        1,
        "restart never re-sends"
    );
}

#[test]
fn test_record_writer_reports_a_failed_write_as_not_durable() {
    let scratch = Scratch::new("writer-fail");
    // A FILE where the record folder should be: every write fails.
    let blocked = scratch.0.join("blocked");
    std::fs::write(&blocked, b"x").expect("write");
    let handle = spawn_record_writer(blocked, None).expect("spawn");
    handle
        .tx
        .send(RecordCmd {
            day: 20_777,
            line: RecordLine {
                kind: RecordKind::Entry,
                id: 0,
                segment: 0,
                nanos: 1,
            },
        })
        .expect("send");
    let ack = handle
        .acks
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("ack");
    assert!(!ack.durable);
}

// --- the detector -------------------------------------------------------------

#[test]
fn test_classify_vote() {
    let r = receipt(RECEIPT_1100_UTC_SECS);
    assert_eq!(classify_vote(raw_utc(RECEIPT_1100_UTC_SECS), r), Vote::Utc);
    assert_eq!(
        classify_vote(raw_normal(RECEIPT_1100_UTC_SECS), r),
        Vote::Normal
    );
    assert_eq!(
        classify_vote(raw_normal(RECEIPT_1100_UTC_SECS - 120), r),
        Vote::Normal
    );
    assert_eq!(
        classify_vote(raw_normal(RECEIPT_1100_UTC_SECS - 121), r),
        Vote::None
    );
    assert_eq!(
        classify_vote(raw_normal(RECEIPT_1100_UTC_SECS + 6), r),
        Vote::None
    );
    assert_eq!(classify_vote(0, r), Vote::None);
    assert_eq!(classify_vote(RAW_UTC_1100, 0), Vote::None);
}

fn feed(
    det: &mut LttDetector,
    segment: u8,
    instruments: u64,
    utc: u32,
    normal: u32,
) -> Option<Verdict> {
    let r = receipt(RECEIPT_1100_UTC_SECS);
    let mut out = None;
    for i in 0..utc {
        out = det
            .vote(
                segment,
                u64::from(i) % instruments,
                raw_utc(RECEIPT_1100_UTC_SECS),
                r,
            )
            .or(out);
    }
    for i in 0..normal {
        out = det
            .vote(
                segment,
                u64::from(i) % instruments,
                raw_normal(RECEIPT_1100_UTC_SECS),
                r,
            )
            .or(out);
    }
    out
}

#[test]
fn test_detector_needs_votes_distinct_instruments_and_agreement() {
    let mut det = LttDetector::new();
    assert_eq!(feed(&mut det, 0, 3, 99, 0), None, "under 100 votes");
    assert_eq!(feed(&mut det, 0, 3, 1, 0), Some(Verdict::Utc));
    let mut det = LttDetector::new();
    assert_eq!(feed(&mut det, 0, 2, 100, 0), None, "two instruments only");
    let mut det = LttDetector::new();
    assert_eq!(feed(&mut det, 0, 5, 98, 2), None, "98% is not enough");
    let mut det = LttDetector::new();
    assert_eq!(feed(&mut det, 0, 5, 99, 1), Some(Verdict::Utc), "99%");
    let mut det = LttDetector::new();
    assert_eq!(feed(&mut det, 1, 5, 0, 100), Some(Verdict::Normal));
    // Unknown segments and non-votes are ignored.
    let mut det = LttDetector::new();
    assert_eq!(
        det.vote(2, 1, RAW_UTC_1100, receipt(RECEIPT_1100_UTC_SECS)),
        None
    );
    assert_eq!(det.vote(0, 1, 0, receipt(RECEIPT_1100_UTC_SECS)), None);
    det.reset();
    assert_eq!(det, LttDetector::new());
}

// --- the permanent UTC rule (scope lock 2026-11-21) ----------------------------

/// 2026-11-23 (Monday) 09:20:00 IST as UTC epoch seconds.
const MON_0920_UTC_SECS: i64 = 1_795_113_000 + 3 * 86_400 + 9 * 3_600 + 20 * 60;

#[test]
fn test_cutover_constant_is_2026_11_20_midnight_ist() {
    let cutover = DHAN_LTT_UTC_FROM_RECEIPT_NANOS;
    assert_eq!(cutover % NANOS, 0);
    let utc = chrono::DateTime::from_timestamp(cutover / NANOS, 0).expect("in range");
    assert_eq!(utc.to_rfc3339(), "2026-11-19T18:30:00+00:00");
    assert_eq!(receipt_ist_day(cutover), date_ist_day(d("2026-11-20")));
    assert_eq!(receipt_ist_day(cutover - 1), date_ist_day(d("2026-11-19")));
    assert_eq!(
        receipt_ist_secs(cutover).rem_euclid(86_400),
        0,
        "IST midnight"
    );
}

#[test]
fn test_utc_verdict_guards() {
    let r = receipt(RECEIPT_1100_UTC_SECS);
    assert_eq!(
        utc_verdict(RAW_UTC_1100, r),
        Ok(RAW_UTC_1100 + LTT_UTC_OFFSET_SECS)
    );
    // The band: 0, the sentinels and anything outside the exchange band.
    for raw in [0, 1, MIN_PLAUSIBLE_LTT - 1, MAX_PLAUSIBLE_LTT + 1, u32::MAX] {
        assert_eq!(utc_verdict(raw, r), Err(LttRefusal::Band), "{raw}");
    }
    // An implausible receipt (not a real WAL stamp).
    assert_eq!(
        utc_verdict(RAW_UTC_1100, i64::MAX),
        Err(LttRefusal::Receipt)
    );
    // The cap is exactly receipt + 5 s.
    assert_eq!(
        utc_verdict(RAW_UTC_1100 + 5, r),
        Ok(RAW_UTC_1100 + 5 + LTT_UTC_OFFSET_SECS)
    );
    assert_eq!(utc_verdict(RAW_UTC_1100 + 6, r), Err(LttRefusal::Cap));
    assert_eq!(LttRefusal::Receipt.as_str(), "receipt");
    assert_eq!(LttRefusal::Band.as_str(), "band");
    assert_eq!(LttRefusal::Cap.as_str(), "cap");
}

#[test]
fn test_regression_print_two_days_old_corrected() {
    // Monday 09:20 IST receipt; Dhan's last trade was Friday 20 Nov 15:29 IST,
    // sent as plain UTC. The old guard refused anything older than the previous
    // day's session; Dhan says every Live LTT is UTC, so it is corrected.
    let friday_1529_utc = 1_795_113_000 + 15 * 3_600 + 29 * 60;
    let raw = raw_utc(friday_1529_utc);
    let local = LttRefusals::new();
    let out = corrected_ltt_with(raw, receipt(MON_0920_UTC_SECS), &local);
    assert_eq!(out, raw + LTT_UTC_OFFSET_SECS);
    assert_eq!(
        i64::from(out).rem_euclid(86_400),
        15 * 3_600 + 29 * 60,
        "15:29 IST"
    );
    assert_eq!(
        i64::from(out).div_euclid(86_400),
        date_ist_day(d("2026-11-20"))
    );
    assert_eq!(local.snapshot(), LttRefusalCounts::default());
    // A print three weeks old is corrected the same way.
    let old = raw_utc(friday_1529_utc - 21 * 86_400);
    assert_eq!(
        corrected_ltt_with(old, receipt(MON_0920_UTC_SECS), &local),
        old + LTT_UTC_OFFSET_SECS
    );
}

#[test]
fn test_regression_2026_11_20_first_packet_corrected_without_warm_up() {
    // 2026-11-20 08:31:22 IST, the box's first NIFTY packet: corrected with no
    // detector warm-up and no record.
    let first_utc = 1_795_113_000 + 8 * 3_600 + 31 * 60 + 22;
    let out = corrected_ltt(raw_utc(first_utc), SEGMENT_IDX_I, receipt(first_utc));
    assert_eq!(out, raw_utc(first_utc) + LTT_UTC_OFFSET_SECS);
    assert_eq!(i64::from(out).rem_euclid(86_400), 8 * 3_600 + 31 * 60 + 22);
}

fn total(c: LttRefusalCounts) -> u64 {
    c.receipt + c.band + c.cap
}

/// A raw time and a receipt at or after the cutover: near the receipt (both
/// encodings), anywhere in the band, or anything at all.
fn post_cutover_case() -> impl Strategy<Value = (u32, i64)> {
    let near =
        (0i64..400 * 86_400, -40_000i64..40_000, 0i64..NANOS).prop_map(|(after, delta, sub)| {
            let r_secs = DHAN_LTT_UTC_FROM_RECEIPT_NANOS / NANOS + after;
            let raw = u32::try_from((r_secs + delta).clamp(0, i64::from(u32::MAX))).unwrap_or(0);
            (raw, r_secs * NANOS + sub)
        });
    let wild = (any::<u32>(), DHAN_LTT_UTC_FROM_RECEIPT_NANOS..=i64::MAX);
    prop_oneof![4 => near, 1 => wild]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4096))]

    /// Every receipt before 2026-11-20 00:00 IST (0, negatives and the old WAL
    /// included), every raw `u32` and every segment: byte-identical to before
    /// (the raw value) and nothing counted.
    #[test]
    fn proptest_pre_cutover_is_byte_identical(
        raw in any::<u32>(),
        segment in any::<u8>(),
        r in prop_oneof![
            i64::MIN..DHAN_LTT_UTC_FROM_RECEIPT_NANOS,
            (DHAN_LTT_UTC_FROM_RECEIPT_NANOS - 400 * 86_400 * NANOS)..DHAN_LTT_UTC_FROM_RECEIPT_NANOS,
            Just(0i64),
            Just(-1i64),
            Just(DHAN_LTT_UTC_FROM_RECEIPT_NANOS - 1),
        ],
    ) {
        let local = LttRefusals::new();
        prop_assert_eq!(corrected_ltt_with(raw, r, &local), raw);
        prop_assert_eq!(corrected_ltt(raw, segment, r), raw);
        prop_assert_eq!(local.snapshot(), LttRefusalCounts::default());
    }

    /// Every receipt at or after the cutover: the result is exactly raw +
    /// 19,800 and no later than receipt + 5 s, or it is raw and exactly one
    /// refusal counter grew by one. The process function agrees.
    #[test]
    fn proptest_post_cutover_exact_or_refused(
        (raw, r) in post_cutover_case(),
        segment in any::<u8>(),
    ) {
        let local = LttRefusals::new();
        let out = corrected_ltt_with(raw, r, &local);
        let after = local.snapshot();
        if total(after) == 0 {
            prop_assert_eq!(Some(out), raw.checked_add(LTT_UTC_OFFSET_SECS));
            prop_assert!(i64::from(out) <= receipt_ist_secs(r) + RECEIPT_FUTURE_CAP_SECS);
            prop_assert!(utc_verdict(raw, r).is_ok());
        } else {
            prop_assert_eq!(out, raw);
            prop_assert_eq!(total(after), 1);
            let refusal = utc_verdict(raw, r).err();
            let expected = match refusal {
                Some(LttRefusal::Receipt) => LttRefusalCounts { receipt: 1, ..Default::default() },
                Some(LttRefusal::Band) => LttRefusalCounts { band: 1, ..Default::default() },
                Some(LttRefusal::Cap) => LttRefusalCounts { cap: 1, ..Default::default() },
                None => LttRefusalCounts::default(),
            };
            prop_assert_eq!(after, expected);
        }
        prop_assert_eq!(corrected_ltt(raw, segment, r), out);
    }
}

#[test]
fn test_post_cutover_proptest_strategy_reaches_every_outcome() {
    // Non-vacuity: the generator reaches a correction and each refusal.
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let strategy = post_cutover_case();
    let (mut ok, mut band, mut cap, mut rec) = (0, 0, 0, 0);
    for _ in 0..4_096 {
        let (raw, r) = strategy.new_tree(&mut runner).expect("tree").current();
        match utc_verdict(raw, r) {
            Ok(_) => ok += 1,
            Err(LttRefusal::Band) => band += 1,
            Err(LttRefusal::Cap) => cap += 1,
            Err(LttRefusal::Receipt) => rec += 1,
        }
    }
    assert!(
        ok > 100 && band > 10 && cap > 100 && rec > 10,
        "{ok} {band} {cap} {rec}"
    );
}

// --- the reversion guard (noise lock §2.24-a) -----------------------------------

/// `windows` vote windows (20 frames of 5 distinct index or stock prints).
fn guard_frames(ctl: &mut LttController, start_utc_secs: i64, windows: i64, seg: u8, utc: bool) {
    for s in 0..windows * 20 {
        let t = start_utc_secs + s;
        let r = receipt(t);
        ctl.on_frame(r);
        for i in 0..5u64 {
            let raw = if utc { raw_utc(t) } else { raw_normal(t) };
            ctl.observe(seg, i, raw, r);
        }
    }
}

#[test]
fn test_regression_ist_epoch_fresh_print_stays_raw_counted_and_pages_once_per_segment_per_day() {
    // Dhan reverts to IST epoch: a fresh print is refused by the cap, left raw
    // (which is then the right IST time), and counted.
    let t = RECEIPT_1100_UTC_SECS;
    let raw = raw_normal(t);
    let local = LttRefusals::new();
    assert_eq!(corrected_ltt_with(raw, receipt(t), &local), raw);
    assert_eq!(
        local.snapshot(),
        LttRefusalCounts {
            cap: 1,
            ..Default::default()
        }
    );
    let before = refusal_counts().cap;
    assert_eq!(corrected_ltt(raw, SEGMENT_IDX_I, receipt(t)), raw);
    assert!(refusal_counts().cap > before, "the process counter moved");

    // The guard pages once per segment per IST day.
    let mut ctl = LttController::new(LttUtcAutocorrect::Act);
    guard_frames(&mut ctl, t, 3, SEGMENT_IDX_I, false);
    assert_eq!(ctl.counts().normal_verdicts, 3);
    assert_eq!(ctl.counts().reversion_reports, 1, "once for index prints");
    guard_frames(&mut ctl, t + 100, 2, SEGMENT_NSE_EQ, false);
    assert_eq!(ctl.counts().reversion_reports, 2, "once for stock prints");
    guard_frames(&mut ctl, t + 200, 2, SEGMENT_IDX_I, false);
    assert_eq!(ctl.counts().reversion_reports, 2, "not again the same day");
    // The next IST day, same time: once more.
    guard_frames(&mut ctl, t + 86_400, 2, SEGMENT_IDX_I, false);
    assert_eq!(ctl.counts().reversion_reports, 3);
}

#[test]
fn test_utc_verdict_never_pages() {
    let mut ctl = LttController::new(LttUtcAutocorrect::Act);
    guard_frames(&mut ctl, RECEIPT_1100_UTC_SECS, 5, SEGMENT_IDX_I, true);
    guard_frames(
        &mut ctl,
        RECEIPT_1100_UTC_SECS + 200,
        5,
        SEGMENT_NSE_EQ,
        true,
    );
    assert_eq!(ctl.counts().utc_verdicts, 10);
    assert_eq!(ctl.counts().normal_verdicts, 0);
    assert_eq!(ctl.counts().reversion_reports, 0);
}

#[test]
fn test_guard_off_never_pages() {
    let mut ctl = LttController::new(LttUtcAutocorrect::Off);
    guard_frames(&mut ctl, RECEIPT_1100_UTC_SECS, 3, SEGMENT_IDX_I, false);
    assert_eq!(ctl.counts().votes, 0);
    assert_eq!(ctl.counts().reversion_reports, 0);
    let mut inert = LttController::inert();
    guard_frames(&mut inert, RECEIPT_1100_UTC_SECS, 3, SEGMENT_IDX_I, false);
    assert_eq!(inert.counts(), ControllerCounts::default());
    // Shadow logs a warn once (counted) and never an error.
    let mut shadow = LttController::new(LttUtcAutocorrect::Shadow);
    guard_frames(&mut shadow, RECEIPT_1100_UTC_SECS, 3, SEGMENT_IDX_I, false);
    assert_eq!(shadow.counts().reversion_reports, 1);
}

#[test]
fn test_guard_is_quiet_before_the_cutover_and_outside_the_window() {
    let mut ctl = LttController::new(LttUtcAutocorrect::Act);
    // 2026-11-19 11:00 IST: a normal day before the change.
    guard_frames(
        &mut ctl,
        RECEIPT_1100_UTC_SECS - 86_400,
        3,
        SEGMENT_IDX_I,
        false,
    );
    assert_eq!(ctl.counts().normal_verdicts, 3);
    assert_eq!(ctl.counts().reversion_reports, 0, "before the cutover");
    // 2026-11-20 16:00 IST: after the capture window.
    guard_frames(
        &mut ctl,
        RECEIPT_1100_UTC_SECS + 5 * 3_600,
        3,
        SEGMENT_IDX_I,
        false,
    );
    assert_eq!(ctl.counts().reversion_reports, 0, "outside 09:00–15:40");
    // Unknown segments never vote.
    guard_frames(&mut ctl, RECEIPT_1100_UTC_SECS, 3, 2, false);
    assert!(in_guard_window(receipt(RECEIPT_1100_UTC_SECS)));
    assert!(!in_guard_window(receipt(RECEIPT_1100_UTC_SECS + 5 * 3_600)));
}

#[test]
fn test_controller_publishes_once_a_minute_of_receipts() {
    let mut ctl = LttController::new(LttUtcAutocorrect::Act);
    let minute = (RECEIPT_1100_UTC_SECS / 60) * 60;
    ctl.on_frame(receipt(minute));
    ctl.on_frame(receipt(minute + 30));
    assert_eq!(ctl.counts().publishes, 1);
    ctl.on_frame(receipt(minute + 60));
    assert_eq!(ctl.counts().publishes, 2);
    ctl.on_frame(0); // not a WAL stamp: ignored
    assert_eq!(ctl.counts().publishes, 2);
    let mut inert = LttController::inert();
    inert.on_frame(receipt(minute));
    assert_eq!(inert.counts().publishes, 0);
}

#[test]
fn test_install_at_boot_reads_nothing_and_holds_nothing() {
    // A leftover record with a dangling intent (it held the day before
    // 2026-11-21) is ignored and kept.
    let scratch = Scratch::new("boot-ignored");
    let r = receipt(RECEIPT_1100_UTC_SECS);
    let day = receipt_ist_day(r);
    append_line(
        &scratch.0,
        day,
        &RecordLine {
            kind: RecordKind::Entry,
            id: 0,
            segment: 0,
            nanos: r,
        },
    )
    .expect("append");
    install_at_boot(&scratch.0, LttUtcAutocorrect::Act);
    assert!(!any_receipt_day_held(std::iter::once(r)));
    assert_eq!(current_table(), LttTable::empty());
    assert!(record_path(&scratch.0, day).expect("path").exists(), "kept");
    assert_eq!(BOOT_REPORT.get().map(|b| b.mode), Some("act"));
    log_boot_summary();
}

#[test]
fn test_regression_review_a_reclose_after_a_voided_close_ends_the_entry() {
    // Review fix 2026-11-21: C, X (failed attempt), then C, K must replay as
    // closed at K, and C, X, C with no K must hold the day.
    use RecordKind::*;
    let closed = DayRecord::parse(&lines(&[
        (Entry, 0, 1),
        (Armed, 0, 100),
        (Close, 0, 0),
        (CloseVoid, 0, 0),
        (Close, 0, 0),
        (Closed, 0, 300),
    ]));
    assert!(!closed.held());
    assert_eq!(closed.armed_entries(), vec![entry(0, 0, 100, 300)]);
    let dangling = DayRecord::parse(&lines(&[
        (Entry, 0, 1),
        (Armed, 0, 100),
        (Close, 0, 0),
        (CloseVoid, 0, 0),
        (Close, 0, 0),
    ]));
    assert!(dangling.held());
}

#[test]
fn test_regression_review_a_record_day_older_than_the_window_is_held() {
    let scratch = Scratch::new("old-day");
    let r = receipt(RECEIPT_1100_UTC_SECS);
    let day = receipt_ist_day(r);
    let line = RecordLine {
        kind: RecordKind::Entry,
        id: 0,
        segment: 0,
        nanos: r,
    };
    append_line(&scratch.0, day, &line).expect("append");
    append_line(
        &scratch.0,
        day,
        &RecordLine {
            kind: RecordKind::Armed,
            ..line
        },
    )
    .expect("append");
    let later = day + BOOT_LOAD_DAYS + 5;
    let (table, _) = load_record_dir(&scratch.0, later, LttUtcAutocorrect::Act);
    assert!(table.any_held());
    assert!(
        table.receipt_day_held(r),
        "an unloaded corrected day is held"
    );
    assert!(
        !table.receipt_day_held(r + 86_400 * NANOS),
        "other days are not"
    );
}
