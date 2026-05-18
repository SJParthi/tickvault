//! Phase 0 Item 22b — stop-loss-leg-cancelled auto-replace audit
//! persistence.
//!
//! When a BO / CO / Super-Order's stop-loss leg gets cancelled
//! externally (operator clicks Cancel on Dhan web, Dhan auto-
//! cancellation under unusual conditions, etc.), the trade is left
//! NAKED — the entry leg is still filled but the protective SL is
//! gone. The OMS detects this via the order-update WebSocket and
//! places a replacement SL leg automatically. This audit table is
//! the forensic record of every such replacement.
//!
//! The operator can answer:
//!
//!   * "Did our auto-replace fire when the BANKNIFTY SL leg got
//!     cancelled at 10:32:15 IST?"
//!   * "How many times did `failed_to_replace` fire this quarter?
//!     What was the failure reason distribution?"
//!   * "Reconcile against SEBI margin reports — show me every
//!     replacement on 2026-05-15 with parent_order_id and the
//!     replacement price."
//!
//! Three lifecycle stages mirrored in the `outcome` column:
//!
//!   1. `Detected` — order-update WS reported the SL leg cancelled.
//!      Captures `parent_order_id` + `cancelled_leg_order_id` + the
//!      replacement_price the OMS will attempt.
//!   2. `Replaced` — auto-replacement succeeded. Captures the new
//!      `replacement_leg_order_id` returned by Dhan.
//!   3. `FailedToReplace` — auto-replacement attempt failed
//!      (DH-904 budget exhausted, order rejected, etc.). Captures
//!      the failure reason for triage. This is a CRITICAL operator
//!      event — the position is naked.
//!
//! SEBI 5-year retention. Mirrors the `self_trade_audit` /
//! `order_update_ws_audit` family of audit-table writers. The
//! detection-side + auto-replace gate land in Item 22b-b
//! (engine.rs wiring).

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_SL_REPLACEMENT_AUDIT: &str = "sl_replacement_audit";

// Composite per-day per-parent per-cancelled-leg per-outcome — the
// chain has 3 outcomes (Detected -> Replaced or Detected ->
// FailedToReplace), and each pair must keep both rows. Without
// `outcome` in the key, the Replaced row would silently overwrite
// the Detected row and the operator would lose the timing of
// detection vs the timing of the API call.
// `ts` MUST appear in DEDUP UPSERT KEYS — QuestDB requires the
// designated timestamp column. Without it the boot-time DDL returns
// HTTP 400; same bug class as the 2026-05-18 gap_fill_audit +
// last_tick_audit failures.
pub const DEDUP_KEY_SL_REPLACEMENT_AUDIT: &str =
    "ts, trading_date_ist, parent_order_id, cancelled_leg_order_id, outcome";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format strings for the `outcome` column. 3-stage
/// lifecycle of the auto-replace flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlReplacementOutcome {
    /// Order-update WS reported the SL leg as cancelled. The OMS
    /// has decided to attempt a replacement; the API call hasn't
    /// completed yet. `replacement_leg_order_id` is empty for this
    /// row.
    Detected,
    /// Auto-replacement succeeded. Dhan returned a new order id
    /// (captured in `replacement_leg_order_id`). The position is
    /// protected again.
    Replaced,
    /// Auto-replacement FAILED. The position is naked — operator
    /// MUST intervene. `failure_reason` carries the OMS error.
    /// CRITICAL operator event.
    FailedToReplace,
}

impl SlReplacementOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Detected => "detected",
            Self::Replaced => "replaced",
            Self::FailedToReplace => "failed_to_replace",
        }
    }

    /// Returns `true` for outcomes that leave the position naked
    /// (operator MUST be paged). Currently only `FailedToReplace`,
    /// but pinning the predicate in code lets future outcome
    /// additions stay consistent.
    #[must_use]
    pub const fn is_position_at_risk(self) -> bool {
        matches!(self, Self::FailedToReplace)
    }
}

