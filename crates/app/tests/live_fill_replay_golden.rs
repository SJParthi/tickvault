//! Golden day for the offline live-fill replay (plan item LF-8).
//!
//! The WAL segments are written at test time with the real WAL writer
//! (`WsFrameSpill::append_with_seq_at`, fixed receipts), so no binary fixture
//! is checked in. The day (Wed 18 Nov 2026, 09:14:55 to 09:21:00 IST) holds:
//! the 09:15 opening minute (opening wait 2.5 s), a tick-closed and a
//! clock-closed candle, a late tick that skips a candle, seconds with no
//! trade, a repeat quote, a backup duplicate (dropped through
//! `--backup-history`), a crossed book, stacked frames, a partial fill
//! cancelled at the end of its 60 s window, an exit cut to the position held,
//! and a damaged record in the second segment.
//!
//! Every output file is compared byte for byte with
//! `tests/fixtures/lf8_golden/expected_*`. `LF8_BLESS=1` rewrites them (read
//! the diff before committing a bless).

use std::path::{Path, PathBuf};
use std::time::Duration;

use tickvault_storage::ws_frame_spill::{WalEndpoint, WsFrameSpill, WsType};

const NS: i64 = 1_000_000_000;
/// 2026-11-18 00:00 IST, IST epoch seconds.
const DAY: i64 = 1_794_960_000;
const IST_OFFSET_SECS: i64 = 19_800;
const A: u32 = 2885;
const B: u32 = 1333;
const C: u32 = 49081;

fn sod(h: i64, m: i64, s: i64) -> i64 {
    DAY + h * 3_600 + m * 60 + s
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lf8_golden")
}

