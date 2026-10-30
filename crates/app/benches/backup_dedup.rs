//! Criterion benchmarks for the main-feed backup dedup's per-packet call
//! (plan ITEM DT, side stores, 2026-10-30).
//!
//! `BackupDedup::admit_tick` runs on the frame drain for every main-feed tick
//! packet (`drain_main_feed_frame`), before the fold. Every case runs against
//! a set FULL at `BACKUP_RANK_MAX` (5,000 contracts, the largest set the
//! publisher builds); packets are 162-byte Full packets, the size the
//! fingerprint pays for in production.
//!
//! Targets (budget key `backup_dedup` in `quality/benchmark-budgets.toml`):
//!   - `backup_dedup/admit_tick_inactive`: no set published (one bool test).
//!   - `backup_dedup/admit_tick_not_backup`: a set is published, the packet's
//!     contract is not in it (the pre-filter, plus a hash probe on a
//!     false positive).
//!   - `backup_dedup/admit_tick_accept`: a backup contract's newer packet
//!     from one socket (fingerprint, ring scan, ring insert).
//!   - `backup_dedup/admit_tick_pair`: the first copy from socket 0, then the
//!     byte-identical second copy from socket 1 (dropped). Two calls per
//!     iteration; `[elements]` divides by 2, so the budget is per packet.
//!   - `backup_dedup/admit_tick_older`: an older packet the ring cannot place
//!     (dropped as older after the late-ring and `ring_covers` scans).
//!
//! After Criterion, `admit_tick_not_backup`, `admit_tick_accept` and
//! `admit_tick_older` are timed one call at a time and a p99 over the budget
//! exits 1 (`../../core/benches/support/tail_gate.rs`). Only under
//! `cargo bench` (`--bench`).

#![allow(clippy::unwrap_used)] // APPROVED: bench harness
#![allow(clippy::expect_used)] // APPROVED: bench harness
#![allow(missing_docs)] // APPROVED: criterion_group! macro

use std::hint::black_box;

#[path = "../../core/benches/support/tail_gate.rs"]
mod tail_gate;

use criterion::{Criterion, criterion_group};
use tickvault_app::main_feed_backup::{BACKUP_RANK_MAX, BackupDedup, BackupVerdict};
use tickvault_common::tick_types::ParsedTick;

/// Budget key in `quality/benchmark-budgets.toml`.
const BUDGET_KEY: &str = "backup_dedup";
/// NSE_FNO wire code.
const SEG: u8 = 2;
/// Full packet size on the wire.
const FULL_PACKET_BYTES: usize = 162;
const LTT: u32 = 1_761_818_400;

fn backup_sid(i: usize) -> u64 {
    50_000 + i as u64
}

fn full_dedup() -> BackupDedup {
    let set: Vec<(u64, u8)> = (0..BACKUP_RANK_MAX).map(|i| (backup_sid(i), SEG)).collect();
    let mut d = BackupDedup::new();
    d.install(&set);
    assert_eq!(d.len(), BACKUP_RANK_MAX);
    d
}

/// Contract and cumulative volume for call `n`: the contract walks the set,
/// and each contract's cumulative only rises.
fn tick_for(n: u64, sid: u64) -> ParsedTick {
    ParsedTick {
        security_id: sid,
        exchange_segment_code: SEG,
        last_traded_price: 100.0,
        exchange_timestamp: LTT + (n / BACKUP_RANK_MAX as u64) as u32,
        volume: 1_000 + (n / BACKUP_RANK_MAX as u64) as u32,
        ..ParsedTick::default()
    }
}

/// Writes the fields that differ into a reused 162-byte packet buffer.
fn fill(packet: &mut [u8; FULL_PACKET_BYTES], t: &ParsedTick) {
    packet[4..8].copy_from_slice(&(t.security_id as u32).to_le_bytes());
    packet[22..26].copy_from_slice(&t.volume.to_le_bytes());
    packet[14..18].copy_from_slice(&t.exchange_timestamp.to_le_bytes());
}

fn admit(
    d: &mut BackupDedup,
    packet: &mut [u8; FULL_PACKET_BYTES],
    t: &ParsedTick,
    conn: u8,
) -> BackupVerdict {
    fill(packet, t);
    d.admit_tick(black_box(t), black_box(&packet[..]), conn, 1_000)
}

