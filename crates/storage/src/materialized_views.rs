//! QuestDB materialized views for multi-timeframe candle aggregation.
//!
//! Creates the `candles_1s` base table and 18 materialized views covering
//! timeframes from 5 seconds to 1 month. All views use IST-aligned candle
//! boundaries via `ALIGN TO CALENDAR WITH OFFSET '05:30'`.
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

/// DEDUP UPSERT KEY column for the candles_1s table.
const DEDUP_KEY_CANDLES_1S: &str = "security_id";

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

/// Builds the CREATE MATERIALIZED VIEW SQL for a given view definition.
///
/// Uses IST-aligned candle boundaries via ALIGN TO CALENDAR WITH OFFSET '05:30'.
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
         sum(volume) AS volume, last(oi) AS oi{tick_count} \
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
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "failed to build HTTP client for candle view DDL");
            return;
        }
    };

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
    let dedup_sql = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_CANDLES_1S, DEDUP_KEY_CANDLES_1S
    );
    execute_ddl(&client, &base_url, &dedup_sql, "candles_1s DEDUP").await;

    // Step 3: Create materialized views in dependency order.
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
        let def = &VIEW_DEFS[0]; // candles_5s
        let sql = build_view_sql(def);
        assert!(sql.contains("ALIGN TO CALENDAR WITH OFFSET '05:30'"));
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
}
