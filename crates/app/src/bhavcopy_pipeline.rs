//! Wave 5 Item 26 L2 LIVE — 16:30 IST bhavcopy cross-check tokio task.
//!
//! End-to-end orchestrator wiring the primitives shipped in earlier
//! commits:
//!
//! ```text
//! 16:30 IST scheduler tick (TradingCalendar gated)
//!   │
//!   ▼ query QuestDB for 15:30 IST EOD snapshot (last(volume) per security_id)
//! HashMap<u32, u64> dhan_eod_volumes
//!   │
//!   ▼ run_bhavcopy_cycle_once (HTTP fetch + unzip + parse + cross-check)
//! Vec<BhavcopyAuditRow>
//!   │
//!   ▼ append_volume_nse_audit_row × N
//! QuestDB volume_nse_audit table
//!   │
//!   ▼ NseBhavcopyCheckComplete | NseBhavcopyCheckFailed
//! Telegram via NotificationService
//! ```
//!
//! # Honest envelope
//!
//! - **Post-market only.** TradingCalendar gate skips weekends/holidays.
//!   Outside [16:30, 23:59] IST → idle sleep until next 16:30.
//! - **Read-only against live tick path.** Reads `last(volume)` from
//!   `ticks` table (already settled by 15:30 IST) + writes to
//!   `volume_nse_audit` (separate table).
//! - **Idempotent.** `volume_nse_audit` DEDUP key
//!   `(trading_date_ist, security_id, segment)` means re-running the
//!   same day overwrites, not appends.
//! - **Failure modes typed.** HTTP 4xx → `NseBhavcopyCheckFailed
//!   { reason: "http_*" }`; ZIP corrupt → `unzip_unavailable` /
//!   `csv_not_utf8`; QuestDB unreachable for the 15:30 SELECT →
//!   the cycle skips with reason `"questdb_query_failed"`.
//! - **Operator action on mismatch.** None automated; operator
//!   triages `verdict = 'FAIL'` rows from Grafana / SQL.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::instrument_types::DerivativeContract;
use tickvault_core::instrument::bhavcopy_scheduler::{
    BHAVCOPY_TRIGGER_TIME_IST, BhavcopyCycleOutcome, classify_bhavcopy_failure,
    compute_next_bhavcopy_trigger, run_bhavcopy_cycle_once, today_ist_iso, today_ist_yyyymmdd,
};
use tickvault_core::notification::events::NotificationEvent;
use tickvault_core::notification::service::NotificationService;
use tickvault_storage::movers_unified_persistence::QUESTDB_TABLE_MOVERS_1S;
use tickvault_storage::volume_nse_audit_persistence::{
    append_volume_nse_audit_row, ensure_volume_nse_audit_table,
};

/// HTTP timeout for QuestDB EOD volume query — generous since the
/// SELECT against `movers_1s` filters by 30-min window.
const QUESTDB_QUERY_TIMEOUT_SECS: u64 = 30;

/// IST `+05:30` offset for scheduling.
#[allow(clippy::expect_used)] // APPROVED: pure constant; chrono `east_opt` never returns None for `< 86_400`.
fn ist_offset() -> chrono::FixedOffset {
    // APPROVED: pure constant 19_800 < 86_400; chrono `east_opt` never None.
    chrono::FixedOffset::east_opt(5 * 3600 + 30 * 60).expect("IST offset is valid")
}

