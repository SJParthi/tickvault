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

// --- the guard ---------------------------------------------------------------

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

// --- the controller -------------------------------------------------------------

struct Rig {
    ctl: LttController,
    cmds: Receiver<RecordCmd>,
    acks: Sender<RecordAck>,
    written: Vec<(i64, RecordLine)>,
}

fn noop_publish(_: LttTable) {}

impl Rig {
    fn new(mode: LttUtcAutocorrect) -> Self {
        let (tx, cmds) = crossbeam_channel::bounded(64);
        let (acks, ack_rx) = crossbeam_channel::bounded(16);
        let ctl = LttController::with_table(
            mode,
            Some(RecordWriterHandle::from_channels(tx, ack_rx)),
            LttTable::empty(),
            noop_publish,
        );
        Self {
            ctl,
            cmds,
            acks,
            written: Vec::new(),
        }
    }

    /// The record writer: writes every queued line and acks intents.
    fn pump(&mut self, durable: bool) {
        while let Ok(cmd) = self.cmds.try_recv() {
            if durable {
                self.written.push((cmd.day, cmd.line));
            }
            if cmd.line.kind.is_intent() {
                self.acks
                    .send(RecordAck {
                        day: cmd.day,
                        id: cmd.line.id,
                        kind: cmd.line.kind,
                        durable,
                    })
                    .expect("ack");
            }
        }
    }

    /// One frame of `n` index ticks from distinct instruments.
    fn frame(&mut self, utc_secs: i64, utc: bool) {
        let r = receipt(utc_secs);
        self.ctl.on_frame(r);
        for i in 0..5u64 {
            let raw = if utc {
                raw_utc(utc_secs)
            } else {
                raw_normal(utc_secs)
            };
            self.ctl.observe(SEGMENT_IDX_I, i, raw, r);
        }
    }

    fn replay_table(&self) -> LttTable {
        let mut by_day: std::collections::BTreeMap<i64, DayRecord> = Default::default();
        for (day, line) in &self.written {
            by_day.entry(*day).or_default().apply(line);
        }
        let mut table = LttTable::empty();
        for (day, record) in &by_day {
            table.install_day(*day, record);
        }
        table
    }
}

#[test]
fn test_controller_arms_from_one_past_every_dispatched_receipt() {
    let mut rig = Rig::new(LttUtcAutocorrect::Act);
    let t = RECEIPT_1100_UTC_SECS;
    for s in 0..20 {
        rig.frame(t + s, true);
    }
    // The intent is queued; nothing is armed until it is durable.
    assert_eq!(rig.ctl.counts().armed, 0);
    assert!(!rig.ctl.table().any_entry());
    rig.pump(true);
    rig.frame(t + 20, true);
    assert_eq!(rig.ctl.counts().armed, 1);
    let day = receipt_ist_day(receipt(t));
    let armed = rig.ctl.table().day(day).expect("day").entries()[0];
    assert_eq!(
        armed.from_nanos,
        receipt(t + 19) + 1,
        "past the last dispatched receipt"
    );
    assert_eq!(armed.segment, SEGMENT_IDX_I);
    rig.pump(true);
    let kinds: Vec<RecordKind> = rig.written.iter().map(|(_, l)| l.kind).collect();
    assert_eq!(kinds, vec![RecordKind::Entry, RecordKind::Armed]);
    assert_eq!(rig.replay_table(), rig.ctl.table().clone());
    assert!(rig.ctl.open_on(SEGMENT_IDX_I));
}

#[test]
fn test_controller_closes_on_normal_evidence_with_a_two_step_commit() {
    let mut rig = Rig::new(LttUtcAutocorrect::Act);
    let t = RECEIPT_1100_UTC_SECS;
    for s in 0..20 {
        rig.frame(t + s, true);
    }
    rig.pump(true);
    for s in 20..40 {
        rig.frame(t + s, false);
    }
    rig.pump(true);
    rig.frame(t + 40, false);
    assert_eq!(rig.ctl.counts().closed, 1);
    assert!(!rig.ctl.open_on(SEGMENT_IDX_I));
    rig.pump(true);
    let kinds: Vec<RecordKind> = rig.written.iter().map(|(_, l)| l.kind).collect();
    assert_eq!(
        kinds,
        vec![
            RecordKind::Entry,
            RecordKind::Armed,
            RecordKind::Close,
            RecordKind::Closed
        ]
    );
    assert_eq!(rig.replay_table(), rig.ctl.table().clone());
}

