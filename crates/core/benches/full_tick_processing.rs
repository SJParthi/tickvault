//! Benchmark: per-component breakdown of the tick-processor hot loop.
//!
//! The live `tv_tick_processing_duration_ns` histogram wraps the region
//! `tick_processor.rs:932` (`tick_start = Instant::now()` after recv) →
//! `:1646` (`m_tick_duration.record(tick_start.elapsed())`). This bench
//! measures each component inside that region in isolation, plus a
//! composite chain, plus the REAL `TickPersistenceWriter::append_tick_with_seq`
//! (including its amortized 1-in-1000 `force_flush()` TCP write) against a
//! local TCP drain listener — ILP/TCP V1 is handshake-free, so the listener
//! only needs to read-and-discard.
//!
//! Components NOT separately benched (private to `tick_processor.rs`):
//! the dedup ring (`TickDedupRing::is_duplicate` — FNV-class hash + ring
//! compare, same cost class as `ilp/payload_hash` below) and the window
//! filter helpers (pure modulo + range compares, single-digit ns). The
//! composite-vs-sum residual bounds their combined contribution.
//!
//! Run: `cargo bench -p tickvault-core --bench full_tick_processing`

use std::hint::black_box;
use std::io::Read;
use std::sync::atomic::{AtomicI64, Ordering};

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{QUOTE_PACKET_SIZE, RESPONSE_CODE_QUOTE};
use tickvault_common::tick_types::ParsedTick;
use tickvault_core::parser::dispatcher::dispatch_frame;
// TickGapDetector import removed in PR-C3 (2026-07-14) — the detector
// module was deleted with the Dhan WS lane (operator Q4-ii 2026-07-13).
use tickvault_core::pipeline::tick_enricher::TickEnricher;
use tickvault_core::pipeline::volume_monotonicity_guard::VolumeMonotonicityGuard;
use tickvault_storage::tick_persistence::TickPersistenceWriter;
use tickvault_storage::tick_persistence_testing::build_tick_row_seq_pub;

/// Mirror of the live loop's flush batch size (constants.rs
/// `TICK_FLUSH_BATCH_SIZE = 1000`) for the buffer-clear amortization in
/// the row-build bench.
const ROW_BUILD_CLEAR_EVERY: usize = 1000;

/// Installs the SAME recorder family production uses
/// (`metrics-exporter-prometheus`) so counter/histogram costs are measured
/// against real atomic registry ops, not the no-op recorder. Returns a
/// handle whose `render()` drains the histogram sample buckets — callers
/// MUST drain after each histogram-recording bench group, because without
/// a scraper the buckets grow unbounded (hostile-review HIGH #1: ~10⁸
/// samples per 5s Criterion run ≈ multi-GB if never drained; production
/// drains on every scrape).
fn install_prometheus_recorder() -> &'static metrics_exporter_prometheus::PrometheusHandle {
    static HANDLE: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    HANDLE.get_or_init(|| {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        // Ignore AlreadySet — another bench group may have won the race.
        drop(metrics::set_global_recorder(recorder));
        handle
    })
}

/// A realistic in-window Quote-mode tick (NIFTY spot shape).
fn sample_tick() -> ParsedTick {
    ParsedTick {
        security_id: 13,
        exchange_segment_code: 0,
        last_traded_price: 23_146.45,
        last_trade_quantity: 0,
        exchange_timestamp: 1_770_000_000,
        received_at_nanos: 1_770_000_000_000_000_000,
        average_traded_price: 23_140.10,
        volume: 1_234_567,
        total_sell_quantity: 100,
        total_buy_quantity: 200,
        day_open: 23_100.0,
        day_close: 23_050.0,
        day_high: 23_200.0,
        day_low: 23_000.0,
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

/// Builds a valid 50-byte Quote binary packet (same shape as the pipeline
/// bench).
fn build_quote_packet(security_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; QUOTE_PACKET_SIZE];
    buf[0] = RESPONSE_CODE_QUOTE;
    #[allow(clippy::cast_possible_truncation)] // APPROVED: 50 fits u16
    let len = QUOTE_PACKET_SIZE as u16;
    buf[1..3].copy_from_slice(&len.to_le_bytes());
    buf[3] = 0; // IDX_I
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    // LTP (bytes 8-11) — non-zero finite so validity gates pass.
    buf[8..12].copy_from_slice(&23_146.45_f32.to_le_bytes());
    // LTT (bytes 14-17) — in-window IST epoch seconds.
    buf[14..18].copy_from_slice(&1_770_000_000_u32.to_le_bytes());
    buf
}

fn bench_clock_reads(c: &mut Criterion) {
    c.bench_function("clock/chrono_utc_now_nanos", |b| {
        b.iter(|| black_box(chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)));
    });
    c.bench_function("clock/chrono_utc_now_secs", |b| {
        b.iter(|| black_box(chrono::Utc::now().timestamp()));
    });
    c.bench_function("clock/instant_now", |b| {
        b.iter(|| black_box(std::time::Instant::now()));
    });
}

