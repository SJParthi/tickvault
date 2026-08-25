//! WAL-SUSPEND-01 — QuestDB per-table WAL-suspension probe (W2 PR #6,
//! 2026-07-09 audit follow-up row 10).
//!
//! # The gap this closes
//!
//! A QuestDB table can enter **WAL-suspended** state (after a disk-full
//! episode or a WAL apply error): ILP ingestion keeps ACKing rows into the
//! table's WAL, but WAL APPLY is suspended — rows silently stop becoming
//! visible/durable-applied. Nothing else in tickvault sees this: the boot
//! probe + `make doctor` check reachability (`SELECT 1`), and
//! `questdb_health.rs` tracks the ILP writer's CONNECTION state — none of
//! them see per-table apply health. A suspended `ticks` or `candles_1m`
//! table = silent data-visibility loss until someone manually notices.
//!
//! # What this module does
//!
//! Every [`WAL_SUSPENSION_POLL_INTERVAL_SECS`] a supervised task issues
//! `select * from wal_tables()` against the QuestDB HTTP `/exec` endpoint
//! (via the SHARED [`crate::http_client::shared_probe_client`] — the
//! HTTP-CLIENT-01 contract: a client-build failure degrades the single
//! tick, never panics) and:
//!
//! - parses the response DEFENSIVELY BY COLUMN NAME (never position) —
//!   the column set (`name`, `suspended`, `writerTxn`, `bufferedTxnSize`,
//!   `sequencerTxn`, `errorTag`, `errorMessage`, `memoryPressure`) was
//!   verified against upstream QuestDB source
//!   (`WalTableListFunctionFactory.java`); the live 9.3.5 image's exact
//!   shape is honestly UNVERIFIED-LIVE from the dev sandbox, so any drift
//!   fails soft into `tv_wal_suspension_probe_failed_total{reason}` — a
//!   loud monitoring degradation, never a silent miss;
//! - feeds the rows into the pure edge-latched [`WalSuspensionTracker`]:
//!   ONE `error!(code = "WAL-SUSPEND-01")` per (table, suspension episode)
//!   on the rising edge (audit-findings Rule 4 — edge-triggered alerts
//!   only), a falling-edge `info!` on recovery/disappearance;
//! - sets the `tv_questdb_wal_suspended_tables` gauge on every SUCCESSFUL
//!   probe (including 0).
//!
//! # Down ≠ suspended (no double-page)
//!
//! Suspension is asserted ONLY from a SUCCESSFUL 2xx response whose parsed
//! rows show `suspended=true`. An unreachable/erroring QuestDB is
//! BOOT-01/02 + `tv_questdb_connected` territory — here it is a `debug!` +
//! probe-failed counter, and the episode latch is PRESERVED so the page
//! does not re-fire when the server comes back still-suspended.
//!
//! # Paging + triage
//!
//! The `error!` routes errors.jsonl → CloudWatch Logs → the
//! `tv-<env>-errcode-wal-suspend-01` log metric filter + alarm
//! (`deploy/aws/terraform/error-code-alarms.tf`) → SNS → Telegram, ≤~5 min.
//! Recovery action (`ALTER TABLE <t> RESUME WAL`) is an OPERATOR decision —
//! NEVER auto-executed (auto-resume can replay into a still-broken disk).
//! Runbook: `.claude/rules/project/wal-suspension-error-codes.md`.
//!
//! **AMENDED 2026-08-19 — resume IS now auto-executed, CONDITIONALLY.** The
//! paragraph above stands as the reasoning and is not withdrawn: replaying a
//! WAL into a still-broken disk is exactly the failure it warns about, and
//! nothing here does that. What changed is that the operator required the box
//! to run unattended, and "wait for a human" was the only remaining path out
//! of a suspended table — the terminal state in the whole self-management
//! chain.
//!
//! [`crate::wal_auto_resume`] issues the statement only when the disk that
//! caused the suspension has measurably recovered (a quarter of the volume
//! free, well clear of where the disk-pressure loop stops reclaiming), and it
//! refuses on a tight disk, refuses when `df` cannot be read at all, and stops
//! after three attempts per episode — at which point the WAL-SUSPEND-01 page
//! stands and this paragraph's original advice applies again. The caution was
//! right; what it lacked was a condition.
//!
//! # Honest bound — the FAST crash-recovery boot arm (2026-07-10 review)
//!
//! The main.rs spawn site sits in the process-global supervised-monitor
//! block AFTER the fast crash-recovery boot arm's early return — so a
//! market-hours crash-restart session runs WITHOUT this watcher (and
//! without its siblings: disk / OOM / resource monitors — the identical
//! pre-existing sibling-wide gap). Disk-full → crash → fast-restart is
//! exactly a suspension-producing sequence, so this bound is stated
//! rather than hidden; moving the whole monitor block ahead of the fast
//! arm is a flagged sibling-wide follow-up (NOT this PR — it would
//! change the boot semantics of four existing monitors at once).

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::Value;
use tracing::{debug, error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::capture_rest_error_body;

use crate::disk_health_watcher::classify_join_exit;

/// Cadence of the WAL-suspension probe. 60s mirrors the sibling
/// process-global monitors (disk / OOM / resource) — a suspension pages
/// within one poll + the ≤5-min alarm evaluation, while one tiny SELECT
/// per minute burns negligible QuestDB + network budget.
pub const WAL_SUSPENSION_POLL_INTERVAL_SECS: u64 = 60;

/// Backoff between a watcher death and its respawn (mirrors
/// DISK-WATCHER-01 / the resource-monitor supervisor): small so
/// suspension monitoring resumes within seconds, non-zero so an
/// instant-panic loop cannot busy-spin the CPU.
pub const WAL_WATCHER_RESPAWN_BACKOFF_SECS: u64 = 5;

/// The exact query the probe issues. `select * from wal_tables()` (not a
/// pinned projection) so a server-side RENAME of an optional diagnostic
/// column degrades to "diagnostic missing" instead of erroring the whole
/// query; the mandatory `name`/`suspended` columns are then resolved by
/// NAME from the response header.
pub const WAL_TABLES_QUERY_URLENCODED: &str = "select%20*%20from%20wal_tables()";

/// One row parsed from `wal_tables()`. Only `name` + `suspended` are
/// mandatory; the rest are best-effort diagnostics carried into the page.
#[derive(Debug, Clone, PartialEq)]
pub struct WalTableRow {
    /// Table name (`name` column).
    pub name: String,
    /// WAL apply suspended flag (`suspended` column).
    pub suspended: bool,
    /// Last txn applied by the table writer (`writerTxn`), if present.
    pub writer_txn: Option<i64>,
    /// Last txn committed to the sequencer (`sequencerTxn`), if present.
    pub sequencer_txn: Option<i64>,
    /// Server-side error tag for the suspension (`errorTag`), if present.
    pub error_tag: Option<String>,
    /// Server-side error message for the suspension (`errorMessage`), if
    /// present. Sanitized + truncated at the emit site before logging.
    pub error_message: Option<String>,
}

/// Why a probe attempt produced no usable row set. Stable static labels
/// for `tv_wal_suspension_probe_failed_total{reason}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalProbeFailure {
    /// HTTP send failed (server down/unreachable/timeout) — BOOT-01/02 +
    /// `tv_questdb_connected` own the "DB down" page; never WAL-SUSPEND-01.
    Http,
    /// Reachable server returned non-2xx.
    Status,
    /// Body was not the expected `/exec` JSON shape.
    Parse,
    /// The `columns` header lacks `name` and/or `suspended` (schema drift).
    MissingColumn,
    /// The header resolved, rows arrived, and EVERY one was skipped -- the
    /// mandatory cells are present by name but not by TYPE. A QuestDB upgrade
    /// that renders `suspended` as the string `"true"` instead of a boolean
    /// produces exactly this, and until 2026-08-25 it returned `Ok(vec![])`:
    /// no error, no counter, and `emit_wal_delta` then set the gauge to a
    /// confident ZERO. The one detector for the one failure mode where every
    /// tick counter reports success and the rows are not there would have read
    /// green for as long as the drift lasted.
    AllRowsSkipped,
}