fn not_backup(d: &mut BackupDedup, packet: &mut [u8; FULL_PACKET_BYTES], n: u64) -> BackupVerdict {
    // Ids outside the set; the pre-filter rejects most, a false positive
    // pays the hash probe, as in production.
    let t = tick_for(n, 1_000_000 + (n % 20_000));
    admit(d, packet, &t, 0)
}

fn accept(d: &mut BackupDedup, packet: &mut [u8; FULL_PACKET_BYTES], n: u64) -> BackupVerdict {
    let t = tick_for(n, backup_sid((n as usize) % BACKUP_RANK_MAX));
    admit(d, packet, &t, 0)
}

fn older(d: &mut BackupDedup, packet: &mut [u8; FULL_PACKET_BYTES], n: u64) -> BackupVerdict {
    let mut t = tick_for(0, backup_sid((n as usize) % BACKUP_RANK_MAX));
    // Below the seeded newest, and a different book so it copies nothing.
    t.volume = 10;
    packet[100..108].copy_from_slice(&n.to_le_bytes());
    admit(d, packet, &t, 1)
}

/// A full set whose every contract has a newest packet from socket 0, at
/// cumulative 1,000 + `passes` (so `older` has a newest to be older than).
fn seeded_dedup(passes: u64) -> (BackupDedup, [u8; FULL_PACKET_BYTES]) {
    let mut d = full_dedup();
    let mut packet = [0u8; FULL_PACKET_BYTES];
    for n in 0..BACKUP_RANK_MAX as u64 * passes {
        assert_eq!(accept(&mut d, &mut packet, n), BackupVerdict::Accept);
    }
    (d, packet)
}

fn bench_backup_dedup(c: &mut Criterion) {
    let mut group = c.benchmark_group("backup_dedup");
    let mut packet = [0u8; FULL_PACKET_BYTES];
    let mut n = 0u64;

    let mut inactive = BackupDedup::new();
    group.bench_function("admit_tick_inactive", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(accept(&mut inactive, &mut packet, n))
        });
    });

    let mut d = full_dedup();
    group.bench_function("admit_tick_not_backup", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(not_backup(&mut d, &mut packet, n))
        });
    });

    let mut d = full_dedup();
    let mut a = 0u64;
    group.bench_function("admit_tick_accept", |b| {
        b.iter(|| {
            a = a.wrapping_add(1);
            black_box(accept(&mut d, &mut packet, a))
        });
    });

    let mut d = full_dedup();
    let mut p = 0u64;
    group.bench_function("admit_tick_pair", |b| {
        b.iter(|| {
            p = p.wrapping_add(1);
            let t = tick_for(p, backup_sid((p as usize) % BACKUP_RANK_MAX));
            let first = admit(&mut d, &mut packet, &t, 0);
            let second = d.admit_tick(black_box(&t), black_box(&packet[..]), 1, 1_000);
            black_box((first, second))
        });
    });

    let (mut d, mut seeded_packet) = seeded_dedup(2);
    group.bench_function("admit_tick_older", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            black_box(older(&mut d, &mut seeded_packet, n))
        });
    });

    group.finish();
}

fn tail_gate() -> Result<(), String> {
    let mut gate = tail_gate::Gate::new("backup_dedup", BUDGET_KEY)?;
    let mut packet = [0u8; FULL_PACKET_BYTES];
    let mut d = full_dedup();
    gate.case("admit_tick_not_backup", |n| {
        black_box(not_backup(&mut d, &mut packet, n)) == BackupVerdict::NotBackup
    });
    let mut d = full_dedup();
    gate.case("admit_tick_accept", |n| {
        black_box(accept(&mut d, &mut packet, n)) == BackupVerdict::Accept
    });
    let (mut d, mut seeded_packet) = seeded_dedup(2);
    gate.case("admit_tick_older", |n| {
        black_box(older(&mut d, &mut seeded_packet, n)) == BackupVerdict::DropOlder
    });
    gate.finish()
}

criterion_group!(benches, bench_backup_dedup);

fn main() {
    benches();
    Criterion::default().configure_from_args().final_summary();
    tail_gate::run_under_bench("backup_dedup", tail_gate);
}
