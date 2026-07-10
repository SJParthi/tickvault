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
///   `suspended` (server schema drift; fail-soft, loud counter upstream).
///
/// Individual malformed ROWS (non-array, missing cells, wrong cell types
/// for the mandatory columns) are skipped defensively so one bad row
/// cannot blind the probe to the remaining tables.
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
        // Edge-latch for parse/schema failures so a server-version drift
        // is loud ONCE per contiguous failure run, not every 60s forever.
        let mut schema_warned = false;
        let mut ticker =
            tokio::time::interval(Duration::from_secs(WAL_SUSPENSION_POLL_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            match probe_wal_tables(&base_url).await {
                Ok(rows) => {
                    schema_warned = false;
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
                }
                Err(failure) => {
                    metrics::counter!(
                        "tv_wal_suspension_probe_failed_total",
                        "reason" => failure.as_str()
                    )
                    .increment(1);
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
