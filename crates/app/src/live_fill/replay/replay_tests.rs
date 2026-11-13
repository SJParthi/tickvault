#![cfg(test)]
//! Tests of the offline live-fill replay (plan item LF-8). Each was written
//! to fail before the code it covers existed.

use tickvault_common::config::LiveFillConfig;
use tickvault_storage::live_order_events_persistence::LiveOrderEventKind;
use tickvault_storage::ws_frame_spill::{ReplayedFrame, WalEndpoint, WsType};
use tickvault_trading::brutex_host::InstrumentKey;
use tickvault_trading::brutex_host::engine::{BarInput, IntentSink, StrategyEngine};
use tickvault_trading::brutex_host::live_candles::IST_OFFSET_NS;

use super::drive::{
    BootInstant, DecisionTiming, ReplayDriver, ReplayOutput, decision_timings, grid_end_ns,
    key_label, tail_ns,
};
use super::out;
use super::scripted::{parse_schedule, parse_segment};
use super::source::{input_segment_name, list_wal_inputs, walk_frame};
use super::*;

const NS: i64 = 1_000_000_000;
/// 2026-11-18 (a Wednesday) 00:00 IST, IST epoch seconds.
const DAY: i64 = 1_794_960_000;
const DAY_TEXT: &str = "2026-11-18";
const A: u32 = 2885;
const B: u32 = 1333;

fn sod(h: i64, m: i64, s: i64) -> i64 {
    DAY + h * 3_600 + m * 60 + s
}

fn quote(sid: u32, ltp: f32, ltt: i64, volume: u32) -> Vec<u8> {
    let mut b = vec![0u8; 50];
    b[0] = 4;
    b[1..3].copy_from_slice(&50u16.to_le_bytes());
    b[3] = 1; // NSE_EQ
    b[4..8].copy_from_slice(&sid.to_le_bytes());
    b[8..12].copy_from_slice(&ltp.to_le_bytes());
    b[12..14].copy_from_slice(&1u16.to_le_bytes());
    b[14..18].copy_from_slice(&u32::try_from(ltt).expect("fits").to_le_bytes());
    b[18..22].copy_from_slice(&ltp.to_le_bytes());
    b[22..26].copy_from_slice(&volume.to_le_bytes());
    for o in [34, 38, 42, 46] {
        b[o..o + 4].copy_from_slice(&ltp.to_le_bytes());
    }
    b
}

fn frame(bytes: Vec<u8>, receipt_ist_ns: i64) -> ReplayedFrame {
    ReplayedFrame {
        ws_type: WsType::LiveFeed,
        frame: bytes,
        frame_seq: 0,
        received_at_nanos: receipt_ist_ns - IST_OFFSET_NS,
        endpoint: WalEndpoint::MainFeed,
        after_gap: false,
        first_in_segment: false,
    }
}

fn params() -> ChainParams {
    ChainParams::from(&LiveFillConfig::default())
}

const SCHEDULE_0916: &str = r#"
[[intent]]
security_id = 2885
segment = "NSE_EQ"
frame_secs = 60
bar_open_ist = "09:16:00"
side = "BUY"
lots = 1
kind = "entry"
"#;

fn universe_text() -> String {
    format!(
        "security_id,segment,tick_paise,lot_size,symbol\n{A},NSE_EQ,5,1,AAA\n{B},NSE_EQ,5,1,BBB\n"
    )
}

fn driver(schedule: &str, pass_ns: i64) -> ReplayDriver {
    driver_booted(schedule, pass_ns, BootInstant::scheduled(DAY * NS))
}

fn driver_booted(schedule: &str, pass_ns: i64, boot: BootInstant) -> ReplayDriver {
    let (frames, engine) = parse_schedule(schedule).expect("schedule");
    let (universe, contracts) = parse_universe(&universe_text()).expect("universe");
    let setup = ChainSetup {
        recipe: replay_recipe(&frames, &universe),
        snapshot: None,
        engine_frames_secs: frames,
        universe,
        contracts,
        trading_day: DAY / 86_400,
    };
    ReplayDriver::new(engine, setup, params(), DAY * NS, pass_ns, None, boot).expect("driver")
}

/// `B` trades every 500 ms from 09:15:00 to `until`; `A` only once a minute
/// (09:15:00, 09:16:00, ...). Volumes climb.
fn feed_stream(d: &mut ReplayDriver, until: i64) {
    let mut vol_a = 100u32;
    let mut vol_b = 100u32;
    // A pre-open frame first: the live chain starts at boot, before 09:15
    // (a chain built after 09:15 marks its first candles untrusted).
    d.feed_frame(&frame(
        quote(B, 50.0, sod(9, 14, 55), 1),
        sod(9, 14, 55) * NS,
    ));
    let mut t = sod(9, 15, 0) * NS;
    let mut n = 0i64;
    while t <= until * NS {
        let secs = t / NS;
        vol_b += 10;
        let mut bytes = quote(B, 50.0 + (n % 7) as f32 * 0.05, secs, vol_b);
        if (secs - DAY).rem_euclid(60) == 0 && t % NS == 0 {
            vol_a += 50;
            bytes.extend(quote(A, 100.0 + (n % 5) as f32 * 0.05, secs, vol_a));
        }
        d.feed_frame(&frame(bytes, t + 3_000_000));
        t += NS / 2;
        n += 1;
    }
}

fn run(schedule: &str, until: i64) -> ReplayOutput {
    let mut d = driver(schedule, 1_000_000);
    feed_stream(&mut d, until);
    d.finish().expect("finish")
}

#[test]
fn test_scripted_intents_never_read_price_rule_count_instruments() {
    let (_, mut a) = parse_schedule(SCHEDULE_0916).expect("schedule");
    let mut b = a.clone();
    let key = InstrumentKey {
        security_id: u64::from(A),
        exchange_segment: 1,
    };
    let bar = |close: f64, volume: u64| BarInput {
        key,
        timeframe: 0,
        open_ist_secs: sod(9, 16, 0),
        open: close,
        high: close * 2.0,
        low: close / 2.0,
        close,
        volume,
    };
    let mut sa = IntentSink::with_capacity(8);
    let mut sb = IntentSink::with_capacity(8);
    a.on_bar(&bar(100.0, 1), &mut sa);
    b.on_bar(&bar(f64::MAX / 4.0, 999_999), &mut sb);
    assert_eq!(
        sa.intents(),
        sb.intents(),
        "same bars, different prices, same intents"
    );
    assert_eq!(sa.intents().len(), 1);
    let mut sc = IntentSink::with_capacity(8);
    let mut other = bar(100.0, 1);
    other.open_ist_secs = sod(9, 17, 0);
    a.on_bar(&other, &mut sc);
    assert!(sc.intents().is_empty(), "only the named bar asks");
    assert!(a.value_columns().is_empty());
    assert!(!a.bar_values(key, 0, &mut []));
    assert_eq!(a.rule_count(), 1);
    assert_eq!(a.instruments(), vec![key]);
}

#[test]
fn test_parse_schedule_refuses_bad_time_and_unknown_key_parse_segment() {
    assert!(parse_schedule(&SCHEDULE_0916.replace("09:16:00", "9h16")).is_err());
    assert!(parse_schedule(&format!("{SCHEDULE_0916}colour = \"red\"\n")).is_err());
    assert!(parse_schedule(&SCHEDULE_0916.replace("BUY", "HOLD")).is_err());
    assert!(parse_schedule(&SCHEDULE_0916.replace("entry", "maybe")).is_err());
    assert!(parse_schedule(&SCHEDULE_0916.replace("NSE_EQ", "MARS")).is_err());
    assert!(parse_schedule(&SCHEDULE_0916.replace("lots = 1", "lots = 0")).is_err());
    assert!(
        parse_schedule(&SCHEDULE_0916.replace(
            "bar_open_ist = \"09:16:00\"",
            "bar_open_ist = \"09:16:00\"\nevery_bar = true"
        ))
        .is_err(),
        "both a time and every_bar"
    );
    assert!(parse_schedule("").is_err());
    let (frames, s) =
        parse_schedule(&SCHEDULE_0916.replace("bar_open_ist = \"09:16:00\"", "every_bar = true"))
            .expect("every bar");
    assert_eq!(frames, vec![60]);
    assert_eq!(s.rule_count(), 1);
    assert_eq!(
        parse_segment("NSE_FNO").map(|s| s.as_str()),
        Some("NSE_FNO")
    );
    assert_eq!(parse_segment("nse_eq"), None);
}

/// A `bar_open_ist` off the frame's grid, or outside the session, can never
/// match a bar, so the schedule is refused instead of replaying to a silent
/// "0 orders" day.
#[test]
fn test_parse_schedule_refuses_a_bar_open_no_bar_can_have_check_bar_open() {
    for (t, why) in [
        ("09:20:30", "off the 60 s grid"),
        ("08:00:00", "before the open"),
        ("15:30:00", "ends after the close"),
        ("09:14:00", "opens before 09:15"),
    ] {
        let e = parse_schedule(&SCHEDULE_0916.replace("09:16:00", t))
            .err()
            .unwrap_or_else(|| panic!("{t} must be refused: {why}"));
        assert!(e.contains("bar_open_ist"), "{e}");
    }
    assert!(parse_schedule(&SCHEDULE_0916.replace("09:16:00", "15:29:00")).is_ok());
    assert!(super::scripted::check_bar_open(sod(9, 16, 30) - DAY, 5).is_ok());
    assert!(super::scripted::check_bar_open(sod(9, 16, 31) - DAY, 5).is_err());
}

/// An `every_bar` frame with no bar inside 09:15-15:30 (6 h bars run
/// 06:00-12:00 and 12:00-18:00) can never fire, so it is refused too.
#[test]
fn test_parse_schedule_refuses_an_every_bar_frame_with_no_session_bar_check_every_bar() {
    let every = |frame: u32| {
        SCHEDULE_0916
            .replace("bar_open_ist = \"09:16:00\"", "every_bar = true")
            .replace("frame_secs = 60", &format!("frame_secs = {frame}"))
    };
    for frame in [21_600, 86_400, 30_000] {
        let e = parse_schedule(&every(frame))
            .err()
            .unwrap_or_else(|| panic!("{frame} s bars must be refused"));
        assert!(e.contains("every_bar"), "{e}");
    }
    for frame in [1, 60, 300, 3_600, 900] {
        assert!(parse_schedule(&every(frame)).is_ok(), "{frame} s");
    }
    assert!(
        super::scripted::check_every_bar(22_500).is_err(),
        "06:15-12:30 opens early, 12:30-18:45 ends late"
    );
    assert!(
        super::scripted::check_every_bar(6_300).is_ok(),
        "10:30-12:15 fits"
    );
}

#[test]
fn test_duplicate_universe_key_refused_parse_universe() {
    let dup = format!("{}{A},NSE_EQ,5,1,AGAIN\n", universe_text());
    assert!(parse_universe(&dup).unwrap_err().contains("twice"));
    // Same id, other segment: two instruments (I-P1-11).
    let other = format!("{}{A},NSE_FNO,5,25,AAA-OPT\n", universe_text());
    assert_eq!(parse_universe(&other).expect("ok").0.len(), 3);
    assert!(parse_universe("id,seg\n1,NSE_EQ\n").is_err());
    assert!(parse_universe("security_id,segment,tick_paise,lot_size,symbol\n").is_err());
    assert!(
        parse_universe("security_id,segment,tick_paise,lot_size,symbol\n1,NSE_EQ,0,1,X\n").is_err()
    );
}

#[test]
fn test_recipe_status_is_fixture_never_approved_replay_recipe() {
    let r = replay_recipe(&[60, 300], &[]);
    assert_eq!(r.status, REPLAY_RECIPE_STATUS);
    assert_ne!(r.status, "approved");
    assert_eq!(r.name, REPLAY_RECIPE_NAME);
    assert_eq!(r.timeframes, vec!["60s".to_owned(), "300s".to_owned()]);
}

/// A decision on the 09:16 minute lands on the first grid instant at or after
/// its release (09:17:00 + 1 s), and the order is worked out on the stream.
#[test]
fn test_replay_driver_feed_frame_finish_decides_on_the_grid() {
    let o = run(SCHEDULE_0916, sod(9, 19, 0));
    assert_eq!(o.decisions.len(), 1, "{:?}", o.decisions);
    let d = &o.decisions[0];
    assert_eq!(d.candle_start_ns, sod(9, 16, 0) * NS);
    assert_eq!(d.candle_end_ns, sod(9, 17, 0) * NS);
    assert_eq!(d.release_ns, sod(9, 17, 1) * NS);
    assert_eq!(
        d.decision_ns, d.release_ns,
        "1 ms grid, release on a grid instant"
    );
    assert!(d.taint.is_none());
    assert!(!d.late_price_skip);
    assert!(o.stats.tapped > 0 && o.stats.passes > 0);
    assert_eq!(o.stats.no_receipt_frames, 0);
    let kinds: Vec<LiveOrderEventKind> = o.events.iter().map(|e| e.event).collect();
    assert_eq!(kinds[0], LiveOrderEventKind::Decision);
    assert_eq!(kinds[1], LiveOrderEventKind::Built);
    assert!(
        kinds.contains(&LiveOrderEventKind::Sent),
        "the order was placed (paper rows): {kinds:?}"
    );
    assert!(out::worse_than_worst_rows(&o.events).is_empty());
}

/// D1's flaw, pinned: every folded tick is tapped, not only the universe's,
/// so a scheduled contract that trades once a minute does not read as a feed
/// gap while a contract OUTSIDE the universe keeps the stream busy (on a real
/// day most of the stream is instruments the schedule never names). A filter
/// on the universe before the tap would leave the chain a silent stream and
/// refuse the decision as `feed_gap`.
#[test]
fn test_all_folded_ticks_tapped_so_quiet_fixture_contract_gets_no_feed_gap() {
    /// Not in `universe_text()`.
    const Z: u32 = 7_777;
    let mut d = driver(SCHEDULE_0916, 1_000_000);
    d.feed_frame(&frame(
        quote(Z, 50.0, sod(9, 14, 55), 1),
        sod(9, 14, 55) * NS,
    ));
    let (mut vol_a, mut vol_z) = (100u32, 100u32);
    let mut universe_ticks = 0u64;
    let mut t = sod(9, 15, 0) * NS;
    let mut n = 0i64;
    while t <= sod(9, 19, 0) * NS {
        let secs = t / NS;
        vol_z += 10;
        let mut bytes = quote(Z, 50.0 + (n % 7) as f32 * 0.05, secs, vol_z);
        if (secs - DAY).rem_euclid(60) == 0 && t % NS == 0 {
            vol_a += 50;
            bytes.extend(quote(A, 100.0 + (n % 5) as f32 * 0.05, secs, vol_a));
            universe_ticks += 1;
        }
        d.feed_frame(&frame(bytes, t + 3_000_000));
        t += NS / 2;
        n += 1;
    }
    let o = d.finish().expect("finish");
    assert_eq!(universe_ticks, 5, "A trades 09:15..09:19 on the minute");
    assert!(
        o.stats.tapped > universe_ticks * 10,
        "the out-of-universe ticks are tapped too: {}",
        o.stats.tapped
    );
    assert_eq!(o.decisions.len(), 1, "{:?}", o.decisions);
    assert!(
        o.decisions.iter().all(|d| d.taint.is_none()),
        "no feed gap on the quiet contract: {:?}",
        o.decisions
    );
}

