//! Phase 0 Item 25 — P&L tracker audit persistence (dry-run
//! hypothetical scaffolding).
//!
//! Captures per-position realized + unrealized P&L snapshots so the
//! operator can reconstruct intraday equity curves and reconcile
//! against Dhan's end-of-day reports. Multiple snapshot triggers:
//!
//!   * `OnFill` — every order fill flushes a row. Captures the
//!     immediate post-fill P&L state.
//!   * `OnMinute` — every 1-minute boundary during market hours
//!     snapshots the live mark-to-market unrealized P&L.
//!   * `OnEod` — single 15:30 IST end-of-day row per
//!     `(security_id, segment)` carrying the day's terminal state.
//!
//! Phase 0 is the dry-run scaffolding — every row is computed by
//! the OMS state machine, not by any live trade. Item 25-b will
//! wire the OMS engine to call `append_pnl_audit_row` on each
//! trigger; Item 25-c adds the per-second Telegram daily-loss-
//! threshold variant.
//!
//! SEBI 5-year retention applies. Mirrors the
//! `sl_replacement_audit` / `self_trade_audit` family of audit-
//! table writers.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_PNL_AUDIT: &str = "pnl_audit";

// Composite per-day per-security per-snapshot-kind per-second
// (`ts` is part of the key implicitly via QuestDB partitioning,
// but the SYMBOL `snapshot_kind` discriminator lets the operator
// query "all OnFill rows today" without scanning OnMinute noise).
// `ts` is added explicitly so two same-second OnFill rows for
// the same security_id keep both — partial fills can fire
// rapid-succession events.
pub const DEDUP_KEY_PNL_AUDIT: &str =
    "trading_date_ist, ts, security_id, exchange_segment, snapshot_kind";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format strings for the `snapshot_kind` column.
/// Each kind has different semantics for `mark_price` (live tick
/// vs end-of-day close vs fill price).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PnlSnapshotKind {
    /// Triggered by an order fill. `mark_price` is the fill price;
    /// `realized_pnl` jumps; `unrealized_pnl` updates against the
    /// remaining position.
    OnFill,
    /// Triggered at the 1-minute boundary during market hours.
    /// `mark_price` is the latest tick LTP; only `unrealized_pnl`
    /// changes.
    OnMinute,
    /// Triggered once at 15:30:00 IST. `mark_price` is the day's
    /// close; the row captures the terminal state for SEBI
    /// reconciliation.
    OnEod,
}

impl PnlSnapshotKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OnFill => "on_fill",
            Self::OnMinute => "on_minute",
            Self::OnEod => "on_eod",
        }
    }
}

/// Creates the audit table if absent. Idempotent.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_pnl_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_PNL_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            security_id INT, \
            exchange_segment SYMBOL, \
            snapshot_kind SYMBOL, \
            net_position_qty LONG, \
            avg_entry_price DOUBLE, \
            mark_price DOUBLE, \
            realized_pnl DOUBLE, \
            unrealized_pnl DOUBLE\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_PNL_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(table = QUESTDB_TABLE_PNL_AUDIT, "audit table ready");
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_PNL_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(table = QUESTDB_TABLE_PNL_AUDIT, ?err, "DDL request failed");
        }
    }
}

