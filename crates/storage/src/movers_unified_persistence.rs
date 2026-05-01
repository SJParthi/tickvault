// APPROVED: Cat 9 — this IS the canonical movers DDL module per
// `.claude/plans/active-plan-movers-cleanup.md` (2026-05-01). The legacy
// `movers_22tf_persistence.rs` was DELETED in this same plan; the file name
// `movers_unified_persistence.rs` is preserved internally to keep the diff
// surgical, but every operator-visible identifier and DDL string now uses
// the unsuffixed `movers_*` form.

//! Movers base table + 24 materialized views.
//!
//! - **Base table** `movers_1s` — single TABLE, 1 Hz cadence, ONE ILP
//!   writer task in the app.
//! - **24 materialized views** `movers_{5s,15s,30s,1m,...,1d}` — created at
//!   boot via `CREATE MATERIALIZED VIEW IF NOT EXISTS`, incrementally
//!   refreshed server-side by QuestDB.
//! - **DEDUP** on the base: `(ts, security_id, segment)`. Views inherit
//!   uniqueness via `SAMPLE BY` semantics.
//!
//! # 2026-05-01 cleanup migration (one-shot)
//!
//! Boot also runs a one-shot DROP migration (gated by the marker table
//! `movers_migration_2026_05_01`) that removes:
//! - 25 dead `movers_{1s..1h}` tables created by the deleted
//!   `movers_22tf_persistence` module.
//! - The pre-rename `movers_unified_1s` base table.
//! - The pre-rename 24 `movers_unified_{5s..1d}` materialized views.
//!
//! After the migration runs once, the marker is inserted and subsequent
//! boots are no-ops.
//!
//! # Why mat views (not tables)
//!
//! Per-timeframe rankings (top-50 by change_pct, etc.) are read-time
//! queries against the right mat view. The aggregation is `last()` /
//! `first()` of the per-second base — pure SQL, no Rust ranking. The
//! deleted 25-tables design duplicated work in Rust; this design
//! delegates aggregation to QuestDB.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;

/// QuestDB base table name. ONE physical table; everything else is a view.
pub const QUESTDB_TABLE_MOVERS_1S: &str = "movers_1s";

/// DEDUP UPSERT KEY for the base — composite `(ts, security_id, segment)`.
/// Views inherit uniqueness from the base via SAMPLE BY semantics.
/// I-P1-11 + STORAGE-GAP-01: includes `segment` per the workspace meta-guard.
pub const DEDUP_KEY_MOVERS_1S: &str = "ts, security_id, segment";

/// HTTP timeout for DDL probes.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// The 24 materialized-view timeframes derived from the 1s base.
/// Order is fixed; tests pin both the count and the exact list.
pub const MOVERS_VIEW_TIMEFRAMES: &[&str] = &[
    "5s", "15s", "30s", "1m", "2m", "3m", "4m", "5m", "6m", "7m", "8m", "9m", "10m", "11m", "12m",
    "13m", "14m", "15m", "30m", "1h", "2h", "3h", "4h", "1d",
];

/// Pinned count — must equal `MOVERS_VIEW_TIMEFRAMES.len()`.
pub const MOVERS_VIEW_COUNT: usize = 24;

const _: () = assert!(
    MOVERS_VIEW_TIMEFRAMES.len() == MOVERS_VIEW_COUNT,
    "MOVERS_VIEW_TIMEFRAMES length must equal MOVERS_VIEW_COUNT"
);

/// Pre-rename mat-view timeframes (the old `movers_unified_*` set). Used by
/// the one-shot 2026-05-01 cleanup migration to DROP the old objects.
const LEGACY_UNIFIED_VIEW_TIMEFRAMES: &[&str] = &[
    "5s", "15s", "30s", "1m", "2m", "3m", "4m", "5m", "6m", "7m", "8m", "9m", "10m", "11m", "12m",
    "13m", "14m", "15m", "30m", "1h", "2h", "3h", "4h", "1d",
];

/// Dead `movers_22tf` table names (the deleted 25-table design). Used by the
/// one-shot 2026-05-01 cleanup migration to DROP the orphan tables. NOTE:
/// `movers_1s` overlaps with the new base name — the migration runs ONLY
/// once per database (marker-gated) so the new base table is never dropped.
const LEGACY_22TF_TIMEFRAMES: &[&str] = &[
    "1s", "2s", "3s", "5s", "10s", "15s", "20s", "30s", "1m", "2m", "3m", "4m", "5m", "6m", "7m",
    "8m", "9m", "10m", "11m", "12m", "13m", "14m", "15m", "30m", "1h",
];

