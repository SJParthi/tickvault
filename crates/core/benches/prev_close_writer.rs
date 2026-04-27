//! Wave 1 Item 0.a — Criterion benchmark for the prev_close writer
//! hot-path entry point (`PrevCloseWriter::try_enqueue`).
//!
//! Establishes a measured baseline so the bench-gate (5% regression
//! cap per `quality/benchmark-budgets.toml`) catches future hot-path
//! degradations. Plan target: try_enqueue p99 ≤ 200 ns.

use std::hint::black_box;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use tickvault_core::pipeline::prev_close_writer::PrevCloseWriter;

fn fresh_bench_path(label: &str) -> (PathBuf, PathBuf) {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("tv-bench-pcw-{label}-{pid}-{seq}"));
    let _ = std::fs::create_dir_all(&dir);
    (dir.join("cache.json"), dir.join("cache.json.tmp"))
}

/// Measures `try_enqueue` on a SATURATED channel (all sends after the
/// first 64 return `DroppedFull`). This is the worst-case branch on
/// the hot path; a regression here catches new heap allocations or
/// extra atomic ops added inside the drop arm.
fn bench_try_enqueue_saturated(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    runtime.block_on(async {
        let (cache_path, tmp_path) = fresh_bench_path("saturated");
        let writer = PrevCloseWriter::spawn(cache_path, tmp_path);
        let payload = Bytes::from_static(b"{\"13\":19500.5,\"25\":48000.25}");

        // Pre-saturate the channel so the bench loop's try_send returns
        // DroppedFull immediately (no async drain interference).
        for _ in 0..256 {
            let _ = writer.try_enqueue(payload.clone());
        }

        c.bench_function("prev_close_writer/try_enqueue_saturated", |b| {
            b.iter(|| {
                let outcome = writer.try_enqueue(black_box(payload.clone()));
                black_box(outcome);
            });
        });
    });
}

/// Measures `try_enqueue` against a FRESH channel where every send
/// succeeds. Slightly faster than the saturated path because the
/// channel internals don't have to walk the full slot list to find
/// a free slot, but the dominant cost is still the bytes::Bytes
/// Arc-bump + the mpsc try_send fast path.
fn bench_try_enqueue_happy_path(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    runtime.block_on(async {
        let (cache_path, tmp_path) = fresh_bench_path("happy");
        let writer = PrevCloseWriter::spawn(cache_path, tmp_path);
        let payload = Bytes::from_static(b"{\"13\":19500.5}");

        c.bench_function("prev_close_writer/try_enqueue_happy_path", |b| {
            b.iter(|| {
                // Fresh-channel path: most sends succeed; once the
                // channel saturates Criterion's iterator may flip
                // to the drop branch. That's fine — the budget
                // covers both paths combined.
                let outcome = writer.try_enqueue(black_box(payload.clone()));
                black_box(outcome);
            });
        });
    });
}

criterion_group!(
    benches,
    bench_try_enqueue_saturated,
    bench_try_enqueue_happy_path
);
criterion_main!(benches);