fn bench_parse(c: &mut Criterion) {
    let packet = build_quote_packet(13);
    c.bench_function("parse/dispatch_quote", |b| {
        b.iter(|| {
            let _ = black_box(dispatch_frame(black_box(&packet), black_box(1_000_000)));
        });
    });
}

// `bench_gap_detector` removed in PR-C3 (2026-07-14) with the deleted
// tick-gap detector module (operator Q4-ii 2026-07-13).

fn bench_enricher(c: &mut Criterion) {
    let enricher = TickEnricher::new();
    let tick = sample_tick();
    let secs_of_day = tick.exchange_timestamp % 86_400;
    c.bench_function("enricher/enrich_tick", |b| {
        b.iter(|| {
            let e = enricher.enrich_tick(black_box(&tick), black_box(secs_of_day));
            black_box(e.volume_delta);
        });
    });
}

fn bench_ilp_row_build(c: &mut Criterion) {
    let mut buffer = tickvault_storage::tick_persistence_testing::new_ilp_buffer_pub();
    let tick = sample_tick();
    let mut rows: usize = 0;
    let mut seq: i64 = 1;
    c.bench_function("ilp/build_tick_row_17_cols", |b| {
        b.iter(|| {
            seq = seq.wrapping_add(1);
            let _ = black_box(build_tick_row_seq_pub(
                &mut buffer,
                black_box(&tick),
                black_box(seq),
            ));
            rows += 1;
            // Mirror production: the buffer is drained (flushed) every
            // TICK_FLUSH_BATCH_SIZE rows; clear amortizes the same way.
            if rows >= ROW_BUILD_CLEAR_EVERY {
                buffer.clear();
                rows = 0;
            }
        });
    });
}

fn bench_payload_hash(c: &mut Criterion) {
    let tick = sample_tick();
    c.bench_function("ilp/payload_hash", |b| {
        b.iter(|| {
            black_box(tickvault_storage::tick_persistence::tick_payload_hash(
                black_box(&tick),
            ))
        });
    });
}

fn bench_broadcast_send(c: &mut Criterion) {
    let (sender, receiver) = tokio::sync::broadcast::channel::<ParsedTick>(262_144);
    // Keep one receiver alive (non-draining — ring overwrite, like a lagged
    // subscriber) so send() takes the success path as in production.
    let _keep_alive = receiver;
    let tick = sample_tick();
    c.bench_function("broadcast/send_one_lagging_receiver", |b| {
        b.iter(|| {
            let _ = black_box(sender.send(black_box(tick)));
        });
    });
}

fn bench_metrics_ops(c: &mut Criterion) {
    let handle = install_prometheus_recorder();
    let counter = metrics::counter!("bench_tv_counter_total");
    let histogram = metrics::histogram!("bench_tv_histogram_ns");
    c.bench_function("metrics/counter_increment", |b| {
        b.iter(|| counter.increment(1));
    });
    c.bench_function("metrics/histogram_record", |b| {
        let mut v = 0.0_f64;
        b.iter(|| {
            v += 1.0;
            histogram.record(black_box(v));
        });
    });
    // Drain the accumulated histogram samples (hostile-review HIGH #1).
    drop(handle.render());
}