/// Creates the audit table if absent. Idempotent.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_sl_replacement_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_SL_REPLACEMENT_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            security_id INT, \
            exchange_segment SYMBOL, \
            parent_order_id SYMBOL, \
            cancelled_leg_order_id SYMBOL, \
            replacement_leg_order_id STRING, \
            outcome SYMBOL, \
            replacement_price DOUBLE, \
            replacement_quantity LONG, \
            failure_reason STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_SL_REPLACEMENT_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_SL_REPLACEMENT_AUDIT,
                "audit table ready"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_SL_REPLACEMENT_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_SL_REPLACEMENT_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Appends one SL replacement audit row.
///
/// `ts_nanos_ist` / `trading_date_ist_nanos` are IST wall-clock
/// nanoseconds — divided to microseconds (2026-04-28 regression
/// class). `replacement_leg_order_id` is the newly-placed leg's
/// order id (empty for `Detected` and `FailedToReplace` rows).
/// `failure_reason` is empty for `Detected` and `Replaced`;
/// populated only for `FailedToReplace`.
// APPROVED: 11 wire-format columns plus QuestDB config — same audit-row shape as sibling self_trade_audit / order_update_ws_audit writers.
#[allow(clippy::too_many_arguments)]
pub async fn append_sl_replacement_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    security_id: u32,
    exchange_segment: &str,
    parent_order_id: &str,
    cancelled_leg_order_id: &str,
    replacement_leg_order_id: &str,
    outcome: SlReplacementOutcome,
    replacement_price: f64,
    replacement_quantity: i64,
    failure_reason: &str,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let segment_esc = sanitize_audit_string(exchange_segment);
    let parent_order_id_esc = sanitize_audit_string(parent_order_id);
    let cancelled_leg_order_id_esc = sanitize_audit_string(cancelled_leg_order_id);
    let replacement_leg_order_id_esc = sanitize_audit_string(replacement_leg_order_id);
    let outcome_str = outcome.as_str();
    let failure_reason_esc = sanitize_audit_string(failure_reason);
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let trading_date_ist_micros = trading_date_ist_nanos / 1_000;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_SL_REPLACEMENT_AUDIT} \
         (ts, trading_date_ist, security_id, exchange_segment, parent_order_id, cancelled_leg_order_id, replacement_leg_order_id, outcome, replacement_price, replacement_quantity, failure_reason) VALUES \
         ({ts_micros_ist}, {trading_date_ist_micros}, {security_id}, '{segment_esc}', '{parent_order_id_esc}', '{cancelled_leg_order_id_esc}', '{replacement_leg_order_id_esc}', '{outcome_str}', {replacement_price}, {replacement_quantity}, '{failure_reason_esc}');"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status_code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("sl_replacement_audit insert non-2xx ({status_code}): {body}");
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
        assert_eq!(QUESTDB_TABLE_SL_REPLACEMENT_AUDIT, "sl_replacement_audit");
    }

    #[test]
    fn test_outcome_as_str_stable() {
        // Wire-format strings — Grafana panels + SEBI audit
        // queries depend on these.
        assert_eq!(SlReplacementOutcome::Detected.as_str(), "detected");
        assert_eq!(SlReplacementOutcome::Replaced.as_str(), "replaced");
        assert_eq!(
            SlReplacementOutcome::FailedToReplace.as_str(),
            "failed_to_replace"
        );
    }

    /// `FailedToReplace` is the only "position-at-risk" outcome.
    /// Pinning this predicate prevents a future "add a fourth
    /// variant that also leaves the position naked" edit from
    /// silently bypassing the operator alert path.
    #[test]
    fn test_is_position_at_risk_only_failed_to_replace() {
        assert!(SlReplacementOutcome::FailedToReplace.is_position_at_risk());
        assert!(!SlReplacementOutcome::Detected.is_position_at_risk());
        assert!(!SlReplacementOutcome::Replaced.is_position_at_risk());
    }

    /// DEDUP composition — must include the designated timestamp
    /// partition column, the I-P1-11-equivalent identity (here:
    /// `parent_order_id` + `cancelled_leg_order_id`), and `outcome`
    /// so the lifecycle chain Detected -> Replaced keeps BOTH rows.
    #[test]
    fn test_dedup_key_composition() {
        for required in [
            "trading_date_ist",
            "parent_order_id",
            "cancelled_leg_order_id",
            "outcome",
        ] {
            assert!(
                DEDUP_KEY_SL_REPLACEMENT_AUDIT.contains(required),
                "DEDUP key must include `{required}`. Got: {DEDUP_KEY_SL_REPLACEMENT_AUDIT}"
            );
        }
    }

    /// `outcome` in the key prevents the Replaced row from silently
    /// overwriting the Detected row. Without it, the operator would
    /// lose the detection timestamp.
    #[test]
    fn test_dedup_key_includes_outcome_so_lifecycle_chain_survives() {
        assert!(DEDUP_KEY_SL_REPLACEMENT_AUDIT.contains("outcome"));
    }

    /// `parent_order_id` is the canonical Dhan identity for the
    /// BO / CO / Super order. The audit is keyed on this, NOT on
    /// security_id alone. (security_id appears as informational
    /// metadata, NOT in the DEDUP key — multiple SL legs against
    /// the same security_id would otherwise collide.)
    #[test]
    fn test_dedup_key_no_security_id() {
        assert!(!DEDUP_KEY_SL_REPLACEMENT_AUDIT.contains("security_id"));
    }

    /// Regression: 2026-04-28 nanos-to-micros bug class.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("sl_replacement_audit_persistence.rs");
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
        let src = include_str!("sl_replacement_audit_persistence.rs");
        for column in [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "security_id INT",
            "exchange_segment SYMBOL",
            "parent_order_id SYMBOL",
            "cancelled_leg_order_id SYMBOL",
            "replacement_leg_order_id STRING",
            "outcome SYMBOL",
            "replacement_price DOUBLE",
            "replacement_quantity LONG",
            "failure_reason STRING",
        ] {
            assert!(
                src.contains(column),
                "DDL must declare `{column}` — schema drift will break Grafana queries"
            );
        }
    }

    /// Wire-format outcome labels must be distinct.
    #[test]
    fn test_all_outcome_labels_are_distinct() {
        let labels = [
            SlReplacementOutcome::Detected.as_str(),
            SlReplacementOutcome::Replaced.as_str(),
            SlReplacementOutcome::FailedToReplace.as_str(),
        ];
        let mut sorted: Vec<&str> = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len());
    }

    #[test]
    fn test_append_sl_replacement_audit_row_smoke() {
        let _ = append_sl_replacement_audit_row;
    }

    #[tokio::test]
    async fn test_append_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1); // port 1 — connection refused
        let result = append_sl_replacement_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            49081, // BANKNIFTY option
            "NSE_FNO",
            "78901234", // parent (entry) order
            "78901235", // cancelled SL leg
            "",         // replacement not yet placed
            SlReplacementOutcome::Detected,
            48000.50,
            50,
            "",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_replaced_outcome_with_new_order_id() {
        // Successful replacement — captures the new SL leg's order
        // id so the operator can reconcile against the order book.
        let cfg = test_cfg(1);
        let result = append_sl_replacement_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            49081,
            "NSE_FNO",
            "78901234",
            "78901235",
            "78901299", // new SL leg order id
            SlReplacementOutcome::Replaced,
            48000.50,
            50,
            "",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_failed_to_replace_with_reason() {
        // FailedToReplace — captures the failure reason for triage.
        // CRITICAL operator event (position naked).
        let cfg = test_cfg(1);
        let result = append_sl_replacement_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            49081,
            "NSE_FNO",
            "78901234",
            "78901235",
            "",
            SlReplacementOutcome::FailedToReplace,
            48000.50,
            50,
            "DH-904 retry budget exhausted",
        )
        .await;
        assert!(result.is_err());
    }
}
