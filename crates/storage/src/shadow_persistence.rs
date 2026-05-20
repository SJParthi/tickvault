//! Candle-engine re-architecture #T1a — plain QuestDB candle tables.
//!
//! Direct-flush target tables for the live multi-timeframe aggregator.
//! Sealed candles flow `aggregator → ring → ILP → candles_<tf>` WITHOUT
//! cascading through the legacy `candles_1s` materialized-view chain and
//! WITHOUT the interim `_shadow` tables of the Wave 6 design.
//!
//! - 21 timeframes (1m / 2m / … / 15m / 30m / 1h / 2h / 3h / 4h / 1d)
//!   each get their OWN plain QuestDB table — `candles_<tf>`. Names are
//!   derived from `TfIndex::table_name()` (the single source of truth).
//! - Every candle table carries DEDUP UPSERT KEYS
//!   `(ts, security_id, segment)` — composite per
//!   `.claude/rules/project/security-id-uniqueness.md` (I-P1-11), since
//!   Dhan reuses `security_id` across segments. The designated
//!   timestamp `ts` is part of the key (QuestDB requires it).
//! - The 22nd constant `DEDUP_KEY_AGGREGATOR_SEAL_AUDIT` covers the
//!   per-seal forensic audit table.
//!
//! ## Schema (10 columns, NO pct columns)
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS candles_1m (
//!     segment       SYMBOL,
//!     security_id   INT,
//!     ts            TIMESTAMP,
//!     open          DOUBLE,
//!     high          DOUBLE,
//!     low           DOUBLE,
//!     close         DOUBLE,
//!     volume        LONG,
//!     oi            LONG,
//!     tick_count    INT
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, security_id, segment);
//! ```
//!
//! The 3 `*_pct_from_prev_day` DOUBLE columns of the legacy shadow
//! schema are intentionally dropped — the live aggregator no longer
//! stamps prev-day pct values at seal time.
//!
//! ## DEDUP rationale for each candle table
//!
//! `(ts, security_id, segment)` is the composite key. `ts` is the
//! candle open timestamp (IST nanos derived from the WS LTT field per
//! `data-integrity.md` — NEVER `Utc::now()`). `(security_id, segment)`
//! is the I-P1-11 composite identity. Three properties hold together:
//!
//! 1. **Idempotent re-flush** — a sealed candle re-emitted on aggregator
//!    crash recovery collapses into the same row.
//! 2. **Cross-segment safety** — NIFTY (`security_id=13`, `IDX_I`) and a
//!    distinct NSE_EQ instrument with the same numeric id stay as two
//!    rows, not one.
//! 3. **Per-instrument-per-bucket uniqueness** — one (instrument, bucket)
//!    pair → exactly one row, regardless of how many ticks contributed.
//!
//! ## Aggregator seal audit table
//!
//! `aggregator_seal_audit` — DEDUP UPSERT KEYS `(ts, security_id,
//! exchange_segment, timeframe, candle_ts, trading_date_ist)`. One audit
//! row per (day, instrument, TF, bucket).
//!
//! Cross-refs:
//! - I-P1-11: `.claude/rules/project/security-id-uniqueness.md`
//! - Self-heal pattern: `crates/storage/src/instrument_persistence.rs::ensure_instrument_tables`

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_trading::candles::{TF_COUNT, TfIndex};

// ---------------------------------------------------------------------------
// QuestDB table names — one per timeframe, plus the seal audit table.
//
// The 21 candle table names are derived from `TfIndex::table_name()`
// (the single source of truth in `crates/trading/src/candles/tf_index.rs`).
// Use `candle_table_names()` below to enumerate them.
// ---------------------------------------------------------------------------

/// Per-seal forensic audit table.
pub const QUESTDB_TABLE_AGGREGATOR_SEAL_AUDIT: &str = "aggregator_seal_audit";

// ---------------------------------------------------------------------------
// DEDUP UPSERT keys — composite per I-P1-11.
//
// All 21 candle tables share the same key shape because they are
// uniformly partitioned `(ts, security_id, segment)`. The
// `dedup_segment_meta_guard.rs` workspace meta-guard scans every
// `DEDUP_KEY_*` constant in `crates/storage/src/` and FAILS the build
// if any key that mentions `security_id` does NOT also mention
// `segment` / `exchange_segment` / `exchange`. The constant below
// contains the literal substring `segment` to satisfy that invariant.
// ---------------------------------------------------------------------------

/// DEDUP UPSERT key for every candle table. Includes the designated
/// timestamp `ts` (QuestDB requires it) and `segment` per I-P1-11. The
/// same value is returned by `TfIndex::dedup_key()`.
pub const DEDUP_KEY_CANDLES: &str = "ts, security_id, segment";