impl WalProbeFailure {
    /// Static metric label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Status => "status",
            Self::Parse => "parse",
            Self::MissingColumn => "missing_column",
            Self::AllRowsSkipped => "all_rows_skipped",
        }
    }
}

/// PURE: parse a QuestDB `/exec` JSON body for `wal_tables()` rows,
/// resolving cell indices BY COLUMN NAME from the `columns` header —
/// never by position (the task's defensive-parsing contract: the live
/// server's column ORDER and optional-column set must be free to drift
/// without corrupting the mandatory `name`/`suspended` reads).
///
/// # Errors
///
/// - [`WalProbeFailure::Parse`] — body is not an object with a `columns`
///   array (not the `/exec` shape at all);
/// - [`WalProbeFailure::MissingColumn`] — header lacks `name` or
///   `suspended` (server schema drift; fail-soft, loud counter upstream);
/// - [`WalProbeFailure::AllRowsSkipped`] — rows arrived and EVERY one was
///   skipped (mandatory cells present by NAME but not by TYPE).
///
/// Individual malformed ROWS (non-array, missing cells, wrong cell types
/// for the mandatory columns) are skipped defensively so one bad row
/// cannot blind the probe to the remaining tables — but ALL of them being
/// skipped is drift, not an empty answer, and is reported as such.
pub fn parse_wal_tables_response(body: &Value) -> Result<Vec<WalTableRow>, WalProbeFailure> {
    let columns = body
        .get("columns")
        .and_then(Value::as_array)
        .ok_or(WalProbeFailure::Parse)?;
    let col_index = |wanted: &str| -> Option<usize> {
        columns.iter().position(|c| {
            c.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n == wanted)
        })
    };
    let (Some(name_idx), Some(suspended_idx)) = (col_index("name"), col_index("suspended")) else {
        return Err(WalProbeFailure::MissingColumn);
    };
    let writer_txn_idx = col_index("writerTxn");
    let sequencer_txn_idx = col_index("sequencerTxn");
    let error_tag_idx = col_index("errorTag");
    let error_message_idx = col_index("errorMessage");

    // An absent/empty dataset is a legitimate "no WAL tables" answer.
    let rows = match body.get("dataset").and_then(Value::as_array) {
        Some(rows) => rows,
        None => return Ok(Vec::new()),
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(cells) = row.as_array() else {
            continue; // malformed row — skip defensively
        };
        let (Some(name), Some(suspended)) = (
            cells.get(name_idx).and_then(Value::as_str),
            cells.get(suspended_idx).and_then(Value::as_bool),
        ) else {
            continue; // mandatory cells missing/wrong type — skip
        };
        let opt_i64 = |idx: Option<usize>| idx.and_then(|i| cells.get(i)).and_then(Value::as_i64);
        let opt_str = |idx: Option<usize>| {
            idx.and_then(|i| cells.get(i))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
        };
        out.push(WalTableRow {
            name: name.to_string(),
            suspended,
            writer_txn: opt_i64(writer_txn_idx),
            sequencer_txn: opt_i64(sequencer_txn_idx),
            error_tag: opt_str(error_tag_idx),
            error_message: opt_str(error_message_idx),
        });
    }
    // A dataset that arrived with rows and produced NOTHING is schema drift,
    // not an empty answer. Returning `Ok(vec![])` here let `emit_wal_delta`
    // set `tv_questdb_wal_suspended_tables` to a confident 0 while every row
    // was being silently skipped -- and WAL suspension is the one failure
    // where ILP keeps ACKing, `flush()` returns Ok, every loss counter reads
    // zero, and the rows are simply not there. A blind probe reporting health
    // is strictly worse than no probe at all.
    //
    // An EMPTY dataset stays a legitimate `Ok(vec![])`: no WAL tables is a
    // real answer, and the boot DDL means it is only true before any table
    // exists.
    if out.is_empty() && !rows.is_empty() {
        return Err(WalProbeFailure::AllRowsSkipped);
    }
    Ok(out)
}

/// What one observation of the current `wal_tables()` rows changed,
/// relative to the tracker's latched episode set.
#[derive(Debug, Clone, PartialEq)]
pub struct WalSuspensionDelta {
    /// Tables that just ENTERED suspension (rising edge — emit ONE
    /// WAL-SUSPEND-01 `error!` per entry).
    pub newly_suspended: Vec<WalTableRow>,
    /// Tables that just LEFT suspension — resumed, caught up, or no
    /// longer reported by `wal_tables()` (dropped). Falling edge —
    /// `info!` per entry.
    pub recovered: Vec<String>,
    /// Count of tables currently suspended (the gauge value).
    pub currently_suspended: usize,
}

/// PURE edge-latch state machine: per-table suspension episodes.
///
/// Mirrors the `QuestDbHealthPoller` house shape — no I/O, fed by the
/// owning task, deterministically unit-testable. The latch means a table
/// suspended for hours emits ONE `error!` (Rule 4: edge-triggered alerts
/// only); the CloudWatch alarm's `ok_recovery = false` + the falling-edge
/// `info!` + the gauge are the recovery signals.
#[derive(Debug, Default)]
pub struct WalSuspensionTracker {
    suspended: BTreeSet<String>,
}

impl WalSuspensionTracker {
    /// Fresh tracker with no latched episodes. NOTE (documented honest
    /// bound): after a watcher respawn or process restart, a
    /// still-suspended table re-fires its rising edge once — bounded
    /// re-page, and strictly better than a silent gap.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of currently-latched suspended tables.
    #[must_use]
    pub fn currently_suspended(&self) -> usize {
        self.suspended.len()
    }

    /// Feed one SUCCESSFUL probe's rows; returns the edge delta. Callers
    /// MUST NOT call this for failed probes (the latch must survive an
    /// outage window untouched — down ≠ recovered).
    ///
    /// A duplicate table NAME in one response (unreachable in practice —
    /// `wal_tables()` keys by table) contributes ONE `newly_suspended`
    /// entry (first row wins), never N pages per poll (2026-07-10
    /// hostile-review LOW).
    pub fn observe(&mut self, rows: &[WalTableRow]) -> WalSuspensionDelta {
        let now_suspended: BTreeSet<String> = rows
            .iter()
            .filter(|r| r.suspended)
            .map(|r| r.name.clone())
            .collect();
        let mut emitted: BTreeSet<&str> = BTreeSet::new();
        let newly_suspended: Vec<WalTableRow> = rows
            .iter()
            .filter(|r| {
                r.suspended && !self.suspended.contains(&r.name) && emitted.insert(r.name.as_str())
            })
            .cloned()
            .collect();
        let recovered: Vec<String> = self
            .suspended
            .iter()
            .filter(|name| !now_suspended.contains(*name))
            .cloned()
            .collect();
        self.suspended = now_suspended;
        WalSuspensionDelta {
            newly_suspended,
            recovered,
            currently_suspended: self.suspended.len(),
        }
    }

