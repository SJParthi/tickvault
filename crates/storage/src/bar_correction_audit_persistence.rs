//! Phase 0 Item 28 — bar correction audit persistence.
//!
//! At 09:16:05 IST every trading day, the post-open cross-check
//! (Item 15) fetches Dhan REST `/v2/charts/intraday` for the 09:15
//! minute bar across all 222 SIDs (Phase 0 universe: 4 IDX_I +
//! 218 NSE_EQ) and compares against the live-aggregated 1m candle.
//! Any OHLCV field mismatch triggers a correction write to:
//!
//!   1. `candles_1m_shadow` (writable Wave 6 table — RAM bar cache source)
//!   2. `historical_candles` (mirror for post-market cross-verify parity)
//!   3. `bar_correction_audit` (this table — forensic chain)
//!
//! The same post-open cross-check is invoked again by the 15:31 IST
//! post-market cross-verify pass for ALL 1m bars of the day. A bar
//! already corrected in the 09:16:05 pass MUST be skipped (idempotent
//! re-correction guard) — the writer SELECT-FIRST against this table
//! and writes `outcome = AlreadyCorrected` no-op rows.
//!
//! **DEDUP key:** `(trading_date_ist, security_id, exchange_segment,
//! bar_minute, ts)` per I-P1-11 composite identity + 2026-04-28
//! designated-timestamp regression rule.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_BAR_CORRECTION_AUDIT: &str = "bar_correction_audit";

// QuestDB requires the designated timestamp column to be present in
// DEDUP UPSERT KEYS (regression 2026-04-28). I-P1-11 requires
// `(security_id, exchange_segment)` composite. `trading_date_ist`
// + `bar_minute` give per-day per-minute idempotency.
pub const DEDUP_KEY_BAR_CORRECTION_AUDIT: &str =
    "trading_date_ist, security_id, exchange_segment, bar_minute, ts";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format outcome strings for the `outcome` SYMBOL column.
/// Kept as a typed enum so callers can't fragment the SYMBOL set
/// Grafana queries depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarCorrectionOutcome {
    /// Our `candles_1m_shadow` row matched Dhan REST values bit-for-bit.
    /// No correction written. Green-path daily row per (security_id,
    /// exchange_segment, bar_minute).
    Match,
    /// Mismatch detected; correction written to `candles_1m_shadow` +
    /// `historical_candles`. Original values + Dhan values both
    /// captured in the audit row.
    Corrected,
    /// 15:31 post-market cross-verify pass re-evaluated a bar already
    /// corrected in the 09:16:05 pass. No re-fetch from Dhan; idempotent
    /// no-op row written so the audit chain reflects the second pass.
    AlreadyCorrected,
    /// App booted mid-day (after 09:16:05 IST). One-shot late
    /// cross-check ran; outcome captured here so post-market run
    /// skips the bar.
    BootCatchUp,
    /// Cross-check attempted but Dhan REST returned an error or empty
    /// response. `reason` field carries the typed failure label. The
    /// 15:31 post-market pass will retry.
    Failed,
}

impl BarCorrectionOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Corrected => "corrected",
            Self::AlreadyCorrected => "already_corrected",
            Self::BootCatchUp => "boot_catchup",
            Self::Failed => "failed",
        }
    }
}

/// Creates the audit table if absent. Idempotent — safe to call on
/// every boot.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_bar_correction_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_BAR_CORRECTION_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            bar_minute TIMESTAMP, \
            security_id LONG, \
            exchange_segment SYMBOL, \
            trading_symbol SYMBOL, \
            outcome SYMBOL, \
            reason SYMBOL, \
            field_label SYMBOL, \
            old_open DOUBLE, \
            old_high DOUBLE, \
            old_low DOUBLE, \
            old_close DOUBLE, \
            old_volume LONG, \
            dhan_open DOUBLE, \
            dhan_high DOUBLE, \
            dhan_low DOUBLE, \
            dhan_close DOUBLE, \
            dhan_volume LONG, \
            cross_check_pass SYMBOL\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_BAR_CORRECTION_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_BAR_CORRECTION_AUDIT,
                "audit table ready"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_BAR_CORRECTION_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_BAR_CORRECTION_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Appends one bar-correction audit row.
