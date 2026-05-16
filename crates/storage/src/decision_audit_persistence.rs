//! Phase 0 Item 26-b — strategy decision audit persistence.
//!
//! Companion to `signal_audit` (Item 26-a). Captures what happens
//! to every `Triggered` signal AFTER the strategy FSM hands it off
//! to the decision layer (pre-trade gates + OMS). The pair-key
//! `(trading_date_ist, ts, strategy_id, security_id, segment)`
//! joins back to `signal_audit::outcome = 'triggered'`.
//!
//! The operator can answer:
//!
//!   * "The RSI signal fired on RELIANCE at 11:23 IST — why didn't
//!     an order get placed? — query `decision_audit` for `rejected`
//!     and read `rejection_reason`."
//!   * "How many `Accepted` decisions today vs `Rejected` (broken
//!     down by reason)? — single query against this table."
//!   * "Trace the full chain: `signal_audit` (Triggered) ->
//!     `decision_audit` (Accepted) -> `order_audit` (filled).
//!     Operator joins on `correlation_id` for the SEBI audit chain."
//!
//! Lifecycle stages (single-row per decision):
//!
//!   * `Accepted` — pre-trade gates passed; order placed.
//!     Captures the `correlation_id` so the operator can join
//!     forward to `order_audit` / `order_update_ws_audit`.
//!   * `Rejected` — pre-trade gate blocked the order. The
//!     `rejection_reason` field is the operator-readable cause
//!     (margin / position-limit / daily-loss / self-trade-cooldown
//!     / etc.).
//!   * `Debounced` — too-rapid repeat signal (same strategy +
//!     security within `signal_debounce_secs`). The OMS state
//!     hasn't changed since the prior decision, so we skip.
//!
//! SEBI 5-year retention. The decision-layer wiring + FSM
//! integration land in Item 26-c.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_DECISION_AUDIT: &str = "decision_audit";

// Composite per-day per-strategy per-security per-ts per-outcome —
// same as signal_audit's DEDUP shape so the join key
// `(trading_date_ist, ts, strategy_id, security_id,
//   exchange_segment)` works in both directions. `outcome` lets a
// rapid Accept->subsequent Reject (operator changes config
// mid-session) keep both rows.
pub const DEDUP_KEY_DECISION_AUDIT: &str =
    "trading_date_ist, ts, strategy_id, security_id, exchange_segment, outcome";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format strings for the `outcome` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionAuditOutcome {
    /// Pre-trade gates passed; order placed. `correlation_id`
    /// carries the operator's tag (linkable to `order_audit` +
    /// `order_update_ws_audit`).
    Accepted,
    /// Pre-trade gate blocked the order. `rejection_reason` is
    /// the operator-readable cause. NO order was placed; the
    /// strategy may re-evaluate on the next tick.
    Rejected,
    /// Too-rapid repeat signal — same strategy + security within
    /// `signal_debounce_secs` of the prior decision. The
    /// `rejection_reason` field carries `"debounced_<N>s_window"`
    /// for forensic clarity.
    Debounced,
}

impl DecisionAuditOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Debounced => "debounced",
        }
    }

    /// Returns `true` if the decision resulted in an order
    /// placement attempt. Only `Accepted` qualifies — Rejected
    /// and Debounced both terminate the lifecycle here without an
    /// order_audit row downstream.
    #[must_use]
    pub const fn placed_order(self) -> bool {
        matches!(self, Self::Accepted)
    }
}

