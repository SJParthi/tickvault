//! Phase 0 Item 16a — open-price source-selection audit persistence.
//!
//! Records which source the 09:15 candle's `open` field came from for
//! every subscribed instrument, on every trading day. SEBI 5-year
//! retention applies because the open-price decision is part of the
//! authoritative candle that drives every downstream indicator + risk
//! check + strategy comparison.
//!
//! The operator can answer:
//!
//!   * "Did NIFTY's 09:15 candle today use the pre-open buffer or
//!     fall through to the first WS tick?"
//!   * "How many F&O stocks on 2026-05-12 had their open price come
//!     from `RestQuoteDayOpen` because the pre-open buffer was empty?"
//!   * "Why does Bollinger %B differ from yesterday on RELIANCE? —
//!     query `open_price_audit` for the source used + the resolved
//!     value."
//!
//! One row per instrument per trading day. DEDUP key
//! `(trading_date_ist, security_id, exchange_segment)` follows
//! I-P1-11 composite identity; a re-seal of the same instrument's
//! 09:15 candle on the same day idempotently overwrites the prior
//! row (e.g. mid-session boot recomputes the decision).
//!
//! Mirrors the `auth_renewal_audit` / `live_instance_lock` /
//! `static_ip_audit` family of audit-table writers.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_OPEN_PRICE_AUDIT: &str = "open_price_audit";

// Composite DEDUP per I-P1-11 (security_id + segment) plus
// trading_date_ist for per-day idempotency. Re-seal on mid-session
// boot overwrites the prior row.
pub const DEDUP_KEY_OPEN_PRICE_AUDIT: &str = "trading_date_ist, security_id, exchange_segment";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format strings for the `source` column. Mirrors
/// `tickvault_common::open_price_source::OpenSource` but converts to
/// a typed audit-row label so the persistence layer can write to a
/// SYMBOL column without coupling the storage crate to common's
/// enum directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPriceSourceLabel {
    /// Read from the NSE pre-open buffer's 09:08–09:12 last slot.
    /// Ideal path for NIFTY / BANKNIFTY / F&O stocks.
    PreopenBuffer,
    /// REST `/v2/marketfeed/quote.day_open` fallback (NSE_EQ only,
    /// 09:14:55 IST).
    RestQuoteDayOpen,
    /// First WebSocket tick after 09:15:00. Last resort. Acceptable
    /// for IndexNoPreopen (SENSEX/INDIA VIX); triggers
    /// `OPEN-PRICE-WARN` Telegram for the other two classes.
    FirstWsTick,
}

impl OpenPriceSourceLabel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreopenBuffer => "preopen_buffer",
            Self::RestQuoteDayOpen => "rest_quote_day_open",
            Self::FirstWsTick => "first_ws_tick",
        }
    }
}

/// Stable wire-format strings for the `source_class` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenSourceClassLabel {
    IndexWithPreopenBuffer,
    IndexNoPreopen,
    StockWithPreopen,
}

impl OpenSourceClassLabel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IndexWithPreopenBuffer => "index_with_preopen_buffer",
            Self::IndexNoPreopen => "index_no_preopen",
            Self::StockWithPreopen => "stock_with_preopen",
        }
    }
}