/// Aggregator seal audit DEDUP key — one row per (day, instrument, TF,
/// bucket). Includes `exchange_segment` for I-P1-11 and `timeframe` so
/// the same instrument's 1m + 5m seals coexist.
pub const DEDUP_KEY_AGGREGATOR_SEAL_AUDIT: &str =
    "security_id, exchange_segment, timeframe, candle_ts, trading_date_ist";

// ---------------------------------------------------------------------------
// Public helpers — aggregate the table names for downstream consumers.
// ---------------------------------------------------------------------------

/// All 21 candle table names in canonical order (1m → 1d), derived from
/// `TfIndex::table_name()`.
///
/// Used by:
/// - the DDL setup loop below.
/// - the hot-path seal-writer chain to index into the `[Sender; 21]`
///   ILP sender array.
#[must_use]
// TEST-EXEMPT: pure const-array accessor; tested via test_candle_table_names_are_plain_and_canonical + test_all_candle_table_names_distinct.
pub fn candle_table_names() -> [&'static str; TF_COUNT] {
    let mut names = [""; TF_COUNT];
    let mut idx = 0;
    while idx < TF_COUNT {
        // `TfIndex::from_ordinal` is const and total over `0..TF_COUNT`.
        if let Some(tf) = TfIndex::from_ordinal(idx) {
            names[idx] = tf.table_name();
        }
        idx += 1;
    }
    names
}

// ---------------------------------------------------------------------------
// DDL setup — idempotent CREATE + DEDUP UPSERT.
//
// Follows the schema-self-heal pattern documented in
// `.claude/rules/project/observability-architecture.md` ("Schema
// self-heal at boot"). Future column additions ride
// `ALTER TABLE ADD COLUMN IF NOT EXISTS` so older deployments
// auto-migrate.
// ---------------------------------------------------------------------------

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Create all 21 plain candle tables + the per-seal audit table if they
/// do not already exist, with DEDUP UPSERT enabled on each.
///
/// Idempotent: safe to call on every boot. Failures are logged at
/// `error!` level (Telegram-routable per `error_level_meta_guard.rs`
/// Rule 5) but do NOT block boot — the writer falls back to ring/spill
/// on subsequent ILP errors.
///
/// Requires a running QuestDB; covered by boot integration in CI and
/// by manual `make doctor` post-boot.
// TEST-EXEMPT: requires a running QuestDB; tested via boot integration in CI and by `make doctor`. WIRING-EXEMPT: boot wiring lives in crates/app/src/main.rs alongside ensure_instrument_tables.
pub async fn ensure_shadow_candle_tables(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    for table in candle_table_names() {
        let create_ddl = format!(
            "CREATE TABLE IF NOT EXISTS {table} (\
                segment                     SYMBOL, \
                security_id                 INT, \
                ts                          TIMESTAMP, \
                open                        DOUBLE, \
                high                        DOUBLE, \
                low                         DOUBLE, \
                close                       DOUBLE, \
                volume                      LONG, \
                oi                          LONG, \
                tick_count                  INT\
            ) timestamp(ts) PARTITION BY DAY \
            DEDUP UPSERT KEYS({DEDUP_KEY_CANDLES});"
        );
        run_ddl(&client, &base_url, table, &create_ddl).await;
    }

    let audit_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_AGGREGATOR_SEAL_AUDIT} (\
            ts                  TIMESTAMP, \
            trading_date_ist    DATE, \
            security_id         INT, \
            exchange_segment    SYMBOL, \
            timeframe           SYMBOL, \
            candle_ts           TIMESTAMP, \
            seals_emitted       LONG, \
            seals_dropped       LONG, \
            late_ticks_discarded LONG, \
            outcome             SYMBOL\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS(ts, {DEDUP_KEY_AGGREGATOR_SEAL_AUDIT});"
    );
    run_ddl(
        &client,
        &base_url,
        QUESTDB_TABLE_AGGREGATOR_SEAL_AUDIT,
        &audit_ddl,
    )
    .await;
}

// ---------------------------------------------------------------------------
// #T1b — drop legacy candle objects (Engine A + Engine C teardown).
//
// The candle-engine re-architecture leaves ONLY Engine B (the 21-TF
// in-memory aggregator flushing to plain `candles_<tf>` tables). Engine A
// (`candles_1s` base table) and Engine C (the 9 `candles_<tf>` materialized
// views + the 9 legacy `candles_<tf>_shadow` tables) are deleted.
//
// CRITICAL ORDERING: the 9 matviews are NAMED `candles_1m` … `candles_1d`.
// Engine B's plain tables want those exact names. A matview named
// `candles_1m` occupying that name makes `CREATE TABLE IF NOT EXISTS
// candles_1m` a silent no-op. Therefore this MUST run at boot BEFORE
// `ensure_shadow_candle_tables`. Matviews are dropped before their base
// table (`candles_1s`) because a matview depends on its base.
// ---------------------------------------------------------------------------

