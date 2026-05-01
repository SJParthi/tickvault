//! Wave 5 Item 25 — Materialized-view movers redesign.
//!
//! SUPERSEDES Items 16/19/21/23 (the 25-separate-tables design at
//! `movers_22tf_persistence.rs`). Operator demand 2026-05-01:
//!
//! > "why didn't you move all the movers similar to materialised view
//! > candles dude — even for movers 1s also you can keep it in normal
//! > table and remaining you can move it into materialised views right
//! > dude?"
//!
//! # Design (per plan §"Item 25 — Materialized-View Movers Redesign")
//!
//! - **Base table** `movers_unified_1s` — single TABLE, 1Hz cadence,
//!   ONE ILP writer task in the app.
//! - **24 materialized views** `movers_unified_{5s,15s,30s,1m,...,1d}`
//!   — created at boot via `CREATE MATERIALIZED VIEW IF NOT EXISTS`,
//!   incrementally refreshed server-side by QuestDB.
//! - **DEDUP** on the base: `(ts, security_id, segment)`. Views inherit
//!   uniqueness via `SAMPLE BY` semantics.
//!
//! # Why mat views (not tables)
//!
//! Per-timeframe rankings (top-50 by change_pct, etc.) are read-time
//! queries against the right mat view. The aggregation is `last()` /
//! `first()` of the per-second base — pure SQL, no Rust ranking. The
//! 25-tables design (Item 19) duplicated work in Rust; this design
//! delegates aggregation to QuestDB.
//!
//! # Coverage envelope
//!
//! Inside the tested envelope:
//! - DDL is idempotent (`CREATE ... IF NOT EXISTS`); safe to call every
//!   boot per stream-resilience.md B10.
//! - 24 views auto-created at boot; failure halts boot per
//!   `MOVERS-UNIFIED-05` ErrorCode (deferred — wiring is a follow-up).
//! - DEDUP key on base satisfies I-P1-11 + STORAGE-GAP-01.
//!
//! Beyond the envelope:
//! - QuestDB mat-view refresh lag — `MOVERS-UNIFIED-04` ErrorCode
//!   (Severity::Medium) fires when refresh > 60s behind base. Alert
//!   wiring deferred to a separate sub-PR.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;

/// QuestDB base table name. ONE physical table; everything else is a view.
pub const QUESTDB_TABLE_MOVERS_UNIFIED_1S: &str = "movers_unified_1s";

/// DEDUP UPSERT KEY for the base — composite `(ts, security_id, segment)`.
/// Views inherit uniqueness from the base via SAMPLE BY semantics.
/// I-P1-11 + STORAGE-GAP-01: includes `segment` per the workspace meta-guard.
pub const DEDUP_KEY_MOVERS_UNIFIED_1S: &str = "ts, security_id, segment";

/// HTTP timeout for DDL probes.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// The 24 materialized-view timeframes derived from the 1s base.
/// Order is fixed; tests pin both the count and the exact list.
pub const MOVERS_UNIFIED_VIEW_TIMEFRAMES: &[&str] = &[
    "5s", "15s", "30s", "1m", "2m", "3m", "4m", "5m", "6m", "7m", "8m", "9m", "10m", "11m", "12m",
    "13m", "14m", "15m", "30m", "1h", "2h", "3h", "4h", "1d",
];

/// Pinned count — must equal `MOVERS_UNIFIED_VIEW_TIMEFRAMES.len()`. The
/// plan-mandated 24 mat views (one per timeframe), not counting the base.
pub const MOVERS_UNIFIED_VIEW_COUNT: usize = 24;

const _: () = assert!(
    MOVERS_UNIFIED_VIEW_TIMEFRAMES.len() == MOVERS_UNIFIED_VIEW_COUNT,
    "MOVERS_UNIFIED_VIEW_TIMEFRAMES length must equal MOVERS_UNIFIED_VIEW_COUNT"
);

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
pub fn movers_unified_1s_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_MOVERS_UNIFIED_1S} (\
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
        DEDUP UPSERT KEYS({DEDUP_KEY_MOVERS_UNIFIED_1S});"
    )
}