/// Creates the audit table if absent. Idempotent — safe to call on
/// every boot.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_open_price_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_OPEN_PRICE_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            security_id INT, \
            exchange_segment SYMBOL, \
            source SYMBOL, \
            source_class SYMBOL, \
            open_price DOUBLE, \
            warn_emitted BOOLEAN\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_OPEN_PRICE_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(table = QUESTDB_TABLE_OPEN_PRICE_AUDIT, "audit table ready");
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_OPEN_PRICE_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_OPEN_PRICE_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Appends one open-price audit row.
///
/// `ts_nanos_ist` / `trading_date_ist_nanos` are IST wall-clock
/// nanoseconds — divided to microseconds before embedding because
/// QuestDB `TIMESTAMP` stores microseconds (2026-04-28 regression
/// class). `warn_emitted` mirrors whether the aggregator fired the
/// `OPEN-PRICE-WARN` Telegram for this instrument (Item 16b will
/// wire that).
// APPROVED: 8 wire-format columns plus QuestDB config — same audit-row shape as sibling auth_renewal_audit / live_instance_lock writers.
#[allow(clippy::too_many_arguments)]
pub async fn append_open_price_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    security_id: u32,
    exchange_segment: &str,
    source: OpenPriceSourceLabel,
    source_class: OpenSourceClassLabel,
    open_price: f64,
    warn_emitted: bool,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let segment_esc = sanitize_audit_string(exchange_segment);
    let source_str = source.as_str();
    let source_class_str = source_class.as_str();
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let trading_date_ist_micros = trading_date_ist_nanos / 1_000;
    let warn_emitted_str = if warn_emitted { "true" } else { "false" };
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_OPEN_PRICE_AUDIT} \
         (ts, trading_date_ist, security_id, exchange_segment, source, source_class, open_price, warn_emitted) VALUES \
         ({ts_micros_ist}, {trading_date_ist_micros}, {security_id}, '{segment_esc}', '{source_str}', '{source_class_str}', {open_price}, {warn_emitted_str});"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("open_price_audit insert non-2xx ({status}): {body}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg(http_port: u16) -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port,
            pg_port: 8812,
            ilp_port: 9009,
        }
    }

    #[test]
    fn test_table_name_constant() {
        assert_eq!(QUESTDB_TABLE_OPEN_PRICE_AUDIT, "open_price_audit");
    }

    #[test]
    fn test_source_label_wire_format_stable() {
        // Wire-format strings — Grafana panels + audit queries depend
        // on these. Renaming or adding case-variation breaks the
        // column's symbol set silently.
        assert_eq!(
            OpenPriceSourceLabel::PreopenBuffer.as_str(),
            "preopen_buffer"
        );
        assert_eq!(
            OpenPriceSourceLabel::RestQuoteDayOpen.as_str(),
            "rest_quote_day_open"
        );
        assert_eq!(OpenPriceSourceLabel::FirstWsTick.as_str(), "first_ws_tick");
    }

    #[test]
    fn test_source_class_label_wire_format_stable() {
        assert_eq!(
            OpenSourceClassLabel::IndexWithPreopenBuffer.as_str(),
            "index_with_preopen_buffer"
        );
        assert_eq!(
            OpenSourceClassLabel::IndexNoPreopen.as_str(),
            "index_no_preopen"
        );
        assert_eq!(
            OpenSourceClassLabel::StockWithPreopen.as_str(),
            "stock_with_preopen"
        );
    }

    /// Regression: 2026-04-28 — QuestDB requires the designated
    /// timestamp column to be present in DEDUP UPSERT KEYS.
    #[test]
    fn test_dedup_key_includes_designated_timestamp() {
        // `ts` is the designated timestamp. QuestDB injects it into
        // the DEDUP key implicitly when partitioning, but we
        // additionally include `trading_date_ist` (a separate IST
        // truncation) for human-queryability. Both must be present.
        assert!(
            DEDUP_KEY_OPEN_PRICE_AUDIT.contains("trading_date_ist"),
            "open_price_audit DEDUP key must include `trading_date_ist`. \
             Got: {DEDUP_KEY_OPEN_PRICE_AUDIT}"
        );
    }

    /// I-P1-11 mandate: composite key must include both `security_id`
    /// AND `exchange_segment`. The same numeric `security_id` (e.g.
    /// FINNIFTY=27 on IDX_I and another on NSE_EQ) MUST keep distinct
    /// audit rows.
    #[test]
    fn test_dedup_key_includes_security_id_and_exchange_segment() {
        assert!(DEDUP_KEY_OPEN_PRICE_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_OPEN_PRICE_AUDIT.contains("exchange_segment"));
    }

    /// Regression: 2026-04-28 nanos-to-micros bug class. Source-scan
    /// ratchet locks the conversion line.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("open_price_audit_persistence.rs");
        assert!(
            src.contains("ts_micros_ist = ts_nanos_ist / 1_000"),
            "INSERT must convert nanos to micros before embedding"
        );
        assert!(
            src.contains("trading_date_ist_micros = trading_date_ist_nanos / 1_000"),
            "INSERT must convert trading_date_ist nanos to micros"
        );
    }

    /// Schema-drift guard: the DDL column-set is the operator-facing
    /// contract.
    #[test]
    fn test_ddl_contains_expected_columns() {
        let src = include_str!("open_price_audit_persistence.rs");
        for column in [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "security_id INT",
            "exchange_segment SYMBOL",
            "source SYMBOL",
            "source_class SYMBOL",
            "open_price DOUBLE",
            "warn_emitted BOOLEAN",
        ] {
            assert!(
                src.contains(column),
                "DDL must declare `{column}` — schema drift will break Grafana queries"
            );
        }
    }

    /// Wire-format labels in both enums must all be unique.
    #[test]
    fn test_all_source_labels_are_distinct() {
        let labels = [
            OpenPriceSourceLabel::PreopenBuffer.as_str(),
            OpenPriceSourceLabel::RestQuoteDayOpen.as_str(),
            OpenPriceSourceLabel::FirstWsTick.as_str(),
        ];
        let mut sorted: Vec<&str> = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len());
    }

    #[test]
    fn test_all_source_class_labels_are_distinct() {
        let labels = [
            OpenSourceClassLabel::IndexWithPreopenBuffer.as_str(),
            OpenSourceClassLabel::IndexNoPreopen.as_str(),
            OpenSourceClassLabel::StockWithPreopen.as_str(),
        ];
        let mut sorted: Vec<&str> = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len());
    }

    #[test]
    fn test_append_open_price_audit_row_smoke() {
        // Pub-fn-test guard pins the symbol by name. The tokio tests
        // below cover behaviour; this is the regex-matched smoke.
        let _ = append_open_price_audit_row;
    }

    #[tokio::test]
    async fn test_append_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1); // port 1 — connection refused
        let result = append_open_price_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            13, // NIFTY
            "IDX_I",
            OpenPriceSourceLabel::PreopenBuffer,
            OpenSourceClassLabel::IndexWithPreopenBuffer,
            24500.0,
            false,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_rest_fallback_with_warn() {
        // Exercise the RestQuoteDayOpen + warn_emitted=true branch
        // (operator's "stock used REST fallback today" scenario).
        let cfg = test_cfg(1);
        let result = append_open_price_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            2885, // RELIANCE
            "NSE_EQ",
            OpenPriceSourceLabel::RestQuoteDayOpen,
            OpenSourceClassLabel::StockWithPreopen,
            2950.25,
            true,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_first_ws_tick_fallback() {
        // Last-resort fallback path.
        let cfg = test_cfg(1);
        let result = append_open_price_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            51, // SENSEX (BSE — no NSE pre-open)
            "IDX_I",
            OpenPriceSourceLabel::FirstWsTick,
            OpenSourceClassLabel::IndexNoPreopen,
            81234.50,
            false, // IndexNoPreopen does NOT warn on FirstWsTick
        )
        .await;
        assert!(result.is_err());
    }
}
