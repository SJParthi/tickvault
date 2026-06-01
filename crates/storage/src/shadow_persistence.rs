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
//!
//! ## Schema (11 columns)
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS candles_1m (
//!     segment                  SYMBOL,
//!     security_id              LONG,
//!     ts                       TIMESTAMP,
//!     open                     DOUBLE,
//!     high                     DOUBLE,
//!     low                      DOUBLE,
//!     close                    DOUBLE,
//!     volume                   LONG,
//!     oi                       LONG,
//!     tick_count               LONG,
//!     close_pct_from_prev_day  DOUBLE
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, security_id, segment);
//! ```
//!
//! `close_pct_from_prev_day` (re-added 2026-05-28, PR-4b) is the seal-time
//! price % vs the previous-day close, sourced live from the tick `close`
//! column (`LiveCandleState::prev_day_close`). The legacy `oi_pct` /
//! `volume_pct` columns stay dropped — spot instruments have no OI and
//! indices have no volume, so neither is meaningful for this universe
//! (operator decision 2026-05-28).
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
//! Cross-refs:
//! - I-P1-11: `.claude/rules/project/security-id-uniqueness.md`
//! - Self-heal pattern: `crates/storage/src/instrument_persistence.rs::ensure_instrument_tables`

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_trading::candles::{TF_COUNT, TfIndex};

// ---------------------------------------------------------------------------
// QuestDB table names — one per timeframe.
//
// The 21 candle table names are derived from `TfIndex::table_name()`
// (the single source of truth in `crates/trading/src/candles/tf_index.rs`).
// Use `candle_table_names()` below to enumerate them.
// ---------------------------------------------------------------------------

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
        // Schema self-heal: candle tables created before the
        // security_id LONG fix have `security_id` / `tick_count` typed
        // INT. The ILP seal writer sends them via `column_i64` (LONG);
        // QuestDB rejects every row on the type mismatch, so the table
        // stays empty. QuestDB cannot ALTER a column's type — drop the
        // (broken, empty) table and let the corrected CREATE rebuild it.
        if candle_table_has_int_security_id(&client, &base_url, table).await {
            let drop_ddl = format!("DROP TABLE IF EXISTS {table};");
            run_drop_ddl(&client, &base_url, table, &drop_ddl).await;
            info!(
                table,
                "candle table dropped — security_id INT→LONG self-heal"
            );
        }
        let create_ddl = format!(
            "CREATE TABLE IF NOT EXISTS {table} (\
                segment                     SYMBOL, \
                security_id                 LONG, \
                ts                          TIMESTAMP, \
                open                        DOUBLE, \
                high                        DOUBLE, \
                low                         DOUBLE, \
                close                       DOUBLE, \
                volume                      LONG, \
                oi                          LONG, \
                tick_count                  LONG, \
                close_pct_from_prev_day     DOUBLE, \
                open_pct                    DOUBLE\
            ) timestamp(ts) PARTITION BY DAY \
            DEDUP UPSERT KEYS({DEDUP_KEY_CANDLES});"
        );
        run_ddl(&client, &base_url, table, &create_ddl).await;

        // Schema self-heal: candle tables created before the
        // close_pct_from_prev_day column existed (pre-2026-05-28 Engine-B
        // 10-col schema) auto-migrate. QuestDB ignores the ADD when the
        // column already exists, so running every boot is free (per
        // observability-architecture.md "Schema self-heal at boot").
        let alter_ddl =
            format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS close_pct_from_prev_day DOUBLE;");
        run_ddl(&client, &base_url, table, &alter_ddl).await;

        // §31 Option 2 (2026-06-01): self-heal the `open_pct` column for
        // tables created before it existed. Free on every boot.
        let alter_open_pct =
            format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS open_pct DOUBLE;");
        run_ddl(&client, &base_url, table, &alter_open_pct).await;
    }
}