fn fresh_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tv-lf8-golden-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn quote(sid: u32, seg: u8, ltp: f32, ltt: i64, volume: u32) -> Vec<u8> {
    let mut b = vec![0u8; 50];
    b[0] = 4;
    b[1..3].copy_from_slice(&50u16.to_le_bytes());
    b[3] = seg;
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

#[allow(clippy::too_many_arguments)] // APPROVED: test packet builder, one argument per wire field
fn full(
    sid: u32,
    ltp: f32,
    ltt: i64,
    volume: u32,
    bid: f32,
    ask: f32,
    bq: u32,
    aq: u32,
) -> Vec<u8> {
    let mut b = vec![0u8; 162];
    b[0] = 8;
    b[1..3].copy_from_slice(&162u16.to_le_bytes());
    b[3] = 1;
    b[4..8].copy_from_slice(&sid.to_le_bytes());
    b[8..12].copy_from_slice(&ltp.to_le_bytes());
    b[12..14].copy_from_slice(&1u16.to_le_bytes());
    b[14..18].copy_from_slice(&u32::try_from(ltt).expect("fits").to_le_bytes());
    b[18..22].copy_from_slice(&ltp.to_le_bytes());
    b[22..26].copy_from_slice(&volume.to_le_bytes());
    for o in [46, 50, 54, 58] {
        b[o..o + 4].copy_from_slice(&ltp.to_le_bytes());
    }
    for level in 0..5usize {
        let o = 62 + level * 20;
        let step = 0.05 * level as f32;
        b[o..o + 4].copy_from_slice(&(bq + level as u32).to_le_bytes());
        b[o + 4..o + 8].copy_from_slice(&(aq + level as u32).to_le_bytes());
        b[o + 8..o + 10].copy_from_slice(&3u16.to_le_bytes());
        b[o + 10..o + 12].copy_from_slice(&3u16.to_le_bytes());
        b[o + 12..o + 16].copy_from_slice(&(bid - step).to_le_bytes());
        b[o + 16..o + 20].copy_from_slice(&(ask + step).to_le_bytes());
    }
    b
}

/// The day's frames: `(receipt IST ns, bytes)`, in write order.
fn day_frames() -> Vec<(i64, Vec<u8>)> {
    let mut out = Vec::new();
    // Pre-open: the first frame. The chain and the fold start at the boot
    // (`--boot-ist 08:30:00`), as live builds both at boot.
    out.push((
        sod(9, 14, 55) * NS + 2_000_000,
        full(B, 99.0, sod(9, 14, 55), 0, 98.95, 99.05, 500, 400),
    ));
    let (mut vb, mut va, mut vc) = (1_000u32, 2_000u32, 300u32);
    let mut last_a: Vec<u8> = Vec::new();
    let mut last_b: Vec<u8> = Vec::new();
    let mut t = sod(9, 15, 0) * NS;
    let mut n = 0i64;
    while t <= sod(9, 21, 0) * NS {
        let secs = t / NS;
        let on_second = t % NS == 0;
        let mut frame: Vec<u8> = Vec::new();
        // B: a Full packet every 500 ms, except one silent second (09:19:10).
        let silent = (sod(9, 19, 10) * NS..sod(9, 19, 11) * NS).contains(&t);
        if !silent {
            vb += 20;
            let ltp = 100.0 + (n % 9) as f32 * 0.05;
            let crossed = t == sod(9, 17, 30) * NS;
            let (bid, ask) = if crossed {
                (ltp + 0.10, ltp - 0.10)
            } else {
                (ltp - 0.05, ltp + 0.05)
            };
            let p = full(B, ltp, secs, vb, bid, ask, 500, 400);
            frame.extend(&p);
            last_b = p;
        }
        // A: a quote every second.
        if on_second {
            va += 30;
            let ltp = 2_500.0 + (n % 13) as f32 * 0.05;
            let p = quote(A, 1, ltp, secs, va);
            frame.extend(&p);
            last_a = p;
        }
        // C: a thin option, a trade every 2 s.
        // Silent from 09:19:00 to 09:19:05, so its 09:18 candle is closed by
        // the clock (B keeps the stream busy: no feed gap).
        let c_quiet = (sod(9, 19, 0)..sod(9, 19, 6)).contains(&secs);
        if on_second && (secs - DAY) % 2 == 0 && !c_quiet {
            vc += 8;
            frame.extend(quote(C, 2, 120.0 + (n % 5) as f32 * 0.05, secs, vc));
        }
        // A repeat quote (A's packet again, unchanged) at 09:16:10.5.
        if t == sod(9, 16, 10) * NS + NS / 2 {
            frame.extend(&last_a);
        }
        // The backup copy of B's packet (byte-identical) at 09:18:20.5.
        if t == sod(9, 18, 20) * NS + NS / 2 {
            frame.extend(&last_b.clone());
        }
        out.push((t + 2_000_000, frame));
        // A late tick on B's 09:16:30 candle, after a tick closed it.
        if t == sod(9, 16, 35) * NS {
            vb += 5;
            out.push((
                t + 300_000_000,
                full(B, 103.0, sod(9, 16, 34), vb, 102.95, 103.05, 500, 400),
            ));
        }
        t += NS / 2;
        n += 1;
    }
    out
}

/// Writes the day as two WAL segments (split at 09:20:30) and damages one
/// record in the middle of the second. Returns the folder.
fn build_wal(dir: &Path) {
    std::fs::create_dir_all(dir).expect("wal dir");
    let frames = day_frames();
    let split = frames
        .iter()
        .position(|(r, _)| *r > sod(9, 20, 30) * NS)
        .expect("split");
    for (part, range) in [(0usize, 0..split), (1, split..frames.len())] {
        let spill = WsFrameSpill::new(dir).expect("spill");
        for (i, (receipt_ist, bytes)) in frames[range].iter().enumerate() {
            let seq = (part as u64 + 1) * 1_000_000 + i as u64;
            let _ = spill.append_with_seq_at(
                WsType::LiveFeed,
                bytes.clone(),
                seq,
                receipt_ist - IST_OFFSET_SECS * NS,
                WalEndpoint::MainFeed,
            );
        }
        assert_eq!(
            spill.shutdown(Duration::from_secs(20)),
            0,
            "every record written"
        );
        drop(spill);
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut segs: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("list")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "wal"))
        .collect();
    segs.sort();
    assert_eq!(segs.len(), 2, "two segments: {segs:?}");
    // Name each segment on the replayed day, as the live box names it (its
    // creation instant): the replay reads a local folder like the bucket, by
    // the IST date in the name.
    let named = [
        (sod(8, 59, 0) - IST_OFFSET_SECS) * NS,
        (sod(9, 20, 30) - IST_OFFSET_SECS) * NS,
    ];
    let segs: Vec<PathBuf> = segs
        .iter()
        .zip(named)
        .map(|(p, n)| {
            let to = dir.join(format!("ws-frames-{n}.wal"));
            std::fs::rename(p, &to).expect("name on the day");
            to
        })
        .collect();
    let second = &segs[1];
    let mut body = std::fs::read(second).expect("read");
    let mid = body.len() / 2;
    body[mid] ^= 0xFF;
    std::fs::write(second, body).expect("damage");
}

fn backup_history(dir: &Path) -> PathBuf {
    let p = dir.join("backup-history.json");
    let published_utc = (sod(8, 30, 0) - IST_OFFSET_SECS) * NS;
    std::fs::write(
        &p,
        format!(
            "{{\"publications\":[{{\"published_at_nanos\":{published_utc},\"contracts\":[[{B},1]]}}]}}"
        ),
    )
    .expect("history");
    p
}