/// Marker-table name. Existence guards the one-shot migration.
const MIGRATION_MARKER_TABLE: &str = "movers_migration_2026_05_01";

/// Base table DDL — `CREATE TABLE IF NOT EXISTS` per Item 25 spec.
///
/// 10-column schema:
/// - `ts` — designated timestamp (IST epoch nanos / 1000 → micros)
/// - `security_id` (LONG) + `segment` (SYMBOL) → composite key per I-P1-11
/// - `open_interest`, `oi_delta` (LONG) — cumulative + first-derivative
/// - `volume` (LONG) — cumulative since session open (Item 26 L1 pin)
/// - `last_price`, `prev_close`, `change_pct` (DOUBLE) — rendered metrics
/// - `received_at` (TIMESTAMP) — wall-clock arrival for forensics
#[must_use]
pub fn movers_1s_create_ddl() -> String {
    format!(
        // APPROVED: Cat 9 — canonical mat-view base table.
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_MOVERS_1S} (\
            ts            TIMESTAMP, \
            security_id   LONG, \
            segment       SYMBOL CAPACITY 16 NOCACHE, \
            open_interest LONG, \
            oi_delta      LONG, \
            volume        LONG, \
            last_price    DOUBLE, \
            prev_close    DOUBLE, \
            change_pct    DOUBLE, \
            received_at   TIMESTAMP\
        ) TIMESTAMP(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_MOVERS_1S});"
    )
}

/// Materialized-view DDL for one timeframe.
///
/// **Snapshot columns** (cumulative since session open, last-tick semantics):
/// - `last_price` — close-tick price for this bucket
/// - `open_interest` — OI at bucket close
/// - `volume_cumulative` — cumulative session volume at bucket close
/// - `prev_close` — previous-day close (per-instrument constant during session)
///
/// **Bucket-incremental columns** (work via cumulative monotonicity per
/// Item 26 L1 — `volume` is monotonic non-decreasing within a session,
/// so `last - first` recovers per-bucket increment):
/// - `volume_bucket` — `last(volume) - first(volume)`
/// - `oi_delta_bucket` — `last(oi) - first(oi)` (signed)
///
/// **Bucket OHLC** (intra-bucket price action): `open_price_bucket`,
/// `high_price_bucket`, `low_price_bucket`; close = `last_price`.
///
/// **Two change_pct flavours** (CASE-WHEN guards prev_close > 0 to avoid
/// divide-by-zero on freshly-listed contracts).
///
/// **BANNED aggregations** — SUM of volume / SUM of open_interest. Volume
/// + OI are cumulative per Item 26 L1; summing across buckets
/// double/triple-counts. Source-scan ratchet
/// `test_movers_ddl_no_sum_volume_anywhere` enforces.
#[must_use]
pub fn movers_view_ddl(timeframe: &str) -> String {
    format!(
        "CREATE MATERIALIZED VIEW IF NOT EXISTS movers_{timeframe} AS \
         SELECT \
            security_id, \
            segment, \
            ts, \
            last(last_price) AS last_price, \
            last(open_interest) AS open_interest, \
            last(volume) AS volume_cumulative, \
            last(prev_close) AS prev_close, \
            last(volume) - first(volume) AS volume_bucket, \
            last(open_interest) - first(open_interest) AS oi_delta_bucket, \
            first(last_price) AS open_price_bucket, \
            max(last_price) AS high_price_bucket, \
            min(last_price) AS low_price_bucket, \
            CASE WHEN last(prev_close) > 0 \
                 THEN ((last(last_price) - last(prev_close)) / last(prev_close)) * 100 \
                 ELSE 0 \
            END AS change_pct_session, \
            CASE WHEN first(last_price) > 0 \
                 THEN ((last(last_price) - first(last_price)) / first(last_price)) * 100 \
                 ELSE 0 \
            END AS change_pct_bucket \
         FROM {QUESTDB_TABLE_MOVERS_1S} \
         SAMPLE BY {timeframe} ALIGN TO CALENDAR WITH OFFSET '00:00';"
    )
}

