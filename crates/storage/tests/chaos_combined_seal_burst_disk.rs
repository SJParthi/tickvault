//! Combined-fault chaos: the midnight seal burst on a dying disk, the tick
//! spill replay against a refusing / flapping QuestDB with the quarantine
//! disk unwritable, the audit spill at its cap while QuestDB is down, and a
//! bar amended twice across a crash.
//!
//! Every existing chaos test in this crate injects ONE fault at a time
//! (`chaos_midnight_seal_burst.rs`: ring overflow into a healthy spill;
//! `chaos_seal_disk_full_dlq_capture.rs`: spill dead from the start;
//! `seal_drain_recovery.rs`: boot drain on its own). These scenarios stack
//! faults inside ONE run and then assert the honest envelope end to end:
//! no uncounted loss, no duplicate row, the counted outcomes (and, where the
//! code emits one, the metric counter) move, and recovery happens.
//!
//! | # | Test | Faults stacked |
//! |---|------|----------------|
//! | S5 | `s5_midnight_burst_on_a_dying_disk_every_seal_lands_once_or_is_counted_dropped` | ring overflow at IST midnight → the spill volume dies AT the day rotation (ring → spill → DLQ in one burst) → the DLQ dies too (counted `Dropped`) → disk comes back with QuestDB still down (ring rescued to spill) → restart: boot drain replays every surviving seal exactly once; a second boot replays nothing |
//! | S1a | `s1_refused_lines_with_the_quarantine_disk_unwritable_never_drop_or_duplicate_a_good_line` | QuestDB permanently refuses 5 lines of a chunk AND the quarantine set-aside cannot be written (disk at the floor): the file is kept intact; once the disk frees, the 5 lines are set aside exactly once and every good line lands exactly once in that round |
//! | S1b | `s1_transient_503_mid_bisection_then_recovery_sets_bad_lines_aside_once` | a permanent 400 whose bisection is cut by a transient 503: nothing is set aside, the file is kept; the next round isolates and every good line is in QuestDB, each bad line kept once |
//! | S1c | `s1_past_the_line_cap_the_whole_file_is_quarantined_byte_for_byte` | 65 bad lines in one chunk (over `REPLAY_MAX_REJECTED_LINES_PER_CHUNK`): the whole file moves to quarantine with every byte intact and the quarantine counter moves |
//! | AUD | `audit_spill_at_its_cap_while_questdb_is_down_counts_every_refused_row` | QuestDB down → batches spill → the directory reaches its byte cap → a batch is REFUSED (counted on `tv_audit_spill_refused_rows_total`, never silent) → drain while still down keeps every file → QuestDB back with one poison batch: good rows land once, the poison batch is quarantined and counted lost; offered = replayed + quarantined + refused |
//! | OOB | `oob_bar_amended_twice_across_a_crash_with_a_torn_tail_keeps_the_fullest_copy` | the fuller copy of a bar spills, the process dies mid-write (torn tail), the NEXT process (empty ledger) spills an emptier copy and dead-letters a third, emptier still; boot 1 commits once and then QuestDB dies; boot 2 must end on the fullest copy |
//!
//! | S1d | `s1d_exactly_at_the_line_cap_every_bad_line_is_set_aside_none_quarantined` | exactly `REPLAY_MAX_REJECTED_LINES_PER_CHUNK` (64) bad lines: every one is set aside, the file is not quarantined, every good line lands once (the post cap must not trip first) |
//! | AUD-B | `audit_spill_cap_exact_fit_boundary_and_quarantine_not_counted` | a batch that exactly fits under the byte cap is accepted, one byte more is refused (counted by exactly its rows); a full-size file in `quarantine/` does not count; the 50,000-file cap admits the 50,000th file and refuses the next |
//!
//! Hermetic in its dependencies: temp dirs under `std::env::temp_dir()`
//! tagged with the process id; QuestDB is a loopback listener in this
//! process (no Docker, no network). NOT hermetic about free disk space: S5
//! runs the real seal disk floor, so it needs at least 4 GiB (plus a margin)
//! free on the temp volume and fails at once, naming that floor, when it is
//! short. Every test takes one process-wide lock so the metric deltas and
//! the audit test's working-directory switch cannot interleave.
//!
//! Honest limit: the audit spill's only public constructor writes to a
//! RELATIVE directory, so the audit test switches the process working
//! directory to a temp root for its duration (restored by a drop guard).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tickvault_common::feed::Feed;
use tickvault_storage::audit_spill::{
    AUDIT_SPILL_MAX_BYTES_PER_TABLE, AUDIT_SPILL_MAX_FILES_PER_TABLE, AuditSpill, AuditSpillTable,
    drain_audit_spill_dir, spill_failed_batch,
};
use tickvault_storage::disk_health_watcher::{DiskHealthOutcome, probe_disk_free_bytes};
use tickvault_storage::seal_absorption::{
    SEAL_ESCALATION_MIN_FREE_BYTES, SealAbsorptionPipeline, SubmitOutcome,
};
use tickvault_storage::seal_dlq::{SealDlqRecord, SealDlqWriter};
use tickvault_storage::seal_spill::{SealSpillWriter, SerializedSeal};
use tickvault_storage::seal_writer_task::{
    DrainOutcome, SealSink, drain_once, drain_recovered_seals,
};
use tickvault_storage::shadow_candle_writer::ShadowCandleWriter;
use tickvault_storage::tick_spill_replay::{
    QUARANTINE_DIR, REJECTED_LINES_SUFFIX, REPLAY_LINES_REJECTED_COUNTER,
    REPLAY_MAX_REJECTED_LINES_PER_CHUNK, REPLAY_QUARANTINED_COUNTER, SpillSinkView,
    replay_spill_dir,
};
use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

/// Serialises every test in this file (metric deltas, working directory).
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 2026-01-01 23:59:30 IST (18:29:30 UTC): thirty seconds before the IST
/// midnight the multi-TF aggregator force-seals the whole universe.
const NOW_BEFORE_MIDNIGHT: i64 = 1_767_292_170;
/// 2026-01-02 00:00:30 IST: the next IST day, so the spill rotates its file.
const NOW_AFTER_MIDNIGHT: i64 = NOW_BEFORE_MIDNIGHT + 60;

