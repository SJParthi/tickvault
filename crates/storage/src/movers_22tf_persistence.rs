//! Movers 22-timeframe persistence — DDL + table registration (Phase 8 of v3 plan).
//!
//! Creates 22 `movers_{T}` tables in QuestDB, one per timeframe in
//! `MOVERS_TIMEFRAMES`. Each table holds full-universe (~24K instruments)
//! snapshots at its cadence, written by `Movers22TfWriter` (one writer per
//! timeframe — see Phase 10).
//!
//! ## Schema (26 columns)
//!
//! See `tickvault_common::mover_types::MoverRow` for the full column list.
//! All 22 tables share the IDENTICAL schema.
//!
//! ## DEDUP UPSERT KEYS
//!
//! `(ts, security_id, segment)` — composite key per I-P1-11 (security_id
//! alone is NOT unique because Dhan reuses ids across segments).
//!
//! ## Partitioning
//!
//! `PARTITION BY DAY WAL` — one partition per trading day. Partition manager
//! detaches partitions older than retention_days (90d hot tier). All 22
//! tables MUST be added to `partition_manager.rs::HOUR_PARTITIONED_TABLES`
//! (despite the DAY partitioning, the term is "high-frequency tables that
//! need partition rotation"; the constant should arguably be renamed but
//! semantics are correct).
//!
//! ## See also
//!
//! - `.claude/plans/v2-architecture.md` Section A (movers core)
//! - `tickvault_common::mover_types::MOVERS_TIMEFRAMES` (the 22-entry catalog)

use std::time::Duration;

use reqwest::Client;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::mover_types::{MOVERS_TIMEFRAMES, MoversTimeframe};
use tracing::{info, warn};

/// HTTP timeout for DDL execution against QuestDB. Mirrors the value used
/// by `movers_persistence.rs::ensure_movers_tables`.
const DDL_TIMEOUT_SECS: u64 = 10;

/// DEDUP UPSERT KEYS for every `movers_{T}` table — composite key per
/// I-P1-11. Pinned by ratchet `test_movers_22tf_dedup_key_includes_segment`.
pub const DEDUP_KEY_MOVERS_22TF: &str = "ts, security_id, segment";

/// Returns the CREATE TABLE DDL for one `movers_{T}` table.
///
/// Idempotent (`CREATE TABLE IF NOT EXISTS`). Safe to call on every boot
/// per stream-resilience.md B10. Self-heals via `ALTER TABLE ADD COLUMN
/// IF NOT EXISTS` if columns are added later (handled by a separate
/// `ensure_movers_22tf_alter_columns` helper, not yet needed at v1).
#[must_use]
pub fn movers_22tf_create_ddl(timeframe: &MoversTimeframe) -> String {
    let table = timeframe.table_name();
    format!(
        "CREATE TABLE IF NOT EXISTS {table} (\
            ts                      TIMESTAMP, \
            security_id             INT, \
            segment                 SYMBOL CAPACITY 16 NOCACHE, \
            instrument_type         SYMBOL CAPACITY 32 NOCACHE, \
            underlying_symbol       SYMBOL CAPACITY 1024 CACHE, \
            underlying_security_id  INT, \
            expiry_date             DATE, \
            strike_price            DOUBLE, \
            option_type             SYMBOL CAPACITY 4 NOCACHE, \
            ltp                     DOUBLE, \
            prev_close              DOUBLE, \
            change_pct              DOUBLE, \
            change_abs              DOUBLE, \
            volume                  LONG, \
            buy_qty                 LONG, \
            sell_qty                LONG, \
            open_interest           LONG, \
            oi_change               LONG, \
            oi_change_pct           DOUBLE, \
            spot_price              DOUBLE, \
            best_bid                DOUBLE, \
            best_ask                DOUBLE, \
            spread_pct              DOUBLE, \
            bid_pressure_5          LONG, \
            ask_pressure_5          LONG, \
            received_at             TIMESTAMP\
        ) TIMESTAMP(ts) PARTITION BY DAY WAL"
    )
}

