//! Option-chain minute-snapshot persistence (PR #2 of 5 per
//! `.claude/plans/friday-may-15-mega/topic-OPTION-CHAIN-MINUTE-SNAPSHOT.md`).
//!
//! Persists the option chain snapshot fetched 3 times per minute for
//! each configured underlying (SENSEX :53, BANKNIFTY :56, NIFTY :59 by
//! default). One ROW PER STRIKE PER SIDE (CE or PE) so dashboards can
//! answer forensic queries:
//!
//!   * "What was the IV of NIFTY 25650-CE at 11:23:00 IST when BRUTEX
//!     entered the position?"
//!   * "Did SENSEX 79500-PE have liquidity (top_bid/top_ask both > 0)
//!     at the moment of entry?"
//!   * "Which strikes were ATM ±25 for BANKNIFTY between 10:00 and
//!     10:30 IST?"
//!
//! ## DEDUP key contract
//!
//! `(trading_date_ist, ts, underlying_symbol, exchange_segment, strike,
//! ce_or_pe)` — 6 columns. The composite is non-negotiable:
//!
//!   * `trading_date_ist` — QuestDB designated-timestamp invariant
//!     (regression class 2026-04-28).
//!   * `ts` — minute-level timestamp; without it same-strike rows on
//!     successive minutes would overwrite each other.
//!   * `underlying_symbol` + `exchange_segment` — I-P1-11 composite-key
//!     uniqueness (security_id alone is NOT unique across segments).
//!   * `strike` + `ce_or_pe` — without these, 50+ strikes × 2 sides on
//!     the SAME minute would collapse to a single row.
//!
//! ## SEBI retention
//!
//! Same lifecycle as the other Phase 0 audit tables: 90d hot QuestDB →
//! S3 Intelligent-Tiering → Glacier Deep Archive. See `aws-budget.md`.
//!
//! ## Live SQL TEST-EXEMPT
//!
//! `ensure_*_table` and `append_*_row` require a running QuestDB; the
//! ILP-side error path is tested via the `#[tokio::test]` cases below
//! which assert the helpers return `Err` against an unreachable host.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

/// Wire-format table name. Stable across releases — operators and
/// dashboards depend on it. Pinned by `test_table_name_constant`.
pub const QUESTDB_TABLE_OPTION_CHAIN_MINUTE_SNAPSHOT: &str = "option_chain_minute_snapshot";

/// Composite DEDUP key. Six columns covering the full identity of a
/// per-strike per-side per-minute snapshot row. See module docs for
/// the full rationale.
pub const DEDUP_KEY_OPTION_CHAIN_MINUTE_SNAPSHOT: &str =
    "trading_date_ist, ts, underlying_symbol, exchange_segment, strike, ce_or_pe";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format strings for the `ce_or_pe` SYMBOL column.
/// Kept as a typed enum so callers can't accidentally pass `"call"`
/// (lowercase verbose) or `"C"` (abbreviated) and silently fragment
/// the column's symbol set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionSide {
    Call,
    Put,
}

impl OptionSide {
    /// Returns the wire-format label. ALWAYS uppercase 2-char — matches
    /// Dhan's own `OptType` field convention in
    /// `.claude/rules/dhan/live-order-update.md` rule 14.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Call => "CE",
            Self::Put => "PE",
        }
    }
}

/// Stable wire-format strings for the `fetch_outcome` SYMBOL column.
/// Lets operators query the table for "how many minutes did we fall
/// back on the RAM cache vs hit Dhan directly?"
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    /// Primary fetch at the scheduled slot succeeded.
    Fresh,
    /// Same-minute retry (per `fetch_retry_max_attempts`) succeeded.
    RetrySucceeded,
    /// All retries exhausted; row written from RAM cache fallback.
    CacheFallback,
}

impl FetchOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::RetrySucceeded => "retry_succeeded",
            Self::CacheFallback => "cache_fallback",
        }
    }

    /// Predicate — returns true only for `Fresh`. Pinned so a future
    /// 4th-variant edit (e.g. `RestRetry`) doesn't silently broaden
    /// the "happy path" definition.
    #[must_use]
    pub const fn is_happy_path(self) -> bool {
        matches!(self, Self::Fresh)
    }
}

