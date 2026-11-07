#![cfg(test)]
//! PR TG-4 (2026-11-06 live incident): the partition archiver's `/exp`
//! export against a REAL QuestDB, while ILP writes and `/exec` probes run
//! beside it.
//!
//! On 2026-11-06 the live QuestDB ran every HTTP request on one shared network
//! thread (`http.worker.count=0`). The disk-pressure archiver's `/exp` export
//! of one `market_depth` hour (95-115M rows) held that thread for 4-5 minutes
//! at a time, and every ILP `/write` and every `/exec` probe waited behind it:
//! 14 BOOT-01, 30 reconnects, 2,935,971 tick rows rescued to spill. PR DP
//! (#2196) gave QuestDB an HTTP worker pool of its own (3 workers, export
//! limit 2, so one worker is always free for writes) and gated the in-session
//! archive pass. TG-1..3 test the archiver against stand-ins; this is the
//! real-QuestDB version, run weekly by `.github/workflows/chaos-nightly.yml`
//! against the stack `deploy/docker/docker-compose.yml` starts (the
//! production QuestDB image, memory limit, cpuset and HTTP worker settings).
//!
//! # What it does
//!
//! 1. Writes one `market_depth` HOUR partition (`TV_TG4_DEPTH_ROWS` rows,
//!    default 5,000,000, through the real `DepthWriter`) dated ten days ago,
//!    plus a few rows yesterday so that hour is not the table's active
//!    partition. The incident exported 95-115M-row hours; 5M rows is about 5%
//!    of one, sized so the export lasts tens of seconds on a CI runner (the
//!    debug-built archiver gzips and hashes every byte) without filling the
//!    runner's disk. Raise it for a closer copy of a live hour.
//! 2. Starts, and keeps running for the whole pass:
//!    * an ILP writer: the real `TickWriter`, `TV_TG4_TICK_ROWS_PER_SEC` rows a
//!      second (default 5,000, the per-second envelope this repo sizes the
//!      drain for), flushed every 500 ms like the drain's flush arm, each
//!      flush timed;
//!    * an `/exec` probe: `SELECT 1` once a second, timed (the boot probe and
//!      the pool watchdog ask the same question);
//!    * a second `/exp` stream of the same hour (`TV_TG4_SECOND_EXPORT`, on by
//!      default), standing in for a portal read: with the archiver's export it
//!      fills QuestDB's export limit of 2, the case PR DP review M6 sized the
//!      worker pool for. It reads the whole hour, which is harsher than a
//!      portal read (capped at 1,000 rows live).
//! 3. Runs the REAL archive pass (`archive_and_drop_old_partitions`, the
//!    production entry point) with no pass limits, against the real QuestDB
//!    and the stub S3 of the parent module. It exports the hour, uploads it,
//!    recounts it and drops it.
//!
//! # The bounds, and why these numbers
//!
//! While the pass runs:
//! * **No ILP flush fails, and none takes longer than
//!   `ILP_REQUEST_TIMEOUT_SECS` (5 s).** That is the writers' own request
//!   timeout: in production a flush that waits longer is cut off and its rows
//!   go to the rescue spill, which is exactly the 2026-11-06 symptom. The
//!   incident stalled writes for 4-5 minutes, so the bound catches it with a
//!   wide margin and still leaves a loaded runner room.
//! * **No `/exec` probe fails, and none takes longer than
//!   `PROBE_REQUEST_TIMEOUT_SECS` (2 s).** That is the shared readiness-probe
//!   client's timeout; a slower answer is a failed probe in production (the
//!   incident's BOOT-01 lines).
//!
//! # Not vacuous
//!
//! The test fails, with numbers, unless: the pass ran and exported and
//! dropped the seeded hour (its rows are gone afterwards), at least
//! `MIN_SAMPLES_IN_PASS` flushes and probes STARTED inside the pass, and every
//! tick row the writer sent is in `ticks` at the end. A pass too short to
//! overlap the probes fails the test and says to raise `TV_TG4_DEPTH_ROWS`.
//!
//! # How it runs
//!
//! `#[ignore]`d, so it never gates a PR. It needs a running QuestDB on
//! `TV_QDB_REAL_HTTP=host:port`. With that unset it prints `SKIPPED` and
//! returns, unless `TV_QDB_REAL_REQUIRED=1` (set by the nightly lane), which
//! makes an unset or unreachable QuestDB a failure.
//!
//! ```text
//! docker compose -f deploy/docker/docker-compose.yml up -d tv-questdb
//! TV_QDB_REAL_HTTP=127.0.0.1:9000 TV_QDB_REAL_REQUIRED=1 \
//!     cargo test -p tickvault-storage --lib tg4_archiver_export_contention \
//!     -- --ignored --nocapture
//! ```
//!
//! To see it catch the regression, start QuestDB with
//! `TV_QDB_HTTP_WORKERS=0` (HTTP back on the shared network thread, the
//! pre-2026-11-06 setting) and run it again.
//!
//! Honest limits: a GitHub runner is not the r8g.large (disk, CPU and the
//! 2-core cpuset differ); the writes go to `ticks`, not `market_depth`,
//! because a write to the exported table would make the archiver's
//! pre-export WAL gate skip it; the S3 side is the local stub, so the upload
//! takes local time, not network time.