    /// 2026-07-10 hostile-review LOW guard: a 2xx response whose dataset
    /// is EMPTY while the latch holds suspended tables is treated as
    /// SUSPICIOUS (server mid-start, tables not yet registered) rather
    /// than as a mass recovery — clearing the latch on it would emit a
    /// false-recovery `info!` and a bounded re-page on the next honest
    /// poll. The `ticks`/`candles_*` WAL tables always exist in this
    /// product, so a legitimately-empty `wal_tables()` with a non-empty
    /// latch does not occur; if it ever did, the latch clears on the
    /// first non-empty poll.
    #[must_use]
    pub fn is_suspicious_empty(&self, rows: &[WalTableRow]) -> bool {
        rows.is_empty() && !self.suspended.is_empty()
    }
}

/// Minimum txn lag before a table is even considered for the growing-lag
/// signal.
///
/// A busy table is ALWAYS a little behind — that is what asynchronous WAL
/// apply means — so an absolute threshold alone would either page on healthy
/// load or miss a real stall, depending on which number was guessed. The
/// floor exists only to keep tiny oscillations out of the growth test below;
/// it is NOT the alert condition on its own.
pub const WAL_APPLY_LAG_MIN_TXN: i64 = 1_000;

/// Consecutive polls of NON-DECREASING lag (above the floor) before the
/// growing-lag signal fires. At the 60s poll interval this is five minutes.
pub const WAL_APPLY_LAG_GROWING_POLLS: u32 = 5;

/// Consecutive failed probes before the watcher declares itself BLIND.
///
/// At the 60s poll interval this is five minutes of not knowing.
pub const WAL_PROBE_BLIND_AFTER_FAILURES: u32 = 5;

/// Gauge sentinel meaning "the probe has been failing long enough that the
/// last reading is not trustworthy".
///
/// Negative deliberately: the gauge's honest values are `0..=n`, so no real
/// reading can collide with it, and an alarm on `< 0` cannot be confused
/// with an alarm on "some tables are suspended".
pub const WAL_SUSPENDED_TABLES_GAUGE_BLIND: f64 = -1.0;

/// Tracks per-table WAL-apply lag and reports which tables have been falling
/// further behind for [`WAL_APPLY_LAG_GROWING_POLLS`] consecutive polls.
///
/// # Why lag, when there is already a `suspended` flag
///
/// On 2026-08-25 fourteen QuestDB tables stopped applying rows during a
/// disk-full episode. The recorded evidence is a txn table: `market_depth`
/// sat **29,908** transactions behind, `candles_1s` 5,073, `ticks` 4,511 —
/// while `ws_event_audit`, the one healthy table, was exactly 0 behind. The
/// operator discovered it by asking why an order was missing, not from a
/// page.
///
/// The `suspended` flag is set by QuestDB only once apply gives up. A table
/// that is retrying — or simply never able to drain because the volume is
/// full — reads `suspended = false` and falls further behind every minute.
/// That state is operationally identical to suspension (ILP keeps ACKing;
/// rows stop becoming visible) and had no detector at all.
///
/// # Why "growing", not "large"
///
/// This repository has never measured what a normal session's peak lag looks
/// like, and picking an absolute number without that measurement is the
/// exact failure this detector exists to correct. Monotonic growth needs no
/// baseline: a healthy busy table's lag oscillates as apply catches up, a
/// stuck one's only rises. The floor suppresses noise; the growth is the
/// signal.
///
/// Pure and allocation-free per observation apart from the map itself, which
/// is bounded by the table count (~30 in this product). Edge-latched: a
/// table that stays stuck emits once, not once per minute.
#[derive(Debug, Default)]
pub struct WalLagTracker {
    /// Per table: the last lag seen, how many consecutive polls it has been
    /// non-decreasing above the floor, and whether it has already been
    /// reported for this episode.
    state: std::collections::BTreeMap<String, LagState>,
}

/// Per-table lag bookkeeping. Private — the tracker's API is the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LagState {
    last_lag: i64,
    growing_polls: u32,
    reported: bool,
}

impl WalLagTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one poll's rows. Returns the tables that crossed into the
    /// growing-lag condition ON THIS POLL, each with its current lag.
    ///
    /// A table already reported stays silent until its lag actually falls,
    /// which clears the latch (Rule 4: edge-triggered alerts).
    pub fn observe(&mut self, rows: &[WalTableRow]) -> Vec<(String, i64)> {
        let mut fired = Vec::new();
        for row in rows {
            let (Some(seq), Some(writer)) = (row.sequencer_txn, row.writer_txn) else {
                // The diagnostic columns are optional by design (the query is
                // `select *` precisely so a server rename degrades rather than
                // errors). No lag reading means no lag verdict — never a
                // fabricated zero.
                continue;
            };
            let lag = seq.saturating_sub(writer);
            let entry = self.state.entry(row.name.clone()).or_insert(LagState {
                last_lag: lag,
                growing_polls: 0,
                reported: false,
            });

            if lag < WAL_APPLY_LAG_MIN_TXN {
                // Below the floor is the healthy state: reset everything,
                // including the report latch, so a genuine second episode
                // pages again.
                entry.growing_polls = 0;
                entry.reported = false;
                entry.last_lag = lag;
                continue;
            }

            if lag < entry.last_lag {
                // Apply is catching up. Not the condition.
                entry.growing_polls = 0;
                entry.reported = false;
            } else {
                entry.growing_polls = entry.growing_polls.saturating_add(1);
            }
            entry.last_lag = lag;

            if entry.growing_polls >= WAL_APPLY_LAG_GROWING_POLLS && !entry.reported {
                entry.reported = true;
                fired.push((row.name.clone(), lag));
            }
        }
        fired
    }

    /// Tables currently latched as growing — exposed for tests and for the
    /// startup log line.
    #[must_use]
    pub fn reported_count(&self) -> usize {
        self.state.values().filter(|s| s.reported).count()
    }
}