/// Returns the DEDUP-enable ALTER statement for one `movers_{T}` table.
#[must_use]
pub fn movers_22tf_dedup_ddl(timeframe: &MoversTimeframe) -> String {
    let table = timeframe.table_name();
    format!("ALTER TABLE {table} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_MOVERS_22TF})")
}

/// Creates all 22 `movers_{T}` tables and enables DEDUP on each.
///
/// Idempotent — safe to call on every boot (each CREATE / ALTER is itself
/// idempotent via `IF NOT EXISTS` and DEDUP is no-op if already enabled).
///
/// Best-effort: individual DDL failures are logged at WARN but do not
/// halt the function. Boot continues with whichever tables succeeded;
/// the `Movers22TfWriter` retries persist on the next snapshot.
// WIRING-EXEMPT: pending Phase 10 boot integration in main.rs (the call site lands when the snapshot scheduler ships per the Phase 10 implementation guide).
// TEST-EXEMPT: integration test requires live QuestDB; component tests cover DDL string output (test_movers_22tf_create_ddl_*).
pub async fn ensure_movers_22tf_tables(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = match Client::builder()
        .timeout(Duration::from_secs(DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(
                ?err,
                "movers_22tf: HTTP client build failed — falling back to default Client"
            );
            Client::new()
        }
    };

    info!(
        timeframe_count = MOVERS_TIMEFRAMES.len(),
        "movers_22tf: ensuring all {} tables exist + DEDUP enabled",
        MOVERS_TIMEFRAMES.len()
    );

    for timeframe in MOVERS_TIMEFRAMES {
        let create_sql = movers_22tf_create_ddl(timeframe);
        let create_label = format!("movers_{} CREATE", timeframe.label);
        execute_ddl(&client, &base_url, &create_sql, &create_label).await;

        let dedup_sql = movers_22tf_dedup_ddl(timeframe);
        let dedup_label = format!("movers_{} DEDUP", timeframe.label);
        execute_ddl(&client, &base_url, &dedup_sql, &dedup_label).await;
    }

    info!("movers_22tf: all 22 tables ensured");
}