/// Composite: every component of the live Tick arm chained in loop order
/// (parse → heartbeat → gap detector → canary scan → enricher → ILP row →
/// persisted counter → broadcast → monotonicity guard → 2 histogram records
/// + trailing clock read), with the production recorder installed. Pieces
/// missing vs the live loop (all small, bounded by the composite-vs-sum
/// residual): the private dedup ring + window filters, and the
/// `current_received_at_nanos()` strict-monotonic CAS bump (private —
/// the raw `Utc::now()` here under-counts it by one uncontended
/// load+CAS, ~10-20 ns).
fn bench_composite(c: &mut Criterion) {
    let handle = install_prometheus_recorder();
    let packet = build_quote_packet(13);
    let m_frames = metrics::counter!("bench_frames_total");
    let m_ticks = metrics::counter!("bench_ticks_total");
    let m_persisted = metrics::counter!("bench_persisted_total");
    let m_tick_duration = metrics::histogram!("bench_tick_duration_ns");
    let m_wire_to_done = metrics::histogram!("bench_wire_to_done_ns");
    let heartbeat = AtomicI64::new(0);
    let enricher = TickEnricher::new();
    let mut mono_guard = VolumeMonotonicityGuard::new();
    let mut buffer = tickvault_storage::tick_persistence_testing::new_ilp_buffer_pub();
    let (sender, receiver) = tokio::sync::broadcast::channel::<ParsedTick>(262_144);
    let _keep_alive = receiver;
    // `ParsedTick.security_id` is `u64` (2026-06-29 widening); mirrors the
    // production `CANARY_UNDERLYINGS: &[(u64, &str)]` in `tick_processor.rs`.
    let canary_sids: [u64; 3] = [13, 25, 51];
    let mut rows: usize = 0;
    let mut seq: i64 = 1;

    c.bench_function("composite/quote_tick_full_chain", |b| {
        b.iter(|| {
            let tick_start = std::time::Instant::now();
            m_frames.increment(1);
            let received_at_nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);

            let Ok(parsed) = dispatch_frame(black_box(&packet), received_at_nanos) else {
                return;
            };
            let tickvault_core::parser::ParsedFrame::Tick(tick) = parsed else {
                return;
            };
            m_ticks.increment(1);

            // Heartbeat — latency-hunt 2026-06-10: derived from
            // received_at_nanos (mirrors the post-cut production loop).
            heartbeat.store(
                received_at_nanos.saturating_div(1_000_000_000),
                Ordering::Relaxed,
            );

            // Gap-detector record site removed in PR-C3 (2026-07-14) —
            // mirrors the deleted tick_processor.rs production site.

            // Canary 3-element scan — received_at-derived seconds
            // (post-cut production loop).
            for sid in &canary_sids {
                if *sid == tick.security_id {
                    #[allow(clippy::cast_precision_loss)]
                    // APPROVED: epoch seconds are exactly representable in f64
                    {
                        black_box(received_at_nanos.saturating_div(1_000_000_000) as f64);
                    }
                    break;
                }
            }

            // Lifecycle enricher (loop line ~1200).
            let secs_of_day = tick.exchange_timestamp % 86_400;
            let enriched = enricher.enrich_tick(&tick, secs_of_day);
            black_box(enriched.volume_delta);

            // ILP row build + amortized batch clear (writer.append path).
            seq = seq.wrapping_add(1);
            let _ = build_tick_row_seq_pub(&mut buffer, &tick, seq);
            rows += 1;
            if rows >= ROW_BUILD_CLEAR_EVERY {
                buffer.clear();
                rows = 0;
            }
            m_persisted.increment(1);

            // Broadcast fan-out (loop line ~1262).
            let _ = sender.send(tick);

            // Monotonicity guard (loop line ~1302).
            let _ = mono_guard.observe(tick.security_id, tick.exchange_segment_code, tick.volume);

            // Trailing histogram records + wire-to-done clock (lines 1646-1653).
            #[allow(clippy::cast_precision_loss)] // APPROVED: same cast as live loop
            m_tick_duration.record(tick_start.elapsed().as_nanos() as f64);
            let wire_elapsed = chrono::Utc::now()
                .timestamp_nanos_opt()
                .unwrap_or(0)
                .saturating_sub(received_at_nanos);
            #[allow(clippy::cast_precision_loss)] // APPROVED: same cast as live loop
            m_wire_to_done.record(wire_elapsed.max(0) as f64);
        });
    });
    // Drain the accumulated histogram samples (hostile-review HIGH #1).
    drop(handle.render());
}