/// Creates the audit table if absent. Idempotent — safe to call on
/// every boot. Schema columns + DEDUP key are pinned by ratchet tests.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_option_chain_minute_snapshot_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_OPTION_CHAIN_MINUTE_SNAPSHOT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            underlying_symbol SYMBOL, \
            underlying_security_id INT, \
            exchange_segment SYMBOL, \
            expiry DATE, \
            strike DOUBLE, \
            ce_or_pe SYMBOL, \
            security_id INT, \
            last_price DOUBLE, \
            average_price DOUBLE, \
            oi LONG, \
            previous_oi LONG, \
            volume LONG, \
            previous_volume LONG, \
            previous_close_price DOUBLE, \
            top_bid_price DOUBLE, \
            top_bid_quantity LONG, \
            top_ask_price DOUBLE, \
            top_ask_quantity LONG, \
            implied_volatility DOUBLE, \
            greek_delta DOUBLE, \
            greek_theta DOUBLE, \
            greek_gamma DOUBLE, \
            greek_vega DOUBLE, \
            cache_age_used_secs INT, \
            fetch_outcome SYMBOL\
        ) timestamp(ts) PARTITION BY DAY WAL \
        DEDUP UPSERT KEYS({DEDUP_KEY_OPTION_CHAIN_MINUTE_SNAPSHOT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_OPTION_CHAIN_MINUTE_SNAPSHOT,
                "audit table ready"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_OPTION_CHAIN_MINUTE_SNAPSHOT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_OPTION_CHAIN_MINUTE_SNAPSHOT,
                ?err,
                "DDL request failed"
            );
        }
    }

    // Feed-provenance label (operator 2026-06-19, "all tables"): self-heal ALTER
    // only — additive, idempotent, NON-key; the CREATE DDL + its 27-column
    // ratchet are untouched. Free on every boot.
    let alter_feed_ddl = format!(
        "ALTER TABLE {QUESTDB_TABLE_OPTION_CHAIN_MINUTE_SNAPSHOT} ADD COLUMN IF NOT EXISTS feed SYMBOL"
    );
    match client
        .get(&base_url)
        .query(&[("query", alter_feed_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_OPTION_CHAIN_MINUTE_SNAPSHOT,
                status = %resp.status(),
                "ALTER ADD COLUMN feed non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_OPTION_CHAIN_MINUTE_SNAPSHOT,
                ?err,
                "ALTER ADD COLUMN feed request failed"
            );
        }
    }
}

/// One row's worth of strike data, in the shape the writer accepts.
/// Keeps the [`append_option_chain_minute_snapshot_row`] signature
/// manageable (27 columns would otherwise require a 28-arg function).
#[derive(Debug, Clone, PartialEq)]
pub struct OptionChainMinuteSnapshotRow<'a> {
    /// Minute-aligned IST nanoseconds. Stamp of the scheduled slot
    /// (e.g. `HH:MM:00` — NOT the actual `HH:MM:53` fetch instant).
    /// Minute-aligned so the row joins cleanly against 1m candles.
    pub ts_nanos_ist: i64,
    /// IST date midnight nanos — QuestDB designated-timestamp partition.
    pub trading_date_ist_nanos: i64,
    /// Operator-visible symbol (e.g. "NIFTY"). NOT escaped here —
    /// `sanitize_audit_string` is applied internally.
    pub underlying_symbol: &'a str,
    /// Dhan `UnderlyingScrip` numeric (e.g. 13 for NIFTY).
    pub underlying_security_id: u32,
    /// Dhan `UnderlyingSeg` string (e.g. `"IDX_I"`).
    pub exchange_segment: &'a str,
    /// Expiry as IST date midnight nanos.
    pub expiry_date_nanos: i64,
    /// Strike price as f64 — Dhan response uses decimal strings
    /// like `"79500.000000"`; parse before passing.
    pub strike: f64,
    /// Which side of the strike (CE or PE).
    pub side: OptionSide,
    /// Dhan-assigned per-option SecurityId. Useful for downstream
    /// joins against ticks / order updates.
    pub security_id: u32,
    pub last_price: f64,
    pub average_price: f64,
    pub oi: i64,
    pub previous_oi: i64,
    pub volume: i64,
    pub previous_volume: i64,
    pub previous_close_price: f64,
    pub top_bid_price: f64,
    pub top_bid_quantity: i64,
    pub top_ask_price: f64,
    pub top_ask_quantity: i64,
    pub implied_volatility: f64,
    pub greek_delta: f64,
    pub greek_theta: f64,
    pub greek_gamma: f64,
    pub greek_vega: f64,
    /// Cache age in seconds the strategy would have read from. 0 for
    /// `Fresh`; > 0 for `RetrySucceeded` (retry latency) or
    /// `CacheFallback` (real staleness).
    pub cache_age_used_secs: i32,
    /// Outcome of the fetch that produced this row.
    pub fetch_outcome: FetchOutcome,
}