/// One probe attempt: GET `/exec?query=select * from wal_tables()` via the
/// shared probe client, parse defensively, return rows or a typed failure.
///
/// Client-build failure (HTTP-CLIENT-01) is handled INSIDE: it logs the
/// typed `error!` + increments the site counter and maps to
/// [`WalProbeFailure::Http`] so the caller's skip-tick semantics are
/// uniform. Never panics.
// TEST-EXEMPT: thin I/O shell — the pure parse core (`parse_wal_tables_response`) and the edge machine (`WalSuspensionTracker`) are fully unit-tested; this fn needs a live QuestDB to exercise and is covered by the first live boot (honest live-unverified note in the plan).
async fn probe_wal_tables(base_url: &str) -> Result<Vec<WalTableRow>, WalProbeFailure> {
    let client = match crate::http_client::shared_probe_client() {
        Ok(client) => client,
        Err(err) => {
            error!(
                error = %err,
                code = ErrorCode::HttpClient01BuildFailed.code_str(),
                "HTTP-CLIENT-01 reqwest client build failed — WAL-suspension probe skipped this tick"
            );
            metrics::counter!("tv_http_client_build_failed_total", "site" => "wal_suspension_probe")
                .increment(1);
            return Err(WalProbeFailure::Http);
        }
    };
    let resp = match client.get(base_url).send().await {
        Ok(resp) => resp,
        Err(err) => {
            // Server down/unreachable — BOOT-01/02 + tv_questdb_connected
            // own that page; here it is only a probe-failed count.
            debug!(?err, "WAL-suspension probe network error — skipping tick");
            return Err(WalProbeFailure::Http);
        }
    };
    if !resp.status().is_success() {
        debug!(status = %resp.status(), "WAL-suspension probe non-2xx — skipping tick");
        return Err(WalProbeFailure::Status);
    }
    // Honest boundary (2026-07-10 security-review LOW, accepted): no
    // explicit body-size cap — QuestDB is operator-controlled internal
    // infra on the Docker network (not a vendor endpoint like the CSV
    // downloader's 50 MB-capped fetch), wal_tables() returns ~30 tiny
    // rows, and the shared client's 2s timeout bounds the wall clock.
    let body: Value = match resp.json().await {
        Ok(body) => body,
        Err(err) => {
            debug!(?err, "WAL-suspension probe body was not JSON");
            return Err(WalProbeFailure::Parse);
        }
    };
    parse_wal_tables_response(&body)
}

/// Emit the growing-lag pages. One `error!` per table per episode.
///
/// Reuses `WAL-SUSPEND-01` deliberately rather than minting a new code: the
/// operator-facing condition is identical ("rows are being ACKed and are not
/// becoming visible"), the remediation starts at the same place (`df -h`, then
/// the QuestDB logs), and the existing metric filter already pages on it — so
/// this costs no new CloudWatch metric and no user-data byte, both of which
/// are at their ceiling. The `source` field is what separates the two on
/// triage.
// TEST-EXEMPT: pure log/metric side effects over the fully-tested WalLagTracker output; no branch here is reachable without a verdict the tracker tests already pin.
fn emit_wal_lag(fired: &[(String, i64)]) {
    for (name, lag) in fired {
        let table = capture_rest_error_body(name);
        error!(
            code = ErrorCode::WalSuspend01TableSuspended.code_str(),
            source = "apply_lag_growing",
            table = %table,
            txn_lag = *lag,
            growing_polls = WAL_APPLY_LAG_GROWING_POLLS,
            "WAL-SUSPEND-01: QuestDB table WAL apply has fallen {lag} transactions \
             behind and has NOT caught up for {WAL_APPLY_LAG_GROWING_POLLS} consecutive \
             polls. The table is NOT flagged `suspended` — ingestion keeps ACKing rows \
             while they stop becoming visible, which is the same operator-visible \
             failure with none of the same warning. Typical cause is a full or saturated \
             volume: check `df -h /data` FIRST, then the QuestDB logs. Do NOT resume or \
             restart into a still-full disk. Runbook: docs/error-runbooks/wal-suspension-error-codes.md"
        );
    }
}

/// Emit the metrics + logs implied by one observation delta. Separated
/// from the tracker so the state machine stays pure.
// TEST-EXEMPT: pure metric/log side effects over the fully-tested WalSuspensionDelta; no branch here is reachable without a delta the tracker tests already pin.
fn emit_wal_delta(delta: &WalSuspensionDelta) {
    metrics::gauge!("tv_questdb_wal_suspended_tables").set(delta.currently_suspended as f64);
    for row in &delta.newly_suspended {
        let lag = match (row.sequencer_txn, row.writer_txn) {
            (Some(seq), Some(writer)) => seq.saturating_sub(writer),
            _ => -1, // unknown
        };
        // Server-controlled text: sanitize + truncate (≤300 chars,
        // control-chars stripped, credential/JWT-redacted) before it
        // reaches a log line. `capture_rest_error_body` is reused here
        // beyond its Dhan-REST origin — its control-char strip + bound is
        // exactly the log-injection defense these fields need. The table
        // NAME is sanitized too (2026-07-10 security-review MEDIUM): a
        // crafted table name with newlines/ANSI escapes must not forge
        // lines in the text-formatted sinks.
        let table = capture_rest_error_body(&row.name);
        let error_tag = capture_rest_error_body(row.error_tag.as_deref().unwrap_or(""));
        let error_message = capture_rest_error_body(row.error_message.as_deref().unwrap_or(""));
        error!(
            code = ErrorCode::WalSuspend01TableSuspended.code_str(),
            table = %table,
            error_tag = %error_tag,
            error_message = %error_message,
            writer_txn = row.writer_txn.unwrap_or(-1),
            sequencer_txn = row.sequencer_txn.unwrap_or(-1),
            txn_lag = lag,
            "WAL-SUSPEND-01: QuestDB table WAL apply is SUSPENDED — ILP keeps \
             ACKing rows but they silently stop becoming visible/applied. \
             Operator action required: diagnose the cause (disk-full episode? \
             apply error?), then `ALTER TABLE <table> RESUME WAL` — NEVER \
             auto-executed (resuming into a still-broken disk replays the \
             failure). Runbook: .claude/rules/project/wal-suspension-error-codes.md"
        );
    }
    for name in &delta.recovered {
        info!(
            table = %capture_rest_error_body(name),
            "WAL-SUSPEND-01 recovery: table WAL apply is no longer suspended \
             (resumed, caught up, or no longer reported by wal_tables())"
        );
    }
}