/// 09:15:00 IST on a real trading date (a plausible epoch, see
/// `seal_drain_recovery.rs::mk_seal` for why this must not be a
/// seconds-of-day value).
const BUCKET_BASE: u32 = 1_716_023_700;

fn unique_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!(
        "tv-chaos-combined-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

/// Removes the temp root when the test ends, pass or fail.
struct RootGuard(PathBuf);
impl Drop for RootGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mk_seal(sid: u64, bucket: u32, ticks: u32) -> BufferedSeal {
    let mut state = LiveCandleState::empty();
    state.bucket_start_ist_secs = bucket;
    state.open = 100.0;
    state.high = 105.5;
    state.low = 99.25;
    state.close = 101.75;
    state.volume = 100 * u64::from(ticks);
    state.bucket_start_cumulative = 1000;
    state.oi = 50_000;
    state.tick_count = ticks;
    BufferedSeal::new(sid, 1, TfIndex::M1, state, Feed::Dhan)
}

/// `(security_id, segment, bucket)` — the bar identity under the candle
/// DEDUP key (feed is constant here).
type BarId = (u64, u8, u32);

fn bar_of(seal: &BufferedSeal) -> BarId {
    (
        seal.security_id,
        seal.exchange_segment_code,
        seal.state.bucket_start_ist_secs,
    )
}

/// A `SealSink` that records `(bar, tick_count)` in commit order and can let
/// only `ok_flushes` flushes land before QuestDB "dies".
struct RecordingSink {
    pending: Vec<(BarId, u32)>,
    committed: Vec<(BarId, u32)>,
    ok_flushes: Option<usize>,
}

impl RecordingSink {
    fn healthy() -> Self {
        Self {
            pending: Vec::new(),
            committed: Vec::new(),
            ok_flushes: None,
        }
    }
    fn dying_after(ok_flushes: usize) -> Self {
        Self {
            ok_flushes: Some(ok_flushes),
            ..Self::healthy()
        }
    }
}

impl SealSink for RecordingSink {
    fn append_seal(&mut self, seal: &BufferedSeal) -> anyhow::Result<()> {
        self.pending.push((bar_of(seal), seal.state.tick_count));
        Ok(())
    }
    fn flush(&mut self) -> anyhow::Result<()> {
        if let Some(left) = self.ok_flushes.as_mut() {
            if *left == 0 {
                anyhow::bail!("questdb connection refused");
            }
            *left -= 1;
        }
        self.committed.append(&mut self.pending);
        Ok(())
    }
    fn discard_pending(&mut self) {
        self.pending.clear();
    }
}

/// Moves `path` aside and puts a regular FILE in its place, so every
/// `create_dir_all(path)` fails deterministically on every OS (the dead
/// volume). Returns where the real directory went.
fn kill_dir(path: &Path) -> PathBuf {
    let parked = path.with_extension("parked");
    std::fs::rename(path, &parked).expect("park the dir");
    std::fs::write(path, b"dead volume").expect("block the dir path with a file");
    parked
}

/// Undoes [`kill_dir`]: the volume is back with everything it held.
fn revive_dir(path: &Path, parked: &Path) {
    std::fs::remove_file(path).expect("remove the blocking file");
    std::fs::rename(parked, path).expect("restore the dir");
}

// ---------------------------------------------------------------------------
// Metric capture: a minimal process-wide recorder (no new dependency).
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Cell(AtomicU64);

impl metrics::CounterFn for Cell {
    fn increment(&self, value: u64) {
        self.0.fetch_add(value, Ordering::SeqCst);
    }
    fn absolute(&self, value: u64) {
        self.0.fetch_max(value, Ordering::SeqCst);
    }
}

#[derive(Clone, Default)]
struct TestRecorder {
    cells: Arc<Mutex<HashMap<String, Arc<Cell>>>>,
}

fn metric_key(name: &str, labels: &[(&str, &str)]) -> String {
    let mut sorted: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    sorted.sort();
    format!("{name}{{{}}}", sorted.join(","))
}

impl metrics::Recorder for TestRecorder {
    fn describe_counter(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_gauge(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_histogram(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn register_counter(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Counter {
        let labels: Vec<(&str, &str)> = key.labels().map(|l| (l.key(), l.value())).collect();
        let id = metric_key(key.name(), &labels);
        let cell = Arc::clone(
            self.cells
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entry(id)
                .or_default(),
        );
        metrics::Counter::from_arc(cell)
    }
    fn register_gauge(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
        metrics::Gauge::noop()
    }
    fn register_histogram(
        &self,
        _: &metrics::Key,
        _: &metrics::Metadata<'_>,
    ) -> metrics::Histogram {
        metrics::Histogram::noop()
    }
}

/// Installs the recorder once per process and returns a reader handle.
fn recorder() -> &'static TestRecorder {
    static REC: OnceLock<TestRecorder> = OnceLock::new();
    REC.get_or_init(|| {
        let rec = TestRecorder::default();
        // Another recorder can never be installed in this binary; a failure
        // would only mean a second init, which OnceLock rules out.
        let _ = metrics::set_global_recorder(rec.clone());
        rec
    })
}

fn counter(name: &str, labels: &[(&str, &str)]) -> u64 {
    recorder()
        .cells
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&metric_key(name, labels))
        .map_or(0, |c| c.0.load(Ordering::SeqCst))
}

// ---------------------------------------------------------------------------
// Fake QuestDB `/write`: loopback, one request per connection, transactional
// per request (any refused line refuses the whole body, as QuestDB's HTTP ILP
// endpoint does).
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeDbState {
    /// QuestDB unreachable-ish: every POST answers 503.
    down: bool,
    /// 1-based POST numbers answered 503 regardless of the body.
    transient_posts: Vec<usize>,
    posts: usize,
    /// Every line QuestDB accepted, in arrival order (repeats kept).
    accepted: Vec<Vec<u8>>,
}

#[derive(Clone)]
struct FakeDb {
    url: String,
    state: Arc<Mutex<FakeDbState>>,
}

/// A body line QuestDB refuses for good (400): a malformed row.
const POISON_MARKER: &[u8] = b"BADROW";

impl FakeDb {
    async fn spawn() -> (Self, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let url = format!("http://{}/write", listener.local_addr().expect("addr"));
        let state = Arc::new(Mutex::new(FakeDbState::default()));
        let server_state = Arc::clone(&state);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match sock.read(&mut byte).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => head.push(byte[0]),
                    }
                }
                let head_text = String::from_utf8_lossy(&head).to_ascii_lowercase();
                let len: usize = head_text
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                if sock.read_exact(&mut body).await.is_err() {
                    continue;
                }
                let status = {
                    let mut st = server_state.lock().unwrap_or_else(|p| p.into_inner());
                    st.posts += 1;
                    let post = st.posts;
                    if st.down || st.transient_posts.contains(&post) {
                        "503 Service Unavailable"
                    } else if body
                        .windows(POISON_MARKER.len())
                        .any(|w| w == POISON_MARKER)
                    {
                        "400 Bad Request"
                    } else {
                        for line in body.split_inclusive(|b| *b == b'\n') {
                            if !line.is_empty() {
                                st.accepted.push(line.to_vec());
                            }
                        }
                        "204 No Content"
                    }
                };
                let resp =
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (Self { url, state }, handle)
    }

    fn with<R>(&self, f: impl FnOnce(&mut FakeDbState) -> R) -> R {
        f(&mut self.state.lock().unwrap_or_else(|p| p.into_inner()))
    }

    fn accepted_len(&self) -> usize {
        self.with(|s| s.accepted.len())
    }

    fn accepted_since(&self, from: usize) -> Vec<Vec<u8>> {
        self.with(|s| s.accepted[from..].to_vec())
    }

    fn posts(&self) -> usize {
        self.with(|s| s.posts)
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("client")
}

/// A replay view with a quiet, trusted sink and no market-data floor.
fn quiet_view() -> SpillSinkView {
    SpillSinkView {
        suspect: false,
        clean_probes: 0,
        suspect_episodes: 0,
        episode_start: None,
        now_unix_secs: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        live: false,
        market_data_floor_unix_nanos: None,
    }
}

fn count_lines(lines: &[Vec<u8>]) -> BTreeMap<Vec<u8>, usize> {
    let mut map = BTreeMap::new();
    for line in lines {
        *map.entry(line.clone()).or_insert(0) += 1;
    }
    map
}

/// A tick spill body: `good` good lines with a bad line after every
/// `stride`-th, `bad` bad lines in all. The timestamp `1` sits below the
/// plausible-epoch band, so the replay's window filter keeps every line and
/// QuestDB alone judges it.
fn tick_body(good: usize, bad: usize, stride: usize) -> (Vec<u8>, Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut body = Vec::new();
    let mut goods = Vec::new();
    let mut bads = Vec::new();
    let mut emitted_bad = 0;
    for n in 0..good {
        let line = format!("ticks,segment=NSE_EQ,feed=dhan security_id={n}i,ltp=1.5 1\n");
        body.extend_from_slice(line.as_bytes());
        goods.push(line.into_bytes());
        if emitted_bad < bad && n % stride == stride / 2 {
            let line = format!("ticks,segment=NSE_EQ,feed=dhan BADROW{emitted_bad} torn\n");
            body.extend_from_slice(line.as_bytes());
            bads.push(line.into_bytes());
            emitted_bad += 1;
        }
    }
    while emitted_bad < bad {
        let line = format!("ticks,segment=NSE_EQ,feed=dhan BADROW{emitted_bad} torn\n");
        body.extend_from_slice(line.as_bytes());
        bads.push(line.into_bytes());
        emitted_bad += 1;
    }
    (body, goods, bads)
}

/// The rejected-lines file the replay keeps for `file_name`, split in lines.
fn rejected_lines(dir: &Path, file_name: &str) -> Vec<Vec<u8>> {
    let path = dir
        .join(QUARANTINE_DIR)
        .join(format!("{file_name}{REJECTED_LINES_SUFFIX}"));
    std::fs::read(path)
        .map(|bytes| {
            bytes
                .split_inclusive(|b| *b == b'\n')
                .map(<[u8]>::to_vec)
                .collect()
        })
        .unwrap_or_default()
}

/// Margin above the seal escalation floor this file needs on the temp volume
/// (the seals it writes, rounded well up).
const SEAL_FLOOR_TEST_MARGIN_BYTES: u64 = 256 * 1024 * 1024;

/// S5 runs the REAL `SealDiskFloor`, which refuses every spill and DLQ write
/// (`DroppedBelowFloor`) when the volume has less than
/// `SEAL_ESCALATION_MIN_FREE_BYTES` (4 GiB) free. The floor's probe override
/// is `#[cfg(test)] pub(crate)`, unreachable from an integration test, so on
/// a short volume the test fails here, naming the cause, instead of failing
/// later with counts that read like a probe bug.
fn require_room_above_the_seal_floor(dir: &Path) {
    let need = SEAL_ESCALATION_MIN_FREE_BYTES + SEAL_FLOOR_TEST_MARGIN_BYTES;
    match probe_disk_free_bytes(dir) {
        DiskHealthOutcome::Ok { free_bytes, .. } => assert!(
            free_bytes >= need,
            "precondition: the temp volume at {} has {free_bytes} bytes free, under the \
             {SEAL_ESCALATION_MIN_FREE_BYTES}-byte (4 GiB) seal escalation floor plus a \
             {SEAL_FLOOR_TEST_MARGIN_BYTES}-byte margin; every spill and DLQ write would be \
             refused as DroppedBelowFloor. Free space on the temp volume and re-run.",
            dir.display()
        ),
        DiskHealthOutcome::ProbeFailed { reason } => {
            panic!(
                "precondition: cannot read free space at {}: {reason}",
                dir.display()
            )
        }
    }
}

// ---------------------------------------------------------------------------
// S5 — midnight seal burst on a dying disk
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
struct SubmitTally {
    buffered: usize,
    spilled: usize,
    dlq: usize,
    dropped: Vec<BarId>,
    dropped_below_floor: Vec<BarId>,
}

impl SubmitTally {
    fn note(&mut self, outcome: SubmitOutcome) {
        match outcome {
            SubmitOutcome::Buffered => self.buffered += 1,
            SubmitOutcome::Spilled => self.spilled += 1,
            SubmitOutcome::DlqWritten => self.dlq += 1,
            SubmitOutcome::Dropped(seal) => self.dropped.push(bar_of(&seal)),
            SubmitOutcome::DroppedBelowFloor(seal) => self.dropped_below_floor.push(bar_of(&seal)),
        }
    }
}

#[test]
fn s5_midnight_burst_on_a_dying_disk_every_seal_lands_once_or_is_counted_dropped() {
    let _serial = SERIAL.blocking_lock();
    let _ = recorder();
    let root = unique_root("s5");
    let _guard = RootGuard(root.clone());
    let spill = root.join("spill");
    let dlq = root.join("dlq");
    std::fs::create_dir_all(&spill).expect("spill");
    std::fs::create_dir_all(&dlq).expect("dlq");
    require_room_above_the_seal_floor(&root);

    const RING: usize = 1_000;
    let mut pipeline =
        SealAbsorptionPipeline::with_capacity_and_dirs_for_test(RING, spill.clone(), dlq.clone());
    let mut tally = SubmitTally::default();
    let mut all_bars: Vec<BarId> = Vec::new();
    let mut next_sid: u64 = 50_000;
    let mut burst =
        |pipeline: &mut SealAbsorptionPipeline, n: usize, now: i64, tally: &mut SubmitTally| {
            for _ in 0..n {
                let seal = mk_seal(next_sid, BUCKET_BASE, 7);
                next_sid += 1;
                all_bars.push(bar_of(&seal));
                tally.note(pipeline.submit(seal, now));
            }
        };

    // Phase A, 23:59:30 IST: the ring fills, then overflows into a healthy spill.
    burst(&mut pipeline, RING + 1_000, NOW_BEFORE_MIDNIGHT, &mut tally);
    assert_eq!(tally.buffered, RING);
    assert_eq!(
        tally.spilled, 1_000,
        "ring overflow → spill while the disk is healthy"
    );

    // Phase B, 00:00:30 IST: the spill volume dies exactly as the spill
    // rotates to the new day's file — the open fails, every eviction goes to
    // the DLQ inside the same burst.
    let spill_parked = kill_dir(&spill);
    burst(&mut pipeline, 1_000, NOW_AFTER_MIDNIGHT, &mut tally);
    assert_eq!(
        tally.dlq, 1_000,
        "spill dead mid-burst → DLQ catches every eviction"
    );

    // Phase C: the DLQ volume dies too. The terminal tier: each eviction is a
    // typed `Dropped` carrying the seal (the AGGREGATOR-DROP-01 page).
    let dlq_parked = kill_dir(&dlq);
    burst(&mut pipeline, 300, NOW_AFTER_MIDNIGHT, &mut tally);
    assert_eq!(
        tally.dropped.len(),
        300,
        "all three tiers dead → counted Dropped"
    );
    assert!(
        tally.dropped_below_floor.is_empty(),
        "the floor never refused (disk has room); a refusal here would mean the probe misread"
    );

    // CONSERVATION at submit time: every seal in exactly one place.
    let submitted = all_bars.len();
    assert_eq!(
        pipeline.ring_len() + tally.spilled + tally.dlq + tally.dropped.len(),
        submitted,
        "ring + spill + DLQ + dropped must equal submitted: {tally:?}, ring {}",
        pipeline.ring_len()
    );

    // Phase D: the disk comes back; QuestDB is still DOWN, so the writer's
    // drain cycles fail their flush and rescue the ring to the spill.
    revive_dir(&spill, &spill_parked);
    revive_dir(&dlq, &dlq_parked);
    let in_ring = pipeline.ring_len();
    let mut writer = ShadowCandleWriter::for_test();
    let mut rescued = DrainOutcome {
        flushed_ok: true,
        ..DrainOutcome::default()
    };
    let mut cycles = 0;
    while pipeline.ring_len() > 0 {
        cycles += 1;
        assert!(cycles < 100, "drain must converge");
        let out = drain_once(&mut pipeline, &mut writer, 256, NOW_AFTER_MIDNIGHT);
        rescued.accumulate(&out);
    }
    assert!(
        !rescued.flushed_ok,
        "QuestDB is down: no flush may report OK"
    );
    assert_eq!(
        rescued.rescued_to_spill, in_ring,
        "the revived spill takes the whole ring"
    );
    assert_eq!(rescued.rescued_to_dlq + rescued.rescued_dropped, 0);
    drop(pipeline); // process restart

    // Restart: the boot drain replays every surviving seal exactly once.
    let mut sink = RecordingSink::healthy();
    let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 512);
    let surviving = submitted - tally.dropped.len();
    assert_eq!(outcome.seals_recovered, surviving, "{outcome:?}");
    assert_eq!(outcome.seals_reingested, surviving, "{outcome:?}");
    assert_eq!(outcome.records_undecodable, 0);
    assert_eq!(outcome.seals_superseded, 0, "every bar is distinct");
    assert_eq!(outcome.files_left_pending, 0);
    assert_eq!(outcome.seals_left_pending, 0);

    let mut per_bar: HashMap<BarId, usize> = HashMap::new();
    for (bar, _) in &sink.committed {
        *per_bar.entry(*bar).or_insert(0) += 1;
    }
    let duplicated: Vec<_> = per_bar.iter().filter(|(_, n)| **n > 1).collect();
    assert!(
        duplicated.is_empty(),
        "a seal was written twice: {duplicated:?}"
    );
    let dropped: std::collections::HashSet<BarId> = tally.dropped.iter().copied().collect();
    for bar in &all_bars {
        let landed = per_bar.contains_key(bar);
        assert_ne!(
            landed,
            dropped.contains(bar),
            "bar {bar:?}: must be EITHER replayed OR counted dropped, never both / neither"
        );
    }

    // A second boot finds nothing: no replay twice.
    let mut sink2 = RecordingSink::healthy();
    let again = drain_recovered_seals(&mut sink2, &spill, &dlq, 512);
    assert_eq!(again.seals_recovered, 0, "{again:?}");
    assert!(sink2.committed.is_empty());
}

// ---------------------------------------------------------------------------
// OUT-OF-BOX — one bar amended twice across a crash
// ---------------------------------------------------------------------------

#[test]
fn oob_bar_amended_twice_across_a_crash_with_a_torn_tail_keeps_the_fullest_copy() {
    let _serial = SERIAL.blocking_lock();
    let _ = recorder();
    let root = unique_root("oob");
    let _guard = RootGuard(root.clone());
    let spill = root.join("spill");
    let dlq = root.join("dlq");
    std::fs::create_dir_all(&spill).expect("spill");
    std::fs::create_dir_all(&dlq).expect("dlq");

    const SID: u64 = 777;
    let amended_bar = BUCKET_BASE;
    let later_bar = BUCKET_BASE + 60;

    // Process 1: the FULLER copy (9 ticks, the late-tick amend) spills, then a
    // later bucket of the same slot; then the process dies mid-write of a
    // third record, leaving a torn tail.
    {
        let p1 = SealSpillWriter::with_spill_dir_for_test(spill.clone());
        p1.append_seal(
            &SerializedSeal::from(&mk_seal(SID, amended_bar, 9)),
            NOW_BEFORE_MIDNIGHT,
        )
        .expect("spill fuller");
        p1.append_seal(
            &SerializedSeal::from(&mk_seal(SID, later_bar, 1)),
            NOW_BEFORE_MIDNIGHT,
        )
        .expect("spill later bucket");
        let day_file = p1.spill_path(NOW_BEFORE_MIDNIGHT);
        drop(p1);
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&day_file)
            .and_then(|mut f| f.write_all(&[0xAB; 50]))
            .expect("torn tail");
    }

    // Process 2 (empty ledger, so it cannot know a fuller copy exists): an
    // EMPTIER copy of the same bar (4 ticks, a stale re-fold) spills into the
    // same day file, and an emptier one still (3 ticks) is dead-lettered.
    {
        let p2 = SealSpillWriter::with_spill_dir_for_test(spill.clone());
        p2.append_seal(
            &SerializedSeal::from(&mk_seal(SID, amended_bar, 4)),
            NOW_BEFORE_MIDNIGHT,
        )
        .expect("spill emptier after the crash (the torn tail is cut first)");
        SealDlqWriter::with_dlq_dir_for_test(dlq.clone())
            .append_record(
                &SealDlqRecord::from(&SerializedSeal::from(&mk_seal(SID, amended_bar, 3))),
                NOW_AFTER_MIDNIGHT,
            )
            .expect("dlq emptiest");
    }

    // Boot 1: one flush lands (batch of 1 record), then QuestDB dies.
    let mut boot1 = RecordingSink::dying_after(1);
    let first = drain_recovered_seals(&mut boot1, &spill, &dlq, 1);
    assert_eq!(
        first.records_undecodable, 0,
        "the torn tail was cut, not read: {first:?}"
    );
    assert_eq!(boot1.committed, vec![((SID, 1, amended_bar), 9)]);
    assert!(
        first.files_left_pending >= 1,
        "the dead DB leaves files staged: {first:?}"
    );

    // Boot 2: QuestDB is back.
    let mut boot2 = RecordingSink::healthy();
    let second = drain_recovered_seals(&mut boot2, &spill, &dlq, 1);
    assert_eq!(second.files_left_pending, 0, "{second:?}");
    assert!(
        second.seals_superseded >= 2,
        "the 4-tick and 3-tick copies are refused and counted: {second:?}"
    );

    // The row the candle table keeps is the LAST committed write per bar,
    // across both boots (last-write-wins DEDUP key).
    let mut table: HashMap<BarId, u32> = HashMap::new();
    for (bar, ticks) in boot1.committed.iter().chain(boot2.committed.iter()) {
        table.insert(*bar, *ticks);
    }
    assert_eq!(
        table.get(&(SID, 1, amended_bar)),
        Some(&9),
        "the fullest copy must be the final row: boot1 {:?} boot2 {:?}",
        boot1.committed,
        boot2.committed
    );
    assert_eq!(table.get(&(SID, 1, later_bar)), Some(&1));
    for (bar, ticks) in boot1.committed.iter().chain(boot2.committed.iter()) {
        if *bar == (SID, 1, amended_bar) {
            assert_eq!(*ticks, 9, "an emptier copy was written at any point");
        }
    }
}

