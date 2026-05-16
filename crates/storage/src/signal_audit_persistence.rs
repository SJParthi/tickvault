//! Phase 0 Item 26-a — strategy signal audit persistence (sampled).
//!
//! Captures every signal evaluated by the strategy FSM — both the
//! triggered ones (which become potential orders) and the
//! not-triggered ones (which give the operator visibility into "what
//! was the engine thinking?"). Phase 0 strategies are dry-run by
//! default; this audit table is the only forensic chain that survives
//! between sessions.
//!
//! The operator can answer:
//!
//!   * "Did the RSI-30-cross-up signal fire on RELIANCE at 11:23 IST,
//!     and what was the indicator state at that moment?"
//!   * "How many `Triggered` signals fired today vs how many were
//!     `Suppressed` by risk/halt/cooldown?"
//!   * "Reconcile: why did the strategy log the signal but no order
//!     was placed? — query `signal_audit` (Triggered) JOIN
//!     `decision_audit` (Rejected) on `(trading_date_ist, ts,
//!     strategy_id, security_id)`."
//!
//! Lifecycle (single-row per signal evaluation):
//!
//!   * `Triggered` — signal predicate evaluated true; eligible to
//!     become an order (subject to the decision-layer gate in Item
//!     26-b).
//!   * `NotTriggered` — signal predicate evaluated false. Sampled
//!     write to control row volume (default 1-in-100 ratio per
//!     `signal_sample_pct` config).
//!   * `Suppressed` — predicate true but a Phase 0 guard
//!     (market-hours, cooldown, halt) blocked emission. Captures
//!     the reason for triage.
//!
//! SEBI 5-year retention. Mirrors the `pnl_audit` /
//! `sl_replacement_audit` audit-table writer family. The decision-
//! layer counterpart table + the strategy FSM wiring land in
//! Item 26-b.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_SIGNAL_AUDIT: &str = "signal_audit";

// Composite per-day per-strategy per-security per-ts per-outcome —
// the chain has 3 outcomes, all distinct rows. Same strategy may
// evaluate the same security multiple times per second (e.g. on
// every tick); `ts` MUST be in the DEDUP key so rapid-succession
// evaluations both survive. `outcome` lets a Triggered + Suppressed
// pair at the same ts both survive (rare but possible: signal
// fires, but pre-trade guard blocks).
pub const DEDUP_KEY_SIGNAL_AUDIT: &str =
    "trading_date_ist, ts, strategy_id, security_id, exchange_segment, outcome";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format strings for the `outcome` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalAuditOutcome {
    /// Signal predicate evaluated true and the signal is eligible to
    /// become an order. The decision-layer audit (Item 26-b) records
    /// what happened next (accepted / rejected / debounced).
    Triggered,
    /// Signal predicate evaluated false. Sampled write — default
    /// 1-in-100 evaluations land here to keep row volume manageable
    /// while still providing forensic visibility into "what was the
    /// engine thinking at 11:23?".
    NotTriggered,
    /// Predicate true but a Phase 0 guard (market-hours, cooldown,
    /// halt) blocked emission. The `suppression_reason` field
    /// carries the operator-readable cause.
    Suppressed,
}

impl SignalAuditOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Triggered => "triggered",
            Self::NotTriggered => "not_triggered",
            Self::Suppressed => "suppressed",
        }
    }

    /// Returns `true` if the outcome leaves the signal eligible to
    /// flow into the decision-layer audit (Item 26-b). Only
    /// `Triggered` qualifies — `NotTriggered` and `Suppressed` end
    /// the lifecycle here.
    #[must_use]
    pub const fn is_eligible_for_decision_layer(self) -> bool {
        matches!(self, Self::Triggered)
    }
}