/// Spawn the 60s WAL-suspension probe loop. Callers use the SUPERVISED
/// wrapper [`spawn_supervised_wal_suspension_watcher`]; this inner fn is
/// separate so the supervisor can respawn it (house DISK-WATCHER-01 /
/// resource-monitor shape).
// TEST-EXEMPT: tokio task spawn — the pure parse + tracker cores are fully unit-tested; the supervisor keep-running guard below exercises the spawn chain.
pub fn spawn_wal_suspension_watcher(questdb: QuestDbConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let base_url = format!(
            "http://{}:{}/exec?query={}",
            questdb.host, questdb.http_port, WAL_TABLES_QUERY_URLENCODED
        );
        info!(
            interval_secs = WAL_SUSPENSION_POLL_INTERVAL_SECS,
            "WAL-suspension watcher started (per-table wal_tables() probe)"
        );
        let mut tracker = WalSuspensionTracker::new();
        let mut lag_tracker = WalLagTracker::new();
        let mut resume_ledger = crate::wal_auto_resume::ResumeLedger::new();
        // Edge-latch for parse/schema failures so a server-version drift
        // is loud ONCE per contiguous failure run, not every 60s forever.
        let mut schema_warned = false;
        // Consecutive failed probes, and whether the BLIND state has already
        // been announced for this run of failures.
        let mut consecutive_probe_failures: u32 = 0;
        let mut blind_announced = false;
        let mut ticker =
            tokio::time::interval(Duration::from_secs(WAL_SUSPENSION_POLL_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            match probe_wal_tables(&base_url).await {
                Ok(rows) => {
                    schema_warned = false;
                    if blind_announced {
                        info!(
                            failures = consecutive_probe_failures,
                            "WAL-suspension probe recovered — suspension monitoring is \
                             sighted again and the gauge is a live reading once more"
                        );
                    }
                    consecutive_probe_failures = 0;
                    blind_announced = false;
                    if tracker.is_suspicious_empty(&rows) {
                        // 2xx with an EMPTY dataset while tables are
                        // latched suspended = server mid-start, not a
                        // mass recovery — skip so the latch survives
                        // (2026-07-10 hostile-review LOW: clearing here
                        // would fake a recovery info! + re-page later).
                        debug!(
                            latched = tracker.currently_suspended(),
                            "wal_tables() returned zero rows while tables \
                             are latched suspended — treating as a \
                             suspicious transient, latch preserved"
                        );
                        continue;
                    }
                    let delta = tracker.observe(&rows);
                    emit_wal_delta(&delta);
                    // The 2026-08-25 gap: a table can stop applying rows
                    // WITHOUT the `suspended` flag ever being set, and that
                    // is the state that actually happened. See
                    // `WalLagTracker` for why the signal is growth rather
                    // than magnitude.
                    emit_wal_lag(&lag_tracker.observe(&rows));
                    // Attempt recovery, CONDITIONALLY. The module header
                    // above says a resume is an operator decision because it
                    // "can replay into a still-broken disk" — which is right,
                    // and is exactly what `attempt_auto_resume` checks before
                    // issuing anything. It refuses on a tight disk, refuses on
                    // an unmeasurable one, and hands back to the operator
                    // after a bounded number of tries.
                    for table in &delta.recovered {
                        resume_ledger.clear(table);
                    }
                    attempt_auto_resume(&questdb, &rows, &mut resume_ledger).await;
                }
                Err(failure) => {
                    // A failed probe leaves the gauge holding its last value.
                    // That is right for one tick and WRONG for an outage: a
                    // stale `0` reads as "no tables suspended" while the
                    // watcher can no longer see anything, which is the
                    // false-OK class this whole module exists to prevent. Past
                    // the threshold the gauge says UNKNOWN instead of lying.
                    //
                    // This is not hypothetical. During the 2026-08-25
                    // disk-full episode QuestDB was under heavy distress; any
                    // probe that failed then left a stale reading behind, and
                    // the only trace was a `debug!` line production logging
                    // does not carry.
                    metrics::counter!(
                        "tv_wal_suspension_probe_failed_total",
                        "reason" => failure.as_str()
                    )
                    .increment(1);
                    consecutive_probe_failures = consecutive_probe_failures.saturating_add(1);
                    if consecutive_probe_failures >= WAL_PROBE_BLIND_AFTER_FAILURES
                        && !blind_announced
                    {
                        blind_announced = true;
                        metrics::gauge!("tv_questdb_wal_suspended_tables")
                            .set(WAL_SUSPENDED_TABLES_GAUGE_BLIND);
                        error!(
                            code = ErrorCode::WalSuspend01TableSuspended.code_str(),
                            source = "probe_blind",
                            reason = failure.as_str(),
                            consecutive_failures = consecutive_probe_failures,
                            "WAL-SUSPEND-01: the WAL-suspension probe has failed \
                             {consecutive_probe_failures} times in a row — suspension \
                             monitoring is BLIND and the suspended-tables gauge is now \
                             reporting UNKNOWN rather than a stale reading. Tables may be \
                             suspended right now with nothing able to say so. Check that \
                             QuestDB is answering /exec (df -h /data first — a full volume \
                             is the cause that produces both this and real suspensions)."
                        );
                    }
                    if matches!(
                        failure,
                        WalProbeFailure::Parse | WalProbeFailure::MissingColumn
                    ) && !schema_warned
                    {
                        schema_warned = true;
                        warn!(
                            reason = failure.as_str(),
                            "WAL-suspension probe cannot parse wal_tables() — \
                             QuestDB schema drift? Suspension monitoring is \
                             DEGRADED until the parser matches the server \
                             (gauge holds its last value; probe-failed counter \
                             is rising). Down-server errors stay at debug — \
                             BOOT-01/02 own that page."
                        );
                    }
                    // Latch + gauge deliberately untouched: a failed probe
                    // proves nothing about suspension state either way.
                }
            }
        }
    })
}

/// Directory the disk-evidence probe measures.
///
/// The same relative path the disk-pressure loop watches, so the two agree
/// about which volume they are talking about. Measuring a different
/// filesystem than the one that suspended the table would make the resume
/// decision confidently wrong.
const RESUME_DISK_PROBE_PATH: &str = "data";

/// Issues `ALTER TABLE … RESUME WAL` for suspended tables whose cause has
/// demonstrably cleared.
///
/// Every refusal is logged with its reason, because "nothing happened" and
/// "we decided not to" are different states and only one of them means the
/// operator should look.
// TEST-EXEMPT: async I/O composition (a `df` probe plus one /exec per resume); every decision it makes — the disk classification, the attempt budget, the give-up point and the SQL — is a pure function tested in `wal_auto_resume`.
async fn attempt_auto_resume(
    questdb: &QuestDbConfig,
    rows: &[WalTableRow],
    ledger: &mut crate::wal_auto_resume::ResumeLedger,
) {
    use crate::wal_auto_resume::{DiskEvidence, ResumeDecision, build_resume_sql};

    let suspended: Vec<&WalTableRow> = rows.iter().filter(|r| r.suspended).collect();
    if suspended.is_empty() {
        return;
    }

    // Measured ONCE per poll, not per table: `df` is a subprocess, and the
    // answer cannot differ between two tables on the same volume.
    let disk = match crate::disk_health_watcher::probe_disk_free_bytes(std::path::Path::new(
        RESUME_DISK_PROBE_PATH,
    )) {
        crate::disk_health_watcher::DiskHealthOutcome::Ok {
            free_bytes,
            total_bytes,
        } => DiskEvidence::from_measurement(free_bytes, total_bytes),
        crate::disk_health_watcher::DiskHealthOutcome::ProbeFailed { .. } => DiskEvidence::Unknown,
    };

    for row in suspended {
        match ledger.decide(&row.name, disk) {
            ResumeDecision::Resume { attempt } => {
                let Some(sql) = build_resume_sql(&row.name) else {
                    error!(
                        code = ErrorCode::WalSuspend01TableSuspended.code_str(),
                        table = %row.name,
                        "auto-resume REFUSED: the table name is not a plain identifier, so no \
                         statement was built. Resume it manually and check where that name \
                         came from — wal_tables() should never emit one."
                    );
                    continue;
                };
                match exec_resume(questdb, &sql).await {
                    Ok(()) => {
                        metrics::counter!("tv_wal_auto_resume_total", "outcome" => "issued")
                            .increment(1);
                        info!(
                            table = %row.name,
                            attempt,
                            "WAL auto-resume issued — the disk has recovered, so the replay \
                             has somewhere to go. If the table re-suspends this will retry, \
                             then stop and page."
                        );
                    }
                    Err(err) => {
                        metrics::counter!("tv_wal_auto_resume_total", "outcome" => "failed")
                            .increment(1);
                        warn!(
                            table = %row.name,
                            attempt,
                            %err,
                            "WAL auto-resume statement failed — the table stays suspended and \
                             the attempt is spent."
                        );
                    }
                }
            }
            ResumeDecision::WaitForDisk => {
                metrics::counter!("tv_wal_auto_resume_total", "outcome" => "waiting_disk")
                    .increment(1);
                debug!(
                    table = %row.name,
                    "WAL auto-resume held: the disk is still too tight to replay into"
                );
            }
            ResumeDecision::WaitForEvidence => {
                metrics::counter!("tv_wal_auto_resume_total", "outcome" => "waiting_evidence")
                    .increment(1);
                debug!(
                    table = %row.name,
                    "WAL auto-resume held: free space could not be measured, and an \
                     unmeasurable volume is treated exactly like a full one"
                );
            }
            ResumeDecision::GiveUp => {
                metrics::counter!("tv_wal_auto_resume_total", "outcome" => "gave_up").increment(1);
                debug!(
                    table = %row.name,
                    "WAL auto-resume exhausted its attempts — the WAL-SUSPEND-01 page stands \
                     and the operator owns this table now"
                );
            }
        }
    }
}