fn args(wal: &Path, out: &Path, history: &Path) -> Vec<String> {
    let f = fixtures();
    let config = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/base.toml");
    [
        "tv-live-fill-replay",
        "--day",
        "2026-11-18",
        "--wal-dir",
        wal.to_str().expect("utf8"),
        "--schedule",
        f.join("schedule.toml").to_str().expect("utf8"),
        "--universe",
        f.join("universe.csv").to_str().expect("utf8"),
        "--out",
        out.to_str().expect("utf8"),
        "--config",
        config.to_str().expect("utf8"),
        "--backup-history",
        history.to_str().expect("utf8"),
        "--open-wait-ms",
        "2500",
        // The scheduled pre-market boot: the first frame (09:14:55) is after
        // 09:10, so without it the boot is unknown and the day incomplete.
        "--boot-ist",
        "08:30:00",
        "--allow-partial",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

/// The output files the golden compares, and their fixture names.
const COMPARED: [(&str, &str); 4] = [
    ("decisions.csv", "expected_decisions.csv"),
    ("events.csv", "expected_events.csv"),
    ("report-2026-11-18.json", "expected_report.json"),
    ("summary.txt", "expected_summary.txt"),
];

fn replay_once(tag: &str) -> PathBuf {
    let base = fresh_dir(tag);
    let wal = base.join("wal");
    build_wal(&wal);
    let history = backup_history(&base);
    let out = base.join("out");
    let code = tickvault_app::live_fill::replay::run_cli(&args(&wal, &out, &history));
    assert_eq!(code, 0, "the golden day replays cleanly");
    out
}

#[test]
fn lf8_golden_day_matches() {
    let out = replay_once("match");
    let bless = std::env::var("LF8_BLESS").is_ok_and(|v| v == "1");
    for (got, want) in COMPARED {
        let got_bytes = std::fs::read(out.join(got)).unwrap_or_else(|e| panic!("{got}: {e}"));
        let want_path = fixtures().join(want);
        if bless {
            std::fs::write(&want_path, &got_bytes).expect("bless");
            continue;
        }
        let want_bytes = std::fs::read(&want_path)
            .unwrap_or_else(|e| panic!("{}: {e} (run with LF8_BLESS=1 once)", want_path.display()));
        assert!(
            got_bytes == want_bytes,
            "{got} differs from {want}:\n--- got ---\n{}\n--- want ---\n{}",
            String::from_utf8_lossy(&got_bytes),
            String::from_utf8_lossy(&want_bytes)
        );
    }
    // What the day is meant to show, read from the outputs themselves.
    let decisions = std::fs::read_to_string(out.join("decisions.csv")).expect("decisions");
    assert!(
        decisions.contains(",true,false,"),
        "an opening-clamp decision"
    );
    assert!(decisions.contains(",false,true,"), "a late-price skip");
    let events = std::fs::read_to_string(out.join("events.csv")).expect("events");
    assert!(
        events.contains("remainder_after_window"),
        "a partial fill cancelled"
    );
    assert!(events.contains(",clock,"), "a clock-closed candle");
    assert!(events.contains(",tick,"), "a tick-closed candle");
    let summary = std::fs::read_to_string(out.join("summary.txt")).expect("summary");
    assert!(summary.contains("damaged segments: 1"), "{summary}");
    assert!(summary.contains("backup copies dropped: 1"), "{summary}");
    assert!(summary.contains("Nothing real was sent"), "{summary}");
    let _ = std::fs::remove_dir_all(out.parent().expect("base"));
}

#[test]
fn lf8_golden_is_deterministic() {
    let a = replay_once("det-a");
    let b = replay_once("det-b");
    let mut names: Vec<String> = std::fs::read_dir(&a)
        .expect("list")
        .filter_map(|e| e.ok().and_then(|e| e.file_name().into_string().ok()))
        .collect();
    names.sort();
    assert!(names.len() >= 8, "{names:?}");
    for n in &names {
        assert_eq!(
            std::fs::read(a.join(n)).expect("a"),
            std::fs::read(b.join(n)).expect("b"),
            "{n} differs between two runs of the same input"
        );
    }
    let _ = std::fs::remove_dir_all(a.parent().expect("base"));
    let _ = std::fs::remove_dir_all(b.parent().expect("base"));
}

#[test]
fn lf8_bin_runs_and_exits_zero() {
    let base = fresh_dir("bin");
    let wal = base.join("wal");
    build_wal(&wal);
    let history = backup_history(&base);
    let out = base.join("out");
    let a = args(&wal, &out, &history);
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_tv-live-fill-replay"))
        .args(&a[1..])
        .stdout(std::process::Stdio::null())
        .status()
        .expect("spawn");
    assert_eq!(status.code(), Some(0));
    assert!(out.join("summary.txt").is_file());
    let _ = std::fs::remove_dir_all(&base);
}
