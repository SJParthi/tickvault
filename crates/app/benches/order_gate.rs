//! Order-runtime dry-run PR (2026-07-14) — Criterion benchmarks for the
//! Groww-bridge mark-forward tap (`order_runtime::MarkForwarder`).
//!
//! Three targets, budget key `order_gate_mark_forward` in
//! `quality/benchmark-budgets.toml` (substring-matches all three ids):
//!   - `order_gate/mark_forward_disarmed` — the DOMINANT per-tick path
//!     (no positions / no pending paper orders): one `Relaxed` load.
//!   - `order_gate/mark_forward_armed_full` — the backpressure drop arm:
//!     bounded `try_send` against a full channel + static-key counter.
//!   - `order_gate/mark_forward_armed_accept` — the armed accepted arm
//!     (send/drain steady state — tokio mpsc block reuse).
//!
//! Companion zero-alloc evidence: `tests/dhat_mark_forward.rs`.
//!
//! Run: `cargo bench --bench order_gate -p tickvault-app`

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_app::order_runtime::MarkForwarder;

fn bench_mark_forward(c: &mut Criterion) {
    // Disarmed: the dominant per-tick cost — a single Relaxed load.
    {
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let forwarder = MarkForwarder {
            marks_wanted: Arc::new(AtomicBool::new(false)),
            tx,
        };
        c.bench_function("order_gate/mark_forward_disarmed", |b| {
            let mut i: u32 = 0;
            b.iter(|| {
                i = i.wrapping_add(37);
                forwarder.mark_forward(
                    black_box(u64::from(i % 8)),
                    black_box((i % 3) as u8),
                    black_box(100.5),
                );
            });
        });
    }

    // Armed + full channel: the backpressure drop arm (counted, never
    // blocking). Channel filled once; every iteration takes the Full path.
    {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let forwarder = MarkForwarder {
            marks_wanted: Arc::new(AtomicBool::new(true)),
            tx,
        };
        for i in 0..4_u32 {
            forwarder.mark_forward(u64::from(i), 0, 1.0);
        }
        c.bench_function("order_gate/mark_forward_armed_full", |b| {
            let mut i: u32 = 0;
            b.iter(|| {
                i = i.wrapping_add(37);
                forwarder.mark_forward(
                    black_box(u64::from(i % 8)),
                    black_box((i % 3) as u8),
                    black_box(100.5),
                );
            });
        });
    }

    // Armed + accepted: send/drain steady state (a same-thread try_recv
    // keeps depth ~0 so tokio's block list is reused, mirroring the live
    // runtime's batch-drain consumer).
    {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8192);
        let forwarder = MarkForwarder {
            marks_wanted: Arc::new(AtomicBool::new(true)),
            tx,
        };
        c.bench_function("order_gate/mark_forward_armed_accept", |b| {
            let mut i: u32 = 0;
            b.iter(|| {
                i = i.wrapping_add(37);
                forwarder.mark_forward(
                    black_box(u64::from(i % 8)),
                    black_box((i % 3) as u8),
                    black_box(100.5),
                );
                let _ = black_box(rx.try_recv());
            });
        });
    }
}

criterion_group!(benches, bench_mark_forward);
criterion_main!(benches);