/// The 9 legacy candle timeframe suffixes shared by the retired Engine-C
/// materialized views (`candles_<sfx>`) and the retired Wave-6
/// `candles_<sfx>_shadow` tables. These names are decoupled from
/// `TfIndex::table_name()` on purpose — they are *legacy* objects that no
/// longer correspond to any live timeframe enum.
const LEGACY_CANDLE_TF_SUFFIXES: [&str; 9] =
    ["1m", "5m", "15m", "30m", "1h", "2h", "3h", "4h", "1d"];

/// Idempotently drop every legacy candle object: the 9 Engine-C
/// materialized views, the `candles_1s` base table, and the 9 legacy
/// `candles_<tf>_shadow` tables.
///
/// Drop order is load-bearing:
/// 1. The 9 materialized views (`candles_1m` … `candles_1d`) — a matview
///    must be dropped before its base table.
/// 2. The `candles_1s` base table.
/// 3. The 9 `candles_<tf>_shadow` tables.
///
/// Each statement is `DROP … IF EXISTS`, so this is safe to call on every
/// boot regardless of which objects actually exist. Failures are logged at
/// `error!` level (Telegram-routable per `error_level_meta_guard.rs`
/// Rule 5) but do NOT block boot.
///
/// MUST be awaited at boot BEFORE `ensure_shadow_candle_tables` so that the
/// `candles_1m` … `candles_1d` names are free for Engine B's plain tables.
///
/// Requires a running QuestDB; boot wiring lives in `crates/app/src/main.rs`
/// immediately before the `ensure_shadow_candle_tables` call.
// WIRING-EXEMPT: boot wiring lives in crates/app/src/main.rs before ensure_shadow_candle_tables; integration-covered by CI boot + `make doctor`.
pub async fn drop_legacy_candle_objects(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    // 1. Drop the 9 Engine-C materialized views FIRST — before the base
    //    table they cascade from.
    for sfx in LEGACY_CANDLE_TF_SUFFIXES {
        let view = format!("candles_{sfx}");
        let ddl = format!("DROP MATERIALIZED VIEW IF EXISTS {view};");
        run_drop_ddl(&client, &base_url, &view, &ddl).await;
    }

    // 2. Drop the Engine-A `candles_1s` base table.
    run_drop_ddl(
        &client,
        &base_url,
        "candles_1s",
        "DROP TABLE IF EXISTS candles_1s;",
    )
    .await;

    // 3. Drop the 9 legacy Wave-6 `candles_<tf>_shadow` tables.
    for sfx in LEGACY_CANDLE_TF_SUFFIXES {
        let table = format!("candles_{sfx}_shadow");
        let ddl = format!("DROP TABLE IF EXISTS {table};");
        run_drop_ddl(&client, &base_url, &table, &ddl).await;
    }
}

/// Issue one DROP DDL statement to QuestDB's `/exec` endpoint.
///
/// Same GET-based transport as [`run_ddl`]. A non-2xx response is logged
/// at `warn!` (a missing object on a fresh deploy is benign because the
/// statement is `IF EXISTS`); a transport error is logged at `error!`.
async fn run_drop_ddl(client: &Client, base_url: &str, object: &str, ddl: &str) {
    match client.get(base_url).query(&[("query", ddl)]).send().await {
        Ok(resp) if resp.status().is_success() => {
            info!(object, "legacy candle object dropped (or already absent)");
        }
        Ok(resp) => {
            warn!(object, status = %resp.status(), "legacy candle DROP non-2xx");
        }
        Err(err) => {
            error!(object, ?err, "legacy candle DROP request failed");
        }
    }
}

