//! Wall-clock harness for the drain's 30 s silence arm (2026-11-17).
//!
//! Live on 17 Nov 2026 the arm (`drain_arm_silence`) took about 2.9 ms every
//! 30 s and held frames in the ring past 1 ms in 56 of 64 minutes; this
//! harness split that cost and found about 85% in the subscribe reconcile's
//! hash probe per report. `#[ignore]`d so a wall-clock number never becomes a
//! flaky CI gate. Run it in release on one core:
//!
//! ```text
//! cargo test --release -p tickvault-app --lib silence_arm_cost -- --ignored --nocapture
//! ```
//!
//! Prints one `MEASURE` line per phase (min / median / max over 15 rounds,
//! WARM and COLD, COLD flushing 768 MiB of cache first) and one `PROBE` line
//! per hash shape.
#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::subscribe_reconcile::SubscribeReconcile;
    use std::hint::black_box;
    use std::time::{Duration, Instant};
    use tickvault_core::pipeline::tick_gap_detector::TickObservation;

    const ROUNDS: usize = 15;
    const FLUSH_BYTES: usize = 768 << 20;

    fn flush(buf: &mut [u64]) {
        let mut acc = 0u64;
        for x in buf.iter_mut().step_by(8) {
            acc = acc.wrapping_add(*x);
            *x = acc;
        }
        black_box(acc);
    }

    #[allow(clippy::print_stdout)] // APPROVED: #[ignore]d measurement harness run with --nocapture; the printed figures are its only output.
    fn stats(name: &str, mut v: Vec<Duration>) {
        v.sort();
        let us = |d: Duration| d.as_secs_f64() * 1e6;
        println!(
            "MEASURE {name:<48} min {:>8.1} us  median {:>8.1} us  max {:>8.1} us",
            us(v[0]),
            us(v[v.len() / 2]),
            us(v[v.len() - 1])
        );
    }

    fn universe() -> Vec<(u64, ExchangeSegment)> {
        let mut v = Vec::new();
        // ~19,780 stock + index options with spread-out ids, 210 cash stocks,
        // 10 index spots; interleaved roughly as attach order (spots first).
        for id in [13u64, 25, 51, 21, 27, 442, 38, 17, 5, 69] {
            v.push((id, ExchangeSegment::IdxI));
        }
        for i in 0..210u64 {
            v.push((1_000 + i * 37, ExchangeSegment::NseEquity));
        }
        let mut x: u64 = 0x9E37_79B9;
        let mut seen = std::collections::HashSet::new();
        while v.len() < 20_000 {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let id = 35_000 + (x >> 33) % 150_000;
            if seen.insert(id) {
                v.push((id, ExchangeSegment::NseFno));
            }
        }
        v
    }

    #[test]
    #[ignore = "wall-clock measurement harness"]
    #[allow(clippy::print_stdout)] // APPROVED: #[ignore]d measurement harness run with --nocapture; the printed figures are its only output.
    fn silence_arm_cost_at_the_authorized_ceiling() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let _ = metrics::set_global_recorder(recorder);

        let now_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
        let start = now_ms - 3_600_000;
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 1)
            .with_detector_capacity(AGGREGATOR_MAX_SLOTS);
        let uni = universe();
        let mut x: u64 = 7;
        for (n, &(id, seg)) in uni.iter().enumerate() {
            assert!(ingest.seed(id, seg, start));
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let cadence = 1_000 + (x >> 40) % 20_000; // 1-21 s
            // ~1% go quiet 10 min before now; the rest last ticked inside 0-30 s.
            let last_age = if n % 100 == 7 {
                600_000
            } else {
                (x >> 20) % 30_000
            };
            let last = now_ms - last_age;
            for k in (0..12u64).rev() {
                let t = last - k * cadence;
                ingest.detector.observe(TickObservation {
                    key: (id, seg),
                    ltt_epoch_secs: u32::try_from(t / 1_000).unwrap_or(0),
                    volume: u32::try_from(1_000 + (12 - k) * 50).unwrap_or(0),
                    open_interest: 500,
                    recv_monotonic_millis: t,
                });
            }
        }
        assert_eq!(ingest.tracked_instruments(), 20_000);

        let (book, _rounds_rx) = SubscribeReconcile::new(false);
        let mut rxs = Vec::new();
        for chunk in uni.chunks(4_000) {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let instruments: Vec<SubscribeInstrument> = chunk
                .iter()
                .map(|&(security_id, segment)| SubscribeInstrument {
                    security_id,
                    segment,
                })
                .collect();
            book.record_subscribed(&tx, &instruments);
            rxs.push((tx, rx));
        }
        assert_eq!(book.expected_len(), 20_000);
        println!(
            "SIZES InstrumentState-slot-bytes={} tracked={} expected={}",
            std::mem::size_of::<SilentInstrument>(),
            ingest.tracked_instruments(),
            book.expected_len()
        );

        // Collected reports for the reconcile-only phase.
        let mut reports = Vec::with_capacity(20_000);
        ingest.detector.scan_silence(now_ms, |r| reports.push(r));
        let mut slotted = Vec::with_capacity(20_000);
        ingest
            .detector
            .scan_silence_slots(now_ms, |slot, r| slotted.push((slot, r)));
        let mut verdicts = std::collections::HashMap::new();
        for r in &reports {
            *verdicts.entry(format!("{:?}", r.verdict)).or_insert(0u32) += 1;
        }
        println!("VERDICTS {verdicts:?}");

        let mut flushbuf = vec![1u64; FLUSH_BYTES / 8];
        let mut worst = [SilentInstrument::EMPTY; WORST_SILENT_NAMED];

        for cold in [false, true] {
            let tag = if cold { "COLD" } else { "WARM" };
            let mut p1 = Vec::new();
            let mut p2 = Vec::new();
            let mut p2b = Vec::new();
            let mut p2c = Vec::new();
            let mut p2d = Vec::new();
            let mut p2e = Vec::new();
            let mut p3 = Vec::new();
            let mut whole = Vec::new();
            for _ in 0..ROUNDS {
                // (1) detector walk alone, closure only black-boxes the report.
                if cold {
                    flush(&mut flushbuf);
                }
                let t = Instant::now();
                ingest.detector.scan_silence(now_ms, |r| {
                    black_box(&r);
                });
                p1.push(t.elapsed());

                // (2) full scan_silence_named with the reconcile book.
                ingest.subscribe_reconcile = Some(Arc::clone(&book));
                if cold {
                    flush(&mut flushbuf);
                }
                let t = Instant::now();
                black_box(ingest.scan_silence_named(now_ms, &mut worst));
                p2.push(t.elapsed());

                // (2b) same without the reconcile book.
                ingest.subscribe_reconcile = None;
                if cold {
                    flush(&mut flushbuf);
                }
                let t = Instant::now();
                black_box(ingest.scan_silence_named(now_ms, &mut worst));
                p2b.push(t.elapsed());

                // (2c) reconcile alone over pre-collected reports.
                if cold {
                    flush(&mut flushbuf);
                }
                let t = Instant::now();
                let mut sweep = book.sweep();
                for r in &reports {
                    sweep.note(r);
                }
                black_box(sweep.finish_now());
                p2c.push(t.elapsed());

                // (2e) reconcile alone through the slot cells (2026-11-17),
                // over the same reports with their detector slots; the cells
                // were filled by the earlier rounds, so this is the steady
                // state the arm runs in.
                if cold {
                    flush(&mut flushbuf);
                }
                let t = Instant::now();
                let mut sweep = book.sweep();
                for (slot, r) in &slotted {
                    sweep.note_at_slot(*slot, r);
                }
                black_box(sweep.finish_now());
                p2e.push(t.elapsed());

                // (2d) bare hash probes into a std HashMap of the same keys.
                // (measures only the hash + probe cost, no Expected work)
                if cold {
                    flush(&mut flushbuf);
                }
                let t = Instant::now();
                let mut hits = 0u32;
                {
                    let mut sweep = book.sweep();
                    for r in reports.iter() {
                        // Same call; kept separate to show variance only.
                        sweep.note(r);
                        hits += 1;
                    }
                    drop(sweep);
                }
                black_box(hits);
                p2d.push(t.elapsed());

                // (3) the rest of the arm's work (not the scan, not paging).
                ingest.subscribe_reconcile = Some(Arc::clone(&book));
                if cold {
                    flush(&mut flushbuf);
                }
                let t = Instant::now();
                tickvault_storage::ws_frame_spill::refresh_receipt_anchor();
                black_box(ingest.refusals());
                metrics::gauge!(SILENCE_DETECTOR_REFUSED_GAUGE)
                    .set(ingest.detector_refused() as f64);
                metrics::gauge!(WORST_CONN_TICK_AGE_GAUGE).set(
                    worst_connection_tick_age_secs(chrono::Utc::now().timestamp_millis())
                        .map_or(-1.0, |s| s as f64),
                );
                publish_connection_deliveries(chrono::Utc::now().timestamp_millis());
                tickvault_core::websocket::pool_supervisor::publish_frame_gap_peaks();
                black_box(
                    ingest.check_depth200_cross_feed(now_ms, configured_frame_silence_fast_path()),
                );
                metrics::gauge!("tv_dhan_feed_ring_resident_pct_by_pool", "pool" => "main_feed")
                    .set(1.0);
                metrics::gauge!("tv_dhan_feed_ring_resident_pct_by_pool", "pool" => "depth")
                    .set(1.0);
                let secs = now_ist_secs_of_day();
                black_box(is_within_market_hours_ist(secs));
                black_box(silence_page_allowed_today());
                black_box(muhurat_silence_check_due_now(secs));
                black_box(ingest.tracked_instruments());
                p3.push(t.elapsed());

                // (4) whole: (2) then (3) back to back from cold.
                if cold {
                    flush(&mut flushbuf);
                }
                let t = Instant::now();
                tickvault_storage::ws_frame_spill::refresh_receipt_anchor();
                black_box(ingest.scan_silence_named(now_ms, &mut worst));
                publish_connection_deliveries(chrono::Utc::now().timestamp_millis());
                tickvault_core::websocket::pool_supervisor::publish_frame_gap_peaks();
                black_box(
                    ingest.check_depth200_cross_feed(now_ms, configured_frame_silence_fast_path()),
                );
                whole.push(t.elapsed());
            }
            stats(
                &format!("{tag} (1) detector.scan_silence, black_box closure"),
                p1,
            );
            stats(&format!("{tag} (2) scan_silence_named WITH reconcile"), p2);
            stats(
                &format!("{tag} (2b) scan_silence_named WITHOUT reconcile"),
                p2b,
            );
            stats(
                &format!("{tag} (2c) reconcile sweep+note x20k+finish only"),
                p2c,
            );
            stats(
                &format!("{tag} (2d) reconcile sweep+note x20k (no finish)"),
                p2d,
            );
            stats(
                &format!("{tag} (2e) reconcile sweep+note_at_slot x20k+finish"),
                p2e,
            );
            stats(
                &format!("{tag} (3) rest of arm (gauges/publish/checks)"),
                p3,
            );
            stats(&format!("{tag} (4) scan_named + publishes"), whole);
        }
        drop(rxs);
    }
}