/// MEDIUM-7 (B6 adversarial round 1): the writer benches are (or gate)
/// bench-budget rows in `quality/benchmark-budgets.toml`. A silent early
/// return on loopback-bind failure would leave no Criterion estimate, so
/// `scripts/bench-gate.sh` would never evaluate the budget row and exit 0
/// — the gate self-disarms invisibly. A runner that cannot execute a
/// gated bench MUST fail loudly instead. (Benches are exempt from the
/// prod no-panic lints — the scanner excludes `/benches/`.)
fn bind_loopback_or_die(bench: &str) -> (std::net::TcpListener, std::net::SocketAddr) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| {
        panic!(
            "{bench}: cannot bind loopback TCP listener — the bench-gate budget \
             row would silently self-disarm; fix the runner (loopback sockets \
             required): {e}"
        )
    });
    let addr = listener.local_addr().unwrap_or_else(|e| {
        panic!("{bench}: cannot read loopback listener addr — fix the runner: {e}")
    });
    (listener, addr)
}

/// REAL writer against a local TCP drain: measures
/// `append_tick_with_seq` including the amortized 1-in-1000 batch handoff.
/// B6 (2026-07-03): the batch flush is now handed to the off-thread
/// tick-flush-worker (O(1) buffer swap) instead of a blocking inline TCP
/// write, so this bench measures the offloaded hot path — the residual
/// 1-in-1000 cost is the swap + channel send, not the flush I/O.
/// Panics loudly if the runner forbids loopback sockets (MEDIUM-7).
fn bench_writer_append_amortized_flush(c: &mut Criterion) {
    install_prometheus_recorder();
    let (listener, addr) = bind_loopback_or_die("writer/append_with_seq_amortized_flush");
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
    let cfg = QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: 0,
        pg_port: 0,
        ilp_port: addr.port(),
    };
    let mut writer = TickPersistenceWriter::new(&cfg).unwrap_or_else(|e| {
        panic!(
            "writer/append_with_seq_amortized_flush: writer setup failed against \
             the local drain listener — bench must not silently self-skip: {e}"
        )
    });
    let tick = sample_tick();
    let mut seq: i64 = 1;
    c.bench_function("writer/append_with_seq_amortized_flush", |b| {
        b.iter(|| {
            seq = seq.wrapping_add(1);
            let _ = black_box(writer.append_tick_with_seq(black_box(&tick), seq));
        });
    });
}

