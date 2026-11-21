//! Unit tests for the trade-safety switch (plan item UTC-SAFETY).

use super::*;
use proptest::prelude::*;

const DAY: i64 = 20_778; // 2026-11-21 IST

#[test]
fn test_entries_blocked_on_same_day_next_day_and_earlier_day() {
    let s = SafetySwitch::new();
    assert_eq!(
        s.entries_blocked_on(DAY),
        None,
        "a fresh switch blocks nothing"
    );
    assert!(s.engage(DAY, CAUSE_PRICES_OUTSIDE_SESSION));
    assert_eq!(
        s.entries_blocked_on(DAY),
        Some(CAUSE_PRICES_OUTSIDE_SESSION)
    );
    assert_eq!(
        s.entries_blocked_on(DAY + 1),
        None,
        "it ends at the day roll"
    );
    assert_eq!(
        s.entries_blocked_on(DAY - 1),
        None,
        "yesterday is not blocked"
    );
    // The same bit again changes nothing; the other cause ORs in.
    assert!(!s.engage(DAY, CAUSE_PRICES_OUTSIDE_SESSION));
    assert!(s.engage(DAY, CAUSE_NO_CANDLES));
    assert_eq!(s.entries_blocked_on(DAY), Some(3));
    // An earlier day (a clock stepped back) never moves the switch back.
    assert!(!s.engage(DAY - 1, CAUSE_NO_CANDLES));
    assert_eq!(s.entries_blocked_on(DAY - 1), None);
    assert_eq!(s.entries_blocked_on(DAY), Some(3));
}

#[test]
fn test_a_new_day_resets_the_cause_bits() {
    let s = SafetySwitch::new();
    assert!(s.engage(DAY, CAUSE_PRICES_OUTSIDE_SESSION | CAUSE_NO_CANDLES));
    assert!(s.engage(DAY + 1, CAUSE_NO_CANDLES));
    assert_eq!(s.entries_blocked_on(DAY + 1), Some(CAUSE_NO_CANDLES));
    assert_eq!(s.entries_blocked_on(DAY), None);
}

#[test]
fn test_zero_bit_and_extreme_days_never_misread() {
    let s = SafetySwitch::default();
    assert!(!s.engage(DAY, 0), "no cause, no change");
    assert_eq!(s.entries_blocked_on(DAY), None);
    // A negative day (a box with no clock yet) packs and reads back.
    assert!(s.engage(-5, CAUSE_NO_CANDLES));
    assert_eq!(s.entries_blocked_on(-5), Some(CAUSE_NO_CANDLES));
    assert!(s.engage(DAY, CAUSE_NO_CANDLES));
    assert_eq!(s.entries_blocked_on(-5), None);
    // The never-engaged word's day can never read blocked.
    let fresh = SafetySwitch::new();
    assert_eq!(fresh.entries_blocked_on(i64::MIN >> 8), None);
    assert_eq!(fresh.entries_blocked_on(i64::MIN), None);
}

#[test]
fn test_ist_day_now_equals_the_drain_and_chain_day_formulas() {
    // The drain's day and the chain's day (IST nanos / 1e9 / 86,400) must be
    // the switch's day, or a gate would read another day's flag.
    let before = crate::dhan_feed_stack::ist_day_number_now();
    let ours = ist_day_now();
    let chain_day = crate::live_fill::wall_ist_ns()
        .div_euclid(1_000_000_000)
        .div_euclid(86_400);
    let after = crate::dhan_feed_stack::ist_day_number_now();
    assert!(ours == before || ours == after, "{before} {ours} {after}");
    assert!(chain_day == before || chain_day == after);
}

#[test]
fn test_ist_day_of_utc_secs_floors_at_the_ist_boundary() {
    // The IST boundary: 18:29:59 UTC is still the same IST day, 18:30:00 the next.
    let midnight_ist_utc = 1_795_199_400; // 2026-11-21 00:00 IST
    assert_eq!(ist_day_of_utc_secs(midnight_ist_utc), DAY);
    assert_eq!(ist_day_of_utc_secs(midnight_ist_utc - 1), DAY - 1);
    assert_eq!(ist_day_of_utc_secs(-20_000), -1, "floor, not truncation");
}

