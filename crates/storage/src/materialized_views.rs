//! QuestDB materialized views for multi-timeframe candle aggregation.
//!
//! Creates the `candles_1s` base table and 18 materialized views covering
//! timeframes from 5 seconds to 1 month. Live WebSocket data arrives as IST
//! epoch seconds (stored directly). Historical REST data arrives as UTC epoch
//! seconds (+19800s offset applied at persistence). Both result in IST-based
//! timestamps.
//!
//! # Bucket alignment
//!
//! Most views use `OFFSET '00:00'` (IST midnight). The hourly view
//! `candles_1h` is the exception — it uses `OFFSET '00:15'` so its buckets
//! start at the NSE market open (09:15 IST) and match Dhan's `/charts/intraday`
//! `interval="60"` response timestamps exactly. Without that offset, live 60m
//! candles land at 09:00/10:00/... while historical 60m candles land at
//! 09:15/10:15/..., and every cross-verification `(ts, security_id)` join
//! misses on 60m. (1m/5m/15m naturally align because 15 divides into 1/5/15.)
//!
//! Timeframes 20-21 (3 months, 1 year) are computed in Rust from monthly data.
//!
//! # Idempotency
//! All DDL uses `CREATE TABLE IF NOT EXISTS` / `CREATE MATERIALIZED VIEW IF NOT EXISTS`.
//! Safe to call every startup. A one-time migration
//! (`drop_misaligned_hourly_chain_if_needed`) drops the hourly view chain
//! when it detects old `:00` alignment, so Step 5 recreates it at `:15`.

use std::time::Duration;

