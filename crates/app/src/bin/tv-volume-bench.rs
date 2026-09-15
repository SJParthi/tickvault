//! Synthetic in-process measurements of the canonical candle-ranking code.
//! Run a release build on the target AWS host. No network, DB, credentials or
//! trading API is used. These are observed distributions, never latency bounds.

#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]

use std::hint::black_box;
use std::time::Instant;

use tickvault_app::bucket_top_volume::{
    BucketContract, BucketDecisionRequest, BucketTopVolumeEngine, BucketTopVolumeRuntime,
    BucketVolumeMetric,
};
use tickvault_app::volume_leaderboard::OptionFamily;
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::VolumeQuality;
use tickvault_common::types::ExchangeSegment;
use tickvault_trading::candles::{
    CandleMetadata, CandleVolumeUpdate, TF_COUNT, TOP_VOLUME_TF_COUNT, TfIndex,
};

const NOW: u32 = 20_700 * 86_400 + 33_360;
const GENERATION: u64 = 1;
const METRIC: BucketVolumeMetric = BucketVolumeMetric::SignedBarVolumeVsOneLotV3;

fn update(id: usize, tf: TfIndex, revision: u64, numerator: i64) -> CandleVolumeUpdate {
    CandleVolumeUpdate {
        feed: Feed::Dhan,
        security_id: id as u64 + 1,
        segment_code: ExchangeSegment::NseFno.binary_code(),
        tf,
        session_day: NOW / 86_400,
        bucket_start_secs: tf.bucket_start(NOW),
        bucket_end_secs: tf.bucket_end(tf.bucket_start(NOW)),
        gross_volume: numerator.unsigned_abs(),
        estimated_net_volume: Some(numerator),
        metadata: CandleMetadata {
            lot_size: 25 + (id % 100) as u32,
            instrument_definition_version: GENERATION,
            underlying_id: id as u64 + 100_000,
            family_code: 1,
        },
        volume_quality: 0,
        revision,
        last_observed_secs: NOW,
        quality: VolumeQuality {
            baseline_known: true,
            counter_ambiguous: false,
            volume_missing: false,
            attribution_uncertain: false,
        },
        closed: false,
    }
}

fn summarize(mut samples: Vec<u64>) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(
        !samples.is_empty(),
        "a latency distribution needs at least one measurement"
    );
    samples.sort_unstable();
    let percentile = |percent: usize| samples[((samples.len() - 1) * percent) / 100];
    Ok(serde_json::json!({
        "samples": samples.len(), "min_ns": samples[0],
        "p50_ns": percentile(50), "p95_ns": percentile(95),
        "p99_ns": percentile(99), "max_ns": samples[samples.len() - 1],
        "timer_overhead_included": true,
        "timer_overhead_subtracted": false,
        "percentile_method": "lower order statistic at floor((n - 1) * percent / 100)",
        "individual_samples_retained": false
    }))
}

fn measure(
    mut operation: impl FnMut(usize) -> anyhow::Result<()>,
    iterations: usize,
) -> anyhow::Result<serde_json::Value> {
    let mut samples = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let start = Instant::now();
        operation(i)?;
        let elapsed = start.elapsed().as_nanos();
        samples.push(u64::try_from(elapsed)?);
    }
    summarize(samples)
}

fn registered_engine(instruments: usize) -> anyhow::Result<BucketTopVolumeEngine> {
    let registered = (0..instruments).map(|id| BucketContract {
        security_id: id as u64 + 1,
        segment: ExchangeSegment::NseFno,
        underlying_id: id as u64 + 100_000,
        family: OptionFamily::Stock,
        lot_size: 25 + (id % 100) as u32,
        instrument_definition_version: GENERATION,
    });
    BucketTopVolumeEngine::new(Feed::Dhan, NOW / 86_400, GENERATION, METRIC, registered, 2)
        .map_err(|e| anyhow::anyhow!("registration refused: {e:?}"))
}

fn rollover_update(id: usize, timestamp: u32) -> CandleVolumeUpdate {
    let mut value = update(
        id,
        TfIndex::S1,
        u64::from(timestamp - NOW) + 1,
        (id % 20_001) as i64 - 10_000,
    );
    value.bucket_start_secs = timestamp;
    value.bucket_end_secs = timestamp + 1;
    value.last_observed_secs = timestamp;
    value
}