/// Returns `true` if `table` already exists with `security_id` typed
/// `INT` — the pre-fix schema the i64 ILP seal writer cannot populate.
///
/// A missing table, a `LONG` column, or any query error returns
/// `false`: a missing table is handled by the `CREATE TABLE IF NOT
/// EXISTS` that follows, and a transient query error is best-effort —
/// the worst case is the operator keeps the old empty table until the
/// next boot.
async fn candle_table_has_int_security_id(client: &Client, base_url: &str, table: &str) -> bool {
    let query = format!("SELECT type FROM table_columns('{table}') WHERE column = 'security_id'");
    match client
        .get(base_url)
        .query(&[("query", query.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.text().await.unwrap_or_default();
            // QuestDB /exec returns the matching row in `dataset` as
            // `[["INT"]]` / `[["LONG"]]`.
            body.contains("[[\"INT\"]]")
        }
        _ => false,
    }
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

/// Legacy candle materialized views whose names do NOT collide with any
/// of the 21 Engine-B `candles_<tf>` tables — the retired sub-minute and
/// 7-day aggregations from the pre-#T1 (PR #517-era) candle cascade.
///
/// The 21 Engine-B names ARE swept for stale matviews separately (every
/// one is `DROP MATERIALIZED VIEW IF EXISTS`-ed before the `CREATE TABLE`
/// loop, in case an old deployment squats the name with a matview — this
/// is exactly the `candles_2m` / `candles_3m` / `candles_10m` 400 bug).
/// This list covers the names that have NO Engine-B counterpart and so
/// would otherwise leak forever.
const LEGACY_EXTRA_CANDLE_MATVIEW_NAMES: [&str; 5] = [
    "candles_5s",
    "candles_10s",
    "candles_15s",
    "candles_30s",
    "candles_7d",
];

/// QuestDB tables retired by the table-cleanup plan (#T3 instrument, #T4
/// misc) and the PR #3 greeks teardown. The persistence modules + boot
/// DDL were deleted in those PRs, but pre-existing QuestDB deployments
/// still physically hold the tables (a Docker volume survives the code
/// change). This idempotent `DROP TABLE IF EXISTS` sweep converges every
/// deployment to the 24-table KEEP set with zero manual migration.
///
/// Deliberately EXCLUDES every audit table — `order_audit` carries a
/// SEBI 5-year retention obligation and must never be auto-dropped; the
/// other audit tables were already retired with their own boot wiring in
/// #T2. This list is ONLY the instrument / misc / greeks tables.
const RETIRED_QUESTDB_TABLES: [&str; 12] = [
    // #T3 — instrument tables
    "fno_underlyings",
    "derivative_contracts",
    "subscribed_indices",
    "instrument_build_metadata",
    "index_constituents",
    // #T4 — misc tables
    "market_depth",
    "previous_close",
    "nse_holidays",
    // PR #3 — greeks tables
    "option_greeks",
    "pcr_snapshots",
    "dhan_option_chain_raw",
    "greeks_verification",
];

/// Idempotently drop every legacy QuestDB object so the live schema
/// converges to the 24-table KEEP set.
///
/// Drop order is load-bearing:
/// 1. Candle materialized views — every one of the 21 Engine-B
///    `candles_<tf>` names (a stale matview squatting the name makes
///    `CREATE TABLE` 400) plus the legacy sub-minute / 7-day matviews.
/// 2. The `candles_1s` base table.
/// 3. The 9 `candles_<tf>_shadow` tables.
/// 4. The `aggregator_seal_audit` table (#T2a table cleanup).
/// 5. The instrument / misc / greeks tables retired by #T3, #T4 and the
///    PR #3 greeks teardown (`RETIRED_QUESTDB_TABLES`).
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
///
/// ## PR #798 — one-shot marker file gate (2026-05-25)
///
/// The 45+ DROP statements below are an **upgrade safety net** — they
/// protect against pre-#T1 deployments where stale materialized views
/// occupy the `candles_<tf>` names and cause `CREATE TABLE` to 400.
/// Once a deployment has gone through this cleanup ONCE, repeating the
/// sweep on every subsequent boot is pure waste (~5s of log spam +
/// 45 HTTP round-trips against QuestDB).
///
/// Operator demand 2026-05-25 19:00 IST: "remove all the stale unused
/// unwanted not used everything". The DROPs themselves can't be deleted
/// (they're load-bearing for fresh deployments), but we can gate them
/// behind a one-shot marker file that records "cleanup already done"
/// on the local disk. After the first successful run the marker exists
/// and all subsequent boots skip the loop entirely.
///
/// Marker path: `data/state/legacy_candle_objects_dropped.marker`.
/// To re-run the cleanup (e.g. after restoring from a stale backup),
/// `rm` the marker file.
// WIRING-EXEMPT: boot wiring lives in crates/app/src/main.rs before ensure_shadow_candle_tables; integration-covered by CI boot + `make doctor`.
pub async fn drop_legacy_candle_objects(questdb_config: &QuestDbConfig) {
    // PR #798 marker-file gate — skip the entire cleanup loop on
    // deployments where it has already run successfully.
    let marker_path = std::path::Path::new(LEGACY_DROP_MARKER_PATH);
    if marker_path.exists() {
        tracing::debug!(
            marker = LEGACY_DROP_MARKER_PATH,
            "legacy candle cleanup already done on this deployment — skipping (PR #798)"
        );
        return;
    }

    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    // 1. Drop legacy candle MATERIALIZED VIEWS FIRST — before the base
    //    table they cascade from, AND before `ensure_shadow_candle_tables`
    //    runs its `CREATE TABLE` loop.
    //
    //    Every one of the 21 Engine-B `candles_<tf>` names is swept: the
    //    pre-#T1 architecture created matviews under those exact names,
    //    and a matview occupying the name makes `CREATE TABLE IF NOT
    //    EXISTS candles_<tf>` return `400 Bad Request` (the
    //    `candles_2m` / `candles_3m` / `candles_10m` bug). `DROP
    //    MATERIALIZED VIEW IF EXISTS` against a name that is already a
    //    plain table — or absent — is a safe 2xx no-op, so sweeping all
    //    21 every boot is idempotent.
    for view in candle_table_names() {
        let ddl = format!("DROP MATERIALIZED VIEW IF EXISTS {view};");
        run_drop_ddl(&client, &base_url, view, &ddl).await;
    }
    //    Plus the legacy sub-minute / 7-day matviews that have no
    //    Engine-B counterpart and would otherwise leak forever.
    for view in LEGACY_EXTRA_CANDLE_MATVIEW_NAMES {
        let ddl = format!("DROP MATERIALIZED VIEW IF EXISTS {view};");
        run_drop_ddl(&client, &base_url, view, &ddl).await;
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

    // 4. Drop the retired `aggregator_seal_audit` forensic table (#T2a —
    //    QuestDB table cleanup). The per-seal audit module is deleted; the
    //    table itself is dropped here so existing deployments converge.
    run_drop_ddl(
        &client,
        &base_url,
        "aggregator_seal_audit",
        "DROP TABLE IF EXISTS aggregator_seal_audit;",
    )
    .await;

    // 5. Drop the instrument / misc / greeks tables retired by the
    //    table-cleanup plan (#T3, #T4) and the PR #3 greeks teardown.
    //    The persistence code is gone; this sweep removes the physical
    //    tables on pre-existing deployments so the live QuestDB schema
    //    converges to the 24-table KEEP set with no manual migration.
    for table in RETIRED_QUESTDB_TABLES {
        let ddl = format!("DROP TABLE IF EXISTS {table};");
        run_drop_ddl(&client, &base_url, table, &ddl).await;
    }

    // PR #798 (operator-locked 2026-05-25) — write one-shot marker so
    // subsequent boots skip the entire cleanup loop. Best-effort: if
    // the marker can't be written (disk full, perms), the cleanup just
    // repeats on next boot — same as before this PR. No data corruption
    // risk because every DROP is `IF EXISTS`.
    if let Some(parent) = marker_path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            tracing::warn!(
                ?err,
                path = %parent.display(),
                "PR #798: failed to create data/state/ dir for legacy-drop marker"
            );
            return;
        }
    }
    if let Err(err) = std::fs::write(
        marker_path,
        "PR #798 marker — legacy candle objects cleaned up on this deployment.\n",
    ) {
        tracing::warn!(
            ?err,
            path = LEGACY_DROP_MARKER_PATH,
            "PR #798: failed to write legacy-drop marker (cleanup will repeat next boot)"
        );
    } else {
        tracing::info!(
            path = LEGACY_DROP_MARKER_PATH,
            "PR #798: legacy candle cleanup complete — marker written"
        );
    }
}

/// PR #798 — one-shot marker file path. After the legacy candle DROP
/// sweep runs successfully on a deployment, this file records the fact
/// so subsequent boots skip the entire loop. To force a re-run (e.g.
/// after restoring from an older QuestDB backup that still has the
/// legacy matviews), delete the file.
const LEGACY_DROP_MARKER_PATH: &str = "data/state/legacy_candle_objects_dropped.marker";

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
    fn test_legacy_extra_matview_names_have_no_engine_b_counterpart() {
        // The 5 extra legacy matviews (sub-minute + 7d) must NOT name-
        // collide with any of the 21 Engine-B tables — those 21 are swept
        // separately. A collision here would mean the name is dropped
        // twice (harmless) but signals a list-maintenance mistake.
        let engine_b = candle_table_names();
        for view in LEGACY_EXTRA_CANDLE_MATVIEW_NAMES {
            assert!(
                !engine_b.contains(&view),
                "legacy extra matview {view} must not be an Engine B table name"
            );
        }
        let mut seen = std::collections::HashSet::new();
        for view in LEGACY_EXTRA_CANDLE_MATVIEW_NAMES {
            assert!(seen.insert(view), "duplicate legacy extra matview: {view}");
        }
    }

    #[test]
    fn test_retired_questdb_tables_count_is_twelve_and_unique() {
        assert_eq!(RETIRED_QUESTDB_TABLES.len(), 12);
        let mut seen = std::collections::HashSet::new();
        for table in RETIRED_QUESTDB_TABLES {
            assert!(seen.insert(table), "duplicate retired table: {table}");
        }
    }

    #[test]
    fn test_retired_questdb_tables_excludes_keep_set_and_audit() {
        // The retired-table sweep must NEVER touch a KEEP-24 table or any
        // audit table — `order_audit` carries a SEBI 5-year retention
        // obligation and auto-dropping it would be a compliance breach.
        let engine_b = candle_table_names();
        for table in RETIRED_QUESTDB_TABLES {
            assert!(
                !engine_b.contains(&table),
                "retired table {table} must not be a KEEP-24 candle table"
            );
            assert!(
                !matches!(
                    table,
                    "ticks" | "historical_candles" | "option_chain_minute_snapshot"
                ),
                "retired table {table} must not be a KEEP-24 table"
            );
            assert!(
                !table.ends_with("_audit") && table != "order_audit",
                "retired-table sweep must not drop audit tables (SEBI retention): {table}"
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

    /// PR #798 (operator-locked 2026-05-25) — marker constant is set
    /// to the canonical relative path under `data/state/`. Future
    /// Claude sessions that need to add a similar marker should reuse
    /// the same directory.
    #[test]
    fn test_legacy_drop_marker_path_is_in_data_state() {
        assert_eq!(
            LEGACY_DROP_MARKER_PATH,
            "data/state/legacy_candle_objects_dropped.marker"
        );
    }

    /// PR #798 — marker path lives under `data/state/`, the same
    /// directory other one-shot markers use (`historical_fetch_done.json`,
    /// `yesterdays_1d_pre_market_done.json`).
    #[test]
    fn test_legacy_drop_marker_path_starts_with_data_state() {
        assert!(LEGACY_DROP_MARKER_PATH.starts_with("data/state/"));
    }

    /// PR #798 — marker filename uses a stable, descriptive name so
    /// operators reading the directory can identify it without
    /// cross-referencing the code.
    #[test]
    fn test_legacy_drop_marker_filename_is_descriptive() {
        assert!(LEGACY_DROP_MARKER_PATH.ends_with("legacy_candle_objects_dropped.marker"));
    }
}