#[test]
fn test_grid_passes_cover_gaps_and_clock_never_moves_back_stats() {
    let mut d = driver(SCHEDULE_0916, 1_000_000);
    let t0 = sod(9, 15, 0) * NS;
    d.feed_frame(&frame(quote(B, 50.0, sod(9, 15, 0), 10), t0));
    let p0 = d.stats().passes;
    d.feed_frame(&frame(quote(B, 50.05, sod(9, 15, 0), 20), t0 + 10_000_000));
    assert_eq!(
        d.stats().passes - p0,
        10,
        "one pass per ms of the 10 ms gap"
    );
    let p1 = d.stats().passes;
    d.feed_frame(&frame(quote(B, 50.1, sod(9, 15, 0), 30), t0 + 5_000_000));
    assert_eq!(d.stats().passes, p1, "an older receipt moves no clock");
    assert_eq!(d.stats().frames, 3);
    assert_eq!(d.stats().first_receipt_ist_ns, Some(t0));
    assert_eq!(d.stats().last_receipt_ist_ns, Some(t0 + 10_000_000));
}

#[test]
fn test_damaged_segment_bumps_drop_seq_and_taints_note_segment_damage() {
    let mut d = driver(SCHEDULE_0916, 1_000_000);
    d.note_segment();
    feed_stream(&mut d, sod(9, 16, 30));
    d.note_segment_damage();
    d.note_frames_skipped();
    // The rest of the stream, after the damage.
    let mut vol = 10_000u32;
    let mut t = sod(9, 16, 30) * NS + NS / 2;
    while t <= sod(9, 19, 0) * NS {
        vol += 10;
        d.feed_frame(&frame(quote(B, 50.0, t / NS, vol), t));
        t += NS / 2;
    }
    let o = d.finish().expect("finish");
    assert_eq!(o.stats.drop_seq, 1);
    assert_eq!(o.stats.damaged_segments, 1);
    assert_eq!(o.stats.segments, 1);
    assert_eq!(o.decisions.len(), 1);
    assert_eq!(
        o.decisions[0].taint.as_deref(),
        Some("tick_dropped"),
        "the 09:16 candle overlaps the damage: refused"
    );
}

#[test]
fn test_v2_record_without_receipt_refused() {
    let mut d = driver(SCHEDULE_0916, 1_000_000);
    let mut f = frame(quote(B, 50.0, sod(9, 15, 0), 10), sod(9, 15, 0) * NS);
    f.received_at_nanos = tickvault_storage::ws_frame_spill::WAL_RECEIPT_UNKNOWN_NANOS;
    d.feed_frame(&f);
    let other_day = frame(
        quote(B, 50.0, sod(9, 15, 0) - 86_400, 10),
        (sod(9, 15, 0) - 86_400) * NS,
    );
    d.feed_frame(&other_day);
    let s = d.stats();
    assert_eq!(s.no_receipt_frames, 1);
    assert_eq!(s.other_day_frames, 1);
    assert_eq!(s.tapped, 0);
    assert_eq!(s.frames, 0);
}

#[test]
fn test_walk_frame_stacks_packets_and_abandons_a_partial_tail() {
    let mut bytes = quote(A, 100.0, sod(9, 15, 0), 10);
    bytes.extend(quote(B, 50.0, sod(9, 15, 0), 10));
    let mut ticks = Vec::new();
    let w = walk_frame(&bytes, 1, &mut ticks);
    assert_eq!((w.packets, w.abandoned_bytes, ticks.len()), (2, 0, 2));
    assert_eq!((ticks[1].start, ticks[1].end), (50, 100));
    bytes.extend_from_slice(&[4, 50, 0, 1]);
    ticks.clear();
    let w = walk_frame(&bytes, 1, &mut ticks);
    assert_eq!((w.packets, w.abandoned_bytes, ticks.len()), (2, 4, 2));
    let mut d = driver(SCHEDULE_0916, 1_000_000);
    d.feed_frame(&frame(bytes, sod(9, 15, 0) * NS));
    assert_eq!(d.stats().abandoned_bytes, 4);
    let o = d.finish().expect("finish");
    assert_eq!(
        o.stats.drop_seq, 1,
        "an abandoned tail moves the loss count"
    );
}

