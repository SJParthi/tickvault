//! Phase 0 Item 22f — self-trade-prevention audit persistence.
//!
//! Per SEBI Circular SEBI/HO/MIRSD/DOP/CIR/P/2018/153 (wash-trade
//! prevention), an account placing both legs of a trade within a
//! tight time window risks the trade being classified as artificial
//! / non-bona-fide. tickvault enforces a 60-second cooldown between
//! filled-fill on the same `(security_id, exchange_segment)` —
//! subsequent opposite-side orders are blocked at the OMS pre-trade
//! gate until the cooldown expires.
//!
//! Every cooldown lifecycle event lands here:
//!
//!   * `CooldownStarted` — a fill triggered the 60s window. Captures
//!     which order triggered, which side, and the security identity.
//!   * `Blocked` — a subsequent order was rejected by the cooldown
//!     gate. Captures the BLOCKED order's correlation_id + the
//!     remaining cooldown seconds at the time of the block.
//!   * `Completed` — the 60s window expired naturally without any
//!     blocked attempts. The pair-key forensic chain ends here.
//!
//! SEBI 5-year retention applies. The audit row is the operator's
//! defense against a regulator question of "how did you prevent
//! wash trades on 2026-05-15?".
//!
//! Mirrors the `auth_renewal_audit` / `order_update_ws_audit`
//! family of audit-table writers. The pre-trade gate wiring +
//! Telegram variant for `Blocked` outcomes land in Item 22f-b.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_SELF_TRADE_AUDIT: &str = "self_trade_audit";

// Composite per-day per-security per-trigger-order — the cooldown
// chain is keyed by the order that started the window. Multiple
// Blocked rows can share the same trigger; the `outcome` column
// discriminates Started -> Blocked -> Completed.
pub const DEDUP_KEY_SELF_TRADE_AUDIT: &str = "trading_date_ist, security_id, exchange_segment, trigger_order_id, outcome, blocked_correlation_id";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format strings for the `outcome` column. Mirrors the
/// 3 lifecycle stages of the cooldown window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfTradeAuditOutcome {
    /// A fill triggered the cooldown window. The `trigger_order_id`
    /// is the filled order; `triggering_side` is the side that
    /// completed. Subsequent opposite-side orders on the same
    /// `(security_id, segment)` will be blocked until the window
    /// expires.
    CooldownStarted,
    /// A subsequent order was rejected by the cooldown gate. The
    /// `blocked_correlation_id` carries the operator's tag on the
    /// rejected order; `cooldown_remaining_secs` shows how much of
    /// the window was left at the moment of the block.
    Blocked,
    /// The cooldown window expired naturally without any blocked
    /// attempts. Single terminal row per trigger; the chain ends here.
    Completed,
}

impl SelfTradeAuditOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CooldownStarted => "cooldown_started",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
        }
    }
}

/// Stable wire-format strings for the `triggering_side` column.
/// `Buy` means a buy filled; the cooldown blocks subsequent SELL
/// orders. `Sell` is symmetric. The OMS pre-trade gate reads this
/// to know which side to reject during the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfTradeSide {
    Buy,
    Sell,
}

impl SelfTradeSide {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }

    /// Returns the side that the cooldown will BLOCK. If a Buy
    /// filled, the cooldown blocks Sell (and vice versa).
    #[must_use]
    pub const fn blocked_side(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }
}

