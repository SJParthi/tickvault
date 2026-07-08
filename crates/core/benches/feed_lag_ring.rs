//! Silent-feed hardening Item 4 — Criterion benchmarks for the Dhan
//! exchange-lag monitor.
//!
//! Two targets, two honesty tiers:
//! - `feed_lag/record_dhan_tick` — the HOT-PATH ring write (O(1),
//!   zero-alloc). Budget: `feed_lag_record_dhan_tick` in
//!   `quality/benchmark-budgets.toml` (≤ 100 ns).
//! - `feed_lag/p99_window_full_ring` — the COLD-PATH publisher p99 over a
//!   full 32,768-sample window via `select_nth_unstable`. This is
//!   **O(N-window), NOT O(1)** — it runs on a 10 s supervised task off the
//!   tick thread. Budget: `feed_lag_p99_window` (cold, ≤ 5 ms).
//!
//! Run: `cargo bench --bench feed_lag_ring -p tickvault-core`

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_core::pipeline::feed_lag_monitor::{compute_window_p99_ns, record_dhan_tick};

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Steady-state hot-path ring write on the process-global ring (the exact
/// production entry point: two-condition replay check (boundary + dwell) +
/// clock alignment + two relaxed stores + head bump). HONEST COVERAGE:
/// like the DHAT ratchet, this
/// measures the `Admitted { clamped: false }` steady-state arm only — the
/// metrics-emitting `ExcludedReplay` / clamped arms are not in the loop.
fn bench_record_dhan_tick(c: &mut Criterion) {
    // 2026-07-06 ~10:00 IST as UTC nanos (2026-07-06 00:00 UTC =
    // 1_783_296_000 epoch secs).
    let t0_utc_secs: i64 = 1_783_296_000 + 4 * 3600 + 1800;
    let t0_utc_nanos: i64 = t0_utc_secs * NANOS_PER_SEC;
    let exchange_ist_secs: u32 = u32::try_from(t0_utc_secs + 19_800).unwrap_or(u32::MAX);

    // Pre-init the global ring outside the measured loop.
    record_dhan_tick(t0_utc_nanos, t0_utc_nanos - 1_000_000, exchange_ist_secs);

    c.bench_function("feed_lag/record_dhan_tick", |b| {
        let mut i: i64 = 0;
        b.iter(|| {
            i = i.wrapping_add(1_000_000);
            let recv = t0_utc_nanos + i;
            record_dhan_tick(
                black_box(recv),
                black_box(recv - 1_000_000),
                black_box(exchange_ist_secs),
            );
        });
    });
}

/// Cold-path p99 over a full-capacity window — honestly O(N), N = 32,768.
/// `select_nth_unstable` reorders in place, so each iteration re-clones the
/// pre-built window into a reused scratch buffer (the clone cost is part of
/// the measured loop but is itself O(N) memcpy, consistent with the cold
/// budget).
fn bench_p99_window_full_ring(c: &mut Criterion) {
    let window: Vec<u64> = (0..32_768u64)
        .map(|i| (i % 200) * NANOS_PER_SEC as u64 / 100)
        .collect();
    let mut scratch: Vec<u64> = Vec::with_capacity(window.len());

    c.bench_function("feed_lag/p99_window_full_ring", |b| {
        b.iter(|| {
            scratch.clear();
            scratch.extend_from_slice(&window);
            black_box(compute_window_p99_ns(black_box(&mut scratch)));
        });
    });
}

criterion_group!(benches, bench_record_dhan_tick, bench_p99_window_full_ring);
criterion_main!(benches);