/// Issue one DDL statement to QuestDB's `/exec` endpoint.
///
/// QuestDB's `/exec` endpoint is **GET-only** — it returns
/// `405 Method Not Allowed` for POST. The `query` parameter is
/// URL-encoded by reqwest's query-string builder. (The #T1a precursor
/// briefly switched this to POST to keep DDL out of access logs — that
/// regressed every candle-table DDL to 405; reverted here.)
async fn run_ddl(client: &Client, base_url: &str, table: &str, ddl: &str) {
    match client.get(base_url).query(&[("query", ddl)]).send().await {
        Ok(resp) if resp.status().is_success() => {
            info!(table, "candle table ready");
        }
        Ok(resp) => {
            warn!(table, status = %resp.status(), "DDL non-2xx");
        }
        Err(err) => {
            error!(table, ?err, "DDL request failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_candle_table_names_has_twenty_one_entries() {
        assert_eq!(candle_table_names().len(), TF_COUNT);
        assert_eq!(TF_COUNT, 21);
    }

    #[test]
    fn test_all_candle_table_names_distinct() {
        let names = candle_table_names();
        let mut seen = std::collections::HashSet::new();
        for name in names {
            assert!(seen.insert(name), "duplicate table name: {name}");
        }
    }

    #[test]
    fn test_candle_table_names_are_plain_and_canonical() {
        // Plain first-class tables — no `_shadow` suffix anywhere.
        let names = candle_table_names();
        for name in names {
            assert!(
                name.starts_with("candles_"),
                "table name {name:?} prefix wrong"
            );
            assert!(
                !name.contains("shadow"),
                "table name {name:?} must be a plain table (no _shadow)"
            );
        }
    }

    #[test]
    fn test_candle_table_names_match_tf_index_table_name() {
        // The DDL loop derives names from `TfIndex::table_name()`; this
        // pins that alignment so the writer's `[Sender; 21]` indexing
        // stays consistent with the DDL it created.
        let names = candle_table_names();
        for (idx, tf) in TfIndex::ALL.iter().enumerate() {
            assert_eq!(
                names[idx],
                tf.table_name(),
                "candle_table_names()[{idx}] diverges from TfIndex::table_name()"
            );
        }
    }

    #[test]
    fn test_dedup_key_candles_includes_ts_security_id_segment() {
        // QuestDB rejects a DEDUP key omitting the designated timestamp;
        // I-P1-11 requires `segment` alongside `security_id`.
        assert!(DEDUP_KEY_CANDLES.contains("ts"));
        assert!(DEDUP_KEY_CANDLES.contains("security_id"));
        assert!(DEDUP_KEY_CANDLES.contains("segment"));
    }

    #[test]
    fn test_dedup_key_candles_matches_tf_index_dedup_key() {
        // The shared DEDUP key constant must equal `TfIndex::dedup_key()`
        // so the DDL and the writer agree.
        for tf in TfIndex::ALL {
            assert_eq!(DEDUP_KEY_CANDLES, tf.dedup_key());
        }
    }

    #[test]
    fn test_aggregator_seal_audit_dedup_includes_segment_and_tf() {
        assert!(DEDUP_KEY_AGGREGATOR_SEAL_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_AGGREGATOR_SEAL_AUDIT.contains("exchange_segment"));
        assert!(DEDUP_KEY_AGGREGATOR_SEAL_AUDIT.contains("timeframe"));
        assert!(DEDUP_KEY_AGGREGATOR_SEAL_AUDIT.contains("candle_ts"));
        assert!(DEDUP_KEY_AGGREGATOR_SEAL_AUDIT.contains("trading_date_ist"));
    }

    #[test]
    fn test_legacy_candle_tf_suffixes_has_nine_entries() {
        // The 9 legacy Engine-C timeframes (1m..1d) — distinct from the
        // 21 live TFs of Engine B.
        assert_eq!(LEGACY_CANDLE_TF_SUFFIXES.len(), 9);
        let mut seen = std::collections::HashSet::new();
        for sfx in LEGACY_CANDLE_TF_SUFFIXES {
            assert!(seen.insert(sfx), "duplicate legacy suffix: {sfx}");
        }
    }

    #[test]
    fn test_legacy_candle_tf_suffixes_are_canonical() {
        assert_eq!(
            LEGACY_CANDLE_TF_SUFFIXES,
            ["1m", "5m", "15m", "30m", "1h", "2h", "3h", "4h", "1d"]
        );
    }

    #[test]
    fn test_legacy_drop_targets_do_not_collide_with_engine_b_tables() {
        // Engine B's plain `candles_1s_shadow`-free tables must NOT be in
        // the legacy shadow-drop set — only the 9 retired `_shadow`
        // tables. The 9 matview names (`candles_1m` etc.) DO collide with
        // Engine B plain tables by design — that collision is exactly why
        // the matviews must be dropped first.
        let engine_b = candle_table_names();
        for sfx in LEGACY_CANDLE_TF_SUFFIXES {
            let shadow = format!("candles_{sfx}_shadow");
            assert!(
                !engine_b.contains(&shadow.as_str()),
                "legacy shadow table {shadow} must not be an Engine B table"
            );
        }
    }

    #[test]
    fn test_candle_table_names_canonical_ordering_1m_to_1d() {
        let names = candle_table_names();
        let expected = [
            "candles_1m",
            "candles_2m",
            "candles_3m",
            "candles_4m",
            "candles_5m",
            "candles_6m",
            "candles_7m",
            "candles_8m",
            "candles_9m",
            "candles_10m",
            "candles_11m",
            "candles_12m",
            "candles_13m",
            "candles_14m",
            "candles_15m",
            "candles_30m",
            "candles_1h",
            "candles_2h",
            "candles_3h",
            "candles_4h",
            "candles_1d",
        ];
        assert_eq!(names, expected);
    }
}