///
/// All input numeric fields MUST be pre-validated as
/// `is_finite() && > 0.0` (prices) or `>= 0` (volumes) by the caller
/// (per `validate_candle_ohlc` in `candle_fetcher.rs`). String inputs
/// are sanitized in this function via `sanitize_audit_string`.
// APPROVED: 20 wire-format columns plus QuestDB config — bar-correction audit row shape matching plan §28 spec.
#[allow(clippy::too_many_arguments)]
pub async fn append_bar_correction_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    bar_minute_nanos: i64,
    security_id: i64,
    exchange_segment: &str,
    trading_symbol: &str,
    outcome: BarCorrectionOutcome,
    reason: &str,
    field_label: &str,
    old_open: f64,
    old_high: f64,
    old_low: f64,
    old_close: f64,
    old_volume: i64,
    dhan_open: f64,
    dhan_high: f64,
    dhan_low: f64,
    dhan_close: f64,
    dhan_volume: i64,
    cross_check_pass: &str,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let outcome_str = outcome.as_str();
    let exchange_segment_esc = sanitize_audit_string(exchange_segment);
    let trading_symbol_esc = sanitize_audit_string(trading_symbol);
    let reason_esc = sanitize_audit_string(reason);
    let field_label_esc = sanitize_audit_string(field_label);
    let cross_check_pass_esc = sanitize_audit_string(cross_check_pass);
    // QuestDB TIMESTAMP columns store microseconds since epoch — divide
    // nanos to micros before embedding so the value stays in range.
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let trading_date_ist_micros = trading_date_ist_nanos / 1_000;
    let bar_minute_micros = bar_minute_nanos / 1_000;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_BAR_CORRECTION_AUDIT} \
         (ts, trading_date_ist, bar_minute, security_id, exchange_segment, \
          trading_symbol, outcome, reason, field_label, \
          old_open, old_high, old_low, old_close, old_volume, \
          dhan_open, dhan_high, dhan_low, dhan_close, dhan_volume, \
          cross_check_pass) VALUES \
         ({ts_micros_ist}, {trading_date_ist_micros}, {bar_minute_micros}, \
          {security_id}, '{exchange_segment_esc}', '{trading_symbol_esc}', \
          '{outcome_str}', '{reason_esc}', '{field_label_esc}', \
          {old_open}, {old_high}, {old_low}, {old_close}, {old_volume}, \
          {dhan_open}, {dhan_high}, {dhan_low}, {dhan_close}, {dhan_volume}, \
          '{cross_check_pass_esc}');"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        // Truncate body to 200 chars per security-reviewer recommendation
        // (avoid log inflation from misbehaving QuestDB).
        let body_truncated: String = body.chars().take(200).collect();
        anyhow::bail!("bar_correction_audit insert non-2xx ({status}): {body_truncated}");
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
        assert_eq!(QUESTDB_TABLE_BAR_CORRECTION_AUDIT, "bar_correction_audit");
    }

    #[test]
    fn test_outcome_as_str_stable() {
        // Wire-format strings — Grafana panels + audit queries depend on these.
        assert_eq!(BarCorrectionOutcome::Match.as_str(), "match");
        assert_eq!(BarCorrectionOutcome::Corrected.as_str(), "corrected");
        assert_eq!(
            BarCorrectionOutcome::AlreadyCorrected.as_str(),
            "already_corrected"
        );
        assert_eq!(BarCorrectionOutcome::BootCatchUp.as_str(), "boot_catchup");
        assert_eq!(BarCorrectionOutcome::Failed.as_str(), "failed");
    }

    /// Regression: 2026-04-28 — QuestDB requires the designated
    /// timestamp column in every DEDUP UPSERT KEYS clause.
    #[test]
    fn test_dedup_key_includes_designated_timestamp() {
        assert!(
            DEDUP_KEY_BAR_CORRECTION_AUDIT.contains("ts"),
            "bar_correction_audit DEDUP key must include `ts` \
             (designated timestamp); QuestDB rejects DDL otherwise. \
             Got: {DEDUP_KEY_BAR_CORRECTION_AUDIT}"
        );
    }

    /// I-P1-11: `security_id` alone is not unique across segments.
    /// The DEDUP key MUST pair `security_id` with `exchange_segment`.
    #[test]
    fn test_dedup_key_pairs_security_id_with_exchange_segment() {
        assert!(DEDUP_KEY_BAR_CORRECTION_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_BAR_CORRECTION_AUDIT.contains("exchange_segment"));
        assert!(DEDUP_KEY_BAR_CORRECTION_AUDIT.contains("trading_date_ist"));
        assert!(DEDUP_KEY_BAR_CORRECTION_AUDIT.contains("bar_minute"));
    }

    /// Regression: 2026-04-28 nanos-to-micros bug class. Source-scan
    /// ratchet locks the conversion lines.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("bar_correction_audit_persistence.rs");
        assert!(
            src.contains("ts_micros_ist = ts_nanos_ist / 1_000"),
            "INSERT must convert ts nanos to micros before embedding"
        );
        assert!(
            src.contains("trading_date_ist_micros = trading_date_ist_nanos / 1_000"),
            "INSERT must convert trading_date_ist nanos to micros"
        );
        assert!(
            src.contains("bar_minute_micros = bar_minute_nanos / 1_000"),
            "INSERT must convert bar_minute nanos to micros"
        );
    }

    /// DDL column-set ratchet so schema drift fails the test loudly.
    #[test]
    fn test_ddl_contains_expected_columns() {
        let src = include_str!("bar_correction_audit_persistence.rs");
        for column in [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "bar_minute TIMESTAMP",
            "security_id LONG",
            "exchange_segment SYMBOL",
            "trading_symbol SYMBOL",
            "outcome SYMBOL",
            "reason SYMBOL",
            "field_label SYMBOL",
            "old_open DOUBLE",
            "old_high DOUBLE",
            "old_low DOUBLE",
            "old_close DOUBLE",
            "old_volume LONG",
            "dhan_open DOUBLE",
            "dhan_high DOUBLE",
            "dhan_low DOUBLE",
            "dhan_close DOUBLE",
            "dhan_volume LONG",
            "cross_check_pass SYMBOL",
        ] {
            assert!(
                src.contains(column),
                "DDL must declare `{column}` — schema drift breaks queries"
            );
        }
    }

    /// Body truncation guard — security review M3 (audit string injection).
    #[test]
    fn test_error_body_truncated_to_200_chars() {
        let src = include_str!("bar_correction_audit_persistence.rs");
        assert!(
            src.contains("body.chars().take(200)"),
            "Error body from QuestDB must be truncated to 200 chars \
             to prevent log inflation per Item 15+28+29 security review M3"
        );
    }

    #[tokio::test]
    async fn test_append_bar_correction_audit_row_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1);
        let result = append_bar_correction_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            1_709_982_900_000_000_000,
            13, // NIFTY
            "IDX_I",
            "NIFTY",
            BarCorrectionOutcome::Corrected,
            "open price mismatch",
            "open",
            24500.0,
            24550.0,
            24480.0,
            24530.0,
            1_000_000,
            24505.5,
            24550.0,
            24480.0,
            24530.0,
            1_000_000,
            "post_open_09_16_05",
        )
        .await;
        assert!(
            result.is_err(),
            "append must surface connection errors to caller"
        );
    }

    #[tokio::test]
    async fn test_append_bar_correction_audit_row_match_green_path() {
        // Green-path (no correction) audit row — outcome=Match.
        // Old/Dhan values are the same.
        let cfg = test_cfg(1);
        let result = append_bar_correction_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            1_709_982_900_000_000_000,
            13,
            "IDX_I",
            "NIFTY",
            BarCorrectionOutcome::Match,
            "",
            "",
            24500.0,
            24550.0,
            24480.0,
            24530.0,
            1_000_000,
            24500.0,
            24550.0,
            24480.0,
            24530.0,
            1_000_000,
            "post_open_09_16_05",
        )
        .await;
        assert!(result.is_err(), "still err on unreachable QuestDB");
    }
}