/// Creates the audit table if absent. Idempotent.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_signal_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_SIGNAL_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            strategy_id SYMBOL, \
            security_id INT, \
            exchange_segment SYMBOL, \
            outcome SYMBOL, \
            signal_name SYMBOL, \
            indicator_snapshot_json STRING, \
            suppression_reason STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_SIGNAL_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(table = QUESTDB_TABLE_SIGNAL_AUDIT, "audit table ready");
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_SIGNAL_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_SIGNAL_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Appends one signal audit row.
///
/// `ts_nanos_ist` / `trading_date_ist_nanos` are IST wall-clock
/// nanoseconds — divided to microseconds (2026-04-28 regression
/// class). `indicator_snapshot_json` is a serialized snapshot of
/// the relevant indicator state at the moment of evaluation —
/// kept as STRING (JSON) so the schema doesn't need a column per
/// indicator. `suppression_reason` is empty for Triggered +
/// NotTriggered; populated for Suppressed only.
// APPROVED: 9 wire-format columns plus QuestDB config — same audit-row shape as sibling pnl_audit / sl_replacement_audit / self_trade_audit writers.
#[allow(clippy::too_many_arguments)]
pub async fn append_signal_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    strategy_id: &str,
    security_id: u32,
    exchange_segment: &str,
    outcome: SignalAuditOutcome,
    signal_name: &str,
    indicator_snapshot_json: &str,
    suppression_reason: &str,
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
    let snapshot_json_esc = sanitize_audit_string(indicator_snapshot_json);
    let suppression_reason_esc = sanitize_audit_string(suppression_reason);
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let trading_date_ist_micros = trading_date_ist_nanos / 1_000;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_SIGNAL_AUDIT} \
         (ts, trading_date_ist, strategy_id, security_id, exchange_segment, outcome, signal_name, indicator_snapshot_json, suppression_reason) VALUES \
         ({ts_micros_ist}, {trading_date_ist_micros}, '{strategy_id_esc}', {security_id}, '{segment_esc}', '{outcome_str}', '{signal_name_esc}', '{snapshot_json_esc}', '{suppression_reason_esc}');"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status_code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("signal_audit insert non-2xx ({status_code}): {body}");
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
        assert_eq!(QUESTDB_TABLE_SIGNAL_AUDIT, "signal_audit");
    }

    #[test]
    fn test_outcome_as_str_stable() {
        // Wire-format strings — Grafana panels + SEBI audit
        // queries depend on these.
        assert_eq!(SignalAuditOutcome::Triggered.as_str(), "triggered");
        assert_eq!(SignalAuditOutcome::NotTriggered.as_str(), "not_triggered");
        assert_eq!(SignalAuditOutcome::Suppressed.as_str(), "suppressed");
    }

    /// `is_eligible_for_decision_layer()` predicate — only
    /// `Triggered` flows to the Item 26-b decision audit. Pinning
    /// this predicate prevents a future fourth-variant edit (e.g.
    /// adding `Provisional`) from accidentally bypassing the
    /// decision gate.
    #[test]
    fn test_is_eligible_for_decision_layer_only_triggered() {
        assert!(SignalAuditOutcome::Triggered.is_eligible_for_decision_layer());
        assert!(!SignalAuditOutcome::NotTriggered.is_eligible_for_decision_layer());
        assert!(!SignalAuditOutcome::Suppressed.is_eligible_for_decision_layer());
    }

    /// DEDUP composition — must include the designated timestamp
    /// partition column + ts (rapid-succession evaluations) +
    /// strategy_id + (security_id, exchange_segment) per I-P1-11 +
    /// outcome (lifecycle chain).
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
                DEDUP_KEY_SIGNAL_AUDIT.contains(required),
                "DEDUP key must include `{required}`. Got: {DEDUP_KEY_SIGNAL_AUDIT}"
            );
        }
    }

    /// `ts` MUST be in the DEDUP key — same strategy can evaluate
    /// the same security on rapid-succession ticks. Without `ts`,
    /// the second evaluation would silently overwrite the first.
    #[test]
    fn test_dedup_key_includes_ts_for_rapid_succession() {
        assert!(DEDUP_KEY_SIGNAL_AUDIT.contains("ts"));
    }

    /// I-P1-11 composite identity — `security_id` MUST pair with
    /// `exchange_segment`.
    #[test]
    fn test_dedup_key_includes_security_id_and_exchange_segment() {
        assert!(DEDUP_KEY_SIGNAL_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_SIGNAL_AUDIT.contains("exchange_segment"));
    }

    /// `outcome` MUST be in the DEDUP key so Triggered +
    /// Suppressed at the same ts both survive (rare but possible:
    /// signal fires, pre-trade guard blocks, both rows logged
    /// from the same evaluation point).
    #[test]
    fn test_dedup_key_includes_outcome() {
        assert!(DEDUP_KEY_SIGNAL_AUDIT.contains("outcome"));
    }

    /// Regression: 2026-04-28 nanos-to-micros bug class.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("signal_audit_persistence.rs");
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
        let src = include_str!("signal_audit_persistence.rs");
        for column in [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "strategy_id SYMBOL",
            "security_id INT",
            "exchange_segment SYMBOL",
            "outcome SYMBOL",
            "signal_name SYMBOL",
            "indicator_snapshot_json STRING",
            "suppression_reason STRING",
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
            SignalAuditOutcome::Triggered.as_str(),
            SignalAuditOutcome::NotTriggered.as_str(),
            SignalAuditOutcome::Suppressed.as_str(),
        ];
        let mut sorted: Vec<&str> = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len());
    }

    #[test]
    fn test_append_signal_audit_row_smoke() {
        let _ = append_signal_audit_row;
    }

    #[tokio::test]
    async fn test_append_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1); // port 1 — connection refused
        let result = append_signal_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            "rsi_cross_30",
            2885, // RELIANCE
            "NSE_EQ",
            SignalAuditOutcome::Triggered,
            "rsi_cross_up",
            r#"{"rsi":29.5,"ema_fast":2950.0}"#,
            "",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_suppressed_outcome_with_reason() {
        // Exercise the Suppressed branch — captures the operator-
        // readable suppression reason for triage.
        let cfg = test_cfg(1);
        let result = append_signal_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            "rsi_cross_30",
            2885,
            "NSE_EQ",
            SignalAuditOutcome::Suppressed,
            "rsi_cross_up",
            r#"{"rsi":29.5,"ema_fast":2950.0}"#,
            "halted_by_daily_loss_threshold",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_not_triggered_outcome_sampled() {
        // The NotTriggered branch is sampled — keeps row volume
        // manageable while still providing forensic visibility.
        let cfg = test_cfg(1);
        let result = append_signal_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            "rsi_cross_30",
            2885,
            "NSE_EQ",
            SignalAuditOutcome::NotTriggered,
            "rsi_cross_up",
            r#"{"rsi":45.0,"ema_fast":2950.0}"#,
            "",
        )
        .await;
        assert!(result.is_err());
    }
}
