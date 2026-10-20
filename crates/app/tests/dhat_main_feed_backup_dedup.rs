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
//! through every verdict arm (Accept, DropIdentical on the second copy, older
//! packets, aux pairs, packets outside the set) under the production metrics
//! recorder type, and holds 10,000 rounds to the house budget. The install is
//! cold (once per publication) and stays outside the measured window.
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

#[derive(Default)]
struct Seen {
    accept: u64,
    identical: u64,
    older_or_late: u64,
    not_backup: u64,
}

impl Seen {
    fn note(&mut self, v: BackupVerdict) {
        match v {
            BackupVerdict::Accept => self.accept += 1,
            BackupVerdict::DropIdentical => self.identical += 1,
            BackupVerdict::AcceptLate | BackupVerdict::DropOlder => self.older_or_late += 1,
            BackupVerdict::NotBackup => self.not_backup += 1,
        }
    }
}

/// One round per call step: socket 0's copy, socket 1's copy, an older packet,
/// an aux pair and a packet outside the set.
fn drive(dedup: &mut BackupDedup, seen: &mut Seen, round: &mut u32, rounds: u32) {
    for _ in 0..rounds {
        *round += 1;
        let r = *round;
        let id = u64::from(r % SET_SIZE) + 1;
        let volume = 1_000 + r * 10;
        let ltt = 1_700_000_000 + r;
        let now_ms = u64::from(r);
        let t = tick(id, volume, ltt);
        let p = packet(id, volume);
        dedup.on_frame(0, now_ms);
        dedup.on_frame(1, now_ms);
        seen.note(dedup.admit_tick(&t, &p, 0, now_ms));
        seen.note(dedup.admit_tick(&t, &p, 1, now_ms));
        let older = tick(id, volume - 5, ltt - 1);
        let older_p = packet(id, volume - 5);
        seen.note(dedup.admit_tick(&older, &older_p, 0, now_ms));
        seen.note(dedup.admit_aux(id, SEGMENT_FNO, &p[..12], 0));
        seen.note(dedup.admit_aux(id, SEGMENT_FNO, &p[..12], 1));
        let out = tick(OUTSIDE_SET, volume, ltt);
        seen.note(dedup.admit_tick(&out, &packet(OUTSIDE_SET, volume), 0, now_ms));
        std::hint::black_box(&p);
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

    // The window must have exercised every arm, or it proved nothing.
    assert!(measured.accept > 0, "no Accept in the window");
    assert!(measured.identical > 0, "no DropIdentical in the window");
    assert!(measured.older_or_late > 0, "no older packet in the window");
    assert!(
        measured.not_backup > 0,
        "no packet outside the set in the window"
    );
}
