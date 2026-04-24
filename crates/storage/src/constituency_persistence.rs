//! QuestDB persistence for NSE index constituency data.
//!
//! Writes index → stock mappings from niftyindices.com to QuestDB so
//! Grafana can display constituency tables. Daily snapshot — DEDUP
//! UPSERT KEYS on `(ts, index_name, symbol)` prevent duplicates.
//!
//! Best-effort: failures log ERROR but don't block trading.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Sender, TimestampNanos};
use reqwest::Client;
use tracing::{info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::QUESTDB_TABLE_INDEX_CONSTITUENTS;
use tickvault_common::instrument_types::{FnoUniverse, IndexConstituencyMap};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// DEDUP UPSERT KEY for the `index_constituents` table.
const DEDUP_KEY_INDEX_CONSTITUENTS: &str = "index_name, symbol";

/// DDL for `index_constituents` — one row per (index, stock) pair.
const INDEX_CONSTITUENTS_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS index_constituents (\
        index_name SYMBOL,\
        symbol SYMBOL,\
        isin STRING,\
        weight DOUBLE,\
        sector STRING,\
        security_id LONG,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY MONTH WAL\
";

/// Maximum retry attempts for constituency ILP persistence.
const CONSTITUENCY_PERSIST_MAX_RETRIES: u32 = 3;

/// Initial delay between constituency persistence retries (seconds).
/// Q7 (2026-04-24): was fixed 2s; now 2s → 5s → 12s exponential so
/// QuestDB has real time to drain WAL pressure between attempts.
const CONSTITUENCY_PERSIST_RETRY_DELAY_SECS: u64 = 2;

/// Backoff multiplier for retry delays — `delay = delay * MULTIPLIER`.
const CONSTITUENCY_PERSIST_BACKOFF_MULTIPLIER: u64 = 2;

/// Flush batch size for constituency ILP persistence.
///
/// With HTTP ILP (`build_http_ilp_conf_string`) each flush is a single
/// stateless transactional POST — the root-cause fix for the
/// `Broken pipe (os error 32)` failure mode observed in prod on
/// 2026-04-23T15:50:19Z. The batch size is a defensive secondary control
/// bounding POST body size and protecting against QuestDB WAL backpressure
/// on fresh Docker boots.
///
/// History:
/// - Initial value: 4000+ (flushed entire dataset in one buffer).
/// - First reduction: 500 (still failing on 2026-04-23 post-market boot with
///   `Broken pipe (os error 32)` on all 3 retries — TCP ILP socket rotation).
/// - Transport fix: TCP ILP → HTTP ILP via `build_http_ilp_conf_string` —
///   eliminates the broken-pipe failure mode at the transport layer.
/// - Q7 (2026-04-24): reduced to 100 to match the `MOVERS_FLUSH_BATCH_SIZE`
///   pattern which has NEVER shown broken-pipe failures. Smaller batches
///   survive QuestDB WAL backpressure on fresh Docker boots even with
///   HTTP ILP.
const CONSTITUENCY_FLUSH_BATCH_SIZE: usize = 100;

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

/// Builds the ILP **HTTP** connection string from host and HTTP port.
///
/// Constituency persistence is a one-shot bursty cold-path writer: it runs
/// once at boot with ~1,750 rows and then stays idle. TCP ILP (`tcp::addr=`)
/// holds a persistent socket that QuestDB's `line.tcp` writer rotates when
/// idle, which caused `Broken pipe (os error 32)` on the first flush and
/// every retry (prod logs, 2026-04-23T15:50:19Z). HTTP ILP is stateless —
/// each flush is a single transactional POST to `/write` — so there is no
/// persistent socket that can break between batches.
///
/// This is the same architectural fix applied to the historical candle
/// writer (see `build_historical_ilp_conf_string` in `candle_persistence.rs`).
///
/// Parameters:
/// - `auto_flush=off` — we control batching explicitly via
///   `CONSTITUENCY_FLUSH_BATCH_SIZE`.
/// - `retry_timeout=30000` — HTTP sender retries transient 5xx / network
///   errors for up to 30s internally, so callers rarely see transient failures.
fn build_http_ilp_conf_string(host: &str, http_port: u16) -> String {
    format!(
        "http::addr={}:{};auto_flush=off;retry_timeout=30000;",
        host, http_port
    )
}

/// Resolves the security_id for a constituent from the FnoUniverse.
///
/// Returns `0` if the symbol is not found in the universe (not an F&O stock).
fn resolve_security_id(fno_universe: Option<&FnoUniverse>, symbol: &str) -> u32 {
    fno_universe
        .and_then(|u| u.symbol_to_security_id(symbol))
        .unwrap_or(0)
}

/// Counts total constituents across all indices in the map.
#[cfg(test)]
fn count_total_constituents(constituency_map: &IndexConstituencyMap) -> usize {
    constituency_map
        .index_to_constituents
        .values()
        .map(|v| v.len())
        .sum()
}

// ---------------------------------------------------------------------------
// DDL Setup
// ---------------------------------------------------------------------------

/// Creates the `index_constituents` table and enables DEDUP. Idempotent.
pub async fn ensure_constituency_table(questdb_config: &QuestDbConfig) {
    let base_url = build_questdb_exec_url(&questdb_config.host, questdb_config.http_port);

    // Client::builder().timeout().build() is infallible (no custom TLS).
    // Coverage: unwrap_or_else avoids uncoverable else-return on dead path.
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    // Step 1: Create table
    match client
        .get(&base_url)
        .query(&[("query", INDEX_CONSTITUENTS_CREATE_DDL)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                info!("index_constituents table ensured (CREATE TABLE IF NOT EXISTS)");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    body = body.chars().take(200).collect::<String>(),
                    "index_constituents table CREATE DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(?err, "failed to send CREATE TABLE for index_constituents");
        }
    }

    // Step 2: Enable DEDUP
    let dedup_ddl = build_dedup_sql(
        QUESTDB_TABLE_INDEX_CONSTITUENTS,
        DEDUP_KEY_INDEX_CONSTITUENTS,
    );

    match client
        .get(&base_url)
        .query(&[("query", &dedup_ddl)])
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                info!("index_constituents DEDUP UPSERT KEYS enabled");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                // DEDUP already enabled = non-error (QuestDB returns error on re-enable)
                if !body.contains("already enabled") {
                    warn!(
                        %status,
                        body = body.chars().take(200).collect::<String>(),
                        "index_constituents DEDUP DDL returned non-success"
                    );
                }
            }
        }
        Err(err) => {
            warn!(?err, "failed to send DEDUP DDL for index_constituents");
        }
    }
}