#[test]
fn test_a_failed_intent_is_voided_and_never_armed() {
    let mut rig = Rig::new(LttUtcAutocorrect::Act);
    let t = RECEIPT_1100_UTC_SECS;
    for s in 0..20 {
        rig.frame(t + s, true);
    }
    rig.pump(false);
    rig.frame(t + 20, true);
    assert_eq!(rig.ctl.counts().armed, 0);
    assert_eq!(rig.ctl.counts().intents_failed, 1);
    rig.pump(true);
    assert_eq!(
        rig.written.last().map(|(_, l)| l.kind),
        Some(RecordKind::Void)
    );
}

#[test]
fn test_shadow_and_off_write_nothing() {
    for mode in [LttUtcAutocorrect::Shadow, LttUtcAutocorrect::Off] {
        let mut rig = Rig::new(mode);
        for s in 0..40 {
            rig.frame(RECEIPT_1100_UTC_SECS + s, true);
        }
        rig.pump(true);
        assert!(rig.written.is_empty(), "{mode:?}");
        assert!(!rig.ctl.table().any_entry());
    }
}

#[test]
fn test_a_change_pending_at_ist_midnight_is_voided() {
    let mut rig = Rig::new(LttUtcAutocorrect::Act);
    let day = date_ist_day(d("2026-11-21"));
    let midnight_utc = day * 86_400 - IST_OFFSET_SECS;
    for s in 0..20 {
        rig.frame(midnight_utc - 30 + s, true);
    }
    // The next frame is past IST midnight: the pending intent is voided for
    // the old day and the detector starts over.
    rig.frame(midnight_utc + 1, true);
    rig.pump(true);
    let kinds: Vec<(i64, RecordKind)> = rig.written.iter().map(|(d, l)| (*d, l.kind)).collect();
    assert_eq!(
        kinds,
        vec![(day - 1, RecordKind::Entry), (day - 1, RecordKind::Void)]
    );
    assert!(!rig.ctl.table().any_entry());
}

#[test]
fn test_the_detector_freezes_after_eight_entries_a_day() {
    let mut rig = Rig::new(LttUtcAutocorrect::Act);
    let mut s = RECEIPT_1100_UTC_SECS;
    for _ in 0..10 {
        for _ in 0..20 {
            rig.frame(s, true);
            s += 1;
        }
        rig.pump(true);
        // The first normal frame applies the arm; 20 frames close it.
        for _ in 0..20 {
            rig.frame(s, false);
            s += 1;
        }
        rig.pump(true);
    }
    rig.frame(s, false);
    rig.pump(true);
    assert_eq!(rig.ctl.counts().armed, 8);
    assert!(rig.ctl.counts().frozen);
    assert_eq!(rig.replay_table(), rig.ctl.table().clone());
}

#[test]
fn test_a_restart_continues_an_open_entry() {
    let r = receipt(RECEIPT_1100_UTC_SECS);
    let day = receipt_ist_day(r);
    let mut table = LttTable::empty();
    assert!(table.arm(day, entry(0, SEGMENT_IDX_I, r, i64::MAX)));
    let (tx, cmds) = crossbeam_channel::bounded(64);
    let (_acks, ack_rx) = crossbeam_channel::bounded::<RecordAck>(16);
    let mut ctl = LttController::with_table(
        LttUtcAutocorrect::Act,
        Some(RecordWriterHandle::from_channels(tx, ack_rx)),
        table,
        noop_publish,
    );
    ctl.on_frame(r + NANOS);
    assert!(ctl.open_on(SEGMENT_IDX_I));
    // A UTC verdict proposes nothing new on the open segment.
    for i in 0..100u64 {
        ctl.observe(
            SEGMENT_IDX_I,
            i % 5,
            raw_utc(RECEIPT_1100_UTC_SECS + 1),
            r + NANOS,
        );
    }
    assert!(cmds.try_recv().is_err());
}

// --- live vs replay identity, with crash injection ---------------------------

#[derive(Debug, Clone)]
struct Step {
    jitter_ms: i64,
    pump: bool,
    stale_secs: i64,
}

fn step() -> impl Strategy<Value = Step> {
    (-900i64..900, prop::bool::weighted(0.3), 0i64..400).prop_map(
        |(jitter_ms, pump, stale_secs)| Step {
            jitter_ms,
            pump,
            stale_secs,
        },
    )
}

