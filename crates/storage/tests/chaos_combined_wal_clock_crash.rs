//! Chaos, COMBINED faults on the WAL: a clock that runs backwards across a
//! segment rotation, a crash between the two watermark files, torn files, and
//! a replay that meets two kinds of damage at once.
//!
//! Every test here drives REAL public APIs (`WsFrameSpill`,
//! `request_idle_seal`, `replay_all_with_report`, `AppliedWatermark`,
//! `AppliedSnapshot::load`) over real files in a private temp directory. No
//! QuestDB, no network, no Docker. Bounded waits only.
//!
//! # S2: clock stepped back across WAL rotations (`s2_*`)
//!
//! The wall clock is not injectable into the writer, so the backward step is
//! built from what it leaves on disk: an earlier process whose clock ran ONE
//! HOUR ahead wrote a live segment named (and sequenced) an hour in the future,
//! and its archive holds a segment named TWO hours ahead. This process starts
//! with a correct clock, i.e. a clock that stepped back by one to two hours.
//! It then appends, seals the open segment twice through `request_idle_seal`
//! (a real rotation each time), appends more, and shuts down. Asserted:
//!
//! * every new segment is named strictly above the archive's newest name and
//!   the names rise by exactly one each time (`max(clock, newest + 1)`);
//! * every rotation counted `tv_wal_segment_name_clamped_total`;
//! * every new frame's sequence is above every frame the earlier process
//!   wrote (the sequence is ratcheted past the disk);
//! * the replay returns the old frames then the new frames in capture order,
//!   strictly rising sequences, every frame exactly once, payloads intact.
//!
//! # S3: crash between `unapplied_ranges.tvur` and `applied.tvaw` (`s3_*`)
//!
//! A range change writes the sidecar under a new generation, then
//! `applied.tvaw` naming it. The crash is simulated by putting back the
//! `applied.tvaw` bytes from before the change (or the sidecar bytes from
//! before it, the reverse order), then loading and running the real replay over
//! five real segments whose frames are all below the acked watermark except
//! the ones inside the unapplied ranges. A control run on the same layout with
//! no ranges skips four of five segments, which is what gives the other runs
//! teeth. Asserted for every permutation: every frame inside an unapplied range
//! is returned by the replay (never read as applied), no frame twice. Torn
//! (truncated) sidecar, torn `applied.tvaw`, and a torn `applied.tvaw.tmp`
//! beside a good file are covered too.
//!
//! # Out of the box: two kinds of damage in ONE replay (`oob_*`)
//!
//! Three segments in one directory: the oldest has a CRC flip in the middle,
//! the middle one a length field pointing past EOF with records after it, the
//! newest a torn tail (an interrupted write). The replay must return the exact
//! surviving records of each, flag the gaps exactly where frames are missing,
//! count two resyncs and two damaged segments with the exact abandoned bytes,
//! and leave the torn tail uncounted (it is what every unclean stop leaves).
//!
//! Plus: a torn tail in an OLDER segment (pinned: not counted as damage, no
//! `after_gap`; left to the consumer's process-boundary receipt rule, which
//! cannot fire when the receipts are unknown), damage in a segment's first record, and 0-byte and
//! 12-byte segments between damaged ones; and S2b, the backward clock against
//! frames that exist only in `archive/` and `replaying/`.
//!
//! Tests share one process-wide metrics recorder and run one at a time
//! (`SERIAL`) so counter deltas belong to the test reading them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use metrics::{
    Counter, CounterFn, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit,
};
use tickvault_storage::wal_applied_watermark::{
    APPLIED_INVALID_COUNTER, APPLIED_WATERMARK_FILE, APPLIED_WATERMARK_TMP, AppliedSnapshot,
    AppliedWatermark, UNAPPLIED_BUCKET_SHIFT, UNAPPLIED_BUCKETS, WATERMARK_DURABILITY_LAG_SECS,
};
use tickvault_storage::wal_idle_seal::request_idle_seal;
use tickvault_storage::wal_unapplied_ranges::UNAPPLIED_RANGES_FILE;
use tickvault_storage::ws_frame_spill::{
    AppendOutcome, ReplayedFrame, WAL_REPLAY_ABANDONED_BYTES_COUNTER, WAL_REPLAY_MAX_BYTES,
    WAL_REPLAY_RESYNCS_COUNTER, WAL_REPLAY_TRUNCATED_SEGMENTS_COUNTER,
    WAL_SEGMENT_NAME_CLAMPED_COUNTER, WalEndpoint, WalReplayBatch, WsFrameSpill, WsType,
    replay_all_with_report, segment_name_nanos,
};

// ---------------------------------------------------------------------------
// Shared harness: one recorder, one test at a time.
// ---------------------------------------------------------------------------

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Default)]
struct AtomicCounter(AtomicU64);

impl CounterFn for AtomicCounter {
    fn increment(&self, value: u64) {
        self.0.fetch_add(value, Ordering::Relaxed);
    }
    fn absolute(&self, value: u64) {
        self.0.fetch_max(value, Ordering::Relaxed);
    }
}

/// Counts every counter by NAME (labels summed). Gauges and histograms are
/// dropped: nothing here reads them.
#[derive(Default)]
struct CountingRecorder {
    counters: Mutex<HashMap<String, Arc<AtomicCounter>>>,
}

impl CountingRecorder {
    fn value(&self, name: &str) -> u64 {
        self.counters
            .lock()
            .unwrap()
            .get(name)
            .map_or(0, |c| c.0.load(Ordering::Relaxed))
    }
}

impl Recorder for CountingRecorder {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        let arc = Arc::clone(
            self.counters
                .lock()
                .unwrap()
                .entry(key.name().to_string())
                .or_default(),
        );
        Counter::from_arc(arc)
    }
    fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::noop()
    }
    fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::noop()
    }
}