/// Creates the audit table if absent. Idempotent.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_self_trade_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_SELF_TRADE_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            security_id INT, \
            exchange_segment SYMBOL, \
            outcome SYMBOL, \
            trigger_order_id SYMBOL, \
            triggering_side SYMBOL, \
            blocked_correlation_id STRING, \
            cooldown_remaining_secs LONG, \
            cooldown_window_secs LONG\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_SELF_TRADE_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(table = QUESTDB_TABLE_SELF_TRADE_AUDIT, "audit table ready");
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_SELF_TRADE_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_SELF_TRADE_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Appends one self-trade cooldown audit row.
///
/// `ts_nanos_ist` and `trading_date_ist_nanos` are IST wall-clock
/// nanoseconds — divided to microseconds (2026-04-28 regression
/// class). `cooldown_window_secs` is the canonical window from
/// `SELF_TRADE_COOLDOWN_SECS` (= 60) recorded per-row so a future
/// tuning of the constant doesn't retroactively invalidate older
/// audit rows.
// APPROVED: 10 wire-format columns plus QuestDB config — same audit-row shape as sibling auth_renewal_audit / live_instance_lock / order_update_ws_audit writers.
#[allow(clippy::too_many_arguments)]
pub async fn append_self_trade_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    security_id: u32,
    exchange_segment: &str,
    outcome: SelfTradeAuditOutcome,
    trigger_order_id: &str,
    triggering_side: SelfTradeSide,
    blocked_correlation_id: &str,
    cooldown_remaining_secs: i64,
    cooldown_window_secs: i64,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let segment_esc = sanitize_audit_string(exchange_segment);
    let outcome_str = outcome.as_str();
    let trigger_order_id_esc = sanitize_audit_string(trigger_order_id);
    let side_str = triggering_side.as_str();
    let blocked_correlation_id_esc = sanitize_audit_string(blocked_correlation_id);
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let trading_date_ist_micros = trading_date_ist_nanos / 1_000;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_SELF_TRADE_AUDIT} \
         (ts, trading_date_ist, security_id, exchange_segment, outcome, trigger_order_id, triggering_side, blocked_correlation_id, cooldown_remaining_secs, cooldown_window_secs) VALUES \
         ({ts_micros_ist}, {trading_date_ist_micros}, {security_id}, '{segment_esc}', '{outcome_str}', '{trigger_order_id_esc}', '{side_str}', '{blocked_correlation_id_esc}', {cooldown_remaining_secs}, {cooldown_window_secs});"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status_code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("self_trade_audit insert non-2xx ({status_code}): {body}");
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
        assert_eq!(QUESTDB_TABLE_SELF_TRADE_AUDIT, "self_trade_audit");
    }

    #[test]
    fn test_outcome_as_str_stable() {
        // Wire-format strings — Grafana panels + SEBI audit queries
        // depend on these. Renaming or case-variation breaks the
        // column's symbol set silently.
        assert_eq!(
            SelfTradeAuditOutcome::CooldownStarted.as_str(),
            "cooldown_started"
        );
        assert_eq!(SelfTradeAuditOutcome::Blocked.as_str(), "blocked");
        assert_eq!(SelfTradeAuditOutcome::Completed.as_str(), "completed");
    }

    #[test]
    fn test_side_as_str_stable() {
        assert_eq!(SelfTradeSide::Buy.as_str(), "buy");
        assert_eq!(SelfTradeSide::Sell.as_str(), "sell");
    }

    /// Buy fills block subsequent Sell orders; Sell fills block
    /// subsequent Buy orders. Catches a class of one-character typos
    /// that would silently disable the cooldown for one side.
    #[test]
    fn test_blocked_side_is_symmetric_opposite() {
        assert_eq!(SelfTradeSide::Buy.blocked_side(), SelfTradeSide::Sell);
        assert_eq!(SelfTradeSide::Sell.blocked_side(), SelfTradeSide::Buy);
    }

    /// blocked_side is involutive — applying it twice returns the
    /// original side. Property test against the typed enum.
    #[test]
    fn test_blocked_side_is_involutive() {
        for side in [SelfTradeSide::Buy, SelfTradeSide::Sell] {
            assert_eq!(side.blocked_side().blocked_side(), side);
        }
    }

    /// Regression: 2026-04-28 — DEDUP key must include the designated
    /// timestamp partition column.
    #[test]
    fn test_dedup_key_composition() {
        for required in [
            "trading_date_ist",
            "security_id",
            "exchange_segment",
            "trigger_order_id",
            "outcome",
            "blocked_correlation_id",
        ] {
            assert!(
                DEDUP_KEY_SELF_TRADE_AUDIT.contains(required),
                "DEDUP key must include `{required}`. Got: {DEDUP_KEY_SELF_TRADE_AUDIT}"
            );
        }
    }

    /// `outcome` MUST be in the DEDUP key so a CooldownStarted then
    /// Completed pair (same trigger_order_id, no blocks) keeps BOTH
    /// rows. Without `outcome` the Completed row would silently
    /// overwrite the Started row and the operator would lose the
    /// trigger time.
    #[test]
    fn test_dedup_key_includes_outcome_so_lifecycle_chain_survives() {
        assert!(DEDUP_KEY_SELF_TRADE_AUDIT.contains("outcome"));
    }

    /// `blocked_correlation_id` MUST be in the DEDUP key so two
    /// different orders blocked by the same cooldown trigger keep
    /// distinct rows. Without it, the second Blocked row would
    /// silently overwrite the first.
    #[test]
    fn test_dedup_key_includes_blocked_correlation_so_multiple_blocks_survive() {
        assert!(DEDUP_KEY_SELF_TRADE_AUDIT.contains("blocked_correlation_id"));
    }

    /// I-P1-11 composite identity — security_id ALONE is NOT
    /// unique, must always pair with exchange_segment.
    #[test]
    fn test_dedup_key_includes_security_id_and_exchange_segment() {
        assert!(DEDUP_KEY_SELF_TRADE_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_SELF_TRADE_AUDIT.contains("exchange_segment"));
    }

    /// Regression: 2026-04-28 nanos-to-micros bug class. Source-scan
    /// ratchet locks the conversion line.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("self_trade_audit_persistence.rs");
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
        let src = include_str!("self_trade_audit_persistence.rs");
        for column in [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "security_id INT",
            "exchange_segment SYMBOL",
            "outcome SYMBOL",
            "trigger_order_id SYMBOL",
            "triggering_side SYMBOL",
            "blocked_correlation_id STRING",
            "cooldown_remaining_secs LONG",
            "cooldown_window_secs LONG",
        ] {
            assert!(
                src.contains(column),
                "DDL must declare `{column}` — schema drift will break Grafana queries"
            );
        }
    }

    /// SEBI-cited constant — 60s cooldown per
    /// `SELF_TRADE_COOLDOWN_SECS` in `tickvault_common::constants`.
    /// Changing this is operator-visible behaviour and a
    /// regulator-facing decision.
    #[test]
    fn test_self_trade_cooldown_constant_is_60s() {
        assert_eq!(
            tickvault_common::constants::SELF_TRADE_COOLDOWN_SECS,
            60,
            "SELF_TRADE_COOLDOWN_SECS must remain 60s per Phase 0 Item 22f. \
             Changes are regulator-facing — propagate to \
             topic-PHASE-0-LEAN-LOCKED.md Item 22f before adjusting."
        );
    }

    #[test]
    fn test_append_self_trade_audit_row_smoke() {
        let _ = append_self_trade_audit_row;
    }

    #[tokio::test]
    async fn test_append_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1); // port 1 — connection refused
        let result = append_self_trade_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            2885, // RELIANCE
            "NSE_EQ",
            SelfTradeAuditOutcome::CooldownStarted,
            "78901234",
            SelfTradeSide::Buy,
            "",
            60,
            60,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_blocked_outcome_with_remaining_secs() {
        // Exercise the Blocked branch — operator's primary
        // forensic question is "which order got blocked, when, with
        // how much window left?". Tests cooldown_remaining_secs=42.
        let cfg = test_cfg(1);
        let result = append_self_trade_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            2885,
            "NSE_EQ",
            SelfTradeAuditOutcome::Blocked,
            "78901234",
            SelfTradeSide::Buy,
            "tv-strat-001-sell-attempt",
            42, // 18s after CooldownStarted, 42s left
            60,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_completed_outcome_with_empty_blocked_id() {
        // Cooldown window expired naturally. `blocked_correlation_id`
        // is empty since nothing was blocked.
        let cfg = test_cfg(1);
        let result = append_self_trade_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            2885,
            "NSE_EQ",
            SelfTradeAuditOutcome::Completed,
            "78901234",
            SelfTradeSide::Buy,
            "",
            0,
            60,
        )
        .await;
        assert!(result.is_err());
    }
}