#[test]
fn test_list_wal_inputs_orders_by_name_sidecar_segment_nanos_input_segment_name_read_main_feed_segment()
 {
    let dir = std::env::temp_dir().join(format!("tv-lf8-list-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    for n in [
        "ws-frames-30.wal",
        "ws-frames-4.wal.gz",
        "notes.txt",
        "ws-frames-x.wal",
        "ws-frames-30.wal.sha-ab.gz",
    ] {
        std::fs::write(dir.join(n), b"").expect("write");
    }
    let folder = list_wal_inputs(&dir).expect("list");
    let got: Vec<String> = folder
        .segments
        .iter()
        .map(|(_, p)| {
            p.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_owned()
        })
        .collect();
    assert_eq!(got, vec!["ws-frames-4.wal.gz", "ws-frames-30.wal"]);
    assert_eq!(
        folder.sidecars,
        vec![("ws-frames-30.wal.sha-ab.gz".to_owned(), 30)],
        "a sidecar is listed with the segment it copies, never refused here"
    );
    assert_eq!(
        super::source::sidecar_segment_nanos("ws-frames-7.wal.sha-ff.gz"),
        Some(7)
    );
    assert_eq!(
        super::source::sidecar_segment_nanos("ws-frames-7.wal.gz"),
        None
    );
    // An unzipped sidecar is still a sidecar (LF-8 review round 5).
    assert_eq!(
        super::source::sidecar_segment_nanos("ws-frames-7.wal.sha-ff"),
        Some(7)
    );
    assert!(super::source::is_local_sidecar(
        "dir/ws-frames-7.wal.sha-ff"
    ));
    assert!(super::source::is_local_sidecar("ws-frames-7.wal.sha-ff.gz"));
    assert!(!super::source::is_local_sidecar("ws-frames-7.wal.gz"));
    assert!(!super::source::is_local_sidecar("ws-frames-7.wal"));
    assert_eq!(
        input_segment_name(std::path::Path::new("ws-frames-4.wal.gz")),
        Some(("ws-frames-4.wal".to_owned(), 4))
    );
    assert!(super::source::read_main_feed_segment(&dir.join("ws-frames-4.wal.gz"), None).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

/// UTC name nanos of a segment named at `ist_secs` (IST epoch seconds).
fn name_nanos(ist_secs: i64) -> u128 {
    ((ist_secs - 19_800) * NS) as u128
}

fn folder_of(names: &[&str]) -> super::source::WalFolder {
    let mut f = super::source::WalFolder::default();
    for n in names {
        if let Some(s) = super::source::sidecar_segment_nanos(n) {
            f.sidecars.push(((*n).to_owned(), s));
        } else if let Some((_, s)) = input_segment_name(std::path::Path::new(n)) {
            f.segments.push((s, std::path::PathBuf::from(n)));
        }
    }
    f.segments.sort();
    f.sidecars.sort();
    f
}

/// A segment held twice (plain and gzipped) and a `.sha-` sidecar copy each
/// refuse the day when that segment is READ, so no segment is fed twice and
/// no second copy is picked silently (the bucket path's rule).
#[test]
fn test_select_day_inputs_refuses_a_twice_held_segment_and_a_sidecar_it_reads() {
    let day = chrono::NaiveDate::from_ymd_opt(2026, 11, 18).expect("date");
    let a = format!("ws-frames-{}.wal", name_nanos(sod(9, 0, 0)));
    let b = format!("ws-frames-{}.wal", name_nanos(sod(12, 0, 0)));
    let twice = folder_of(&[&a, &format!("{a}.gz"), &b]);
    assert!(matches!(
        super::source::select_day_inputs(twice, day),
        Err(super::source::InputsError::Duplicate(n)) if n == a
    ));
    let side = format!("{b}.sha-abcd.gz");
    let with_sidecar = folder_of(&[&a, &b, &side]);
    assert!(matches!(
        super::source::select_day_inputs(with_sidecar, day),
        Err(super::source::InputsError::Sidecars(v)) if v == vec![side.clone()]
    ));
    let clean = super::source::select_day_inputs(folder_of(&[&a, &b]), day).expect("clean");
    assert_eq!(clean.day_segments, 2);
}

/// Finding: a duplicate or sidecar of a segment the day does NOT read (an
/// older day in a flattened folder) never blocks the day, as the bucket
/// never lists it.
#[test]
fn test_select_day_inputs_ignores_duplicates_and_sidecars_of_unread_segments() {
    let day = chrono::NaiveDate::from_ymd_opt(2026, 11, 18).expect("date");
    let old = format!("ws-frames-{}.wal", name_nanos(sod(10, 0, 0) - 5 * 86_400));
    let prev_old = format!("ws-frames-{}.wal", name_nanos(sod(9, 0, 0) - 86_400));
    let prev_new = format!("ws-frames-{}.wal", name_nanos(sod(16, 0, 0) - 86_400));
    let today = format!("ws-frames-{}.wal", name_nanos(sod(8, 59, 0)));
    let got = super::source::select_day_inputs(
        folder_of(&[
            &old,
            &format!("{old}.gz"),
            &format!("{old}.sha-abcd.gz"),
            &prev_old,
            &format!("{prev_old}.sha-11.gz"),
            &prev_new,
            &today,
        ]),
        day,
    )
    .expect("an unread segment's copies are ignored");
    assert_eq!(
        got.paths,
        vec![
            std::path::PathBuf::from(&prev_new),
            std::path::PathBuf::from(&today)
        ]
    );
    assert_eq!((got.day_segments, got.left_out), (1, 3));
    // A sidecar of the previous date's segment that IS read still refuses.
    assert!(matches!(
        super::source::select_day_inputs(
            folder_of(&[&prev_new, &format!("{prev_new}.sha-ab.gz"), &today]),
            day,
        ),
        Err(super::source::InputsError::Sidecars(_))
    ));
}

/// A local folder is read like the bucket: the day's segments by name date
/// (IST), plus the newest one named on the PREVIOUS date only; segments of
/// other days are never read, and a day with only an earlier segment has no
/// segment of its own (`day_segments` 0).
#[test]
fn test_select_day_inputs_takes_the_day_and_the_newest_of_the_previous_date_name_ist_date() {
    let day = chrono::NaiveDate::from_ymd_opt(2026, 11, 18).expect("date");
    let n = |ist: i64| format!("ws-frames-{}.wal", name_nanos(ist));
    let two_days_before = n(sod(10, 0, 0) - 2 * 86_400);
    let day_before_a = n(sod(9, 0, 0) - 86_400);
    let day_before_b = n(sod(23, 50, 0) - 86_400);
    let day_a = n(sod(0, 5, 0));
    let day_b = n(sod(12, 0, 0));
    let after = n(sod(1, 0, 0) + 86_400);
    assert_eq!(
        super::source::name_ist_date(name_nanos(sod(23, 59, 59))),
        Some(day)
    );
    let got = super::source::select_day_inputs(
        folder_of(&[
            &two_days_before,
            &day_before_a,
            &day_before_b,
            &day_a,
            &day_b,
            &after,
        ]),
        day,
    )
    .expect("select");
    let p = |s: &str| std::path::PathBuf::from(s);
    assert_eq!(got.paths, vec![p(&day_before_b), p(&day_a), p(&day_b)]);
    assert_eq!((got.left_out, got.day_segments), (3, 2));
    // After a weekend (Monday 2026-11-16): Friday's segment is never read.
    let monday = chrono::NaiveDate::from_ymd_opt(2026, 11, 16).expect("date");
    let friday = n(sod(16, 20, 0) - 5 * 86_400);
    let mon = n(sod(8, 59, 0) - 2 * 86_400);
    let got =
        super::source::select_day_inputs(folder_of(&[&friday, &mon]), monday).expect("select");
    assert_eq!(got.paths, vec![p(&mon)], "Friday is two days before");
    // Only an earlier segment: nothing of the day.
    let got = super::source::select_day_inputs(folder_of(&[&day_before_b]), day).expect("ok");
    assert_eq!((got.paths.len(), got.day_segments), (1, 0));
}

/// The local folder and the bucket choose the SAME segments and the same
/// refusing sidecars for the same names (`select_day_inputs` against
/// `raw_frame_fetch::plan_day_listing`), weekends and old sidecars included.
#[test]
fn test_select_day_inputs_matches_the_bucket_listing_plan_day_listing() {
    let n = |ist: i64| format!("ws-frames-{}.wal", name_nanos(ist));
    let cases: Vec<(chrono::NaiveDate, Vec<String>)> = vec![
        (
            chrono::NaiveDate::from_ymd_opt(2026, 11, 18).expect("date"),
            vec![
                n(sod(10, 0, 0) - 2 * 86_400),
                n(sod(9, 0, 0) - 86_400),
                n(sod(23, 50, 0) - 86_400),
                n(sod(0, 5, 0)),
                n(sod(12, 0, 0)),
                n(sod(1, 0, 0) + 86_400),
            ],
        ),
        (
            chrono::NaiveDate::from_ymd_opt(2026, 11, 16).expect("date"),
            vec![
                n(sod(16, 20, 0) - 5 * 86_400),
                n(sod(8, 59, 0) - 2 * 86_400),
            ],
        ),
        (
            chrono::NaiveDate::from_ymd_opt(2026, 11, 18).expect("date"),
            vec![
                n(sod(9, 0, 0) - 86_400),
                format!("{}.sha-11.gz", n(sod(9, 0, 0) - 86_400)),
                n(sod(16, 0, 0) - 86_400),
                n(sod(9, 0, 0)),
                format!("{}.sha-22.gz", n(sod(10, 0, 0) - 4 * 86_400)),
            ],
        ),
        (
            chrono::NaiveDate::from_ymd_opt(2026, 11, 18).expect("date"),
            vec![
                n(sod(16, 0, 0) - 86_400),
                format!("{}.sha-33.gz", n(sod(16, 0, 0) - 86_400)),
                n(sod(9, 0, 0)),
            ],
        ),
    ];
    for (day, names) in cases {
        // The bucket: each name filed as `raw-frames/<its IST date>/<name>.gz`
        // (a sidecar already ends in `.gz`).
        let key_of = |name: &str| -> (String, chrono::NaiveDate) {
            let seg_nanos = super::source::sidecar_segment_nanos(name)
                .or_else(|| input_segment_name(std::path::Path::new(name)).map(|(_, x)| x))
                .expect("a segment or sidecar");
            let date = super::source::name_ist_date(seg_nanos).expect("date");
            let file = if name.ends_with(".gz") {
                name.to_owned()
            } else {
                format!("{name}.gz")
            };
            (format!("raw-frames/{date}/{file}"), date)
        };
        let prev_date = day.pred_opt().expect("prev");
        let mut day_keys = Vec::new();
        let mut prev_keys = Vec::new();
        for name in &names {
            let (k, d) = key_of(name);
            if d == day {
                day_keys.push((k, 1u64));
            } else if d == prev_date {
                prev_keys.push((k, 1u64));
            }
        }
        let bucket = tickvault_storage::raw_frame_fetch::plan_day_listing(&day_keys, &prev_keys);
        let local = super::source::select_day_inputs(
            folder_of(&names.iter().map(String::as_str).collect::<Vec<_>>()),
            day,
        );
        let tail = |k: &str| k.rsplit('/').next().unwrap_or("").to_owned();
        match local {
            Err(super::source::InputsError::Sidecars(v)) => {
                let want: Vec<String> = bucket.sidecars.iter().map(|k| tail(k)).collect();
                assert_eq!(v, want, "{day}: the same sidecars refuse");
            }
            Ok(got) => {
                assert!(bucket.sidecars.is_empty(), "{day}: {:?}", bucket.sidecars);
                let local_names: Vec<String> = got
                    .paths
                    .iter()
                    .map(|p| format!("{}.gz", p.display()))
                    .collect();
                let bucket_names: Vec<String> = bucket.keys.iter().map(|(k, _)| tail(k)).collect();
                assert_eq!(local_names, bucket_names, "{day}: the same segments");
                assert_eq!(got.day_segments, bucket.day_segments, "{day}");
            }
            Err(e) => panic!("{day}: {e}"),
        }
    }
}

#[test]
fn test_grid_end_ns_and_tail_ns_key_label() {
    let p = params();
    assert_eq!(
        grid_end_ns(DAY * NS, &p),
        sod(15, 30, 0) * NS + NS + 60 * NS + NS
    );
    assert_eq!(tail_ns(&p, &[60, 300]), (300 + 60 + 1) * NS + NS);
    assert_eq!(
        key_label(&InstrumentKey {
            security_id: 7,
            exchange_segment: 2
        }),
        "7/NSE_FNO"
    );
}

#[test]
fn test_parse_args_and_chain_params() {
    let a = |v: &[&str]| -> Vec<String> {
        std::iter::once("tv-live-fill-replay")
            .chain(v.iter().copied())
            .map(str::to_owned)
            .collect()
    };
    let ok = parse_args(&a(&[
        "--day",
        DAY_TEXT,
        "--wal-dir",
        "w",
        "--schedule",
        "s",
        "--universe",
        "u",
        "--out",
        "o",
        "--open-wait-ms",
        "2500",
        "--pass-us",
        "500",
    ]))
    .expect("ok");
    assert_eq!(ok.pass_us, 500);
    let p = chain_params(&LiveFillConfig::default(), &ok).expect("params");
    assert_eq!(p.open_wait_ms, 2_500);
    let mut too = ok.clone();
    too.wait_ms = Some(5);
    assert!(
        chain_params(&LiveFillConfig::default(), &too).is_err(),
        "bounds-checked"
    );
    assert!(parse_args(&a(&["--nope"])).is_err());
    let with = |extra: &[&str]| {
        let mut v = vec![
            "--day",
            DAY_TEXT,
            "--wal-dir",
            "w",
            "--schedule",
            "s",
            "--universe",
            "u",
            "--out",
            "o",
        ];
        v.extend_from_slice(extra);
        parse_args(&a(&v))
    };
    assert_eq!(
        with(&["--boot-ist", "09:14:40"]).expect("boot").boot_sod,
        Some(9 * 3_600 + 14 * 60 + 40)
    );
    assert_eq!(with(&[]).expect("no boot").boot_sod, None);
    for bad in ["9:14", "25:00:00", "", "09:14:40x"] {
        assert!(with(&["--boot-ist", bad]).is_err(), "{bad:?}");
    }
    // An empty path is the current folder to `create_dir_all` and `join`.
    for flag in [
        "--out",
        "--staging",
        "--wal-dir",
        "--schedule",
        "--universe",
        "--config",
        "--compare",
        "--contracts-artifact",
        "--backup-history",
    ] {
        let e = with(&[flag, ""]).expect_err(flag);
        assert!(e.contains(flag) && e.contains("empty"), "{flag}: {e}");
    }
    assert!(parse_args(&a(&["--day", DAY_TEXT])).is_err(), "no input");
    assert!(
        parse_args(&a(&[
            "--s3-date",
            DAY_TEXT,
            "--schedule",
            "s",
            "--universe",
            "u",
            "--out",
            "o"
        ]))
        .is_err(),
        "--s3-date needs --staging"
    );
    assert!(
        parse_args(&a(&[
            "--day",
            DAY_TEXT,
            "--wal-dir",
            "w",
            "--s3-date",
            DAY_TEXT,
            "--staging",
            "x",
            "--schedule",
            "s",
            "--universe",
            "u",
            "--out",
            "o"
        ]))
        .is_err(),
        "two inputs"
    );
    assert!(
        parse_args(&a(&[
            "--day",
            DAY_TEXT,
            "--wal-dir",
            "w",
            "--schedule",
            "s",
            "--out",
            "o"
        ]))
        .is_err(),
        "no universe"
    );
    assert!(
        parse_args(&a(&[
            "--day",
            DAY_TEXT,
            "--wal-dir",
            "w",
            "--schedule",
            "s",
            "--universe",
            "u",
            "--out",
            "o",
            "--pass-us",
            "0"
        ]))
        .is_err()
    );
}

/// Refusals of `run_cli` that fire with an empty folder: a bad day, a
/// folder with no segment, a bad flag, an empty path. The refusals a good
/// folder must still meet (a non-trading day, a non-empty `--out`, a missing
/// config) are in
/// `test_run_cli_refusals_fire_on_a_folder_that_would_replay`, where the
/// folder alone would exit 0.
#[test]
fn test_run_cli_refuses_bad_input_with_exit_two() {
    let base = std::env::temp_dir().join(format!("tv-lf8-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("dir");
    let sched = base.join("s.toml");
    std::fs::write(&sched, SCHEDULE_0916).expect("write");
    let uni = base.join("u.csv");
    std::fs::write(&uni, universe_text()).expect("write");
    let wal = base.join("wal");
    std::fs::create_dir_all(&wal).expect("dir");
    let args = |extra: &[&str], out: &str| -> Vec<String> {
        let mut v: Vec<String> = [
            "tv-live-fill-replay",
            "--wal-dir",
            wal.to_str().expect("utf8"),
            "--schedule",
            sched.to_str().expect("utf8"),
            "--universe",
            uni.to_str().expect("utf8"),
            "--out",
            base.join(out).to_str().expect("utf8"),
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        v.extend(extra.iter().map(|s| (*s).to_owned()));
        v
    };
    assert_eq!(
        run_cli(&args(&["--day", "2026-11-18x"], "o2")),
        EXIT_BAD_INPUT
    );
    assert_eq!(
        run_cli(&args(&["--day", DAY_TEXT], "o3")),
        EXIT_BAD_INPUT,
        "no segments in the folder"
    );
    assert_eq!(
        run_cli(&["x".to_owned(), "--bad".to_owned()]),
        EXIT_BAD_INPUT
    );
    // `--out ""` would write into the current folder: refused, and nothing
    // is written there.
    let mut empty_out = args(&["--day", DAY_TEXT], "o6");
    let at = empty_out.iter().position(|s| s == "--out").expect("--out") + 1;
    empty_out[at] = String::new();
    let cwd_events = std::path::Path::new("events.csv").exists();
    assert_eq!(run_cli(&empty_out), EXIT_BAD_INPUT, "an empty --out");
    assert_eq!(std::path::Path::new("events.csv").exists(), cwd_events);
    // The refusal is the flag's own, before any input is read: the folder
    // above has no segment, which alone would also exit 2.
    let names_empty = |v: &[String], flag: &str| {
        parse_args(v).is_err_and(|e| e.contains(flag) && e.contains("empty"))
    };
    assert!(
        names_empty(&empty_out, "--out"),
        "{:?}",
        parse_args(&empty_out)
    );
    for flag in ["--wal-dir", "--schedule", "--universe"] {
        let mut v = args(&["--day", DAY_TEXT], "o7");
        let at = v.iter().position(|s| s == flag).expect("flag") + 1;
        v[at] = String::new();
        assert!(names_empty(&v, flag), "{flag}: {:?}", parse_args(&v));
    }
    let staged = args(&["--day", DAY_TEXT, "--staging", ""], "o8");
    assert!(
        names_empty(&staged, "--staging"),
        "{:?}",
        parse_args(&staged)
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn test_lot_size_mismatches_checks_derivatives_only() {
    let (_, mut specs) = parse_universe(&universe_text()).expect("universe");
    specs.insert(
        InstrumentKey {
            security_id: 40,
            exchange_segment: 2,
        },
        ContractSpec {
            tick_paise: 5,
            lot_size: 25,
            symbol: "OPT".to_owned(),
        },
    );
    let artifact = r#"[{"i":40,"x":"NSE","c":"OPTSTK","e":20261126,"s":100,"l":"CE","u":"X","z":50},
                       {"i":2885,"x":"NSE","c":"FUTSTK","e":20261126,"s":0,"l":"","u":"X","z":9}]"#;
    let m = lot_size_mismatches(artifact, &specs, 20261118).expect("parse");
    assert_eq!(m.len(), 1, "{m:?}");
    assert!(m[0].contains("40/NSE_FNO"));
    assert!(lot_size_mismatches("not json", &specs, 20261118).is_err());
    // Not checked is not a pass: a derivative the artifact does not hold,
    // and one whose only row has no lot, are both reported.
    for key in [41u64, 42] {
        specs.insert(
            InstrumentKey {
                security_id: key,
                exchange_segment: 2,
            },
            ContractSpec {
                tick_paise: 5,
                lot_size: 75,
                symbol: format!("OPT{key}"),
            },
        );
    }
    let artifact = r#"[{"i":40,"x":"NSE","c":"OPTSTK","e":20261126,"s":100,"l":"CE","u":"X","z":25},
                       {"i":42,"x":"NSE","c":"OPTSTK","e":20261126,"s":100,"l":"PE","u":"X","z":0}]"#;
    let m = lot_size_mismatches(artifact, &specs, 20261118).expect("parse");
    assert_eq!(m.len(), 2, "{m:?}");
    assert!(
        m[0].contains("41/NSE_FNO") && m[0].contains("not in the artifact"),
        "{m:?}"
    );
    assert!(
        m[1].contains("42/NSE_FNO") && m[1].contains("not in the artifact"),
        "{m:?}"
    );
    assert!(
        lot_size_mismatches("[]", &specs, 20261118)
            .expect("parse")
            .iter()
            .all(|l| l.contains("not in the artifact")),
        "an empty artifact checks nothing and passes nothing"
    );
}

/// One security id on two contracts in the day's file (an expired twin
/// before the current one, `expiry_scope_check`'s `ambiguous` case): the
/// check reads only the rows expiring on or after the day, so the stale lot
/// neither refuses the right universe nor passes the wrong one; two live rows
/// with different lots are a problem, never a first-wins pick (LF-8 review
/// round 5).
#[test]
fn test_lot_size_mismatches_reads_rows_on_or_after_the_day_only() {
    let key = InstrumentKey {
        security_id: 49_081,
        exchange_segment: 2,
    };
    let with_lot = |lot: u32| {
        let mut m = HashMap::new();
        m.insert(
            key,
            ContractSpec {
                tick_paise: 5,
                lot_size: lot,
                symbol: "OPT".to_owned(),
            },
        );
        m
    };
    let twin = r#"[{"i":49081,"x":"NSE","c":"OPTSTK","e":20261028,"s":100,"l":"CE","u":"X","z":25},
                   {"i":49081,"x":"NSE","c":"OPTSTK","e":20261126,"s":100,"l":"CE","u":"Y","z":75}]"#;
    assert!(
        lot_size_mismatches(twin, &with_lot(75), 20261118)
            .expect("parse")
            .is_empty(),
        "the current contract's lot passes"
    );
    let stale = lot_size_mismatches(twin, &with_lot(25), 20261118).expect("parse");
    assert_eq!(stale.len(), 1, "{stale:?}");
    assert!(stale[0].contains("artifact says 75"), "{stale:?}");
    // A row expiring ON the day is the day's.
    assert!(
        lot_size_mismatches(twin, &with_lot(75), 20261126)
            .expect("parse")
            .is_empty()
    );
    // Only an expired row: not held.
    let gone = lot_size_mismatches(twin, &with_lot(75), 20261127).expect("parse");
    assert!(gone[0].contains("not in the artifact"), "{gone:?}");
    let both = r#"[{"i":49081,"x":"NSE","c":"OPTSTK","e":20261126,"s":100,"l":"CE","u":"X","z":25},
                   {"i":49081,"x":"NSE","c":"OPTSTK","e":20261126,"s":100,"l":"CE","u":"Y","z":75}]"#;
    for lot in [25, 75] {
        let m = lot_size_mismatches(both, &with_lot(lot), 20261118).expect("parse");
        assert_eq!(m.len(), 1, "{m:?}");
        assert!(
            m[0].contains("different lot sizes (25, 75)"),
            "never a silent pick: {m:?}"
        );
    }
}

#[test]
fn test_decision_timings_decisions_csv_and_ms_text_ist_text() {
    let o = run(SCHEDULE_0916, sod(9, 19, 0));
    let again = decision_timings(&o.events, &params(), &[60]);
    assert_eq!(again, o.decisions);
    let csv = out::decisions_csv(&o.decisions);
    assert!(csv.starts_with("signal_id,security_id,segment,symbol,frame_secs,"));
    assert!(csv.contains(",1000.000,1000.000,false,false,"), "{csv}");
    assert_eq!(out::ms_text(-250_000), "-0.250");
    assert_eq!(out::ms_text(1_234_567_000), "1234.567");
    assert_eq!(
        out::ist_text(sod(9, 15, 0) * NS + 5),
        "2026-11-18T09:15:00.000000005"
    );
}

#[test]
fn test_events_csv_event_row_of_rechecks_day_report_report_files_summary_text_compare_sql_worse_than_worst_rows()
 {
    let o = run(SCHEDULE_0916, sod(9, 19, 0));
    let csv = out::events_csv(&o.events);
    let text = String::from_utf8(csv).expect("utf8");
    assert_eq!(text.lines().count(), o.events.len() + 1);
    let rows: Vec<_> = o.events.iter().map(out::event_row_of).collect();
    assert_eq!(rows[0].event, "decision");
    assert_eq!(rows[0].ts_ns % 1_000, 0, "microseconds, as stored");
    let checks = out::rechecks(&rows, &o.seconds);
    assert_eq!(checks.len(), 1, "one priced order re-priced");
    let report = out::day_report(DAY_TEXT, &o, &[]);
    assert_eq!(report.counts.orders, 1);
    let files = out::report_files(&report);
    assert_eq!(files.len(), 5);
    assert!(files[0].0.starts_with("report-2026-11-18"));
    let summary = out::summary_text(
        &o,
        &report,
        &out::SummaryInputs {
            day: DAY_TEXT.to_owned(),
            weekday: "Wed".to_owned(),
            incomplete_reasons: vec!["test".to_owned()],
            largest_segment_bytes: 1,
            file_hashes: vec![("a".to_owned(), "00".to_owned())],
        },
    );
    assert!(summary.starts_with("Replay of Wed 2026-11-18: 1 pretend orders"));
    assert!(summary.contains("Nothing real was sent"));
    assert!(summary.contains("INCOMPLETE (test)"));
    for l in out::HONEST_LIMITS {
        assert!(summary.contains(l));
        assert!(
            !l.contains("no-loss"),
            "the replay is not a no-loss view: {l}"
        );
    }
    assert!(
        out::HONEST_LIMITS
            .iter()
            .any(|l| l.contains("socket outages are invisible")),
        "a main-feed socket outage is named as a limit"
    );
    assert!(summary.contains("losses wholly before the day's chain (not handed to it): 0"));
    assert!(summary.contains(&out::compare_sql(DAY_TEXT)));
    assert!(
        out::worse_than_worst_rows(&o.events).is_empty(),
        "paper fills are the model"
    );
}

#[test]
fn test_compare_csv_matches_on_instrument_and_candle_end() {
    let o = run(SCHEDULE_0916, sod(9, 19, 0));
    let live = format!(
        "event,security_id,segment,candle_start,candle_end,decision_ts\n\
         decision,{A},NSE_EQ,2026-11-18T09:16:00.000000Z,2026-11-18T09:17:00.000000Z,2026-11-18T09:17:01.004000Z\n\
         built,{A},NSE_EQ,2026-11-18T09:16:00.000000Z,2026-11-18T09:17:00.000000Z,2026-11-18T09:17:01.004000Z\n\
         decision,{B},NSE_EQ,2026-11-18T09:17:00.000000Z,2026-11-18T09:18:00.000000Z,2026-11-18T09:18:01.000000Z\n"
    );
    let diff = out::compare_csv(&live, &o.decisions).expect("diff");
    let lines: Vec<&str> = diff.lines().collect();
    assert_eq!(lines.len(), 3, "{diff}");
    assert!(
        lines[1].starts_with("1333,") && lines[1].ends_with(","),
        "only live decided: {diff}"
    );
    assert!(
        lines[2].starts_with("2885,") && lines[2].ends_with(",-4.000"),
        "replay 4 ms earlier: {diff}"
    );
    assert!(out::compare_csv("nope\n", &o.decisions).is_err());
    let no_start = "event,security_id,segment,candle_end,decision_ts\n";
    assert!(
        out::compare_csv(no_start, &o.decisions).is_err(),
        "the candle start is part of the match"
    );
}

/// Bars of different sizes ending at the same instant, and two rules on one
/// bar, each keep their own row: a 900 ms live delay on the 5 m bar is not
/// folded into the 1 m bar's earlier decision (`out::compare_csv`).
#[test]
fn test_compare_csv_keeps_frames_and_rules_ending_together_apart() {
    let replay = |start: i64, decision: i64| DecisionTiming {
        signal_id: 1,
        security_id: i64::from(A),
        segment: "NSE_EQ",
        symbol: "AAA".to_owned(),
        frame_secs: sod(9, 20, 0) - start,
        candle_start_ns: start * NS,
        candle_end_ns: sod(9, 20, 0) * NS,
        close_by: "",
        last_tick_exchange_ns: None,
        last_tick_receipt_ns: None,
        release_ns: decision,
        decision_ns: decision,
        opening_clamp: false,
        late_price_skip: false,
        taint: None,
    };
    let at = sod(9, 20, 1) * NS;
    let decisions = vec![
        replay(sod(9, 19, 0), at),
        replay(sod(9, 15, 0), at),
        replay(sod(9, 15, 0), at + 1_000_000),
    ];
    let live = format!(
        "event,security_id,segment,candle_start,candle_end,decision_ts\n\
         decision,{A},NSE_EQ,2026-11-18T09:19:00Z,2026-11-18T09:20:00Z,2026-11-18T09:20:01.002Z\n\
         decision,{A},NSE_EQ,2026-11-18T09:15:00Z,2026-11-18T09:20:00Z,2026-11-18T09:20:01.900Z\n\
         decision,{A},NSE_EQ,2026-11-18T09:15:00Z,2026-11-18T09:20:00Z,2026-11-18T09:20:01.950Z\n"
    );
    let diff = out::compare_csv(&live, &decisions).expect("diff");
    let lines: Vec<&str> = diff.lines().collect();
    assert_eq!(lines.len(), 4, "one row per decision: {diff}");
    let deltas: Vec<&str> = lines[1..]
        .iter()
        .map(|l| l.rsplit(',').next().unwrap_or(""))
        .collect();
    assert!(deltas.contains(&"-2.000"), "the 1 m bar: {diff}");
    assert!(
        deltas.contains(&"-900.000"),
        "the 5 m bar's first rule: {diff}"
    );
    assert!(
        deltas.contains(&"-949.000"),
        "the 5 m bar's second rule: {diff}"
    );
}

fn output_fingerprint(o: &ReplayOutput) -> (Vec<u8>, Vec<DecisionTiming>) {
    (out::events_csv(&o.events), o.decisions.clone())
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(12))]

    /// Random streams: every decision lands at or after its release (which is
    /// at least the wait after the candle's end) and before the next grid
    /// instant after it.
    #[test]
    fn proptest_decision_delay_at_least_wait(
        steps in proptest::collection::vec((1i64..1_400, 0u32..3), 40..400),
        pass_us in proptest::sample::select(vec![250i64, 1_000, 3_000]),
    ) {
        let schedule = SCHEDULE_0916.replace("bar_open_ist = \"09:16:00\"", "every_bar = true");
        let mut d = driver(&schedule, pass_us * 1_000);
        let mut t = sod(9, 15, 0) * NS;
        let mut vol = 100u32;
        for (dt_ms, which) in steps {
            t += dt_ms * 1_000_000;
            vol += 7;
            let sid = if which == 0 { A } else { B };
            d.feed_frame(&frame(quote(sid, 100.0 + (vol % 9) as f32 * 0.05, t / NS, vol), t));
        }
        let o = d.finish().expect("finish");
        let wait = i64::try_from(params().wait_ms).expect("fits") * 1_000_000;
        for dt in &o.decisions {
            proptest::prop_assert!(dt.release_ns - dt.candle_end_ns >= wait);
            proptest::prop_assert!(dt.decision_ns >= dt.release_ns, "{dt:?}");
            proptest::prop_assert!(dt.decision_ns < dt.release_ns + pass_us * 1_000, "{dt:?}");
        }
    }

    /// Where the day is cut into REAL segments (written by the live WAL
    /// writer, read back by the reader, fed through `feed_segment`) does not
    /// change a byte of output.
    #[test]
    fn proptest_segment_split_points_do_not_change_output(
        steps in proptest::collection::vec(1i64..900, 60..300),
        cuts in proptest::collection::vec(1usize..300, 0..6),
    ) {
        let schedule = SCHEDULE_0916.replace("bar_open_ist = \"09:16:00\"", "every_bar = true");
        let mut frames = Vec::new();
        let mut t = sod(9, 15, 0) * NS;
        let mut vol = 100u32;
        for (i, dt_ms) in steps.iter().enumerate() {
            t += dt_ms * 1_000_000;
            vol += 3;
            let sid = if i % 3 == 0 { A } else { B };
            frames.push(frame(quote(sid, 100.0 + (vol % 11) as f32 * 0.05, t / NS, vol), t));
        }
        let mut bounds: Vec<usize> = cuts.iter().copied().filter(|c| *c < frames.len()).collect();
        bounds.push(0);
        bounds.push(frames.len());
        bounds.sort_unstable();
        bounds.dedup();
        let parts: Vec<Vec<ReplayedFrame>> = bounds
            .windows(2)
            .map(|w| frames[w[0]..w[1]].to_vec())
            .collect();
        let dir_whole = tmp_dir("prop-whole");
        let dir_split = tmp_dir("prop-split");
        let whole = replay_segments(&schedule, &write_segments(&dir_whole, &[frames.clone()]));
        let split_paths = write_segments(&dir_split, &parts);
        proptest::prop_assert_eq!(split_paths.len(), parts.len());
        let split = replay_segments(&schedule, &split_paths);
        let _ = std::fs::remove_dir_all(&dir_whole);
        let _ = std::fs::remove_dir_all(&dir_split);
        proptest::prop_assert_eq!(whole.stats.drop_seq, 0);
        proptest::prop_assert_eq!(split.stats.drop_seq, 0);
        proptest::prop_assert_eq!(split.stats.segments, parts.len() as u64);
        proptest::prop_assert_eq!(output_fingerprint(&whole), output_fingerprint(&split));
    }
}

/// `ReplayDriver::new` refuses a schedule naming an instrument outside the
/// universe (the engine's own configure check).
#[test]
fn test_replay_driver_new_refuses_a_schedule_outside_the_universe() {
    let (frames, engine) = parse_schedule(&SCHEDULE_0916.replace("2885", "999")).expect("schedule");
    let (universe, contracts) = parse_universe(&universe_text()).expect("universe");
    let setup = ChainSetup {
        recipe: replay_recipe(&frames, &universe),
        snapshot: None,
        engine_frames_secs: frames,
        universe,
        contracts,
        trading_day: DAY / 86_400,
    };
    assert!(
        ReplayDriver::new(
            engine,
            setup,
            params(),
            DAY * NS,
            1_000_000,
            None,
            BootInstant::scheduled(DAY * NS),
        )
        .is_err()
    );
}

#[test]
fn test_note_frames_skipped_moves_the_loss_count_only() {
    let mut d = driver(SCHEDULE_0916, 1_000_000);
    d.note_frames_skipped();
    d.feed_frame(&frame(
        quote(B, 50.0, sod(9, 14, 55), 1),
        sod(9, 14, 55) * NS,
    ));
    d.note_frames_skipped();
    d.note_frames_skipped();
    let o = d.finish().expect("finish");
    assert_eq!(
        o.stats.drop_seq, 3,
        "the two after the chain started, and the one before it, which ended \
         at a frame after the 08:30 boot (handed as the chain is built)"
    );
    assert_eq!(o.stats.losses_before_chain, 0);
    // A held loss no frame of the day follows touched no chain candle.
    let mut d = driver(SCHEDULE_0916, 1_000_000);
    d.note_frames_skipped();
    let o = d.finish().expect("finish");
    assert_eq!((o.stats.drop_seq, o.stats.losses_before_chain), (0, 1));
    assert_eq!(
        o.stats.damaged_segments, 0,
        "a skip inside a segment is not a segment count"
    );
}

/// The cost of a synthetic full day, printed (`cargo test --release ... --
/// --ignored replay_full_day_cost_harness --nocapture`). `LF8_HARNESS_TICKS`
/// sets the tick count (default 64 million, the measured live day). A
/// wall-clock harness, never a CI gate (same shape as the rank sweep
/// harness).
#[test]
#[ignore = "wall-clock harness; run by hand in release"]
fn replay_full_day_cost_harness() {
    let ticks: u64 = std::env::var("LF8_HARNESS_TICKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64_000_000);
    let contracts = 2_000u32;
    let per_frame = 4u64;
    let frames = ticks / per_frame;
    let start = sod(9, 0, 0) * NS;
    let span = (sod(15, 40, 0) - sod(9, 0, 0)) * NS;
    let step = span / i64::try_from(frames.max(1)).expect("fits");
    let mut d = driver(SCHEDULE_0916, 1_000_000);
    let mut vols = vec![100u32; contracts as usize];
    let began = std::time::Instant::now();
    let mut bytes = Vec::with_capacity(200);
    for f in 0..frames {
        let t = start + i64::try_from(f).expect("fits") * step;
        bytes.clear();
        for k in 0..per_frame {
            let c = u32::try_from((f * per_frame + k) % u64::from(contracts)).expect("fits");
            let v = &mut vols[c as usize];
            *v = v.wrapping_add(3);
            bytes.extend(quote(
                10_000 + c,
                100.0 + (*v % 17) as f32 * 0.05,
                t / NS,
                *v,
            ));
        }
        d.feed_frame(&frame(bytes.clone(), t));
    }
    let o = d.finish().expect("finish");
    let peak = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "VmHWM: unknown".to_owned());
    use std::io::Write as _;
    let _ok = writeln!(
        std::io::stderr().lock(),
        "LF-8 harness: {ticks} ticks, {} frames, {} passes in {:.1} s; {peak}",
        o.stats.frames,
        o.stats.passes,
        began.elapsed().as_secs_f64()
    );
}

// ---- loss placement, out-of-band loss, self-check, incomplete day ----

/// `B` every 500 ms over `[from, to]` (IST seconds), with `A` once a minute;
/// volumes start at `vol` and climb. One frame per tick instant.
fn stream_frames(from: i64, to: i64, vol: &mut u32) -> Vec<ReplayedFrame> {
    let mut out = Vec::new();
    let mut t = from * NS;
    while t <= to * NS {
        let secs = t / NS;
        *vol += 10;
        let mut bytes = quote(B, 50.0, secs, *vol);
        if (secs - DAY).rem_euclid(60) == 0 && t % NS == 0 {
            bytes.extend(quote(A, 100.0, secs, *vol));
        }
        out.push(frame(bytes, t + 3_000_000));
        t += NS / 2;
    }
    out
}

fn seg_read(frames: Vec<ReplayedFrame>, damaged: bool, stopped_early: bool) -> SegmentRead {
    SegmentRead {
        frames,
        dropped_as_applied: 0,
        damaged,
        trailing_gap: false,
        stopped_early,
    }
}

fn schedule_at(t: &str) -> String {
    SCHEDULE_0916.replace("09:16:00", t)
}

/// Damage at the HEAD of a segment, resynced: the reader marks its first
/// frame `after_gap` (and `first_in_segment`). The loss is marked there, so
/// the 09:16 candle, released inside that segment, is refused. The frames
/// either side are 1 s apart, under the feed-gap threshold, so only the loss
/// can refuse it.
#[test]
fn test_feed_segment_marks_head_damage_at_the_first_frame() {
    let mut d = driver(SCHEDULE_0916, 1_000_000);
    let mut vol = 100;
    let mut first = vec![frame(
        quote(B, 50.0, sod(9, 14, 55), 1),
        sod(9, 14, 55) * NS,
    )];
    first.extend(stream_frames(sod(9, 15, 0), sod(9, 16, 20), &mut vol));
    feed_segment(&mut d, &seg_read(first, false, false));
    let mut second = stream_frames(sod(9, 16, 21), sod(9, 19, 0), &mut vol);
    second[0].after_gap = true;
    second[0].first_in_segment = true;
    feed_segment(&mut d, &seg_read(second, true, false));
    let o = d.finish().expect("finish");
    assert_eq!(o.stats.drop_seq, 1, "one loss, at the head of segment 2");
    assert_eq!(o.stats.damaged_segments, 1);
    assert_eq!(o.decisions.len(), 1);
    assert_eq!(
        o.decisions[0].taint.as_deref(),
        Some("tick_dropped"),
        "the 09:16 candle overlaps the lost head of segment 2"
    );
}

/// Damage in the MIDDLE of a segment, resynced, followed by another segment:
/// one loss at the skip, none after the segment, so a candle long after the
/// skip and before the next segment is trusted.
#[test]
fn test_feed_segment_marks_mid_damage_once_and_never_after_the_segment() {
    let mut d = driver(&schedule_at("09:19:00"), 1_000_000);
    let mut vol = 100;
    let mut first = vec![frame(
        quote(B, 50.0, sod(9, 14, 55), 1),
        sod(9, 14, 55) * NS,
    )];
    first.extend(stream_frames(sod(9, 15, 0), sod(9, 20, 0), &mut vol));
    let mid = first
        .iter()
        .position(|f| f.received_at_nanos + IST_OFFSET_NS > sod(9, 15, 30) * NS)
        .expect("mid");
    first[mid].after_gap = true;
    feed_segment(&mut d, &seg_read(first, true, false));
    let second = stream_frames(sod(9, 20, 0) + 1, sod(9, 22, 0), &mut vol);
    feed_segment(&mut d, &seg_read(second, false, false));
    let o = d.finish().expect("finish");
    assert_eq!(o.stats.drop_seq, 1, "one loss, at the skip only");
    assert_eq!(o.stats.damaged_segments, 1);
    assert_eq!(o.decisions.len(), 1);
    assert_eq!(
        o.decisions[0].taint, None,
        "the 09:19 candle is far from the 09:15:30 skip: no false loss after the segment"
    );
}

/// A walk that STOPPED early lost the rest of its segment: the loss is marked
/// after the segment, before the next one's ticks.
#[test]
fn test_feed_segment_marks_a_stopped_walk_after_the_segment() {
    let mut d = driver(SCHEDULE_0916, 1_000_000);
    let mut vol = 100;
    let mut first = vec![frame(
        quote(B, 50.0, sod(9, 14, 55), 1),
        sod(9, 14, 55) * NS,
    )];
    first.extend(stream_frames(sod(9, 15, 0), sod(9, 16, 50), &mut vol));
    feed_segment(&mut d, &seg_read(first, true, true));
    let second = stream_frames(sod(9, 16, 51), sod(9, 19, 0), &mut vol);
    feed_segment(&mut d, &seg_read(second, false, false));
    let o = d.finish().expect("finish");
    assert_eq!(o.stats.drop_seq, 1);
    assert_eq!(o.stats.damaged_segments, 1);
    assert_eq!(o.decisions[0].taint.as_deref(), Some("tick_dropped"));
}

/// A loss found before the day's chain exists is judged by where it ends
/// (`ReplayDriver::note_frames_skipped`). One in a damaged segment of the
/// previous date, followed by a frame of that date, touched no chain candle
/// (the live tap takes its baseline when it is created): counted, never
/// handed to the chain. Damage at the head of the day's first segment, whose
/// first readable frame (09:14:55) is after the 08:30 boot, may have lost
/// frames the live chain folded: it is handed to the chain as it is built,
/// and taints only up to that frame (plus the skew), so the 09:16 bar stays
/// trusted (LF-8 review round 5; before it, this loss was never handed).
#[test]
fn test_a_loss_before_the_chain_exists_taints_no_opening_bar_note_frames_skipped() {
    let mut d = driver(SCHEDULE_0916, 1_000_000);
    let mut vol = 100;
    // The previous date's newest segment, damaged and resynced: all its
    // frames are other-day, so they never start the chain.
    let mut prev = vec![frame(quote(B, 50.0, DAY - 600, 1), (DAY - 600) * NS)];
    prev.push(frame(quote(B, 50.0, DAY - 590, 2), (DAY - 590) * NS));
    prev[1].after_gap = true;
    feed_segment(&mut d, &seg_read(prev, true, false));
    // The day's first segment, damaged at its head.
    let mut first = vec![frame(
        quote(B, 50.0, sod(9, 14, 55), 1),
        sod(9, 14, 55) * NS,
    )];
    first[0].after_gap = true;
    first[0].first_in_segment = true;
    first.extend(stream_frames(sod(9, 15, 0), sod(9, 19, 0), &mut vol));
    feed_segment(&mut d, &seg_read(first, true, false));
    let o = d.finish().expect("finish");
    assert_eq!(
        o.stats.losses_before_chain, 1,
        "the previous date's loss ended on that date"
    );
    assert_eq!(
        o.stats.drop_seq, 1,
        "the day's head loss ended after the boot: handed to the chain"
    );
    assert_eq!(o.stats.damaged_segments, 2);
    assert_eq!(o.decisions.len(), 1);
    assert_eq!(
        o.decisions[0].taint, None,
        "the head loss ended at 09:14:55, so it does not reach the 09:16 bar"
    );
}

/// Damage at the head of the day's first segment that ends AFTER the open
/// (the reader resynced to a frame received at 09:15:01.6, inside the
/// first-seen opening window) lost the 09:15:00 trades the live chain folded:
/// the 09:15 bar is refused `tick_dropped`, as one segment later in the day
/// would be. Before the fix the loss was only counted (`losses_before_chain`)
/// and the bar was decided, filled and reported with no taint (LF-8 review
/// round 5). A head loss whose frame is at or before the chain's start (a
/// frame before `--boot-ist`) still touched no chain candle.
#[test]
fn test_head_damage_past_the_boot_refuses_the_opening_bar_note_frames_skipped() {
    let schedule = schedule_at("09:15:00");
    let opening = |o: &ReplayOutput| {
        o.decisions
            .iter()
            .find(|d| d.candle_start_ns == sod(9, 15, 0) * NS)
            .cloned()
    };
    let replay = |boot: BootInstant, head_damaged: bool| {
        let mut d = driver_booted(&schedule, 1_000_000, boot);
        let mut bytes = quote(B, 50.0, sod(9, 15, 1), 110);
        bytes.extend(quote(A, 100.0, sod(9, 15, 1), 150));
        let mut first = vec![frame(bytes, sod(9, 15, 1) * NS + 600_000_000)];
        first[0].after_gap = head_damaged;
        first[0].first_in_segment = true;
        let mut vol = 200;
        first.extend(stream_frames(sod(9, 15, 2), sod(9, 17, 30), &mut vol));
        feed_segment(&mut d, &seg_read(first, head_damaged, false));
        d.finish().expect("finish")
    };
    let boot = BootInstant::given(DAY * NS, 8 * 3_600 + 30 * 60);
    let clean = replay(boot, false);
    assert_eq!(
        opening(&clean).expect("decided").taint,
        None,
        "the control: no damage, the opening bar is trusted"
    );
    let damaged = replay(boot, true);
    assert_eq!(damaged.stats.drop_seq, 1, "handed to the chain");
    assert_eq!(damaged.stats.losses_before_chain, 0);
    assert_eq!(
        opening(&damaged).and_then(|d| d.taint).as_deref(),
        Some("tick_dropped"),
        "the lost 09:15:00 trades refuse the opening bar"
    );
    // The same damage before a frame received BEFORE a given boot (the chain
    // starts at that frame): the stretch ended at or before the chain's
    // start, so no chain candle was touched.
    let early = replay(BootInstant::given(DAY * NS, 9 * 3_600 + 15 * 60 + 2), true);
    assert_eq!(early.stats.drop_seq, 0);
    assert_eq!(early.stats.losses_before_chain, 1);
}

/// A silence across the 09:15 open or the 15:30 close is measured, clamped at
/// the session's ends (as the chain's feed-gap watch clamps it at the open),
/// so a day missing 09:15-10:00 or 15:00-15:30 is not read as complete
/// (`ReplayDriver::feed_frame`, `incomplete_reasons`).
#[test]
fn test_feed_frame_measures_a_silence_across_the_open_and_the_close() {
    let gap_of = |times: &[i64]| {
        let mut d = driver(SCHEDULE_0916, NS);
        for &t in times {
            d.feed_frame(&frame(quote(B, 50.0, t, 1), t * NS));
        }
        d.stats()
    };
    // Every 2 s over [from, to] (IST seconds).
    let every_2s = |from: i64, to: i64| (from..=to).step_by(2).collect::<Vec<i64>>();
    // Up 08:30 to 09:14:50, down until 10:00, then steady to 15:40: the
    // 09:15-10:00 hole is the longest silence.
    let mut t = vec![sod(8, 30, 0), sod(9, 14, 50)];
    t.extend(every_2s(sod(10, 0, 0), sod(15, 40, 0)));
    let s = gap_of(&t);
    assert_eq!(s.max_session_gap_ns, 45 * 60 * NS, "09:15 to 10:00");
    let r = incomplete_reasons(DAY_TEXT, &s, 1);
    assert!(r.iter().any(|x| x.contains("silence")), "{r:?}");
    assert!(
        r.iter().any(|x| x.contains("process restart")),
        "a 45-minute silence is named as a possible restart: {r:?}"
    );
    let short = super::drive::ReplayStats {
        max_session_gap_ns: 10 * NS,
        ..s
    };
    let r = incomplete_reasons(DAY_TEXT, &short, 1);
    assert!(
        r.iter()
            .any(|x| x.contains("silence") && !x.contains("restart")),
        "a 10 s silence is not: {r:?}"
    );
    assert!(
        out::HONEST_LIMITS
            .iter()
            .any(|l| l.contains("process restart") && l.contains("flat paper position")),
        "the limits say a restart is not reproduced"
    );
    // Steady from 09:15 to 15:00, then nothing until 15:35: the last 30
    // minutes of the session are a silence.
    let mut t = vec![sod(9, 14, 50)];
    t.extend(every_2s(sod(9, 15, 0), sod(15, 0, 0)));
    t.push(sod(15, 35, 0));
    let s = gap_of(&t);
    assert_eq!(
        s.max_session_gap_ns,
        30 * 60 * NS,
        "15:00 to the 15:30 close"
    );
    assert!(
        incomplete_reasons(DAY_TEXT, &s, 1)
            .iter()
            .any(|x| x.contains("silence")),
        "a day missing 15:00-15:30 is not complete"
    );
    let s = gap_of(&[sod(15, 29, 0), sod(15, 31, 0)]);
    assert_eq!(
        s.max_session_gap_ns,
        60 * NS,
        "15:29:00 to the 15:30 close, not to the 15:31 frame"
    );
    let s = gap_of(&[sod(8, 30, 0), sod(9, 14, 0)]);
    assert_eq!(
        s.max_session_gap_ns, 0,
        "a pre-open silence is not in session"
    );
}

/// With a `--pass-us` that does not divide the grid's end instant, the grid
/// stops short of it; one more pass at exactly the end, run before the frame
/// that reached it is tapped, still finishes an order held to it (a decision
/// on the 15:29 bar), so it is never left with a decision row and nothing
/// after, and it ends exactly as on the default 1 ms grid
/// (`ReplayDriver::feed_frame`, `ReplayDriver::finish`). `A` stops trading at
/// 15:29:00, so nothing fills the order early: it is held to the end of its
/// fill window (15:31:02.000 with the default waits, not a multiple of 3 ms).
#[test]
fn test_an_order_held_to_the_grid_end_finishes_for_an_odd_pass_us() {
    assert_ne!(
        grid_end_ns(DAY * NS, &params()).rem_euclid(3_000_000),
        0,
        "the end is off the 3 ms grid"
    );
    let ending = |pass_ns: i64| {
        let mut d = driver(&schedule_at("15:29:00"), pass_ns);
        let mut vol = 100u32;
        let mut t = sod(15, 27, 0) * NS;
        while t <= sod(15, 40, 0) * NS {
            let secs = t / NS;
            vol += 10;
            let mut bytes = quote(B, 50.0, secs, vol);
            if secs <= sod(15, 29, 0) && (secs - DAY).rem_euclid(60) == 0 && t % NS == 0 {
                bytes.extend(quote(A, 100.0, secs, vol));
            }
            d.feed_frame(&frame(bytes, t + 3_000_000));
            t += NS / 2;
        }
        let o = d.finish().expect("finish");
        assert_eq!(o.decisions.len(), 1, "{:?}", o.decisions);
        let sid = o.decisions[0].signal_id;
        o.events
            .iter()
            .filter(|e| e.signal_id == sid)
            .map(|e| (e.event, e.reason.clone(), e.ts_ist_nanos))
            .collect::<Vec<_>>()
    };
    let odd = ending(3_000_000);
    let last = odd.last().expect("rows");
    assert!(
        matches!(
            last.0,
            LiveOrderEventKind::Reject | LiveOrderEventKind::Fill | LiveOrderEventKind::Cancel
        ),
        "the 15:29 order ended: {odd:?}"
    );
    assert_eq!(
        last.2,
        grid_end_ns(DAY * NS, &params()),
        "at the grid's end"
    );
    let default = ending(1_000_000);
    let kinds = |v: &[(LiveOrderEventKind, Option<String>, i64)]| {
        v.iter()
            .map(|(k, r, _)| (*k, r.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        kinds(&odd),
        kinds(&default),
        "same outcome as the 1 ms grid"
    );
}

/// A loss between a release instant and the next tick is seen by the chain at
/// once, as the live thread reads the tap's loss total before every pass: the
/// 09:16 candle is refused at its release, never placed and then dropped.
#[test]
fn test_a_loss_before_the_release_is_seen_before_the_release_note_frames_skipped() {
    let mut d = driver(SCHEDULE_0916, 1_000_000);
    let mut vol = 100;
    for f in std::iter::once(frame(
        quote(B, 50.0, sod(9, 14, 55), 1),
        sod(9, 14, 55) * NS,
    ))
    .chain(stream_frames(sod(9, 15, 0), sod(9, 17, 0), &mut vol))
    {
        d.feed_frame(&f);
    }
    // 09:17:00.950: a frame whose tail is abandoned (a loss), then nothing
    // until 09:17:01.200, after the 09:16 candle's release at 09:17:01.000.
    let mut torn = quote(B, 50.0, sod(9, 17, 0), vol + 5);
    torn.extend_from_slice(&[4, 50, 0, 1]);
    d.feed_frame(&frame(torn, sod(9, 17, 0) * NS + 950_000_000));
    for f in stream_frames(sod(9, 17, 1), sod(9, 19, 0), &mut vol)
        .into_iter()
        .map(|mut f| {
            f.received_at_nanos += 200_000_000 - 3_000_000;
            f
        })
    {
        d.feed_frame(&f);
    }
    let o = d.finish().expect("finish");
    assert_eq!(o.stats.drop_seq, 1);
    assert_eq!(o.decisions.len(), 1);
    let sid = o.decisions[0].signal_id;
    assert_eq!(o.decisions[0].taint.as_deref(), Some("tick_dropped"));
    let kinds: Vec<LiveOrderEventKind> = o
        .events
        .iter()
        .filter(|e| e.signal_id == sid)
        .map(|e| e.event)
        .collect();
    assert!(
        !kinds.contains(&LiveOrderEventKind::Built) && !kinds.contains(&LiveOrderEventKind::Sent),
        "refused at the decision, never built or sent: {kinds:?}"
    );
}

/// The worst-case self-check reads the LF-7 re-pricing: a fill booked worse
/// than the worst case the replay's own 1 s bars allow exits 3, though no row
/// carries `worse_than_model = true`.
#[test]
fn test_a_fill_booked_worse_than_the_worst_case_exits_three_self_check_failures() {
    let mut o = run(SCHEDULE_0916, sod(9, 19, 0));
    let clean = out::day_report(DAY_TEXT, &o, &[]);
    assert_eq!(self_check_failures(&clean, &o.events), 0);
    assert_eq!(final_exit_code(0, 0, false, false, false), EXIT_OK);
    let fill = o
        .events
        .iter_mut()
        .find(|e| e.event == LiveOrderEventKind::Fill)
        .expect("a fill");
    fill.avg_price_paise = fill.avg_price_paise.map(|p| p + 5_000);
    assert!(out::worse_than_worst_rows(&o.events).is_empty());
    let report = out::day_report(DAY_TEXT, &o, &[]);
    let failures = self_check_failures(&report, &o.events);
    assert!(failures > 0, "{:?}", report.counts);
    assert_eq!(
        final_exit_code(failures, 0, false, false, true),
        EXIT_SELF_CHECK
    );
}

/// A day whose head is missing (first frame after the open) is incomplete
/// and exits 2 without `--allow-partial`.
#[test]
fn test_incomplete_reasons_flags_a_missing_head_final_exit_code() {
    let mut s = super::drive::ReplayStats {
        first_receipt_ist_ns: Some(sod(10, 0, 0) * NS),
        last_receipt_ist_ns: Some(sod(15, 39, 0) * NS),
        max_session_gap_ns: NS,
        ..Default::default()
    };
    let r = incomplete_reasons(DAY_TEXT, &s, 1);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(r[0].contains("after the 09:15 open"), "{r:?}");
    assert_eq!(
        final_exit_code(0, 0, false, !r.is_empty(), false),
        EXIT_BAD_INPUT
    );
    assert_eq!(final_exit_code(0, 0, false, !r.is_empty(), true), EXIT_OK);
    s.first_receipt_ist_ns = Some(sod(9, 14, 55) * NS);
    assert!(incomplete_reasons(DAY_TEXT, &s, 1).is_empty());
    s.first_receipt_ist_ns = Some(sod(9, 15, 4) * NS);
    assert!(
        incomplete_reasons(DAY_TEXT, &s, 1).is_empty(),
        "inside the gap"
    );
    s.last_receipt_ist_ns = Some(sod(15, 0, 0) * NS);
    assert_eq!(incomplete_reasons(DAY_TEXT, &s, 1).len(), 1);
    assert_eq!(final_exit_code(0, 1, false, true, true), EXIT_BAD_INPUT);
    assert_eq!(
        incomplete_reasons(DAY_TEXT, &super::drive::ReplayStats::default(), 1).len(),
        1
    );
}

/// No frame of the day is a hard refusal that `--allow-partial` does not
/// waive, and a run with no decision is incomplete (it proves nothing) and
/// its report never reads Green (`final_exit_code`, `incomplete_reasons`,
/// `out::day_report`).
#[test]
fn test_no_frame_of_the_day_is_hard_and_no_decision_never_reads_green_final_exit_code() {
    assert_eq!(
        final_exit_code(0, 0, true, true, true),
        EXIT_BAD_INPUT,
        "no frame of the day: --allow-partial does not accept it"
    );
    assert_eq!(final_exit_code(0, 0, false, true, true), EXIT_OK);
    let complete = super::drive::ReplayStats {
        first_receipt_ist_ns: Some(sod(9, 14, 55) * NS),
        last_receipt_ist_ns: Some(sod(15, 39, 0) * NS),
        max_session_gap_ns: NS,
        ..Default::default()
    };
    assert!(incomplete_reasons(DAY_TEXT, &complete, 1).is_empty());
    let r = incomplete_reasons(DAY_TEXT, &complete, 0);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(r[0].contains("no decision"), "{r:?}");
    // A stream on which the scheduled bar never closes: B trades, the
    // schedule names A's 15:00 bar, the stream stops at 09:19. The first
    // frame comes before 09:00 (a pre-open boot), so nothing else in the
    // report would read it Amber.
    let mut d = driver(&schedule_at("15:00:00"), NS);
    d.feed_frame(&frame(quote(B, 50.0, sod(8, 50, 0), 1), sod(8, 50, 0) * NS));
    feed_stream(&mut d, sod(9, 19, 0));
    let o = d.finish().expect("finish");
    assert!(o.decisions.is_empty());
    let report = out::day_report(DAY_TEXT, &o, &[]);
    assert_ne!(report.light, "green", "{:?}", report.status_reasons);
    let with_one = run(SCHEDULE_0916, sod(9, 19, 0));
    assert_eq!(with_one.decisions.len(), 1);
    assert!(
        !out::day_report(DAY_TEXT, &with_one, &[])
            .status_reasons
            .contains(&"unsettled"),
        "a run with decisions is settled"
    );
}

/// A day the replay itself finds incomplete never reads complete and Green
/// in the LF-7 report files, which are read and shared without summary.txt:
/// the run below has a decision but ends at 09:19, so its last frame came
/// before 15:30 (LF-8 review, the report's false OK).
#[test]
fn test_an_incomplete_day_report_reads_partial_and_amber_day_report() {
    let o = run(SCHEDULE_0916, sod(9, 19, 0));
    assert_eq!(o.decisions.len(), 1);
    let reasons = incomplete_reasons(DAY_TEXT, &o.stats, o.decisions.len());
    assert!(
        reasons.iter().any(|r| r.contains("before 15:30")),
        "{reasons:?}"
    );
    let report = out::day_report(DAY_TEXT, &o, &reasons);
    assert_eq!(report.status, "partial", "{:?}", report.status_reasons);
    assert_eq!(report.light, "amber");
    assert!(report.status_reasons.contains(&"unsettled"));
    let files = out::report_files(&report);
    let json = &files
        .iter()
        .find(|(n, _)| n.ends_with(".json"))
        .expect("json")
        .1;
    assert!(
        json.contains("partial") && !json.contains("green"),
        "{json}"
    );
    // The same run judged complete reads complete, so the reasons alone move
    // it.
    let complete = out::day_report(DAY_TEXT, &o, &[]);
    assert_eq!(complete.status, "complete");
    assert_eq!(complete.light, "green");
}

// ---- real WAL segments ----

/// Writes each part as one real WAL segment (the live writer), named after
/// its first frame's receipt so the name falls on the day (as the live box
/// names them). Returns the paths, in order.
fn write_segments(dir: &std::path::Path, parts: &[Vec<ReplayedFrame>]) -> Vec<std::path::PathBuf> {
    use tickvault_storage::ws_frame_spill::WsFrameSpill;
    std::fs::create_dir_all(dir).expect("dir");
    let mut out = Vec::new();
    for (pi, part) in parts.iter().enumerate() {
        let Some(first) = part.first() else {
            continue;
        };
        let tmp = dir.join(format!("part-{pi}"));
        std::fs::create_dir_all(&tmp).expect("dir");
        let spill = WsFrameSpill::new(&tmp).expect("spill");
        for (i, f) in part.iter().enumerate() {
            let _ = spill.append_with_seq_at(
                WsType::LiveFeed,
                f.frame.clone(),
                (pi as u64 + 1) * 1_000_000 + i as u64,
                f.received_at_nanos,
                WalEndpoint::MainFeed,
            );
        }
        assert_eq!(spill.shutdown(std::time::Duration::from_secs(20)), 0);
        drop(spill);
        let seg = std::fs::read_dir(&tmp)
            .expect("list")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|e| e == "wal"))
            .expect("one segment");
        let name = dir.join(format!("ws-frames-{}.wal", first.received_at_nanos));
        std::fs::rename(&seg, &name).expect("rename");
        let _ = std::fs::remove_dir_all(&tmp);
        out.push(name);
    }
    out
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tv-lf8-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Replays real segments through the reader and `feed_segment`.
fn replay_segments(schedule: &str, paths: &[std::path::PathBuf]) -> ReplayOutput {
    let mut d = driver(schedule, 1_000_000);
    for p in paths {
        let (read, _) = super::source::read_main_feed_segment(p, None).expect("read");
        feed_segment(&mut d, &read);
    }
    d.finish().expect("finish")
}

/// Damage at the head of a REAL segment (its first record's magic flipped)
/// is read back as `after_gap` on its first frame and marked there.
#[test]
fn test_real_segment_head_damage_marks_the_loss_at_its_first_frame() {
    let dir = tmp_dir("head");
    let mut vol = 100;
    let mut first = vec![frame(
        quote(B, 50.0, sod(9, 14, 55), 1),
        sod(9, 14, 55) * NS,
    )];
    first.extend(stream_frames(sod(9, 15, 0), sod(9, 16, 20), &mut vol));
    let second = stream_frames(sod(9, 16, 21), sod(9, 19, 0), &mut vol);
    let paths = write_segments(&dir, &[first, second]);
    let mut body = std::fs::read(&paths[1]).expect("read");
    body[0] ^= 0xFF;
    std::fs::write(&paths[1], body).expect("damage");
    let (read, _) = super::source::read_main_feed_segment(&paths[1], None).expect("read");
    assert!(read.damaged && !read.stopped_early);
    assert!(read.frames[0].after_gap && read.frames[0].first_in_segment);
    let o = replay_segments(SCHEDULE_0916, &paths);
    assert_eq!(o.stats.drop_seq, 1);
    assert_eq!(o.stats.damaged_segments, 1);
    assert_eq!(o.decisions[0].taint.as_deref(), Some("tick_dropped"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A gzip member holding `data` in STORED (uncompressed) deflate blocks: no
/// compression crate in this one's dependencies, and the reader only needs a
/// valid stream. O(len).
fn gzip_stored(data: &[u8]) -> Vec<u8> {
    let crc = data.iter().fold(!0u32, |mut c, &b| {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 == 1 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
        }
        c
    }) ^ !0u32;
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    let chunks: Vec<&[u8]> = data.chunks(65_535).collect();
    for (i, c) in chunks.iter().enumerate() {
        out.push(u8::from(i + 1 == chunks.len()));
        let len = u16::try_from(c.len()).expect("fits");
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(c);
    }
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&u32::try_from(data.len()).expect("fits").to_le_bytes());
    out
}

/// A gzipped input reports its UNZIPPED size, the bytes actually held (the
/// `.gz` file's own size is not what the replay loads).
#[test]
fn test_read_main_feed_segment_reports_the_unzipped_size_of_a_gz_input() {
    let dir = tmp_dir("gzsize");
    let mut vol = 100;
    let paths = write_segments(
        &dir,
        &[stream_frames(sod(9, 15, 0), sod(9, 16, 0), &mut vol)],
    );
    let plain = std::fs::read(&paths[0]).expect("read");
    let gz = dir.join(format!(
        "{}.gz",
        paths[0].file_name().and_then(|n| n.to_str()).expect("name")
    ));
    std::fs::write(&gz, gzip_stored(&plain)).expect("write");
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).expect("dir");
    let (read, bytes) = super::source::read_main_feed_segment(&gz, Some(&staging)).expect("read");
    assert_eq!(bytes, plain.len() as u64);
    assert_ne!(std::fs::metadata(&gz).expect("meta").len(), bytes);
    assert_eq!(read.frames.len(), 121);
    let (_, plain_bytes) = super::source::read_main_feed_segment(&paths[0], None).expect("read");
    assert_eq!(plain_bytes, plain.len() as u64);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The CLI refuses, with nothing read and `--out` untouched: a missing
/// `--compare` file, a non-empty `--staging`, a segment held twice (exit 2),
/// and a sidecar copy (exit 3).
#[test]
fn test_run_cli_refuses_compare_staging_duplicate_and_sidecar_before_replaying() {
    let base = tmp_dir("cli2");
    std::fs::create_dir_all(&base).expect("dir");
    let sched = base.join("s.toml");
    std::fs::write(&sched, SCHEDULE_0916).expect("write");
    let uni = base.join("u.csv");
    std::fs::write(&uni, universe_text()).expect("write");
    let wal = base.join("wal");
    let mut vol = 100;
    let paths = write_segments(
        &wal,
        &[stream_frames(sod(9, 15, 0), sod(9, 16, 0), &mut vol)],
    );
    let config =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/base.toml");
    let args = |extra: &[&str], out: &str| -> Vec<String> {
        let mut v: Vec<String> = [
            "tv-live-fill-replay",
            "--day",
            DAY_TEXT,
            "--wal-dir",
            wal.to_str().expect("utf8"),
            "--schedule",
            sched.to_str().expect("utf8"),
            "--universe",
            uni.to_str().expect("utf8"),
            "--config",
            config.to_str().expect("utf8"),
            "--out",
            base.join(out).to_str().expect("utf8"),
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        v.extend(extra.iter().map(|s| (*s).to_owned()));
        v
    };
    let missing = base.join("no-such-live.csv");
    assert_eq!(
        run_cli(&args(&["--compare", missing.to_str().expect("utf8")], "o1")),
        EXIT_BAD_INPUT
    );
    assert!(
        !base.join("o1").exists(),
        "the compare file is read before anything else"
    );
    let staging = base.join("staging");
    std::fs::create_dir_all(&staging).expect("dir");
    std::fs::write(staging.join("ws-frames-9.wal.gz"), b"mine").expect("write");
    assert_eq!(
        run_cli(&args(&["--staging", staging.to_str().expect("utf8")], "o2")),
        EXIT_BAD_INPUT
    );
    assert!(!base.join("o2").exists());
    assert_eq!(
        std::fs::read(staging.join("ws-frames-9.wal.gz")).expect("kept"),
        b"mine"
    );
    let gz = std::path::PathBuf::from(format!("{}.gz", paths[0].display()));
    std::fs::copy(&paths[0], &gz).expect("copy");
    assert_eq!(run_cli(&args(&[], "o3")), EXIT_BAD_INPUT, "held twice");
    std::fs::remove_file(&gz).expect("rm");
    let sidecar = std::path::PathBuf::from(format!("{}.sha-ab.gz", paths[0].display()));
    std::fs::write(&sidecar, b"x").expect("write");
    assert_eq!(run_cli(&args(&[], "o4")), EXIT_SELF_CHECK, "a second copy");
    std::fs::remove_file(&sidecar).expect("rm");
    // The same second copy UNZIPPED (an operator ran `gunzip *.gz` on a
    // downloaded day) is refused the same way, never silently skipped
    // (LF-8 review round 5).
    let unzipped = std::path::PathBuf::from(format!("{}.sha-ab", paths[0].display()));
    std::fs::write(&unzipped, b"x").expect("write");
    assert_eq!(
        run_cli(&args(&[], "o4u")),
        EXIT_SELF_CHECK,
        "an unzipped second copy"
    );
    assert!(!base.join("o4u").exists(), "refused before --out is made");
    std::fs::remove_file(&unzipped).expect("rm");
    // A chosen `.gz` segment with no `--staging` is bad input (exit 2),
    // refused before `--out` is made or any segment is read, never an I/O
    // failure (exit 4) after the plain segments before it were replayed.
    let plain = std::fs::read(&paths[0]).expect("read");
    std::fs::write(&gz, gzip_stored(&plain)).expect("write");
    std::fs::remove_file(&paths[0]).expect("rm");
    assert_eq!(
        run_cli(&args(&[], "o4g")),
        EXIT_BAD_INPUT,
        "a .gz with no --staging"
    );
    assert!(!base.join("o4g").exists(), "refused before --out is made");
    std::fs::write(&paths[0], &plain).expect("write");
    std::fs::remove_file(&gz).expect("rm");
    // A sidecar and a twice-held copy of a segment of ANOTHER day (a folder
    // of a week's downloads) are not read, so they block nothing.
    let old = name_nanos(sod(10, 0, 0) - 5 * 86_400);
    std::fs::write(wal.join(format!("ws-frames-{old}.wal.sha-ab.gz")), b"x").expect("write");
    std::fs::write(wal.join(format!("ws-frames-{old}.wal")), b"").expect("write");
    std::fs::write(wal.join(format!("ws-frames-{old}.wal.gz")), b"").expect("write");
    assert_eq!(
        run_cli(&args(&["--allow-partial"], "o5")),
        EXIT_OK,
        "the clean folder replays"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A folder holding only a segment named BEFORE the day (the day not copied
/// yet) is refused as having no segment of the day, `--allow-partial` or
/// not: it never replays an earlier segment alone to a green, empty day.
#[test]
fn test_run_cli_refuses_a_folder_with_no_segment_named_on_the_day() {
    let base = tmp_dir("cli3");
    std::fs::create_dir_all(&base).expect("dir");
    let sched = base.join("s.toml");
    std::fs::write(&sched, SCHEDULE_0916).expect("write");
    let uni = base.join("u.csv");
    std::fs::write(&uni, universe_text()).expect("write");
    let wal = base.join("wal");
    let mut vol = 100;
    let paths = write_segments(
        &wal,
        &[stream_frames(sod(9, 15, 0), sod(9, 16, 0), &mut vol)],
    );
    // The same bytes, named on the previous date only.
    let prev = wal.join(format!(
        "ws-frames-{}.wal",
        name_nanos(sod(16, 0, 0) - 86_400)
    ));
    std::fs::rename(&paths[0], &prev).expect("rename");
    let config =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/base.toml");
    for (extra, out) in [(None, "o1"), (Some("--allow-partial"), "o2")] {
        let mut v: Vec<String> = [
            "tv-live-fill-replay",
            "--day",
            DAY_TEXT,
            "--wal-dir",
            wal.to_str().expect("utf8"),
            "--schedule",
            sched.to_str().expect("utf8"),
            "--universe",
            uni.to_str().expect("utf8"),
            "--config",
            config.to_str().expect("utf8"),
            "--out",
            base.join(out).to_str().expect("utf8"),
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        v.extend(extra.map(str::to_owned));
        assert_eq!(run_cli(&v), EXIT_BAD_INPUT, "{extra:?}");
        assert!(
            !base.join(out).join("summary.txt").exists(),
            "refused before replaying"
        );
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// The chain and the fold start at the live BOOT, not at the day's first
/// frame. A box that booted at 09:14:40 and got its first frame at
/// 09:15:01.2 (carrying a 09:15:01 trade) decided the opening bar live; a
/// chain built at that frame refused it as `chain_start`. Without
/// `--boot-ist` a first frame after 09:10 reads incomplete, and a frame
/// before a given `--boot-ist` does too (`BootInstant`, `ReplayDriver::new`,
/// `incomplete_reasons`).
#[test]
fn test_chain_and_fold_start_at_the_boot_not_the_first_frame_boot_instant() {
    let schedule = schedule_at("09:15:00");
    let first_ns = sod(9, 15, 1) * NS + 200_000_000;
    let replay = |boot: BootInstant| {
        let mut d = driver_booted(&schedule, 1_000_000, boot);
        let mut bytes = quote(B, 50.0, sod(9, 15, 1), 110);
        bytes.extend(quote(A, 100.0, sod(9, 15, 1), 150));
        d.feed_frame(&frame(bytes, first_ns));
        let mut vol = 200;
        for f in stream_frames(sod(9, 15, 2), sod(9, 17, 30), &mut vol) {
            d.feed_frame(&f);
        }
        d.finish().expect("finish")
    };
    let opening = |o: &ReplayOutput| {
        o.decisions
            .iter()
            .find(|d| d.candle_start_ns == sod(9, 15, 0) * NS)
            .cloned()
    };
    // Booted before the open: the opening bar is decided, untainted.
    let booted = replay(BootInstant::given(DAY * NS, 9 * 3_600 + 14 * 60 + 40));
    let decided = opening(&booted).expect("the 09:15 bar is decided");
    assert_eq!(decided.taint, None, "{decided:?}");
    assert_eq!(
        booted.stats.chain_start_ist_ns,
        Some(sod(9, 14, 40) * NS),
        "the chain starts at the boot"
    );
    assert_eq!(booted.stats.frames_before_boot, 0);
    assert!(
        incomplete_reasons(DAY_TEXT, &booted.stats, booted.decisions.len())
            .iter()
            .all(|r| !r.contains("--boot-ist")),
        "a given boot before the first frame is not a reason"
    );
    // Booted at the first frame itself (what the replay did before): no
    // untainted decision on the opening bar, as a chain built after 09:15
    // treats its first candles as untrusted (here it decides none at all).
    let late = replay(BootInstant::given(DAY * NS, 9 * 3_600 + 15 * 60 + 1));
    assert!(
        opening(&late).is_none_or(|d| d.taint.is_some()),
        "a chain built after 09:15 never decides its first candle as trusted: {:?}",
        opening(&late)
    );
    // A frame before a given boot is an earlier process's: the replay
    // starts at it and says so.
    let early = replay(BootInstant::given(DAY * NS, 9 * 3_600 + 15 * 60 + 2));
    assert_eq!(early.stats.frames_before_boot, 1);
    assert_eq!(early.stats.chain_start_ist_ns, Some(first_ns));
    assert!(
        incomplete_reasons(DAY_TEXT, &early.stats, early.decisions.len())
            .iter()
            .any(|r| r.contains("1 frames came before --boot-ist")),
        "{:?}",
        incomplete_reasons(DAY_TEXT, &early.stats, early.decisions.len())
    );
    // No --boot-ist: the scheduled 08:30 boot is assumed (the opening bar is
    // decided as by a pre-market boot), and a first frame after 09:10 leaves
    // the boot unknown, so the day is not reported complete.
    let assumed = replay(BootInstant::scheduled(DAY * NS));
    assert_eq!(
        assumed.stats.chain_start_ist_ns,
        Some(sod(8, 30, 0) * NS),
        "the scheduled boot"
    );
    assert_eq!(opening(&assumed).expect("decided").taint, None);
    let r = incomplete_reasons(DAY_TEXT, &assumed.stats, assumed.decisions.len());
    assert!(
        r.iter()
            .any(|r| r.contains("after 09:10") && r.contains("--boot-ist HH:MM:SS")),
        "{r:?}"
    );
    // The same first frame at 09:10:00 or earlier leaves nothing unknown.
    let mut s = assumed.stats;
    s.first_receipt_ist_ns = Some(sod(9, 10, 0) * NS);
    assert!(
        incomplete_reasons(DAY_TEXT, &s, 1)
            .iter()
            .all(|r| !r.contains("--boot-ist")),
        "a first frame at 09:10:00 is a pre-market boot either way"
    );
}

/// The fold is handed over as live hands it over (`finish_wal_replay`): its
/// capture start is provisional and the first tick folded confirms it at its
/// own receipt, so the fold's first-trade rule reads the day's FIRST RECEIPT,
/// not the boot. Booted at 09:59:30, first frame at 10:01:30.2 carrying
/// `A`'s 10:01:00 trade (30 s old, inside the fresh-first lag): live reads
/// it as no new trade (10:01:00 is before its confirmed capture start
/// 10:01:30), so its 10:01 bar gets no trade and live makes no trusted
/// decision on it. Before the fix the replay read it as a new trade and
/// decided the bar untainted (LF-8 review round 5).
#[test]
fn test_the_fold_capture_start_is_confirmed_at_the_first_receipt_as_live() {
    let mut d = driver_booted(
        &schedule_at("10:01:00"),
        1_000_000,
        BootInstant::given(DAY * NS, 9 * 3_600 + 59 * 60 + 30),
    );
    let mut bytes = quote(B, 50.0, sod(10, 1, 30), 110);
    bytes.extend(quote(A, 100.0, sod(10, 1, 0), 150));
    d.feed_frame(&frame(bytes, sod(10, 1, 30) * NS + 200_000_000));
    let mut vol = 200;
    for f in stream_frames(sod(10, 1, 31), sod(10, 3, 30), &mut vol) {
        d.feed_frame(&f);
    }
    let o = d.finish().expect("finish");
    assert_eq!(o.stats.chain_start_ist_ns, Some(sod(9, 59, 30) * NS));
    let bar = o
        .decisions
        .iter()
        .find(|x| x.candle_start_ns == sod(10, 1, 0) * NS);
    assert!(
        bar.is_none_or(|x| x.taint.is_some()),
        "a trade older than the first receipt is not a new trade: {bar:?}"
    );
    // The control: the same trade at the receipt second IS a new trade.
    let mut d = driver_booted(
        &schedule_at("10:01:00"),
        1_000_000,
        BootInstant::given(DAY * NS, 9 * 3_600 + 59 * 60 + 30),
    );
    let mut bytes = quote(B, 50.0, sod(10, 1, 30), 110);
    bytes.extend(quote(A, 100.0, sod(10, 1, 30), 150));
    d.feed_frame(&frame(bytes, sod(10, 1, 30) * NS + 200_000_000));
    let mut vol = 200;
    for f in stream_frames(sod(10, 1, 31), sod(10, 3, 30), &mut vol) {
        d.feed_frame(&f);
    }
    let o = d.finish().expect("finish");
    let bar = o
        .decisions
        .iter()
        .find(|x| x.candle_start_ns == sod(10, 1, 0) * NS)
        .expect("decided");
    assert_eq!(bar.taint, None, "{bar:?}");
}

/// The report reads the process start from where the chain started (the
/// boot), never from the first frame: an 08:30 boot whose first frame came
/// at 09:05 is a pre-market start, not a restart after 09:00, so the report
/// carries no `process_started_after_0900` (LF-8 review round 5).
#[test]
fn test_day_report_reads_the_process_start_from_the_chain_start() {
    let mut d = driver(SCHEDULE_0916, 1_000_000);
    d.feed_frame(&frame(quote(B, 50.0, sod(9, 5, 0), 1), sod(9, 5, 0) * NS));
    let mut vol = 100;
    for f in stream_frames(sod(9, 5, 1), sod(9, 19, 0), &mut vol) {
        d.feed_frame(&f);
    }
    let o = d.finish().expect("finish");
    assert_eq!(o.stats.chain_start_ist_ns, Some(sod(8, 30, 0) * NS));
    let report = out::day_report(DAY_TEXT, &o, &[]);
    let json = serde_json::to_string(&report).expect("json");
    assert!(
        !json.contains("process_started_after_0900"),
        "an 08:30 boot is not a restart: {json}"
    );
    // A chain started after 09:00 (a given late boot) still reads as one.
    let mut d = driver_booted(
        SCHEDULE_0916,
        1_000_000,
        BootInstant::given(DAY * NS, 9 * 3_600 + 5 * 60),
    );
    let mut vol = 100;
    for f in stream_frames(sod(9, 5, 1), sod(9, 19, 0), &mut vol) {
        d.feed_frame(&f);
    }
    let o = d.finish().expect("finish");
    let json = serde_json::to_string(&out::day_report(DAY_TEXT, &o, &[])).expect("json");
    assert!(json.contains("process_started_after_0900"), "{json}");
}

// ---- records of another transport ----

/// A TrueData frame and an order-update record that reads as main feed (a
/// v1 to v3 record carries no endpoint) are kept by the main-feed endpoint
/// read, but the live replay and catch-up never fold them
/// (`frame_feeds_the_candle_fold`). Walked as Dhan binary, their text would
/// be abandoned and move the loss count, refusing the next candle as
/// `tick_dropped`. They are skipped and counted, and the run is the same as
/// one without them (LF-8 review, `ReplayDriver::feed_frame`).
#[test]
fn test_records_of_another_transport_are_skipped_not_walked_feed_frame() {
    let clean = run(SCHEDULE_0916, sod(9, 19, 0));
    let mut d = driver(SCHEDULE_0916, 1_000_000);
    let mut vol = 100;
    let mut frames = vec![frame(
        quote(B, 50.0, sod(9, 14, 55), 1),
        sod(9, 14, 55) * NS,
    )];
    frames.extend(stream_frames(sod(9, 15, 0), sod(9, 19, 0), &mut vol));
    let mut other = 0u64;
    for (i, f) in frames.into_iter().enumerate() {
        d.feed_frame(&f);
        if i % 40 == 7 {
            let text = br#"{"type":"tick","symbol":"X","ltp":1}"#.to_vec();
            let mut td = frame(text.clone(), f.received_at_nanos + IST_OFFSET_NS + 1_000);
            td.ws_type = WsType::TruedataFeed;
            d.feed_frame(&td);
            // A legacy order-update record: no receipt and no endpoint, so
            // it reads as main feed with an unknown receipt.
            let mut ou = frame(text, 0);
            ou.ws_type = WsType::OrderUpdate;
            ou.received_at_nanos = tickvault_storage::ws_frame_spill::WAL_RECEIPT_UNKNOWN_NANOS;
            d.feed_frame(&ou);
            other += 2;
        }
    }
    let o = d.finish().expect("finish");
    assert!(other > 0);
    assert_eq!(o.stats.other_transport_frames, other);
    assert_eq!(o.stats.drop_seq, 0, "no loss from a frame live never walks");
    assert_eq!(o.stats.abandoned_bytes, 0);
    assert_eq!(
        o.stats.no_receipt_frames, 0,
        "an order-update record is not a refused main-feed frame"
    );
    assert_eq!(o.decisions.len(), 1);
    assert_eq!(o.decisions[0].taint, None);
    assert_eq!(o.decisions, clean.decisions);
    assert_eq!(o.events, clean.events);
}

/// The same through a REAL segment and the reader: a TrueData record and an
/// order-update record written with a main-feed endpoint reach the driver
/// through `read_main_feed_segment`, and are skipped there.
#[test]
fn test_a_real_segment_with_other_transports_replays_clean_read_main_feed_segment() {
    use tickvault_storage::ws_frame_spill::WsFrameSpill;
    let dir = tmp_dir("transport");
    std::fs::create_dir_all(&dir).expect("dir");
    let tmp = dir.join("w");
    std::fs::create_dir_all(&tmp).expect("dir");
    let mut vol = 100;
    let mut frames = vec![frame(
        quote(B, 50.0, sod(9, 14, 55), 1),
        sod(9, 14, 55) * NS,
    )];
    frames.extend(stream_frames(sod(9, 15, 0), sod(9, 19, 0), &mut vol));
    let spill = WsFrameSpill::new(&tmp).expect("spill");
    let mut seq = 1u64;
    for (i, f) in frames.iter().enumerate() {
        let _ = spill.append_with_seq_at(
            WsType::LiveFeed,
            f.frame.clone(),
            seq,
            f.received_at_nanos,
            WalEndpoint::MainFeed,
        );
        seq += 1;
        if i % 40 == 7 {
            for ws in [WsType::TruedataFeed, WsType::OrderUpdate] {
                let _ = spill.append_with_seq_at(
                    ws,
                    br#"{"x":1}"#.to_vec(),
                    seq,
                    f.received_at_nanos + 1_000,
                    WalEndpoint::MainFeed,
                );
                seq += 1;
            }
        }
    }
    assert_eq!(spill.shutdown(std::time::Duration::from_secs(20)), 0);
    drop(spill);
    let seg = std::fs::read_dir(&tmp)
        .expect("list")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|e| e == "wal"))
        .expect("one segment");
    let (read, _) = super::source::read_main_feed_segment(&seg, None).expect("read");
    let kept_other = read
        .frames
        .iter()
        .filter(|f| f.ws_type != WsType::LiveFeed)
        .count();
    assert!(kept_other > 0, "the endpoint read keeps them");
    let mut d = driver(SCHEDULE_0916, 1_000_000);
    feed_segment(&mut d, &read);
    let o = d.finish().expect("finish");
    assert_eq!(o.stats.other_transport_frames, kept_other as u64);
    assert_eq!(o.stats.drop_seq, 0);
    assert_eq!(o.stats.abandoned_bytes, 0);
    assert_eq!(o.decisions.len(), 1);
    assert_eq!(o.decisions[0].taint, None);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- refusals on a folder that would replay ----

/// Each refusal below is run against a folder holding a good segment named
/// on the day, and the same run with only the checked condition removed
/// exits 0, so the refusal is the check under test and never the empty
/// folder's own (LF-8 review: the old test passed with each check deleted).
/// Covers the non-trading-day refusal and `--allow-non-trading-day`, a
/// non-empty `--out` (left untouched), and a missing `--config`.
#[test]
fn test_run_cli_refusals_fire_on_a_folder_that_would_replay() {
    let base = tmp_dir("cli4");
    std::fs::create_dir_all(&base).expect("dir");
    let sched = base.join("s.toml");
    std::fs::write(&sched, SCHEDULE_0916).expect("write");
    let uni = base.join("u.csv");
    std::fs::write(&uni, universe_text()).expect("write");
    let config =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/base.toml");
    // Wednesday 2026-11-18 and Saturday 2026-11-21, one segment each.
    let wed = base.join("wed");
    let sat = base.join("sat");
    let shift = |fs: Vec<ReplayedFrame>, days: i64| -> Vec<ReplayedFrame> {
        fs.into_iter()
            .map(|mut f| {
                f.received_at_nanos += days * 86_400 * NS;
                f
            })
            .collect()
    };
    let mut vol = 100;
    let day_frames = stream_frames(sod(9, 15, 0), sod(9, 17, 0), &mut vol);
    let sat_frames = shift(day_frames.clone(), 3);
    write_segments(&wed, &[day_frames]);
    write_segments(&sat, &[sat_frames]);
    let args = |wal: &std::path::Path,
                day: &str,
                cfg: &std::path::Path,
                extra: &[&str],
                out: &str|
     -> Vec<String> {
        let mut v: Vec<String> = [
            "tv-live-fill-replay",
            "--day",
            day,
            "--wal-dir",
            wal.to_str().expect("utf8"),
            "--schedule",
            sched.to_str().expect("utf8"),
            "--universe",
            uni.to_str().expect("utf8"),
            "--config",
            cfg.to_str().expect("utf8"),
            "--out",
            base.join(out).to_str().expect("utf8"),
            "--allow-partial",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        v.extend(extra.iter().map(|s| (*s).to_owned()));
        v
    };
    let summary = |out: &str| base.join(out).join("summary.txt").exists();
    // A Saturday is refused before anything is written ...
    assert_eq!(
        run_cli(&args(&sat, "2026-11-21", &config, &[], "s1")),
        EXIT_BAD_INPUT,
        "a Saturday"
    );
    assert!(!base.join("s1").exists(), "refused before --out is made");
    // ... and the same run with --allow-non-trading-day replays it.
    assert_eq!(
        run_cli(&args(
            &sat,
            "2026-11-21",
            &config,
            &["--allow-non-trading-day"],
            "s2"
        )),
        EXIT_OK,
        "--allow-non-trading-day goes past the calendar"
    );
    assert!(summary("s2"));
    // The weekday folder replays into a new folder ...
    assert_eq!(
        run_cli(&args(&wed, DAY_TEXT, &config, &[], "w1")),
        EXIT_OK,
        "the good folder replays"
    );
    assert!(summary("w1"));
    // ... is refused into a non-empty one, which is left as it was ...
    std::fs::create_dir_all(base.join("w2")).expect("dir");
    std::fs::write(base.join("w2/x"), b"mine").expect("write");
    assert_eq!(
        run_cli(&args(&wed, DAY_TEXT, &config, &[], "w2")),
        EXIT_BAD_INPUT,
        "out not empty"
    );
    assert_eq!(std::fs::read(base.join("w2/x")).expect("kept"), b"mine");
    assert!(
        !summary("w2"),
        "nothing written into a folder that was not new"
    );
    // ... and refused with a config that does not exist.
    assert_eq!(
        run_cli(&args(
            &wed,
            DAY_TEXT,
            std::path::Path::new("/no/such.toml"),
            &[],
            "w3"
        )),
        EXIT_BAD_INPUT,
        "a missing config"
    );
    assert!(!base.join("w3").exists());
    let _ = std::fs::remove_dir_all(&base);
}

/// The bucket path's go-ahead, judged on the listing before any download
/// (`bucket_go_ahead`): a second copy first (exit 3), then no segment named
/// on the day (exit 2), then the download cap (exit 2 over it without
/// `--yes`; exactly at it goes ahead; `--yes` waives only the cap).
#[test]
fn test_bucket_go_ahead_refuses_sidecars_an_empty_day_and_over_the_cap() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let listing = |bytes: u64, day_segments: usize, sidecars: &[&str]| DayListing {
        keys: vec![("raw-frames/2026-11-18/ws-frames-1.wal.gz".to_owned(), bytes)],
        total_bytes: bytes,
        day_segments,
        sidecars: sidecars.iter().map(|s| (*s).to_owned()).collect(),
    };
    let code = |r: Result<(), (i32, String)>| r.err().map(|(c, _)| c);
    let at_cap = listing(5 * GIB, 1, &[]);
    assert_eq!(code(bucket_go_ahead(&at_cap, "2026-11-18", 5, false)), None);
    let over = listing(5 * GIB + 1, 1, &[]);
    let refused = bucket_go_ahead(&over, "2026-11-18", 5, false);
    assert_eq!(code(refused.clone()), Some(EXIT_BAD_INPUT));
    assert!(
        refused
            .err()
            .is_some_and(|(_, m)| m.contains("--max-download-gib 5") && m.contains("--yes")),
    );
    assert_eq!(code(bucket_go_ahead(&over, "2026-11-18", 5, true)), None);
    assert_eq!(
        code(bucket_go_ahead(&listing(1, 1, &[]), "2026-11-18", 0, false)),
        Some(EXIT_BAD_INPUT),
        "a zero cap refuses any download without --yes"
    );
    assert_eq!(
        code(bucket_go_ahead(&listing(1, 0, &[]), "2026-11-18", 5, true)),
        Some(EXIT_BAD_INPUT),
        "no segment named on the day, --yes or not"
    );
    let side = listing(1, 1, &["raw-frames/2026-11-18/ws-frames-1.wal.sha-ab.gz"]);
    assert_eq!(
        code(bucket_go_ahead(&side, "2026-11-18", 5, true)),
        Some(EXIT_SELF_CHECK),
        "a second copy, --yes or not"
    );
    // The order: a second copy on a day with no segment and over the cap is
    // reported as the second copy; no segment over the cap as no segment.
    assert_eq!(
        code(bucket_go_ahead(
            &listing(100 * GIB, 0, &["x.sha-ab.gz"]),
            "2026-11-18",
            5,
            false
        )),
        Some(EXIT_SELF_CHECK)
    );
    let empty_over = bucket_go_ahead(&listing(100 * GIB, 0, &[]), "2026-11-18", 5, false);
    assert!(
        empty_over
            .err()
            .is_some_and(|(c, m)| c == EXIT_BAD_INPUT && m.contains("no segments under"))
    );
}

/// The inputs of the stated memory figure for the replay's 1 s bars (about
/// 80 B per kept second, about 3.5 to 4 GB for 2,000 liquid contracts over a
/// full day, Assumed; see the module's cost docs): an 8 B key and a 32 B
/// `SecondBar` per entry. If either grows, the figure in the docs and in the
/// CLAUDE.md O(1) table row must move with it (LF-8 review round 6: the row
/// said about 2 GB, about 1.8x low).
#[test]
fn test_second_bar_entry_size_behind_the_memory_figure() {
    use tickvault_trading::brutex_host::worst_fill::SecondBar;
    assert_eq!(std::mem::size_of::<i64>(), 8);
    assert_eq!(std::mem::size_of::<SecondBar>(), 32);
    // 2,000 contracts x 23,100 kept seconds (09:15 to 15:40) x 80 B.
    let bytes: u64 = 2_000 * 23_100 * 80;
    assert!((3_500_000_000..=4_000_000_000).contains(&bytes), "{bytes}");
}

#[test]
fn test_boot_sod_reads_hh_mm_ss_and_refuses_anything_else() {
    assert_eq!(boot_sod("00:00:00"), Ok(0));
    assert_eq!(boot_sod("09:14:40"), Ok(9 * 3_600 + 14 * 60 + 40));
    assert_eq!(boot_sod("23:59:59"), Ok(86_399));
    for bad in ["9:14", "24:00:00", "", "09:14:40x", "09-14-40"] {
        assert!(boot_sod(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn test_boot_instant_given_is_the_day_start_plus_the_second_and_not_assumed() {
    let b = BootInstant::given(DAY * NS, 9 * 3_600 + 14 * 60 + 40);
    assert_eq!(b.ist_ns, sod(9, 14, 40) * NS);
    assert!(!b.assumed);
    assert!(BootInstant::scheduled(DAY * NS).assumed);
    // Saturates, never wraps.
    assert_eq!(BootInstant::given(i64::MAX, 1).ist_ns, i64::MAX);
}

#[test]
fn test_is_local_sidecar_matches_only_hash_sidecars() {
    assert!(super::source::is_local_sidecar("ws-frames-7.wal.sha-ff"));
    assert!(super::source::is_local_sidecar("ws-frames-7.wal.sha-ff.gz"));
    assert!(!super::source::is_local_sidecar("ws-frames-7.wal.gz"));
    assert!(!super::source::is_local_sidecar("ws-frames-7.wal"));
}