static RECORDER: OnceLock<&'static CountingRecorder> = OnceLock::new();

fn recorder() -> &'static CountingRecorder {
    RECORDER.get_or_init(|| {
        let rec: &'static CountingRecorder = Box::leak(Box::default());
        metrics::set_global_recorder(rec).expect("first and only recorder in this test binary");
        rec
    })
}

fn counter(name: &str) -> u64 {
    recorder().value(name)
}

// ---------------------------------------------------------------------------
// File helpers
// ---------------------------------------------------------------------------

fn wall_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn chaos_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "tv-chaos-wcc-{tag}-{}-{}",
        std::process::id(),
        wall_nanos()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create chaos dir");
    p
}

/// Removes the directory on drop, even when an assertion fails.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn crc32(chunks: &[&[u8]]) -> u32 {
    let mut c: u32 = 0xFFFF_FFFF;
    for chunk in chunks {
        for &b in *chunk {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
    }
    c ^ 0xFFFF_FFFF
}

const PAYLOAD_LEN: usize = 16;
/// TVW4 record = magic 4 + type 1 + seq 8 + receipt 8 + endpoint 1 + len 4 +
/// frame + crc 4.
const RECORD_LEN: usize = 30 + PAYLOAD_LEN;
/// Offset of the frame-length field inside a TVW4 record.
const LEN_FIELD_AT: usize = 22;
/// Offset of the frame inside a TVW4 record.
const FRAME_AT: usize = 26;

/// The payload carries its own sequence, so a replayed frame can be checked
/// against the record it came from.
fn payload_for(seq: u64) -> Vec<u8> {
    let mut p = vec![0x5Au8; PAYLOAD_LEN];
    p[..8].copy_from_slice(&seq.to_le_bytes());
    p
}

/// One TVW4 record, byte for byte what the writer produces, with a receipt
/// the WAL does not know (`7`, below the plausible band).
fn record_bytes(seq: u64) -> Vec<u8> {
    record_bytes_at(seq, 7)
}

/// One TVW4 record with receipt `received_at_nanos`.
fn record_bytes_at(seq: u64, received_at_nanos: i64) -> Vec<u8> {
    let ws = WsType::LiveFeed.as_u8();
    let ep = WalEndpoint::MainFeed.as_u8();
    let frame = payload_for(seq);
    let seq_b = seq.to_le_bytes();
    let recv = received_at_nanos.to_le_bytes();
    let len = (PAYLOAD_LEN as u32).to_le_bytes();
    let crc = crc32(&[&[ws], &seq_b[..], &recv[..], &[ep], &len[..], &frame[..]]);
    let mut v = Vec::with_capacity(RECORD_LEN);
    v.extend_from_slice(b"TVW4");
    v.push(ws);
    v.extend_from_slice(&seq_b);
    v.extend_from_slice(&recv);
    v.push(ep);
    v.extend_from_slice(&len);
    v.extend_from_slice(&frame);
    v.extend_from_slice(&crc.to_le_bytes());
    assert_eq!(v.len(), RECORD_LEN);
    v
}

fn segment_path(dir: &Path, name: u128) -> PathBuf {
    dir.join(format!("ws-frames-{name:020}.wal"))
}

fn write_segment(dir: &Path, name: u128, seqs: &[u64]) -> PathBuf {
    let path = segment_path(dir, name);
    let mut f = std::fs::File::create(&path).expect("create segment");
    for &s in seqs {
        f.write_all(&record_bytes(s)).expect("write record");
    }
    f.sync_all().expect("sync segment");
    path
}

/// A segment whose records carry receipts `receipt(k)` for the k-th record.
fn write_segment_at(
    dir: &Path,
    name: u128,
    seqs: &[u64],
    receipt: impl Fn(usize) -> i64,
) -> PathBuf {
    let path = segment_path(dir, name);
    let mut f = std::fs::File::create(&path).expect("create segment");
    for (k, &s) in seqs.iter().enumerate() {
        f.write_all(&record_bytes_at(s, receipt(k)))
            .expect("write record");
    }
    f.sync_all().expect("sync segment");
    path
}

/// Segment names (nanos) directly in `dir`, sorted.
fn live_segment_names(dir: &Path) -> Vec<u128> {
    let mut v: Vec<u128> = std::fs::read_dir(dir)
        .expect("list dir")
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(segment_name_nanos))
        .collect();
    v.sort_unstable();
    v
}

/// A frame base (low 17 bits zero) at `secs` past a fixed 2026 instant, the
/// shape every real `capture_seq` has.
fn seq(secs: u64) -> u64 {
    ((1_780_000_000 + secs) * 1_000_000_000) >> 17 << 17
}

fn frame_base(nanos: u64) -> u64 {
    (nanos >> 17) << 17
}

fn replay(dir: &Path) -> WalReplayBatch {
    replay_all_with_report(dir, WAL_REPLAY_MAX_BYTES).expect("replay must not fail")
}

fn assert_strictly_rising_and_unique(frames: &[ReplayedFrame], what: &str) {
    let mut seen = HashSet::new();
    for f in frames {
        assert!(
            seen.insert(f.frame_seq),
            "{what}: frame {} twice",
            f.frame_seq
        );
    }
    for w in frames.windows(2) {
        assert!(
            w[0].frame_seq < w[1].frame_seq,
            "{what}: replay order broke at {} -> {}",
            w[0].frame_seq,
            w[1].frame_seq
        );
    }
}

// ---------------------------------------------------------------------------
// S2: clock stepped back across real rotations
// ---------------------------------------------------------------------------