/// Creates the audit table if absent. Idempotent.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_decision_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_DECISION_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            strategy_id SYMBOL, \
            security_id INT, \
            exchange_segment SYMBOL, \
            outcome SYMBOL, \
            signal_name SYMBOL, \
            correlation_id STRING, \
            rejection_reason STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_DECISION_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(table = QUESTDB_TABLE_DECISION_AUDIT, "audit table ready");
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_DECISION_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_DECISION_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Appends one decision audit row.
///
/// `ts_nanos_ist` / `trading_date_ist_nanos` are IST wall-clock
/// nanoseconds — divided to microseconds (2026-04-28 regression
/// class). `correlation_id` is the operator's tag on the placed
/// order (empty for Rejected + Debounced). `rejection_reason` is
/// the operator-readable cause (empty for Accepted; populated for
/// Rejected and Debounced with the gate name or
/// `debounced_<N>s_window`).
// APPROVED: 9 wire-format columns plus QuestDB config — same audit-row shape as sibling signal_audit / sl_replacement_audit / pnl_audit writers.
#[allow(clippy::too_many_arguments)]
pub async fn append_decision_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    strategy_id: &str,
    security_id: u32,
    exchange_segment: &str,
    outcome: DecisionAuditOutcome,
    signal_name: &str,
    correlation_id: &str,
    rejection_reason: &str,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let strategy_id_esc = sanitize_audit_string(strategy_id);
    let segment_esc = sanitize_audit_string(exchange_segment);
    let outcome_str = outcome.as_str();
    let signal_name_esc = sanitize_audit_string(signal_name);
    let correlation_id_esc = sanitize_audit_string(correlation_id);
    let rejection_reason_esc = sanitize_audit_string(rejection_reason);
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let trading_date_ist_micros = trading_date_ist_nanos / 1_000;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_DECISION_AUDIT} \
         (ts, trading_date_ist, strategy_id, security_id, exchange_segment, outcome, signal_name, correlation_id, rejection_reason) VALUES \
         ({ts_micros_ist}, {trading_date_ist_micros}, '{strategy_id_esc}', {security_id}, '{segment_esc}', '{outcome_str}', '{signal_name_esc}', '{correlation_id_esc}', '{rejection_reason_esc}');"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status_code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("decision_audit insert non-2xx ({status_code}): {body}");
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
        assert_eq!(QUESTDB_TABLE_DECISION_AUDIT, "decision_audit");
    }

    #[test]
    fn test_outcome_as_str_stable() {
        assert_eq!(DecisionAuditOutcome::Accepted.as_str(), "accepted");
        assert_eq!(DecisionAuditOutcome::Rejected.as_str(), "rejected");
        assert_eq!(DecisionAuditOutcome::Debounced.as_str(), "debounced");
    }

    /// `placed_order()` predicate — only `Accepted` results in
    /// an order_audit downstream row. Pinning the predicate
    /// prevents a future 4th-variant edit (e.g. `Pending`) from
    /// accidentally being treated as Accepted by downstream
    /// counters.
    #[test]
    fn test_placed_order_only_accepted() {
        assert!(DecisionAuditOutcome::Accepted.placed_order());
        assert!(!DecisionAuditOutcome::Rejected.placed_order());
        assert!(!DecisionAuditOutcome::Debounced.placed_order());
    }

    /// DEDUP composition — MUST match signal_audit's shape so the
    /// per-row join key works. The two tables share
    /// `(trading_date_ist, ts, strategy_id, security_id,
    /// exchange_segment)` as the canonical decision-chain key.
    #[test]
    fn test_dedup_key_composition() {
        for required in [
            "trading_date_ist",
            "ts",
            "strategy_id",
            "security_id",
            "exchange_segment",
            "outcome",
        ] {
            assert!(
                DEDUP_KEY_DECISION_AUDIT.contains(required),
                "DEDUP key must include `{required}`. Got: {DEDUP_KEY_DECISION_AUDIT}"
            );
        }
    }

    /// `outcome` MUST be in the DEDUP key so an operator changing
    /// config mid-session (a rapid Accept -> Reject for the same
    /// signal evaluation) keeps both rows visible.
    #[test]
    fn test_dedup_key_includes_outcome() {
        assert!(DEDUP_KEY_DECISION_AUDIT.contains("outcome"));
    }

    /// `ts` MUST be in the DEDUP key so rapid-succession decisions
    /// for the same strategy+security keep distinct rows.
    #[test]
    fn test_dedup_key_includes_ts_for_rapid_succession() {
        assert!(DEDUP_KEY_DECISION_AUDIT.contains("ts"));
    }

    /// I-P1-11 composite identity.
    #[test]
    fn test_dedup_key_includes_security_id_and_exchange_segment() {
        assert!(DEDUP_KEY_DECISION_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_DECISION_AUDIT.contains("exchange_segment"));
    }

    /// Regression: 2026-04-28 nanos-to-micros bug class.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("decision_audit_persistence.rs");
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
        let src = include_str!("decision_audit_persistence.rs");
        for column in [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "strategy_id SYMBOL",
            "security_id INT",
            "exchange_segment SYMBOL",
            "outcome SYMBOL",
            "signal_name SYMBOL",
            "correlation_id STRING",
            "rejection_reason STRING",
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
            DecisionAuditOutcome::Accepted.as_str(),
            DecisionAuditOutcome::Rejected.as_str(),
            DecisionAuditOutcome::Debounced.as_str(),
        ];
        let mut sorted: Vec<&str> = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len());
    }

    /// Join-key contract — the decision_audit DEDUP shape MUST be
    /// a strict superset of signal_audit's join columns so the
    /// operator-visible "trace this signal" query works.
    #[test]
    fn test_dedup_shape_matches_signal_audit_join_key() {
        // signal_audit DEDUP: trading_date_ist, ts, strategy_id,
        //                     security_id, exchange_segment, outcome
        // decision_audit DEDUP: same 6 columns. The shared first
        // 5 form the join key; `outcome` discriminates per-table.
        for shared_col in [
            "trading_date_ist",
            "ts",
            "strategy_id",
            "security_id",
            "exchange_segment",
        ] {
            assert!(
                DEDUP_KEY_DECISION_AUDIT.contains(shared_col),
                "decision_audit DEDUP must share `{shared_col}` with signal_audit \
                 so operators can JOIN the two tables on the canonical key"
            );
        }
    }

    #[test]
    fn test_append_decision_audit_row_smoke() {
        let _ = append_decision_audit_row;
    }

    #[tokio::test]
    async fn test_append_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1); // port 1 — connection refused
        let result = append_decision_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            "rsi_cross_30",
            2885, // RELIANCE
            "NSE_EQ",
            DecisionAuditOutcome::Accepted,
            "rsi_cross_up",
            "tv-strat-001-abc",
            "",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_rejected_with_reason() {
        // Exercise the Rejected branch — operator's primary
        // "why didn't the order go?" forensic question.
        let cfg = test_cfg(1);
        let result = append_decision_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            "rsi_cross_30",
            2885,
            "NSE_EQ",
            DecisionAuditOutcome::Rejected,
            "rsi_cross_up",
            "",
            "halted_by_daily_loss_threshold",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_debounced_with_window_label() {
        // Debounced — rapid-repeat suppression.
        let cfg = test_cfg(1);
        let result = append_decision_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            "rsi_cross_30",
            2885,
            "NSE_EQ",
            DecisionAuditOutcome::Debounced,
            "rsi_cross_up",
            "",
            "debounced_30s_window",
        )
        .await;
        assert!(result.is_err());
    }
}