/// B6 (2026-07-03) — the 10µs-gated TRUE-COMPUTE bench: the full live-loop
/// chain (parse → heartbeat → gap detector → canary → enricher → REAL
/// `append_tick_with_seq` under the off-thread flush offload → broadcast →
/// monotonicity guard → histogram records incl. the compute split). Flush
/// I/O runs on the worker thread, NOT in the measured thread — this is the
/// steady-state per-tick hot path the operator's p50 ≈ 30µs claim refers
/// to. Budget: `composite_quote_tick_compute_only = 10000` ns in
/// `quality/benchmark-budgets.toml`, enforced by `scripts/bench-gate.sh`.
/// Panics loudly if the runner forbids loopback sockets (MEDIUM-7 — a
/// silent skip would leave no estimates.json, so the 10µs gate would
/// never be evaluated and the budget row silently self-disarms).
fn bench_composite_compute_only(c: &mut Criterion) {
    let handle = install_prometheus_recorder();
    let (listener, addr) = bind_loopback_or_die("composite/quote_tick_compute_only");
    // Multi-accept drain: the writer AND its off-thread flush worker each
    // open one ILP TCP connection.
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
    let cfg = QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: 0,
        pg_port: 0,
        ilp_port: addr.port(),
    };
    let mut writer = TickPersistenceWriter::new(&cfg).unwrap_or_else(|e| {
        panic!(
            "composite/quote_tick_compute_only: writer setup failed against the \
             local drain listener — the 10µs bench-gate row must not silently \
             self-disarm: {e}"
        )
    });

    let packet = build_quote_packet(13);
    let m_frames = metrics::counter!("bench_co_frames_total");
    let m_ticks = metrics::counter!("bench_co_ticks_total");
    let m_persisted = metrics::counter!("bench_co_persisted_total");
    let m_tick_duration = metrics::histogram!("bench_co_tick_duration_ns");
    let m_tick_compute = metrics::histogram!("bench_co_tick_compute_ns");
    let m_wire_to_done = metrics::histogram!("bench_co_wire_to_done_ns");
    let heartbeat = AtomicI64::new(0);
    let enricher = TickEnricher::new();
    let mut mono_guard = VolumeMonotonicityGuard::new();
    let (sender, receiver) = tokio::sync::broadcast::channel::<ParsedTick>(262_144);
    let _keep_alive = receiver;
    let canary_sids: [u64; 3] = [13, 25, 51];
    let mut seq: i64 = 1;

    c.bench_function("composite/quote_tick_compute_only", |b| {
        b.iter(|| {
            let tick_start = std::time::Instant::now();
            m_frames.increment(1);
            let received_at_nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);

            let Ok(parsed) = dispatch_frame(black_box(&packet), received_at_nanos) else {
                return;
            };
            let tickvault_core::parser::ParsedFrame::Tick(tick) = parsed else {
                return;
            };
            m_ticks.increment(1);

            heartbeat.store(
                received_at_nanos.saturating_div(1_000_000_000),
                Ordering::Relaxed,
            );

            // Gap-detector record site removed in PR-C3 (2026-07-14).

            for sid in &canary_sids {
                if *sid == tick.security_id {
                    #[allow(clippy::cast_precision_loss)]
                    // APPROVED: epoch seconds are exactly representable in f64
                    {
                        black_box(received_at_nanos.saturating_div(1_000_000_000) as f64);
                    }
                    break;
                }
            }

            let secs_of_day = tick.exchange_timestamp % 86_400;
            let enriched = enricher.enrich_tick(&tick, secs_of_day);
            black_box(enriched.volume_delta);

            // REAL writer under the B6 off-thread flush offload — at the
            // 1-in-1000 batch boundary this is an O(1) buffer swap +
            // bounded-channel send; the TCP flush happens on the worker.
            seq = seq.wrapping_add(1);
            let _ = black_box(writer.append_tick_with_seq(&tick, seq));
            m_persisted.increment(1);

            let _ = sender.send(tick);

            let _ = mono_guard.observe(tick.security_id, tick.exchange_segment_code, tick.volume);

            // Trailing records — mirrors the live loop's B6 split exactly.
            let total_elapsed_ns =
                u64::try_from(tick_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
            #[allow(clippy::cast_precision_loss)] // APPROVED: same cast as live loop
            m_tick_duration.record(total_elapsed_ns as f64);
            let stall_ns = writer.take_last_stall_ns();
            #[allow(clippy::cast_precision_loss)] // APPROVED: same cast as live loop
            m_tick_compute.record(total_elapsed_ns.saturating_sub(stall_ns) as f64);
            let wire_elapsed = chrono::Utc::now()
                .timestamp_nanos_opt()
                .unwrap_or(0)
                .saturating_sub(received_at_nanos);
            #[allow(clippy::cast_precision_loss)] // APPROVED: same cast as live loop
            m_wire_to_done.record(wire_elapsed.max(0) as f64);
        });
    });
    // Drain the accumulated histogram samples (hostile-review HIGH #1).
    drop(handle.render());
}

criterion_group!(
    benches,
    bench_clock_reads,
    bench_parse,
    bench_enricher,
    bench_ilp_row_build,
    bench_payload_hash,
    bench_broadcast_send,
    bench_metrics_ops,
    bench_composite,
    bench_writer_append_amortized_flush,
    bench_composite_compute_only,
);
criterion_main!(benches);
