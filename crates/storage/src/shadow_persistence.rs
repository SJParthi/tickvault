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
//!   `(ts, security_id, segment, feed)` — composite per
//!   `.claude/rules/project/security-id-uniqueness.md` (I-P1-11), since
//!   Dhan reuses `security_id` across segments. The designated
//!   timestamp `ts` is part of the key (QuestDB requires it). `feed`
//!   (operator 2026-06-19, "same tables + feed column") keeps Dhan and
//!   Groww candles for the same minute/instrument distinct, never merged.
//!
//! ## Schema (15 columns)
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS candles_1m (
//!     feed                     SYMBOL,
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
//!     close_pct_from_prev_day  DOUBLE,
//!     open_pct                 DOUBLE,
//!     change_pct               DOUBLE,
//!     open_gap_pct             DOUBLE
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, security_id, segment, feed);
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

use crate::shadow_candle_writer::CANDLE_FEED_DHAN;

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
/// timestamp `ts` (QuestDB requires it), `segment` per I-P1-11, and `feed`
/// (operator 2026-06-19, "same tables + feed column") so a Dhan candle and a
/// Groww candle for the same `(ts, security_id, segment)` are BOTH kept —
/// distinct feeds, never collide. `feed` is replay-stable (constant per writer
/// — the Dhan candle writer stamps `CANDLE_FEED_DHAN='dhan'`), preserving
/// same-feed minute-bucket idempotency. The same value is returned by
/// `TfIndex::dedup_key()`.
///
/// **Brownfield honesty (no hallucination):** rows persisted under the OLD
/// 3-column key carry `feed=NULL`. A new `feed='dhan'` row for the same
/// `(ts, security_id, segment)` would be a DISTINCT key (NULL != 'dhan') and
/// therefore a DUPLICATE, not an upsert. To eliminate that overlap window
/// entirely, [`ensure_shadow_candle_tables`] backfills `feed='dhan'` on every
/// pre-existing NULL-feed row (`UPDATE ... WHERE feed IS NULL`) BEFORE
/// re-enabling DEDUP — so post-migration same-minute re-seals upsert cleanly.
pub const DEDUP_KEY_CANDLES: &str = "ts, security_id, segment, feed";

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
    // C2 (2026-07-03): panic-free client build — Client::new() panics on
    // TLS/resolver/fd init failure (silent tokio-task death). Degrade:
    // skip the DDL this boot; the next boot re-runs it (idempotent), and
    // the seal writer falls back to ring/spill on ILP errors as documented.
    let client = match Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            error!(
                error = %err,
                code = tickvault_common::error_code::ErrorCode::HttpClient01BuildFailed.code_str(),
                "HTTP-CLIENT-01 reqwest client build failed — candle-table DDL skipped: \
                 if a table does not exist yet, ILP auto-create will lack DEDUP keys until \
                 the next successful boot (duplicate-row window)"
            );
            metrics::counter!(
                "tv_http_client_build_failed_total",
                "site" => "shadow_ensure_tables"
            )
            .increment(1);
            return;
        }
    };

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
                feed                        SYMBOL, \
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
                open_pct                    DOUBLE, \
                change_pct                  DOUBLE, \
                open_gap_pct                DOUBLE\
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

        // Operator request 2026-06-02: self-heal the `change_pct` +
        // `open_gap_pct` columns for tables created before they existed.
        // Free on every boot (QuestDB ignores ADD when the column exists).
        let alter_change_pct =
            format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS change_pct DOUBLE;");
        run_ddl(&client, &base_url, table, &alter_change_pct).await;
        let alter_open_gap_pct =
            format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS open_gap_pct DOUBLE;");
        run_ddl(&client, &base_url, table, &alter_open_gap_pct).await;
        // Feed-provenance label (operator 2026-06-19, "same tables + feed
        // column"): broker source (`'dhan'`/`'groww'`). It IS part of the DEDUP
        // key now (`DEDUP_KEY_CANDLES` includes `feed`), so a Dhan candle and a
        // Groww candle for the same minute/instrument are BOTH kept. MUST run
        // BEFORE the DEDUP-ENABLE migration below so the key column exists on
        // pre-existing tables. Additive + idempotent.
        let alter_feed = format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS feed SYMBOL;");
        run_ddl(&client, &base_url, table, &alter_feed).await;
        // Brownfield NULL-feed backfill (worst-case coverage, no-hallucination):
        // rows persisted under the OLD 3-col key have `feed=NULL`. Without this,
        // a new `feed='dhan'` row for the same `(ts, security_id, segment)` is a
        // DISTINCT key (NULL != 'dhan') → a DUPLICATE, not an upsert. Stamping
        // `feed='dhan'` on every legacy NULL row BEFORE re-enabling DEDUP closes
        // that overlap window. Idempotent + cheap on every subsequent boot:
        // `WHERE feed IS NULL` matches nothing once backfilled. MUST run BEFORE
        // the DEDUP-ENABLE below (UPDATE on the live key column is cleanest
        // before the key is re-applied).
        let backfill_feed =
            format!("UPDATE {table} SET feed = '{CANDLE_FEED_DHAN}' WHERE feed IS NULL;");
        run_ddl(&client, &base_url, table, &backfill_feed).await;
        // Brownfield DEDUP migration: re-enable the UPSERT key with `feed`
        // included so EXISTING candle tables (created before the feed-in-key
        // change) get the new 4-col key. Idempotent — re-enabling the same key
        // is a no-op; greenfield tables already have it from the CREATE DDL.
        let dedup_enable =
            format!("ALTER TABLE {table} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_CANDLES});");
        run_ddl(&client, &base_url, table, &dedup_enable).await;
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
///
/// NOTE: any future `candles_*` prefix sweep MUST exclude `*_named` views
/// (`console_views::VIEW_TICKS_NAMED` / `VIEW_CANDLES_NAMED`).
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
const RETIRED_QUESTDB_TABLES: [&str; 18] = [
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
    // dead REST-era feed tables (drop-sweep 2026-07-17)
    "deep_market_depth",
    // Track A drop extension (2026-07-18, operator-approved cleanup wave):
    // - `historical_candles`: no live writer since the 2026-05-26 Dhan
    //   historical chain deletion (PR-E); remaining refs are history-only.
    // - `stock_movers` / `option_movers`: movers runtime deleted 2026-05-19
    //   (AWS-lifecycle PRs #2-#4); the per-timeframe movers grid (matview OR
    //   table era) is swept by `retired_movers_object_names()` below.
    // - the two movers one-shot migration MARKER tables.
    "historical_candles",
    "stock_movers",
    "option_movers",
    "movers_migration_2026_05_01",
    "movers_legacy_retire_2026_05_03",
];