#[cfg(test)]
mod hash_probe {
    use super::super::*;
    use std::collections::HashMap;
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy)]
    struct Fat([u64; 9]); // ~72 B, the size class of `Expected`

    fn flush(buf: &mut [u64]) {
        let mut acc = 0u64;
        for x in buf.iter_mut().step_by(8) {
            acc = acc.wrapping_add(*x);
            *x = acc;
        }
        black_box(acc);
    }
    fn med(mut v: Vec<Duration>) -> (f64, f64) {
        v.sort();
        (v[0].as_secs_f64() * 1e6, v[v.len() / 2].as_secs_f64() * 1e6)
    }

    #[test]
    #[ignore = "wall-clock measurement harness"]
    #[allow(clippy::print_stdout)] // APPROVED: #[ignore]d measurement harness run with --nocapture; the printed figures are its only output.
    fn probe_cost_std_vs_ahash_vs_dense() {
        let keys: Vec<(u64, ExchangeSegment)> = {
            let mut x: u64 = 0x9E37_79B9;
            let mut s = std::collections::HashSet::new();
            let mut v = Vec::new();
            while v.len() < 20_000 {
                x = x
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let id = 35_000 + (x >> 33) % 150_000;
                if s.insert(id) {
                    v.push((id, ExchangeSegment::NseFno));
                }
            }
            v
        };
        let mut std_map: HashMap<(u64, ExchangeSegment), Fat> = HashMap::with_capacity(25_000);
        let mut a_map: HashMap<(u64, ExchangeSegment), Fat, ahash::RandomState> =
            HashMap::with_capacity_and_hasher(25_000, ahash::RandomState::new());
        let mut dense: Vec<Fat> = Vec::with_capacity(25_000);
        for (i, k) in keys.iter().enumerate() {
            std_map.insert(*k, Fat([i as u64; 9]));
            a_map.insert(*k, Fat([i as u64; 9]));
            dense.push(Fat([i as u64; 9]));
        }
        let mut flushbuf = vec![1u64; (768usize << 20) / 8];
        for cold in [false, true] {
            let (mut a, mut b, mut c, mut d) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for _ in 0..15 {
                if cold {
                    flush(&mut flushbuf);
                }
                let t = Instant::now();
                let mut s = 0u64;
                for k in &keys {
                    if let Some(e) = std_map.get_mut(k) {
                        e.0[1] = e.0[1].wrapping_add(1);
                        s += e.0[0];
                    }
                }
                black_box(s);
                a.push(t.elapsed());

                if cold {
                    flush(&mut flushbuf);
                }
                let t = Instant::now();
                let mut s = 0u64;
                for k in &keys {
                    if let Some(e) = a_map.get_mut(k) {
                        e.0[1] = e.0[1].wrapping_add(1);
                        s += e.0[0];
                    }
                }
                black_box(s);
                b.push(t.elapsed());

                if cold {
                    flush(&mut flushbuf);
                }
                let t = Instant::now();
                let mut s = 0u64;
                for e in dense.iter_mut() {
                    e.0[1] = e.0[1].wrapping_add(1);
                    s += e.0[0];
                }
                black_box(s);
                c.push(t.elapsed());

                // hash only (SipHash), no memory touch beyond keys
                if cold {
                    flush(&mut flushbuf);
                }
                let t = Instant::now();
                let mut s = 0u64;
                for k in &keys {
                    s = s.wrapping_add(std::hash::BuildHasher::hash_one(std_map.hasher(), k));
                }
                black_box(s);
                d.push(t.elapsed());
            }
            let tag = if cold { "COLD" } else { "WARM" };
            println!(
                "PROBE {tag} std SipHash get_mut x20k   min/med {:?} us",
                med(a)
            );
            println!(
                "PROBE {tag} ahash get_mut x20k         min/med {:?} us",
                med(b)
            );
            println!(
                "PROBE {tag} dense Vec by slot x20k     min/med {:?} us",
                med(c)
            );
            println!(
                "PROBE {tag} SipHash hash_one only x20k min/med {:?} us",
                med(d)
            );
        }
    }
}
