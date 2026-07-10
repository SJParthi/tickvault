//! DHAT allocation test for the B6 off-thread tick flush offload
//! (operator directive 2026-07-03).
//!
//! Separate binary because DHAT allows only one profiler per process.
//! Verifies Principle #1 on the offloaded hot path: after warm-up (the
//! pool buffers reach their high-water capacity and `clear()` retains it),
//! steady-state `append_tick_with_seq` calls BELOW the flush threshold are
//! ZERO-allocation — the ILP row build reuses the pooled `Buffer`, the
//! in-flight mirror reuses its pre-allocated `Vec`, and the batch handoff
//! itself is a `mem::replace` + `try_recv` + `try_send` (no heap).
//!
//! HONESTY NOTE: dhat profiles the GLOBAL allocator across ALL threads, so
//! a flush-crossing window cannot assert zero — the worker thread's questdb
//! `sender.flush` may allocate internally and would race into the measured
//! window. The measured window therefore stays strictly below the batch
//! threshold, with a settle-sleep after warm-up so no worker flush overlaps
//! it. The flush-crossing SWAP cost is covered by the Criterion bench
//! `composite/quote_tick_compute_only` (10µs budget) instead.

use std::io::Read as _;

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::TICK_FLUSH_BATCH_SIZE;
use tickvault_common::tick_types::ParsedTick;
use tickvault_storage::tick_persistence::TickPersistenceWriter;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

mod dhat_support;

fn sample_tick(security_id: u64, ltp: f32) -> ParsedTick {
    ParsedTick {
        security_id,
        exchange_segment_code: 0,
        last_traded_price: ltp,
        last_trade_quantity: 0,
        exchange_timestamp: 1_770_000_000,
        received_at_nanos: 1_770_000_000_000_000_000,
        average_traded_price: ltp,
        volume: 0,
        total_sell_quantity: 100,
        total_buy_quantity: 200,
        day_open: ltp,
        day_close: ltp,
        day_high: ltp,
        day_low: ltp,
        open_interest: 0,
        oi_day_high: 0,
        oi_day_low: 0,
        iv: f64::NAN,
        delta: f64::NAN,
        gamma: f64::NAN,
        theta: f64::NAN,
        vega: f64::NAN,
    }
}

/// Multi-accept TCP drain (the writer AND its flush worker each open one
/// ILP connection). Returns `None` when the sandbox forbids loopback.
fn spawn_tcp_drain() -> Option<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { return };
            std::thread::spawn(move || {
                let mut sink = [0_u8; 65_536];
                while let Ok(n) = s.read(&mut sink) {
                    if n == 0 {
                        break;
                    }
                }
            });
        }
    });
    Some(port)
}

#[test]
fn dhat_offloaded_append_steady_state_zero_alloc() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let Some(port) = spawn_tcp_drain() else {
        return; // sandbox forbids loopback sockets — skip honestly
    };
    let cfg = QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: 0,
        pg_port: 0,
        ilp_port: port,
    };
    let Ok(mut writer) = TickPersistenceWriter::new(&cfg) else {
        return;
    };

    // Warm-up: cycle EVERY pool buffer through a full batch so each reaches
    // its high-water capacity (clear() retains it) — 5 full batches covers
    // the 3 spares + the writer's active buffer with margin.
    // RefCell: the phantom-retry helper takes a `stabilize` AND a `workload`
    // closure; both need the writer + seq mutably, so interior mutability
    // resolves the (strictly sequential) double borrow.
    let writer = std::cell::RefCell::new(writer);
    let seq = std::cell::Cell::new(1_i64);
    for i in 0..(5 * TICK_FLUSH_BATCH_SIZE) {
        seq.set(seq.get() + 1);
        let tick = sample_tick(13, 23_000.0 + (i % 100) as f32);
        let _ = writer.borrow_mut().append_tick_with_seq(&tick, seq.get());
    }
    // Settle: let the worker finish any in-flight flush so its internal
    // allocations cannot race into the measured window.
    std::thread::sleep(std::time::Duration::from_millis(300));

    // 2026-07-09: measured via the bounded phantom-retry helper (roaming
    // 4-block cross-thread flake on 2-core CI runners; this binary ALSO
    // runs a flush-worker thread + TCP drain threads — see
    // dhat_support/mod.rs). Budget UNCHANGED: exactly 0 blocks.
    //
    // Stateful stabilize: each measured attempt appends BATCH-1 ticks, so a
    // RE-attempt must first top up the ONE remaining slot (triggering the
    // batch dispatch off the measured clock) and settle until the worker's
    // flush is done — restoring the strictly-below-threshold invariant the
    // measured window depends on.
    let (_, blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {
            seq.set(seq.get() + 1);
            let tick = sample_tick(13, 23_050.0);
            let _ = writer.borrow_mut().append_tick_with_seq(&tick, seq.get());
            std::thread::sleep(std::time::Duration::from_millis(300));
        },
        || {
            // Measured window: strictly below the flush threshold — no dispatch,
            // no worker activity. This is the per-tick steady state.
            for i in 0..(TICK_FLUSH_BATCH_SIZE - 1) {
                seq.set(seq.get() + 1);
                let tick = sample_tick(13, 23_100.0 + (i % 100) as f32);
                let _ = writer.borrow_mut().append_tick_with_seq(&tick, seq.get());
            }
        },
    );

    assert_eq!(
        blocks,
        0,
        "offloaded steady-state append allocated {blocks} heap blocks over \
         {} calls — PRINCIPLE #1 VIOLATED. The pooled Buffer + pre-allocated \
         in-flight Vec must make below-threshold appends zero-allocation.",
        TICK_FLUSH_BATCH_SIZE - 1
    );
}
