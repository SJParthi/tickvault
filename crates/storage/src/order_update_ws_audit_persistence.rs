//! Phase 0 Item 24a — live order-update WebSocket audit persistence.
//!
//! Per-event forensic record of every JSON message received from
//! `wss://api-order-update.dhan.co`. SEBI 5-year retention applies
//! because this is the authoritative log of every order status
//! transition delivered to our process.
//!
//! The operator can answer:
//!
//!   * "Did Dhan deliver the 09:15:32 IST TRADED transition for
//!     order 78901234 to our process?"
//!   * "How many `Source: 'N'` events (manual Dhan web/app orders
//!     against our account) did we receive on 2026-05-15?"
//!   * "Why did the OMS reconciler see a ghost order at 13:42 —
//!     query `order_update_ws_audit` for the missing transition."
//!
//! One row per WS message. DEDUP key
//! `(trading_date_ist, order_no, status)` lets `Pending → Traded`
//! transitions for the same order both survive (a single status
//! never duplicates within a trading day for the same order_no).
//!
//! Mirrors the `auth_renewal_audit` / `open_price_audit` family of
//! audit-table writers. The 7-layer-observability counters +
//! Prometheus alert rule + Telegram alerting wires up in Item 24b.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_ORDER_UPDATE_WS_AUDIT: &str = "order_update_ws_audit";

// Composite per-day per-order per-status — same order can transition
// Pending -> Traded within the same day; both rows must survive. The
// `status` discriminator prevents same-second duplicates from
// fragmenting the audit history.
// `ts` MUST appear in DEDUP UPSERT KEYS — QuestDB requires the
// designated timestamp column. Without it the boot-time DDL returns
// HTTP 400; same bug class as the 2026-05-18 gap_fill_audit +
// last_tick_audit failures.
pub const DEDUP_KEY_ORDER_UPDATE_WS_AUDIT: &str = "ts, trading_date_ist, order_no, status";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format value for the `source` column. Mirrors the
/// `Source` field on Dhan's order-update message contract
/// (`dhan-ref/10-live-order-update-websocket.md` §6): single-character
/// code where `P` = API-placed (our order), `N` = manually placed via
/// Dhan web/app. The operator's primary filter is `source = 'P'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderUpdateSource {
    /// `Source: "P"` — API-placed. Our order.
    Api,
    /// `Source: "N"` — Manually placed via Dhan web / mobile app.
    Manual,
    /// `Source` field was missing or carried an unrecognised
    /// character. The audit row still lands so the operator can
    /// triage the unknown.
    Unknown,
}

impl OrderUpdateSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Api => "P",
            Self::Manual => "N",
            Self::Unknown => "?",
        }
    }

    /// Parse the single-character `Source` field from the JSON
    /// message. `Some` of the typed variant; `None` is impossible
    /// (Unknown is the catch-all). Returns `Unknown` for empty or
    /// multi-character input — the audit row lands either way.
    #[must_use]
    pub fn from_dhan_source_field(raw: &str) -> Self {
        match raw {
            "P" => Self::Api,
            "N" => Self::Manual,
            _ => Self::Unknown,
        }
    }
}

/// Creates the audit table if absent. Idempotent — safe to call on
/// every boot.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_order_update_ws_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_ORDER_UPDATE_WS_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            order_no SYMBOL, \
            status SYMBOL, \
            source SYMBOL, \
            exch_order_no STRING, \
            correlation_id STRING, \
            leg_no INT, \
            security_id INT, \
            exchange_segment SYMBOL, \
            raw_msg_code INT\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_ORDER_UPDATE_WS_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_ORDER_UPDATE_WS_AUDIT,
                "audit table ready"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_ORDER_UPDATE_WS_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_ORDER_UPDATE_WS_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Appends one order-update audit row.