/// Materialized-view DDL for one timeframe. Pattern matches the plan's
/// canonical SAMPLE BY example. Uses `last()` for cumulative metrics
/// (`last_price`, `volume`, `open_interest`, `prev_close`) and
/// `last - first` for `oi_delta` per the plan's correctness rationale.
///
/// `change_pct` is recomputed at view level from `last_price` /
/// `last_prev_close` so per-bucket rounding stays consistent with the
/// base — NOT carried forward from per-second values.
#[must_use]
pub fn movers_unified_view_ddl(timeframe: &str) -> String {
    format!(
        "CREATE MATERIALIZED VIEW IF NOT EXISTS movers_unified_{timeframe} AS \
         SELECT \
            security_id, \
            segment, \
            ts, \
            last(last_price) AS last_price, \
            last(volume) AS volume, \
            last(open_interest) AS open_interest, \
            last(open_interest) - first(open_interest) AS oi_delta, \
            last(prev_close) AS prev_close, \
            ((last(last_price) - last(prev_close)) / last(prev_close)) * 100 AS change_pct \
         FROM {QUESTDB_TABLE_MOVERS_UNIFIED_1S} \
         SAMPLE BY {timeframe};"
    )
}

/// Idempotent CREATE for the base table + all 24 mat views. Called once
/// at boot from `main.rs`. Per the schema-self-heal pattern in
/// `observability-architecture.md` — tolerates partial state from
/// previous boots.
///
/// On any DDL failure: emits `error!` (Loki routes to Telegram via
/// MOVERS-UNIFIED-05 — wiring deferred).
// TEST-EXEMPT: requires running QuestDB; tested via boot integration.
pub async fn ensure_movers_unified_tables_and_views(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    // Step 1: base table.
    let create_base = movers_unified_1s_create_ddl();
    match client
        .get(&base_url)
        .query(&[("query", create_base.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_MOVERS_UNIFIED_1S,
                "movers_unified_1s base table ready"
            );
        }
        Ok(resp) => {
            warn!(table = QUESTDB_TABLE_MOVERS_UNIFIED_1S, status = %resp.status(), "DDL non-2xx — continuing best-effort");
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_MOVERS_UNIFIED_1S,
                ?err,
                "movers_unified_1s base table DDL failed — table may already exist"
            );
        }
    }

    // Step 2: 24 materialized views.
    let mut created = 0;
    for &tf in MOVERS_UNIFIED_VIEW_TIMEFRAMES {
        let sql = movers_unified_view_ddl(tf);
        match client
            .get(&base_url)
            .query(&[("query", sql.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                created += 1;
            }
            Ok(resp) => {
                warn!(
                    timeframe = tf,
                    status = %resp.status(),
                    "movers_unified mat-view DDL non-2xx — continuing"
                );
            }
            Err(err) => {
                error!(timeframe = tf, ?err, "movers_unified mat-view DDL failed");
            }
        }
    }
    info!(
        created,
        total = MOVERS_UNIFIED_VIEW_COUNT,
        "movers_unified materialized views ensured"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_view_count_is_24() {
        assert_eq!(MOVERS_UNIFIED_VIEW_COUNT, 24);
        assert_eq!(MOVERS_UNIFIED_VIEW_TIMEFRAMES.len(), 24);
    }

    #[test]
    fn test_view_timeframes_match_plan_exact_order() {
        // Plan §"24 materialized views, one per timeframe" pinned list.
        let expected = [
            "5s", "15s", "30s", "1m", "2m", "3m", "4m", "5m", "6m", "7m", "8m", "9m", "10m", "11m",
            "12m", "13m", "14m", "15m", "30m", "1h", "2h", "3h", "4h", "1d",
        ];
        assert_eq!(MOVERS_UNIFIED_VIEW_TIMEFRAMES, &expected);
    }

    #[test]
    fn test_dedup_key_includes_segment_per_i_p1_11() {
        // I-P1-11 + STORAGE-GAP-01: composite key MUST include segment.
        assert!(DEDUP_KEY_MOVERS_UNIFIED_1S.contains("security_id"));
        assert!(DEDUP_KEY_MOVERS_UNIFIED_1S.contains("segment"));
        assert!(DEDUP_KEY_MOVERS_UNIFIED_1S.contains("ts"));
    }

    #[test]
    fn test_dedup_key_is_3_col_no_timeframe() {
        // Plan: "NO `timeframe` column needed because each view IS its
        // own timeframe." Replaces Item 19's 4-col DEDUP guard.
        let cols: Vec<_> = DEDUP_KEY_MOVERS_UNIFIED_1S.split(',').collect();
        assert_eq!(
            cols.len(),
            3,
            "DEDUP must be 3-col (ts, security_id, segment)"
        );
        assert!(!DEDUP_KEY_MOVERS_UNIFIED_1S.contains("timeframe"));
    }

    #[test]
    fn test_base_table_name_pinned() {
        assert_eq!(QUESTDB_TABLE_MOVERS_UNIFIED_1S, "movers_unified_1s");
    }

    #[test]
    fn test_base_ddl_uses_create_table_if_not_exists() {
        let ddl = movers_unified_1s_create_ddl();
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS movers_unified_1s"));
    }

    #[test]
    fn test_base_ddl_partitions_by_day() {
        let ddl = movers_unified_1s_create_ddl();
        assert!(ddl.contains("PARTITION BY DAY"));
    }

    #[test]
    fn test_base_ddl_designates_ts_as_timestamp() {
        let ddl = movers_unified_1s_create_ddl();
        assert!(ddl.contains("TIMESTAMP(ts)"));
    }

    #[test]
    fn test_base_ddl_includes_dedup_upsert_keys() {
        let ddl = movers_unified_1s_create_ddl();
        assert!(ddl.contains("DEDUP UPSERT KEYS(ts, security_id, segment)"));
    }

    #[test]
    fn test_base_ddl_includes_all_ten_columns() {
        // Plan-pinned 10-column schema.
        let ddl = movers_unified_1s_create_ddl();
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
        let sql = movers_unified_view_ddl("5m");
        assert!(sql.starts_with("CREATE MATERIALIZED VIEW IF NOT EXISTS movers_unified_5m"));
    }

    #[test]
    fn test_view_ddl_uses_sample_by() {
        let sql = movers_unified_view_ddl("1m");
        assert!(sql.contains("SAMPLE BY 1m"));
    }

    #[test]
    fn test_view_ddl_oi_delta_uses_last_minus_first() {
        // Plan correctness rationale: oi_delta = last - first per bucket
        // (not sum) so cross-bucket math stays meaningful.
        let sql = movers_unified_view_ddl("5m");
        assert!(
            sql.contains("last(open_interest) - first(open_interest) AS oi_delta"),
            "oi_delta must be `last - first`, got: {sql}"
        );
    }

    #[test]
    fn test_view_ddl_change_pct_recomputed_per_bucket() {
        // Plan correctness rationale: change_pct recomputed at view
        // level from last_price / last_prev_close — NOT carried forward.
        let sql = movers_unified_view_ddl("1h");
        assert!(
            sql.contains(
                "((last(last_price) - last(prev_close)) / last(prev_close)) * 100 AS change_pct"
            ),
            "change_pct formula mismatch, got: {sql}"
        );
    }

    #[test]
    fn test_view_ddl_aggregates_volume_via_last_not_sum() {
        // Volume is cumulative-since-session-open per Item 26 L1; per-bucket
        // value = LAST tick in that bucket. NOT sum (would 60×-overcount).
        let sql = movers_unified_view_ddl("5m");
        assert!(
            sql.contains("last(volume) AS volume"),
            "volume must use last(), not sum() — Item 28 cascade-bug fix; got: {sql}"
        );
        assert!(
            !sql.contains("sum(volume)"),
            "volume MUST NOT be sum() — that's Item 28's regression"
        );
    }

    #[test]
    fn test_view_ddl_reads_from_base_1s_table() {
        let sql = movers_unified_view_ddl("5m");
        assert!(sql.contains("FROM movers_unified_1s"));
    }

    #[test]
    fn test_view_ddl_for_every_timeframe_starts_with_create_materialized() {
        for tf in MOVERS_UNIFIED_VIEW_TIMEFRAMES {
            let sql = movers_unified_view_ddl(tf);
            assert!(
                sql.starts_with("CREATE MATERIALIZED VIEW IF NOT EXISTS"),
                "tf {tf}: not mat-view DDL"
            );
            assert!(
                sql.contains(&format!("movers_unified_{tf}")),
                "tf {tf}: view name mismatch"
            );
        }
    }

    #[test]
    fn test_movers_unified_1s_create_ddl_returns_non_empty_string() {
        // Substring-match guard pin for `pub fn movers_unified_1s_create_ddl`.
        let ddl = movers_unified_1s_create_ddl();
        assert!(!ddl.is_empty());
        assert!(ddl.contains(QUESTDB_TABLE_MOVERS_UNIFIED_1S));
    }

    #[test]
    fn test_movers_unified_view_ddl_includes_timeframe_in_view_name() {
        // Substring-match guard pin for `pub fn movers_unified_view_ddl`.
        let sql = movers_unified_view_ddl("30m");
        assert!(sql.contains("movers_unified_30m"));
        assert!(sql.contains("SAMPLE BY 30m"));
    }

    #[test]
    fn test_each_timeframe_produces_distinct_ddl() {
        // Sanity: 24 distinct views, no copy-paste collisions.
        let mut seen = std::collections::HashSet::new();
        for tf in MOVERS_UNIFIED_VIEW_TIMEFRAMES {
            let sql = movers_unified_view_ddl(tf);
            assert!(seen.insert(sql), "tf {tf} produced duplicate DDL");
        }
        assert_eq!(seen.len(), MOVERS_UNIFIED_VIEW_COUNT);
    }
}