/// Every per-timeframe `movers_*` object name any deployment era could
/// still physically hold (enumerated from the deleted
/// `movers_base_persistence.rs` history, commit `7f259d79^`):
///
/// - the 25-matview era: `movers_{5s,10s,15s,30s,1m..15m,30m,1h,2h,3h,4h,1d}`
/// - the legacy 24 `movers_unified_*` matviews:
///   `movers_unified_{5s,15s,30s,1m..15m,30m,1h,2h,3h,4h,1d}`
/// - the legacy 25 plain TABLES:
///   `movers_{1s,2s,3s,5s,10s,15s,20s,30s,1m..15m,30m,1h}`
///
/// Because the SAME name was a matview in one era and a plain table in
/// another, the sweep issues BOTH `DROP MATERIALIZED VIEW IF EXISTS` and
/// `DROP TABLE IF EXISTS` for every name in the union (53 names). Both
/// forms are idempotent no-ops when the name is absent or held by the
/// other object kind (a non-2xx is logged `warn!` and never blocks boot).
fn retired_movers_object_names() -> Vec<String> {
    let sub_minute = ["1s", "2s", "3s", "5s", "10s", "15s", "20s", "30s"];
    let super_minute = ["30m", "1h", "2h", "3h", "4h", "1d"];
    let mut names = Vec::with_capacity(53);
    for sfx in sub_minute {
        names.push(format!("movers_{sfx}"));
    }
    for m in 1..=15u8 {
        names.push(format!("movers_{m}m"));
    }
    for sfx in super_minute {
        names.push(format!("movers_{sfx}"));
    }
    for sfx in ["5s", "15s", "30s"] {
        names.push(format!("movers_unified_{sfx}"));
    }
    for m in 1..=15u8 {
        names.push(format!("movers_unified_{m}m"));
    }
    for sfx in super_minute {
        names.push(format!("movers_unified_{sfx}"));
    }
    names
}

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
/// 6. The dead per-timeframe movers grid (`retired_movers_object_names`)
///    — matview drop THEN table drop per name (era-dependent object kind).
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
// WIRING-EXEMPT: boot wiring lives in crates/app/src/candle_ddl_boot.rs (awaited from `build_shared_infra` before the seal-writer spawn — the Track A 2026-07-18 re-wire; pinned by crates/app/tests/ensure_ddl_boot_wiring_guard.rs); integration-covered by CI boot + `make doctor`.
pub async fn drop_legacy_candle_objects(questdb_config: &QuestDbConfig) {
    // PR #798 marker-file gate, VERSIONED (Track A, 2026-07-18) — skip the
    // cleanup loop only when the marker records the CURRENT sweep version.
    // A pre-version marker (the original PR #798 free-text content) or a
    // lower version means the drop list was extended since that deployment
    // last swept — re-run exactly once and rewrite the marker at the new
    // version. Deleting the file still forces a manual re-run.
    let marker_path = std::path::Path::new(LEGACY_DROP_MARKER_PATH);
    if marker_path.exists() {
        // The exists() check is the fast-path short-circuit the PR #798
        // ratchet (crates/common/tests/legacy_drop_marker_gate_guard.rs)
        // pins; the version compare inside decides whether the existing
        // marker still covers the CURRENT drop list.
        if let Ok(content) = std::fs::read_to_string(marker_path)
            && marker_records_current_version(&content)
        {
            tracing::debug!(
                marker = LEGACY_DROP_MARKER_PATH,
                version = LEGACY_DROP_SWEEP_VERSION,
                "legacy candle cleanup already done at the current sweep version — skipping (PR #798)"
            );
            return;
        }
        // Marker exists but records an OLDER version (or is unreadable /
        // garbage): the drop list was extended since that deployment last
        // swept — fall through, re-run exactly once, and rewrite the
        // marker at the current version below.
    }

    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    // C2 (2026-07-03): panic-free client build — Client::new() panics on
    // TLS/resolver/fd init failure (silent tokio-task death). Degrade:
    // skip the legacy cleanup this boot (marker not written, so the next
    // boot retries the sweep).
    let client = match Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            error!(
                error = %err,
                code = tickvault_common::error_code::ErrorCode::HttpClient01BuildFailed.code_str(),
                "HTTP-CLIENT-01 reqwest client build failed — legacy candle cleanup skipped this boot"
            );
            metrics::counter!(
                "tv_http_client_build_failed_total",
                "site" => "shadow_drop_legacy"
            )
            .increment(1);
            return;
        }
    };

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

    // 6. Drop the dead per-timeframe movers grid (Track A, 2026-07-18).
    //    The movers runtime was deleted 2026-05-19 (AWS-lifecycle PRs
    //    #2-#4); across eras these names were MATERIALIZED VIEWS or plain
    //    TABLES, so each name gets BOTH drop forms. TWO PASSES, matviews
    //    first: on the matview-era vintage the `movers_5s..1d` /
    //    `movers_unified_*` matviews cascade from the `movers_1s` base
    //    table, and a base table with live dependent matviews refuses to
    //    drop — a single interleaved pass would attempt the `movers_1s`
    //    TABLE drop (grid position 1) before its dependents are gone,
    //    warn, and leave it orphaned FOREVER (the marker is still
    //    written). Dropping every matview first makes every subsequent
    //    table drop dependency-free. Both forms are `IF EXISTS`
    //    idempotent; a form mismatch is a logged warn, never a blocker.
    for name in retired_movers_object_names() {
        let view_ddl = format!("DROP MATERIALIZED VIEW IF EXISTS {name};");
        run_drop_ddl(&client, &base_url, &name, &view_ddl).await;
    }
    for name in retired_movers_object_names() {
        let table_ddl = format!("DROP TABLE IF EXISTS {name};");
        run_drop_ddl(&client, &base_url, &name, &table_ddl).await;
    }

    // PR #798 (operator-locked 2026-05-25) — write one-shot marker so
    // subsequent boots skip the entire cleanup loop. Best-effort: if
    // the marker can't be written (disk full, perms), the cleanup just
    // repeats on next boot — same as before this PR. No data corruption
    // risk because every DROP is `IF EXISTS`.
    if let Some(parent) = marker_path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(
            ?err,
            path = %parent.display(),
            "PR #798: failed to create data/state/ dir for legacy-drop marker"
        );
        return;
    }
    if let Err(err) = std::fs::write(marker_path, legacy_drop_marker_content()) {
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

/// Track A (2026-07-18) — the one-shot marker is now VERSIONED. Bump this
/// integer whenever the drop set is EXTENDED (new retired tables/matviews)
/// so every existing deployment re-runs the sweep exactly once for the new
/// list, then re-parks. Version history:
/// - (unversioned) PR #798 original marker — the pre-2026-07-18 sweep
/// - 2: historical_candles + stock/option_movers + the movers timeframe
///   grid (matview+table dual-drop) + the movers migration marker tables
const LEGACY_DROP_SWEEP_VERSION: u32 = 2;

/// The marker line the versioned gate greps for. Kept on its own line so a
/// future version bump changes exactly one token.
fn legacy_drop_marker_content() -> String {
    format!(
        "legacy candle/table drop sweep complete on this deployment.\n\
         sweep_version={LEGACY_DROP_SWEEP_VERSION}\n"
    )
}

/// True when the marker file content records a sweep version >= the
/// current [`LEGACY_DROP_SWEEP_VERSION`]. The original PR #798 marker had
/// no version line — it parses as version 0 and re-arms the sweep once.
fn marker_records_current_version(content: &str) -> bool {
    content
        .lines()
        .find_map(|line| line.trim().strip_prefix("sweep_version="))
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0)
        >= LEGACY_DROP_SWEEP_VERSION
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

    // ========================================================================
    // P2c (coverage-gaps #1076): DDL-walk + legacy-drop arm coverage via a
    // file-local mock HTTP server (same idiom as tick_persistence::tests).
    // ========================================================================

    const P2C_HTTP_200: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
    const P2C_HTTP_400: &str =
        "HTTP/1.1 400 Bad Request\r\nContent-Length: 13\r\n\r\n{\"error\":\"x\"}";

    async fn p2c_spawn_mock_http(response: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 8192];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            }
        });
        port
    }

    fn p2c_cfg(http_port: u16) -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port,
            pg_port: 1,
            ilp_port: 1,
        }
    }

    /// P2c: the 21-table DDL walk (CREATE + DEDUP + self-heal ALTERs)
    /// completes without panic against a 200-everything QuestDB.
    #[tokio::test]
    async fn test_ensure_shadow_candle_tables_with_mock_200() {
        let port = p2c_spawn_mock_http(P2C_HTTP_200).await;
        ensure_shadow_candle_tables(&p2c_cfg(port)).await;
    }

    /// P2c: a 400-everything QuestDB exercises every error/warn arm of the
    /// DDL walk; the function is best-effort and must not panic or hang.
    #[tokio::test]
    async fn test_ensure_shadow_candle_tables_with_mock_400() {
        let port = p2c_spawn_mock_http(P2C_HTTP_400).await;
        ensure_shadow_candle_tables(&p2c_cfg(port)).await;
    }

    /// P2c: legacy-drop marker lifecycle — first run (marker absent) sweeps
    /// every legacy object against the mock and writes the one-shot marker;
    /// second run takes the early-skip arm. Self-cleaning: the cwd-relative
    /// marker is removed before AND after so no repo pollution survives.
    #[tokio::test]
    async fn test_drop_legacy_candle_objects_marker_lifecycle() {
        let marker = std::path::Path::new(LEGACY_DROP_MARKER_PATH);
        let _ = std::fs::remove_file(marker);

        let port = p2c_spawn_mock_http(P2C_HTTP_200).await;
        drop_legacy_candle_objects(&p2c_cfg(port)).await;
        assert!(
            marker.exists(),
            "first run must write the one-shot marker (PR #798 gate)"
        );
        let written = std::fs::read_to_string(marker).expect("marker readable");
        assert!(
            marker_records_current_version(&written),
            "marker must record the CURRENT sweep version so the versioned \
             gate parks the sweep: {written:?}"
        );

        // Second run: marker present → early skip (no HTTP traffic needed).
        drop_legacy_candle_objects(&p2c_cfg(1)).await;

        let _ = std::fs::remove_file(marker);
        if let Some(parent) = marker.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }

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
        // feed-in-key (operator 2026-06-19): a Dhan candle and a Groww candle
        // for the same minute/instrument are distinct observations, both kept.
        assert!(DEDUP_KEY_CANDLES.contains("feed"));
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
    fn test_retired_questdb_tables_count_is_eighteen_and_unique() {
        assert_eq!(RETIRED_QUESTDB_TABLES.len(), 18);
        let mut seen = std::collections::HashSet::new();
        for table in RETIRED_QUESTDB_TABLES {
            assert!(seen.insert(table), "duplicate retired table: {table}");
        }
        // Track A drop extension (2026-07-18) — the approved additions are
        // pinned by name so a silent list rollback fails the build.
        for expected in [
            "historical_candles",
            "stock_movers",
            "option_movers",
            "movers_migration_2026_05_01",
            "movers_legacy_retire_2026_05_03",
        ] {
            assert!(
                RETIRED_QUESTDB_TABLES.contains(&expected),
                "approved retired table missing from the drop list: {expected}"
            );
        }
    }

    #[test]
    fn test_retired_movers_grid_is_53_unique_movers_names() {
        let names = retired_movers_object_names();
        assert_eq!(names.len(), 53, "movers grid must cover all 53 era names");
        let mut seen = std::collections::HashSet::new();
        for name in &names {
            assert!(seen.insert(name.clone()), "duplicate movers name: {name}");
            assert!(
                name.starts_with("movers_"),
                "movers grid must only contain movers_* names: {name}"
            );
        }
        // The #1615-era single-table entry moved into the grid — it must
        // stay covered (matview AND table drop) and must not silently drop
        // out of the sweep.
        assert!(seen.contains("movers_1s"));
        assert!(seen.contains("movers_unified_1d"));
        // The grid must never collide with a live Engine-B candle table or
        // any audit table (SEBI retention).
        for name in &names {
            assert!(!candle_table_names().contains(&name.as_str()));
            assert!(!name.ends_with("_audit"));
        }
    }

    #[test]
    fn test_legacy_drop_marker_versioning_rearm_semantics() {
        // Current-version marker content parks the sweep.
        assert!(marker_records_current_version(&legacy_drop_marker_content()));
        // The original unversioned PR #798 marker re-arms exactly once.
        assert!(!marker_records_current_version(
            "PR #798 marker — legacy candle objects cleaned up on this deployment.\n"
        ));
        // A lower version re-arms; a higher (future) version stays parked.
        assert!(!marker_records_current_version("sweep_version=1\n"));
        assert!(marker_records_current_version("sweep_version=99\n"));
        // Garbage never parks the sweep (fail-open toward re-running the
        // idempotent IF-EXISTS drops).
        assert!(!marker_records_current_version("sweep_version=abc\n"));
        assert!(!marker_records_current_version(""));
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
            // Track A (2026-07-18): `historical_candles` moved from the
            // KEEP guard into the drop list (no live writer since the
            // 2026-05-26 Dhan historical chain deletion — operator-approved
            // drop). `ticks` stays KEEP (the scoreboard reads it).
            assert!(
                !matches!(table, "ticks" | "option_chain_minute_snapshot"),
                "retired table {table} must not be a KEEP table"
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