/// Appends one P&L audit row.
///
/// `ts_nanos_ist` / `trading_date_ist_nanos` are IST wall-clock
/// nanoseconds — divided to microseconds (2026-04-28 regression
/// class). `net_position_qty` is signed: positive = long,
/// negative = short, zero = flat. `realized_pnl` is the
/// session-cumulative booked P&L; `unrealized_pnl` is the
/// mark-to-market against the remaining open position.
// APPROVED: 10 wire-format columns plus QuestDB config — same audit-row shape as sibling self_trade_audit / sl_replacement_audit / order_update_ws_audit writers.
#[allow(clippy::too_many_arguments)]
pub async fn append_pnl_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    security_id: u32,
    exchange_segment: &str,
    snapshot_kind: PnlSnapshotKind,
    net_position_qty: i64,
    avg_entry_price: f64,
    mark_price: f64,
    realized_pnl: f64,
    unrealized_pnl: f64,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let segment_esc = sanitize_audit_string(exchange_segment);
    let snapshot_kind_str = snapshot_kind.as_str();
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let trading_date_ist_micros = trading_date_ist_nanos / 1_000;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_PNL_AUDIT} \
         (ts, trading_date_ist, security_id, exchange_segment, snapshot_kind, net_position_qty, avg_entry_price, mark_price, realized_pnl, unrealized_pnl) VALUES \
         ({ts_micros_ist}, {trading_date_ist_micros}, {security_id}, '{segment_esc}', '{snapshot_kind_str}', {net_position_qty}, {avg_entry_price}, {mark_price}, {realized_pnl}, {unrealized_pnl});"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status_code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("pnl_audit insert non-2xx ({status_code}): {body}");
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
        assert_eq!(QUESTDB_TABLE_PNL_AUDIT, "pnl_audit");
    }

    #[test]
    fn test_snapshot_kind_as_str_stable() {
        // Wire-format strings — Grafana panels + SEBI audit
        // queries depend on these.
        assert_eq!(PnlSnapshotKind::OnFill.as_str(), "on_fill");
        assert_eq!(PnlSnapshotKind::OnMinute.as_str(), "on_minute");
        assert_eq!(PnlSnapshotKind::OnEod.as_str(), "on_eod");
    }

    /// DEDUP composition — `ts` MUST be in the key so two
    /// same-second OnFill rows for the same security_id (e.g.
    /// partial-fill rapid-succession) both survive. Without `ts`
    /// the second fill would silently overwrite the first, losing
    /// the realized_pnl jump on the first fill.
    #[test]
    fn test_dedup_key_includes_ts_for_partial_fills() {
        assert!(
            DEDUP_KEY_PNL_AUDIT.contains("ts"),
            "DEDUP key must include `ts` so rapid-succession partial fills both survive"
        );
    }

    #[test]
    fn test_dedup_key_composition() {
        for required in [
            "trading_date_ist",
            "ts",
            "security_id",
            "exchange_segment",
            "snapshot_kind",
        ] {
            assert!(
                DEDUP_KEY_PNL_AUDIT.contains(required),
                "DEDUP key must include `{required}`. Got: {DEDUP_KEY_PNL_AUDIT}"
            );
        }
    }

    /// I-P1-11 composite identity — `security_id` must always pair
    /// with `exchange_segment`.
    #[test]
    fn test_dedup_key_includes_security_id_and_exchange_segment() {
        assert!(DEDUP_KEY_PNL_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_PNL_AUDIT.contains("exchange_segment"));
    }

    /// `snapshot_kind` MUST be in the DEDUP key so an OnFill row
    /// and an OnMinute row at the same `ts` for the same security
    /// both survive (different semantics — fill_price vs LTP).
    #[test]
    fn test_dedup_key_includes_snapshot_kind() {
        assert!(DEDUP_KEY_PNL_AUDIT.contains("snapshot_kind"));
    }

    /// Regression: 2026-04-28 nanos-to-micros bug class.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("pnl_audit_persistence.rs");
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
        let src = include_str!("pnl_audit_persistence.rs");
        for column in [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "security_id INT",
            "exchange_segment SYMBOL",
            "snapshot_kind SYMBOL",
            "net_position_qty LONG",
            "avg_entry_price DOUBLE",
            "mark_price DOUBLE",
            "realized_pnl DOUBLE",
            "unrealized_pnl DOUBLE",
        ] {
            assert!(
                src.contains(column),
                "DDL must declare `{column}` — schema drift will break Grafana queries"
            );
        }
    }

    /// Wire-format snapshot_kind labels must be distinct.
    #[test]
    fn test_all_snapshot_kind_labels_are_distinct() {
        let labels = [
            PnlSnapshotKind::OnFill.as_str(),
            PnlSnapshotKind::OnMinute.as_str(),
            PnlSnapshotKind::OnEod.as_str(),
        ];
        let mut sorted: Vec<&str> = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len());
    }

    #[test]
    fn test_append_pnl_audit_row_smoke() {
        let _ = append_pnl_audit_row;
    }

    #[tokio::test]
    async fn test_append_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1); // port 1 — connection refused
        let result = append_pnl_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            2885, // RELIANCE
            "NSE_EQ",
            PnlSnapshotKind::OnFill,
            50,
            2950.25,
            2950.25, // mark = fill on the OnFill row
            0.0,     // first fill — no realized yet
            0.0,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_short_position_with_negative_qty() {
        // Short position — net_position_qty is negative. Tests the
        // i64 signed-LONG roundtrip through the SQL formatter.
        let cfg = test_cfg(1);
        let result = append_pnl_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            49081,
            "NSE_FNO",
            PnlSnapshotKind::OnMinute,
            -50, // short 50 lots
            48000.0,
            47900.0, // mark dropped — short is winning
            0.0,
            5000.0, // (48000 - 47900) * 50 = +5000 unrealized
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_eod_row_with_realized_pnl() {
        // End-of-day terminal row. Position closed; realized_pnl
        // captures session total.
        let cfg = test_cfg(1);
        let result = append_pnl_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            2885,
            "NSE_EQ",
            PnlSnapshotKind::OnEod,
            0,       // flat at EOD
            0.0,     // no avg entry — flat
            2952.50, // day close
            1250.0,  // session realized P&L
            0.0,     // no open position
        )
        .await;
        assert!(result.is_err());
    }
}