#[test]
fn test_register_metrics_and_process_engage_set_the_gauge() {
    // Read through a real Prometheus recorder (2026-11-21 review finding 6:
    // this test asserted nothing about either series). `register_metrics`
    // reads the PROCESS switch, which another test in this binary may have
    // engaged for today, so the gauge it writes must equal that switch's
    // value, never a blind 0; the counter is registered at 0.
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    let line = |text: &str, name: &str| {
        text.lines()
            .find(|l| l.starts_with(name))
            .map(str::to_owned)
    };
    let process_value = metrics::with_local_recorder(&recorder, || {
        register_metrics();
        if FEED_TIME_BLOCK.entries_blocked_on(ist_day_now()).is_some() {
            "1"
        } else {
            "0"
        }
    });
    let text = handle.render();
    assert_eq!(
        line(&text, ENTRIES_BLOCKED_COUNTER).as_deref(),
        Some("tv_live_fill_entries_blocked_feed_time_total 0")
    );
    assert_eq!(
        line(&text, ENTRIES_BLOCKED_GAUGE),
        Some(format!("{ENTRIES_BLOCKED_GAUGE} {process_value}"))
    );
    // The process engage on a test's own switch for TODAY changes the switch
    // once and sets the gauge to 1 (the gauge reads the switch passed in).
    let s: &'static SafetySwitch = Box::leak(Box::new(SafetySwitch::new()));
    let today = ist_day_now();
    metrics::with_local_recorder(&recorder, || {
        assert!(engage(s, today, CAUSE_NO_CANDLES));
        assert!(!engage(s, today, CAUSE_NO_CANDLES));
    });
    assert_eq!(s.entries_blocked_on(today), Some(CAUSE_NO_CANDLES));
    assert_eq!(
        line(&handle.render(), ENTRIES_BLOCKED_GAUGE),
        Some(format!("{ENTRIES_BLOCKED_GAUGE} 1"))
    );
    // The process switch is untouched by a test's own.
    assert_eq!(FEED_TIME_BLOCK.entries_blocked_on(i64::MAX >> 8), None);
}

