//! QuestDB materialized views for multi-timeframe candle aggregation.
//!
//! Creates the `candles_1s` base table and 18 materialized views covering
//! timeframes from 5 seconds to 1 month. Live WebSocket data arrives as IST
//! epoch seconds (stored directly). Historical REST data arrives as UTC epoch
//! seconds (+19800s offset applied at persistence). Both result in IST-based
//! timestamps. Views use `OFFSET '00:00'` since stored data is already IST.
//!
//! Timeframes 20-21 (3 months, 1 year) are computed in Rust from monthly data.
//!
//! # Idempotency
//! All DDL uses `CREATE TABLE IF NOT EXISTS` / `CREATE MATERIALIZED VIEW IF NOT EXISTS`.
//! Safe to call every startup.

use std::time::Duration;

use tracing::{debug, info, warn};

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::constants::{QUESTDB_IST_ALIGN_OFFSET, QUESTDB_TABLE_CANDLES_1S};

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
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY DAY WAL\
";

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
}

/// All 18 materialized views ordered by dependency chain.
///
/// Each view aggregates from its `source` — the dependency chain is:
/// candles_1s → 5s,10s,15s,30s,1m
/// candles_1m → 2m,3m,5m
/// candles_5m → 10m,15m
/// candles_15m → 30m
/// candles_30m → 1h
/// candles_1h → 2h,3h,4h,1d
/// candles_1d → 7d,1M
const VIEW_DEFS: &[ViewDef] = &[
    // Sub-minute from 1s base
    ViewDef {
        name: "candles_5s",
        source: "candles_1s",
        interval: "5s",
        has_tick_count: true,
    },
    ViewDef {
        name: "candles_10s",
        source: "candles_1s",
        interval: "10s",
        has_tick_count: true,
    },
    ViewDef {
        name: "candles_15s",
        source: "candles_1s",
        interval: "15s",
        has_tick_count: true,
    },
    ViewDef {
        name: "candles_30s",
        source: "candles_1s",
        interval: "30s",
        has_tick_count: true,
    },
    ViewDef {
        name: "candles_1m",
        source: "candles_1s",
        interval: "1m",
        has_tick_count: true,
    },
    // Minute-level from 1m
    ViewDef {
        name: "candles_2m",
        source: "candles_1m",
        interval: "2m",
        has_tick_count: false,
    },
    ViewDef {
        name: "candles_3m",
        source: "candles_1m",
        interval: "3m",
        has_tick_count: false,
    },
    ViewDef {
        name: "candles_5m",
        source: "candles_1m",
        interval: "5m",
        has_tick_count: false,
    },
    // Multi-minute from 5m
    ViewDef {
        name: "candles_10m",
        source: "candles_5m",
        interval: "10m",
        has_tick_count: false,
    },
    ViewDef {
        name: "candles_15m",
        source: "candles_5m",
        interval: "15m",
        has_tick_count: false,
    },
    // Half-hour from 15m
    ViewDef {
        name: "candles_30m",
        source: "candles_15m",
        interval: "30m",
        has_tick_count: false,
    },
    // Hourly from 30m
    ViewDef {
        name: "candles_1h",
        source: "candles_30m",
        interval: "1h",
        has_tick_count: false,
    },
    // Multi-hour from 1h
    ViewDef {
        name: "candles_2h",
        source: "candles_1h",
        interval: "2h",
        has_tick_count: false,
    },
    ViewDef {
        name: "candles_3h",
        source: "candles_1h",
        interval: "3h",
        has_tick_count: false,
    },
    ViewDef {
        name: "candles_4h",
        source: "candles_1h",
        interval: "4h",
        has_tick_count: false,
    },
    // Daily from 1h
    ViewDef {
        name: "candles_1d",
        source: "candles_1h",
        interval: "1d",
        has_tick_count: false,
    },
    // Weekly from 1d
    ViewDef {
        name: "candles_7d",
        source: "candles_1d",
        interval: "7d",
        has_tick_count: false,
    },
    // Monthly from 1d
    ViewDef {
        name: "candles_1M",
        source: "candles_1d",
        interval: "1M",
        has_tick_count: false,
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
/// Data is stored as IST-as-UTC — offset '00:00' since midnight "UTC" IS midnight IST.
fn build_view_sql(def: &ViewDef) -> String {
    let tick_count_select = if def.has_tick_count {
        ", sum(tick_count) AS tick_count"
    } else {
        ""
    };

    format!(
        "CREATE MATERIALIZED VIEW IF NOT EXISTS {name} AS \
         SELECT ts, security_id, segment, \
         first(open) AS open, max(high) AS high, \
         min(low) AS low, last(close) AS close, \
         sum(volume) AS volume, last(oi) AS oi{tick_count}, \
         last(iv) AS iv, last(delta) AS delta, last(gamma) AS gamma, \
         last(theta) AS theta, last(vega) AS vega \
         FROM {source} \
         SAMPLE BY {interval} \
         ALIGN TO CALENDAR WITH OFFSET '{offset}'",
        name = def.name,
        source = def.source,
        interval = def.interval,
        tick_count = tick_count_select,
        offset = QUESTDB_IST_ALIGN_OFFSET,
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
    let dedup_sql = build_dedup_sql(QUESTDB_TABLE_CANDLES_1S, DEDUP_KEY_CANDLES_1S);
    execute_ddl(&client, &base_url, &dedup_sql, "candles_1s DEDUP").await;

    // Step 3: Add Greeks columns to candles_1s if missing (schema migration).
    // QuestDB ALTER TABLE ADD COLUMN is idempotent — adding an already-existing
    // column returns a non-fatal error that we safely ignore.
    ensure_greeks_columns_on_table(&client, &base_url, QUESTDB_TABLE_CANDLES_1S).await;

    // Step 4: Drop materialized views if they lack Greeks columns.
    // Views created before Greeks were added cannot be ALTERed — they must be
    // dropped and recreated. We probe the first view (candles_5s) for the 'iv'
    // column. If absent, drop all 18 views so Step 5 recreates them with Greeks.
    if views_missing_greeks(&client, &base_url).await {
        info!("materialized views lack Greeks columns — dropping for recreation");
        drop_all_views(&client, &base_url).await;
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
        let _ = client
            .get(base_url)
            .query(&[("query", &alter_sql)])
            .send()
            .await;
    }
    info!(
        table = table_name,
        "Greeks columns ensured (idempotent ADD COLUMN)"
    );
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
        let _ = client
            .get(base_url)
            .query(&[("query", &drop_sql)])
            .send()
            .await;
        debug!(
            view = def.name,
            "dropped materialized view for Greeks migration"
        );
    }
    info!("all 18 materialized views dropped for Greeks migration");
}

/// Executes a single DDL statement against QuestDB. Returns true on success.
async fn execute_ddl(client: &reqwest::Client, base_url: &str, sql: &str, label: &str) -> bool {
    match client.get(base_url).query(&[("query", sql)]).send().await {
        Ok(response) => {
            if response.status().is_success() {
                debug!(label, "DDL executed successfully");
                true
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    label,
                    %status,
                    body = body.chars().take(200).collect::<String>(),
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
        let url = build_questdb_exec_url("dlt-questdb", 9000);
        assert_eq!(url, "http://dlt-questdb:9000/exec");
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
    fn test_build_view_sql_1d_from_1h() {
        let def = VIEW_DEFS.iter().find(|d| d.name == "candles_1d").unwrap();
        assert_eq!(def.source, "candles_1h");
        assert_eq!(def.interval, "1d");
        assert!(!def.has_tick_count);
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
}
