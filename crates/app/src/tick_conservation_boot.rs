//! Daily end-to-end tick-conservation audit (TICK-CONSERVE-01).
//!
//! Operator directive 2026-06-10 (verbatim): *"Go ahead to achieve zero tick
//! loss"* — following the suspicion *"maybe in some cases we are removing
//! ticks … or dropping or … duplicating or … missing or … miss replaying or
//! wal or being buffer or spill or dlq or in memory ram or db"*.
//!
//! At **15:40:00 IST** each trading day (after the 15:31 cross-verify and the
//! post-close flushes) this auditor reconciles the THREE independent tick
//! record stores end-to-end and writes one forensic row to
//! `tick_conservation_audit`:
//!
//! 1. **WAL disk log** — every frame Dhan delivered, captured durably at the
//!    socket (`ws_frame_spill::count_frames_for_ist_day`, read-only scan).
//! 2. **Processor outcome counters** — self-scraped from the app's own
//!    `/metrics` endpoint (loopback).
//! 3. **QuestDB `ticks` rows** — `select count()` windowed to the IST day.
//!
//! The conservation identities (pure functions, unit-tested):
//!
//! ```text
//! delivery_residual = wal_tick_frames − processed
//! outcome_residual  = processed − (persisted + junk + stale_day
//!                                  + outside_hours + dedup + storage_errors)
//! ```
//!
//! Both zero + full-session coverage → `balanced` (`info!`). Either positive
//! → `error!` with `code = TICK-CONSERVE-01` (→ Telegram via the 5-sink
//! chain). Missing sources or a mid-session boot → `partial` (honest "cannot
//! vouch", never a false OK — audit-findings Rule 11).
//!
//! Stage-2 dead-WS sweep (2026-07-17): the processor outcome counters this
//! auditor scrapes (`tv_ticks_processed_total` / `tv_ticks_persisted_total`
//! / the filter counters / `tv_ticks_dropped_total`) were emitted by the
//! now-DELETED Dhan tick chain (`tick_processor.rs` + `tick_persistence.rs`)
//! — no live producer writes new WAL LiveFeed frames or `ticks` rows either.
//! On a post-sweep boot the scrape finds none of those counters, so
//! `identity_complete()` is false and every run honestly records `partial`
//! (never a fabricated `balanced` / false residual). The audit + its table
//! are KEPT per the stage-2 task constraint (the historical WAL/DB record
//! remains reconcilable); retiring the daily run itself is an operator
//! decision deferred with the dashboard/alarm PR.
//!
//! Runbook: `.claude/rules/project/tick-conservation-audit-error-codes.md`.

use std::path::Path;
use std::time::Duration;

use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_storage::tick_conservation_audit_persistence::{
    CONSERVATION_FEED_DHAN, CONSERVATION_FEED_GROWW, ConservationOutcome,
    TickConservationAuditWriter, TickConservationRow, ensure_tick_conservation_audit_table,
};
use tickvault_storage::ws_frame_spill::{WalDayFrameCounts, count_frames_for_ist_day};

/// IST seconds-of-day for the conservation-audit trigger (15:40:00) — after
/// the 15:31 cross-verify and the post-close flush windows.
const CONSERVATION_TRIGGER_SECS_OF_DAY_IST: u32 = 15 * 3600 + 40 * 60; // 56_400

/// IST seconds-of-day for market open (09:00:00). A process that booted at or
/// after this instant cannot have counters covering the full session →
/// `partial_coverage`.
const MARKET_OPEN_SECS_OF_DAY_IST: u32 = 9 * 3600;

/// HTTP timeout for the loopback metrics scrape + QuestDB count query.
const AUDIT_HTTP_TIMEOUT_SECS: u64 = 10;

/// `true` when a process that booted at `boot_secs_of_day_ist` covers the
/// full trading session — i.e. its since-boot counters can vouch for every
/// market-hours tick. Pure; the spawn site captures boot time once.
#[must_use]
pub fn boot_covers_full_session(boot_secs_of_day_ist: u32) -> bool {
    boot_secs_of_day_ist < MARKET_OPEN_SECS_OF_DAY_IST
}

/// The single source of truth for the WS-frame WAL directory — the SAME
/// derivation as main.rs STAGE-C boot wiring. Hostile-review H1: the env
/// derivation was previously copy-pasted at two main.rs sites and could
/// drift; both now call this.
#[must_use]
pub fn ws_wal_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("TV_WS_WAL_DIR").unwrap_or_else(|_| "./data/ws_wal".to_string()), // O(1) EXEMPT: boot-time
    )
}

/// Decision for WHEN the conservation audit should fire. Historically
/// mirrored the retired cross-verify's `CrossVerifyStart` (its
/// `cross_verify_1m_boot.rs` module was deleted in PR-C3, 2026-07-14 —
/// this audit's own start semantics are unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConservationStart {
    /// Not a trading day and not forced → do not run.
    SkipNonTradingDay,
    /// Past 15:40 IST on a trading-day boot → run the day's audit ONCE,
    /// immediately, as a catch-up (audit fix #2, 2026-07-03). Before this,
    /// a boot between 15:40 and midnight IST SKIPPED the audit entirely —
    /// so a post-incident evening boot (e.g. recovery after the 2026-07-02
    /// mid-market wipe) recorded NO forensic WAL-vs-DB row for the day.
    /// The catch-up row is honestly `partial` (a post-09:00 boot's counters
    /// cannot vouch for the session), but the durable WAL frame count and
    /// the QuestDB row count for the day ARE captured — exactly the
    /// delivered-vs-stored forensic signal a wipe investigation needs.
    /// Idempotency: one run per boot; the audit table's DEDUP UPSERT KEYS
    /// `(ts, trading_date_ist, feed)` absorb exact re-writes, and a second
    /// boot the same evening appends its own honest row.
    RunCatchUp,
    /// Run immediately — operator forced an on-demand run.
    RunNow,
    /// Sleep this many seconds, then run at 15:40:00 IST.
    SleepThenRun(u64),
}