// ---------------------------------------------------------------------------
// S1 — tick spill replay with a refused-chunk mix and a disk at the floor
// ---------------------------------------------------------------------------

const TICK_FILE: &str = "ticks-dhan-1.ilp";

/// Replay dir: the label "other" (exchange-clocked window, never held).
fn replay_dir(root: &Path) -> PathBuf {
    let dir = root.join("spill_other");
    std::fs::create_dir_all(&dir).expect("replay dir");
    dir
}

fn rejected_counter() -> u64 {
    counter(REPLAY_LINES_REJECTED_COUNTER, &[("dir", "other")])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s1_refused_lines_with_the_quarantine_disk_unwritable_never_drop_or_duplicate_a_good_line()
{
    let _serial = SERIAL.lock().await;
    let _ = recorder();
    let root = unique_root("s1a");
    let _guard = RootGuard(root.clone());
    let dir = replay_dir(&root);
    let path = dir.join(TICK_FILE);
    let (body, goods, bads) = tick_body(200, 5, 40);
    std::fs::write(&path, &body).expect("spill file");
    let (db, server) = FakeDb::spawn().await;
    let client = http_client();
    let rejected_before = rejected_counter();

    // Round 1: QuestDB refuses the poison lines AND the quarantine directory
    // cannot be created (the disk is at its floor). Nothing may be lost: the
    // file stays intact for the next round.
    std::fs::write(dir.join(QUARANTINE_DIR), b"disk full").expect("block quarantine");
    let first = replay_spill_dir(&dir, &db.url, &client, &quiet_view()).await;
    assert_eq!(first.files_failed, 1, "{first:?}");
    assert_eq!(first.lines_set_aside, 0, "{first:?}");
    assert_eq!(
        std::fs::read(&path).expect("file"),
        body,
        "the file is kept byte for byte"
    );
    assert_eq!(
        rejected_counter(),
        rejected_before,
        "nothing counted as set aside yet"
    );

    // Round 2: the disk frees.
    std::fs::remove_file(dir.join(QUARANTINE_DIR)).expect("free the disk");
    let mark = db.accepted_len();
    let second = replay_spill_dir(&dir, &db.url, &client, &quiet_view()).await;
    server.abort();
    assert_eq!(second.files_failed, 0, "{second:?}");
    assert_eq!(second.lines_set_aside, bads.len() as u64, "{second:?}");
    assert_eq!(rejected_counter() - rejected_before, bads.len() as u64);

    let kept = rejected_lines(&dir, TICK_FILE);
    assert_eq!(
        count_lines(&kept),
        count_lines(&bads),
        "each bad line kept exactly once"
    );
    let round2 = count_lines(&db.accepted_since(mark));
    assert_eq!(
        round2,
        count_lines(&goods),
        "round 2: every good line exactly once, no bad line"
    );
    let all = count_lines(&db.accepted_since(0));
    assert_eq!(
        all.keys().collect::<Vec<_>>(),
        count_lines(&goods).keys().collect::<Vec<_>>(),
        "across rounds QuestDB holds exactly the good lines (repeats collapse on the DEDUP key)"
    );

    // Round 3: drained — no re-POST, no second copy in quarantine.
    let posts = db.posts();
    let (db2, server2) = FakeDb::spawn().await;
    let third = replay_spill_dir(&dir, &db2.url, &client, &quiet_view()).await;
    server2.abort();
    assert_eq!(third.lines_set_aside, 0, "{third:?}");
    assert_eq!(db2.posts(), 0, "a drained file is not re-sent");
    assert_eq!(db.posts(), posts);
    assert_eq!(rejected_lines(&dir, TICK_FILE).len(), bads.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s1_transient_503_mid_bisection_then_recovery_sets_bad_lines_aside_once() {
    let _serial = SERIAL.lock().await;
    let _ = recorder();
    let root = unique_root("s1b");
    let _guard = RootGuard(root.clone());
    let dir = replay_dir(&root);
    let path = dir.join(TICK_FILE);
    let (body, goods, bads) = tick_body(300, 7, 40);
    std::fs::write(&path, &body).expect("spill file");
    let (db, server) = FakeDb::spawn().await;
    let client = http_client();
    let rejected_before = rejected_counter();

    // POST 1 = the whole chunk (400), POST 2 = the left half, POST 4 = a 503
    // in the middle of the bisection.
    db.with(|s| s.transient_posts = vec![4]);
    let first = replay_spill_dir(&dir, &db.url, &client, &quiet_view()).await;
    assert_eq!(first.files_failed, 1, "{first:?}");
    assert_eq!(first.lines_set_aside, 0, "{first:?}");
    assert!(
        rejected_lines(&dir, TICK_FILE).is_empty(),
        "nothing set aside on a transient stop"
    );
    assert_eq!(std::fs::read(&path).expect("file"), body);
    let after_first = db.accepted_len();

    let second = replay_spill_dir(&dir, &db.url, &client, &quiet_view()).await;
    server.abort();
    assert_eq!(second.files_failed, 0, "{second:?}");
    assert_eq!(second.lines_set_aside, bads.len() as u64, "{second:?}");
    assert_eq!(rejected_counter() - rejected_before, bads.len() as u64);
    assert_eq!(
        count_lines(&rejected_lines(&dir, TICK_FILE)),
        count_lines(&bads)
    );
    assert_eq!(
        count_lines(&db.accepted_since(after_first)),
        count_lines(&goods),
        "the recovery round lands every good line exactly once"
    );
    let all = db.accepted_since(0);
    assert!(
        all.iter()
            .all(|l| !l.windows(6).any(|w| w == POISON_MARKER))
    );
    assert_eq!(
        count_lines(&all).len(),
        goods.len(),
        "no good line missing; the only repeats are round-1 halves accepted before the 503"
    );
    assert_eq!(
        all.len() - goods.len(),
        after_first,
        "repeats are bounded by round 1's accepts"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s1_past_the_line_cap_the_whole_file_is_quarantined_byte_for_byte() {
    let _serial = SERIAL.lock().await;
    let _ = recorder();
    let root = unique_root("s1c");
    let _guard = RootGuard(root.clone());
    let dir = replay_dir(&root);
    let path = dir.join(TICK_FILE);
    let bad = REPLAY_MAX_REJECTED_LINES_PER_CHUNK + 1;
    let (body, _goods, _bads) = tick_body(2 * bad, bad, 2);
    std::fs::write(&path, &body).expect("spill file");
    let (db, server) = FakeDb::spawn().await;
    let client = http_client();
    let quarantined_before = counter(REPLAY_QUARANTINED_COUNTER, &[]);
    let rejected_before = rejected_counter();

    let out = replay_spill_dir(&dir, &db.url, &client, &quiet_view()).await;
    server.abort();
    assert_eq!(out.files_quarantined, 1, "{out:?}");
    assert_eq!(out.lines_set_aside, 0, "{out:?}");
    assert_eq!(
        counter(REPLAY_QUARANTINED_COUNTER, &[]) - quarantined_before,
        1
    );
    assert_eq!(
        rejected_counter(),
        rejected_before,
        "no partial set-aside past the cap"
    );
    assert!(!path.exists(), "the file left the queue");
    let moved = std::fs::read(dir.join(QUARANTINE_DIR).join(TICK_FILE)).expect("quarantined");
    assert_eq!(moved, body, "every byte, good and bad, is kept for salvage");
}

/// Exactly at the line cap: 64 bad lines in one chunk. Every bad line is set
/// aside, the file is NOT quarantined (the bisection post cap,
/// `replay_max_isolation_posts`, must not trip before the line cap: the K1
/// regression), and QuestDB holds every good line exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s1d_exactly_at_the_line_cap_every_bad_line_is_set_aside_none_quarantined() {
    let _serial = SERIAL.lock().await;
    let _ = recorder();
    let root = unique_root("s1d");
    let _guard = RootGuard(root.clone());
    let dir = replay_dir(&root);
    let path = dir.join(TICK_FILE);
    let bad = REPLAY_MAX_REJECTED_LINES_PER_CHUNK;
    let (body, goods, bads) = tick_body(2 * bad, bad, 2);
    assert_eq!(bads.len(), bad, "the body holds exactly the cap");
    std::fs::write(&path, &body).expect("spill file");
    let (db, server) = FakeDb::spawn().await;
    let client = http_client();
    let quarantined_before = counter(REPLAY_QUARANTINED_COUNTER, &[]);
    let rejected_before = rejected_counter();

    let out = replay_spill_dir(&dir, &db.url, &client, &quiet_view()).await;
    server.abort();
    assert_eq!(out.files_quarantined, 0, "{out:?}");
    assert_eq!(out.files_failed, 0, "{out:?}");
    assert_eq!(out.lines_set_aside, bad as u64, "{out:?}");
    assert_eq!(
        counter(REPLAY_QUARANTINED_COUNTER, &[]),
        quarantined_before,
        "at the cap nothing is quarantined"
    );
    assert_eq!(rejected_counter() - rejected_before, bad as u64);
    assert_eq!(
        count_lines(&rejected_lines(&dir, TICK_FILE)),
        count_lines(&bads),
        "exactly the 64 bad lines are set aside, once each"
    );
    let accepted = db.accepted_since(0);
    assert!(
        accepted
            .iter()
            .all(|l| !l.windows(POISON_MARKER.len()).any(|w| w == POISON_MARKER)),
        "no bad line reached QuestDB"
    );
    assert_eq!(
        count_lines(&accepted).keys().collect::<Vec<_>>(),
        count_lines(&goods).keys().collect::<Vec<_>>(),
        "every good line reached QuestDB"
    );
    assert!(
        !dir.join(QUARANTINE_DIR).join(TICK_FILE).exists(),
        "the file was not moved to quarantine"
    );
}

// ---------------------------------------------------------------------------
// AUD — audit spill at its cap while QuestDB is down
// ---------------------------------------------------------------------------

/// Switches the process working directory for the audit spill's relative
/// path and restores it on drop (then removes the root).
struct CwdGuard {
    original: PathBuf,
    root: PathBuf,
}
impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn audit_batch(first_row: usize, rows: usize, poison: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for n in first_row..first_row + rows {
        let marker = if poison { "BADROW" } else { "ok" };
        out.extend_from_slice(
            format!(
                "order_audit,feed=dhan order_id=\"O{n}\",note=\"{marker}\" {}\n",
                1_767_292_170_000_000_000_i64 + n as i64
            )
            .as_bytes(),
        );
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_spill_at_its_cap_while_questdb_is_down_counts_every_refused_row() {
    let _serial = SERIAL.lock().await;
    let _ = recorder();
    let root = unique_root("aud");
    let _cwd = CwdGuard {
        original: std::env::current_dir().expect("cwd"),
        root: root.clone(),
    };
    std::env::set_current_dir(&root).expect("enter temp root");

    let table = AuditSpillTable::OrderAudit;
    let name = table.table_name();
    let dir = root.join(table.default_dir());
    let spill = AuditSpill::for_table(table);
    let spilled_before = counter("tv_audit_spill_rows_total", &[("table", name)]);
    let refused_before = counter("tv_audit_spill_refused_rows_total", &[("table", name)]);
    let replayed_before = counter("tv_audit_spill_replayed_rows_total", &[("table", name)]);
    let lost_before = counter(
        "tv_order_audit_chain_lost_total",
        &[("source", table.lost_source())],
    );

    // QuestDB is down: four failed flushes spill (the second holds a row
    // QuestDB will refuse for good once it is back).
    const ROWS: usize = 3;
    let mut good_rows: Vec<Vec<u8>> = Vec::new();
    for batch in 0..4 {
        let poison = batch == 1;
        let payload = audit_batch(batch * ROWS, ROWS, poison);
        if !poison {
            good_rows.extend(payload.split_inclusive(|b| *b == b'\n').map(<[u8]>::to_vec));
        }
        assert!(
            spill_failed_batch(Some(&spill), &payload, ROWS),
            "batch {batch} must spill"
        );
    }

    // The directory reaches its byte cap (a huge spill still being written,
    // sparse so the test costs no disk) and the next batch is REFUSED: the
    // writer discards it, and the refusal is counted, never silent.
    let filler = dir.join("00000000000000000001-1-0000000000-1.ilp.tmp");
    std::fs::File::create(&filler)
        .and_then(|f| f.set_len(AUDIT_SPILL_MAX_BYTES_PER_TABLE - 64))
        .expect("sparse filler");
    let refused = audit_batch(100, ROWS, false);
    assert!(
        !spill_failed_batch(Some(&spill), &refused, ROWS),
        "past the cap: refused"
    );
    assert_eq!(
        counter("tv_audit_spill_refused_rows_total", &[("table", name)]) - refused_before,
        ROWS as u64,
        "every refused row is counted"
    );
    assert_eq!(
        counter("tv_audit_spill_rows_total", &[("table", name)]) - spilled_before,
        4 * ROWS as u64
    );

    // Drain while QuestDB is still down: every file is kept.
    let (db, server) = FakeDb::spawn().await;
    db.with(|s| s.down = true);
    let client = http_client();
    let down = drain_audit_spill_dir(&dir, table, &db.url, &client).await;
    assert!(down.failed, "{down:?}");
    assert_eq!(down.files_replayed + down.files_quarantined, 0, "{down:?}");
    assert_eq!(
        tickvault_storage::tick_spill_replay::list_spill_files(&dir).len(),
        4,
        "a dead QuestDB never costs a waiting file"
    );

    // QuestDB is back; the oversized spill finished elsewhere.
    std::fs::remove_file(&filler).expect("remove filler");
    db.with(|s| s.down = false);
    let up = drain_audit_spill_dir(&dir, table, &db.url, &client).await;
    server.abort();
    assert!(!up.failed, "{up:?}");
    assert_eq!(up.files_replayed, 3, "{up:?}");
    assert_eq!(up.rows_replayed, 3 * ROWS as u64);
    assert_eq!(
        up.files_quarantined, 1,
        "the poison batch is set aside, the queue moves on"
    );
    assert_eq!(
        counter("tv_audit_spill_replayed_rows_total", &[("table", name)]) - replayed_before,
        3 * ROWS as u64
    );
    assert_eq!(
        counter(
            "tv_order_audit_chain_lost_total",
            &[("source", table.lost_source())]
        ) - lost_before,
        ROWS as u64,
        "a permanently refused batch is counted lost"
    );
    assert_eq!(
        count_lines(&db.accepted_since(0)),
        count_lines(&good_rows),
        "every good row reached QuestDB exactly once; no refused or poison row did"
    );

    // Conservation: offered = replayed + quarantined (lost, counted) +
    // refused at the cap (counted).
    let offered = 5 * ROWS as u64;
    assert_eq!(
        up.rows_replayed
            + (counter(
                "tv_order_audit_chain_lost_total",
                &[("source", table.lost_source())]
            ) - lost_before)
            + (counter("tv_audit_spill_refused_rows_total", &[("table", name)]) - refused_before),
        offered
    );
    assert!(tickvault_storage::tick_spill_replay::list_spill_files(&dir).is_empty());

    // The cap released: a later failed flush spills again.
    assert!(spill_failed_batch(
        Some(&spill),
        &audit_batch(200, ROWS, false),
        ROWS
    ));
}

/// The audit spill caps at their exact boundaries. A batch that exactly fits
/// under the byte cap is accepted; one byte more is refused and counted by
/// exactly its rows; files in `quarantine/` never count toward the cap; the
/// file-count cap admits the 50,000th file and refuses the next.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_spill_cap_exact_fit_boundary_and_quarantine_not_counted() {
    let _serial = SERIAL.lock().await;
    let _ = recorder();
    let root = unique_root("aud-cap");
    let _cwd = CwdGuard {
        original: std::env::current_dir().expect("cwd"),
        root: root.clone(),
    };
    std::env::set_current_dir(&root).expect("enter temp root");
    let table = AuditSpillTable::OrderAudit;
    let name = table.table_name();
    let dir = root.join(table.default_dir());
    std::fs::create_dir_all(&dir).expect("audit dir");
    let spill = AuditSpill::for_table(table);
    const ROWS: usize = 3;
    let payload = audit_batch(0, ROWS, false);
    let len = payload.len() as u64;
    let refused = || counter("tv_audit_spill_refused_rows_total", &[("table", name)]);
    let filler = dir.join("00000000000000000001-1-0000000000-1.ilp.tmp");
    let set_filler = |size: u64| {
        std::fs::File::create(&filler)
            .and_then(|f| f.set_len(size))
            .expect("sparse filler");
    };
    let clear_waiting = || {
        for entry in std::fs::read_dir(&dir).expect("list").flatten() {
            if entry.file_type().is_ok_and(|t| t.is_file()) {
                std::fs::remove_file(entry.path()).expect("remove");
            }
        }
    };

    // Exactly fits: filler + batch == the cap.
    set_filler(AUDIT_SPILL_MAX_BYTES_PER_TABLE - len);
    let before = refused();
    assert!(
        spill_failed_batch(Some(&spill), &payload, ROWS),
        "a batch that brings the directory to exactly the cap is accepted"
    );
    assert_eq!(refused(), before);
    clear_waiting();

    // One byte more: refused, counted by exactly its rows.
    set_filler(AUDIT_SPILL_MAX_BYTES_PER_TABLE - len + 1);
    let before = refused();
    assert!(
        !spill_failed_batch(Some(&spill), &payload, ROWS),
        "one byte over the cap is refused"
    );
    assert_eq!(
        refused() - before,
        ROWS as u64,
        "counted by exactly its rows"
    );
    clear_waiting();

    // A cap-sized file in quarantine/ does not count.
    let q = dir.join(QUARANTINE_DIR);
    std::fs::create_dir_all(&q).expect("quarantine dir");
    std::fs::File::create(q.join("00000000000000000002-1-0000000000-1.ilp"))
        .and_then(|f| f.set_len(AUDIT_SPILL_MAX_BYTES_PER_TABLE))
        .expect("sparse quarantined file");
    let before = refused();
    assert!(
        spill_failed_batch(Some(&spill), &payload, ROWS),
        "quarantine/ never counts toward the cap"
    );
    assert_eq!(refused(), before);
    clear_waiting();

    // The file-count cap: AUDIT_SPILL_MAX_FILES_PER_TABLE - 1 empty files,
    // then the 50,000th (a real spill) is accepted and the next refused.
    for n in 0..AUDIT_SPILL_MAX_FILES_PER_TABLE - 1 {
        std::fs::File::create(dir.join(format!("filler-{n:06}.tmp"))).expect("filler file");
    }
    let before = refused();
    assert!(
        spill_failed_batch(Some(&spill), &payload, ROWS),
        "the 50,000th file is accepted"
    );
    assert!(
        !spill_failed_batch(Some(&spill), &payload, ROWS),
        "the 50,001st file is refused"
    );
    assert_eq!(refused() - before, ROWS as u64);
}