// APPROVED: test-only harness output. The figures are the point of a
// measurement run read from the nightly log; the crate denies eprintln! for
// production code, and this file is under #[cfg(test)].
#![allow(clippy::print_stderr)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;

use super::{HashMap, build_archiver, s3_responder, spawn_stub, test_retention_cfg};
use crate::depth_persistence::{
    DEPTH_KIND_20, DEPTH_SIDE_ASK, DEPTH_SIDE_BID, DepthRow, DepthWriter, ILP_REQUEST_TIMEOUT_SECS,
    ensure_market_depth_table,
};
use crate::http_client::PROBE_REQUEST_TIMEOUT_SECS;
use crate::tick_persistence::{TickWriter, ensure_ticks_table};

/// Longest an ILP flush may take while the export runs (the writers' own
/// request timeout; see the module docs).
const ILP_FLUSH_BOUND: Duration = Duration::from_secs(ILP_REQUEST_TIMEOUT_SECS);
/// Longest an `/exec` probe may take while the export runs (the readiness
/// probe client's timeout).
const PROBE_BOUND: Duration = Duration::from_secs(PROBE_REQUEST_TIMEOUT_SECS);
/// Flushes and probes that must start inside the pass, so the bounds are
/// judged on real overlap, not on a pass that ended before anything ran.
const MIN_SAMPLES_IN_PASS: usize = 5;
/// The drain's flush cadence.
const FLUSH_EVERY: Duration = Duration::from_millis(500);
const PROBE_EVERY: Duration = Duration::from_secs(1);
/// Depth rows per seeding flush.
const SEED_FLUSH_ROWS: u64 = 50_000;
/// Samples taken before and after the pass, for a baseline in the log.
const MARGIN: Duration = Duration::from_secs(3);

const DEFAULT_DEPTH_ROWS: u64 = 5_000_000;
const DEFAULT_TICK_ROWS_PER_SEC: u64 = 5_000;
const DEFAULT_APPLY_DEADLINE_SECS: u64 = 900;

/// Instrument ranges of this test, kept apart from every other real-QuestDB
/// test's ids.
const DEPTH_SID_BASE: i64 = 940_000;
const DEPTH_SIDS: i64 = 200;
const TICK_SID_BASE: u64 = 945_000;
const TICK_SIDS: u64 = 500;

const SECS_PER_DAY: i64 = 86_400;
const IST_OFFSET_SECS: i64 = 19_800;
const NANOS: i64 = 1_000_000_000;
/// 10:00 IST, inside every session-window gate.
const TEN_AM_SECS: i64 = 36_000;
/// How far back the seeded hour sits: past every hot window the test config
/// keeps (depth 3 days, market data 14 days, minimum 2).
const SEED_DAYS_AGO: i64 = 10;

fn env_u64(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a whole number, got {v:?}")),
        _ => default,
    }
}

fn required() -> bool {
    std::env::var("TV_QDB_REAL_REQUIRED").is_ok_and(|v| v == "1")
}