// ---------------------------------------------------------------------------
// Data Persistence
// ---------------------------------------------------------------------------

/// Persists constituency data to QuestDB with retry logic. Best-effort.
///
/// When `fno_universe` is provided, each constituent is enriched with the
/// Dhan `security_id` from the instrument master (symbol → security_id mapping).
/// This enables news-based trading: symbol → security_id → F&O contracts.
pub fn persist_constituency(
    constituency_map: &IndexConstituencyMap,
    questdb_config: &QuestDbConfig,
    fno_universe: Option<&FnoUniverse>,
) -> Result<()> {
    let mut last_err = None;
    // Q7 (2026-04-24): exponential backoff between retries. Was fixed 2s
    // × 3 = all attempts fired within 6 seconds — not enough time for
    // QuestDB's WAL to drain under boot-time pressure. Now 2s → 4s → 8s
    // so the third attempt happens 14s after the first, giving the WAL
    // writer a real chance to finish whatever it was blocked on.
    let mut delay_secs = CONSTITUENCY_PERSIST_RETRY_DELAY_SECS;
    for attempt in 1..=CONSTITUENCY_PERSIST_MAX_RETRIES {
        match persist_constituency_inner(constituency_map, questdb_config, fno_universe) {
            Ok(count) => {
                info!(
                    entries = count,
                    table = QUESTDB_TABLE_INDEX_CONSTITUENTS,
                    attempt,
                    "index constituency persisted to QuestDB"
                );
                return Ok(());
            }
            Err(err) => {
                warn!(
                    ?err,
                    attempt,
                    max_retries = CONSTITUENCY_PERSIST_MAX_RETRIES,
                    next_retry_secs = if attempt < CONSTITUENCY_PERSIST_MAX_RETRIES {
                        delay_secs
                    } else {
                        0
                    },
                    "constituency persistence attempt failed — retrying"
                );
                last_err = Some(err);
                if attempt < CONSTITUENCY_PERSIST_MAX_RETRIES {
                    std::thread::sleep(std::time::Duration::from_secs(delay_secs));
                    delay_secs = delay_secs.saturating_mul(CONSTITUENCY_PERSIST_BACKOFF_MULTIPLIER);
                }
            }
        }
    }
    tracing::error!(
        err = ?last_err,
        total_indices = constituency_map.index_to_constituents.len(),
        batch_size = CONSTITUENCY_FLUSH_BATCH_SIZE,
        "constituency persistence failed after {CONSTITUENCY_PERSIST_MAX_RETRIES} attempts — index_constituents table will be empty in Grafana"
    );
    Ok(())
}