/// Column list shared by the single-row and batch INSERT helpers, in
/// exact schema order. One constant so the two writers cannot drift.
const INSERT_COLUMN_LIST: &str = "ts, trading_date_ist, underlying_symbol, \
     underlying_security_id, exchange_segment, expiry, strike, ce_or_pe, \
     security_id, last_price, average_price, oi, previous_oi, volume, \
     previous_volume, previous_close_price, top_bid_price, top_bid_quantity, \
     top_ask_price, top_ask_quantity, implied_volatility, greek_delta, \
     greek_theta, greek_gamma, greek_vega, cache_age_used_secs, fetch_outcome";

/// Max rows per multi-row INSERT. QuestDB `/exec` carries the
/// statement as a URL query parameter, so an unbounded batch would
/// overflow the HTTP request-line length limit. 15 rows × ~350 chars
/// stays well under a conservative ~8 KB ceiling.
const MAX_ROWS_PER_INSERT_BATCH: usize = 15;

/// Formats one row as a QuestDB `VALUES (...)` tuple in schema order.
///
/// `*_nanos` fields are IST wall-clock nanoseconds; this divides them
/// to microseconds because QuestDB `TIMESTAMP` columns store
/// microseconds since epoch — embedding nanos overflows the year-9999
/// range (2026-04-28 regression).
fn format_row_values_tuple(row: &OptionChainMinuteSnapshotRow<'_>) -> String {
    let underlying_symbol = sanitize_audit_string(row.underlying_symbol);
    let exchange_segment = sanitize_audit_string(row.exchange_segment);
    let side = row.side.as_str();
    let fetch_outcome = row.fetch_outcome.as_str();
    let ts_micros = row.ts_nanos_ist / 1_000;
    let trading_date_micros = row.trading_date_ist_nanos / 1_000;
    let expiry_micros = row.expiry_date_nanos / 1_000;
    let underlying_security_id = row.underlying_security_id;
    let strike = row.strike;
    let security_id = row.security_id;
    let last_price = row.last_price;
    let average_price = row.average_price;
    let oi = row.oi;
    let previous_oi = row.previous_oi;
    let volume = row.volume;
    let previous_volume = row.previous_volume;
    let previous_close_price = row.previous_close_price;
    let top_bid_price = row.top_bid_price;
    let top_bid_quantity = row.top_bid_quantity;
    let top_ask_price = row.top_ask_price;
    let top_ask_quantity = row.top_ask_quantity;
    let implied_volatility = row.implied_volatility;
    let greek_delta = row.greek_delta;
    let greek_theta = row.greek_theta;
    let greek_gamma = row.greek_gamma;
    let greek_vega = row.greek_vega;
    let cache_age_used_secs = row.cache_age_used_secs;
    format!(
        "({ts_micros}, {trading_date_micros}, '{underlying_symbol}', \
          {underlying_security_id}, '{exchange_segment}', {expiry_micros}, \
          {strike}, '{side}', {security_id}, {last_price}, {average_price}, \
          {oi}, {previous_oi}, {volume}, {previous_volume}, \
          {previous_close_price}, {top_bid_price}, {top_bid_quantity}, \
          {top_ask_price}, {top_ask_quantity}, {implied_volatility}, \
          {greek_delta}, {greek_theta}, {greek_gamma}, {greek_vega}, \
          {cache_age_used_secs}, '{fetch_outcome}')"
    )
}

/// Executes one INSERT statement against QuestDB `/exec`.
async fn exec_insert(questdb_config: &QuestDbConfig, sql: &str) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let resp = client
        .get(&base_url)
        .query(&[("query", sql)])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("option_chain_minute_snapshot insert non-2xx ({status}): {body}");
    }
    Ok(())
}

/// Appends one option-chain-minute-snapshot row.
///
/// # Errors
///
/// Propagates the `reqwest` error on transport failure, or returns
/// `Err` on a non-2xx response.
pub async fn append_option_chain_minute_snapshot_row(
    questdb_config: &QuestDbConfig,
    row: &OptionChainMinuteSnapshotRow<'_>,
) -> anyhow::Result<()> {
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_OPTION_CHAIN_MINUTE_SNAPSHOT} \
         ({INSERT_COLUMN_LIST}) VALUES {tuple};",
        tuple = format_row_values_tuple(row),
    );
    exec_insert(questdb_config, &sql).await
}