/// Pure decision: when should the conservation audit fire.
///
/// `force_now` (the `TICKVAULT_TICK_CONSERVE_NOW` env var) overrides both
/// gates so the operator can prove the pipeline on demand; a forced run on a
/// quiet day simply produces a `partial`/zero-count row, never fabricated
/// numbers.
#[must_use]
pub fn decide_conservation_start(
    now_secs_of_day_ist: u32,
    is_trading_day: bool,
    force_now: bool,
) -> ConservationStart {
    if force_now {
        return ConservationStart::RunNow;
    }
    if !is_trading_day {
        return ConservationStart::SkipNonTradingDay;
    }
    if now_secs_of_day_ist >= CONSERVATION_TRIGGER_SECS_OF_DAY_IST {
        return ConservationStart::RunCatchUp;
    }
    ConservationStart::SleepThenRun(u64::from(
        CONSERVATION_TRIGGER_SECS_OF_DAY_IST - now_secs_of_day_ist,
    ))
}

/// Parses one counter value out of a Prometheus exposition body. Pure.
///
/// Matches the EXACT metric name as a whole token (no label set — the tick
/// outcome counters are all label-free) and parses its value as f64 →
/// truncated u64 (exporter renders counters as floats, e.g. `1.23e6`).
/// Returns `None` when the metric is absent or unparseable — the caller
/// treats that as `partial` coverage, never as zero.
#[must_use]
pub fn parse_prom_counter(body: &str, name: &str) -> Option<u64> {
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(metric), Some(raw)) = (parts.next(), parts.next()) else {
            continue;
        };
        if metric == name {
            let v = raw.parse::<f64>().ok()?;
            if !v.is_finite() || v < 0.0 {
                return None;
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            // APPROVED: counter values are non-negative finite; u64 truncation
            // of an exact integral float is the intended conversion.
            return Some(v as u64);
        }
    }
    None
}

/// The processor outcome counters scraped from `/metrics`. Every field is
/// `Option` — a missing counter marks the run `partial`, never silently zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutcomeCounters {
    pub processed: Option<u64>,
    pub persisted: Option<u64>,
    pub junk: Option<u64>,
    pub stale_day: Option<u64>,
    pub outside_hours: Option<u64>,
    pub dedup: Option<u64>,
    pub parse_errors: Option<u64>,
    pub storage_errors: Option<u64>,
    pub dropped_total: Option<u64>,
}

impl OutcomeCounters {
    /// Parses all nine counters from one exposition body. Pure.
    #[must_use]
    pub fn from_metrics_body(body: &str) -> Self {
        Self {
            processed: parse_prom_counter(body, "tv_ticks_processed_total"),
            persisted: parse_prom_counter(body, "tv_ticks_persisted_total"),
            junk: parse_prom_counter(body, "tv_junk_ticks_filtered_total"),
            stale_day: parse_prom_counter(body, "tv_stale_day_filtered_total"),
            outside_hours: parse_prom_counter(body, "tv_outside_hours_filtered_total"),
            dedup: parse_prom_counter(body, "tv_dedup_filtered_total"),
            parse_errors: parse_prom_counter(body, "tv_parse_errors_total"),
            storage_errors: parse_prom_counter(body, "tv_storage_errors_total"),
            dropped_total: parse_prom_counter(body, "tv_ticks_dropped_total"),
        }
    }

    /// `true` when every identity-bearing counter is present. (`parse_errors`
    /// and `dropped_total` are reported but not part of the tick identities —
    /// parse errors never became ticks; drops are a subset of storage flow.)
    #[must_use]
    pub fn identity_complete(&self) -> bool {
        self.processed.is_some()
            && self.persisted.is_some()
            && self.junk.is_some()
            && self.stale_day.is_some()
            && self.outside_hours.is_some()
            && self.dedup.is_some()
            && self.storage_errors.is_some()
    }
}

/// The two conservation residuals. Pure.
///
/// `delivery_residual = wal_tick_frames − (processed − replay_recovered)` —
/// positive means frames Dhan delivered (durably in the WAL) never reached
/// the processor. Hostile-review H2: `processed` counts BOTH live-path
/// ticks AND boot-replayed frames from earlier sessions, so the boot
/// replay count (scraped `tv_wal_replay_recovered_total`) is subtracted
/// (bounded by `processed`) before comparing against today's WAL count.
/// `outcome_residual = processed − (persisted + junk + stale_day +
/// outside_hours + dedup + storage_errors)` — positive means an in-process
/// leak. NEGATIVE residuals mean the identity is not clean (replay
/// inflation beyond the adjustment, rescue-drain double counting) — the
/// caller classifies those as `partial`, never as a silent "balanced"
/// (hostile-review H2: a negative bias of N could otherwise mask a real
/// same-day leak ≤ N).
#[must_use]
pub fn compute_residuals(
    wal_tick_frames: u64,
    replay_recovered: u64,
    c: &OutcomeCounters,
) -> (i64, i64) {
    let processed = c.processed.unwrap_or(0);
    let processed_live = processed.saturating_sub(replay_recovered.min(processed));
    let accounted = c
        .persisted
        .unwrap_or(0)
        .saturating_add(c.junk.unwrap_or(0))
        .saturating_add(c.stale_day.unwrap_or(0))
        .saturating_add(c.outside_hours.unwrap_or(0))
        .saturating_add(c.dedup.unwrap_or(0))
        .saturating_add(c.storage_errors.unwrap_or(0));
    // APPROVED: i64::try_from of realistic daily counts (≪ i64::MAX); clamp
    // keeps the arithmetic total for adversarial inputs.
    let to_i64 = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let delivery = to_i64(wal_tick_frames).saturating_sub(to_i64(processed_live));
    let outcome = to_i64(processed).saturating_sub(to_i64(accounted));
    (delivery, outcome)
}

/// Final verdict. Pure.
///
/// `partial` wins over `leak`: when coverage is incomplete the residuals are
/// not trustworthy numbers, so the row says "cannot vouch" instead of paging
/// with arithmetic built on missing inputs (false-OK Rule 11 — and equally,
/// no false ALARM from a known-incomplete identity). NEGATIVE residuals are
/// likewise `partial` (hostile-review H2): an inflated counter set cannot
/// vouch for the day, and reporting it "balanced" would let the negative
/// bias mask a real leak of equal size.
#[must_use]
pub fn classify_outcome(
    delivery_residual: i64,
    outcome_residual: i64,
    partial_coverage: bool,
) -> ConservationOutcome {
    if partial_coverage {
        return ConservationOutcome::Partial;
    }
    if delivery_residual > 0 || outcome_residual > 0 {
        return ConservationOutcome::Leak;
    }
    if delivery_residual < 0 || outcome_residual < 0 {
        return ConservationOutcome::Partial;
    }
    ConservationOutcome::Balanced
}