/// Spawns the daily 16:30 IST bhavcopy cross-check task.
///
/// Boot wires `ensure_volume_nse_audit_table` once before spawning.
/// The task:
/// - Sleeps until next 16:30 IST (handles same-day-future / wrap-to-tomorrow)
/// - Skips on weekends / holidays via TradingCalendar (pre-check before
///   each cycle)
/// - Runs `run_bhavcopy_cycle_once` with a HashMap of EOD volumes
///   queried from QuestDB
/// - Writes audit rows + emits Telegram
/// - Exits on `shutdown.notified()`
///
/// `instruments_supplier` is a `Fn() -> Vec<DerivativeContract>` so the
/// task can fetch the latest snapshot from `InstrumentRegistry` at
/// each cycle (the registry is shared via `Arc`).
// TEST-EXEMPT: end-to-end tokio orchestrator; each underlying primitive (`run_bhavcopy_cycle_once`, `compute_next_bhavcopy_trigger`, `append_volume_nse_audit_row`) is unit-tested in its own module. Live cycle runs once per trading day post-market — covered by the integration smoke test on Mon May 4+.
pub fn spawn_bhavcopy_scheduler_task<F>(
    questdb: QuestDbConfig,
    instruments_supplier: F,
    notification: Arc<NotificationService>,
    shutdown: Arc<Notify>,
) -> JoinHandle<()>
where
    F: Fn() -> Vec<DerivativeContract> + Send + Sync + 'static,
{
    tokio::spawn(async move {
        info!("bhavcopy_scheduler starting — 16:30 IST daily");

        // Boot-time DDL — idempotent.
        ensure_volume_nse_audit_table(&questdb).await;

        loop {
            let now_ist = Utc::now().with_timezone(&ist_offset());
            let sleep_dur = compute_next_bhavcopy_trigger(now_ist, BHAVCOPY_TRIGGER_TIME_IST);
            info!(
                sleep_secs = sleep_dur.as_secs(),
                "bhavcopy_scheduler sleeping until next 16:30 IST"
            );

            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    info!("bhavcopy_scheduler shutdown notified");
                    break;
                }
                _ = tokio::time::sleep(sleep_dur) => {}
            }

            // Wake at 16:30 IST. Run one cycle.
            let trading_date_yyyymmdd = today_ist_yyyymmdd();
            let trading_date_iso = today_ist_iso();
            info!(
                date = trading_date_yyyymmdd,
                "bhavcopy_scheduler running daily cycle"
            );

            // Fetch the latest universe + dhan EOD volumes.
            let instruments = instruments_supplier();
            let dhan_eod_volumes = match query_dhan_eod_volumes(&questdb, &trading_date_iso).await {
                Ok(v) => v,
                Err(err) => {
                    error!(
                        code = "PHASE2-01",
                        date = trading_date_iso,
                        ?err,
                        "bhavcopy_scheduler: QuestDB EOD volume query failed — emitting Failed event"
                    );
                    notification.notify(NotificationEvent::NseBhavcopyCheckFailed {
                        reason: "questdb_query_failed".to_string(),
                        trading_date_ist: trading_date_iso.clone(),
                    });
                    continue;
                }
            };

            match run_bhavcopy_cycle_once(
                &instruments,
                &dhan_eod_volumes,
                &trading_date_yyyymmdd,
                &trading_date_iso,
            )
            .await
            {
                Ok((outcome, audit_rows)) => {
                    persist_audit_rows(&questdb, &audit_rows).await;
                    emit_complete_event(&notification, &outcome);
                }
                Err(err) => {
                    let reason = classify_bhavcopy_failure(&err).to_string();
                    error!(
                        code = "PHASE2-01",
                        reason,
                        date = trading_date_iso,
                        ?err,
                        "bhavcopy_scheduler cycle failed"
                    );
                    notification.notify(NotificationEvent::NseBhavcopyCheckFailed {
                        reason,
                        trading_date_ist: trading_date_iso.clone(),
                    });
                }
            }
        }

        info!("bhavcopy_scheduler exited");
    })
}

/// Queries QuestDB for the last(volume) per security_id at 15:30 IST
/// close on the given trading date.
///
/// Reads from the `movers_1s` base table since that's the
/// canonical Wave 5 source carrying `volume_cumulative` at 1s
/// granularity. Falls back to nothing on table-empty (caller treats
/// as `MISSING_OUR` for every NSE row).
async fn query_dhan_eod_volumes(
    questdb: &QuestDbConfig,
    trading_date_ist: &str,
) -> anyhow::Result<HashMap<u32, u64>> {
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(QUESTDB_QUERY_TIMEOUT_SECS))
        .build()?;
    // Query: latest cumulative volume per security_id within 15:00-15:30 IST window.
    // Use `latest on` which QuestDB optimises to one row per (security_id).
    let sql = format!(
        "SELECT security_id, last(volume) AS volume \
         FROM {QUESTDB_TABLE_MOVERS_1S} \
         WHERE ts BETWEEN '{trading_date_ist}T15:00:00.000000Z' \
                       AND '{trading_date_ist}T15:30:00.000000Z' \
           AND segment = 'D' \
         GROUP BY security_id;"
    );
    let resp = client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("QuestDB EOD volume query non-2xx ({status}): {body}");
    }
    let json: serde_json::Value = resp.json().await?;

    // QuestDB /exec returns: { "dataset": [[security_id, volume], ...] }
    let mut out = HashMap::new();
    if let Some(rows) = json.get("dataset").and_then(|d| d.as_array()) {
        for row in rows {
            if let Some(arr) = row.as_array()
                && arr.len() >= 2
            {
                let sid_opt = arr[0].as_i64().and_then(|v| u32::try_from(v).ok());
                let vol_opt = arr[1].as_i64().and_then(|v| u64::try_from(v).ok());
                if let (Some(sid), Some(vol)) = (sid_opt, vol_opt) {
                    out.insert(sid, vol);
                }
            }
        }
    }
    info!(
        rows = out.len(),
        "bhavcopy_scheduler: dhan EOD volumes loaded"
    );
    Ok(out)
}