/// The QuestDB to test against, or `None` (the test returns) when none is set
/// and none is required.
fn qdb() -> Option<QuestDbConfig> {
    let Some(addr) = std::env::var("TV_QDB_REAL_HTTP")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        assert!(
            !required(),
            "TV_QDB_REAL_REQUIRED=1 but TV_QDB_REAL_HTTP is unset: the TG-4 contention \
             test would skip and the lane would still pass"
        );
        eprintln!(
            "tg4_archiver_export_contention: SKIPPED (NOT RUN, NOT PASSED) — set \
             TV_QDB_REAL_HTTP=host:port to a running QuestDB"
        );
        return None;
    };
    let (host, port) = addr
        .rsplit_once(':')
        .unwrap_or_else(|| panic!("TV_QDB_REAL_HTTP must be host:port, got {addr}"));
    Some(QuestDbConfig {
        host: host.to_string(),
        http_port: port
            .parse()
            .unwrap_or_else(|_| panic!("TV_QDB_REAL_HTTP port is not a number: {addr}")),
        pg_port: 8812,
        ilp_port: 9009,
    })
}

/// One timed request: when it started, how long it took, whether it worked.
#[derive(Clone, Copy, Debug)]
struct Sample {
    at: Instant,
    took: Duration,
    ok: bool,
}

/// The samples that STARTED inside `[from, to]`.
fn started_within(samples: &[Sample], from: Instant, to: Instant) -> Vec<Sample> {
    samples
        .iter()
        .filter(|s| s.at >= from && s.at <= to)
        .copied()
        .collect()
}

fn max_took(samples: &[Sample]) -> Duration {
    samples.iter().map(|s| s.took).max().unwrap_or_default()
}

fn percentile(samples: &[Sample], q: f64) -> Duration {
    let mut took: Vec<Duration> = samples.iter().map(|s| s.took).collect();
    if took.is_empty() {
        return Duration::ZERO;
    }
    took.sort_unstable();
    let index = ((took.len() as f64 - 1.0) * q).round() as usize;
    took[index.min(took.len() - 1)]
}

fn failures(samples: &[Sample]) -> usize {
    samples.iter().filter(|s| !s.ok).count()
}

/// An IST-epoch second as the ISO text QuestDB compares `ts` against (the
/// stored value is IST epoch read as UTC, so no zone shift).
fn iso(ist_epoch_secs: i64) -> String {
    chrono::DateTime::from_timestamp(ist_epoch_secs, 0)
        .unwrap_or_else(|| panic!("{ist_epoch_secs} is not a valid instant"))
        .format("%Y-%m-%dT%H:%M:%S.000000Z")
        .to_string()
}

fn base_url(cfg: &QuestDbConfig) -> String {
    format!("http://{}:{}", cfg.host, cfg.http_port)
}

/// Runs one SQL statement through `/exec`; panics on a transport failure or a
/// QuestDB error, naming the statement.
async fn exec(client: &reqwest::Client, base: &str, sql: &str) -> Value {
    let body: Value = client
        .get(format!("{base}/exec"))
        .query(&[("query", sql)])
        .send()
        .await
        .unwrap_or_else(|e| panic!("QuestDB unreachable for `{sql}`: {e}"))
        .json()
        .await
        .unwrap_or_else(|e| panic!("QuestDB answer to `{sql}` is not JSON: {e}"));
    assert!(
        body.get("error").is_none(),
        "QuestDB refused `{sql}`: {body}"
    );
    body
}

async fn count(client: &reqwest::Client, base: &str, sql: &str) -> u64 {
    exec(client, base, sql)
        .await
        .get("dataset")
        .and_then(|d| d.get(0))
        .and_then(|r| r.get(0))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("`{sql}` returned no count"))
}