/// Parses the QuestDB `/exec` count response (`{"dataset":[[N]]}`). Pure.
#[must_use]
pub fn parse_questdb_count(body: &str) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("dataset")?.get(0)?.get(0)?.as_i64()
}

/// Runs the end-to-end conservation audit once. Cold path (once/day).
///
/// Fail-soft everywhere: a missing source flips the row to `partial` and the
/// run still records whatever it could measure — absence of a source is
/// never camouflaged as a zero.
// APPROVED: cold-path orchestrator — 7 independent live inputs (wal dir, questdb cfg, metrics port, day numbers, run ts, boot coverage flag); bundling would only relocate the arity.
#[allow(clippy::too_many_arguments)]
// TEST-EXEMPT: orchestration over unit-tested pure parts (decide/parse/residuals/classify/persistence); a direct test needs live QuestDB + metrics endpoints — covered operationally by TICKVAULT_TICK_CONSERVE_NOW
pub async fn run_tick_conservation_audit(
    wal_dir: &Path,
    questdb_config: &QuestDbConfig,
    metrics_port: u16,
    target_ist_day: u64,
    trading_date_ist_nanos: i64,
    run_ts_ist_nanos: i64,
    boot_covered_full_session: bool,
) {
    ensure_tick_conservation_audit_table(questdb_config).await;

    // 1. WAL disk-log counts (read-only scan; blocking file I/O off the
    //    async worker via spawn_blocking).
    let wal_dir_owned = wal_dir.to_path_buf();
    let wal: WalDayFrameCounts = match tokio::task::spawn_blocking(move || {
        count_frames_for_ist_day(&wal_dir_owned, target_ist_day)
    })
    .await
    {
        Ok(c) => c,
        Err(err) => {
            error!(?err, "tick_conservation: WAL count task failed");
            WalDayFrameCounts::default()
        }
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(AUDIT_HTTP_TIMEOUT_SECS))
        .build()
        .ok();

    // Hostile-review H1/M2: a dead or degraded WAL source must flip the
    // verdict to `partial`, never report zero-counts as "balanced". A scan
    // that found NO segments while ticks were processed, or hit ANY
    // unreadable segment, cannot vouch for the delivery identity.
    let mut sources_complete = true;
    let mut wal_scan_healthy = wal.corrupted_segments == 0;

    // 2. Self-scrape the outcome counters from the app's own exporter.
    let mut counters = OutcomeCounters::default();
    let mut replay_recovered: u64 = 0;
    if let Some(ref client) = client {
        let url = format!("http://127.0.0.1:{metrics_port}/metrics");
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.text().await {
                Ok(body) => {
                    counters = OutcomeCounters::from_metrics_body(&body);
                    // H2: boot-replay inflation adjustment for the delivery
                    // identity (frames re-injected from earlier sessions).
                    replay_recovered =
                        parse_prom_counter(&body, "tv_wal_replay_recovered_total").unwrap_or(0);
                }
                Err(err) => {
                    warn!(?err, "tick_conservation: metrics body read failed");
                }
            },
            Ok(resp) => warn!(status = %resp.status(), "tick_conservation: metrics scrape non-2xx"),
            Err(err) => warn!(?err, "tick_conservation: metrics scrape failed"),
        }
    }
    if !counters.identity_complete() {
        sources_complete = false;
    }
    if wal.segments_scanned == 0 && counters.processed.unwrap_or(0) > 0 {
        // Ticks flowed but the WAL scan saw nothing — wrong dir / dead
        // writer / wiped disk. The delivery identity is meaningless.
        wal_scan_healthy = false;
    }
    if !wal_scan_healthy {
        sources_complete = false;
        warn!(
            segments_scanned = wal.segments_scanned,
            corrupted_segments = wal.corrupted_segments,
            "tick_conservation: WAL scan degraded — verdict capped at partial"
        );
    }

    // 3. QuestDB row count for the IST day window. `ts` stores IST-epoch
    //    nanos, so the day window is [day*86400, (day+1)*86400) seconds in
    //    ts-space (data-integrity.md). Feed-filtered to 'dhan' — the shared
    //    `ticks` table also carries feed='groww' rows, which would otherwise
    //    inflate this lane's db_rows and mask the runbook's
    //    "db_rows vs persisted ⇒ DEDUP collapse / WAL lag" triage.
    let mut db_rows: i64 = -1;
    if let Some(ref client) = client {
        let sql = build_conservation_ticks_count_sql(CONSERVATION_FEED_DHAN, target_ist_day);
        let url = format!(
            "http://{}:{}/exec",
            questdb_config.host, questdb_config.http_port
        );
        match client
            .get(&url)
            .query(&[("query", sql.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(body) = resp.text().await {
                    db_rows = parse_questdb_count(&body).unwrap_or(-1);
                }
            }
            Ok(resp) => warn!(status = %resp.status(), "tick_conservation: ticks count non-2xx"),
            Err(err) => warn!(?err, "tick_conservation: ticks count query failed"),
        }
    }
    if db_rows < 0 {
        sources_complete = false;
    }

    // 4. Residuals + verdict (pure).
    let (delivery_residual, outcome_residual) =
        compute_residuals(wal.tick_frames, replay_recovered, &counters);
    let partial_coverage = !boot_covered_full_session || !sources_complete;
    let outcome = classify_outcome(delivery_residual, outcome_residual, partial_coverage);

    metrics::counter!("tv_tick_conservation_audit_runs_total", "outcome" => outcome.as_str())
        .increment(1);

    let to_i64 = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let row = TickConservationRow {
        run_ts_ist_nanos,
        trading_date_ist_nanos,
        wal_tick_frames: to_i64(wal.tick_frames),
        wal_other_frames: to_i64(wal.other_frames),
        wal_unattributable: to_i64(wal.unattributable),
        db_rows,
        processed: to_i64(counters.processed.unwrap_or(0)),
        persisted: to_i64(counters.persisted.unwrap_or(0)),
        junk: to_i64(counters.junk.unwrap_or(0)),
        stale_day: to_i64(counters.stale_day.unwrap_or(0)),
        outside_hours: to_i64(counters.outside_hours.unwrap_or(0)),
        dedup: to_i64(counters.dedup.unwrap_or(0)),
        parse_errors: to_i64(counters.parse_errors.unwrap_or(0)),
        storage_errors: to_i64(counters.storage_errors.unwrap_or(0)),
        dropped_total: to_i64(counters.dropped_total.unwrap_or(0)),
        delivery_residual,
        outcome_residual,
        partial_coverage,
        outcome,
        // operator override 2026-06-28: feed in-key. The 15:40 IST conservation
        // audit reconciles the Dhan stores today.
        feed: CONSERVATION_FEED_DHAN,
    };

    // 5. Operator signal (audit Rule 5: a leak is error! with code).
    match outcome {
        ConservationOutcome::Leak => {
            error!(
                code = ErrorCode::TickConserve01DailyResidual.code_str(),
                wal_tick_frames = wal.tick_frames,
                processed = counters.processed.unwrap_or(0),
                persisted = counters.persisted.unwrap_or(0),
                db_rows,
                delivery_residual,
                outcome_residual,
                "TICK-CONSERVE-01: daily conservation residual — ticks delivered \
                 by Dhan did not all reach a known outcome (WAL still holds them; \
                 see tick_conservation_audit + runbook)"
            );
        }
        ConservationOutcome::Partial => {
            warn!(
                wal_tick_frames = wal.tick_frames,
                db_rows,
                boot_covered_full_session,
                sources_complete,
                "tick_conservation: PARTIAL coverage — cannot vouch for the full \
                 session (mid-day boot or a source unavailable); row recorded"
            );
        }
        ConservationOutcome::Balanced => {
            info!(
                wal_tick_frames = wal.tick_frames,
                wal_other_frames = wal.other_frames,
                db_rows,
                processed = counters.processed.unwrap_or(0),
                persisted = counters.persisted.unwrap_or(0),
                "tick conservation BALANCED — every tick Dhan delivered reached a \
                 known outcome (WAL == processed == persisted + filters; see \
                 tick_conservation_audit)"
            );
        }
    }

    // 6. Forensic row (fail-soft).
    let mut writer = TickConservationAuditWriter::new(questdb_config);
    if let Err(err) = writer.append_row(&row) {
        error!(?err, "tick_conservation: audit row append failed");
        return;
    }
    if let Err(err) = writer.flush() {
        error!(
            ?err,
            "tick_conservation: audit row flush failed (QuestDB down?)"
        );
    }
}