fn wait_persisted(spill: &WsFrameSpill, target: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while spill.persisted_count() < target {
        assert!(
            Instant::now() < deadline,
            "writer did not persist {target} frames in 10 s (has {})",
            spill.persisted_count()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Appends one frame whose payload names its sequence; records it.
fn append_one(spill: &WsFrameSpill, appended: &mut Vec<u64>) {
    let s = tickvault_storage::ws_frame_spill::next_frame_seq();
    assert_eq!(
        spill.append_with_seq(WsType::LiveFeed, payload_for(s), s),
        AppendOutcome::Spilled
    );
    appended.push(s);
    wait_persisted(spill, appended.len() as u64);
}

/// Seals the writer's open segment (a real rotation) and appends frames until
/// one of them opens a new segment, so `dir` holds `want` live segments.
/// The seal happens only in a writer lull, so a frame appended before the
/// lull lands in the old segment; that frame is still recorded and checked.
fn rotate_until(spill: &WsFrameSpill, dir: &Path, want: usize, appended: &mut Vec<u64>) {
    for _ in 0..20 {
        request_idle_seal(dir);
        std::thread::sleep(Duration::from_millis(450));
        append_one(spill, appended);
        if live_segment_names(dir).len() >= want {
            return;
        }
    }
    panic!(
        "no rotation to {want} segments after 20 seal requests (have {})",
        live_segment_names(dir).len()
    );
}

/// S2 + combined: the earlier process ran ahead (live segment an hour ahead,
/// archive two hours ahead), this process's clock is behind both, and the
/// writer rotates twice under that clock.
#[test]
fn s2_backward_clock_across_two_rotations_keeps_names_sequences_and_replay_ordered() {
    let _g = serial();
    let rec_before = counter(WAL_SEGMENT_NAME_CLAMPED_COUNTER);
    let dir = chaos_dir("s2-backward-clock");
    let _cleanup = Cleanup(dir.clone());

    let now = wall_nanos();
    let ahead = now + 3_600 * 1_000_000_000;
    let archive_name = now + 7_200 * 1_000_000_000;
    let ahead_u64 = u64::try_from(ahead).unwrap();
    // The earlier process minted its sequences from its (fast) clock.
    let old_seqs: Vec<u64> = (1..=5u64)
        .map(|k| frame_base(ahead_u64) + (k << 17))
        .collect();
    write_segment(&dir, ahead, &old_seqs);
    let archive = dir.join("archive");
    std::fs::create_dir_all(&archive).unwrap();
    write_segment(&archive, archive_name, &[]);

    let spill = WsFrameSpill::new(&dir).expect("spill");
    let mut appended = Vec::new();
    for _ in 0..3 {
        append_one(&spill, &mut appended);
    }
    // ahead + the writer's first segment.
    assert_eq!(live_segment_names(&dir).len(), 2);
    rotate_until(&spill, &dir, 3, &mut appended);
    for _ in 0..2 {
        append_one(&spill, &mut appended);
    }
    rotate_until(&spill, &dir, 4, &mut appended);
    append_one(&spill, &mut appended);
    assert_eq!(spill.shutdown(Duration::from_secs(10)), 0, "clean drain");
    assert_eq!(spill.drop_critical_count(), 0, "no frame refused");
    drop(spill);

    // Names: the earlier process's live segment first, then three new ones,
    // each exactly one above the newest name already used, archive included.
    let names = live_segment_names(&dir);
    assert_eq!(names.len(), 4, "names: {names:?}");
    assert_eq!(names[0], ahead);
    assert_eq!(
        &names[1..],
        &[archive_name + 1, archive_name + 2, archive_name + 3],
        "every new name is clamped past the newest name on disk and rises by one"
    );
    assert!(
        names[1..].iter().all(|&n| n > wall_nanos()),
        "the clock is behind every new name, so every name was clamped"
    );
    let clamped = counter(WAL_SEGMENT_NAME_CLAMPED_COUNTER) - rec_before;
    assert_eq!(clamped, 3, "each of the three clamped names is counted");

    // Sequences: ratcheted past the earlier process's disk.
    let old_high = *old_seqs.last().unwrap();
    assert!(
        appended.iter().all(|&s| s > old_high),
        "every new frame is sequenced above the earlier process's frames"
    );
    assert!(appended.windows(2).all(|w| w[0] < w[1]));

    // Replay: old frames, then new frames, each once, in capture order.
    let batch = replay(&dir);
    assert_eq!(
        batch.skipped_segments, 0,
        "nothing here was ever acked, nothing may be skipped"
    );
    let got: Vec<u64> = batch.frames.iter().map(|f| f.frame_seq).collect();
    let mut want = old_seqs.clone();
    want.extend_from_slice(&appended);
    assert_eq!(got, want, "exact replay order and content");
    for f in &batch.frames {
        assert_eq!(f.frame, payload_for(f.frame_seq), "payload intact");
    }
    assert_strictly_rising_and_unique(&batch.frames, "S2 replay");
    assert_eq!(batch.segment_ends.len(), 4, "every segment was read");
}

/// S2b: the ratchet and the name clamp read EVERY directory, not only the
/// live one. The earlier process's live segment is an hour ahead; its
/// archive holds a segment named and sequenced two hours ahead WITH frames;
/// `replaying/` holds one three hours ahead, above everything else. A
/// regression that scanned only the live directory would re-issue
/// `capture_seq` values below the archived and replaying frames, and the
/// DEDUP key would silently drop those rows on replay.
#[test]
fn s2b_backward_clock_ratchets_past_archive_and_replaying_frames_and_names() {
    let _g = serial();
    let rec_before = counter(WAL_SEGMENT_NAME_CLAMPED_COUNTER);
    let dir = chaos_dir("s2b-backward-clock");
    let _cleanup = Cleanup(dir.clone());

    const HOUR: u128 = 3_600 * 1_000_000_000;
    let now = wall_nanos();
    let (live_name, archive_name, replaying_name) = (now + HOUR, now + 2 * HOUR, now + 3 * HOUR);
    let seqs_at = |name: u128| -> Vec<u64> {
        let base = frame_base(u64::try_from(name).unwrap());
        (1..=4u64).map(|k| base + (k << 17)).collect()
    };
    let live_seqs = seqs_at(live_name);
    let archive_seqs = seqs_at(archive_name);
    let replaying_seqs = seqs_at(replaying_name);
    write_segment(&dir, live_name, &live_seqs);
    let archive = dir.join("archive");
    let replaying = dir.join("replaying");
    std::fs::create_dir_all(&archive).unwrap();
    std::fs::create_dir_all(&replaying).unwrap();
    write_segment(&archive, archive_name, &archive_seqs);
    write_segment(&replaying, replaying_name, &replaying_seqs);
    let disk_high = *replaying_seqs.last().unwrap();
    assert_eq!(
        tickvault_storage::ws_frame_spill::highest_frame_seq_on_disk(&dir),
        disk_high,
        "the disk high-water mark is the replaying/ segment's newest frame"
    );

    let spill = WsFrameSpill::new(&dir).expect("spill");
    let mut appended = Vec::new();
    for _ in 0..3 {
        append_one(&spill, &mut appended);
    }
    rotate_until(&spill, &dir, 3, &mut appended);
    append_one(&spill, &mut appended);
    assert_eq!(spill.shutdown(Duration::from_secs(10)), 0, "clean drain");
    drop(spill);

    assert!(
        appended.iter().all(|&s| s > disk_high),
        "every new frame is sequenced above the replaying/ and archived frames"
    );
    assert!(appended.windows(2).all(|w| w[0] < w[1]));
    let names = live_segment_names(&dir);
    assert_eq!(names.len(), 3, "names: {names:?}");
    assert_eq!(names[0], live_name);
    assert_eq!(
        &names[1..],
        &[replaying_name + 1, replaying_name + 2],
        "every new name is clamped past the replaying/ name and rises by one"
    );
    assert_eq!(
        counter(WAL_SEGMENT_NAME_CLAMPED_COUNTER) - rec_before,
        2,
        "both clamped names are counted"
    );

    // Replay order across the directories: what waits (live and replaying/)
    // in name order, then the new frames; the archived segment was already
    // replayed by the earlier process and is not read again.
    let batch = replay(&dir);
    let got: Vec<u64> = batch.frames.iter().map(|f| f.frame_seq).collect();
    let mut want = live_seqs.clone();
    want.extend_from_slice(&replaying_seqs);
    want.extend_from_slice(&appended);
    assert_eq!(got, want, "exact replay order across live and replaying/");
    assert!(
        archive_seqs.iter().all(|s| !got.contains(s)),
        "an archived segment is not replayed again"
    );
    assert_strictly_rising_and_unique(&batch.frames, "S2b replay");
}

// ---------------------------------------------------------------------------
// S3: crash between the sidecar and applied.tvaw
// ---------------------------------------------------------------------------

const LAG_NANOS: u64 = WATERMARK_DURABILITY_LAG_SECS * 1_000_000_000;
/// Every frame in the layout is acked up to here.
fn acked_high() -> u64 {
    seq(10_000)
}
/// Segment k (1..=5) holds frames at k*1000 + {0,100,200,300,400} seconds.
fn layout_seqs(k: u64) -> Vec<u64> {
    (0..5).map(|j| seq(k * 1_000 + j * 100)).collect()
}
/// R1 covers one frame of segment 1, R2 one frame of segment 2.
fn r1() -> (u64, u64) {
    (seq(1_200), seq(1_200))
}
fn r2() -> (u64, u64) {
    (seq(2_300), seq(2_300))
}
/// R2 widened past what the 128-slot bucket table can hold: on disk it
/// needs the overflow bit, explained only by a trusted sidecar.
fn r2_wide() -> (u64, u64) {
    (
        seq(2_300),
        seq(2_300) + ((UNAPPLIED_BUCKETS as u64 + 4) << UNAPPLIED_BUCKET_SHIFT),
    )
}

/// Five real segments; the newest is always read by the replay.
fn build_layout(dir: &Path) {
    for k in 1..=5u64 {
        write_segment(dir, u128::from(seq(k * 1_000)), &layout_seqs(k));
    }
}

fn bound_acked_watermark(dir: &Path) -> AppliedWatermark {
    let wm = AppliedWatermark::new();
    wm.bind(dir);
    wm.note_ticks_acked(acked_high());
    wm.note_depth_acked(acked_high());
    wm
}

/// Persist under the real 60 s durability lag so the acks reach the file.
fn persist_durably(wm: &AppliedWatermark, t: u64) {
    wm.persist_at(t);
    wm.persist_at(t + LAG_NANOS + 1);
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("read")
}

/// The replay must return every frame inside `unapplied`, exactly once.
fn assert_unapplied_frames_replayed(batch: &WalReplayBatch, unapplied: &[(u64, u64)], what: &str) {
    assert_strictly_rising_and_unique(&batch.frames, what);
    let got: HashSet<u64> = batch.frames.iter().map(|f| f.frame_seq).collect();
    for k in 1..=5u64 {
        for s in layout_seqs(k) {
            if unapplied.iter().any(|&(lo, hi)| (lo..=hi).contains(&s)) {
                assert!(
                    got.contains(&s),
                    "{what}: frame {s} is unapplied and was NOT replayed (read as applied: LOSS)"
                );
            }
        }
    }
    for f in &batch.frames {
        assert_eq!(f.frame, payload_for(f.frame_seq), "{what}: payload intact");
    }
}

/// Control: the same layout, everything acked, no ranges. Four of five
/// segments are skipped unread and nothing is returned. This is what makes
/// "returned" in the other tests a real outcome and not a no-op skip rule.
#[test]
fn s3_control_without_ranges_skips_every_acked_segment() {
    let _g = serial();
    let dir = chaos_dir("s3-control");
    let _cleanup = Cleanup(dir.clone());
    build_layout(&dir);
    let wm = bound_acked_watermark(&dir);
    persist_durably(&wm, 1);
    let snap = AppliedSnapshot::load(&dir).expect("a clean file loads");
    assert_eq!(snap.hwm_ticks, acked_high());
    let batch = replay(&dir);
    assert_eq!(
        batch.skipped_segments, 4,
        "every closed acked segment skipped"
    );
    assert!(
        batch.frames.is_empty(),
        "every frame of the newest is applied"
    );
}

/// Crash after the sidecar grew (R1 -> R1+R2) and before `applied.tvaw`
/// named the new generation: the file on disk still names R1's generation.
#[test]
fn s3_crash_after_sidecar_grew_before_tvaw_still_replays_both_ranges() {
    let _g = serial();
    let dir = chaos_dir("s3-grow");
    let _cleanup = Cleanup(dir.clone());
    build_layout(&dir);
    let wm = bound_acked_watermark(&dir);
    assert!(wm.add_unapplied_range(r1().0, r1().1));
    persist_durably(&wm, 1);
    let tvaw_g1 = read(&dir.join(APPLIED_WATERMARK_FILE));
    let g1 = AppliedSnapshot::load(&dir).expect("g1").ranges_gen;
    assert_ne!(g1, 0, "a trusted sidecar generation is named");

    assert!(wm.add_unapplied_range(r2().0, r2().1), "sidecar G2 written");
    // CRASH: applied.tvaw was never rewritten.
    std::fs::write(dir.join(APPLIED_WATERMARK_FILE), &tvaw_g1).unwrap();

    let snap = AppliedSnapshot::load(&dir).expect("file still loads");
    for (lo, hi) in [r1(), r2()] {
        assert!(
            snap.range_has_unapplied(lo, hi),
            "range [{lo},{hi}] must read unapplied after the crash"
        );
    }
    let batch = replay(&dir);
    assert!(
        batch.skipped_segments <= 2,
        "segments 1 and 2 hold unapplied frames and are never skipped (skipped {})",
        batch.skipped_segments
    );
    assert_unapplied_frames_replayed(&batch, &[r1(), r2()], "grow crash");
}

/// Crash after the sidecar SHRANK (a trim dropped R2) and before
/// `applied.tvaw` named the new generation. The new sidecar no longer holds
/// R2; R2 must still be protected (through the old file's buckets).
#[test]
fn s3_crash_after_sidecar_shrank_before_tvaw_keeps_the_dropped_range_protected() {
    let _g = serial();
    let dir = chaos_dir("s3-shrink");
    let _cleanup = Cleanup(dir.clone());
    build_layout(&dir);
    let wm = bound_acked_watermark(&dir);
    assert!(wm.add_unapplied_range(r1().0, r1().1));
    assert!(wm.add_unapplied_range(r2().0, r2().1));
    persist_durably(&wm, 1);
    let tvaw_g2 = read(&dir.join(APPLIED_WATERMARK_FILE));

    // A trim keeps only what lies over segment 1.
    assert!(wm.clip_unapplied_ranges_to(&[(seq(1_000), seq(2_000) - 1)]));
    std::fs::write(dir.join(APPLIED_WATERMARK_FILE), &tvaw_g2).unwrap();

    let snap = AppliedSnapshot::load(&dir).expect("loads");
    assert!(
        snap.range_has_unapplied(r2().0, r2().1),
        "R2 still unapplied"
    );
    let batch = replay(&dir);
    assert_unapplied_frames_replayed(&batch, &[r1(), r2()], "shrink crash");
}

/// The same shrink crash with R2 too wide for the bucket table: the old file
/// carries the overflow bit, so the untrusted load must be an UNBOUNDED
/// overflow and the replay reads every frame of every segment.
#[test]
fn s3_crash_after_wide_range_trimmed_loads_as_unbounded_overflow_and_replays_all() {
    let _g = serial();
    let dir = chaos_dir("s3-shrink-wide");
    let _cleanup = Cleanup(dir.clone());
    build_layout(&dir);
    let wm = bound_acked_watermark(&dir);
    assert!(wm.add_unapplied_range(r1().0, r1().1));
    assert!(wm.add_unapplied_range(r2_wide().0, r2_wide().1));
    persist_durably(&wm, 1);
    let trusted = AppliedSnapshot::load(&dir).expect("loads");
    assert!(!trusted.overflowed, "a matched sidecar bounds the bit");
    let tvaw_wide = read(&dir.join(APPLIED_WATERMARK_FILE));

    assert!(wm.clip_unapplied_ranges_to(&[(seq(1_000), seq(2_000) - 1)]));
    std::fs::write(dir.join(APPLIED_WATERMARK_FILE), &tvaw_wide).unwrap();

    let snap = AppliedSnapshot::load(&dir).expect("loads");
    assert!(snap.overflowed, "untrusted sidecar: the bit is honoured");
    assert!(
        snap.range_has_unapplied(seq(4_000), seq(4_000)),
        "unbounded: even a range nobody marked reads unapplied"
    );
    let batch = replay(&dir);
    assert_eq!(batch.skipped_segments, 0, "nothing skipped");
    assert_eq!(
        batch.frames.len(),
        25,
        "every frame of every segment replayed"
    );
    assert_unapplied_frames_replayed(&batch, &[r1(), r2()], "wide shrink crash");
}

/// The reverse order (should not happen with the write order, but a lost
/// sidecar rename or a restore can do it): `applied.tvaw` names G2 while the
/// sidecar on disk is the older G1 without R2.
#[test]
fn s3_tvaw_newer_than_sidecar_is_untrusted_and_still_protects_the_newer_range() {
    let _g = serial();
    let dir = chaos_dir("s3-reverse");
    let _cleanup = Cleanup(dir.clone());
    build_layout(&dir);
    let wm = bound_acked_watermark(&dir);
    assert!(wm.add_unapplied_range(r1().0, r1().1));
    persist_durably(&wm, 1);
    let side_g1 = read(&dir.join(UNAPPLIED_RANGES_FILE));
    assert!(wm.add_unapplied_range(r2().0, r2().1));
    persist_durably(&wm, 10 * LAG_NANOS);
    std::fs::write(dir.join(UNAPPLIED_RANGES_FILE), &side_g1).unwrap();

    let snap = AppliedSnapshot::load(&dir).expect("loads");
    assert_eq!(
        snap.ranges_gen, 0,
        "a mismatched generation is never trusted"
    );
    assert!(snap.range_has_unapplied(r2().0, r2().1));
    let batch = replay(&dir);
    assert_unapplied_frames_replayed(&batch, &[r1(), r2()], "reverse order");
}

/// Torn sidecar (truncated mid-write by a host crash after the rename,
/// narrow and wide ranges): never read as applied.
#[test]
fn s3_torn_sidecar_falls_back_to_buckets_or_unbounded_overflow() {
    let _g = serial();
    for (tag, second) in [("narrow", r2()), ("wide", r2_wide())] {
        let dir = chaos_dir(&format!("s3-torn-side-{tag}"));
        let _cleanup = Cleanup(dir.clone());
        build_layout(&dir);
        let wm = bound_acked_watermark(&dir);
        assert!(wm.add_unapplied_range(r1().0, r1().1));
        assert!(wm.add_unapplied_range(second.0, second.1));
        persist_durably(&wm, 1);
        let side = dir.join(UNAPPLIED_RANGES_FILE);
        let bytes = read(&side);
        std::fs::write(&side, &bytes[..bytes.len() / 2]).unwrap();

        let snap = AppliedSnapshot::load(&dir).expect("applied.tvaw itself is intact");
        assert!(snap.range_has_unapplied(r1().0, r1().1), "{tag}: R1");
        assert!(snap.range_has_unapplied(r2().0, r2().1), "{tag}: R2");
        if tag == "wide" {
            assert!(snap.overflowed, "wide: the bit loads unbounded");
        }
        let batch = replay(&dir);
        assert_unapplied_frames_replayed(&batch, &[r1(), r2()], tag);
        if tag == "wide" {
            assert_eq!(batch.frames.len(), 25, "wide: everything replayed");
        }
    }
}

/// Torn `applied.tvaw` (truncated): rejected, counted, vouches for nothing,
/// and the replay reads every frame.
#[test]
fn s3_torn_applied_tvaw_is_rejected_counted_and_replays_everything() {
    let _g = serial();
    let dir = chaos_dir("s3-torn-tvaw");
    let _cleanup = Cleanup(dir.clone());
    build_layout(&dir);
    let wm = bound_acked_watermark(&dir);
    assert!(wm.add_unapplied_range(r1().0, r1().1));
    persist_durably(&wm, 1);
    let path = dir.join(APPLIED_WATERMARK_FILE);
    let bytes = read(&path);
    std::fs::write(&path, &bytes[..100]).unwrap();

    let invalid_before = counter(APPLIED_INVALID_COUNTER);
    assert!(AppliedSnapshot::load(&dir).is_none(), "torn file rejected");
    assert_eq!(AppliedSnapshot::persisted_high_water(&dir), 0);
    let batch = replay(&dir);
    assert!(
        counter(APPLIED_INVALID_COUNTER) - invalid_before >= 2,
        "the direct load and the replay's load each count the rejection"
    );
    assert_eq!(batch.skipped_segments, 0);
    assert_eq!(batch.frames.len(), 25, "a rejected file replays in full");
    assert_unapplied_frames_replayed(&batch, &[r1()], "torn tvaw");
}

/// A crash while writing `applied.tvaw.tmp` leaves a torn tmp beside the good
/// file: the good file is what loads, and the next persist replaces the tmp.
#[test]
fn s3_torn_tmp_beside_a_good_file_is_ignored_and_replaced_by_the_next_persist() {
    let _g = serial();
    let dir = chaos_dir("s3-torn-tmp");
    let _cleanup = Cleanup(dir.clone());
    build_layout(&dir);
    let wm = bound_acked_watermark(&dir);
    assert!(wm.add_unapplied_range(r1().0, r1().1));
    persist_durably(&wm, 1);
    let good = read(&dir.join(APPLIED_WATERMARK_FILE));
    let tmp = dir.join(APPLIED_WATERMARK_TMP);
    std::fs::write(&tmp, &good[..good.len() / 3]).unwrap();

    let snap = AppliedSnapshot::load(&dir).expect("the good file loads");
    assert_eq!(snap.hwm_ticks, acked_high());
    assert!(snap.range_has_unapplied(r1().0, r1().1));
    let batch = replay(&dir);
    assert_unapplied_frames_replayed(&batch, &[r1()], "torn tmp");

    // A new process binds and persists: the stale tmp is replaced, the file
    // still loads.
    let wm2 = AppliedWatermark::new();
    wm2.bind(&dir);
    persist_durably(&wm2, 1);
    assert!(!tmp.exists(), "the stale tmp is gone after a persist");
    assert!(AppliedSnapshot::load(&dir).is_some());
}

// ---------------------------------------------------------------------------
// Out of the box: mid-segment damage AND a torn tail in one replay
// ---------------------------------------------------------------------------

#[test]
fn oob_replay_with_mid_segment_damage_and_a_torn_newest_tail_returns_exact_survivors() {
    let _g = serial();
    let dir = chaos_dir("oob-two-damages");
    let _cleanup = Cleanup(dir.clone());

    let a_seqs: Vec<u64> = (101..=120).map(seq).collect();
    let b_seqs: Vec<u64> = (201..=210).map(seq).collect();
    let c_seqs: Vec<u64> = (301..=310).map(seq).collect();
    let a = write_segment(&dir, 1, &a_seqs);
    let b = write_segment(&dir, 2, &b_seqs);
    let c = write_segment(&dir, 3, &c_seqs);

    // A: CRC flip inside record 7 (seq 107).
    let mut bytes = read(&a);
    bytes[6 * RECORD_LEN + FRAME_AT + 9] ^= 0xFF;
    std::fs::write(&a, &bytes).unwrap();
    // B: record 3's length points past EOF, records after it intact.
    let mut bytes = read(&b);
    let at = 2 * RECORD_LEN + LEN_FIELD_AT;
    bytes[at..at + 4].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    std::fs::write(&b, &bytes).unwrap();
    // C (newest): an interrupted write, the last record cut to 20 bytes.
    let bytes = read(&c);
    std::fs::write(&c, &bytes[..bytes.len() - (RECORD_LEN - 20)]).unwrap();

    let resyncs = counter(WAL_REPLAY_RESYNCS_COUNTER);
    let truncated = counter(WAL_REPLAY_TRUNCATED_SEGMENTS_COUNTER);
    let abandoned = counter(WAL_REPLAY_ABANDONED_BYTES_COUNTER);

    let batch = replay(&dir);

    let want: Vec<u64> = a_seqs
        .iter()
        .copied()
        .filter(|&s| s != seq(107))
        .chain(b_seqs.iter().copied().filter(|&s| s != seq(203)))
        .chain(c_seqs[..9].iter().copied())
        .collect();
    let got: Vec<u64> = batch.frames.iter().map(|f| f.frame_seq).collect();
    assert_eq!(got.len(), 19 + 9 + 9, "exact survivor count");
    assert_eq!(got, want, "exact survivors, in order");
    for f in &batch.frames {
        assert_eq!(f.frame, payload_for(f.frame_seq), "no fabricated record");
    }
    assert_strictly_rising_and_unique(&batch.frames, "oob replay");

    // Gaps flagged exactly where frames are missing: the pass start, after
    // each skipped stretch, and at the first frame of the segment after each
    // damaged one. The torn tail leaves no trailing gap.
    let gaps: Vec<u64> = batch
        .frames
        .iter()
        .filter(|f| f.after_gap)
        .map(|f| f.frame_seq)
        .collect();
    assert_eq!(
        gaps,
        vec![seq(101), seq(108), seq(201), seq(204), seq(301)],
        "after_gap exactly at the missing-frame boundaries"
    );
    let firsts: Vec<u64> = batch
        .frames
        .iter()
        .filter(|f| f.first_in_segment)
        .map(|f| f.frame_seq)
        .collect();
    assert_eq!(firsts, vec![seq(101), seq(201), seq(301)]);
    assert!(!batch.trailing_gap, "a torn tail is not a gap");
    assert!(
        !batch.leading_gap,
        "nothing was skipped before the first frame"
    );
    assert_eq!(batch.segment_ends.len(), 3);

    assert_eq!(
        counter(WAL_REPLAY_RESYNCS_COUNTER) - resyncs,
        2,
        "two resyncs"
    );
    assert_eq!(
        counter(WAL_REPLAY_TRUNCATED_SEGMENTS_COUNTER) - truncated,
        2,
        "A and B counted damaged; the torn tail of C is not"
    );
    assert_eq!(
        counter(WAL_REPLAY_ABANDONED_BYTES_COUNTER) - abandoned,
        2 * RECORD_LEN as u64,
        "exactly one record's bytes abandoned in A and one in B"
    );
}

fn gaps_and_firsts(batch: &WalReplayBatch) -> (Vec<u64>, Vec<u64>) {
    let gaps = batch
        .frames
        .iter()
        .filter(|f| f.after_gap)
        .map(|f| f.frame_seq)
        .collect();
    let firsts = batch
        .frames
        .iter()
        .filter(|f| f.first_in_segment)
        .map(|f| f.frame_seq)
        .collect();
    (gaps, firsts)
}

/// A torn tail in an OLDER segment (not the newest): the writer can tear only
/// the last record a process wrote, so the next segment belongs to a new
/// process. Pinned behaviour: the torn record is dropped and is NOT counted
/// as damage (no resync, no damaged segment, no abandoned bytes), exactly as
/// for a torn newest tail, and the replay does NOT set `after_gap` on the next
/// segment's first frame. The missing frame is left to the CONSUMER's
/// process-boundary rule (`process_boundary_is_gap`, applied by the app's
/// replay fold to a `first_in_segment` frame): the receipts carried here make
/// it fire for a restart 60 s later. Honest limit, pinned in the second half:
/// with receipts the WAL does not know (legacy records) the rule cannot fire,
/// so the torn frame is missing with no flag anywhere.
#[test]
fn oob_torn_tail_in_a_non_newest_segment_is_a_process_boundary_gap() {
    let _g = serial();
    let t0 = i64::try_from(wall_nanos()).unwrap() - 3_600 * 1_000_000_000;
    let restart = t0 + 60_000_000_000;
    for (label, known) in [("known receipts", true), ("legacy receipts", false)] {
        let dir = chaos_dir("oob-torn-older");
        let _cleanup = Cleanup(dir.clone());
        let a_seqs: Vec<u64> = (101..=110).map(seq).collect();
        let b_seqs: Vec<u64> = (501..=510).map(seq).collect();
        let at = |base: i64| {
            move |k: usize| {
                if known {
                    base + k as i64 * 1_000_000
                } else {
                    7
                }
            }
        };
        let a = write_segment_at(&dir, 1, &a_seqs, at(t0));
        write_segment_at(&dir, 2, &b_seqs, at(restart));
        // A's last record was interrupted: 20 of its bytes reached the disk.
        let bytes = read(&a);
        std::fs::write(&a, &bytes[..bytes.len() - (RECORD_LEN - 20)]).unwrap();

        let resyncs = counter(WAL_REPLAY_RESYNCS_COUNTER);
        let truncated = counter(WAL_REPLAY_TRUNCATED_SEGMENTS_COUNTER);
        let abandoned = counter(WAL_REPLAY_ABANDONED_BYTES_COUNTER);
        let batch = replay(&dir);

        let want: Vec<u64> = a_seqs[..9].iter().chain(b_seqs.iter()).copied().collect();
        let got: Vec<u64> = batch.frames.iter().map(|f| f.frame_seq).collect();
        assert_eq!(
            got, want,
            "{label}: exact survivors, the torn record dropped"
        );
        for f in &batch.frames {
            assert_eq!(
                f.frame,
                payload_for(f.frame_seq),
                "{label}: no fabricated record"
            );
        }
        let (gaps, firsts) = gaps_and_firsts(&batch);
        assert_eq!(firsts, vec![seq(101), seq(501)], "{label}");
        assert_eq!(
            gaps,
            vec![seq(101)],
            "{label}: pinned, the replay itself flags no gap after a torn older tail"
        );
        let last_a = &batch.frames[8];
        let first_b = &batch.frames[9];
        assert!(first_b.first_in_segment);
        assert_eq!(
            tickvault_storage::ws_frame_spill::process_boundary_is_gap(
                last_a.received_at_nanos,
                first_b.received_at_nanos
            ),
            known,
            "{label}: the consumer's process-boundary rule fires only with known receipts"
        );
        assert!(!batch.trailing_gap, "{label}");
        assert_eq!(counter(WAL_REPLAY_RESYNCS_COUNTER) - resyncs, 0, "{label}");
        assert_eq!(
            counter(WAL_REPLAY_TRUNCATED_SEGMENTS_COUNTER) - truncated,
            0,
            "{label}: a torn tail is not counted as damage, older segment or newest"
        );
        assert_eq!(
            counter(WAL_REPLAY_ABANDONED_BYTES_COUNTER) - abandoned,
            0,
            "{label}"
        );
    }
}

/// Damage in a segment's FIRST record, and a 0-byte and a 12-byte segment
/// between intact ones. The first record is skipped by a resync (counted,
/// exactly one record's bytes abandoned), the segment's first REPLAYED frame
/// carries `first_in_segment` and `after_gap`, the tiny segments yield
/// nothing and break nothing, and the segment after the damaged one starts
/// with a gap.
#[test]
fn oob_first_record_damage_and_tiny_segments() {
    let _g = serial();
    let dir = chaos_dir("oob-first-record");
    let _cleanup = Cleanup(dir.clone());
    let a_seqs: Vec<u64> = (101..=110).map(seq).collect();
    let m_seqs: Vec<u64> = (401..=410).map(seq).collect();
    let b_seqs: Vec<u64> = (501..=510).map(seq).collect();
    write_segment(&dir, 1, &a_seqs);
    std::fs::write(segment_path(&dir, 2), b"").unwrap();
    let mut short = b"TVW4".to_vec();
    short.extend_from_slice(&[0u8; 8]);
    assert_eq!(short.len(), 12);
    std::fs::write(segment_path(&dir, 3), &short).unwrap();
    let m = write_segment(&dir, 4, &m_seqs);
    write_segment(&dir, 5, &b_seqs);
    // M: a CRC flip inside its FIRST record (seq 401).
    let mut bytes = read(&m);
    bytes[FRAME_AT + 9] ^= 0xFF;
    std::fs::write(&m, &bytes).unwrap();

    let resyncs = counter(WAL_REPLAY_RESYNCS_COUNTER);
    let truncated = counter(WAL_REPLAY_TRUNCATED_SEGMENTS_COUNTER);
    let abandoned = counter(WAL_REPLAY_ABANDONED_BYTES_COUNTER);
    let batch = replay(&dir);

    let want: Vec<u64> = a_seqs
        .iter()
        .chain(m_seqs[1..].iter())
        .chain(b_seqs.iter())
        .copied()
        .collect();
    let got: Vec<u64> = batch.frames.iter().map(|f| f.frame_seq).collect();
    assert_eq!(got, want, "exact survivors, in order");
    for f in &batch.frames {
        assert_eq!(f.frame, payload_for(f.frame_seq), "no fabricated record");
    }
    assert_strictly_rising_and_unique(&batch.frames, "first-record damage replay");
    let (gaps, firsts) = gaps_and_firsts(&batch);
    assert_eq!(
        firsts,
        vec![seq(101), seq(402), seq(501)],
        "the damaged segment's first replayed frame is marked first"
    );
    assert_eq!(
        gaps,
        vec![seq(101), seq(402), seq(501)],
        "gaps at the pass start, after the skipped first record, and after the damaged segment"
    );
    assert!(!batch.trailing_gap);
    assert_eq!(
        batch.segment_ends.len(),
        5,
        "every segment was read, tiny ones included"
    );
    assert_eq!(
        counter(WAL_REPLAY_RESYNCS_COUNTER) - resyncs,
        1,
        "one resync"
    );
    assert_eq!(
        counter(WAL_REPLAY_TRUNCATED_SEGMENTS_COUNTER) - truncated,
        1,
        "only the damaged segment is counted"
    );
    assert_eq!(
        counter(WAL_REPLAY_ABANDONED_BYTES_COUNTER) - abandoned,
        RECORD_LEN as u64,
        "exactly the first record's bytes"
    );
}