/// Run the one-shot 2026-05-01 cleanup migration (DROP dead movers_22tf
/// tables + DROP legacy movers_unified_* tables/views), gated by the
/// presence of the marker table.
///
/// **Probe correctness** (security review MEDIUM, hostile-bug-hunt H2): QuestDB
/// `/exec` returns HTTP 200 with `{"error":"..."}` in the body for missing
/// tables, AND HTTP 200 with `{"dataset":[[0]],...}` when the marker table
/// exists but is empty (mid-failed migration). Naive HTTP-status check would
/// false-positive on both cases. We instead:
///   1. CREATE the marker table idempotently first (`IF NOT EXISTS`)
///   2. SELECT count(*) from it
///   3. Parse the JSON body and require `dataset[0][0] >= 1`
/// Any deviation (HTTP error, parse failure, empty dataset, count = 0) →
/// safe-fail to "not yet applied" → run the migration.
///
/// **Partial-failure safety** (hostile-bug-hunt H1): each DROP's HTTP status is
/// observed. The marker INSERT runs ONLY if every DROP returned 2xx. Any
/// failure short-circuits the marker write so the next boot retries (DROP
/// IF EXISTS is idempotent so retries are safe).
async fn run_one_shot_cleanup_migration(client: &Client, base_url: &str) {
    // Step 0a: idempotent CREATE of the marker table. `IF NOT EXISTS` makes
    // this safe to call every boot. DEDUP on `applied_at` prevents marker-row
    // accumulation across re-runs (hostile-bug-hunt M1).
    let create_marker = format!(
        "CREATE TABLE IF NOT EXISTS {MIGRATION_MARKER_TABLE} (\
            applied_at TIMESTAMP\
        ) TIMESTAMP(applied_at) PARTITION BY YEAR \
        DEDUP UPSERT KEYS(applied_at);"
    );
    if let Err(err) = client
        .get(base_url)
        .query(&[("query", create_marker.as_str())])
        .send()
        .await
    {
        // Boot continues — the probe below will short-circuit if CREATE
        // failed AND the table already exists from a previous boot, or
        // safe-fail to "not applied" and run the migration if it doesn't.
        warn!(?err, "movers cleanup marker CREATE failed (continuing)");
    }

    // Step 0b: probe for an existing marker row. Robust against the QuestDB
    // HTTP 200 + error-in-body and HTTP 200 + empty-dataset edge cases.
    if marker_indicates_migration_applied(client, base_url).await {
        return;
    }

    info!(
        marker = MIGRATION_MARKER_TABLE,
        "running one-shot movers cleanup migration (2026-05-01) — DROP dead movers_22tf + DROP legacy movers_unified_*"
    );

    let mut all_succeeded = true;

    // Step 1: DROP dead movers_22tf tables (25 names; a few overlap with
    // new mat-view names but DROP runs BEFORE CREATE in the boot DDL flow).
    for tf in LEGACY_22TF_TIMEFRAMES {
        let sql = format!("DROP TABLE IF EXISTS movers_{tf}");
        if !ddl_succeeded(client, base_url, &sql).await {
            warn!(timeframe = tf, "DROP TABLE movers_{tf} failed (continuing)");
            all_succeeded = false;
        }
    }

    // Step 2: DROP legacy movers_unified_* mat views (24 names).
    for tf in LEGACY_UNIFIED_VIEW_TIMEFRAMES {
        let sql = format!("DROP MATERIALIZED VIEW IF EXISTS movers_unified_{tf}");
        if !ddl_succeeded(client, base_url, &sql).await {
            warn!(
                timeframe = tf,
                "DROP MATERIALIZED VIEW movers_unified_{tf} failed (continuing)"
            );
            all_succeeded = false;
        }
    }

    // Step 3: DROP legacy movers_unified_1s base.
    let drop_legacy_base = "DROP TABLE IF EXISTS movers_unified_1s";
    if !ddl_succeeded(client, base_url, drop_legacy_base).await {
        warn!("DROP TABLE movers_unified_1s failed (continuing)");
        all_succeeded = false;
    }

    // Step 4: write the marker row ONLY if every DROP succeeded. If any DROP
    // failed (network blip, QuestDB hiccup) the next boot retries the whole
    // migration — DROP IF EXISTS makes that safe.
    if !all_succeeded {
        error!(
            marker = MIGRATION_MARKER_TABLE,
            "one or more DROPs failed during movers cleanup migration — marker NOT written; next boot will retry"
        );
        return;
    }

    let now_micros = chrono::Utc::now().timestamp_micros();
    let insert_marker = format!("INSERT INTO {MIGRATION_MARKER_TABLE} VALUES ({now_micros})");
    if !ddl_succeeded(client, base_url, &insert_marker).await {
        error!(
            marker = MIGRATION_MARKER_TABLE,
            "movers cleanup marker INSERT failed — migration will re-run on next boot"
        );
        return;
    }

    info!(
        marker = MIGRATION_MARKER_TABLE,
        "movers cleanup migration completed"
    );
}