// ---------------------------------------------------------------------------
// Groww lane conservation audit (feed='groww')
// ---------------------------------------------------------------------------

/// Nanoseconds in one IST day — the day-window width for the RUST-SIDE
/// nanos-vs-nanos scans (the NDJSON line counter). NEVER embed this in a
/// QuestDB literal — see the regression lock on
/// [`build_conservation_ticks_count_sql`].
const NANOS_PER_IST_DAY: i64 = 86_400_000_000_000;

/// Build the feed-filtered per-IST-day `ticks` count SQL for a conservation
/// run. Pure. BOTH lanes go through this so neither query can silently drop
/// the `feed` predicate again — the shared `ticks` table carries every feed's
/// rows, and an unfiltered count inflates one lane's `db_rows` with the other
/// lane's ticks (2026-07-02 adversarial-sweep finding). `feed` is a compile-time
/// constant (`CONSERVATION_FEED_*`), never user input — no injection surface.
///
/// REGRESSION LOCK (hostile review 2026-07-10, empirically confirmed on the
/// pinned QuestDB 9.3.5): a bare integer literal compared against a TIMESTAMP
/// column is interpreted as epoch **MICROSECONDS**, not nanoseconds. The
/// previous NANOSECOND bounds placed the window ~year 58502 and matched ZERO
/// rows — `db_rows = 0` on every run: the Dhan `tick_conservation_audit`
/// forensic column was silently wrong (runbook triage step 5 dead), and the
/// Groww lane's `delivery_residual` equalled the full delivered NDJSON count
/// so `classify_groww_outcome` was permanently Partial (audit-Rule-11
/// false-signal class). The bounds come from the SHARED
/// [`crate::feed_scoreboard_boot::day_bounds_micros`] helper (the single
/// micros source — no per-file nanos re-derivation can regress); the
/// Rust-side NDJSON counter (`count_groww_ndjson_lines_for_ist_day`) compares
/// nanos-vs-nanos in memory and stays nanos. Pinned (digit-magnitude, not
/// substring presence) by `test_build_conservation_ticks_count_sql_feed_filtered`.
#[must_use]
pub fn build_conservation_ticks_count_sql(feed: &str, target_ist_day: u64) -> String {
    let (day_start_micros, day_end_micros) =
        crate::feed_scoreboard_boot::day_bounds_micros(target_ist_day);
    format!(
        "select count() from ticks where feed = '{feed}' \
         and ts >= {day_start_micros} and ts < {day_end_micros}"
    )
}

/// Extract `ts_ist_nanos` from one Groww NDJSON tick line. Pure. Returns `None`
/// for a malformed / ts-less / non-positive line (skipped, never panics).
#[must_use]
pub fn parse_groww_line_ts(line: &str) -> Option<i64> {
    #[derive(serde::Deserialize)]
    struct TsOnly {
        ts_ist_nanos: i64,
    }
    serde_json::from_str::<TsOnly>(line.trim())
        .ok()
        .map(|t| t.ts_ist_nanos)
        .filter(|&t| t > 0)
}

