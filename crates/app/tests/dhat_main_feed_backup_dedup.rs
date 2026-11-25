//! DHAT gate for the backup-copy dedup on the main-feed drain.
//!
//! The backup copy is ON by default (`[dhan_universe] backup_top_n = 1000`),
//! so `BackupDedup::admit_tick` / `admit_aux` run on every packet of the
//! backed-up option legs. Their docs say "O(1), zero allocation", but until
//! 2026-10-20 nothing measured it: `dhat_live_ingest_seam` never publishes a
//! set, so it only ever timed the early return of an idle table (finding M2
//! of the 2026-10-20 hot-path review).
//!
//! This test installs a real set, then drives two sockets' worth of traffic
//! through every verdict arm (Accept, AcceptLate, DropIdentical from both the
//! main and the late ring, DropOlder for a replayed frame, the aux pair, and a
//! packet outside the set) under the production metrics recorder type, and
//! holds 10,000 rounds to the house budget. Each step asserts its own expected
//! verdict, so a path that stops being reached fails the test instead of
//! passing vacuously. The install is cold (once per publication) and stays
//! outside the measured window.
//!
//! Run: `cargo test -p tickvault-app --test dhat_main_feed_backup_dedup`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use metrics_exporter_prometheus::PrometheusBuilder;
use tickvault_app::main_feed_backup::{BackupDedup, BackupVerdict};
use tickvault_common::tick_types::ParsedTick;

mod dhat_support;

const BUDGET_BYTES: u64 = 1024;
const BUDGET_BLOCKS: u64 = 8;

/// NSE_FNO binary segment code.
const SEGMENT_FNO: u8 = 2;
/// Contracts in the backed-up set.
const SET_SIZE: u32 = 1_000;
/// A contract outside the set.
const OUTSIDE_SET: u64 = 9_999_999;
/// Quote packet length; the fingerprint reads whatever bytes it is given.
const PACKET_LEN: usize = 50;
const ROUNDS: u32 = 10_000;

fn tick(security_id: u64, volume: u32, ltt: u32) -> ParsedTick {
    ParsedTick {
        security_id,
        exchange_segment_code: SEGMENT_FNO,
        last_traded_price: 101.5,
        exchange_timestamp: ltt,
        volume,
        ..Default::default()
    }
}

/// Packet bytes on the stack, distinct per (contract, volume).
fn packet(security_id: u64, volume: u32) -> [u8; PACKET_LEN] {
    let mut p = [0u8; PACKET_LEN];
    p[0] = 4;
    p[4..12].copy_from_slice(&security_id.to_le_bytes());
    p[22..26].copy_from_slice(&volume.to_le_bytes());
    p
}

/// The steps of one round, each with the verdict it must produce.
#[derive(Clone, Copy)]
enum Step {
    /// Socket 1, cumulative `a`: new, accepted.
    FirstOnOne,
    /// Socket 1, cumulative `c` > `a`: newest, accepted.
    NewestOnOne,
    /// Socket 0, `a` < `b` < `c`: older than the newest, but socket 1's ring
    /// entry `a` proves socket 1 never sent it, so it is kept (AcceptLate).
    LateOnZero,
    /// Socket 1's copy of `b`: found in the late ring, dropped.
    LateCopyOnOne,
    /// Socket 0's copy of `c`: found in the main ring, dropped.
    NewestCopyOnZero,
    /// A replayed frame (no socket id) older than everything: dropped as older.
    ReplayedOlder,
    /// Aux packet, first copy.
    AuxFirst,
    /// Aux packet, the other socket's identical copy.
    AuxCopy,
    /// A contract outside the backed-up set.
    Outside,
}

const STEPS: usize = 9;

fn expected(step: Step) -> BackupVerdict {
    match step {
        Step::FirstOnOne | Step::NewestOnOne | Step::AuxFirst => BackupVerdict::Accept,
        Step::LateOnZero => BackupVerdict::AcceptLate,
        Step::LateCopyOnOne | Step::NewestCopyOnZero | Step::AuxCopy => {
            BackupVerdict::DropIdentical
        }
        Step::ReplayedOlder => BackupVerdict::DropOlder,
        Step::Outside => BackupVerdict::NotBackup,
    }
}

/// Per-step hit counts and the count of verdicts that differed from the
/// expected one. Fixed arrays: counting allocates nothing.
#[derive(Default)]
struct Seen {
    hits: [u64; STEPS],
    wrong: u64,
}