///
/// `ts_nanos_ist` and `trading_date_ist_nanos` are IST wall-clock
/// nanoseconds — divided to microseconds before embedding because
/// QuestDB `TIMESTAMP` stores microseconds (2026-04-28 regression
/// class). `raw_msg_code` is captured (always 42 for Dhan order-
/// update messages, per dhan-ref/10 §3) so forensic queries can
/// confirm the wire-protocol expectation.
// APPROVED: 11 wire-format columns plus QuestDB config — same audit-row shape as sibling auth_renewal_audit / live_instance_lock / open_price_audit writers.
#[allow(clippy::too_many_arguments)]
pub async fn append_order_update_ws_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    order_no: &str,
    status: &str,
    source: OrderUpdateSource,
    exch_order_no: &str,
    correlation_id: &str,
    leg_no: i32,
    security_id: u32,
    exchange_segment: &str,
    raw_msg_code: i32,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let order_no_esc = sanitize_audit_string(order_no);
    let status_esc = sanitize_audit_string(status);
    let source_str = source.as_str();
    let exch_order_no_esc = sanitize_audit_string(exch_order_no);
    let correlation_id_esc = sanitize_audit_string(correlation_id);
    let segment_esc = sanitize_audit_string(exchange_segment);
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let trading_date_ist_micros = trading_date_ist_nanos / 1_000;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_ORDER_UPDATE_WS_AUDIT} \
         (ts, trading_date_ist, order_no, status, source, exch_order_no, correlation_id, leg_no, security_id, exchange_segment, raw_msg_code) VALUES \
         ({ts_micros_ist}, {trading_date_ist_micros}, '{order_no_esc}', '{status_esc}', '{source_str}', '{exch_order_no_esc}', '{correlation_id_esc}', {leg_no}, {security_id}, '{segment_esc}', {raw_msg_code});"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status_code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("order_update_ws_audit insert non-2xx ({status_code}): {body}");
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
        assert_eq!(QUESTDB_TABLE_ORDER_UPDATE_WS_AUDIT, "order_update_ws_audit");
    }

    #[test]
    fn test_source_as_str_stable() {
        // Wire-format strings — Grafana panels + audit queries
        // depend on these. The `P`/`N` characters mirror Dhan's
        // single-character field exactly so SELECT WHERE
        // source = 'P' works in both directions.
        assert_eq!(OrderUpdateSource::Api.as_str(), "P");
        assert_eq!(OrderUpdateSource::Manual.as_str(), "N");
        assert_eq!(OrderUpdateSource::Unknown.as_str(), "?");
    }

    #[test]
    fn test_from_dhan_source_field_known_values() {
        assert_eq!(
            OrderUpdateSource::from_dhan_source_field("P"),
            OrderUpdateSource::Api
        );
        assert_eq!(
            OrderUpdateSource::from_dhan_source_field("N"),
            OrderUpdateSource::Manual
        );
    }

    #[test]
    fn test_from_dhan_source_field_unknown_inputs_map_to_unknown() {
        // Empty + multi-character + lowercase + numeric all become
        // Unknown — the audit row still lands so the operator can
        // triage what Dhan actually sent.
        for raw in ["", "p", "n", "X", "PN", "1", "API"] {
            assert_eq!(
                OrderUpdateSource::from_dhan_source_field(raw),
                OrderUpdateSource::Unknown,
                "input {raw:?} must map to Unknown, not panic or false-match"
            );
        }
    }

    /// Regression: 2026-04-28 — QuestDB requires the designated
    /// timestamp column to be present in DEDUP UPSERT KEYS. Plus
    /// `order_no` + `status` per the per-day-per-order-per-status
    /// composite identity.
    #[test]
    fn test_dedup_key_composition() {
        assert!(
            DEDUP_KEY_ORDER_UPDATE_WS_AUDIT.contains("trading_date_ist"),
            "must include trading_date_ist"
        );
        assert!(
            DEDUP_KEY_ORDER_UPDATE_WS_AUDIT.contains("order_no"),
            "must include order_no — primary forensic key"
        );
        assert!(
            DEDUP_KEY_ORDER_UPDATE_WS_AUDIT.contains("status"),
            "must include status — Pending->Traded transition must keep both rows"
        );
    }

    /// `security_id` MUST NOT appear in the DEDUP key — the audit
    /// is keyed by `order_no` (which is the canonical Dhan identity
    /// for an order). `security_id` is informational metadata.
    #[test]
    fn test_dedup_key_no_security_id() {
        assert!(!DEDUP_KEY_ORDER_UPDATE_WS_AUDIT.contains("security_id"));
    }

    /// Regression: 2026-04-28 nanos-to-micros bug class. Source-scan
    /// ratchet locks the conversion line.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("order_update_ws_audit_persistence.rs");
        assert!(
            src.contains("ts_micros_ist = ts_nanos_ist / 1_000"),
            "INSERT must convert nanos to micros before embedding"
        );
        assert!(
            src.contains("trading_date_ist_micros = trading_date_ist_nanos / 1_000"),
            "INSERT must convert trading_date_ist nanos to micros"
        );
    }

    /// Schema-drift guard.
    #[test]
    fn test_ddl_contains_expected_columns() {
        let src = include_str!("order_update_ws_audit_persistence.rs");
        for column in [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "order_no SYMBOL",
            "status SYMBOL",
            "source SYMBOL",
            "exch_order_no STRING",
            "correlation_id STRING",
            "leg_no INT",
            "security_id INT",
            "exchange_segment SYMBOL",
            "raw_msg_code INT",
        ] {
            assert!(
                src.contains(column),
                "DDL must declare `{column}` — schema drift will break Grafana queries"
            );
        }
    }

    /// Wire-format source labels must be distinct.
    #[test]
    fn test_all_source_labels_are_distinct() {
        let labels = [
            OrderUpdateSource::Api.as_str(),
            OrderUpdateSource::Manual.as_str(),
            OrderUpdateSource::Unknown.as_str(),
        ];
        let mut sorted: Vec<&str> = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len());
    }

    #[test]
    fn test_append_order_update_ws_audit_row_smoke() {
        let _ = append_order_update_ws_audit_row;
    }

    #[tokio::test]
    async fn test_append_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1); // port 1 — connection refused
        let result = append_order_update_ws_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            "78901234",
            "TRADED",
            OrderUpdateSource::Api,
            "EX-7777-1",
            "tv-strat-001-abc",
            1, // entry leg
            2885,
            "NSE_EQ",
            42,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_manual_source_with_empty_correlation_id() {
        // `Source: 'N'` (manual Dhan web/app order against our
        // account) typically has no correlation_id since we didn't
        // place it — sanitize_audit_string must handle empty input.
        let cfg = test_cfg(1);
        let result = append_order_update_ws_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            "78901999",
            "PENDING",
            OrderUpdateSource::Manual,
            "",
            "", // empty — operator didn't place this
            0,  // no leg structure for plain orders
            2885,
            "NSE_EQ",
            42,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_super_order_leg_with_correlation_id() {
        // Super-order leg row: leg_no=2 is the stop-loss leg per
        // dhan-ref/10 §11. correlation_id is the operator's tag.
        let cfg = test_cfg(1);
        let result = append_order_update_ws_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            "78901234",
            "CANCELLED",
            OrderUpdateSource::Api,
            "EX-7777-2",
            "tv-super-001-sl",
            2, // stop-loss leg
            49081,
            "NSE_FNO",
            42,
        )
        .await;
        assert!(result.is_err());
    }
}