fn hold_dir(tag: &str) -> std::path::PathBuf {
    let base =
        std::env::temp_dir().join(format!("tv_trade_safety_hold_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("dir");
    base
}

/// The hold-file reader behind the restart fix (2026-11-21 review finding 2):
/// missing reads 0, a feed-time hold reads its cause (both bits for an unknown
/// label), another reason reads 0, and a damaged or unreadable file reads 0
/// (only the page's own whole hold blocks; a hand hold does not).
#[test]
fn test_feed_time_hold_bits_reads_the_page_hold_fail_closed() {
    use tickvault_storage::held_days::{MarkerKind, day_marker_path, write_day_marker};
    let dir = hold_dir("bits");
    let date = chrono::NaiveDate::from_ymd_opt(2026, 11, 21).expect("date");
    assert_eq!(feed_time_hold_bits(&dir, date), 0, "no file");
    for (label, bit) in [
        ("prices_outside_session", CAUSE_PRICES_OUTSIDE_SESSION),
        ("no_candles", CAUSE_NO_CANDLES),
        ("a_cause_from_a_later_build", 3),
    ] {
        write_day_marker(
            &dir,
            MarkerKind::Hold,
            date,
            date,
            &format!("{FEED_TIME_HOLD_REASON_PREFIX}{label}"),
        )
        .expect("hold");
        assert_eq!(feed_time_hold_bits(&dir, date), bit, "{label}");
    }
    write_day_marker(&dir, MarkerKind::Hold, date, date, "manual repair hold").expect("hold");
    assert_eq!(feed_time_hold_bits(&dir, date), 0, "another reason");
    // A torn (empty) hold, as a hand hold is: not the page's, 0.
    std::fs::write(day_marker_path(&dir, MarkerKind::Hold, date), b"").expect("torn");
    assert_eq!(feed_time_hold_bits(&dir, date), 0, "damaged");
    // Over the size cap: 0.
    std::fs::write(
        day_marker_path(&dir, MarkerKind::Hold, date),
        vec![b'x'; 5000],
    )
    .expect("big");
    assert_eq!(feed_time_hold_bits(&dir, date), 0, "too long");
    // A path that is a folder (cannot be read): 0.
    let _ = std::fs::remove_file(day_marker_path(&dir, MarkerKind::Hold, date));
    std::fs::create_dir_all(day_marker_path(&dir, MarkerKind::Hold, date)).expect("dir");
    assert_eq!(feed_time_hold_bits(&dir, date), 0, "unreadable");
    // Another day's hold says nothing about this day.
    let next = date.succ_opt().expect("next");
    assert_eq!(feed_time_hold_bits(&dir, next), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_racing_engages_on_a_new_day_never_lose_a_bit() {
    for _ in 0..200 {
        let s = std::sync::Arc::new(SafetySwitch::new());
        assert!(s.engage(DAY - 1, CAUSE_PRICES_OUTSIDE_SESSION));
        let a = std::sync::Arc::clone(&s);
        let b = std::sync::Arc::clone(&s);
        let ta = std::thread::spawn(move || a.engage(DAY, CAUSE_PRICES_OUTSIDE_SESSION));
        let tb = std::thread::spawn(move || b.engage(DAY, CAUSE_NO_CANDLES));
        let _ = ta.join();
        let _ = tb.join();
        assert_eq!(s.entries_blocked_on(DAY), Some(3));
    }
}

/// The oracle: the newest day ever engaged, and the bits engaged on it.
#[derive(Default)]
struct Oracle {
    day: Option<i64>,
    bits: u8,
}

impl Oracle {
    fn engage(&mut self, day: i64, bit: u8) {
        if bit == 0 {
            return;
        }
        match self.day {
            Some(d) if d > day => {}
            Some(d) if d == day => self.bits |= bit,
            _ => {
                self.day = Some(day);
                self.bits = bit;
            }
        }
    }
    fn blocked(&self, day: i64) -> Option<u8> {
        (self.day == Some(day) && self.bits != 0).then_some(self.bits)
    }
}

proptest! {
    /// Random engages and reads against the oracle: the switch reads exactly
    /// what the oracle says, on every day asked, after every step.
    #[test]
    fn proptest_switch_matches_the_oracle(
        steps in proptest::collection::vec((0i64..6, 0u8..4, 0i64..6), 1..64),
    ) {
        let s = SafetySwitch::new();
        let mut o = Oracle::default();
        for (engage_day, bit, read_day) in steps {
            let (d, r) = (DAY + engage_day, DAY + read_day);
            let changed = s.engage(d, bit);
            let before = (o.day, o.bits);
            o.engage(d, bit);
            prop_assert_eq!(changed, before != (o.day, o.bits));
            prop_assert_eq!(s.entries_blocked_on(r), o.blocked(r));
            prop_assert_eq!(s.entries_blocked_on(d), o.blocked(d));
        }
    }
}

#[test]
fn test_publish_gauge_reads_one_only_on_a_blocked_day() {
    let s = SafetySwitch::new();
    assert_eq!(publish_gauge(&s, DAY), 0.0, "nothing engaged");
    assert!(s.engage(DAY, CAUSE_NO_CANDLES));
    assert_eq!(publish_gauge(&s, DAY), 1.0, "blocked today");
    assert_eq!(publish_gauge(&s, DAY + 1), 0.0, "next day reads clear");
    assert_eq!(publish_gauge(&s, DAY - 1), 0.0, "earlier day reads clear");
}

#[test]
fn test_report_engaged_leaves_the_switch_unchanged() {
    let s = SafetySwitch::new();
    assert!(s.engage(DAY, CAUSE_PRICES_OUTSIDE_SESSION));
    report_engaged(&s, DAY);
    assert_eq!(
        s.entries_blocked_on(DAY),
        Some(CAUSE_PRICES_OUTSIDE_SESSION),
        "reporting never changes what is blocked"
    );
    // A report for a day that is not blocked is harmless too.
    report_engaged(&s, DAY + 3);
    assert_eq!(s.entries_blocked_on(DAY + 3), None);
}

#[test]
fn test_process_feed_time_hold_bits_reads_the_process_folder_without_panicking() {
    // The process folder is relative to the working directory; in a test run
    // it holds no feed-time hold for a far-future date, so the answer is 0.
    let far_future = chrono::NaiveDate::from_ymd_opt(2099, 1, 1).expect("valid date");
    assert_eq!(process_feed_time_hold_bits(far_future), 0);
}

#[test]
fn test_engage_from_feed_time_hold_engages_each_recorded_cause() {
    let s = SafetySwitch::new();
    let none = crate::live_fill::engage_from_feed_time_hold(&s, DAY, |_| 0);
    assert_eq!(none, 0);
    assert_eq!(s.entries_blocked_on(DAY), None, "no hold, no block");
    let bits = crate::live_fill::engage_from_feed_time_hold(&s, DAY, |_| CAUSE_NO_CANDLES);
    assert_eq!(bits, CAUSE_NO_CANDLES);
    assert_eq!(s.entries_blocked_on(DAY), Some(CAUSE_NO_CANDLES));
    let both = CAUSE_PRICES_OUTSIDE_SESSION | CAUSE_NO_CANDLES;
    let _ = crate::live_fill::engage_from_feed_time_hold(&s, DAY, |_| both);
    assert_eq!(s.entries_blocked_on(DAY), Some(both));
}