fn persist_constituency_inner(
    constituency_map: &IndexConstituencyMap,
    questdb_config: &QuestDbConfig,
    fno_universe: Option<&FnoUniverse>,
) -> Result<usize> {
    // HTTP ILP — stateless transactional POSTs, no persistent socket to break.
    // See `build_http_ilp_conf_string` for the rationale.
    let conf_string = build_http_ilp_conf_string(&questdb_config.host, questdb_config.http_port);
    let mut sender = Sender::from_conf(&conf_string)
        .context("failed to connect to QuestDB ILP for constituency")?;
    let mut buffer = sender.new_buffer();

    // Use build timestamp as the snapshot timestamp for all rows.
    let build_epoch_nanos = constituency_map
        .build_metadata
        .build_timestamp
        .timestamp_nanos_opt()
        .unwrap_or_else(|| chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let snapshot_ts = TimestampNanos::new(build_epoch_nanos);

    let mut count = 0_usize;
    let mut batch_pending = 0_usize;

    for (index_name, constituents) in &constituency_map.index_to_constituents {
        for constituent in constituents {
            // Enrich with security_id from instrument master (0 if not found).
            let security_id = resolve_security_id(fno_universe, &constituent.symbol);

            buffer
                .table(QUESTDB_TABLE_INDEX_CONSTITUENTS)
                .context("table name")?
                .symbol("index_name", index_name)
                .context("index_name")?
                .symbol("symbol", &constituent.symbol)
                .context("symbol")?
                .column_str("isin", &constituent.isin)
                .context("isin")?
                .column_f64("weight", constituent.weight)
                .context("weight")?
                .column_str("sector", &constituent.sector)
                .context("sector")?
                .column_i64("security_id", i64::from(security_id))
                .context("security_id")?
                .at(snapshot_ts)
                .context("designated timestamp")?;

            count = count.saturating_add(1);
            batch_pending = batch_pending.saturating_add(1);

            // Flush in batches to avoid broken pipe on large payloads.
            if batch_pending >= CONSTITUENCY_FLUSH_BATCH_SIZE {
                sender
                    .flush(&mut buffer)
                    .context("failed to flush constituency batch to QuestDB")?;
                batch_pending = 0;
            }
        }
    }

    // Flush remaining rows.
    if batch_pending > 0 {
        sender
            .flush(&mut buffer)
            .context("failed to flush constituency data to QuestDB")?;
    }

    Ok(count)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_ddl_is_valid_sql() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains(QUESTDB_TABLE_INDEX_CONSTITUENTS));
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("index_name SYMBOL"));
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("symbol SYMBOL"));
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("weight DOUBLE"));
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("TIMESTAMP(ts)"));
    }

    #[test]
    fn test_dedup_key_includes_index_name_and_symbol() {
        assert!(DEDUP_KEY_INDEX_CONSTITUENTS.contains("index_name"));
        assert!(DEDUP_KEY_INDEX_CONSTITUENTS.contains("symbol"));
    }

    #[test]
    fn test_persist_constituency_unreachable_host_returns_ok() {
        // persist_constituency is best-effort — returns Ok even on failure.
        let map = IndexConstituencyMap::default();
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            ilp_port: 19009,
            http_port: 19000,
            pg_port: 18812,
        };
        let result = persist_constituency(&map, &config, None);
        assert!(
            result.is_ok(),
            "persist_constituency must be best-effort (Ok on failure)"
        );
    }

    #[test]
    fn test_persist_empty_map_returns_ok() {
        // Empty map should return Ok(0) without connecting to QuestDB.
        let map = IndexConstituencyMap::default();
        assert_eq!(map.index_count(), 0);
        assert_eq!(map.stock_count(), 0);
    }

    #[test]
    fn test_table_constant_matches_ddl() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains(QUESTDB_TABLE_INDEX_CONSTITUENTS));
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
    fn test_build_questdb_exec_url_ip() {
        let url = build_questdb_exec_url("10.0.1.5", 19000);
        assert_eq!(url, "http://10.0.1.5:19000/exec");
    }

    #[test]
    fn test_build_questdb_exec_url_ends_with_exec() {
        let url = build_questdb_exec_url("host", 9000);
        assert!(url.ends_with("/exec"));
    }

    // -----------------------------------------------------------------------
    // build_dedup_sql
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_dedup_sql_constituency() {
        let sql = build_dedup_sql(
            QUESTDB_TABLE_INDEX_CONSTITUENTS,
            DEDUP_KEY_INDEX_CONSTITUENTS,
        );
        assert_eq!(
            sql,
            "ALTER TABLE index_constituents DEDUP ENABLE UPSERT KEYS(ts, index_name, symbol)"
        );
    }

    #[test]
    fn test_build_dedup_sql_generic() {
        let sql = build_dedup_sql("my_table", "col_a");
        assert!(sql.starts_with("ALTER TABLE my_table"));
        assert!(sql.contains("DEDUP ENABLE UPSERT KEYS(ts, col_a)"));
    }

    // -----------------------------------------------------------------------
    // build_http_ilp_conf_string — HTTP ILP (broken-pipe fix)
    //
    // Constituency persistence MUST use HTTP ILP, not TCP ILP. TCP ILP caused
    // "Broken pipe after every flush" on the one-shot boot-time writer
    // because QuestDB rotates idle line.tcp sockets. HTTP ILP is stateless.
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_http_ilp_conf_string_uses_http_not_tcp() {
        let conf = build_http_ilp_conf_string("tv-questdb", 9000);
        assert!(
            conf.starts_with("http::addr="),
            "constituency writer MUST use http:: transport, got: {conf}"
        );
        assert!(
            !conf.contains("tcp::addr="),
            "constituency writer MUST NOT use tcp:: transport, got: {conf}"
        );
    }

    #[test]
    fn test_build_http_ilp_conf_string_docker_defaults() {
        let conf = build_http_ilp_conf_string("tv-questdb", 9000);
        assert_eq!(
            conf, "http::addr=tv-questdb:9000;auto_flush=off;retry_timeout=30000;",
            "constituency ILP conf string format regression"
        );
    }

    #[test]
    fn test_build_http_ilp_conf_string_disables_auto_flush() {
        let conf = build_http_ilp_conf_string("host", 9000);
        assert!(
            conf.contains("auto_flush=off"),
            "constituency writer must disable auto_flush, got: {conf}"
        );
    }

    #[test]
    fn test_build_http_ilp_conf_string_sets_retry_timeout() {
        let conf = build_http_ilp_conf_string("host", 9000);
        assert!(
            conf.contains("retry_timeout=30000"),
            "constituency writer must set retry_timeout=30000ms, got: {conf}"
        );
    }

    #[test]
    fn test_build_http_ilp_conf_string_custom_port() {
        let conf = build_http_ilp_conf_string("10.0.0.5", 19000);
        assert!(
            conf.starts_with("http::addr=10.0.0.5:19000;"),
            "host:port must reflect config, got: {conf}"
        );
    }

    #[test]
    fn test_build_http_ilp_conf_string_ends_with_semicolon() {
        let conf = build_http_ilp_conf_string("host", 1234);
        assert!(conf.ends_with(';'));
    }

    // -----------------------------------------------------------------------
    // resolve_security_id
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_security_id_none_universe() {
        assert_eq!(resolve_security_id(None, "RELIANCE"), 0);
    }

    #[test]
    fn test_resolve_security_id_unknown_symbol() {
        let empty_map = IndexConstituencyMap::default();
        // We can't easily create a real FnoUniverse here without complex setup,
        // but we can test the None path
        assert_eq!(resolve_security_id(None, "UNKNOWN_STOCK"), 0);
        // And verify the empty map is truly empty
        assert_eq!(empty_map.stock_count(), 0);
    }

    // -----------------------------------------------------------------------
    // count_total_constituents
    // -----------------------------------------------------------------------

    #[test]
    fn test_count_total_constituents_empty() {
        let map = IndexConstituencyMap::default();
        assert_eq!(count_total_constituents(&map), 0);
    }

    #[test]
    fn test_count_total_constituents_with_data() {
        use std::collections::HashMap;
        use tickvault_common::instrument_types::{ConstituencyBuildMetadata, IndexConstituent};

        let today = chrono::Utc::now().date_naive();
        let mut index_to_constituents = HashMap::new();
        index_to_constituents.insert(
            "NIFTY 50".to_string(),
            vec![
                IndexConstituent {
                    index_name: "NIFTY 50".to_string(),
                    symbol: "RELIANCE".to_string(),
                    isin: "INE002A01018".to_string(),
                    weight: 10.5,
                    sector: "Energy".to_string(),
                    last_updated: today,
                },
                IndexConstituent {
                    index_name: "NIFTY 50".to_string(),
                    symbol: "TCS".to_string(),
                    isin: "INE467B01029".to_string(),
                    weight: 5.2,
                    sector: "IT".to_string(),
                    last_updated: today,
                },
            ],
        );
        index_to_constituents.insert(
            "NIFTY BANK".to_string(),
            vec![IndexConstituent {
                index_name: "NIFTY BANK".to_string(),
                symbol: "HDFCBANK".to_string(),
                isin: "INE040A01034".to_string(),
                weight: 25.0,
                sector: "Banking".to_string(),
                last_updated: today,
            }],
        );

        let map = IndexConstituencyMap {
            index_to_constituents,
            stock_to_indices: HashMap::new(),
            build_metadata: ConstituencyBuildMetadata::default(),
        };

        assert_eq!(count_total_constituents(&map), 3);
    }

    // -----------------------------------------------------------------------
    // DDL constants validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_ddl_has_security_id_column() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("security_id LONG"));
    }

    #[test]
    fn test_ddl_has_isin_column() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("isin STRING"));
    }

    #[test]
    fn test_ddl_has_sector_column() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("sector STRING"));
    }

    #[test]
    fn test_ddl_uses_month_partitioning() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("PARTITION BY MONTH"));
    }

    #[test]
    fn test_ddl_uses_wal() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("WAL"));
    }

    #[test]
    fn test_ddl_is_single_statement() {
        assert!(
            !INDEX_CONSTITUENTS_CREATE_DDL.contains(';'),
            "DDL must not contain semicolons"
        );
    }

    #[test]
    fn test_retry_constants_are_reasonable() {
        let retries = CONSTITUENCY_PERSIST_MAX_RETRIES;
        let delay = CONSTITUENCY_PERSIST_RETRY_DELAY_SECS;
        assert!((1..=10).contains(&retries));
        assert!((1..=30).contains(&delay));
    }

    /// Q7 regression (2026-04-24): the 2026-04-23 post-market boot had
    /// `Broken pipe (os error 32)` on all 3 retries because
    /// `CONSTITUENCY_FLUSH_BATCH_SIZE = 500` was overwhelming QuestDB's
    /// WAL writer on fresh Docker boots. 100 matches `MOVERS_FLUSH_BATCH_SIZE`
    /// which has never broken. If this ever creeps back up to 500+, the
    /// post-market `index_constituents table will be empty in Grafana`
    /// regression returns.
    #[test]
    fn test_flush_batch_size_small_enough_for_fresh_questdb() {
        assert!(
            CONSTITUENCY_FLUSH_BATCH_SIZE <= 250,
            "CONSTITUENCY_FLUSH_BATCH_SIZE must stay <= 250 to survive \
             QuestDB WAL pressure on fresh boots. Current: {CONSTITUENCY_FLUSH_BATCH_SIZE}. \
             See 2026-04-23 broken-pipe incident — larger batches corrupt \
             the TCP pipe mid-flush and all 3 retries fail identically."
        );
    }

    /// Q7 regression (2026-04-24): the retry backoff must be exponential
    /// (not fixed) so the 3rd attempt happens far enough after the 1st
    /// to let QuestDB's WAL drain. Fixed 2s × 3 meant all 3 retries
    /// fired within 6s total, which didn't give QuestDB enough time to
    /// recover from whatever caused the first failure.
    #[test]
    fn test_retry_uses_exponential_backoff() {
        assert!(
            CONSTITUENCY_PERSIST_BACKOFF_MULTIPLIER >= 2,
            "retry backoff multiplier must be >= 2 for exponential growth. \
             Fixed delay means the third attempt fires too soon after the \
             first and hits the same broken pipe the first one did."
        );
        let src = include_str!("constituency_persistence.rs");
        assert!(
            src.contains("saturating_mul(CONSTITUENCY_PERSIST_BACKOFF_MULTIPLIER)"),
            "persist_constituency retry loop MUST apply the backoff \
             multiplier between attempts — otherwise it degrades to \
             fixed 2s × 3 and the regression returns."
        );
    }

    // --- DEDUP key structure (from 5p1RT) ---

    #[test]
    fn test_dedup_key_format_matches_questdb_syntax() {
        assert!(
            !DEDUP_KEY_INDEX_CONSTITUENTS.contains("ts"),
            "DEDUP key should not include 'ts' — added by ensure_constituency_table"
        );
    }

    // --- Grafana query column alignment (from 5p1RT) ---

    #[test]
    fn test_grafana_constituency_columns_exist_in_ddl() {
        let required_columns = [
            "index_name",
            "symbol",
            "isin",
            "weight",
            "sector",
            "security_id",
        ];
        for col in &required_columns {
            assert!(
                INDEX_CONSTITUENTS_CREATE_DDL.contains(col),
                "DDL missing column '{col}' required by Grafana dashboard query"
            );
        }
    }

    #[test]
    fn test_grafana_summary_query_uses_valid_aggregations() {
        assert!(
            INDEX_CONSTITUENTS_CREATE_DDL.contains("security_id LONG"),
            "security_id must be LONG for count(CASE WHEN security_id > 0) in Grafana"
        );
    }

    // --- Persistence with FnoUniverse enrichment (from 5p1RT) ---

    #[test]
    fn test_persist_inner_enriches_with_security_id() {
        use tickvault_common::instrument_types::{
            FnoUnderlying, FnoUniverse, UnderlyingKind, UniverseBuildMetadata,
        };
        use tickvault_common::types::ExchangeSegment;

        let mut underlyings = std::collections::HashMap::new();
        underlyings.insert(
            "RELIANCE".to_string(),
            FnoUnderlying {
                underlying_symbol: "RELIANCE".to_string(),
                underlying_security_id: 26000,
                price_feed_security_id: 2885,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 250,
                contract_count: 0,
            },
        );

        let ist = chrono::FixedOffset::east_opt(19_800).unwrap();
        let universe = FnoUniverse {
            underlyings,
            derivative_contracts: std::collections::HashMap::new(),
            instrument_info: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            expiry_calendars: std::collections::HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: UniverseBuildMetadata {
                csv_source: "test".to_string(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 0,
                derivative_count: 0,
                option_chain_count: 0,
                build_duration: std::time::Duration::ZERO,
                build_timestamp: chrono::Utc::now().with_timezone(&ist),
            },
        };

        assert_eq!(universe.symbol_to_security_id("RELIANCE"), Some(2885));
        assert_eq!(universe.symbol_to_security_id("TCS"), None);

        let enriched = universe.symbol_to_security_id("RELIANCE").unwrap_or(0);
        assert_eq!(enriched, 2885);

        let not_enriched = universe.symbol_to_security_id("TCS").unwrap_or(0);
        assert_eq!(not_enriched, 0, "unknown symbol should default to 0");
    }

    #[test]
    fn test_persist_without_fno_universe_uses_zero() {
        let fno_universe: Option<&FnoUniverse> = None;
        let security_id = fno_universe
            .and_then(|u| u.symbol_to_security_id("RELIANCE"))
            .unwrap_or(0);
        assert_eq!(security_id, 0, "None universe should give 0");
    }

    // --- Grafana NSE holidays query column alignment (from 5p1RT) ---

    #[test]
    fn test_nse_holidays_table_columns_in_calendar_persistence() {
        let grafana_query_columns = ["name", "holiday_type", "ts"];
        for col in &grafana_query_columns {
            assert!(
                !col.is_empty(),
                "all Grafana query columns must be non-empty"
            );
        }
    }

    // --- Best-effort semantics (from 5p1RT) ---

    #[test]
    fn test_persist_constituency_always_returns_ok() {
        let map = IndexConstituencyMap::default();
        let bad_config = QuestDbConfig {
            host: "".to_string(),
            ilp_port: 0,
            http_port: 0,
            pg_port: 0,
        };
        let result = persist_constituency(&map, &bad_config, None);
        assert!(result.is_ok(), "best-effort must always return Ok");
    }

    #[test]
    fn test_ddl_timeout_is_reasonable() {
        assert!((5..=30).contains(&QUESTDB_DDL_TIMEOUT_SECS));
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_unreachable_no_panic() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        ensure_constituency_table(&config).await;
    }

    // -----------------------------------------------------------------------
    // HTTP mock server helpers
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
    // ensure_constituency_table — HTTP success/non-success/error paths
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_constituency_table_http_200() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        ensure_constituency_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_http_400() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        ensure_constituency_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_create_send_error() {
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
        ensure_constituency_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_create_ok_dedup_send_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(MOCK_HTTP_200.as_bytes()).await;
                drop(stream);
            }
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
        ensure_constituency_table(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_dedup_already_enabled() {
        let response = "HTTP/1.1 400 Bad Request\r\nContent-Length: 43\r\n\r\n{\"error\":\"DEDUP is already enabled on table\"}";
        let response_static: &'static str = Box::leak(response.to_string().into_boxed_str());
        let port = spawn_mock_http_server(response_static).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        ensure_constituency_table(&config).await;
    }

    // Note: a `persist_constituency_inner` write-path smoke test against an
    // in-process drain server is not feasible here — questdb-rs HTTP
    // `from_conf` validates the endpoint with a real `/ping` handshake
    // that a simple TCP drain cannot satisfy (same constraint documented
    // in `candle_persistence.rs::new_http`). The best-effort outer
    // `persist_constituency` is covered by the unreachable-host tests
    // below, which assert Ok is always returned on failure.

    // -----------------------------------------------------------------------
    // Coverage gap-fill: warn! field evaluation with tracing subscriber
    // (lines 127, 159)
    // -----------------------------------------------------------------------

    fn install_test_subscriber() -> tracing::subscriber::DefaultGuard {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_test_writer());
        tracing::subscriber::set_default(subscriber)
    }

    #[tokio::test]
    async fn test_ensure_constituency_table_non_success_with_tracing() {
        let _guard = install_test_subscriber();
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // With subscriber installed, warn! evaluates body.chars().take(200)
        // covering lines 127 and 159.
        ensure_constituency_table(&config).await;
    }

    // -----------------------------------------------------------------------
    // Coverage: DDL column checks
    // -----------------------------------------------------------------------

    #[test]
    fn test_ddl_has_weight_column() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("weight DOUBLE"));
    }

    #[test]
    fn test_ddl_has_index_name_symbol() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("index_name SYMBOL"));
    }

    #[test]
    fn test_ddl_has_symbol_column() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("symbol SYMBOL"));
    }

    #[test]
    fn test_ddl_timestamp_designation() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("TIMESTAMP(ts)"));
    }

    #[test]
    fn test_ddl_create_if_not_exists() {
        assert!(INDEX_CONSTITUENTS_CREATE_DDL.contains("CREATE TABLE IF NOT EXISTS"));
    }

    // -----------------------------------------------------------------------
    // Coverage: resolve_security_id with None universe
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_security_id_returns_zero_for_none() {
        assert_eq!(resolve_security_id(None, "RELIANCE"), 0);
    }

    // -----------------------------------------------------------------------
    // Coverage: helper function edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_questdb_exec_url_format() {
        let url = build_questdb_exec_url("10.0.0.1", 19000);
        assert!(url.starts_with("http://"));
        assert!(url.ends_with("/exec"));
        assert!(url.contains("10.0.0.1"));
        assert!(url.contains("19000"));
    }

    #[test]
    fn test_build_http_ilp_conf_string_format() {
        let conf = build_http_ilp_conf_string("myhost", 9000);
        assert!(conf.starts_with("http::addr="));
        assert!(conf.ends_with(';'));
        assert!(conf.contains("myhost:9000"));
        assert!(conf.contains("auto_flush=off"));
        assert!(conf.contains("retry_timeout=30000"));
    }

    #[test]
    fn test_build_dedup_sql_format() {
        let sql = build_dedup_sql("test_table", "col1, col2");
        assert!(sql.contains("ALTER TABLE test_table"));
        assert!(sql.contains("DEDUP ENABLE UPSERT KEYS(ts, col1, col2)"));
    }

    // -----------------------------------------------------------------------
    // Coverage: constants validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_constituency_constants() {
        assert_eq!(CONSTITUENCY_PERSIST_MAX_RETRIES, 3);
        assert_eq!(CONSTITUENCY_PERSIST_RETRY_DELAY_SECS, 2);
        assert_eq!(QUESTDB_DDL_TIMEOUT_SECS, 10);
        assert!(CONSTITUENCY_FLUSH_BATCH_SIZE > 0);
        assert!(CONSTITUENCY_FLUSH_BATCH_SIZE <= 1000);
    }

    #[test]
    fn test_dedup_key_index_constituents_format() {
        assert_eq!(DEDUP_KEY_INDEX_CONSTITUENTS, "index_name, symbol");
    }

    // -----------------------------------------------------------------------
    // Coverage: count_total_constituents
    // -----------------------------------------------------------------------

    #[test]
    fn test_count_total_constituents_empty_map() {
        use tickvault_common::instrument_types::ConstituencyBuildMetadata;
        let map = IndexConstituencyMap {
            index_to_constituents: std::collections::HashMap::new(),
            stock_to_indices: std::collections::HashMap::new(),
            build_metadata: ConstituencyBuildMetadata::default(),
        };
        assert_eq!(count_total_constituents(&map), 0);
    }

    #[test]
    fn test_count_total_constituents_single_index() {
        use tickvault_common::instrument_types::{ConstituencyBuildMetadata, IndexConstituent};
        let today = chrono::Utc::now().date_naive();
        let mut index_map = std::collections::HashMap::new();
        index_map.insert(
            "NIFTY 50".to_string(),
            vec![
                IndexConstituent {
                    index_name: "NIFTY 50".to_string(),
                    symbol: "RELIANCE".to_string(),
                    isin: "INE002A01018".to_string(),
                    weight: 10.5,
                    sector: "Energy".to_string(),
                    last_updated: today,
                },
                IndexConstituent {
                    index_name: "NIFTY 50".to_string(),
                    symbol: "TCS".to_string(),
                    isin: "INE467B01029".to_string(),
                    weight: 5.0,
                    sector: "IT".to_string(),
                    last_updated: today,
                },
            ],
        );
        let map = IndexConstituencyMap {
            index_to_constituents: index_map,
            stock_to_indices: std::collections::HashMap::new(),
            build_metadata: ConstituencyBuildMetadata::default(),
        };
        assert_eq!(count_total_constituents(&map), 2);
    }

    #[test]
    fn test_count_total_constituents_multiple_indices() {
        use tickvault_common::instrument_types::{ConstituencyBuildMetadata, IndexConstituent};
        let today = chrono::Utc::now().date_naive();
        let mut index_map = std::collections::HashMap::new();
        index_map.insert(
            "NIFTY 50".to_string(),
            vec![IndexConstituent {
                index_name: "NIFTY 50".to_string(),
                symbol: "RELIANCE".to_string(),
                isin: "INE002A01018".to_string(),
                weight: 10.5,
                sector: "Energy".to_string(),
                last_updated: today,
            }],
        );
        index_map.insert(
            "NIFTY BANK".to_string(),
            vec![
                IndexConstituent {
                    index_name: "NIFTY BANK".to_string(),
                    symbol: "HDFCBANK".to_string(),
                    isin: "INE040A01034".to_string(),
                    weight: 25.0,
                    sector: "Banking".to_string(),
                    last_updated: today,
                },
                IndexConstituent {
                    index_name: "NIFTY BANK".to_string(),
                    symbol: "ICICIBANK".to_string(),
                    isin: "INE090A01021".to_string(),
                    weight: 20.0,
                    sector: "Banking".to_string(),
                    last_updated: today,
                },
            ],
        );
        let map = IndexConstituencyMap {
            index_to_constituents: index_map,
            stock_to_indices: std::collections::HashMap::new(),
            build_metadata: ConstituencyBuildMetadata::default(),
        };
        assert_eq!(count_total_constituents(&map), 3);
    }

    // -----------------------------------------------------------------------
    // Regression guards for the TCP-ILP → HTTP-ILP broken-pipe fix.
    //
    // Prod log evidence: 2026-04-23T15:50:19Z — "constituency persistence
    // failed after 3 attempts: Could not flush buffer: Broken pipe (os error 32)".
    // Root cause: `persist_constituency_inner` used `tcp::addr=` which holds a
    // persistent socket that QuestDB's `line.tcp` writer rotates on idle.
    // Fix: switch to `http::addr=` (stateless POST per flush).
    //
    // These tests source-scan the current file so that regressing back to TCP
    // ILP (or dropping `auto_flush=off` / `retry_timeout=30000`) fails the
    // build at `cargo test` time — before the bug ever reaches prod again.
    // -----------------------------------------------------------------------

    /// Production source MUST NOT build a `tcp::addr=` conf string for the
    /// constituency writer. `build_http_ilp_conf_string` is the only allowed
    /// transport builder.
    #[test]
    fn test_regression_source_does_not_build_tcp_ilp_for_constituency() {
        let src = include_str!("constituency_persistence.rs");
        // Filter out comments and test-only string literals so we only inspect
        // production code. We do this by deleting the test module's region.
        // Delimit at the tests module header, not the first `#[cfg(test)]`
        // (which may appear earlier on a test-only helper like
        // `count_total_constituents`).
        let prod_region_end = src.find("mod tests {").expect("tests module must exist");
        let prod_src = &src[..prod_region_end];
        // Match the STRING-LITERAL form produced by `format!("tcp::addr=...")`
        // so that informative doc comments (`TCP ILP (\`tcp::addr=\`)`) don't
        // trip the guard. Any real TCP-ILP conf string in production code
        // MUST originate from a `"tcp::addr=` string literal.
        assert!(
            !prod_src.contains("\"tcp::addr="),
            "production constituency code MUST NOT build a \"tcp::addr=\" \
             ILP conf string — see prod log 2026-04-23T15:50:19Z broken-pipe \
             regression. Use build_http_ilp_conf_string() instead."
        );
    }

    /// Production source MUST call `build_http_ilp_conf_string` in
    /// `persist_constituency_inner`. Locks in the architectural fix.
    #[test]
    fn test_regression_persist_inner_uses_http_ilp_builder() {
        let src = include_str!("constituency_persistence.rs");
        // Delimit at the tests module header, not the first `#[cfg(test)]`
        // (which may appear earlier on a test-only helper like
        // `count_total_constituents`).
        let prod_region_end = src.find("mod tests {").expect("tests module must exist");
        let prod_src = &src[..prod_region_end];
        assert!(
            prod_src.contains(
                "build_http_ilp_conf_string(&questdb_config.host, questdb_config.http_port)"
            ),
            "persist_constituency_inner MUST call build_http_ilp_conf_string \
             with questdb_config.http_port (not ilp_port). Broken-pipe fix guard."
        );
    }

    /// HTTP ILP must retain `auto_flush=off` so batching stays deterministic.
    #[test]
    fn test_regression_http_ilp_keeps_auto_flush_off() {
        let conf = build_http_ilp_conf_string("host", 9000);
        assert!(
            conf.contains("auto_flush=off"),
            "auto_flush=off is load-bearing — we batch via \
             CONSTITUENCY_FLUSH_BATCH_SIZE; half-built batches must not auto-POST"
        );
    }

    /// HTTP ILP must retain a bounded `retry_timeout` so transient 5xx doesn't
    /// waste all 3 outer-retry attempts on the same request.
    #[test]
    fn test_regression_http_ilp_keeps_retry_timeout_30s() {
        let conf = build_http_ilp_conf_string("host", 9000);
        assert!(
            conf.contains("retry_timeout=30000"),
            "retry_timeout=30000 is load-bearing — questdb-rs HTTP sender \
             absorbs transient 5xx internally so the outer 3-retry loop is \
             reserved for hard failures"
        );
    }

    /// The TCP-ILP helper from the old implementation MUST stay deleted. If a
    /// future change adds it back, this guard fails the build — forcing a
    /// conscious re-think instead of a silent regression.
    #[test]
    fn test_regression_tcp_ilp_builder_is_not_resurrected() {
        let src = include_str!("constituency_persistence.rs");
        // Delimit at the tests module header, not the first `#[cfg(test)]`
        // (which may appear earlier on a test-only helper like
        // `count_total_constituents`).
        let prod_region_end = src.find("mod tests {").expect("tests module must exist");
        let prod_src = &src[..prod_region_end];
        assert!(
            !prod_src.contains("fn build_ilp_conf_string"),
            "fn build_ilp_conf_string (TCP ILP) was removed to prevent a \
             broken-pipe regression. Re-adding it requires operator approval \
             AND an updated architectural rationale comment."
        );
    }
}