/// Count Groww NDJSON lines whose `ts_ist_nanos` floors to `target_ist_day`.
/// The sidecar's fsync'd append-only NDJSON is the durable DELIVERED-count
/// ground truth for the Groww lane (mirrors Dhan's on-disk WAL frame count;
/// survives a restart). Pure over the file contents. Missing file → 0; a
/// malformed / ts-less line is skipped. Blocking file I/O — the caller wraps
/// this in `spawn_blocking`.
#[must_use]
pub fn count_groww_ndjson_lines_for_ist_day(path: &Path, target_ist_day: u64) -> u64 {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else {
        return 0; // Groww disabled / first boot — a valid "0 delivered" measurement.
    };
    // APPROVED: IST day numbers are ~20K, far below i64::MAX — the cast + muls cannot overflow realistically; saturating keeps adversarial inputs bounded.
    let day_start = i64::try_from(target_ist_day)
        .unwrap_or(0)
        .saturating_mul(86_400)
        .saturating_mul(1_000_000_000);
    let day_end = day_start.saturating_add(NANOS_PER_IST_DAY);
    let mut count: u64 = 0;
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(ts) = parse_groww_line_ts(&line)
            && ts >= day_start
            && ts < day_end
        {
            count = count.saturating_add(1);
        }
    }
    count
}

/// Groww conservation verdict. Pure.
///
/// UNLIKE the Dhan `classify_outcome` (which pages `Leak` on any positive
/// residual because the WAL is a strict one-write-per-frame ground truth), a
/// positive Groww residual (`ndjson_lines − persisted_groww`) is EXPECTED for
/// BENIGN reasons — the in-flight NDJSON tail not yet drained at 15:40, and
/// lines the bridge's stricter validation rejects before persist (e.g. a
/// timestamp outside its accepted range). A same-day restart is residual-
/// NEUTRAL: the bridge's byte-offset is in-memory only, so it re-tails the
/// whole file from byte 0, but the deterministic `capture_seq` (seeded from
/// `ts_ist_nanos`) regenerates identical keys → QuestDB DEDUP collapses the
/// replay; neither the NDJSON count nor db_rows moves. So a
/// non-zero residual is classified `Partial` (a recorded DIAGNOSTIC), NEVER
/// `Leak` — this deliberately avoids the false-loss CRITICAL page a naive
/// `residual>0 ⇒ leak` would produce. The forensic row + the numbers are still
/// written every day (the 100%-audit-coverage requirement); leak-THRESHOLD
/// precision is on-box-tuned later, exactly as the Dhan audit was validated at
/// 15:40 IST on a real box.
#[must_use]
pub fn classify_groww_outcome(
    delivery_residual: i64,
    partial_coverage: bool,
) -> ConservationOutcome {
    if partial_coverage {
        return ConservationOutcome::Partial;
    }
    if delivery_residual == 0 {
        return ConservationOutcome::Balanced;
    }
    // Non-zero (either sign) → diagnostic, never a false CRITICAL leak.
    ConservationOutcome::Partial
}