/// The first observation in a new bucket also reclaims the oldest retained
/// bucket. Populate both retained buckets before timing, then refill each new
/// bucket outside its timed region. Every measured eviction contains N rows.
/// "New bucket" means first allocation, not a deliberately flushed CPU cache.
fn rollover_eviction(instruments: usize, iterations: usize) -> anyhow::Result<serde_json::Value> {
    let mut engine = registered_engine(instruments)?;
    for timestamp in [NOW, NOW + 1] {
        for id in 0..instruments {
            engine
                .apply(&rollover_update(id, timestamp))
                .map_err(|e| anyhow::anyhow!("rollover setup refused: {e:?}"))?;
        }
    }
    let mut samples = Vec::with_capacity(iterations);
    for index in 0..iterations {
        let timestamp = NOW
            .checked_add(u32::try_from(index)? + 2)
            .ok_or_else(|| anyhow::anyhow!("rollover fixture clock overflow"))?;
        let oldest = timestamp - 2;
        // Read only cached coverage; do not walk/re-materialize the entire
        // old board and warm every node immediately before timing eviction.
        let before = engine
            .winner_snapshot(OptionFamily::Stock, TfIndex::S1, oldest, timestamp)
            .ok_or_else(|| anyhow::anyhow!("oldest bucket missing before eviction"))?;
        anyhow::ensure!(
            before.coverage.observed_contracts == instruments
                && before.coverage.eligible_contracts == instruments,
            "refuse to benchmark eviction of a partial/empty old bucket"
        );
        drop(before);
        let row = rollover_update(0, timestamp);
        let start = Instant::now();
        black_box(
            engine
                .apply(black_box(&row))
                .map_err(|e| anyhow::anyhow!("rollover update refused: {e:?}"))?,
        );
        let duration = u64::try_from(start.elapsed().as_nanos())?;
        samples.push(duration);
        anyhow::ensure!(
            engine
                .winner_snapshot(OptionFamily::Stock, TfIndex::S1, oldest, timestamp)
                .is_none(),
            "oldest bucket was not evicted"
        );
        let after = engine
            .winner_snapshot(OptionFamily::Stock, TfIndex::S1, timestamp, timestamp)
            .ok_or_else(|| anyhow::anyhow!("new bucket was not created"))?;
        anyhow::ensure!(
            after.coverage.observed_contracts == 1,
            "new bucket did not start with exactly one row"
        );
        drop(after);
        for id in 1..instruments {
            engine
                .apply(&rollover_update(id, timestamp))
                .map_err(|e| anyhow::anyhow!("rollover refill refused: {e:?}"))?;
        }
    }
    Ok(serde_json::json!({
        "distribution": summarize(samples)?,
        "evicted_contract_rows_per_sample": instruments,
        "retained_buckets": 2,
        "includes": "first S1 bucket allocation, exact index insertion and complete oldest-bucket reclamation",
        "setup_refill_and_postconditions_timed": false,
        "cpu_cache_explicitly_flushed": false,
        "observed_case_not_worst_case_bound": true
    }))
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let instruments = args.get(1).map_or(Ok(25_000), |v| v.parse::<usize>())?;
    let iterations = args.get(2).map_or(Ok(10_000), |v| v.parse::<usize>())?;
    anyhow::ensure!(
        (1..=25_000).contains(&instruments),
        "instrument count must be 1..=25000"
    );
    anyhow::ensure!(
        (100..=100_000).contains(&iterations),
        "sample count must be 100..=100000"
    );
    anyhow::ensure!(
        !cfg!(debug_assertions),
        "run a release build for latency measurements"
    );
    let mut engine = registered_engine(instruments)?;
    for id in 0..instruments {
        for tf in TfIndex::TOP_VOLUME_ALL {
            engine
                .apply(&update(id, tf, 1, (id % 20_001) as i64 - 10_000))
                .map_err(|e| anyhow::anyhow!("initial update refused: {e:?}"))?;
        }
    }
    let runtime = BucketTopVolumeRuntime::new();
    let tf = TfIndex::S1;
    let publish = |engine: &BucketTopVolumeEngine| {
        engine
            .publish_winner_latest(&runtime, OptionFamily::Stock, tf, NOW)
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("winner refused: {e:?}"))
    };
    publish(&engine)?;
    let request = BucketDecisionRequest {
        feed: Feed::Dhan,
        family: OptionFamily::Stock,
        tf,
        session_day: NOW / 86_400,
        universe_version: GENERATION,
        metric: METRIC,
        bucket_start_secs: tf.bucket_start(NOW),
        now_secs: NOW,
        max_age_secs: 2,
        require_closed: false,
    };
    let timer = measure(
        |_| {
            black_box(());
            Ok(())
        },
        iterations,
    )?;
    let prepared = runtime
        .load_winner_for_decision(request)
        .map_err(|e| anyhow::anyhow!("decision refused: {e:?}"))?;
    let row = *prepared
        .first()
        .ok_or_else(|| anyhow::anyhow!("winner unavailable"))?;
    let arithmetic = measure(
        |_| {
            black_box(black_box(row).estimated_net_lots_milli());
            black_box(black_box(row).score_milli_pct(black_box(METRIC)));
            Ok(())
        },
        iterations,
    )?;
    let read = measure(
        |_| {
            let snapshot = runtime
                .load_winner_for_decision(black_box(request))
                .map_err(|e| anyhow::anyhow!("decision refused: {e:?}"))?;
            let winner = snapshot
                .first()
                .ok_or_else(|| anyhow::anyhow!("winner disappeared"))?;
            black_box((
                winner.security_id,
                winner.estimated_net_volume,
                winner.lot_size,
            ));
            // The local Arc is released before the timed operation returns.
            Ok(())
        },
        iterations,
    )?;
    let mut revision = 2;
    let indexed = measure(
        |i| {
            let row = update(
                i % instruments,
                tf,
                revision,
                if i % 2 == 0 { 999_999 } else { -999_999 },
            );
            revision += 1;
            black_box(
                engine
                    .apply(black_box(&row))
                    .map_err(|e| anyhow::anyhow!("update refused: {e:?}"))?,
            );
            Ok(())
        },
        iterations,
    )?;
    let indexed_publish = measure(
        |i| {
            let row = update(i % instruments, tf, revision, (i % 20_001) as i64 - 10_000);
            revision += 1;
            black_box(
                engine
                    .apply(black_box(&row))
                    .map_err(|e| anyhow::anyhow!("update refused: {e:?}"))?,
            );
            publish(&engine)
        },
        iterations,
    )?;
    let frames = measure(
        |i| {
            for frame in TfIndex::TOP_VOLUME_ALL {
                let row = update(
                    i % instruments,
                    frame,
                    revision,
                    (i % 20_001) as i64 - 10_000,
                );
                revision += 1;
                black_box(
                    engine
                        .apply(black_box(&row))
                        .map_err(|e| anyhow::anyhow!("update refused: {e:?}"))?,
                );
                engine
                    .publish_winner_latest(&runtime, OptionFamily::Stock, frame, NOW)
                    .map_err(|e| anyhow::anyhow!("winner refused: {e:?}"))?;
            }
            Ok(())
        },
        iterations.min(2_000),
    )?;
    let table = measure(
        |_| {
            black_box(
                engine
                    .snapshot(OptionFamily::Stock, tf, tf.bucket_start(NOW), NOW)
                    .ok_or_else(|| anyhow::anyhow!("table unavailable"))?,
            );
            Ok(())
        },
        iterations.min(200),
    )?;
    let rollover = rollover_eviction(instruments, iterations.min(32))?;
    println!(
        "{}",
        serde_json::json!({
            "schema": "tickvault.canonical-volume-benchmark.v1",
            "metric": METRIC.as_str(), "instruments": instruments, "timeframes": TOP_VOLUME_TF_COUNT,
            "timeframe_selectors": TfIndex::TOP_VOLUME_ALL.map(TfIndex::display_name),
            "timeframe_seconds": TfIndex::TOP_VOLUME_ALL.map(TfIndex::seconds_per_bucket),
            "candle_timeframes": TF_COUNT,
            "registered_option_families": ["stock"],
            "all_timeframes_scope": "one update and winner publication per admitted Top Volume timeframe for one stock-option instrument; candle-only frames do not enter the ranking benchmark and historical measurements with a different frame set are not this result",
            "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "scope": "synthetic in-process ranking; excludes socket, candle fold, WAL, DB, HTTP and order execution",
            "fixture_clock": "fixed synthetic IST seconds supplied to the accessor; no wall-clock acquisition is timed",
            "fixture_now_ist_secs": NOW, "concurrent_writer_during_read": false,
            "instrument_quality": "synthetically asserted eligible; not a measured market-feed completeness result",
            "prepared_read_scope": "snapshot load, metadata validation, first-row fields and local Arc release",
            "arithmetic_scope": "signed net lots and percentage in thousandths, truncated toward zero for display; exact index ordering is measured separately",
            "full_table_scope": "ordered row materialization, allocation and returned snapshot release",
            "timer": timer, "net_lots_and_percentage_arithmetic": arithmetic,
            "prepared_winner_read": read, "exact_index_update": indexed,
            "update_and_winner_publication": indexed_publish,
            "all_timeframes_update_and_publication": frames, "full_sorted_table_copy": table,
            "first_update_with_full_old_bucket_eviction": rollover,
            "hard_latency_guarantee": false
        })
    );
    Ok(())
}