/// Waits until QuestDB answers `SELECT 1` (a chaos test before this one may
/// have restarted it).
async fn wait_ready(client: &reqwest::Client, base: &str) {
    let start = Instant::now();
    loop {
        let answered = client
            .get(format!("{base}/exec"))
            .query(&[("query", "SELECT 1")])
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        if answered {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "QuestDB at {base} did not answer SELECT 1 within 120 s"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Waits until QuestDB has applied every committed WAL transaction of
/// `table`, so a count reads what was written.
async fn wait_applied(client: &reqwest::Client, base: &str, table: &str, deadline: Duration) {
    let sql = format!("SELECT writerTxn, sequencerTxn FROM wal_tables() WHERE name = '{table}'");
    let start = Instant::now();
    loop {
        let body = exec(client, base, &sql).await;
        let row = body.get("dataset").and_then(|d| d.get(0)).cloned();
        if let Some(row) = row.as_ref() {
            let writer = row.get(0).and_then(Value::as_i64);
            if writer.is_some() && writer == row.get(1).and_then(Value::as_i64) {
                return;
            }
        }
        assert!(
            start.elapsed() < deadline,
            "{table}: WAL not applied within {deadline:?} (last: {row:?}); raise \
             TV_TG4_APPLY_DEADLINE_SECS or lower TV_TG4_DEPTH_ROWS"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn depth_row(index: u64, capture_seq: i64, ts_nanos: i64) -> DepthRow {
    let i = index as i64;
    let level = 1 + (i % 20);
    DepthRow {
        security_id: DEPTH_SID_BASE + (i / 40) % DEPTH_SIDS,
        segment: "NSE_FNO",
        depth_kind: DEPTH_KIND_20,
        side: if (i / 20) % 2 == 0 {
            DEPTH_SIDE_BID
        } else {
            DEPTH_SIDE_ASK
        },
        level,
        price: 100.0 + level as f64 * 0.05,
        quantity: 10 * level,
        orders: level,
        capture_seq,
        ts_nanos,
    }
}

/// Writes `rows` depth rows spread over the hour that starts at
/// `hour_start_ist_secs`, flushing every `SEED_FLUSH_ROWS`.
fn seed_depth_hour(cfg: &QuestDbConfig, rows: u64, hour_start_ist_secs: i64, seq_base: i64) {
    let mut writer = DepthWriter::new(cfg, Feed::Dhan);
    let step_nanos = (3_599 * NANOS) / i64::try_from(rows.max(1)).unwrap_or(i64::MAX);
    let start = hour_start_ist_secs * NANOS;
    for index in 0..rows {
        let i = i64::try_from(index).unwrap_or(i64::MAX);
        let row = depth_row(index, seq_base + i, start + i * step_nanos);
        writer.append_row(&row).expect("seed depth append"); // APPROVED: test-only
        if (index + 1) % SEED_FLUSH_ROWS == 0 {
            writer.flush().expect("seed depth flush acknowledged"); // APPROVED: test-only
        }
    }
    writer.flush().expect("seed depth flush acknowledged"); // APPROVED: test-only
    assert_eq!(writer.pending(), 0, "seed depth rows left unflushed");
}

fn tick(index: u64, yesterday_ten_ist_secs: i64) -> ParsedTick {
    // Spread over 10:00-15:00 IST of yesterday, about 1,000 rows a second.
    let secs = yesterday_ten_ist_secs + i64::try_from((index / 1_000) % 18_000).unwrap_or(0);
    ParsedTick {
        security_id: TICK_SID_BASE + index % TICK_SIDS,
        exchange_segment_code: 2,
        last_traded_price: 100.0 + (index % 100) as f32 * 0.05,
        last_trade_quantity: 1,
        exchange_timestamp: u32::try_from(secs).unwrap_or(0),
        received_at_nanos: (secs - IST_OFFSET_SECS) * NANOS,
        volume: u32::try_from(index % 1_000_000).unwrap_or(0),
        ..ParsedTick::default()
    }
}

/// The ILP writer: `rows_per_sec / 2` tick rows every 500 ms, each flush
/// timed. Returns the samples and how many rows it sent.
fn run_tick_writer(
    cfg: &QuestDbConfig,
    rows_per_sec: u64,
    seq_base: i64,
    yesterday_ten_ist_secs: i64,
    stop: &AtomicBool,
) -> (Vec<Sample>, u64) {
    let mut writer = TickWriter::new(cfg, Feed::Dhan);
    let per_flush = (rows_per_sec / 2).max(1);
    let mut samples = Vec::new();
    let mut written: u64 = 0;
    while !stop.load(Ordering::Acquire) {
        let cycle = Instant::now();
        for _ in 0..per_flush {
            let seq = seq_base + i64::try_from(written).unwrap_or(i64::MAX);
            writer
                .append_tick_with_seq(&tick(written, yesterday_ten_ist_secs), seq)
                .expect("tick append"); // APPROVED: test-only
            written += 1;
        }
        let at = Instant::now();
        let ok = writer.flush().is_ok() && writer.pending() == 0;
        samples.push(Sample {
            at,
            took: at.elapsed(),
            ok,
        });
        if let Some(rest) = FLUSH_EVERY.checked_sub(cycle.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    (samples, written)
}

/// Runs `/exec` `SELECT 1` once a second until `stop`.
async fn run_probes(client: reqwest::Client, base: String, stop: Arc<AtomicBool>) -> Vec<Sample> {
    let mut samples = Vec::new();
    while !stop.load(Ordering::Acquire) {
        let at = Instant::now();
        let ok = client
            .get(format!("{base}/exec"))
            .query(&[("query", "SELECT 1")])
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        samples.push(Sample {
            at,
            took: at.elapsed(),
            ok,
        });
        if let Some(rest) = PROBE_EVERY.checked_sub(at.elapsed()) {
            tokio::time::sleep(rest).await;
        }
    }
    samples
}

/// A second `/exp` stream of the seeded hour, read to the end and dropped,
/// again and again until `stop`. Returns (start, took, HTTP status, bytes).
async fn run_second_export(
    client: reqwest::Client,
    base: String,
    sql: String,
    stop: Arc<AtomicBool>,
) -> Vec<(Instant, Duration, u16, u64)> {
    let mut streams = Vec::new();
    while !stop.load(Ordering::Acquire) {
        let at = Instant::now();
        let (status, bytes) = match client
            .get(format!("{base}/exp"))
            .query(&[("query", sql.as_str())])
            .send()
            .await
        {
            Ok(mut response) => {
                let status = response.status().as_u16();
                let mut bytes: u64 = 0;
                while let Ok(Some(chunk)) = response.chunk().await {
                    bytes += chunk.len() as u64;
                }
                (status, bytes)
            }
            Err(_) => (0, 0),
        };
        streams.push((at, at.elapsed(), status, bytes));
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    streams
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a running QuestDB (TV_QDB_REAL_HTTP) and minutes of wall clock; run by the weekly chaos lane"]
async fn tg4_archiver_export_contention_against_real_questdb() {
    let Some(cfg) = qdb() else { return };
    let depth_rows = env_u64("TV_TG4_DEPTH_ROWS", DEFAULT_DEPTH_ROWS);
    let tick_rate = env_u64("TV_TG4_TICK_ROWS_PER_SEC", DEFAULT_TICK_ROWS_PER_SEC);
    let second_export = env_u64("TV_TG4_SECOND_EXPORT", 1) == 1;
    let apply_deadline = Duration::from_secs(env_u64(
        "TV_TG4_APPLY_DEADLINE_SECS",
        DEFAULT_APPLY_DEADLINE_SECS,
    ));
    assert!(depth_rows > 0, "TV_TG4_DEPTH_ROWS must be above 0");

    let base = base_url(&cfg);
    // Long timeout on purpose: a probe slower than the bound is MEASURED, not
    // cut off, so the failure message carries the real figure.
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(600))
        .build()
        .expect("http client"); // APPROVED: test-only
    wait_ready(&client, &base).await;
    assert!(ensure_market_depth_table(&cfg).await, "market_depth ensure");
    assert!(ensure_ticks_table(&cfg).await, "ticks ensure");

    // Dates, in IST-epoch seconds.
    let now_ist = chrono::Utc::now().timestamp() + IST_OFFSET_SECS;
    let today_midnight = now_ist.div_euclid(SECS_PER_DAY) * SECS_PER_DAY;
    let seed_hour = today_midnight - SEED_DAYS_AGO * SECS_PER_DAY + TEN_AM_SECS;
    let yesterday_ten = today_midnight - SECS_PER_DAY + TEN_AM_SECS;
    // Per-run sequence bases, so a row left by an earlier run never merges
    // with this run's rows under DEDUP.
    let run_seq = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(1) / 1_000;
    let depth_seq = run_seq;
    let tick_seq = run_seq + 1_000_000_000_000;
    let seeded_filter = format!(
        "ts >= '{}' AND ts < '{}' AND security_id >= {DEPTH_SID_BASE} AND security_id < {} \
         AND capture_seq >= {depth_seq}",
        iso(seed_hour),
        iso(seed_hour + 3_600),
        DEPTH_SID_BASE + DEPTH_SIDS
    );

    // 1. Seed the hour, and a few recent rows so it is not the active partition.
    let seed_started = Instant::now();
    {
        let cfg = cfg.clone();
        tokio::task::spawn_blocking(move || {
            seed_depth_hour(&cfg, depth_rows, seed_hour, depth_seq);
            seed_depth_hour(&cfg, 40, yesterday_ten, depth_seq + 1_000_000_000_000);
        })
        .await
        .expect("seed task"); // APPROVED: test-only
    }
    wait_applied(&client, &base, "market_depth", apply_deadline).await;
    let seeded = count(
        &client,
        &base,
        &format!("SELECT count() FROM market_depth WHERE {seeded_filter}"),
    )
    .await;
    assert_eq!(
        seeded, depth_rows,
        "seeding: {depth_rows} depth rows written, {seeded} in the hour"
    );
    let seed_took = seed_started.elapsed();

    // 2. The concurrent load.
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let cfg = cfg.clone();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || run_tick_writer(&cfg, tick_rate, tick_seq, yesterday_ten, &stop))
    };
    let probes = tokio::spawn(run_probes(client.clone(), base.clone(), Arc::clone(&stop)));
    let exporter = second_export.then(|| {
        let sql = format!("SELECT * FROM market_depth WHERE {seeded_filter}");
        tokio::spawn(run_second_export(
            client.clone(),
            base.clone(),
            sql,
            Arc::clone(&stop),
        ))
    });
    tokio::time::sleep(MARGIN).await;

    // 3. The real archive pass, against the real QuestDB and the stub S3.
    let objects = Arc::new(Mutex::new(HashMap::new()));
    let (s3_url, _s3_log) = spawn_stub(s3_responder(Arc::clone(&objects))).await;
    let mut archiver = build_archiver(&base, &base, &s3_url, test_retention_cfg(200)).await;
    let pass_start = Instant::now();
    let summary = archiver.archive_and_drop_old_partitions().await;
    let pass_end = Instant::now();

    tokio::time::sleep(MARGIN).await;
    stop.store(true, Ordering::Release);
    let (flushes, ticks_written) = tokio::task::spawn_blocking(move || writer.join())
        .await
        .expect("writer join task") // APPROVED: test-only
        .expect("tick writer thread panicked"); // APPROVED: test-only
    let probes = probes.await.expect("probe task"); // APPROVED: test-only
    let streams = match exporter {
        Some(task) => task.await.expect("second export task"), // APPROVED: test-only
        None => Vec::new(),
    };

    // 4. Evaluate.
    let flushes_in = started_within(&flushes, pass_start, pass_end);
    let probes_in = started_within(&probes, pass_start, pass_end);
    let streams_in = streams
        .iter()
        .filter(|(at, took, _, _)| *at <= pass_end && *at + *took >= pass_start)
        .count();
    // The drop is a WAL transaction on a WAL table: read after it is applied.
    wait_applied(&client, &base, "market_depth", apply_deadline).await;
    let left = count(
        &client,
        &base,
        &format!("SELECT count() FROM market_depth WHERE {seeded_filter}"),
    )
    .await;
    wait_applied(&client, &base, "ticks", apply_deadline).await;
    let ticks_landed = count(
        &client,
        &base,
        &format!(
            "SELECT count() FROM ticks WHERE security_id >= {TICK_SID_BASE} AND security_id < {} \
             AND capture_seq >= {tick_seq}",
            TICK_SID_BASE + TICK_SIDS
        ),
    )
    .await;

    eprintln!("TG-4 archiver export contention against {base}");
    eprintln!("  seeded depth rows        {depth_rows} in {seed_took:?}");
    eprintln!(
        "  archive pass             {:?}: verified {}, dropped {}, failed {}, rows {}, csv bytes {}",
        pass_end - pass_start,
        summary.verified,
        summary.dropped,
        summary.failed,
        summary.rows_archived,
        summary.csv_bytes_exported
    );
    eprintln!(
        "  ILP flushes in pass      {} (all {}), failed {}, p50 {:?}, p99 {:?}, max {:?} (bound {ILP_FLUSH_BOUND:?})",
        flushes_in.len(),
        flushes.len(),
        failures(&flushes),
        percentile(&flushes_in, 0.50),
        percentile(&flushes_in, 0.99),
        max_took(&flushes_in)
    );
    eprintln!(
        "  /exec probes in pass     {} (all {}), failed {}, p50 {:?}, p99 {:?}, max {:?} (bound {PROBE_BOUND:?})",
        probes_in.len(),
        probes.len(),
        failures(&probes),
        percentile(&probes_in, 0.50),
        percentile(&probes_in, 0.99),
        max_took(&probes_in)
    );
    eprintln!(
        "  second /exp streams      {} overlapping the pass; statuses {:?}",
        streams_in,
        streams.iter().map(|s| s.2).collect::<Vec<_>>()
    );
    eprintln!("  tick rows written/landed {ticks_written}/{ticks_landed}");

    // The pass did the real work: it exported, and dropped, the seeded hour.
    assert!(
        summary.pass_ran && !summary.contended,
        "the archive pass did not run (contended {}, ran {})",
        summary.contended,
        summary.pass_ran
    );
    assert!(
        summary.rows_archived >= depth_rows,
        "the pass archived {} rows, below the {depth_rows} seeded: the seeded hour was \
         not exported (failed {}, WAL-gated tables {}), so nothing was contended",
        summary.rows_archived,
        summary.failed,
        summary.tables_wal_not_applied
    );
    assert_eq!(
        left, 0,
        "{left} seeded depth rows are still in QuestDB: the hour was not dropped"
    );

    // Enough overlap to judge.
    assert!(
        flushes_in.len() >= MIN_SAMPLES_IN_PASS && probes_in.len() >= MIN_SAMPLES_IN_PASS,
        "only {} flushes and {} probes started inside the {:?} pass (need {MIN_SAMPLES_IN_PASS} \
         each): the pass was too short to show contention; raise TV_TG4_DEPTH_ROWS",
        flushes_in.len(),
        probes_in.len(),
        pass_end - pass_start
    );
    if second_export {
        assert!(
            streams_in > 0,
            "the second /exp stream never overlapped the pass (statuses {:?})",
            streams.iter().map(|s| s.2).collect::<Vec<_>>()
        );
    }

    // The bounds.
    assert_eq!(
        failures(&flushes),
        0,
        "{} of {} ILP flushes FAILED (in pass: {}); in production their rows go to the \
         rescue spill, the 2026-11-06 symptom",
        failures(&flushes),
        flushes.len(),
        failures(&flushes_in)
    );
    assert!(
        max_took(&flushes_in) <= ILP_FLUSH_BOUND,
        "an ILP flush took {:?} while the archiver exported (bound {ILP_FLUSH_BOUND:?}, \
         p99 {:?}, {} flushes in pass): writes are waiting behind /exp again",
        max_took(&flushes_in),
        percentile(&flushes_in, 0.99),
        flushes_in.len()
    );
    assert_eq!(
        failures(&probes),
        0,
        "{} of {} /exec probes FAILED (in pass: {})",
        failures(&probes),
        probes.len(),
        failures(&probes_in)
    );
    assert!(
        max_took(&probes_in) <= PROBE_BOUND,
        "an /exec probe took {:?} while the archiver exported (bound {PROBE_BOUND:?}, \
         p99 {:?}, {} probes in pass): /exec is waiting behind /exp again",
        max_took(&probes_in),
        percentile(&probes_in, 0.99),
        probes_in.len()
    );
    assert_eq!(
        ticks_landed, ticks_written,
        "{ticks_written} tick rows sent, {ticks_landed} in ticks"
    );
}