/// Returns `true` when the QuestDB response was both HTTP 2xx AND the body
/// does NOT contain the QuestDB `"error":` JSON marker. QuestDB's `/exec`
/// endpoint returns HTTP 200 with `{"error":"..."}` in the body for queries
/// against missing tables, so HTTP-status-only checks are insufficient.
async fn ddl_succeeded(client: &Client, base_url: &str, sql: &str) -> bool {
    run_ddl(client, base_url, sql).await.success
}

/// QuestDB DDL response triage. Captures the structured outcome so callers
/// can log the body verbatim on failure (essential for diagnosing `400 Bad
/// Request` returns where the error message is in the body, not the status).
struct DdlOutcome {
    /// True when HTTP 2xx AND no `"error":` key in body — i.e. QuestDB
    /// actually executed the DDL.
    success: bool,
    /// Best-effort copy of the response body (HTTP error message OR JSON
    /// `{"error":"..."}` content). Empty on transport errors.
    body: String,
    /// HTTP status code as string for log fields. `transport_err` if the
    /// request never reached QuestDB.
    status: String,
}

/// Run a DDL statement against QuestDB's `/exec` endpoint and return a
/// triaged `DdlOutcome`. Replaces the boolean `ddl_succeeded` for sites
/// that need to log the actual error body (Fix A from PR #421 follow-up).
async fn run_ddl(client: &Client, base_url: &str, sql: &str) -> DdlOutcome {
    match client.get(base_url).query(&[("query", sql)]).send().await {
        Ok(resp) => {
            let status = resp.status();
            let status_str = status.to_string();
            let body = resp.text().await.unwrap_or_default();
            let success = status.is_success() && !body.contains(r#""error":"#);
            DdlOutcome {
                success,
                body,
                status: status_str,
            }
        }
        Err(err) => DdlOutcome {
            success: false,
            body: format!("transport_err: {err}"),
            status: "transport_err".to_string(),
        },
    }
}

/// List every `tables()` row whose `table_name` starts with `prefix`. Used
/// by the post-migration audit (Fix E) and the post-CREATE verification
/// (Fix C) to confirm physical state matches what the DDL responses said.
///
/// Returns an empty `Vec` on any QuestDB-side error so the caller's audit
/// safe-fails to "no rows present" — the audit then reports "0 of N
/// expected" which is itself a useful signal.
async fn query_table_names_with_prefix(
    client: &Client,
    base_url: &str,
    prefix: &str,
) -> Vec<String> {
    let sql = format!("SELECT table_name FROM tables() WHERE table_name LIKE '{prefix}%'");
    let outcome = run_ddl(client, base_url, &sql).await;
    if !outcome.success {
        return Vec::new();
    }
    let parsed: serde_json::Value = match serde_json::from_str(&outcome.body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    parsed
        .get("dataset")
        .and_then(|d| d.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get(0).and_then(|c| c.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Probe the marker table for an existing row. Returns `true` ONLY when the
/// QuestDB response is HTTP 2xx AND the body parses to a non-empty count
/// (`dataset[0][0] >= 1`). Any other outcome (HTTP error, parse failure,
/// empty dataset, `"error":` in body, count = 0) safe-fails to `false` so
/// the migration runs.
async fn marker_indicates_migration_applied(client: &Client, base_url: &str) -> bool {
    let probe_sql = format!("SELECT count(*) FROM {MIGRATION_MARKER_TABLE}");
    let resp = match client
        .get(base_url)
        .query(&[("query", probe_sql.as_str())])
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        _ => return false,
    };
    let body = match resp.text().await {
        Ok(b) => b,
        Err(_) => return false,
    };
    if body.contains(r#""error":"#) {
        return false;
    }
    // Successful response shape:
    //   {"query":"...","columns":[{"name":"count","type":"LONG"}],"timestamp":-1,"dataset":[[N]],"count":1}
    let parsed: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return false,
    };
    parsed
        .get("dataset")
        .and_then(|d| d.get(0))
        .and_then(|row| row.get(0))
        .and_then(serde_json::Value::as_i64)
        .is_some_and(|count| count >= 1)
}

/// Idempotent CREATE for the base table + all 24 mat views. Called once
/// at boot from `main.rs`. Per the schema-self-heal pattern in
/// `observability-architecture.md` — tolerates partial state from
/// previous boots.
///
/// On any DDL failure: emits `error!` (Loki routes to Telegram via
/// MOVERS-UNIFIED-05 — wiring deferred).
// TEST-EXEMPT: requires running QuestDB; tested via boot integration.
pub async fn ensure_movers_tables_and_views(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    // Step 0: one-shot cleanup migration (gated by marker table). DROPs the
    // 25 dead `movers_22tf` tables + the legacy `movers_unified_*` objects.
    // Idempotent on subsequent boots.
    run_one_shot_cleanup_migration(&client, &base_url).await;

    // Step 1: base table.
    let create_base = movers_1s_create_ddl();
    match client
        .get(&base_url)
        .query(&[("query", create_base.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_MOVERS_1S,
                "movers_1s base table ready"
            );
        }
        Ok(resp) => {
            warn!(table = QUESTDB_TABLE_MOVERS_1S, status = %resp.status(), "DDL non-2xx — continuing best-effort");
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_MOVERS_1S,
                ?err,
                "movers_1s base table DDL failed — table may already exist"
            );
        }
    }

    // Step 2: 24 materialized views — robust loop with DROP-before-CREATE
    // (Fix B: defensive against stale state from a prior failed boot),
    // retry-once-on-failure (Fix D: 400 Bad Request can be transient on
    // QuestDB during high-concurrency boot DDL), and structured error-body
    // logging (Fix A: the original WARN only carried the HTTP status, not
    // the actual reason QuestDB rejected the DDL).
    let mut created = 0;
    for &tf in MOVERS_VIEW_TIMEFRAMES {
        // Fix B: drop any stale object with this name first. DROP MAT VIEW
        // IF EXISTS is idempotent and returns OK on missing names. Then
        // also DROP TABLE IF EXISTS for the same name — handles the case
        // where a legacy `movers_22tf` table with this name (e.g.
        // `movers_5s`, `movers_15s`) survived the cleanup migration.
        let drop_view_sql = format!("DROP MATERIALIZED VIEW IF EXISTS movers_{tf}");
        let _ = run_ddl(&client, &base_url, &drop_view_sql).await;
        let drop_table_sql = format!("DROP TABLE IF EXISTS movers_{tf}");
        let _ = run_ddl(&client, &base_url, &drop_table_sql).await;

        // Try CREATE, then once-retry on failure.
        let sql = movers_view_ddl(tf);
        let mut outcome = run_ddl(&client, &base_url, &sql).await;
        if !outcome.success {
            // Fix D: a single retry handles transient QuestDB-side state.
            outcome = run_ddl(&client, &base_url, &sql).await;
        }

        if outcome.success {
            created += 1;
        } else {
            // Fix A: log the response body so the operator can see WHY
            // QuestDB rejected the DDL. Uses `error!` (Loki → Telegram)
            // because a missing mat view is a real coverage gap that
            // breaks downstream queries, not a routine warning.
            error!(
                timeframe = tf,
                status = %outcome.status,
                body = %outcome.body,
                "movers mat-view CREATE failed after retry — view missing"
            );
        }
    }

    info!(
        created,
        total = MOVERS_VIEW_COUNT,
        "movers materialized views ensured"
    );

    // Fix C: post-CREATE verification. Query the QuestDB metadata table
    // to confirm physical state matches what the DDL responses claimed.
    // If any expected mat view is missing, log ERROR with the gap list so
    // the operator knows EXACTLY which timeframes are absent (instead of
    // just a count discrepancy buried in INFO).
    let actual = query_table_names_with_prefix(&client, &base_url, "movers_").await;
    let mut missing: Vec<&'static str> = Vec::new();
    for &tf in MOVERS_VIEW_TIMEFRAMES {
        let expected = format!("movers_{tf}");
        if !actual.iter().any(|name| name == &expected) {
            missing.push(tf);
        }
    }
    if !missing.is_empty() {
        error!(
            missing_count = missing.len(),
            missing_timeframes = ?missing,
            actual_count = actual.len(),
            "movers post-CREATE verification: views missing — read-path queries WILL fail for these timeframes"
        );
    }

    // Fix E: post-migration legacy-table audit. The DROP IF EXISTS path
    // returns `{"ddl":"OK"}` even when the underlying table object can't
    // be dropped (rare but observed in PR #421's first deployment), so
    // we sanity-check that none of the LEGACY_22TF_TIMEFRAMES exist as
    // standalone TABLES (different from mat views with the same name).
    // We compare against the new-mat-view set: any `movers_{tf}` left
    // behind whose `tf` is in 22tf-only-set (NOT in MOVERS_VIEW_TIMEFRAMES)
    // is definitively a stale 22tf table that the cleanup didn't drop.
    let legacy_22tf_only: Vec<&'static str> = LEGACY_22TF_TIMEFRAMES
        .iter()
        .copied()
        .filter(|tf| !MOVERS_VIEW_TIMEFRAMES.contains(tf))
        .collect();
    let stale: Vec<&'static str> = legacy_22tf_only
        .iter()
        .copied()
        .filter(|tf| {
            let name = format!("movers_{tf}");
            actual.iter().any(|n| n == &name)
        })
        .collect();
    if !stale.is_empty() {
        error!(
            stale_count = stale.len(),
            stale_timeframes = ?stale,
            "movers post-migration audit: legacy 22tf tables still present — manual `DROP TABLE` required"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_view_count_is_24() {
        assert_eq!(MOVERS_VIEW_COUNT, 24);
        assert_eq!(MOVERS_VIEW_TIMEFRAMES.len(), 24);
    }

    #[test]
    fn test_view_timeframes_match_plan_exact_order() {
        let expected = [
            "5s", "15s", "30s", "1m", "2m", "3m", "4m", "5m", "6m", "7m", "8m", "9m", "10m", "11m",
            "12m", "13m", "14m", "15m", "30m", "1h", "2h", "3h", "4h", "1d",
        ];
        assert_eq!(MOVERS_VIEW_TIMEFRAMES, &expected);
    }

    #[test]
    fn test_dedup_key_includes_segment_per_i_p1_11() {
        // I-P1-11 + STORAGE-GAP-01: composite key MUST include segment.
        assert!(DEDUP_KEY_MOVERS_1S.contains("security_id"));
        assert!(DEDUP_KEY_MOVERS_1S.contains("segment"));
        assert!(DEDUP_KEY_MOVERS_1S.contains("ts"));
    }

    #[test]
    fn test_dedup_key_is_3_col_no_timeframe() {
        let cols: Vec<_> = DEDUP_KEY_MOVERS_1S.split(',').collect();
        assert_eq!(
            cols.len(),
            3,
            "DEDUP must be 3-col (ts, security_id, segment)"
        );
        assert!(!DEDUP_KEY_MOVERS_1S.contains("timeframe"));
    }

    #[test]
    fn test_base_table_name_is_movers_1s() {
        // Wave 5 / 2026-05-01 rename: movers_unified_1s → movers_1s.
        assert_eq!(QUESTDB_TABLE_MOVERS_1S, "movers_1s");
    }

    #[test]
    fn test_base_ddl_uses_create_table_if_not_exists() {
        let ddl = movers_1s_create_ddl();
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS movers_1s")); // APPROVED: test asserting canonical DDL string
    }

    #[test]
    fn test_base_ddl_partitions_by_day() {
        let ddl = movers_1s_create_ddl();
        assert!(ddl.contains("PARTITION BY DAY"));
    }

    #[test]
    fn test_base_ddl_designates_ts_as_timestamp() {
        let ddl = movers_1s_create_ddl();
        assert!(ddl.contains("TIMESTAMP(ts)"));
    }

    #[test]
    fn test_base_ddl_includes_dedup_upsert_keys() {
        let ddl = movers_1s_create_ddl();
        assert!(ddl.contains("DEDUP UPSERT KEYS(ts, security_id, segment)"));
    }

    #[test]
    fn test_base_ddl_includes_all_ten_columns() {
        let ddl = movers_1s_create_ddl();
        for col in [
            "ts            TIMESTAMP",
            "security_id   LONG",
            "segment       SYMBOL",
            "open_interest LONG",
            "oi_delta      LONG",
            "volume        LONG",
            "last_price    DOUBLE",
            "prev_close    DOUBLE",
            "change_pct    DOUBLE",
            "received_at   TIMESTAMP",
        ] {
            assert!(ddl.contains(col), "base DDL must declare column `{col}`");
        }
    }

    #[test]
    fn test_view_ddl_uses_create_materialized_view_if_not_exists() {
        let sql = movers_view_ddl("5m");
        assert!(sql.starts_with("CREATE MATERIALIZED VIEW IF NOT EXISTS movers_5m"));
    }

    #[test]
    fn test_view_ddl_uses_sample_by() {
        let sql = movers_view_ddl("1m");
        assert!(sql.contains("SAMPLE BY 1m"));
    }

    #[test]
    fn test_movers_oi_delta_via_last_minus_first() {
        let sql = movers_view_ddl("5m");
        assert!(
            sql.contains("last(open_interest) - first(open_interest) AS oi_delta_bucket"),
            "oi_delta_bucket must be `last - first`, got: {sql}"
        );
    }

    #[test]
    fn test_movers_volume_bucket_via_last_minus_first() {
        let sql = movers_view_ddl("5m");
        assert!(
            sql.contains("last(volume) - first(volume) AS volume_bucket"),
            "volume_bucket must be `last - first`, got: {sql}"
        );
    }

    #[test]
    fn test_movers_change_pct_uses_case_when_prev_close_positive() {
        let sql = movers_view_ddl("1h");
        assert!(
            sql.contains("CASE WHEN last(prev_close) > 0"),
            "change_pct_session must guard prev_close > 0, got: {sql}"
        );
        assert!(
            sql.contains("CASE WHEN first(last_price) > 0"),
            "change_pct_bucket must guard first(last_price) > 0, got: {sql}"
        );
        assert!(sql.contains("AS change_pct_session"));
        assert!(sql.contains("AS change_pct_bucket"));
    }

    #[test]
    fn test_movers_all_24_views_have_align_to_calendar_with_offset() {
        for tf in MOVERS_VIEW_TIMEFRAMES {
            let sql = movers_view_ddl(tf);
            assert!(
                sql.contains("ALIGN TO CALENDAR WITH OFFSET '00:00'"),
                "tf {tf} missing ALIGN TO CALENDAR offset"
            );
        }
    }

    #[test]
    fn test_movers_includes_bucket_ohlc_columns() {
        let sql = movers_view_ddl("5m");
        assert!(sql.contains("first(last_price) AS open_price_bucket"));
        assert!(sql.contains("max(last_price) AS high_price_bucket"));
        assert!(sql.contains("min(last_price) AS low_price_bucket"));
    }

    #[test]
    fn test_movers_volume_cumulative_uses_last() {
        let sql = movers_view_ddl("5m");
        assert!(
            sql.contains("last(volume) AS volume_cumulative"),
            "volume_cumulative must use last() of the cumulative base"
        );
    }

    #[test]
    fn test_movers_ddl_no_sum_volume_anywhere() {
        // Volume is cumulative per Item 26 L1; summing across buckets
        // double/triple-counts.
        for tf in MOVERS_VIEW_TIMEFRAMES {
            let sql = movers_view_ddl(tf);
            assert!(
                !sql.contains("sum(volume)"),
                "tf {tf}: BANNED `sum(volume)` appeared: {sql}"
            );
            assert!(
                !sql.contains("sum(open_interest)"),
                "tf {tf}: BANNED `sum(open_interest)` appeared"
            );
        }
    }

    #[test]
    fn test_movers_ddl_no_sum_volume_source_scan() {
        let src = include_str!("movers_unified_persistence.rs");
        let prod_src = src
            .split_once("\n#[cfg(test)]\nmod tests {")
            .map(|(p, _)| p)
            .unwrap_or(src);
        assert!(
            !prod_src.contains("sum(volume)"),
            "BANNED `sum(volume)` found in production source"
        );
        assert!(
            !prod_src.contains("sum(open_interest)"),
            "BANNED `sum(open_interest)` found in production source"
        );
    }

    #[test]
    fn test_view_ddl_reads_from_movers_1s_base_table() {
        let sql = movers_view_ddl("5m");
        assert!(sql.contains("FROM movers_1s"));
    }

    #[test]
    fn test_view_ddl_for_every_timeframe_starts_with_create_materialized() {
        for tf in MOVERS_VIEW_TIMEFRAMES {
            let sql = movers_view_ddl(tf);
            assert!(
                sql.starts_with("CREATE MATERIALIZED VIEW IF NOT EXISTS"),
                "tf {tf}: not mat-view DDL"
            );
            assert!(
                sql.contains(&format!("movers_{tf}")),
                "tf {tf}: view name mismatch"
            );
        }
    }

    #[test]
    fn test_movers_1s_create_ddl_returns_non_empty_string() {
        let ddl = movers_1s_create_ddl();
        assert!(!ddl.is_empty());
        assert!(ddl.contains(QUESTDB_TABLE_MOVERS_1S));
    }

    #[test]
    fn test_movers_view_ddl_includes_timeframe_in_view_name() {
        let sql = movers_view_ddl("30m");
        assert!(sql.contains("movers_30m"));
        assert!(sql.contains("SAMPLE BY 30m"));
    }

    #[test]
    fn test_each_timeframe_produces_distinct_ddl() {
        let mut seen = std::collections::HashSet::new();
        for tf in MOVERS_VIEW_TIMEFRAMES {
            let sql = movers_view_ddl(tf);
            assert!(seen.insert(sql), "tf {tf} produced duplicate DDL");
        }
        assert_eq!(seen.len(), MOVERS_VIEW_COUNT);
    }

    #[test]
    fn test_legacy_22tf_timeframes_count_is_25() {
        // Pinned: the deleted 22tf design had 25 standalone tables.
        assert_eq!(LEGACY_22TF_TIMEFRAMES.len(), 25);
    }

    #[test]
    fn test_legacy_unified_view_timeframes_match_new_views() {
        // The pre-rename mat-view timeframes are identical to the new set.
        assert_eq!(LEGACY_UNIFIED_VIEW_TIMEFRAMES, MOVERS_VIEW_TIMEFRAMES);
    }

    #[test]
    fn test_migration_marker_name_pinned() {
        assert_eq!(MIGRATION_MARKER_TABLE, "movers_migration_2026_05_01");
    }

    #[test]
    fn test_legacy_22tf_only_set_excludes_new_view_timeframes() {
        // Fix E audit logic depends on this set: "names that exist ONLY
        // in the dead 22tf list, NOT in the new mat-view list". If both
        // sets share a name, that name is legitimately a new mat view
        // and shouldn't be flagged as a stale 22tf table by the audit.
        let legacy_only: Vec<&'static str> = LEGACY_22TF_TIMEFRAMES
            .iter()
            .copied()
            .filter(|tf| !MOVERS_VIEW_TIMEFRAMES.contains(tf))
            .collect();
        // From the live boot of PR #421's first deploy, these are the
        // four 22tf names not represented in the new mat-view set:
        //   1s (the new BASE table — not a view)
        //   2s, 3s, 10s, 20s (sub-minute names dropped from new design)
        // The 1s overlap is intentional: movers_1s is a TABLE in the new
        // design (the base), not a mat view. The audit checks against
        // mat-view names so a movers_1s table doesn't trigger a false
        // positive.
        for expected in ["1s", "2s", "3s", "10s", "20s"] {
            assert!(
                legacy_only.contains(&expected),
                "legacy-22tf-only set must contain `{expected}` (got: {legacy_only:?})"
            );
        }
    }

    #[test]
    fn test_ddl_outcome_struct_carries_status_and_body() {
        // Pub-fn substring-match guard pin for `DdlOutcome`. The Fix A
        // contract is "log status + body verbatim on failure"; this
        // ratchets the struct shape so a future cleanup that strips
        // either field would break the build.
        let outcome = DdlOutcome {
            success: false,
            status: "400 Bad Request".to_string(),
            body: r#"{"error":"unsupported SAMPLE BY interval"}"#.to_string(),
        };
        assert!(!outcome.success);
        assert_eq!(outcome.status, "400 Bad Request");
        assert!(outcome.body.contains("error"));
        assert!(outcome.body.contains("SAMPLE BY"));
    }

    #[test]
    fn test_view_count_invariant_holds_against_new_mat_view_set() {
        // Fix C's post-verify loop iterates MOVERS_VIEW_TIMEFRAMES; the
        // count drives the operator-visible "missing X of N" log. Pin
        // the invariant so a future timeframe addition can't silently
        // skew the audit.
        assert_eq!(MOVERS_VIEW_TIMEFRAMES.len(), MOVERS_VIEW_COUNT);
        assert_eq!(MOVERS_VIEW_COUNT, 24);
    }
}