/// Executes one resume statement over `/exec`.
async fn exec_resume(questdb: &QuestDbConfig, sql: &str) -> Result<(), String> {
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let client = crate::http_client::shared_probe_client()
        .map_err(|e| format!("probe client unavailable: {e}"))?;
    let resp = client
        .get(&url)
        .query(&[("query", sql)])
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("non-2xx: {}", resp.status()))
    }
}

/// Supervise the WAL-suspension watcher: on task death (panic/cancel)
/// log + increment `tv_wal_suspension_watcher_respawn_total{reason}` and
/// respawn after [`WAL_WATCHER_RESPAWN_BACKOFF_SECS`] — mirrors
/// `spawn_supervised_resource_monitor` / DISK-WATCHER-01 so per-table WAL
/// monitoring can never vanish silently (the respawned watcher re-sets
/// the gauge on its first successful probe, covering gauge staleness
/// across a death; a still-suspended table re-fires one bounded page).
// O(1) EXEMPT: cold-path supervisor — one task per session, fires only on watcher death.
// TEST-EXEMPT: covered by `test_spawn_supervised_wal_suspension_watcher_keeps_running` (different name pattern; exercises the spawn chain + never-resolves contract).
pub fn spawn_supervised_wal_suspension_watcher(
    questdb: QuestDbConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle = spawn_wal_suspension_watcher(questdb.clone());
            let join_result = handle.await;
            let reason = classify_join_exit(&join_result);
            warn!(
                reason,
                backoff_secs = WAL_WATCHER_RESPAWN_BACKOFF_SECS,
                "WAL-suspension watcher exited — respawning so per-table \
                 QuestDB WAL-apply monitoring continues"
            );
            metrics::counter!("tv_wal_suspension_watcher_respawn_total", "reason" => reason)
                .increment(1);
            tokio::time::sleep(Duration::from_secs(WAL_WATCHER_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn header(names: &[&str]) -> Value {
        Value::Array(
            names
                .iter()
                .map(|n| json!({ "name": n, "type": "X" }))
                .collect(),
        )
    }

    #[test]
    fn test_parse_wal_tables_by_column_name_any_order() {
        // Canonical order (upstream WalTableListFunctionFactory).
        let body = json!({
            "query": "select * from wal_tables()",
            "columns": header(&[
                "name", "suspended", "writerTxn", "bufferedTxnSize",
                "sequencerTxn", "errorTag", "errorMessage", "memoryPressure"
            ]),
            "dataset": [
                ["ticks", false, 10, 0, 10, null, null, 0],
                ["candles_1m", true, 5, 0, 42, "DISK FULL", "could not open read-write", 0],
            ],
            "count": 2
        });
        let rows = parse_wal_tables_response(&body).expect("canonical shape parses");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "ticks");
        assert!(!rows[0].suspended);
        assert_eq!(rows[1].name, "candles_1m");
        assert!(rows[1].suspended);
        assert_eq!(rows[1].writer_txn, Some(5));
        assert_eq!(rows[1].sequencer_txn, Some(42));
        assert_eq!(rows[1].error_tag.as_deref(), Some("DISK FULL"));
        assert_eq!(
            rows[1].error_message.as_deref(),
            Some("could not open read-write")
        );

        // SHUFFLED column order — by-name resolution must not care.
        let shuffled = json!({
            "columns": header(&["suspended", "errorMessage", "name", "sequencerTxn"]),
            "dataset": [[true, "boom", "ticks", 7]],
        });
        let rows = parse_wal_tables_response(&shuffled).expect("shuffled shape parses");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "ticks");
        assert!(rows[0].suspended);
        assert_eq!(rows[0].sequencer_txn, Some(7));
        assert_eq!(rows[0].writer_txn, None, "absent optional column is None");
        assert_eq!(rows[0].error_message.as_deref(), Some("boom"));
    }

    #[test]
    fn test_parse_missing_suspended_column_fails_soft() {
        let body = json!({
            "columns": header(&["name", "writerTxn"]),
            "dataset": [["ticks", 1]],
        });
        assert_eq!(
            parse_wal_tables_response(&body),
            Err(WalProbeFailure::MissingColumn)
        );
        // Missing `name` likewise.
        let body = json!({
            "columns": header(&["suspended"]),
            "dataset": [[true]],
        });
        assert_eq!(
            parse_wal_tables_response(&body),
            Err(WalProbeFailure::MissingColumn)
        );
    }

    #[test]
    fn test_parse_missing_columns_header_fails_soft() {
        for body in [
            json!({}),
            json!([]),
            json!({"columns": "nope"}),
            json!(null),
        ] {
            assert_eq!(
                parse_wal_tables_response(&body),
                Err(WalProbeFailure::Parse),
                "non-/exec shape must be Parse failure: {body}"
            );
        }
        // Absent dataset with a valid header = legitimately zero rows.
        let no_dataset = json!({ "columns": header(&["name", "suspended"]) });
        assert_eq!(parse_wal_tables_response(&no_dataset), Ok(Vec::new()));
    }

    #[test]
    fn test_parse_skips_malformed_rows() {
        let body = json!({
            "columns": header(&["name", "suspended"]),
            "dataset": [
                "not-an-array",
                ["missing_suspended_cell"],
                [42, true],            // name has wrong type
                ["ticks", "yes"],      // suspended has wrong type
                ["good_table", true],  // valid — must survive
            ],
        });
        let rows = parse_wal_tables_response(&body).expect("valid rows must survive bad ones");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "good_table");
        assert!(rows[0].suspended);
    }

    #[test]
    fn a_dataset_whose_every_row_is_skipped_is_drift_not_an_empty_answer() {
        // THE 2026-08-25 fail-open. A QuestDB upgrade that renders `suspended`
        // as the STRING "true" leaves the header intact -- `MissingColumn`
        // never fires -- while `Value::as_bool` returns None for every row. The
        // old code returned `Ok(vec![])`, `emit_wal_delta` then set
        // `tv_questdb_wal_suspended_tables` to a confident 0, and the ONE
        // detector for the one failure where ILP keeps ACKing rows that are
        // never applied read green for as long as the drift lasted.
        let string_booleans = json!({
            "columns": header(&["name", "suspended"]),
            "dataset": [
                ["ticks", "false"],
                ["market_depth", "true"],
                ["candles_1m", "true"],
            ],
        });
        assert_eq!(
            parse_wal_tables_response(&string_booleans),
            Err(WalProbeFailure::AllRowsSkipped),
            "schema drift must fail LOUD, never report zero suspended tables"
        );
    }

    #[test]
    fn an_empty_dataset_is_still_a_legitimate_zero() {
        // Non-vacuity in the other direction: "no WAL tables" is a real
        // answer and must not be turned into a probe failure, or the counter
        // would fire on every pre-DDL boot.
        let empty = json!({
            "columns": header(&["name", "suspended"]),
            "dataset": [],
        });
        assert_eq!(parse_wal_tables_response(&empty), Ok(Vec::new()));
    }

    #[test]
    fn one_bad_row_among_good_ones_is_still_skipped_not_failed() {
        // The defensive per-row skip is UNCHANGED. Only ALL rows failing is
        // drift; a single malformed row must never blind the probe to the
        // tables that did parse -- which is what this fix could easily have
        // broken by over-reaching.
        let mixed = json!({
            "columns": header(&["name", "suspended"]),
            "dataset": [
                ["ticks", "not-a-bool"],
                ["market_depth", true],
            ],
        });
        let rows = parse_wal_tables_response(&mixed).expect("one good row survives");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "market_depth");
        assert!(rows[0].suspended);
    }

    #[test]
    fn the_all_rows_skipped_label_is_distinct_and_stable() {
        // The label is the metric dimension the operator greps for; two
        // failures sharing one label would be indistinguishable in CloudWatch.
        let labels: Vec<&str> = [
            WalProbeFailure::Http,
            WalProbeFailure::Status,
            WalProbeFailure::Parse,
            WalProbeFailure::MissingColumn,
            WalProbeFailure::AllRowsSkipped,
        ]
        .iter()
        .map(|f| f.as_str())
        .collect();
        assert_eq!(WalProbeFailure::AllRowsSkipped.as_str(), "all_rows_skipped");
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            labels.len(),
            "every failure label is distinct"
        );
    }

    #[test]
    fn test_parse_empty_string_diagnostics_become_none() {
        let body = json!({
            "columns": header(&["name", "suspended", "errorTag", "errorMessage"]),
            "dataset": [["ticks", true, "", ""]],
        });
        let rows = parse_wal_tables_response(&body).expect("parses");
        assert_eq!(rows[0].error_tag, None);
        assert_eq!(rows[0].error_message, None);
    }

    fn row(name: &str, suspended: bool) -> WalTableRow {
        WalTableRow {
            name: name.to_string(),
            suspended,
            writer_txn: None,
            sequencer_txn: None,
            error_tag: None,
            error_message: None,
        }
    }

    /// A row with real txn numbers, for the lag tracker.
    fn lag_row(name: &str, seq: i64, writer: i64) -> WalTableRow {
        WalTableRow {
            name: name.to_string(),
            suspended: false,
            writer_txn: Some(writer),
            sequencer_txn: Some(seq),
            error_tag: None,
            error_message: None,
        }
    }

    /// The 2026-08-25 shape, replayed: `market_depth` fell 29,908 txns
    /// behind while `suspended` stayed FALSE, and nothing paged.
    ///
    /// This asserts the detector fires on that shape and — the half that
    /// matters more — that it does NOT fire on a busy table whose apply is
    /// keeping up.
    #[test]
    fn growing_lag_fires_on_a_stuck_table_and_stays_quiet_on_a_busy_one() {
        let mut t = WalLagTracker::new();

        // Five polls of a table falling further behind every time, exactly
        // as the recorded evidence describes. Nothing fires until the
        // growth has persisted for the full window — a single bad poll is
        // not a stall.
        let mut fired_at = None;
        for (poll, lag) in [
            (1_u32, 2_000_i64),
            (2, 6_000),
            (3, 12_000),
            (4, 20_000),
            (5, 29_908),
        ] {
            let fired = t.observe(&[lag_row("market_depth", 100_000 + lag, 100_000)]);
            if !fired.is_empty() {
                assert_eq!(fired.len(), 1);
                assert_eq!(fired[0].0, "market_depth");
                assert_eq!(fired[0].1, lag);
                fired_at = Some(poll);
                break;
            }
        }
        assert_eq!(
            fired_at,
            Some(WAL_APPLY_LAG_GROWING_POLLS),
            "the growing-lag signal must fire on exactly the {WAL_APPLY_LAG_GROWING_POLLS}th \
             consecutive non-decreasing poll — earlier is noise, later is a missed stall"
        );

        // Edge-latched: still stuck, still growing, but already reported.
        assert!(
            t.observe(&[lag_row("market_depth", 200_000, 100_000)])
                .is_empty(),
            "a table already reported for this episode must not page every minute"
        );

        // A BUSY but healthy table: lag well above the floor, oscillating as
        // apply catches up. This is the false-positive case, and it is the
        // one that decides whether an operator keeps trusting the alarm.
        let mut b = WalLagTracker::new();
        for lag in [
            5_000_i64, 7_000, 4_000, 9_000, 3_000, 8_000, 2_000, 6_000, 1_500, 5_500,
        ] {
            assert!(
                b.observe(&[lag_row("ticks", 500_000 + lag, 500_000)])
                    .is_empty(),
                "a table whose apply catches up must never page (lag {lag})"
            );
        }

        // `ws_event_audit` on 2026-08-25: exactly zero behind, the one
        // healthy table. Below the floor, so it is not even a candidate.
        let mut h = WalLagTracker::new();
        for _ in 0..(WAL_APPLY_LAG_GROWING_POLLS * 3) {
            assert!(
                h.observe(&[lag_row("ws_event_audit", 3_372, 3_372)])
                    .is_empty(),
                "a table that is fully caught up must never page"
            );
        }

        // Missing diagnostic columns produce NO verdict rather than a
        // fabricated zero — the query is `select *` precisely so a server
        // rename degrades instead of erroring.
        let mut m = WalLagTracker::new();
        for _ in 0..(WAL_APPLY_LAG_GROWING_POLLS * 2) {
            assert!(
                m.observe(&[row("candles_1m", false)]).is_empty(),
                "a row with no txn columns cannot yield a lag verdict"
            );
        }
        assert_eq!(m.reported_count(), 0);
    }

    /// Recovery clears the latch, so a SECOND genuine episode pages again.
    /// Without this the first stall of the day would be the only one ever
    /// reported.
    #[test]
    fn a_recovered_table_can_page_again_on_a_second_episode() {
        let mut t = WalLagTracker::new();
        let grow = |t: &mut WalLagTracker, base: i64| {
            let mut fired = Vec::new();
            for i in 1..=WAL_APPLY_LAG_GROWING_POLLS {
                fired = t.observe(&[lag_row(
                    "candles_1s",
                    base + i64::from(i) * 2_000,
                    base - WAL_APPLY_LAG_MIN_TXN,
                )]);
            }
            fired
        };
        assert_eq!(grow(&mut t, 100_000).len(), 1, "first episode must page");

        // Apply catches up completely — below the floor clears everything.
        assert!(
            t.observe(&[lag_row("candles_1s", 500_000, 500_000)])
                .is_empty()
        );
        assert_eq!(
            t.reported_count(),
            0,
            "recovery must clear the report latch"
        );

        assert_eq!(
            grow(&mut t, 600_000).len(),
            1,
            "second episode must page too"
        );
    }

    /// The blind sentinel is a value no honest reading can produce, which is
    /// what lets an alarm distinguish "nothing is suspended" from "I cannot
    /// see". A positive or zero sentinel would be indistinguishable from
    /// health — the exact defect being fixed.
    #[test]
    fn the_blind_gauge_sentinel_can_never_collide_with_a_real_reading() {
        assert!(
            WAL_SUSPENDED_TABLES_GAUGE_BLIND < 0.0,
            "a real suspended-table count is always >= 0, so the blind sentinel must be \
             negative or an alarm cannot tell UNKNOWN from HEALTHY"
        );
        assert!(
            WAL_PROBE_BLIND_AFTER_FAILURES >= 2,
            "declaring blindness on a single failed probe would page on ordinary transients"
        );
    }

    #[test]
    fn test_tracker_rising_edge_fires_once_per_episode() {
        let mut t = WalSuspensionTracker::new();
        // Poll 1: ticks suspends → rising edge.
        let d1 = t.observe(&[row("ticks", true), row("candles_1m", false)]);
        assert_eq!(d1.newly_suspended.len(), 1);
        assert_eq!(d1.newly_suspended[0].name, "ticks");
        assert!(d1.recovered.is_empty());
        assert_eq!(d1.currently_suspended, 1);
        // Polls 2..10: still suspended → NO re-fire (Rule 4 edge latch).
        for _ in 0..9 {
            let d = t.observe(&[row("ticks", true), row("candles_1m", false)]);
            assert!(
                d.newly_suspended.is_empty(),
                "persistent suspension must not re-fire the rising edge"
            );
            assert!(d.recovered.is_empty());
            assert_eq!(d.currently_suspended, 1);
        }
    }

    #[test]
    fn test_tracker_falling_edge_on_recovery_and_disappearance() {
        let mut t = WalSuspensionTracker::new();
        t.observe(&[row("ticks", true), row("candles_1m", true)]);
        assert_eq!(t.currently_suspended(), 2);
        // ticks resumes (suspended=false); candles_1m DISAPPEARS entirely
        // (dropped table) — BOTH are falling edges.
        let d = t.observe(&[row("ticks", false)]);
        assert!(d.newly_suspended.is_empty());
        let mut recovered = d.recovered.clone();
        recovered.sort();
        assert_eq!(recovered, vec!["candles_1m", "ticks"]);
        assert_eq!(d.currently_suspended, 0);
    }

    #[test]
    fn test_tracker_flapping_refires_per_new_episode() {
        let mut t = WalSuspensionTracker::new();
        assert_eq!(t.observe(&[row("ticks", true)]).newly_suspended.len(), 1);
        assert_eq!(t.observe(&[row("ticks", false)]).recovered.len(), 1);
        // A NEW suspension episode is a genuine new incident → re-fire.
        let d = t.observe(&[row("ticks", true)]);
        assert_eq!(
            d.newly_suspended.len(),
            1,
            "a new episode after recovery must re-fire"
        );
    }

    #[test]
    fn test_tracker_multi_table_single_poll() {
        let mut t = WalSuspensionTracker::new();
        let d = t.observe(&[row("a", true), row("b", true), row("c", false)]);
        assert_eq!(d.newly_suspended.len(), 2);
        assert_eq!(d.currently_suspended, 2);
    }

    #[test]
    fn test_tracker_duplicate_names_emit_once_per_poll() {
        // 2026-07-10 hostile-review LOW: two rows with the SAME name in
        // one response (unreachable in practice) must produce ONE
        // newly_suspended entry, never N pages per poll.
        let mut t = WalSuspensionTracker::new();
        let d = t.observe(&[row("ticks", true), row("ticks", true)]);
        assert_eq!(d.newly_suspended.len(), 1);
        assert_eq!(d.currently_suspended, 1);
    }

    #[test]
    fn test_tracker_suspicious_empty_guard() {
        // 2026-07-10 hostile-review LOW: an empty dataset while tables
        // are latched suspended is SUSPICIOUS (server mid-start), not a
        // mass recovery — the task loop skips observe() on it.
        let mut t = WalSuspensionTracker::new();
        assert!(
            !t.is_suspicious_empty(&[]),
            "empty rows with an EMPTY latch is a legitimate no-WAL-tables answer"
        );
        t.observe(&[row("ticks", true)]);
        assert!(
            t.is_suspicious_empty(&[]),
            "empty rows with a NON-EMPTY latch must be suspicious"
        );
        assert!(
            !t.is_suspicious_empty(&[row("ticks", false)]),
            "a non-empty response is never suspicious — normal observe path"
        );
        // The latch survives the skipped poll: the next honest poll with
        // the table still suspended must NOT re-fire the rising edge.
        let d = t.observe(&[row("ticks", true)]);
        assert!(
            d.newly_suspended.is_empty(),
            "latch preserved across the suspicious-empty skip"
        );
    }

    #[test]
    fn test_probe_failure_labels_are_static_and_distinct() {
        let labels = [
            WalProbeFailure::Http.as_str(),
            WalProbeFailure::Status.as_str(),
            WalProbeFailure::Parse.as_str(),
            WalProbeFailure::MissingColumn.as_str(),
        ];
        let set: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(set.len(), labels.len(), "labels must be distinct");
    }

    #[test]
    fn test_poll_interval_and_backoff_sane() {
        // Too short = wasted QuestDB budget; too long = slow detection.
        assert!(WAL_SUSPENSION_POLL_INTERVAL_SECS >= 30);
        assert!(WAL_SUSPENSION_POLL_INTERVAL_SECS <= 300);
        assert!(WAL_WATCHER_RESPAWN_BACKOFF_SECS >= 1);
        assert!(WAL_WATCHER_RESPAWN_BACKOFF_SECS <= 30);
        // The query constant must stay URL-safe (no raw spaces) and target
        // wal_tables() with a * projection (rename-tolerant).
        assert!(!WAL_TABLES_QUERY_URLENCODED.contains(' '));
        assert!(WAL_TABLES_QUERY_URLENCODED.contains("wal_tables()"));
        assert!(WAL_TABLES_QUERY_URLENCODED.contains("select%20*%20from"));
    }

    #[tokio::test]
    async fn test_spawn_supervised_wal_suspension_watcher_keeps_running() {
        // The supervisor is an infinite loop — its JoinHandle must NOT
        // resolve in normal operation (the inner watcher parks on its 60s
        // ticker; port 1 probes fail soft). Mirrors the disk-watcher guard.
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let handle = spawn_supervised_wal_suspension_watcher(cfg);
        tokio::task::yield_now().await;
        assert!(
            !handle.is_finished(),
            "supervisor must keep running, not exit after spawning the watcher"
        );
        handle.abort();
    }
}