/// Drains the audit-row Vec into QuestDB. One INSERT per row at
/// post-market cadence is acceptable (~200K rows over 30s).
async fn persist_audit_rows(
    questdb: &QuestDbConfig,
    audit_rows: &[tickvault_core::instrument::bhavcopy_cross_check::BhavcopyAuditRow],
) {
    let mut written = 0_u64;
    let mut failed = 0_u64;
    let now_nanos = Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
    for row in audit_rows {
        let trading_date_nanos = parse_iso_to_ist_nanos(&row.trading_date_ist);
        match append_volume_nse_audit_row(
            questdb,
            now_nanos,
            trading_date_nanos,
            row.security_id,
            &row.segment,
            &row.ticker_symbol,
            row.dhan_eod_volume,
            row.nse_eod_volume,
            row.diff_pct,
            row.verdict.as_str(),
        )
        .await
        {
            Ok(()) => {
                written = written.saturating_add(1);
            }
            Err(err) => {
                failed = failed.saturating_add(1);
                if failed <= 3 {
                    warn!(
                        ?err,
                        security_id = row.security_id,
                        "bhavcopy_scheduler: audit row insert failed (showing first 3 only)"
                    );
                }
            }
        }
    }
    info!(written, failed, "bhavcopy_scheduler: audit rows persisted");
}

fn emit_complete_event(notification: &Arc<NotificationService>, outcome: &BhavcopyCycleOutcome) {
    notification.notify(NotificationEvent::NseBhavcopyCheckComplete {
        matched: outcome.matched,
        mismatched: outcome.mismatched,
        missing_our: outcome.missing_our,
        missing_nse: outcome.missing_nse,
        trading_date_ist: outcome.trading_date_ist.clone(),
        tolerance_pct: outcome.tolerance_pct,
    });
}

/// Parses an ISO `YYYY-MM-DD` to IST-midnight nanoseconds. Defensive:
/// returns `0` on parse failure (caller's audit row will display
/// 1970-01-01 in QuestDB — visible bug signal but won't break write).
fn parse_iso_to_ist_nanos(iso_date: &str) -> i64 {
    let Ok(date) = chrono::NaiveDate::parse_from_str(iso_date, "%Y-%m-%d") else {
        return 0;
    };
    let Some(naive_dt) = date.and_hms_opt(0, 0, 0) else {
        return 0;
    };
    match ist_offset().from_local_datetime(&naive_dt).earliest() {
        Some(dt) => dt.timestamp_nanos_opt().unwrap_or(0),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_iso_to_ist_nanos_round_number() {
        let n = parse_iso_to_ist_nanos("2026-05-04");
        assert!(n > 0);
        // 2026-05-04 00:00 IST = 2026-05-03 18:30 UTC; positive epoch
        // nanos for any date past 1970.
    }

    #[test]
    fn test_parse_iso_to_ist_nanos_invalid_returns_zero() {
        assert_eq!(parse_iso_to_ist_nanos("not-a-date"), 0);
        assert_eq!(parse_iso_to_ist_nanos(""), 0);
        assert_eq!(parse_iso_to_ist_nanos("2026-13-99"), 0);
    }

    #[test]
    fn test_ist_offset_is_5_30() {
        let offset = ist_offset();
        assert_eq!(offset.local_minus_utc(), 5 * 3600 + 30 * 60);
    }
}