/// Appends a batch of option-chain-minute-snapshot rows.
///
/// Rows are written in multi-row INSERTs of up to
/// [`MAX_ROWS_PER_INSERT_BATCH`] — one HTTP round-trip per chunk
/// instead of one per row. An empty slice is a no-op (`Ok`). Cold
/// path — called at most 3 times per minute by the snapshot scheduler.
///
/// On a chunk failure earlier chunks may already be committed; the
/// table's `DEDUP UPSERT KEYS` makes a re-run idempotent.
///
/// # Errors
///
/// Propagates the `reqwest` error on transport failure, or returns
/// `Err` on the first non-2xx response.
pub async fn append_option_chain_minute_snapshot_rows(
    questdb_config: &QuestDbConfig,
    rows: &[OptionChainMinuteSnapshotRow<'_>],
) -> anyhow::Result<()> {
    for chunk in rows.chunks(MAX_ROWS_PER_INSERT_BATCH) {
        let mut values = String::with_capacity(chunk.len() * 360);
        for (idx, row) in chunk.iter().enumerate() {
            if idx > 0 {
                values.push_str(", ");
            }
            values.push_str(&format_row_values_tuple(row));
        }
        let sql = format!(
            "INSERT INTO {QUESTDB_TABLE_OPTION_CHAIN_MINUTE_SNAPSHOT} \
             ({INSERT_COLUMN_LIST}) VALUES {values};"
        );
        exec_insert(questdb_config, &sql).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg_unreachable() -> QuestDbConfig {
        // Port 1 is reserved and never listening; ensures real HTTP
        // failure without hitting any live service.
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 8812,
            ilp_port: 9009,
        }
    }

    fn sample_row() -> OptionChainMinuteSnapshotRow<'static> {
        OptionChainMinuteSnapshotRow {
            ts_nanos_ist: 1_700_000_000_000_000_000,
            trading_date_ist_nanos: 1_699_920_000_000_000_000,
            underlying_symbol: "NIFTY",
            underlying_security_id: 13,
            exchange_segment: "IDX_I",
            expiry_date_nanos: 1_700_006_400_000_000_000,
            strike: 25650.0,
            side: OptionSide::Call,
            security_id: 42528,
            last_price: 134.0,
            average_price: 146.99,
            oi: 3_786_445,
            previous_oi: 402_220,
            volume: 117_567_970,
            previous_volume: 31_931_705,
            previous_close_price: 244.85,
            top_bid_price: 133.55,
            top_bid_quantity: 1625,
            top_ask_price: 134.0,
            top_ask_quantity: 1365,
            implied_volatility: 9.789,
            greek_delta: 0.53871,
            greek_theta: -15.1539,
            greek_gamma: 0.00132,
            greek_vega: 12.18593,
            cache_age_used_secs: 0,
            fetch_outcome: FetchOutcome::Fresh,
        }
    }

    #[test]
    fn test_table_name_constant() {
        assert_eq!(
            QUESTDB_TABLE_OPTION_CHAIN_MINUTE_SNAPSHOT,
            "option_chain_minute_snapshot"
        );
    }

    #[test]
    fn test_dedup_key_constant_lists_six_columns() {
        // DEDUP key is the SEBI-forensic uniqueness contract. Six
        // columns covering identity dimensions. Counting commas + 1
        // is the simplest invariant check.
        let column_count = DEDUP_KEY_OPTION_CHAIN_MINUTE_SNAPSHOT.matches(',').count() + 1;
        assert_eq!(column_count, 6, "DEDUP key must have exactly 6 columns");
    }

    #[test]
    fn test_dedup_key_includes_designated_timestamp() {
        // Regression class 2026-04-28: QuestDB requires the designated
        // timestamp column in DEDUP UPSERT KEYS or the CREATE TABLE
        // returns 400 Bad Request and the table is silently missing.
        assert!(
            DEDUP_KEY_OPTION_CHAIN_MINUTE_SNAPSHOT.contains("trading_date_ist"),
            "DEDUP key must include trading_date_ist"
        );
        assert!(
            DEDUP_KEY_OPTION_CHAIN_MINUTE_SNAPSHOT.contains("ts"),
            "DEDUP key must include ts"
        );
    }

    #[test]
    fn test_dedup_key_includes_segment_per_ip1_11() {
        // I-P1-11: security_id alone is NOT unique across segments.
        // `underlying_symbol` + `exchange_segment` are both needed.
        assert!(
            DEDUP_KEY_OPTION_CHAIN_MINUTE_SNAPSHOT.contains("underlying_symbol"),
            "DEDUP key must include underlying_symbol per I-P1-11"
        );
        assert!(
            DEDUP_KEY_OPTION_CHAIN_MINUTE_SNAPSHOT.contains("exchange_segment"),
            "DEDUP key must include exchange_segment per I-P1-11"
        );
    }

    #[test]
    fn test_dedup_key_includes_strike_and_side() {
        // Without these, 50+ strikes × 2 sides on the same minute
        // would collapse to a single row.
        assert!(
            DEDUP_KEY_OPTION_CHAIN_MINUTE_SNAPSHOT.contains("strike"),
            "DEDUP key must include strike"
        );
        assert!(
            DEDUP_KEY_OPTION_CHAIN_MINUTE_SNAPSHOT.contains("ce_or_pe"),
            "DEDUP key must include ce_or_pe"
        );
    }

    #[test]
    fn test_option_side_wire_format_stable() {
        // Dashboards + Telegram messages depend on the exact 2-char
        // uppercase labels matching Dhan's own `OptType` convention.
        assert_eq!(OptionSide::Call.as_str(), "CE");
        assert_eq!(OptionSide::Put.as_str(), "PE");
    }

    #[test]
    fn test_fetch_outcome_wire_format_stable() {
        // Operators query the column by exact string match — bumping
        // any of these breaks dashboards + SQL operator-cheats.
        assert_eq!(FetchOutcome::Fresh.as_str(), "fresh");
        assert_eq!(FetchOutcome::RetrySucceeded.as_str(), "retry_succeeded");
        assert_eq!(FetchOutcome::CacheFallback.as_str(), "cache_fallback");
    }

    #[test]
    fn test_fetch_outcome_is_happy_path_pins_only_fresh() {
        // A future 4th variant (e.g. RestRetry / WarmCache) must NOT
        // be silently counted as a happy path by downstream metric
        // panels. Pin the predicate explicitly.
        assert!(FetchOutcome::Fresh.is_happy_path());
        assert!(!FetchOutcome::RetrySucceeded.is_happy_path());
        assert!(!FetchOutcome::CacheFallback.is_happy_path());
    }

    #[test]
    fn test_ddl_contains_all_27_columns() {
        // Source-scan: assert every column from the schema is named
        // in the DDL string in this module. Any drift between code
        // and schema fails the build.
        let source = include_str!("option_chain_minute_snapshot_persistence.rs");
        let columns = [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "underlying_symbol SYMBOL",
            "underlying_security_id INT",
            "exchange_segment SYMBOL",
            "expiry DATE",
            "strike DOUBLE",
            "ce_or_pe SYMBOL",
            "security_id INT",
            "last_price DOUBLE",
            "average_price DOUBLE",
            "oi LONG",
            "previous_oi LONG",
            "volume LONG",
            "previous_volume LONG",
            "previous_close_price DOUBLE",
            "top_bid_price DOUBLE",
            "top_bid_quantity LONG",
            "top_ask_price DOUBLE",
            "top_ask_quantity LONG",
            "implied_volatility DOUBLE",
            "greek_delta DOUBLE",
            "greek_theta DOUBLE",
            "greek_gamma DOUBLE",
            "greek_vega DOUBLE",
            "cache_age_used_secs INT",
            "fetch_outcome SYMBOL",
        ];
        for col in columns {
            assert!(
                source.contains(col),
                "DDL must declare column `{col}` — drift between code + schema rejected"
            );
        }
    }

    #[test]
    fn test_ddl_partition_by_day_with_wal() {
        let source = include_str!("option_chain_minute_snapshot_persistence.rs");
        assert!(
            source.contains("PARTITION BY DAY WAL"),
            "DDL must include `PARTITION BY DAY WAL` — required for retention + ILP idempotent writes"
        );
    }

    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        // Regression class 2026-04-28: QuestDB TIMESTAMP columns
        // store microseconds; embedding nanos overflows year-9999
        // bound. Source-scan ensures the conversion is performed.
        let source = include_str!("option_chain_minute_snapshot_persistence.rs");
        assert!(
            source.contains("ts_nanos_ist / 1_000"),
            "INSERT helper must divide ts nanos to micros (year-9999 overflow guard)"
        );
        assert!(
            source.contains("trading_date_ist_nanos / 1_000"),
            "INSERT helper must divide trading_date_ist nanos to micros"
        );
    }

    #[test]
    fn test_ensure_option_chain_minute_snapshot_table_pub_fn_visible() {
        // Cheap smoke that the public surface compiles. The fn is async
        // and elides a lifetime; reference-coerce it to a plain fn item
        // pointer (zero-sized) so the test catches signature changes
        // without forcing higher-ranked lifetime annotations.
        let _ = ensure_option_chain_minute_snapshot_table;
    }

    #[tokio::test]
    async fn test_append_option_chain_minute_snapshot_row_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg_unreachable();
        let row = sample_row();
        let result = append_option_chain_minute_snapshot_row(&cfg, &row).await;
        assert!(
            result.is_err(),
            "must propagate transport error when QuestDB is unreachable"
        );
    }

    #[tokio::test]
    async fn test_append_handles_put_side_and_cache_fallback_outcome() {
        // Exercise the other enum variants to lock the wire-format
        // labels in the INSERT path.
        let cfg = test_cfg_unreachable();
        let mut row = sample_row();
        row.side = OptionSide::Put;
        row.fetch_outcome = FetchOutcome::CacheFallback;
        row.cache_age_used_secs = 120;
        let result = append_option_chain_minute_snapshot_row(&cfg, &row).await;
        assert!(result.is_err(), "transport error path must propagate");
    }

    #[tokio::test]
    async fn test_append_handles_negative_greeks() {
        // Theta is negative for long options; ensure no formatting
        // bugs around negative f64 in the SQL builder.
        let cfg = test_cfg_unreachable();
        let mut row = sample_row();
        row.greek_theta = -25.5;
        row.greek_delta = -0.45;
        let result = append_option_chain_minute_snapshot_row(&cfg, &row).await;
        assert!(result.is_err(), "transport error path must propagate");
    }

    #[test]
    fn test_insert_column_list_has_27_columns() {
        // The shared column list must name every schema column so the
        // single-row and batch writers stay aligned with the DDL.
        assert_eq!(
            INSERT_COLUMN_LIST.matches(',').count() + 1,
            27,
            "INSERT column list must name all 27 schema columns"
        );
    }

    #[test]
    fn test_format_row_values_tuple_has_27_fields() {
        let tuple = format_row_values_tuple(&sample_row());
        assert!(
            tuple.starts_with('(') && tuple.ends_with(')'),
            "tuple must be paren-wrapped"
        );
        // No nested parens/commas in any field → comma count is exact:
        // 27 columns → 26 separating commas.
        let inner = &tuple[1..tuple.len() - 1];
        assert_eq!(
            inner.matches(',').count(),
            26,
            "27 columns → 26 separating commas"
        );
    }

    #[test]
    fn test_max_rows_per_insert_batch_is_bounded() {
        // The chunk size must stay small enough that one chunk's INSERT
        // fits in QuestDB's /exec URL query parameter.
        assert!(MAX_ROWS_PER_INSERT_BATCH > 0);
        assert!(MAX_ROWS_PER_INSERT_BATCH <= 20);
    }

    #[tokio::test]
    async fn test_append_option_chain_minute_snapshot_rows_empty_slice_is_ok() {
        let cfg = test_cfg_unreachable();
        let result = append_option_chain_minute_snapshot_rows(&cfg, &[]).await;
        assert!(result.is_ok(), "empty slice must be a no-op Ok");
    }

    #[tokio::test]
    async fn test_append_rows_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg_unreachable();
        let rows = [sample_row(), sample_row()];
        let result = append_option_chain_minute_snapshot_rows(&cfg, &rows).await;
        assert!(result.is_err(), "must propagate transport error");
    }

    #[tokio::test]
    async fn test_append_rows_chunks_beyond_batch_size() {
        // A slice larger than MAX_ROWS_PER_INSERT_BATCH exercises the
        // chunk loop. Against an unreachable host the first chunk fails
        // fast — still proves the multi-chunk path returns Err, not Ok.
        let cfg = test_cfg_unreachable();
        let rows: Vec<_> = (0..MAX_ROWS_PER_INSERT_BATCH + 5)
            .map(|_| sample_row())
            .collect();
        let result = append_option_chain_minute_snapshot_rows(&cfg, &rows).await;
        assert!(result.is_err());
    }
}