use tracing::{debug, error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{QUESTDB_IST_ALIGN_OFFSET, QUESTDB_TABLE_CANDLES_1S};

/// Bucket alignment for the hourly view chain.
///
/// NSE market opens at 09:15 IST. Dhan's `/charts/intraday` API with
/// `interval="60"` returns candles bucketed at `09:15, 10:15, ..., 15:15`.
/// Live 60m candles must bucket at the same offset for cross-match to join.
const QUESTDB_HOURLY_ALIGN_OFFSET: &str = "00:15";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for QuestDB DDL HTTP requests.
const DDL_TIMEOUT_SECS: u64 = 15;

/// DEDUP UPSERT KEY columns for the candles_1s table.
/// Includes `segment` to prevent cross-segment collision when IDX_I and NSE_EQ
/// share a security_id (e.g., NIFTY index vs NIFTY equity).
const DEDUP_KEY_CANDLES_1S: &str = "security_id, segment";

// ---------------------------------------------------------------------------
// candles_1s Base Table DDL
// ---------------------------------------------------------------------------

/// SQL to create the `candles_1s` table.
///
/// This is the base table that all materialized views aggregate from.
/// Built by Rust's `CandleAggregator` → ILP flush.
const CANDLES_1S_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS candles_1s (\
        segment SYMBOL,\
        security_id LONG,\
        open DOUBLE,\
        high DOUBLE,\
        low DOUBLE,\
        close DOUBLE,\
        volume LONG,\
        oi LONG,\
        tick_count INT,\
        iv DOUBLE,\
        delta DOUBLE,\
        gamma DOUBLE,\
        theta DOUBLE,\
        vega DOUBLE,\
        volume_cum_day_at_end LONG,\
        prev_day_close DOUBLE,\
        prev_day_oi LONG,\
        phase SYMBOL,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY DAY WAL\
";

/// Phase 1 lifecycle columns added to the `candles_1s` base table by the
/// 29-timeframes-engine plan (`active-plan-29-tf-and-movers-deletion.md`).
///
/// Pairs of `(column_name, questdb_type)`. Same idempotent ALTER pattern as
/// the Greeks columns. Sit empty after Phase 1 — Phase 2 wires the producer
/// (the Rust `CandleAggregator` projects them from the new tick columns).
///
/// Note: the candles version carries `volume_cum_day_at_end` (last cumulative
/// tick volume in the bar) — the analog of ticks' `volume_delta`. The names
/// differ deliberately: `ticks.volume_delta` is per-tick increment;
/// `candles_1s.volume_cum_day_at_end` is the cum-day total at bar close.
const CANDLES_1S_PHASE1_LIFECYCLE_COLUMNS: &[(&str, &str)] = &[
    ("volume_cum_day_at_end", "LONG"),
    ("prev_day_close", "DOUBLE"),
    ("prev_day_oi", "LONG"),
    ("phase", "SYMBOL"),
];

// ---------------------------------------------------------------------------
// Materialized View Definitions
// ---------------------------------------------------------------------------

/// A single materialized view definition: name, source table, and SAMPLE BY interval.
struct ViewDef {
    /// View name (e.g., "candles_5s").
    name: &'static str,
    /// Source table/view to aggregate from.
    source: &'static str,
    /// QuestDB SAMPLE BY interval (e.g., "5s", "1m", "1h").
    interval: &'static str,
    /// Whether source has tick_count (sub-minute views aggregate tick_count).
    has_tick_count: bool,
    /// `ALIGN TO CALENDAR WITH OFFSET` value (e.g., "00:00", "00:15").
    ///
    /// Defaults to `QUESTDB_IST_ALIGN_OFFSET` ("00:00"). Only the hourly view
    /// `candles_1h` overrides to `QUESTDB_HOURLY_ALIGN_OFFSET` ("00:15") so its
    /// buckets match Dhan's market-open-aligned 60m historical candles.
    align_offset: &'static str,
}

/// All 18 materialized views ordered by dependency chain.
///
/// Each view aggregates from its `source` — the dependency chain is:
/// candles_1s → 5s,10s,15s,30s,1m
/// candles_1m → 2m,3m,5m
/// candles_5m → 10m,15m
/// candles_15m → 30m
/// candles_1s → 1h (direct — '00:15' offset requires the physical base table)
/// candles_30m → 2h,3h,4h,1d
/// candles_1d → 7d,1M
const VIEW_DEFS: &[ViewDef] = &[
    // Sub-minute from 1s base
    ViewDef {
        name: "candles_5s",
        source: "candles_1s",
        interval: "5s",
        has_tick_count: true,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    ViewDef {
        name: "candles_10s",
        source: "candles_1s",
        interval: "10s",
        has_tick_count: true,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    ViewDef {
        name: "candles_15s",
        source: "candles_1s",
        interval: "15s",
        has_tick_count: true,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    ViewDef {
        name: "candles_30s",
        source: "candles_1s",
        interval: "30s",
        has_tick_count: true,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    ViewDef {
        name: "candles_1m",
        source: "candles_1s",
        interval: "1m",
        has_tick_count: true,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    // Minute-level from 1m
    ViewDef {
        name: "candles_2m",
        source: "candles_1m",
        interval: "2m",
        has_tick_count: false,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    ViewDef {
        name: "candles_3m",
        source: "candles_1m",
        interval: "3m",
        has_tick_count: false,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    ViewDef {
        name: "candles_5m",
        source: "candles_1m",
        interval: "5m",
        has_tick_count: false,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    // Multi-minute from 5m
    ViewDef {
        name: "candles_10m",
        source: "candles_5m",
        interval: "10m",
        has_tick_count: false,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    ViewDef {
        name: "candles_15m",
        source: "candles_5m",
        interval: "15m",
        has_tick_count: false,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    // Half-hour from 15m
    ViewDef {
        name: "candles_30m",
        source: "candles_15m",
        interval: "30m",
        has_tick_count: false,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    // Hourly from 1s base — offset '00:15' so buckets start at NSE market open
    // (09:15 IST) and match Dhan's /charts/intraday interval=60 timestamps.
    //
    // SOURCE FIX (post-PR #424, 2026-05-02): sourcing from `candles_15m`
    // (PR #424 attempt) was rejected by QuestDB with HTTP 400 + body
    // `"base table is not referenced in materialized view query: candles_15m"`.
    // QuestDB's mat-view-of-mat-view only works when the target's
    // ALIGN-TO-CALENDAR OFFSET matches the source's offset. `candles_15m`
    // is OFFSET '00:00'; `candles_1h` overrides to '00:15', so QuestDB
    // requires the query to reference the actual physical BASE TABLE
    // (`candles_1s`) — not an intermediate mat-view with a different
    // alignment. Sourcing directly from `candles_1s` (one row per second)
    // gives each 1h-at-:15 bucket a clean 3600× aggregation.
    ViewDef {
        name: "candles_1h",
        source: "candles_1s",
        interval: "1h",
        has_tick_count: false,
        align_offset: QUESTDB_HOURLY_ALIGN_OFFSET,
    },
    // Multi-hour, daily — sourced from `candles_30m` at OFFSET '00:00'.
    // Cannot source from `candles_1h` (OFFSET '00:15'): a 2h/3h/4h/1d
    // bucket aligned at midnight would not map cleanly to source buckets
    // aligned at 00:15. Sourcing from 30m at 00:00 keeps each downstream
    // bucket aligned to midnight while still aggregating from a finer
    // grain than candles_1s.
    ViewDef {
        name: "candles_2h",
        source: "candles_30m",
        interval: "2h",
        has_tick_count: false,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    ViewDef {
        name: "candles_3h",
        source: "candles_30m",
        interval: "3h",
        has_tick_count: false,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    ViewDef {
        name: "candles_4h",
        source: "candles_30m",
        interval: "4h",
        has_tick_count: false,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    // Daily, sourced from `candles_30m` at OFFSET '00:00' for the same
    // alignment reason as 2h/3h/4h. (48× 30m source buckets per day.)
    ViewDef {
        name: "candles_1d",
        source: "candles_30m",
        interval: "1d",
        has_tick_count: false,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    // Weekly from 1d
    ViewDef {
        name: "candles_7d",
        source: "candles_1d",
        interval: "7d",
        has_tick_count: false,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
    // Monthly from 1d
    ViewDef {
        name: "candles_1M",
        source: "candles_1d",
        interval: "1M",
        has_tick_count: false,
        align_offset: QUESTDB_IST_ALIGN_OFFSET,
    },
];

// ---------------------------------------------------------------------------
// Pure helper functions (testable without DB)
// ---------------------------------------------------------------------------

/// Builds the QuestDB HTTP exec URL from host and port.
fn build_questdb_exec_url(host: &str, http_port: u16) -> String {
    format!("http://{}:{}/exec", host, http_port)
}

/// Builds the ALTER TABLE DEDUP ENABLE UPSERT KEYS SQL statement.
fn build_dedup_sql(table_name: &str, dedup_key: &str) -> String {
    format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        table_name, dedup_key
    )
}

/// Returns the total number of materialized view definitions.
#[cfg(test)]
fn view_count() -> usize {
    VIEW_DEFS.len()
}

/// Returns all view names in dependency order.
#[cfg(test)]
fn view_names() -> Vec<&'static str> {
    VIEW_DEFS.iter().map(|d| d.name).collect()
}

/// Validates that all view dependency sources are available.
///
/// Returns `true` if every view's `source` is either the base table or a view defined earlier.
#[cfg(test)]
fn validate_dependency_order(base_table: &str) -> bool {
    let mut available = vec![base_table];
    for def in VIEW_DEFS {
        if !available.contains(&def.source) {
            return false;
        }
        available.push(def.name);
    }
    true
}

/// Builds the CREATE MATERIALIZED VIEW SQL for a given view definition.
///
/// Data is stored as IST-as-UTC. Most views use `offset '00:00'` since midnight
/// "UTC" IS midnight IST. The `candles_1h` view overrides to `'00:15'` so its
/// buckets start at the NSE market open (09:15 IST), matching Dhan's
/// `/charts/intraday` `interval="60"` response exactly — required for
/// cross-verification `(security_id, ts, segment)` joins to succeed on 60m.
fn build_view_sql(def: &ViewDef) -> String {
    let tick_count_select = if def.has_tick_count {
        ", sum(tick_count) AS tick_count"
    } else {
        ""
    };

    // Phase 1 lifecycle columns (29-timeframes engine plan): every matview
    // projects the 4 new columns via `last(...)` so the cum-day volume,
    // frozen prev-day-close/OI and phase are visible at every TF without
    // a JOIN. Source must be a base table or matview that already carries
    // these columns — `CANDLES_1S_PHASE1_LIFECYCLE_COLUMNS` declares them
    // on `candles_1s`, and rebuilding the matview chain in dependency
    // order propagates them through every downstream view.
    format!(
        "CREATE MATERIALIZED VIEW IF NOT EXISTS {name} AS \
         SELECT ts, security_id, segment, \
         first(open) AS open, max(high) AS high, \
         min(low) AS low, last(close) AS close, \
         sum(volume) AS volume, last(oi) AS oi{tick_count}, \
         last(iv) AS iv, last(delta) AS delta, last(gamma) AS gamma, \
         last(theta) AS theta, last(vega) AS vega, \
         last(volume_cum_day_at_end) AS volume_cum_day_at_end, \
         last(prev_day_close) AS prev_day_close, \
         last(prev_day_oi) AS prev_day_oi, \
         last(phase) AS phase \
         FROM {source} \
         SAMPLE BY {interval} \
         ALIGN TO CALENDAR WITH OFFSET '{offset}'",
        name = def.name,
        source = def.source,
        interval = def.interval,
        tick_count = tick_count_select,
        offset = def.align_offset,
    )
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Creates the `candles_1s` base table and all 18 materialized views.
///
/// Idempotent — safe to call every startup. Logs warnings on failure
/// and continues (best-effort). QuestDB must be reachable for views
/// to be created successfully.
pub async fn ensure_candle_views(questdb_config: &QuestDbConfig) {
    let base_url = build_questdb_exec_url(&questdb_config.host, questdb_config.http_port);

    // Client::builder().timeout().build() is infallible (no custom TLS).
    // Coverage: unwrap_or_else avoids uncoverable else-return on dead path.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    // Step 1: Create the candles_1s base table.
    if !execute_ddl(
        &client,
        &base_url,
        CANDLES_1S_CREATE_DDL,
        QUESTDB_TABLE_CANDLES_1S,
    )
    .await
    {
        return;
    }

    // Step 2: Enable DEDUP UPSERT KEYS on candles_1s.
    // If DEDUP fails with "deduplicate key column not found", the table has stale
    // schema — auto-recover by DROP + CREATE + DEDUP.
    let dedup_sql = build_dedup_sql(QUESTDB_TABLE_CANDLES_1S, DEDUP_KEY_CANDLES_1S);
    if !execute_ddl_check_stale(
        &client,
        &base_url,
        &dedup_sql,
        "candles_1s DEDUP",
        QUESTDB_TABLE_CANDLES_1S,
        CANDLES_1S_CREATE_DDL,
    )
    .await
    {
        return;
    }

    // Step 3: Add Greeks columns to candles_1s if missing (schema migration).
    // QuestDB ALTER TABLE ADD COLUMN is idempotent — adding an already-existing
    // column returns a non-fatal error that we safely ignore.
    ensure_greeks_columns_on_table(&client, &base_url, QUESTDB_TABLE_CANDLES_1S).await;

    // Step 3b: Add Phase 1 lifecycle columns to candles_1s (29-timeframes plan).
    // Same idempotent ALTER pattern. Columns sit empty until Phase 2 populates
    // them — Phase 1 only widens the schema.
    ensure_phase1_lifecycle_columns_on_table(&client, &base_url, QUESTDB_TABLE_CANDLES_1S).await;

    // Step 4: Drop materialized views if they lack required columns.
    // Views created before Greeks/Phase1 cannot be ALTERed — they must be
    // dropped and recreated. Three probes:
    //   (a) `views_missing_greeks` — check candles_5s for `iv`.
    //   (b) `views_missing_phase1_columns` — check candles_5s for `phase`.
    //   (c) `base_missing_phase1_columns` — check candles_1s itself for
    //       `phase`. Closes the silent-ALTER gap (the brownfield ALTER
    //       loop discards each `Result` via `drop(...)` per the established
    //       Greeks pattern; probing only views would miss the case where
    //       the base ALTER dropped but a previous boot's views still exist).
    // If any probe fires, drop all 18 views so Step 5 recreates them with
    // the wide schema.
    if views_missing_greeks(&client, &base_url).await
        || views_missing_phase1_columns(&client, &base_url).await
        || base_missing_phase1_columns(&client, &base_url).await
    {
        info!(
            "materialized views or candles_1s base lack Greeks / Phase 1 \
             lifecycle columns — dropping views for recreation"
        );
        drop_all_views(&client, &base_url).await;
    }

    // Step 4b: Drop the hourly-chain views if candles_1h has stale 00:00
    // alignment (old schema). Step 5 will recreate candles_1h at 00:15 so its
    // buckets match Dhan's market-open-aligned 60m historical candles and the
    // cross-match (security_id, ts, segment) join succeeds on 60m.
    if hourly_view_misaligned(&client, &base_url).await {
        info!(
            "candles_1h has stale 00:00 alignment — dropping hourly chain for \
             00:15 realignment (matches Dhan /charts/intraday interval=60)"
        );
        drop_hourly_chain_views(&client, &base_url).await;
    }

    // Step 5: Create materialized views in dependency order.
    let mut created_count: u32 = 0;
    for def in VIEW_DEFS {
        let sql = build_view_sql(def);
        if execute_ddl(&client, &base_url, &sql, def.name).await {
            created_count = created_count.saturating_add(1);
        }
    }

    info!(
        base_table = QUESTDB_TABLE_CANDLES_1S,
        views_created = created_count,
        views_total = VIEW_DEFS.len(),
        "candle materialized views setup complete"
    );

    // Step 6: Verify which views actually exist in QuestDB.
    // This provides clear diagnostics when cross-match fails due to missing views.
    verify_views_exist(&client, &base_url).await;
}

/// The 5 cross-match critical view names checked by `cross_verify.rs`.
/// These are the views that MUST exist for live-vs-historical cross-match to work.
const CROSS_MATCH_CRITICAL_VIEWS: &[&str] = &[
    "candles_1m",
    "candles_5m",
    "candles_15m",
    "candles_1h",
    "candles_1d",
];

/// Verifies which materialized views actually exist in QuestDB after creation.
///
/// Uses `SHOW COLUMNS FROM <view>` instead of `tables()` because QuestDB's
/// `tables()` function does NOT include materialized views — only base tables.
/// The `SHOW COLUMNS` approach is proven: `views_missing_greeks()` already uses it.
/// Missing views are logged at ERROR level to provide clear diagnostics
/// when cross-match verification fails.
async fn verify_views_exist(client: &reqwest::Client, base_url: &str) {
    let mut existing_count: u32 = 0;
    let mut missing: Vec<&str> = Vec::new();

    for def in VIEW_DEFS {
        // SAFETY: def.name is from VIEW_DEFS constants, not user input.
        // Use SHOW COLUMNS instead of tables() — QuestDB tables() doesn't list
        // materialized views. SHOW COLUMNS returns 200 if the view exists.
        let query = format!("SHOW COLUMNS FROM {}", def.name);
        let exists = match client
            .get(base_url)
            .query(&[("query", &query)])
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                // SHOW COLUMNS succeeded → view exists. Verify response has content.
                let body = response.text().await.unwrap_or_default();
                !body.is_empty()
            }
            _ => false,
        };

        if exists {
            existing_count = existing_count.saturating_add(1);
        } else {
            missing.push(def.name);
        }
    }

    if missing.is_empty() {
        info!(
            existing_count,
            "all materialized views verified — cross-match ready"
        );
    } else {
        // Check if any critical views (used by cross-match) are missing
        let critical_missing: Vec<&str> = missing
            .iter()
            .filter(|name| CROSS_MATCH_CRITICAL_VIEWS.contains(name))
            .copied()
            .collect();

        if critical_missing.is_empty() {
            warn!(
                existing = existing_count,
                missing_count = missing.len(),
                missing_views = ?missing,
                "some materialized views missing (non-critical for cross-match)"
            );
        } else {
            error!(
                existing = existing_count,
                missing_count = missing.len(),
                missing_views = ?missing,
                critical_missing = ?critical_missing,
                "CRITICAL: materialized views missing — cross-match will fail. \
                 Check QuestDB logs for view creation errors."
            );
        }
    }
}

/// The 5 Greeks column names used in ticks, candles_1s, and materialized views.
const GREEKS_COLUMN_NAMES: &[&str] = &["iv", "delta", "gamma", "theta", "vega"];

/// Adds the 5 Greeks columns to a table if they are missing.
///
/// QuestDB `ALTER TABLE ADD COLUMN` is idempotent — adding an already-existing
/// column returns a non-fatal error that we safely ignore. Zero manual intervention.
async fn ensure_greeks_columns_on_table(
    client: &reqwest::Client,
    base_url: &str,
    table_name: &str,
) {
    for col in GREEKS_COLUMN_NAMES {
        let alter_sql = format!("ALTER TABLE {table_name} ADD COLUMN {col} DOUBLE");
        drop(
            client
                .get(base_url)
                .query(&[("query", &alter_sql)])
                .send()
                .await,
        );
    }
    info!(
        table = table_name,
        "Greeks columns ensured (idempotent ADD COLUMN)"
    );
}

/// Adds the 4 Phase 1 lifecycle columns (29-timeframes engine plan) to a table
/// if they are missing.
///
/// Mirrors the Greeks helper. Mixed types: LONG / DOUBLE / LONG / SYMBOL.
/// Idempotent — QuestDB rejects adding an already-existing column with a
/// non-fatal error that we safely ignore.
async fn ensure_phase1_lifecycle_columns_on_table(
    client: &reqwest::Client,
    base_url: &str,
    table_name: &str,
) {
    for (col, ty) in CANDLES_1S_PHASE1_LIFECYCLE_COLUMNS {
        // Double-quoted identifiers — defence-in-depth even though the
        // current callers pass compile-time constants only.
        let alter_sql = format!("ALTER TABLE \"{table_name}\" ADD COLUMN \"{col}\" {ty}");
        drop(
            client
                .get(base_url)
                .query(&[("query", &alter_sql)])
                .send()
                .await,
        );
    }
    info!(
        table = table_name,
        "Phase 1 lifecycle columns ensured (idempotent ADD COLUMN)"
    );
}

/// Checks whether the `candles_1s` BASE TABLE is missing the Phase 1
/// lifecycle column `phase` (29-timeframes engine plan).
///
/// Defends against the silent-ALTER scenario flagged by the hostile
/// review: the brownfield ALTER loop discards each `Result` via `drop(...)`
/// per the established Greeks pattern. If a network blip drops the
/// `phase` ALTER on `candles_1s`, the matview probe (`views_missing_phase1_columns`,
/// which queries `candles_5s`) cannot detect it because views still exist
/// from a previous boot. Probing the base table directly closes the gap:
/// if `candles_1s` itself lacks `phase`, we must rebuild matviews so a
/// future Phase 2 writer doesn't ILP into a missing column.
///
/// Returns `true` when the base table needs Phase 1 lifecycle columns
/// (and therefore views also need recreation). Failed probe → `false`.
async fn base_missing_phase1_columns(client: &reqwest::Client, base_url: &str) -> bool {
    let probe_sql = "SHOW COLUMNS FROM candles_1s";
    match client
        .get(base_url)
        .query(&[("query", probe_sql)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                let body = response.text().await.unwrap_or_default();
                // Match the column name as a quoted JSON token to avoid
                // false-positives from substrings like `prev_phase`.
                if !body.contains("\"phase\"") {
                    return true;
                }
            }
            false
        }
        Err(_) => false,
    }
}

/// Checks whether existing materialized views are missing the Phase 1
/// lifecycle column `phase` (29-timeframes engine plan).
///
/// Mirrors `views_missing_greeks` — probes `candles_5s` and looks for
/// the `phase` column in the `SHOW COLUMNS` response. We probe `phase`
/// (the last column added) because if it is present, the other three
/// must also be present (they were added together in this PR).
///
/// Returns `true` when views need recreation. Failed probe → `false`
/// (Step 5's `CREATE ... IF NOT EXISTS` handles missing views directly).
async fn views_missing_phase1_columns(client: &reqwest::Client, base_url: &str) -> bool {
    let probe_sql = "SHOW COLUMNS FROM candles_5s";
    match client
        .get(base_url)
        .query(&[("query", probe_sql)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                let body = response.text().await.unwrap_or_default();
                // If candles_5s exists but does not have a `phase` column,
                // the views need recreation. Match `"phase"` as a quoted JSON
                // token so we don't accidentally match `phase` substrings
                // elsewhere in the response.
                if !body.contains("\"phase\"") {
                    return true;
                }
            }
            false
        }
        Err(_) => false,
    }
}

/// Checks whether existing materialized views are missing Greeks columns.
///
/// Probes `candles_5s` (the first view in the dependency chain) by querying
/// `SHOW COLUMNS FROM candles_5s`. If the response does not contain 'iv',
/// the views were created before Greeks support and must be dropped + recreated.
///
/// Returns `true` if views need recreation, `false` if they already have Greeks
/// or if the probe fails (in which case `CREATE ... IF NOT EXISTS` in Step 5
/// handles it).
async fn views_missing_greeks(client: &reqwest::Client, base_url: &str) -> bool {
    let probe_sql = "SHOW COLUMNS FROM candles_5s";
    match client
        .get(base_url)
        .query(&[("query", probe_sql)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                let body = response.text().await.unwrap_or_default();
                // If candles_5s exists but does not have an 'iv' column,
                // the views need recreation.
                if !body.contains("iv") {
                    return true;
                }
            }
            // Non-success (e.g., view doesn't exist) → Step 5 will create it.
            false
        }
        Err(_) => false,
    }
}

/// Drops all 18 materialized views in reverse dependency order.
///
/// Reverse order ensures child views are dropped before their parents.
/// `DROP MATERIALIZED VIEW IF EXISTS` is idempotent.
async fn drop_all_views(client: &reqwest::Client, base_url: &str) {
    for def in VIEW_DEFS.iter().rev() {
        let drop_sql = format!("DROP MATERIALIZED VIEW IF EXISTS {}", def.name);
        drop(
            client
                .get(base_url)
                .query(&[("query", &drop_sql)])
                .send()
                .await,
        );
        debug!(
            view = def.name,
            "dropped materialized view for Greeks migration"
        );
    }
    info!("all 18 materialized views dropped for Greeks migration");
}

/// Names of the hourly-chain views that must be rebuilt together when
/// `candles_1h`'s alignment offset changes. Listed in reverse dependency order
/// so children are dropped before parents.
///
/// PR #424 source-chain fix: `candles_1h` no longer has any dependents in
/// `VIEW_DEFS` (multi-hour and daily views now source from `candles_30m` for
/// midnight alignment, weekly/monthly source from `candles_1d`). The set is
/// thus a single-element list — only `candles_1h` itself needs dropping when
/// its OFFSET '00:15' alignment changes.
const HOURLY_CHAIN_VIEWS: &[&str] = &["candles_1h"];

/// Checks whether `candles_1h` still uses the old `00:00` bucket alignment.
///
/// Probes the view for a single row's timestamp and computes
/// `(epoch_seconds) % 3600`. With new `00:15` alignment every bucket timestamp
/// has seconds-of-hour = 900. With old `00:00` alignment it is 0.
///
/// Returns `true` when the probe succeeds and the sampled row is `:00`-aligned
/// (migration required). Returns `false` when the view is absent, empty, or
/// already `:15`-aligned.
async fn hourly_view_misaligned(client: &reqwest::Client, base_url: &str) -> bool {
    // Use QuestDB's datediff-friendly SQL: cast ts to long nanos, divide to
    // seconds, modulo 3600. `LIMIT 1` keeps the probe cheap.
    let probe_sql =
        "SELECT (to_long(ts) / 1000000000) % 3600 AS sec_in_hour FROM candles_1h LIMIT 1";
    let response = match client
        .get(base_url)
        .query(&[("query", probe_sql)])
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        // View absent or query failed → Step 5 will create with new alignment.
        _ => return false,
    };

    let body = match response.text().await {
        Ok(b) => b,
        Err(_) => return false,
    };

    // Cheap JSON scan: look for `"dataset":[[0]]` vs `"dataset":[[900]]`.
    // Any value other than 900 on a freshly-produced bucket means old schema.
    // Presence of `[[0]]` specifically locks the old-schema detection.
    if body.contains("\"dataset\":[[0]]") {
        return true;
    }
    // Also treat "count=0" / empty dataset as non-misaligned (nothing to fix).
    false
}

/// Drops the hourly-chain materialized views so Step 5 recreates them with the
/// updated alignment offset. Safe to call even if some views don't exist —
/// `DROP MATERIALIZED VIEW IF EXISTS` is idempotent.
async fn drop_hourly_chain_views(client: &reqwest::Client, base_url: &str) {
    for name in HOURLY_CHAIN_VIEWS {
        let drop_sql = format!("DROP MATERIALIZED VIEW IF EXISTS {name}");
        drop(
            client
                .get(base_url)
                .query(&[("query", &drop_sql)])
                .send()
                .await,
        );
        debug!(view = name, "dropped hourly-chain view for realignment");
    }
    info!(
        view_count = HOURLY_CHAIN_VIEWS.len(),
        "hourly-chain views dropped for 00:15 realignment"
    );
}

/// Executes a DEDUP DDL with auto-recovery on stale schema. Returns true on success.
///
/// If DEDUP fails with "deduplicate key column not found", drops and recreates the table.
async fn execute_ddl_check_stale(
    client: &reqwest::Client,
    base_url: &str,
    dedup_sql: &str,
    label: &str,
    table_name: &str,
    create_ddl: &str,
) -> bool {
    match client
        .get(base_url)
        .query(&[("query", dedup_sql)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                debug!(label, "DDL executed successfully");
                true
            } else {
                let body = response.text().await.unwrap_or_default();
                if body.contains("deduplicate key column not found") {
                    warn!(
                        table_name,
                        "stale schema detected — dropping and recreating"
                    );
                    let drop_sql = format!("DROP TABLE IF EXISTS {table_name}");
                    drop(
                        client
                            .get(base_url)
                            .query(&[("query", &drop_sql)])
                            .send()
                            .await,
                    );
                    drop(
                        client
                            .get(base_url)
                            .query(&[("query", create_ddl)])
                            .send()
                            .await,
                    );
                    drop(
                        client
                            .get(base_url)
                            .query(&[("query", dedup_sql)])
                            .send()
                            .await,
                    );
                    info!(table_name, "table recreated with correct schema");
                    true
                } else {
                    warn!(
                        label,
                        body = body.chars().take(200).collect::<String>(),
                        "DDL returned non-success"
                    );
                    false
                }
            }
        }
        Err(err) => {
            warn!(label, ?err, "DDL request failed");
            false
        }
    }
}

/// Extracts QuestDB's `error` field from a JSON response body, falling back
/// to a length-bounded substring of the raw body when parsing fails.
///
/// QuestDB `/exec` responses for failed DDL look like:
///   `{"query":"<echo>","error":"<message>","position":NN}`
/// The query echo is often hundreds of characters long, so the 200-char
/// truncation that previously fed `tracing::warn!` clipped before reaching
/// the actual error message — leaving operators with HTTP status only and
/// no clue why QuestDB rejected the DDL.
fn extract_questdb_error(body: &str) -> String {
    // Cheap-path: look for `"error":"` and extract the quoted string. Avoids
    // serde_json overhead for the common failure path.
    if let Some(start) = body.find(r#""error":""#) {
        let after_marker = start + r#""error":""#.len();
        if let Some(rest) = body.get(after_marker..) {
            // Find the closing quote, respecting that QuestDB's error
            // messages don't contain backslash-escaped quotes in practice.
            if let Some(end) = rest.find('"') {
                return rest[..end].to_string();
            }
        }
    }
    // Fallback: length-bounded substring so log lines stay finite.
    body.chars().take(800).collect()
}

/// Executes a single DDL statement against QuestDB. Returns true on success.
///
/// Validates BOTH HTTP status code AND response body — QuestDB can return
/// HTTP 200 with an error message in the body for some DDL operations.
async fn execute_ddl(client: &reqwest::Client, base_url: &str, sql: &str, label: &str) -> bool {
    match client.get(base_url).query(&[("query", sql)]).send().await {
        Ok(response) => {
            if response.status().is_success() {
                // QuestDB may return HTTP 200 with error in body — validate content.
                let body = response.text().await.unwrap_or_default();
                if body.contains("\"error\"") || body.contains("\"Error\"") {
                    warn!(
                        label,
                        questdb_error = %extract_questdb_error(&body),
                        body_prefix = %body.chars().take(200).collect::<String>(),
                        "DDL returned HTTP 200 but body contains error — treating as failure"
                    );
                    false
                } else {
                    debug!(label, "DDL executed successfully");
                    true
                }
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    label,
                    %status,
                    questdb_error = %extract_questdb_error(&body),
                    body_prefix = %body.chars().take(200).collect::<String>(),
                    "DDL returned non-success"
                );
                false
            }
        }
        Err(err) => {
            warn!(label, ?err, "DDL request failed");
            false
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
    fn candles_1s_ddl_has_correct_schema() {
        assert!(CANDLES_1S_CREATE_DDL.contains("candles_1s"));
        assert!(CANDLES_1S_CREATE_DDL.contains("TIMESTAMP(ts)"));
        assert!(CANDLES_1S_CREATE_DDL.contains("PARTITION BY DAY"));
        assert!(CANDLES_1S_CREATE_DDL.contains("security_id LONG"));
        assert!(CANDLES_1S_CREATE_DDL.contains("tick_count INT"));
        assert!(CANDLES_1S_CREATE_DDL.contains("iv DOUBLE"));
        assert!(CANDLES_1S_CREATE_DDL.contains("delta DOUBLE"));
        assert!(CANDLES_1S_CREATE_DDL.contains("gamma DOUBLE"));
        assert!(CANDLES_1S_CREATE_DDL.contains("theta DOUBLE"));
        assert!(CANDLES_1S_CREATE_DDL.contains("vega DOUBLE"));
    }

    #[test]
    fn view_defs_has_18_views() {
        assert_eq!(
            VIEW_DEFS.len(),
            18,
            "must have 18 materialized views (5s through 1M)"
        );
    }

    #[test]
    fn view_defs_dependency_order_is_valid() {
        // Each view's source must either be candles_1s or a view defined earlier.
        let mut available = vec!["candles_1s"];
        for def in VIEW_DEFS {
            assert!(
                available.contains(&def.source),
                "view {} depends on {} which is not yet defined",
                def.name,
                def.source
            );
            available.push(def.name);
        }
    }

    #[test]
    fn build_view_sql_includes_ist_offset() {
        // IST-as-UTC data: offset '00:00' since midnight "UTC" IS midnight IST.
        let def = &VIEW_DEFS[0]; // candles_5s
        let sql = build_view_sql(def);
        assert!(sql.contains("ALIGN TO CALENDAR WITH OFFSET '00:00'"));
        assert!(sql.contains("SAMPLE BY 5s"));
        assert!(sql.contains("FROM candles_1s"));
        assert!(sql.contains("candles_5s"));
    }

    /// Cross-verification regression (2026-04-23).
    ///
    /// The hourly view `candles_1h` MUST bucket at NSE market open (09:15 IST),
    /// not IST midnight. Dhan's `/charts/intraday` `interval="60"` response
    /// stamps candles at `09:15, 10:15, ...` — the live view must match so the
    /// cross-match `(security_id, ts, segment)` join succeeds. With the old
    /// `'00:00'` offset every single live 60m candle was flagged as
    /// "MISSING HISTORICAL" because `09:00 != 09:15`.
    #[test]
    fn candles_1h_uses_market_open_aligned_offset() {
        let def = VIEW_DEFS
            .iter()
            .find(|d| d.name == "candles_1h")
            .expect("candles_1h view must exist");
        assert_eq!(
            def.align_offset, "00:15",
            "candles_1h must use 00:15 offset to match Dhan's market-open \
             aligned 60m historical candles (09:15, 10:15, ...). Regressing \
             this breaks the cross-match JOIN on every 60m candle."
        );
    }

    #[test]
    fn build_view_sql_candles_1h_emits_00_15_offset() {
        let def = VIEW_DEFS
            .iter()
            .find(|d| d.name == "candles_1h")
            .expect("candles_1h view must exist");
        let sql = build_view_sql(def);
        assert!(
            sql.contains("ALIGN TO CALENDAR WITH OFFSET '00:15'"),
            "candles_1h SQL must carry 00:15 offset, got: {sql}"
        );
        assert!(sql.contains("SAMPLE BY 1h"));
    }

    #[test]
    fn only_candles_1h_overrides_default_offset() {
        // Every non-hourly view must stay on the default IST-midnight offset.
        // If a future session adds another market-open-aligned view, this test
        // forces them to update this guard deliberately.
        for def in VIEW_DEFS {
            if def.name == "candles_1h" {
                assert_eq!(
                    def.align_offset, QUESTDB_HOURLY_ALIGN_OFFSET,
                    "candles_1h must use hourly offset"
                );
            } else {
                assert_eq!(
                    def.align_offset, QUESTDB_IST_ALIGN_OFFSET,
                    "view {} must use default IST-midnight offset (only \
                     candles_1h is market-open aligned)",
                    def.name
                );
            }
        }
    }

    #[test]
    fn hourly_chain_views_matches_candles_1h_dependents() {
        // HOURLY_CHAIN_VIEWS must cover candles_1h and every view that samples
        // from it (directly or transitively). If a new descendant is added to
        // VIEW_DEFS, this test forces updating HOURLY_CHAIN_VIEWS so the
        // migration keeps dropping the whole sub-tree together.
        let mut expected: Vec<&str> = vec!["candles_1h"];
        // BFS from candles_1h over the VIEW_DEFS dependency graph.
        let mut frontier = vec!["candles_1h"];
        while let Some(parent) = frontier.pop() {
            for def in VIEW_DEFS {
                if def.source == parent && !expected.contains(&def.name) {
                    expected.push(def.name);
                    frontier.push(def.name);
                }
            }
        }
        expected.sort_unstable();
        let mut actual: Vec<&str> = HOURLY_CHAIN_VIEWS.to_vec();
        actual.sort_unstable();
        assert_eq!(
            expected, actual,
            "HOURLY_CHAIN_VIEWS must equal candles_1h + all its transitive \
             dependents. If you added a view sampling from the hourly chain, \
             include it here so the 00:15 migration drops it too."
        );
    }

    #[test]
    fn hourly_align_offset_constant_is_market_open() {
        assert_eq!(
            QUESTDB_HOURLY_ALIGN_OFFSET, "00:15",
            "NSE market opens at 09:15 IST. Changing this constant will \
             break the cross-match alignment contract."
        );
    }

    #[test]
    fn build_view_sql_with_tick_count() {
        let def = &VIEW_DEFS[0]; // candles_5s has tick_count
        let sql = build_view_sql(def);
        assert!(sql.contains("sum(tick_count)"));
    }

    #[test]
    fn build_view_sql_without_tick_count() {
        let def = &VIEW_DEFS[5]; // candles_2m — no tick_count
        let sql = build_view_sql(def);
        assert!(!sql.contains("tick_count"));
    }

    #[test]
    fn all_view_names_start_with_candles() {
        for def in VIEW_DEFS {
            assert!(
                def.name.starts_with("candles_"),
                "view name {} must start with candles_",
                def.name
            );
        }
    }

    #[test]
    fn no_duplicate_view_names() {
        let mut names: Vec<&str> = VIEW_DEFS.iter().map(|d| d.name).collect();
        let original_len = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), original_len, "duplicate view names found");
    }

    #[test]
    fn candles_1m_view_exists() {
        // The 1m view is special — it overlaps with the historical candles_1m table.
        // QuestDB materialized views are separate objects from tables.
        let has_1m = VIEW_DEFS.iter().any(|d| d.name == "candles_1m");
        assert!(has_1m, "must have candles_1m materialized view");
    }

    #[test]
    fn all_views_include_segment_in_select() {
        // segment must be a non-aggregated SELECT column in every view.
        // QuestDB SAMPLE BY groups by all non-aggregated columns, so this
        // ensures candles are never mixed across segments (e.g., IDX_I vs NSE_EQ).
        for def in VIEW_DEFS {
            let sql = build_view_sql(def);
            assert!(
                sql.contains("segment"),
                "view {} must include segment in SELECT to prevent cross-segment aggregation",
                def.name
            );
        }
    }

    #[test]
    fn candles_1s_ddl_has_segment_column() {
        // The base table must have segment as a column for views to reference it.
        assert!(
            CANDLES_1S_CREATE_DDL.contains("segment SYMBOL"),
            "candles_1s DDL must have segment SYMBOL column"
        );
    }

    /// Phase 1 ratchet (29-timeframes engine plan): the four lifecycle columns
    /// MUST appear in the candles_1s base-table DDL so a fresh QuestDB volume
    /// gets the wide schema directly. Pins the QuestDB type per column so any
    /// future drift fails the build.
    #[test]
    fn test_candles_1s_ddl_has_phase1_lifecycle_columns() {
        assert!(
            CANDLES_1S_CREATE_DDL.contains("volume_cum_day_at_end LONG"),
            "candles_1s DDL must declare volume_cum_day_at_end LONG (cum-day total at bar close)"
        );
        assert!(
            CANDLES_1S_CREATE_DDL.contains("prev_day_close DOUBLE"),
            "candles_1s DDL must declare prev_day_close DOUBLE (frozen per trading day)"
        );
        assert!(
            CANDLES_1S_CREATE_DDL.contains("prev_day_oi LONG"),
            "candles_1s DDL must declare prev_day_oi LONG (frozen per trading day)"
        );
        assert!(
            CANDLES_1S_CREATE_DDL.contains("phase SYMBOL"),
            "candles_1s DDL must declare phase SYMBOL"
        );
    }

    /// Phase 1 ratchet: the Phase 1 lifecycle column constant MUST list exactly
    /// the four columns in the order the schema-self-heal helper iterates them.
    #[test]
    fn test_candles_1s_phase1_lifecycle_column_constant_is_pinned() {
        let pairs: Vec<(&str, &str)> = CANDLES_1S_PHASE1_LIFECYCLE_COLUMNS.to_vec();
        assert_eq!(
            pairs,
            vec![
                ("volume_cum_day_at_end", "LONG"),
                ("prev_day_close", "DOUBLE"),
                ("prev_day_oi", "LONG"),
                ("phase", "SYMBOL"),
            ],
            "CANDLES_1S_PHASE1_LIFECYCLE_COLUMNS pinned to the plan-locked \
             column list — drift would split the brownfield ALTER chain from \
             the greenfield CREATE"
        );
    }

    /// Phase 1 ratchet: every entry in `CANDLES_1S_PHASE1_LIFECYCLE_COLUMNS`
    /// must also appear in the CREATE DDL with the same QuestDB type.
    #[test]
    fn test_candles_1s_phase1_lifecycle_columns_match_create_ddl() {
        for (col, ty) in CANDLES_1S_PHASE1_LIFECYCLE_COLUMNS {
            let needle = format!("{} {}", col, ty);
            assert!(
                CANDLES_1S_CREATE_DDL.contains(&needle),
                "CANDLES_1S_CREATE_DDL must contain '{}' so fresh tables \
                 match the brownfield ALTER schema",
                needle
            );
        }
    }

    /// Phase 1 ratchet: every materialized view MUST project the four
    /// lifecycle columns via `last(...)`. Without this projection downstream
    /// queries (the future `/api/movers` v2 SELECT) cannot read the columns
    /// at any TF other than `1s`.
    #[test]
    fn test_build_view_sql_projects_phase1_lifecycle_columns() {
        for def in VIEW_DEFS {
            let sql = build_view_sql(def);
            assert!(
                sql.contains("last(volume_cum_day_at_end) AS volume_cum_day_at_end"),
                "view {} must project last(volume_cum_day_at_end)",
                def.name
            );
            assert!(
                sql.contains("last(prev_day_close) AS prev_day_close"),
                "view {} must project last(prev_day_close)",
                def.name
            );
            assert!(
                sql.contains("last(prev_day_oi) AS prev_day_oi"),
                "view {} must project last(prev_day_oi)",
                def.name
            );
            assert!(
                sql.contains("last(phase) AS phase"),
                "view {} must project last(phase)",
                def.name
            );
        }
    }

    #[test]
    fn test_candles_1s_dedup_key_includes_segment() {
        // Prevents cross-segment collision when IDX_I and NSE_EQ share a
        // security_id (e.g., NIFTY index vs NIFTY equity).
        assert!(
            DEDUP_KEY_CANDLES_1S.contains("security_id"),
            "candles_1s DEDUP key must include security_id"
        );
        assert!(
            DEDUP_KEY_CANDLES_1S.contains("segment"),
            "candles_1s DEDUP key must include segment to prevent cross-segment collision"
        );
        assert_eq!(
            DEDUP_KEY_CANDLES_1S, "security_id, segment",
            "exact candles_1s dedup key value"
        );
    }

    #[tokio::test]
    async fn ensure_candle_views_does_not_panic_unreachable() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        // Should not panic — logs warnings and returns.
        ensure_candle_views(&config).await;
    }

    // -----------------------------------------------------------------------
    // build_questdb_exec_url
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_questdb_exec_url_docker() {
        let url = build_questdb_exec_url("tv-questdb", 9000);
        assert_eq!(url, "http://tv-questdb:9000/exec");
    }

    #[test]
    fn test_build_questdb_exec_url_custom() {
        let url = build_questdb_exec_url("10.0.0.1", 19000);
        assert_eq!(url, "http://10.0.0.1:19000/exec");
    }

    // -----------------------------------------------------------------------
    // build_dedup_sql
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_dedup_sql_candles_1s() {
        let sql = build_dedup_sql(QUESTDB_TABLE_CANDLES_1S, DEDUP_KEY_CANDLES_1S);
        assert_eq!(
            sql,
            "ALTER TABLE candles_1s DEDUP ENABLE UPSERT KEYS(ts, security_id, segment)"
        );
    }

    #[test]
    fn test_build_dedup_sql_generic_table() {
        let sql = build_dedup_sql("my_table", "col1, col2");
        assert!(sql.starts_with("ALTER TABLE my_table"));
        assert!(sql.contains("DEDUP ENABLE UPSERT KEYS(ts, col1, col2)"));
    }

    // -----------------------------------------------------------------------
    // view_count / view_names / validate_dependency_order
    // -----------------------------------------------------------------------

    #[test]
    fn test_view_count_is_18() {
        assert_eq!(view_count(), 18);
    }

    #[test]
    fn test_view_names_unique() {
        let names = view_names();
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "view names must be unique");
    }

    #[test]
    fn test_view_names_all_start_with_candles() {
        for name in view_names() {
            assert!(
                name.starts_with("candles_"),
                "name {} must start with candles_",
                name
            );
        }
    }

    #[test]
    fn test_validate_dependency_order_with_correct_base() {
        assert!(validate_dependency_order("candles_1s"));
    }

    #[test]
    fn test_validate_dependency_order_with_wrong_base() {
        // If base table is not candles_1s, first view (candles_5s from candles_1s) fails
        assert!(!validate_dependency_order("wrong_table"));
    }

    // -----------------------------------------------------------------------
    // build_view_sql — exhaustive per-view tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_view_sql_all_views_valid_sql() {
        for def in VIEW_DEFS {
            let sql = build_view_sql(def);
            assert!(
                sql.starts_with("CREATE MATERIALIZED VIEW IF NOT EXISTS"),
                "view {} SQL must start with CREATE MATERIALIZED VIEW",
                def.name
            );
            assert!(
                sql.contains(&format!("FROM {}", def.source)),
                "view {} must SELECT FROM {}",
                def.name,
                def.source
            );
            assert!(
                sql.contains(&format!("SAMPLE BY {}", def.interval)),
                "view {} must SAMPLE BY {}",
                def.name,
                def.interval
            );
        }
    }

    #[test]
    fn test_build_view_sql_sub_minute_views_have_tick_count() {
        // First 5 views (5s, 10s, 15s, 30s, 1m) have tick_count
        for def in &VIEW_DEFS[..5] {
            let sql = build_view_sql(def);
            assert!(
                sql.contains("sum(tick_count)"),
                "sub-minute view {} must aggregate tick_count",
                def.name
            );
        }
    }

    #[test]
    fn test_build_view_sql_minute_plus_views_no_tick_count() {
        // Views from index 5 onward (2m, 3m, ...) do NOT have tick_count
        for def in &VIEW_DEFS[5..] {
            let sql = build_view_sql(def);
            assert!(
                !sql.contains("tick_count"),
                "minute+ view {} must NOT have tick_count",
                def.name
            );
        }
    }

    #[test]
    fn test_build_view_sql_ohlcv_columns_present() {
        for def in VIEW_DEFS {
            let sql = build_view_sql(def);
            assert!(
                sql.contains("first(open) AS open"),
                "view {} missing open",
                def.name
            );
            assert!(
                sql.contains("max(high) AS high"),
                "view {} missing high",
                def.name
            );
            assert!(
                sql.contains("min(low) AS low"),
                "view {} missing low",
                def.name
            );
            assert!(
                sql.contains("last(close) AS close"),
                "view {} missing close",
                def.name
            );
            assert!(
                sql.contains("sum(volume) AS volume"),
                "view {} missing volume",
                def.name
            );
            assert!(
                sql.contains("last(oi) AS oi"),
                "view {} missing oi",
                def.name
            );
            assert!(
                sql.contains("last(iv) AS iv"),
                "view {} missing iv",
                def.name
            );
            assert!(
                sql.contains("last(delta) AS delta"),
                "view {} missing delta",
                def.name
            );
            assert!(
                sql.contains("last(gamma) AS gamma"),
                "view {} missing gamma",
                def.name
            );
            assert!(
                sql.contains("last(theta) AS theta"),
                "view {} missing theta",
                def.name
            );
            assert!(
                sql.contains("last(vega) AS vega"),
                "view {} missing vega",
                def.name
            );
        }
    }

    #[test]
    fn test_build_view_sql_1m_from_1s() {
        let def = VIEW_DEFS.iter().find(|d| d.name == "candles_1m").unwrap();
        assert_eq!(def.source, "candles_1s");
        assert_eq!(def.interval, "1m");
        assert!(def.has_tick_count);
    }

    #[test]
    fn test_build_view_sql_1d_from_30m_for_alignment() {
        // PR #424 source-chain fix: candles_1d previously sourced from
        // `candles_1h` but `candles_1h` is OFFSET '00:15' aligned, which
        // doesn't divide cleanly into a daily bucket aligned at midnight
        // ('00:00'). Sourcing from `candles_30m` (00:00 aligned) instead
        // gives 48× source buckets per day, fully aligned.
        let def = VIEW_DEFS.iter().find(|d| d.name == "candles_1d").unwrap();
        assert_eq!(
            def.source, "candles_30m",
            "candles_1d must source from candles_30m for clean midnight alignment"
        );
        assert_eq!(def.interval, "1d");
        assert!(!def.has_tick_count);
    }

    #[test]
    fn test_hourly_chain_sources_match_pr424_alignment_fix() {
        // PR #424 fix: the hourly chain's source-table choices encode
        // the QuestDB SAMPLE BY alignment constraint that the source's
        // bucket boundaries must divide cleanly into the target's
        // bucket boundaries.
        //
        // - candles_1h:  OFFSET '00:15' → source candles_1s directly.
        //                QuestDB rejects mat-view-of-mat-view when the
        //                offsets disagree (HTTP 400, "base table is not
        //                referenced in materialized view query: candles_15m"
        //                observed live 2026-05-02). The base table
        //                `candles_1s` is one-second-granular, so 1h-at-:15
        //                aggregates a clean 3600× source rows.
        // - candles_2h/3h/4h/1d: OFFSET '00:00' → source candles_30m
        //                (30m boundaries divide cleanly into 2h/3h/4h/1d-at-:00)
        // - candles_7d: OFFSET '00:00', source candles_1d (daily-aligned)
        // - candles_1M: OFFSET '00:00', source candles_1d (daily-aligned)
        let expected: &[(&str, &str)] = &[
            ("candles_1h", "candles_1s"),
            ("candles_2h", "candles_30m"),
            ("candles_3h", "candles_30m"),
            ("candles_4h", "candles_30m"),
            ("candles_1d", "candles_30m"),
            ("candles_7d", "candles_1d"),
            ("candles_1M", "candles_1d"),
        ];
        for (name, expected_source) in expected {
            let def = VIEW_DEFS
                .iter()
                .find(|d| d.name == *name)
                .unwrap_or_else(|| panic!("view {name} not found in VIEW_DEFS"));
            assert_eq!(
                def.source, *expected_source,
                "{name} must source from {expected_source} (PR #424 alignment fix)"
            );
        }
    }

    #[test]
    fn test_extract_questdb_error_returns_quoted_error_field() {
        let body = r#"{"query":"CREATE MATERIALIZED VIEW...","error":"unsupported SAMPLE BY interval","position":42}"#;
        assert_eq!(
            extract_questdb_error(body),
            "unsupported SAMPLE BY interval"
        );
    }

    #[test]
    fn test_extract_questdb_error_falls_back_to_truncated_body_on_no_marker() {
        let body = "<html>500 Internal Server Error</html>";
        let extracted = extract_questdb_error(body);
        assert_eq!(extracted, "<html>500 Internal Server Error</html>");
    }

    #[test]
    fn test_extract_questdb_error_handles_empty_body() {
        assert_eq!(extract_questdb_error(""), "");
    }

    #[test]
    fn test_extract_questdb_error_caps_fallback_length() {
        let huge = "x".repeat(2000);
        let extracted = extract_questdb_error(&huge);
        assert!(extracted.chars().count() <= 800);
    }

    #[test]
    fn test_build_view_sql_monthly_from_1d() {
        let def = VIEW_DEFS.iter().find(|d| d.name == "candles_1M").unwrap();
        assert_eq!(def.source, "candles_1d");
        assert_eq!(def.interval, "1M");
    }

    #[test]
    fn test_build_view_sql_all_views_idempotent() {
        for def in VIEW_DEFS {
            let sql = build_view_sql(def);
            assert!(
                sql.contains("IF NOT EXISTS"),
                "view {} SQL must contain IF NOT EXISTS for idempotency",
                def.name
            );
        }
    }

    #[test]
    fn test_no_view_sources_from_itself() {
        for def in VIEW_DEFS {
            assert_ne!(
                def.name, def.source,
                "view {} must not source from itself",
                def.name
            );
        }
    }

    #[test]
    fn test_candles_1s_ddl_idempotent() {
        assert!(
            CANDLES_1S_CREATE_DDL.contains("IF NOT EXISTS"),
            "base table DDL must contain IF NOT EXISTS for idempotent startup"
        );
    }

    #[test]
    fn test_view_chain_5s_to_1m_all_from_1s() {
        let sub_minute = [
            "candles_5s",
            "candles_10s",
            "candles_15s",
            "candles_30s",
            "candles_1m",
        ];
        for name in &sub_minute {
            let def = VIEW_DEFS.iter().find(|d| d.name == *name).unwrap();
            assert_eq!(
                def.source, "candles_1s",
                "sub-minute view {} must source from candles_1s",
                name
            );
        }
    }

    #[test]
    fn test_view_chain_2m_3m_5m_from_1m() {
        let minute_views = ["candles_2m", "candles_3m", "candles_5m"];
        for name in &minute_views {
            let def = VIEW_DEFS.iter().find(|d| d.name == *name).unwrap();
            assert_eq!(
                def.source, "candles_1m",
                "minute view {} must source from candles_1m",
                name
            );
        }
    }

    #[test]
    fn test_view_intervals_non_empty() {
        for def in VIEW_DEFS {
            assert!(
                !def.interval.is_empty(),
                "view {} must have a non-empty interval",
                def.name
            );
        }
    }

    #[test]
    fn test_build_view_sql_security_id_in_select() {
        for def in VIEW_DEFS {
            let sql = build_view_sql(def);
            assert!(
                sql.contains("security_id"),
                "view {} must include security_id in SELECT",
                def.name
            );
        }
    }

    #[test]
    fn test_build_view_sql_ts_in_select() {
        for def in VIEW_DEFS {
            let sql = build_view_sql(def);
            assert!(
                sql.contains("ts"),
                "view {} must include ts in SELECT",
                def.name
            );
        }
    }

    #[test]
    fn test_ddl_timeout_is_reasonable() {
        assert!((5..=30).contains(&DDL_TIMEOUT_SECS));
    }

    #[test]
    fn test_candles_1s_ddl_no_semicolons() {
        assert!(
            !CANDLES_1S_CREATE_DDL.contains(';'),
            "DDL must not contain semicolons"
        );
    }

    #[test]
    fn test_candles_1s_ddl_has_wal() {
        assert!(CANDLES_1S_CREATE_DDL.contains("WAL"));
    }

    #[test]
    fn test_candles_1s_ddl_has_oi_column() {
        assert!(CANDLES_1S_CREATE_DDL.contains("oi LONG"));
    }

    #[test]
    fn test_candles_1s_ddl_has_volume_column() {
        assert!(CANDLES_1S_CREATE_DDL.contains("volume LONG"));
    }

    #[test]
    fn test_candles_1s_ddl_has_ohlc_columns() {
        assert!(CANDLES_1S_CREATE_DDL.contains("open DOUBLE"));
        assert!(CANDLES_1S_CREATE_DDL.contains("high DOUBLE"));
        assert!(CANDLES_1S_CREATE_DDL.contains("low DOUBLE"));
        assert!(CANDLES_1S_CREATE_DDL.contains("close DOUBLE"));
    }

    #[test]
    fn test_candles_1s_ddl_has_greeks_columns() {
        assert!(CANDLES_1S_CREATE_DDL.contains("iv DOUBLE"));
        assert!(CANDLES_1S_CREATE_DDL.contains("delta DOUBLE"));
        assert!(CANDLES_1S_CREATE_DDL.contains("gamma DOUBLE"));
        assert!(CANDLES_1S_CREATE_DDL.contains("theta DOUBLE"));
        assert!(CANDLES_1S_CREATE_DDL.contains("vega DOUBLE"));
    }

    // -----------------------------------------------------------------------
    // HTTP mock helpers
    // -----------------------------------------------------------------------

    const MOCK_HTTP_200: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
    const MOCK_HTTP_400: &str = "HTTP/1.1 400 Bad Request\r\nContent-Length: 31\r\n\r\n{\"error\":\"table does not exist\"}";

    async fn spawn_mock_http_server(response: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            }
        });
        port
    }

    // -----------------------------------------------------------------------
    // ensure_candle_views — HTTP success/failure paths (covers lines 291-336)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_candle_views_all_200() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises success path: create table (line 307), DEDUP (310-311),
        // all 18 view DDLs (315-319), final info log (322-325)
        ensure_candle_views(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_candle_views_all_400() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises non-success path: create table returns false → early return (line 307)
        // execute_ddl returns false for non-success (lines 335-336, 343, 349-351)
        ensure_candle_views(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_candle_views_create_send_error_returns_early() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises execute_ddl Err branch (lines 349-351)
        ensure_candle_views(&config).await;
    }

    // -----------------------------------------------------------------------
    // execute_ddl — direct tests for success/failure/error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_execute_ddl_success_returns_true() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        tokio::task::yield_now().await;
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        let client = reqwest::Client::new();
        let result = execute_ddl(&client, &base_url, "SELECT 1", "test_label").await;
        assert!(result, "execute_ddl must return true on 200");
    }

    #[tokio::test]
    async fn test_execute_ddl_non_success_returns_false() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        let client = reqwest::Client::new();
        let result = execute_ddl(&client, &base_url, "BAD SQL", "test_label").await;
        assert!(!result, "execute_ddl must return false on non-success");
    }

    #[tokio::test]
    async fn test_execute_ddl_send_error_returns_false() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        tokio::task::yield_now().await;
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        let client = reqwest::Client::new();
        let result = execute_ddl(&client, &base_url, "SELECT 1", "test_label").await;
        assert!(!result, "execute_ddl must return false on send error");
    }

    #[tokio::test]
    async fn test_execute_ddl_200_with_error_body_returns_false() {
        // QuestDB can return HTTP 200 with error in body — execute_ddl must detect this.
        let response_with_error = "HTTP/1.1 200 OK\r\nContent-Length: 42\r\n\r\n{\"error\":\"source table candles_1s not found\"}";
        let port = spawn_mock_http_server(response_with_error).await;
        tokio::task::yield_now().await;
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        let client = reqwest::Client::new();
        let result = execute_ddl(&client, &base_url, "CREATE VIEW test", "test_label").await;
        assert!(
            !result,
            "execute_ddl must return false when body contains error despite HTTP 200"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: tracing subscriber to evaluate warn!/info! fields
    // (lines 325, 343)
    // -----------------------------------------------------------------------

    fn install_test_subscriber() -> tracing::subscriber::DefaultGuard {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_test_writer());
        tracing::subscriber::set_default(subscriber)
    }

    #[tokio::test]
    async fn test_ensure_candle_views_all_200_with_tracing() {
        let _guard = install_test_subscriber();
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // With subscriber installed, info! evaluates views_total field (line 325).
        ensure_candle_views(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_candle_views_all_400_with_tracing() {
        let _guard = install_test_subscriber();
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // With subscriber installed, warn! evaluates body.chars().take(200) (line 343).
        ensure_candle_views(&config).await;
    }

    // -----------------------------------------------------------------------
    // Greeks schema migration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_greeks_column_names_constant() {
        assert_eq!(GREEKS_COLUMN_NAMES.len(), 5);
        assert!(GREEKS_COLUMN_NAMES.contains(&"iv"));
        assert!(GREEKS_COLUMN_NAMES.contains(&"delta"));
        assert!(GREEKS_COLUMN_NAMES.contains(&"gamma"));
        assert!(GREEKS_COLUMN_NAMES.contains(&"theta"));
        assert!(GREEKS_COLUMN_NAMES.contains(&"vega"));
    }

    #[test]
    fn test_all_views_include_greeks_in_sql() {
        // Every materialized view SQL must include Greeks aggregations.
        for def in VIEW_DEFS {
            let sql = build_view_sql(def);
            assert!(
                sql.contains("last(iv) AS iv"),
                "view {} must include iv",
                def.name
            );
            assert!(
                sql.contains("last(delta) AS delta"),
                "view {} must include delta",
                def.name
            );
            assert!(
                sql.contains("last(gamma) AS gamma"),
                "view {} must include gamma",
                def.name
            );
            assert!(
                sql.contains("last(theta) AS theta"),
                "view {} must include theta",
                def.name
            );
            assert!(
                sql.contains("last(vega) AS vega"),
                "view {} must include vega",
                def.name
            );
        }
    }

    #[tokio::test]
    async fn test_views_missing_greeks_returns_false_on_unreachable() {
        // When QuestDB is unreachable, views_missing_greeks returns false
        // (conservative — let CREATE IF NOT EXISTS handle it).
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let base_url = "http://127.0.0.1:1/exec";
        let result = views_missing_greeks(&client, base_url).await;
        assert!(!result);
    }

    #[tokio::test]
    async fn test_views_missing_greeks_returns_false_when_iv_present() {
        // Mock server returns a response containing "iv" — views have Greeks.
        // Body = 81 bytes.
        let response_with_iv = "HTTP/1.1 200 OK\r\nContent-Length: 81\r\n\r\n{\"columns\":[{\"name\":\"ts\"},{\"name\":\"security_id\"},{\"name\":\"iv\"},{\"name\":\"delta\"}]}";
        let port = spawn_mock_http_server(response_with_iv).await;
        tokio::task::yield_now().await;
        let client = reqwest::Client::new();
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        let result = views_missing_greeks(&client, &base_url).await;
        assert!(!result, "should return false when iv column is present");
    }

    #[tokio::test]
    async fn test_views_missing_greeks_returns_true_when_iv_absent() {
        // Mock server returns a response WITHOUT "iv" — views need recreation.
        // Body = 66 bytes.
        let response_without_iv = "HTTP/1.1 200 OK\r\nContent-Length: 66\r\n\r\n{\"columns\":[{\"name\":\"ts\"},{\"name\":\"security_id\"},{\"name\":\"open\"}]}";
        let port = spawn_mock_http_server(response_without_iv).await;
        tokio::task::yield_now().await;
        let client = reqwest::Client::new();
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        let result = views_missing_greeks(&client, &base_url).await;
        assert!(result, "should return true when iv column is absent");
    }

    /// Phase 1 ratchet (29-timeframes engine plan): base-table probe must
    /// return `true` when `candles_1s` lacks the `phase` column. Defends
    /// against the silent-ALTER scenario where `drop(client.send().await)`
    /// swallows a network/HTTP failure.
    #[tokio::test]
    async fn test_base_missing_phase1_columns_returns_true_when_phase_absent() {
        let response_without_phase = "HTTP/1.1 200 OK\r\nContent-Length: 70\r\n\r\n{\"columns\":[{\"name\":\"ts\"},{\"name\":\"security_id\"},{\"name\":\"close\"},{\"name\":\"iv\"}]}";
        let port = spawn_mock_http_server(response_without_phase).await;
        tokio::task::yield_now().await;
        let client = reqwest::Client::new();
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        let result = base_missing_phase1_columns(&client, &base_url).await;
        assert!(
            result,
            "should return true when candles_1s lacks the phase column"
        );
    }

    /// Phase 1 ratchet: base-table probe returns `false` when `phase`
    /// is already present (no rebuild needed).
    #[tokio::test]
    async fn test_base_missing_phase1_columns_returns_false_when_phase_present() {
        let response_with_phase = "HTTP/1.1 200 OK\r\nContent-Length: 81\r\n\r\n{\"columns\":[{\"name\":\"ts\"},{\"name\":\"security_id\"},{\"name\":\"close\"},{\"name\":\"phase\"}]}";
        let port = spawn_mock_http_server(response_with_phase).await;
        tokio::task::yield_now().await;
        let client = reqwest::Client::new();
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        let result = base_missing_phase1_columns(&client, &base_url).await;
        assert!(
            !result,
            "should return false when candles_1s already has the phase column"
        );
    }

    /// Phase 1 ratchet: probe is robust to network failure — returns
    /// `false` on unreachable host (the next boot's idempotent ALTER
    /// will retry, so a one-shot transient won't block startup).
    #[tokio::test]
    async fn test_base_missing_phase1_columns_returns_false_on_unreachable() {
        let client = reqwest::Client::new();
        let base_url = "http://127.0.0.1:1/exec";
        let result = base_missing_phase1_columns(&client, base_url).await;
        assert!(!result);
    }

    /// Phase 1 ratchet: HTTP 400 (view/table doesn't exist) returns
    /// `false`. Step 1 / Step 5 handle the missing-table case via
    /// `CREATE ... IF NOT EXISTS`.
    #[tokio::test]
    async fn test_base_missing_phase1_columns_returns_false_on_non_success() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let client = reqwest::Client::new();
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        let result = base_missing_phase1_columns(&client, &base_url).await;
        assert!(!result);
    }

    /// Phase 1 ratchet: the substring probe `"\"phase\""` does NOT
    /// false-positive on column names that contain `phase` as a
    /// non-quoted substring (e.g. `prev_phase`).
    #[tokio::test]
    async fn test_base_missing_phase1_columns_no_false_positive_on_substring() {
        // Body contains `"prev_phase"` but no standalone `"phase"`.
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 73\r\n\r\n{\"columns\":[{\"name\":\"ts\"},{\"name\":\"security_id\"},{\"name\":\"prev_phase\"}]}";
        let port = spawn_mock_http_server(response).await;
        tokio::task::yield_now().await;
        let client = reqwest::Client::new();
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        let result = base_missing_phase1_columns(&client, &base_url).await;
        assert!(
            result,
            "must return true (rebuild needed) when phase is missing — \
             prev_phase substring should NOT false-match"
        );
    }

    #[tokio::test]
    async fn test_views_missing_greeks_returns_false_on_non_success() {
        // Mock returns 400 (view doesn't exist) — return false, let CREATE handle it.
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let client = reqwest::Client::new();
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        let result = views_missing_greeks(&client, &base_url).await;
        assert!(!result);
    }

    #[tokio::test]
    async fn test_drop_all_views_with_mock_200() {
        // DROP MATERIALIZED VIEW IF EXISTS requests all return 200 — no panic.
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        tokio::task::yield_now().await;
        let client = reqwest::Client::new();
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        drop_all_views(&client, &base_url).await;
    }

    #[tokio::test]
    async fn test_drop_all_views_with_unreachable_no_panic() {
        // When QuestDB is unreachable, drop_all_views must not panic.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let base_url = "http://127.0.0.1:1/exec";
        drop_all_views(&client, base_url).await;
    }

    #[tokio::test]
    async fn test_ensure_greeks_columns_on_table_with_mock_200() {
        // ALTER TABLE ADD COLUMN requests all return 200 — no panic.
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        tokio::task::yield_now().await;
        let client = reqwest::Client::new();
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        ensure_greeks_columns_on_table(&client, &base_url, "test_table").await;
    }

    #[tokio::test]
    async fn test_ensure_greeks_columns_on_table_with_unreachable_no_panic() {
        // When QuestDB is unreachable, ensure_greeks_columns_on_table must not panic.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let base_url = "http://127.0.0.1:1/exec";
        ensure_greeks_columns_on_table(&client, base_url, "test_table").await;
    }

    // -----------------------------------------------------------------------
    // verify_views_exist tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_match_critical_views_count() {
        assert_eq!(
            CROSS_MATCH_CRITICAL_VIEWS.len(),
            5,
            "must have 5 critical views for cross-match"
        );
    }

    #[test]
    fn test_cross_match_critical_views_are_defined() {
        // Every critical view must exist in VIEW_DEFS
        let all_names: Vec<&str> = VIEW_DEFS.iter().map(|d| d.name).collect();
        for &critical in CROSS_MATCH_CRITICAL_VIEWS {
            assert!(
                all_names.contains(&critical),
                "critical view {critical} must exist in VIEW_DEFS"
            );
        }
    }

    #[test]
    fn test_cross_match_critical_views_exact_names() {
        assert_eq!(CROSS_MATCH_CRITICAL_VIEWS[0], "candles_1m");
        assert_eq!(CROSS_MATCH_CRITICAL_VIEWS[1], "candles_5m");
        assert_eq!(CROSS_MATCH_CRITICAL_VIEWS[2], "candles_15m");
        assert_eq!(CROSS_MATCH_CRITICAL_VIEWS[3], "candles_1h");
        assert_eq!(CROSS_MATCH_CRITICAL_VIEWS[4], "candles_1d");
    }

    #[tokio::test]
    async fn test_verify_views_exist_with_mock_200_no_panic() {
        // When QuestDB returns 200 for all queries, verify_views_exist should not panic.
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        tokio::task::yield_now().await;
        let client = reqwest::Client::new();
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        verify_views_exist(&client, &base_url).await;
    }

    #[tokio::test]
    async fn test_verify_views_exist_with_mock_400_no_panic() {
        // When QuestDB returns 400 for all queries, verify_views_exist logs errors but no panic.
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let client = reqwest::Client::new();
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        verify_views_exist(&client, &base_url).await;
    }

    #[tokio::test]
    async fn test_verify_views_exist_with_unreachable_no_panic() {
        // When QuestDB is unreachable, verify_views_exist must not panic.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let base_url = "http://127.0.0.1:1/exec";
        verify_views_exist(&client, base_url).await;
    }

    #[tokio::test]
    async fn test_verify_views_exist_with_tracing() {
        // With subscriber installed, verify info!/warn!/error! fields evaluate.
        let _guard = install_test_subscriber();
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        tokio::task::yield_now().await;
        let client = reqwest::Client::new();
        let base_url = build_questdb_exec_url("127.0.0.1", port);
        verify_views_exist(&client, &base_url).await;
    }
}