impl Seen {
    fn note(&mut self, step: Step, v: BackupVerdict) {
        if v == expected(step) {
            self.hits[step as usize] += 1;
        } else {
            self.wrong += 1;
        }
    }
}

/// The connection index a replayed WAL frame carries: no socket.
const REPLAYED: u8 = u8::MAX;

/// One round per contract visit, every verdict arm once.
fn drive(dedup: &mut BackupDedup, seen: &mut Seen, round: &mut u32, rounds: u32) {
    for _ in 0..rounds {
        *round += 1;
        let r = *round;
        let id = u64::from(r % SET_SIZE) + 1;
        let (a, b, c) = (1_000 + r * 10, 1_003 + r * 10, 1_006 + r * 10);
        let ltt = 1_700_000_000 + r * 10;
        let now_ms = u64::from(r);
        let (pa, pb, pc) = (packet(id, a), packet(id, b), packet(id, c));
        dedup.on_frame(0, now_ms);
        dedup.on_frame(1, now_ms);
        let mut admit = |step: Step, cum: u32, dt: u32, p: &[u8], conn: u8| {
            let v = dedup.admit_tick(&tick(id, cum, ltt + dt), p, conn, now_ms);
            seen.note(step, v);
        };
        admit(Step::FirstOnOne, a, 0, &pa, 1);
        admit(Step::NewestOnOne, c, 6, &pc, 1);
        admit(Step::LateOnZero, b, 3, &pb, 0);
        admit(Step::LateCopyOnOne, b, 3, &pb, 1);
        admit(Step::NewestCopyOnZero, c, 6, &pc, 0);
        let po = packet(id, a - 1);
        admit(Step::ReplayedOlder, a - 1, 0, &po, REPLAYED);
        let aux = &pc[..26];
        seen.note(Step::AuxFirst, dedup.admit_aux(id, SEGMENT_FNO, aux, 0));
        seen.note(Step::AuxCopy, dedup.admit_aux(id, SEGMENT_FNO, aux, 1));
        let out = tick(OUTSIDE_SET, c, ltt);
        let v = dedup.admit_tick(&out, &packet(OUTSIDE_SET, c), 0, now_ms);
        seen.note(Step::Outside, v);
        std::hint::black_box(&pc);
    }
}

#[test]
fn dhat_backup_dedup_admits_and_drops_without_allocating() {
    // The production recorder type, installed first, so every counter the
    // dedup increments is a real handle (a handle resolved before a recorder
    // exists is a no-op forever, which would hide an allocation).
    let recorder = PrometheusBuilder::new().build_recorder();
    metrics::set_global_recorder(recorder).expect("no recorder installed yet");

    let set: Vec<(u64, u8)> = (1..=u64::from(SET_SIZE))
        .map(|id| (id, SEGMENT_FNO))
        .collect();
    let mut dedup = BackupDedup::new();
    dedup.install(&set);
    assert_eq!(dedup.len(), SET_SIZE as usize);

    // Warm-up outside the window: first touch of every slot and counter
    // handle registration are one-time costs.
    let mut round: u32 = 0;
    let mut warm = Seen::default();
    drive(&mut dedup, &mut warm, &mut round, 2 * SET_SIZE);

    let _profiler = dhat::Profiler::builder().testing().build();
    let mut measured = Seen::default();
    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || drive(&mut dedup, &mut measured, &mut round, ROUNDS),
    );
    assert!(
        bytes <= BUDGET_BYTES && blocks <= BUDGET_BLOCKS,
        "BackupDedup allocated {bytes} bytes / {blocks} blocks over {ROUNDS} rounds of \
         two-socket traffic (budget {BUDGET_BYTES} B / {BUDGET_BLOCKS} blocks). It runs on \
         every packet of the backed-up legs on the frame drain; anything that allocates \
         per packet there (a Vec, a String, a labelled metrics macro) must move out."
    );

    // Every step must have produced exactly its expected verdict, on every
    // round, or the window measured a different path than it claims.
    assert_eq!(
        warm.wrong + measured.wrong,
        0,
        "a step produced an unexpected verdict (warm-up per-step hits {:?}, measured {:?})",
        warm.hits,
        measured.hits
    );
    for (i, hits) in measured.hits.iter().enumerate() {
        assert!(*hits > 0, "step {i} never ran in the measured window");
    }
}