/// Runs the Groww lane's daily conservation audit once. Cold path (once/day at
/// 15:40 IST, right after the Dhan run). Reconciles the sidecar NDJSON
/// delivered-count vs the persisted `feed='groww'` ticks for the IST day, writes
/// one forensic row to `tick_conservation_audit` tagged `feed='groww'`.
///
/// Fail-soft everywhere: a missing NDJSON file → 0 delivered; QuestDB
/// unreachable → `db_rows=-1` → `partial`. Never panics, never a false leak.
// APPROVED: cold-path orchestrator — 6 independent live inputs; bundling only relocates the arity.
#[allow(clippy::too_many_arguments)]
// TEST-EXEMPT: orchestration over the unit-tested pure parts (count/classify/persistence); a direct test needs a live QuestDB — the pure helpers below ARE tested.
pub async fn run_groww_tick_conservation_audit(
    ndjson_path: &Path,
    questdb_config: &QuestDbConfig,
    target_ist_day: u64,
    trading_date_ist_nanos: i64,
    run_ts_ist_nanos: i64,
    boot_covered_full_session: bool,
) {
    ensure_tick_conservation_audit_table(questdb_config).await;

    // 1. DELIVERED — Groww NDJSON line count for the IST day (on-disk ground
    //    truth; blocking scan off the async worker).
    let path_owned = ndjson_path.to_path_buf();
    let ndjson_lines: u64 = match tokio::task::spawn_blocking(move || {
        count_groww_ndjson_lines_for_ist_day(&path_owned, target_ist_day)
    })
    .await
    {
        Ok(n) => n,
        Err(err) => {
            warn!(?err, "groww_conservation: ndjson scan task failed");
            0
        }
    };

    // 2. PERSISTED — feed='groww' ticks in QuestDB for the IST day. Bounded
    //    client: `Client::new()` has NO total-request timeout, so a wedged
    //    QuestDB could park this cold-path task forever (2026-07-02 sweep).
    let mut groww_db_rows: i64 = -1;
    if let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(AUDIT_HTTP_TIMEOUT_SECS))
        .build()
    {
        let sql = build_conservation_ticks_count_sql(CONSERVATION_FEED_GROWW, target_ist_day);
        let url = format!(
            "http://{}:{}/exec",
            questdb_config.host, questdb_config.http_port
        );
        match client
            .get(&url)
            .query(&[("query", sql.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(body) = resp.text().await {
                    groww_db_rows = parse_questdb_count(&body).unwrap_or(-1);
                }
            }
            Ok(resp) => warn!(status = %resp.status(), "groww_conservation: ticks count non-2xx"),
            Err(err) => warn!(?err, "groww_conservation: ticks count query failed"),
        }
    }

    // 3. Residual + verdict (Groww-specific: a positive residual is a recorded
    //    diagnostic, NEVER a false CRITICAL leak).
    let sources_complete = groww_db_rows >= 0;
    let partial_coverage = !boot_covered_full_session || !sources_complete;
    let delivery_residual = i64::try_from(ndjson_lines)
        .unwrap_or(i64::MAX)
        .saturating_sub(groww_db_rows.max(0));
    let outcome = classify_groww_outcome(delivery_residual, partial_coverage);

    metrics::counter!("tv_tick_conservation_audit_runs_total", "outcome" => outcome.as_str(), "feed" => CONSERVATION_FEED_GROWW).increment(1);

    let to_i64 = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let row = TickConservationRow {
        run_ts_ist_nanos,
        trading_date_ist_nanos,
        // `wal_tick_frames` carries the Groww DELIVERED count (NDJSON lines) —
        // the on-disk ground truth analogous to the Dhan WAL frame count.
        wal_tick_frames: to_i64(ndjson_lines),
        wal_other_frames: 0,
        wal_unattributable: 0,
        db_rows: groww_db_rows,
        // The Groww bridge is a simple tail→persist path with no intermediate
        // processed/junk/stale classification stage, so these Dhan-pipeline
        // counters are 0 for the Groww lane (delivered vs db_rows is the signal).
        processed: 0,
        persisted: 0,
        junk: 0,
        stale_day: 0,
        outside_hours: 0,
        dedup: 0,
        parse_errors: 0,
        storage_errors: 0,
        dropped_total: 0,
        delivery_residual,
        outcome_residual: 0,
        partial_coverage,
        outcome,
        feed: CONSERVATION_FEED_GROWW,
    };

    // 4. Operator signal — INFO only (a positive Groww residual is benign; the
    //    row + numbers are the audit coverage, no false CRITICAL page).
    info!(
        ndjson_lines,
        groww_db_rows,
        delivery_residual,
        outcome = outcome.as_str(),
        partial_coverage,
        "groww tick conservation: reconciled capture NDJSON delivered-count vs \
         persisted feed='groww' ticks (DORMANT since 2026-07-15 — the live-feed \
         capture producer was deleted; row lands in tick_conservation_audit)"
    );

    // 5. Forensic row (fail-soft).
    let mut writer = TickConservationAuditWriter::new(questdb_config);
    if let Err(err) = writer.append_row(&row) {
        error!(?err, "groww_conservation: audit row append failed");
        return;
    }
    if let Err(err) = writer.flush() {
        error!(
            ?err,
            "groww_conservation: audit row flush failed (QuestDB down?)"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Groww conservation audit (feed='groww') pure helpers ──

    /// Day 20601 (an arbitrary IST day number); its start in ts_ist_nanos.
    const GROWW_TEST_DAY: u64 = 20_601;
    fn day_ts(day: u64, secs_into_day: i64) -> i64 {
        (day as i64) * 86_400 * 1_000_000_000 + secs_into_day * 1_000_000_000
    }

    /// Guard that removes its unique temp dir on drop (no `tempfile` dep — mirrors
    /// the `unique_dir()` pattern in `crates/app/tests/groww_live_pipeline_e2e.rs`).
    struct TmpDir(std::path::PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    static NDJSON_TMP_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn write_ndjson(lines: &[String]) -> (TmpDir, std::path::PathBuf) {
        let n = NDJSON_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("tv_groww_conserve_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("live-ticks.ndjson");
        std::fs::write(&path, lines.join("\n")).expect("write ndjson");
        (TmpDir(dir), path)
    }

    #[test]
    fn test_build_conservation_ticks_count_sql_feed_filtered() {
        // Both lanes' counts MUST be feed-filtered — the shared `ticks` table
        // carries every feed's rows (2026-07-02 sweep: the unfiltered Dhan
        // count silently included Groww rows on a both-feeds day).
        let dhan = build_conservation_ticks_count_sql(CONSERVATION_FEED_DHAN, GROWW_TEST_DAY);
        let groww = build_conservation_ticks_count_sql(CONSERVATION_FEED_GROWW, GROWW_TEST_DAY);
        assert!(
            dhan.contains("feed = 'dhan'"),
            "Dhan count must filter feed: {dhan}"
        );
        assert!(
            groww.contains("feed = 'groww'"),
            "Groww count must filter feed: {groww}"
        );
        // REGRESSION LOCK (2026-07-10, proven live on QuestDB 9.3.5): the
        // embedded day-window literals MUST be epoch MICROS (16 digits for a
        // 2026 day) — the old 19-digit NANOS literals sat ~year 58502 and
        // matched ZERO rows (db_rows=0 forever; Groww permanently Partial).
        // Exact micros day window bounds: [day*86400e6, (day+1)*86400e6).
        let start = (GROWW_TEST_DAY as i64) * 86_400 * 1_000_000;
        let end = start + 86_400_000_000;
        assert_eq!(
            start.to_string().len(),
            16,
            "2026-era micros bound must be 16 digits"
        );
        let nanos_start = (GROWW_TEST_DAY as i64) * 86_400 * 1_000_000_000;
        for sql in [&dhan, &groww] {
            assert!(
                sql.contains(&format!("ts >= {start}")),
                "window start: {sql}"
            );
            assert!(sql.contains(&format!("ts < {end}")), "window end: {sql}");
            assert!(
                !sql.contains(&nanos_start.to_string()),
                "nanos literal banned: {sql}"
            );
            assert!(
                !sql.contains(&(nanos_start + 86_400_000_000_000).to_string()),
                "nanos end literal banned: {sql}"
            );
        }
    }

    #[test]
    fn test_parse_groww_line_ts_extracts_and_filters() {
        assert_eq!(
            parse_groww_line_ts(r#"{"security_id":13,"ts_ist_nanos":1780000000000000000}"#),
            Some(1_780_000_000_000_000_000)
        );
        // Malformed / missing field / non-positive → None (skipped, no panic).
        assert_eq!(parse_groww_line_ts("not json"), None);
        assert_eq!(parse_groww_line_ts(r#"{"security_id":13}"#), None);
        assert_eq!(parse_groww_line_ts(r#"{"ts_ist_nanos":0}"#), None);
    }

    #[test]
    fn test_count_groww_ndjson_lines_for_ist_day_in_day() {
        // 3 lines inside GROWW_TEST_DAY (09:15, 12:00, 15:29) → count 3.
        let lines: Vec<String> = [33_300, 43_200, 55_740]
            .iter()
            .map(|s| {
                format!(
                    r#"{{"security_id":13,"ts_ist_nanos":{}}}"#,
                    day_ts(GROWW_TEST_DAY, *s)
                )
            })
            .collect();
        let (_d, path) = write_ndjson(&lines);
        assert_eq!(
            count_groww_ndjson_lines_for_ist_day(&path, GROWW_TEST_DAY),
            3
        );
    }

    #[test]
    fn test_groww_ndjson_line_count_excludes_other_day() {
        // 2 in-day + 2 next-day + 1 prev-day → only 2 counted for GROWW_TEST_DAY.
        let mut lines: Vec<String> = vec![
            format!(r#"{{"ts_ist_nanos":{}}}"#, day_ts(GROWW_TEST_DAY, 40_000)),
            format!(r#"{{"ts_ist_nanos":{}}}"#, day_ts(GROWW_TEST_DAY, 50_000)),
        ];
        lines.push(format!(
            r#"{{"ts_ist_nanos":{}}}"#,
            day_ts(GROWW_TEST_DAY + 1, 100)
        ));
        lines.push(format!(
            r#"{{"ts_ist_nanos":{}}}"#,
            day_ts(GROWW_TEST_DAY + 1, 200)
        ));
        lines.push(format!(
            r#"{{"ts_ist_nanos":{}}}"#,
            day_ts(GROWW_TEST_DAY - 1, 80_000)
        ));
        let (_d, path) = write_ndjson(&lines);
        assert_eq!(
            count_groww_ndjson_lines_for_ist_day(&path, GROWW_TEST_DAY),
            2
        );
    }

    #[test]
    fn test_groww_ndjson_line_count_skips_malformed_and_blank() {
        let lines = vec![
            format!(r#"{{"ts_ist_nanos":{}}}"#, day_ts(GROWW_TEST_DAY, 40_000)),
            "not json at all".to_string(),
            String::new(),
            r#"{"security_id":13}"#.to_string(), // no ts field
            format!(r#"{{"ts_ist_nanos":{}}}"#, day_ts(GROWW_TEST_DAY, 41_000)),
        ];
        let (_d, path) = write_ndjson(&lines);
        assert_eq!(
            count_groww_ndjson_lines_for_ist_day(&path, GROWW_TEST_DAY),
            2,
            "malformed/blank/ts-less lines skipped without panic"
        );
    }

    #[test]
    fn test_groww_ndjson_line_count_missing_file_is_zero() {
        let missing = std::path::Path::new("/nonexistent/groww/live-ticks.ndjson");
        assert_eq!(
            count_groww_ndjson_lines_for_ist_day(missing, GROWW_TEST_DAY),
            0
        );
    }

    #[test]
    fn test_classify_groww_outcome_positive_residual_is_partial_not_leak() {
        // The false-alarm guard: a positive residual (tail / DEDUP-collapse) must
        // classify Partial (diagnostic), NEVER Leak — Groww's NDJSON is not a
        // strict one-write-per-tick ground truth like the Dhan WAL.
        assert_eq!(
            classify_groww_outcome(10, false),
            ConservationOutcome::Partial,
            "positive Groww residual must NOT be a false CRITICAL leak"
        );
        // Exact match → Balanced.
        assert_eq!(
            classify_groww_outcome(0, false),
            ConservationOutcome::Balanced
        );
        // Incomplete sources → Partial regardless.
        assert_eq!(
            classify_groww_outcome(0, true),
            ConservationOutcome::Partial
        );
        // Negative residual (db has more, e.g. prior-session rows) → Partial, not leak.
        assert_eq!(
            classify_groww_outcome(-5, false),
            ConservationOutcome::Partial
        );
    }

    #[test]
    fn test_decide_conservation_start_before_after_force() {
        // 15:39:00 IST → sleep 60s to 15:40:00.
        let now = 15 * 3600 + 39 * 60;
        assert_eq!(
            decide_conservation_start(now, true, false),
            ConservationStart::SleepThenRun(60)
        );
        // At/after the trigger on a trading day → catch-up run (audit fix #2,
        // 2026-07-03; was SkipPastTrigger — a late boot skipped the day's
        // audit entirely, leaving no forensic row after an evening recovery).
        assert_eq!(
            decide_conservation_start(CONSERVATION_TRIGGER_SECS_OF_DAY_IST, true, false),
            ConservationStart::RunCatchUp
        );
        // Non-trading day → skip.
        assert_eq!(
            decide_conservation_start(now, false, false),
            ConservationStart::SkipNonTradingDay
        );
        // Force overrides both gates.
        assert_eq!(
            decide_conservation_start(now + 7200, false, true),
            ConservationStart::RunNow
        );
        // Trigger constant pin: 15:40:00 IST.
        assert_eq!(CONSERVATION_TRIGGER_SECS_OF_DAY_IST, 56_400);
        assert_eq!(MARKET_OPEN_SECS_OF_DAY_IST, 32_400);
        // Boot-coverage helper: pre-09:00 boot covers the session; later not.
        assert!(boot_covers_full_session(8 * 3600 + 35 * 60));
        assert!(!boot_covers_full_session(MARKET_OPEN_SECS_OF_DAY_IST));
        assert!(!boot_covers_full_session(11 * 3600));
    }

    #[test]
    fn test_decide_conservation_start_late_boot_catches_up() {
        // Audit fix #2 (2026-07-03): a trading-day boot ANYWHERE in
        // [15:40:00, midnight) runs the day's audit once, immediately —
        // previously skipped, so a post-incident evening boot recorded no
        // forensic WAL-vs-DB row for the day.
        for late in [
            15 * 3600 + 40 * 60,      // 15:40:00 — exactly the trigger
            16 * 3600 + 30 * 60,      // 16:30:00 — post-close recovery boot
            23 * 3600 + 59 * 60 + 59, // 23:59:59 — last second of the day
        ] {
            assert_eq!(
                decide_conservation_start(late, true, false),
                ConservationStart::RunCatchUp,
                "late boot at {late}s must catch up"
            );
        }
        // One second before the trigger still sleeps (unchanged path).
        assert_eq!(
            decide_conservation_start(CONSERVATION_TRIGGER_SECS_OF_DAY_IST - 1, true, false),
            ConservationStart::SleepThenRun(1)
        );
        // Non-trading day late boot still skips — never a weekend row.
        assert_eq!(
            decide_conservation_start(16 * 3600 + 30 * 60, false, false),
            ConservationStart::SkipNonTradingDay
        );
    }

    #[test]
    fn test_parse_prom_counter() {
        let body = "# HELP tv_ticks_processed_total x\n\
                    # TYPE tv_ticks_processed_total counter\n\
                    tv_ticks_processed_total 123456\n\
                    tv_ticks_persisted_total 1.23e3\n\
                    tv_junk_ticks_filtered_total{label=\"x\"} 7\n";
        assert_eq!(
            parse_prom_counter(body, "tv_ticks_processed_total"),
            Some(123_456)
        );
        // Float exposition format parses.
        assert_eq!(
            parse_prom_counter(body, "tv_ticks_persisted_total"),
            Some(1_230)
        );
        // A labelled series does NOT match the bare name (exact token).
        assert_eq!(
            parse_prom_counter(body, "tv_junk_ticks_filtered_total"),
            None
        );
        // Absent metric → None (never silently zero).
        assert_eq!(parse_prom_counter(body, "tv_absent_total"), None);
        // Negative / non-finite → None.
        assert_eq!(parse_prom_counter("m -1\n", "m"), None);
        assert_eq!(parse_prom_counter("m NaN\n", "m"), None);
    }

    #[test]
    fn test_compute_residuals() {
        let c = OutcomeCounters {
            processed: Some(1_000),
            persisted: Some(900),
            junk: Some(10),
            stale_day: Some(40),
            outside_hours: Some(30),
            dedup: Some(15),
            parse_errors: Some(0),
            storage_errors: Some(5),
            dropped_total: Some(0),
        };
        // 900+10+40+30+15+5 = 1000 → outcome balanced; WAL 1_000, no
        // replay → delivery 0.
        assert_eq!(compute_residuals(1_000, 0, &c), (0, 0));
        // WAL saw 1_050 frames but only 1_000 processed → delivery leak 50.
        assert_eq!(compute_residuals(1_050, 0, &c), (50, 0));
        // Hostile-review H2: boot replay inflates `processed` — the replay
        // count is subtracted before the delivery comparison. 1_000 WAL
        // frames + 1_000 processed of which 100 were replays → live
        // processed 900 → delivery residual +100 (a REAL leak no longer
        // masked by the replay inflation).
        assert_eq!(compute_residuals(1_000, 100, &c), (100, 0));
        // Unadjusted replay inflation → negative residual (classified
        // partial by classify_outcome, never silently balanced).
        assert_eq!(compute_residuals(900, 0, &c).0, -100);
        // Replay bound: replay_recovered > processed never underflows.
        assert_eq!(compute_residuals(0, 5_000, &c), (0, 0));
        // An unaccounted outcome: drop persisted by 25 → outcome leak 25.
        let mut leaky = c;
        leaky.persisted = Some(875);
        assert_eq!(compute_residuals(1_000, 0, &leaky), (0, 25));
    }

    #[test]
    fn test_classify_outcome_partial_leak_balanced() {
        use ConservationOutcome as O;
        assert_eq!(classify_outcome(0, 0, false), O::Balanced);
        assert_eq!(classify_outcome(1, 0, false), O::Leak);
        assert_eq!(classify_outcome(0, 1, false), O::Leak);
        // Hostile-review H2: NEGATIVE residuals are PARTIAL, not balanced —
        // an inflated identity cannot vouch for the day and could mask a
        // real leak of equal size.
        assert_eq!(classify_outcome(-100, 0, false), O::Partial);
        assert_eq!(classify_outcome(0, -1, false), O::Partial);
        // Partial coverage wins over leak — residuals on missing inputs are
        // not trustworthy numbers (no false alarm, no false OK).
        assert_eq!(classify_outcome(50, 50, true), O::Partial);
        assert_eq!(classify_outcome(0, 0, true), O::Partial);
    }

    #[test]
    fn test_ws_wal_dir_default() {
        // Single source of truth for the WAL dir (hostile-review H1) —
        // default matches the STAGE-C boot wiring.
        if std::env::var("TV_WS_WAL_DIR").is_err() {
            assert_eq!(ws_wal_dir(), std::path::PathBuf::from("./data/ws_wal"));
        }
    }

    #[test]
    fn test_parse_questdb_count() {
        assert_eq!(
            parse_questdb_count(r#"{"dataset":[[42]],"count":1}"#),
            Some(42)
        );
        assert_eq!(parse_questdb_count(r#"{"dataset":[]}"#), None);
        assert_eq!(parse_questdb_count("not json"), None);
    }

    #[test]
    fn test_boot_covers_full_session_boundaries() {
        // Pre-09:00 IST boot covers the session; at/after does not.
        assert!(boot_covers_full_session(8 * 3600 + 35 * 60));
        assert!(!boot_covers_full_session(MARKET_OPEN_SECS_OF_DAY_IST));
        assert!(!boot_covers_full_session(11 * 3600));
    }

    #[test]
    fn test_outcome_counters_from_metrics_body() {
        let body = "tv_ticks_processed_total 100\n\
                    tv_ticks_persisted_total 90\n\
                    tv_junk_ticks_filtered_total 1\n\
                    tv_stale_day_filtered_total 2\n\
                    tv_outside_hours_filtered_total 3\n\
                    tv_dedup_filtered_total 4\n\
                    tv_parse_errors_total 0\n\
                    tv_storage_errors_total 0\n\
                    tv_ticks_dropped_total 0\n";
        let c = OutcomeCounters::from_metrics_body(body);
        assert_eq!(c.processed, Some(100));
        assert_eq!(c.persisted, Some(90));
        assert_eq!(c.junk, Some(1));
        assert_eq!(c.stale_day, Some(2));
        assert_eq!(c.outside_hours, Some(3));
        assert_eq!(c.dedup, Some(4));
        assert!(c.identity_complete());
        // Empty body → all None.
        let empty = OutcomeCounters::from_metrics_body("");
        assert!(!empty.identity_complete());
        assert_eq!(empty.processed, None);
    }

    #[test]
    fn test_outcome_counters_identity_complete() {
        let mut c = OutcomeCounters {
            processed: Some(1),
            persisted: Some(1),
            junk: Some(0),
            stale_day: Some(0),
            outside_hours: Some(0),
            dedup: Some(0),
            parse_errors: None, // not identity-bearing
            storage_errors: Some(0),
            dropped_total: None, // not identity-bearing
        };
        assert!(c.identity_complete());
        c.persisted = None;
        assert!(!c.identity_complete());
    }
}