/// Drives the live controller through `phases` (UTC or normal, in frames)
/// with jittered receipts and a record writer that runs at random steps, then
/// rebuilds the table from the lines written up to `crash_at` and checks that
/// every packet dispatched by then gets the same decision (or its day is
/// held). Returns `(armed, closed, packets compared)`.
fn check_identity(phases: &[(bool, usize)], steps: &[Step], crash_at: usize) -> (u32, u32, usize) {
    let mut rig = Rig::new(LttUtcAutocorrect::Act);
    // (step index, segment, raw, receipt, live result)
    let mut decisions: Vec<(usize, u8, u32, i64, Option<u32>)> = Vec::new();
    let mut lines_at_step: Vec<usize> = Vec::new();
    let mut base = RECEIPT_1100_UTC_SECS * 1_000;
    let mut idx = 0usize;
    'outer: for &(utc, len) in phases {
        for _ in 0..len {
            let Some(s) = steps.get(idx) else {
                break 'outer;
            };
            base += 1_000;
            let ms = base + s.jitter_ms;
            let r = ms * 1_000_000;
            // The drain: the frame first (it may apply an acked change), then
            // each packet is dispatched (corrected) and voted on.
            rig.ctl.on_frame(r);
            let secs = ms.div_euclid(1_000);
            for i in 0..5u64 {
                let raw = if utc { raw_utc(secs) } else { raw_normal(secs) };
                let live = rig.ctl.table().correct(raw, SEGMENT_IDX_I, r);
                decisions.push((idx, SEGMENT_IDX_I, raw, r, live));
                rig.ctl.observe(SEGMENT_IDX_I, i, raw, r);
            }
            // An option print on an unconfirmed segment, possibly stale.
            let opt_secs = secs - s.stale_secs;
            let opt_raw = if utc {
                raw_utc(opt_secs)
            } else {
                raw_normal(opt_secs)
            };
            let live = rig.ctl.table().correct(opt_raw, 2, r);
            decisions.push((idx, 2, opt_raw, r, live));
            if s.pump {
                rig.pump(true);
            }
            lines_at_step.push(rig.written.len());
            idx += 1;
        }
    }
    let crash = crash_at.min(lines_at_step.len().saturating_sub(1));
    let kept = lines_at_step.get(crash).copied().unwrap_or(0);
    let mut by_day: std::collections::BTreeMap<i64, DayRecord> = Default::default();
    for (day, line) in &rig.written[..kept] {
        by_day.entry(*day).or_default().apply(line);
    }
    let mut replay = LttTable::empty();
    for (day, record) in &by_day {
        replay.install_day(*day, record);
    }
    let mut compared = 0usize;
    for &(at, seg, raw, r, live) in &decisions {
        if at > crash || replay.receipt_day_held(r) {
            continue;
        }
        assert_eq!(replay.correct(raw, seg, r), live, "step {at} seg {seg}");
        compared += 1;
    }
    let counts = rig.ctl.counts();
    (counts.armed, counts.closed, compared)
}

#[test]
fn test_live_and_replay_agree_on_a_fixed_flip_and_flip_back() {
    // Non-vacuity: this scenario arms twice and closes once, with jittered
    // (reordered) receipts and a writer that runs every third frame.
    let steps: Vec<Step> = (0..400)
        .map(|i| Step {
            jitter_ms: (i as i64 * 397) % 1_700 - 850,
            pump: i % 3 == 0,
            stale_secs: (i as i64 * 53) % 400,
        })
        .collect();
    let phases = [(false, 30), (true, 40), (false, 40), (true, 40)];
    let (armed, closed, compared) = check_identity(&phases, &steps, 399);
    assert_eq!((armed, closed), (2, 1), "the scenario must arm and close");
    assert!(compared > 500, "{compared} packets compared");
    // A crash in the middle compares fewer packets, never a different answer.
    for crash in [0, 37, 71, 72, 73, 110, 111] {
        let _ = check_identity(&phases, &steps, crash);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For every packet, the replay (a table rebuilt from the record lines
    /// written up to a crash point) decides exactly as the live table did when
    /// the packet was dispatched — or the day is held.
    #[test]
    fn proptest_live_and_replay_agree_for_every_packet_or_the_day_is_held(
        phases in prop::collection::vec((any::<bool>(), 10usize..40), 1..6),
        steps in prop::collection::vec(step(), 400),
        crash_at in 0usize..400,
    ) {
        let (armed, closed, _compared) = check_identity(&phases, &steps, crash_at);
        // A close always follows its own arm (and the per-packet identity is
        // asserted inside `check_identity`).
        prop_assert!(closed <= armed, "closed {} > armed {}", closed, armed);
        prop_assert!(armed <= MAX_ENTRIES_PER_DAY as u32);
    }
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