/// Executes a DDL statement against QuestDB HTTP API. Best-effort.
async fn execute_ddl(client: &Client, base_url: &str, sql: &str, label: &str) {
    match client.get(base_url).query(&[("query", sql)]).send().await {
        Ok(response) => {
            if response.status().is_success() {
                info!("{label} DDL executed successfully");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    body = body.chars().take(200).collect::<String>(),
                    "{label} DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(?err, "{label} DDL request failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 8 ratchet: DDL contains all 26 columns.
    #[test]
    fn test_movers_22tf_create_ddl_contains_all_26_columns() {
        let ddl = movers_22tf_create_ddl(&MOVERS_TIMEFRAMES[0]);
        for col in [
            "ts ",
            "security_id ",
            "segment ",
            "instrument_type ",
            "underlying_symbol ",
            "underlying_security_id ",
            "expiry_date ",
            "strike_price ",
            "option_type ",
            "ltp ",
            "prev_close ",
            "change_pct ",
            "change_abs ",
            "volume ",
            "buy_qty ",
            "sell_qty ",
            "open_interest ",
            "oi_change ",
            "oi_change_pct ",
            "spot_price ",
            "best_bid ",
            "best_ask ",
            "spread_pct ",
            "bid_pressure_5 ",
            "ask_pressure_5 ",
            "received_at ",
        ] {
            assert!(
                ddl.contains(col),
                "movers_22tf DDL missing column `{col}`: {ddl}"
            );
        }
    }

    /// Phase 8 ratchet: DEDUP key includes segment per I-P1-11.
    #[test]
    fn test_movers_22tf_dedup_key_includes_segment() {
        assert!(
            DEDUP_KEY_MOVERS_22TF.contains("segment"),
            "I-P1-11 violation: DEDUP_KEY_MOVERS_22TF must include `segment`: {DEDUP_KEY_MOVERS_22TF}"
        );
        assert!(
            DEDUP_KEY_MOVERS_22TF.contains("security_id"),
            "DEDUP key must include security_id: {DEDUP_KEY_MOVERS_22TF}"
        );
        assert!(
            DEDUP_KEY_MOVERS_22TF.contains("ts"),
            "DEDUP key must include ts: {DEDUP_KEY_MOVERS_22TF}"
        );
    }

    /// Phase 8 ratchet: every timeframe produces a unique DDL with the
    /// correct table name embedded.
    #[test]
    fn test_movers_22tf_each_timeframe_has_unique_ddl() {
        let mut seen_tables: Vec<String> = Vec::new();
        for timeframe in MOVERS_TIMEFRAMES {
            let ddl = movers_22tf_create_ddl(timeframe);
            let expected_table = format!("movers_{}", timeframe.label);
            assert!(
                ddl.contains(&expected_table),
                "DDL for timeframe {} must contain table name {expected_table}",
                timeframe.label
            );
            assert!(
                !seen_tables.contains(&expected_table),
                "duplicate table name detected: {expected_table}"
            );
            seen_tables.push(expected_table);
        }
        assert_eq!(seen_tables.len(), 22);
    }

    /// Phase 8 ratchet: DEDUP statement is well-formed and refers to the
    /// expected table name.
    #[test]
    fn test_movers_22tf_dedup_ddl_format() {
        let timeframe = &MOVERS_TIMEFRAMES[5]; // "1m"
        let ddl = movers_22tf_dedup_ddl(timeframe);
        assert_eq!(
            ddl,
            "ALTER TABLE movers_1m DEDUP ENABLE UPSERT KEYS(ts, security_id, segment)"
        );
    }

    /// Phase 8 ratchet: PARTITION BY DAY WAL pinned for all tables.
    #[test]
    fn test_movers_22tf_partition_by_day_with_wal() {
        for timeframe in MOVERS_TIMEFRAMES {
            let ddl = movers_22tf_create_ddl(timeframe);
            assert!(
                ddl.contains("PARTITION BY DAY"),
                "movers_{} must use PARTITION BY DAY",
                timeframe.label
            );
            assert!(
                ddl.contains(" WAL"),
                "movers_{} must use WAL",
                timeframe.label
            );
            assert!(
                ddl.contains("TIMESTAMP(ts)"),
                "movers_{} must use TIMESTAMP(ts) as designated timestamp",
                timeframe.label
            );
        }
    }

    /// Phase 8 ratchet: column types match the spec (TIMESTAMP/INT/SYMBOL/
    /// DOUBLE/LONG/DATE).
    #[test]
    fn test_movers_22tf_column_types_match_spec() {
        let ddl = movers_22tf_create_ddl(&MOVERS_TIMEFRAMES[0]);
        assert!(ddl.contains("ts                      TIMESTAMP"));
        assert!(ddl.contains("security_id             INT"));
        assert!(ddl.contains("segment                 SYMBOL"));
        assert!(ddl.contains("ltp                     DOUBLE"));
        assert!(ddl.contains("volume                  LONG"));
        assert!(ddl.contains("expiry_date             DATE"));
        assert!(ddl.contains("received_at             TIMESTAMP"));
    }

    /// Phase 8 ratchet: SYMBOL capacity tuning matches plan
    /// (segment NOCACHE, underlying_symbol CAPACITY 1024 CACHE).
    #[test]
    fn test_movers_22tf_symbol_capacity_tuning() {
        let ddl = movers_22tf_create_ddl(&MOVERS_TIMEFRAMES[0]);
        assert!(
            ddl.contains("segment                 SYMBOL CAPACITY 16 NOCACHE"),
            "segment must be SYMBOL CAPACITY 16 NOCACHE: {ddl}"
        );
        assert!(
            ddl.contains("underlying_symbol       SYMBOL CAPACITY 1024 CACHE"),
            "underlying_symbol must be SYMBOL CAPACITY 1024 CACHE: {ddl}"
        );
        assert!(
            ddl.contains("instrument_type         SYMBOL CAPACITY 32 NOCACHE"),
            "instrument_type must be SYMBOL CAPACITY 32 NOCACHE"
        );
        assert!(
            ddl.contains("option_type             SYMBOL CAPACITY 4 NOCACHE"),
            "option_type must be SYMBOL CAPACITY 4 NOCACHE"
        );
    }
}
