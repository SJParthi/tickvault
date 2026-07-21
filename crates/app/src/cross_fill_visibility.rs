//! Cross-fill visibility — audit-channel consumer + daily Telegram digest
//! (operator directive 2026-07-20: every cross-fill "highlighted, logged,
//! monitored, audited, visualised — every day, week, month — precisely at
//! what time it is happening").
//!
//! Two spawns, both owned by `cadence_boot::spawn_cadence_scheduler` (the
//! cadence scheduler is the SOLE producer of cross-fill events, so the
//! consumer + digest live with its boot):
//!
//! 1. **Consumer** ([`spawn_cross_fill_audit_consumer`]): installs the
//!    process-global core sink
//!    (`tickvault_core::cadence::init_cross_fill_audit_sink`) and drains
//!    the bounded channel into the `cross_fill_audit` QuestDB table via
//!    [`CrossFillAuditWriter`] (the `ws_audit_consumer` pattern —
//!    best-effort, append+flush per event, CADENCE-04 on failure, never on
//!    the cadence decision path).
//! 2. **Daily digest** ([`spawn_cross_fill_digest`]): one plain-language
//!    Telegram at 15:47 IST per trading day
//!    ([`NotificationEvent::CrossFillDailyDigest`]). Gated on the audit
//!    table being QUERYABLE — a read failure sends the honest "count
//!    unknown" body, never a false "0 times" green (audit Rule 11).
//!
//! Weekly/monthly rollups are operator SQL over the same table — see
//! `docs/runbooks/cross-fill-visibility.md`.

use std::sync::Arc;

use chrono::Timelike;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::cadence::{CrossFillAuditEvent, init_cross_fill_audit_sink};
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_storage::cross_fill_audit_persistence::{
    CROSS_FILL_AUDIT_TABLE, CROSS_FILL_RESOLUTION_CROSS_FILL,
    CROSS_FILL_RESOLUTION_NATIVE_LATE_RETRY, CROSS_FILL_STAGE_CROSS_FILL,
    CROSS_FILL_STAGE_GROWW_FALLBACK, CrossFillAuditRow, CrossFillAuditWriter,
};
use tracing::{error, info};

/// Bounded capacity for the cross-fill audit channel. Cross-fills are rare
/// (a handful per day by construction — one per degraded lane per cycle),
/// so a small bound is ample; the producer `try_send`s and drops loudly
/// (CADENCE-04) on the practically-unreachable full case.
const CROSS_FILL_AUDIT_CHANNEL_CAPACITY: usize = 256;

/// Daily digest trigger — 15:47:00 IST (after the 15:30 close and clear of
/// the 15:45 scorecard; before the prod box's 16:30 auto-stop).
pub const CROSS_FILL_DIGEST_TRIGGER_SECS_OF_DAY_IST: u32 = 15 * 3_600 + 47 * 60;

/// Cap on per-event lines carried in the Telegram body (a runaway day
/// degrades to "+N more" — the table holds the full record).
const DIGEST_MAX_LINES: usize = 20;

/// HTTP timeout for the once-per-day digest QuestDB `/exec` read (cold
/// path). Generous vs the 2s probe timeout because the day-query scans
/// the day's `cross_fill_audit` rows (bounded — a handful/day).
const DIGEST_HTTP_TIMEOUT_SECS: u64 = 10;

/// Map a core [`CrossFillAuditEvent`] onto its `cross_fill_audit` row.
/// Pure — the resolution SYMBOL derives from the stage (`cross_fill` →
/// `cross_fill`; `groww_fallback` → `native_late_retry`, the lane's OWN
/// late retry; the T+4s retry session's PR stamps the other values).
#[must_use]
pub fn row_from_event(ev: &CrossFillAuditEvent) -> CrossFillAuditRow {
    let resolution = if ev.stage == CROSS_FILL_STAGE_GROWW_FALLBACK {
        CROSS_FILL_RESOLUTION_NATIVE_LATE_RETRY
    } else {
        CROSS_FILL_RESOLUTION_CROSS_FILL
    };
    CrossFillAuditRow {
        ts_ist_nanos: ev.ts_ist_nanos,
        trading_date_ist_nanos: ev.trading_date_ist_nanos,
        lane: ev.lane,
        source_lane: ev.source_lane,
        stage: ev.stage,
        cycle_minute_ist: i64::from(ev.cycle_minute_ist),
        legs: ev.legs(),
        rows_filled: i64::from(ev.spots.saturating_add(ev.chains)),
        cycle_latency_ms: ev.cycle_latency_ms,
        ladder_rung: i64::from(ev.ladder_rung),
        resolution,
        // 0 today by contract — the T+4s retry session's PR stamps
        // measured attempt counts.
        retry_attempts: 0,
        resolved_at_ms_after_close: ev.resolved_at_ms_after_close,
    }
}

/// Install the core sink + spawn the channel→QuestDB consumer. Idempotent:
/// if a sink is already installed this process (the other boot path won),
/// nothing is spawned.
// TEST-EXEMPT: thin tokio wiring — the row mapping (row_from_event) and the writer (storage crate) are unit-tested; the spawn is pinned by the cadence_boot call site.
pub fn spawn_cross_fill_audit_consumer(questdb_cfg: QuestDbConfig) {
    let (tx, mut rx) =
        tokio::sync::mpsc::channel::<CrossFillAuditEvent>(CROSS_FILL_AUDIT_CHANNEL_CAPACITY);
    if !init_cross_fill_audit_sink(tx) {
        // The other boot path already installed the sink + consumer.
        return;
    }
    tokio::spawn(async move {
        let mut writer = CrossFillAuditWriter::new(&questdb_cfg);
        while let Some(ev) = rx.recv().await {
            let row = row_from_event(&ev);
            if let Err(err) = writer.append_row(&row) {
                metrics::counter!("tv_cross_fill_audit_write_errors_total", "stage" => "append")
                    .increment(1);
                error!(
                    code = ErrorCode::Cadence04AuditWriteFailed.code_str(),
                    stage = "append",
                    lane = row.lane,
                    cycle_minute_ist = row.cycle_minute_ist,
                    ?err,
                    "CADENCE-04: cross_fill_audit append failed"
                );
                continue;
            }
            if let Err(err) = writer.flush() {
                metrics::counter!("tv_cross_fill_audit_write_errors_total", "stage" => "flush")
                    .increment(1);
                error!(
                    code = ErrorCode::Cadence04AuditWriteFailed.code_str(),
                    stage = "flush",
                    lane = row.lane,
                    cycle_minute_ist = row.cycle_minute_ist,
                    ?err,
                    "CADENCE-04: cross_fill_audit flush failed — row discarded \
                     (best-effort forensics; the CADENCE-01 signal still fired)"
                );
            }
        }
        info!("cross_fill_audit consumer: channel closed — exiting");
    });
}

/// Spawn the daily 15:47 IST digest loop (fire-and-forget; sleeps across
/// non-trading days; a boot AFTER the trigger sends tomorrow's digest —
/// the prod box boots 08:30 IST, well before the trigger).
// TEST-EXEMPT: thin tokio scheduling shell over the unit-tested pure helpers (digest SQL builders, parser, formatter, wait math below).
pub fn spawn_cross_fill_digest(
    calendar: Arc<TradingCalendar>,
    questdb_cfg: QuestDbConfig,
    notifier: Arc<NotificationService>,
) {
    tokio::spawn(async move {
        loop {
            let secs = ist_now().num_seconds_from_midnight();
            let wait = secs_until_next_trigger(secs, CROSS_FILL_DIGEST_TRIGGER_SECS_OF_DAY_IST);
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            let date = ist_now().date_naive();
            if !calendar.is_trading_day(date) {
                continue; // weekend/holiday — no cycles ran, nothing to say.
            }
            run_cross_fill_digest_once(&questdb_cfg, &notifier, date).await;
        }
    });
}

/// Seconds to sleep from `now_secs_of_day` until the NEXT trigger instant
/// (today's if still ahead, else tomorrow's). Pure.
#[must_use]
pub fn secs_until_next_trigger(now_secs_of_day: u32, trigger_secs_of_day: u32) -> u64 {
    if now_secs_of_day < trigger_secs_of_day {
        u64::from(trigger_secs_of_day - now_secs_of_day)
    } else {
        u64::from(86_400 - now_secs_of_day + trigger_secs_of_day)
    }
}

/// IST "now" (UTC + the IST offset) — the house chrono idiom.
fn ist_now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
        + chrono::Duration::nanoseconds(tickvault_common::constants::IST_UTC_OFFSET_NANOS)
}

/// The day's cross-fill event query (times + directions + legs), IST
/// day-bounded on the designated `ts` (partition-pruned). Pure.
#[must_use]
pub fn build_cross_fill_day_sql(date_ymd: &str) -> String {
    format!(
        "SELECT cycle_minute_ist, lane, source_lane, legs FROM {CROSS_FILL_AUDIT_TABLE} \
         WHERE ts >= '{date_ymd}T00:00:00.000000Z' \
         AND ts < dateadd('d', 1, '{date_ymd}T00:00:00.000000Z') \
         AND stage = '{CROSS_FILL_STAGE_CROSS_FILL}' ORDER BY ts"
    )
}

/// The day's Groww-fallback launch count. Pure.
#[must_use]
pub fn build_groww_fallback_day_count_sql(date_ymd: &str) -> String {
    format!(
        "SELECT count() FROM {CROSS_FILL_AUDIT_TABLE} \
         WHERE ts >= '{date_ymd}T00:00:00.000000Z' \
         AND ts < dateadd('d', 1, '{date_ymd}T00:00:00.000000Z') \
         AND stage = '{CROSS_FILL_STAGE_GROWW_FALLBACK}'"
    )
}

/// One parsed cross-fill event (the digest's working subset).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossFillDigestEvent {
    pub cycle_minute_ist: u32,
    pub lane: String,
    pub source_lane: String,
    pub legs: String,
}

/// Parse the QuestDB `/exec` dataset for [`build_cross_fill_day_sql`].
/// `None` = the body was not the expected shape (table unreadable /
/// QuestDB error body) — the digest then reports "count unknown".
#[must_use]
pub fn parse_cross_fill_day_dataset(body: &str) -> Option<Vec<CrossFillDigestEvent>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let dataset = v.get("dataset")?.as_array()?;
    let mut out = Vec::with_capacity(dataset.len());
    for row in dataset {
        let row = row.as_array()?;
        out.push(CrossFillDigestEvent {
            cycle_minute_ist: u32::try_from(row.first()?.as_i64()?).ok()?,
            lane: row.get(1)?.as_str()?.to_string(),
            source_lane: row.get(2)?.as_str()?.to_string(),
            legs: row.get(3)?.as_str()?.to_string(),
        });
    }
    Some(out)
}

/// IST seconds-of-day → operator 12-hour clock text ("9:16 AM"). Pure.
#[must_use]
pub fn format_ist_12h(secs_of_day: u32) -> String {
    let h24 = (secs_of_day / 3_600) % 24;
    let m = (secs_of_day / 60) % 60;
    let (h12, half) = match h24 {
        0 => (12, "AM"),
        1..=11 => (h24, "AM"),
        12 => (12, "PM"),
        _ => (h24 - 12, "PM"),
    };
    format!("{h12}:{m:02} {half}")
}

/// Broker lane wire label → operator display name. Unknown labels pass
/// through verbatim (never a panic, never a fabricated name).
fn display_lane(lane: &str) -> &str {
    match lane {
        "dhan" => "Dhan",
        "groww" => "Groww",
        other => other,
    }
}

/// Legs slug → plain words.
fn display_legs(legs: &str) -> &str {
    match legs {
        "spot" => "spot price",
        "chain" => "option chain",
        "spot+chain" => "spot price + option chain",
        other => other,
    }
}

/// Build the plain-English per-event digest lines ("9:16 AM — Dhan spot
/// price filled from Groww"), capped at [`DIGEST_MAX_LINES`]. Pure.
#[must_use]
pub fn build_digest_lines(events: &[CrossFillDigestEvent]) -> String {
    let mut lines: Vec<String> = events
        .iter()
        .take(DIGEST_MAX_LINES)
        .map(|e| {
            format!(
                "{} — {} {} filled from {}",
                format_ist_12h(e.cycle_minute_ist),
                display_lane(&e.lane),
                display_legs(&e.legs),
                display_lane(&e.source_lane),
            )
        })
        .collect();
    if events.len() > DIGEST_MAX_LINES {
        lines.push(format!("+{} more", events.len() - DIGEST_MAX_LINES));
    }
    lines.join("\n")
}

/// One digest run: query the day's rows, build the body, send ONE Telegram.
/// A read failure sends the honest "count unknown" body (never a false
/// "0 times" green) and logs CADENCE-04 stage `digest_read`.
async fn run_cross_fill_digest_once(
    questdb_cfg: &QuestDbConfig,
    notifier: &Arc<NotificationService>,
    date: chrono::NaiveDate,
) {
    let date_ymd = date.format("%Y-%m-%d").to_string();
    let base_url = format!("http://{}:{}/exec", questdb_cfg.host, questdb_cfg.http_port);
    // Panic-free client build (HTTP-CLIENT-01 discipline — NEVER fall back
    // to `Client::new()`, which panics on the same conditions that make the
    // builder fail). A build failure degrades this one digest run to the
    // honest "count unknown" body via the read-failure arm below.
    let client = match tickvault_storage::http_client::build_probe_client(DIGEST_HTTP_TIMEOUT_SECS)
    {
        Ok(c) => c,
        Err(e) => {
            metrics::counter!("tv_cross_fill_audit_write_errors_total", "stage" => "digest_read")
                .increment(1);
            error!(
                code = ErrorCode::Cadence04AuditWriteFailed.code_str(),
                stage = "digest_read",
                error = %e.message(),
                date = %date_ymd,
                "CADENCE-04: digest HTTP client build failed — \
                 sending the honest count-unknown digest (never a false 0)"
            );
            notifier.notify(NotificationEvent::CrossFillDailyDigest {
                trading_date_ist: date_ymd,
                count: -1,
                lines: String::new(),
                fallback_count: -1,
            });
            return;
        }
    };
    let events = match fetch_body(&client, &base_url, &build_cross_fill_day_sql(&date_ymd)).await {
        Some(body) => parse_cross_fill_day_dataset(&body),
        None => None,
    };
    let fallback_count = match fetch_body(
        &client,
        &base_url,
        &build_groww_fallback_day_count_sql(&date_ymd),
    )
    .await
    {
        Some(body) => parse_count(&body),
        None => None,
    };
    match events {
        Some(events) => {
            notifier.notify(NotificationEvent::CrossFillDailyDigest {
                trading_date_ist: date_ymd,
                count: events.len() as i64,
                lines: build_digest_lines(&events),
                fallback_count: fallback_count.unwrap_or(-1),
            });
        }
        None => {
            metrics::counter!("tv_cross_fill_audit_write_errors_total", "stage" => "digest_read")
                .increment(1);
            error!(
                code = ErrorCode::Cadence04AuditWriteFailed.code_str(),
                stage = "digest_read",
                date = %date_ymd,
                "CADENCE-04: cross_fill_audit unreadable at digest time — \
                 sending the honest count-unknown digest (never a false 0)"
            );
            notifier.notify(NotificationEvent::CrossFillDailyDigest {
                trading_date_ist: date_ymd,
                count: -1,
                lines: String::new(),
                fallback_count: -1,
            });
        }
    }
}

/// One bounded QuestDB `/exec` GET; `None` on transport/non-2xx.
async fn fetch_body(client: &reqwest::Client, base_url: &str, sql: &str) -> Option<String> {
    let resp = client
        .get(base_url)
        .query(&[("query", sql)])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.text().await.ok()
}

/// Parse a single `count()` value out of a QuestDB `/exec` body.
#[must_use]
pub fn parse_count(body: &str) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("dataset")?
        .as_array()?
        .first()?
        .as_array()?
        .first()?
        .as_i64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secs_until_next_trigger_before_and_after() {
        // 09:00 → 15:47 same day.
        assert_eq!(
            secs_until_next_trigger(9 * 3_600, CROSS_FILL_DIGEST_TRIGGER_SECS_OF_DAY_IST),
            u64::from(CROSS_FILL_DIGEST_TRIGGER_SECS_OF_DAY_IST - 9 * 3_600)
        );
        // Exactly at the trigger → tomorrow's trigger (full day).
        assert_eq!(
            secs_until_next_trigger(
                CROSS_FILL_DIGEST_TRIGGER_SECS_OF_DAY_IST,
                CROSS_FILL_DIGEST_TRIGGER_SECS_OF_DAY_IST
            ),
            86_400
        );
        // 16:00 → tomorrow 15:47.
        assert_eq!(
            secs_until_next_trigger(16 * 3_600, CROSS_FILL_DIGEST_TRIGGER_SECS_OF_DAY_IST),
            u64::from(86_400 - 16 * 3_600 + CROSS_FILL_DIGEST_TRIGGER_SECS_OF_DAY_IST)
        );
    }

    #[test]
    fn test_format_ist_12h() {
        assert_eq!(format_ist_12h(33_360), "9:16 AM"); // the live fixture minute
        assert_eq!(format_ist_12h(0), "12:00 AM");
        assert_eq!(format_ist_12h(12 * 3_600), "12:00 PM");
        assert_eq!(format_ist_12h(15 * 3_600 + 29 * 60), "3:29 PM");
    }

    #[test]
    fn test_build_cross_fill_day_sql_and_build_groww_fallback_day_count_sql() {
        let sql = build_cross_fill_day_sql("2026-07-20");
        assert!(sql.contains("FROM cross_fill_audit"));
        assert!(sql.contains("stage = 'cross_fill'"));
        assert!(sql.contains("'2026-07-20T00:00:00.000000Z'"));
        assert!(sql.contains("ORDER BY ts"));
        let sql2 = build_groww_fallback_day_count_sql("2026-07-20");
        assert!(sql2.contains("count()"));
        assert!(sql2.contains("stage = 'groww_fallback'"));
    }

    #[test]
    fn test_parse_cross_fill_day_dataset_and_parse_count() {
        let body = r#"{"columns":[{"name":"cycle_minute_ist"},{"name":"lane"},{"name":"source_lane"},{"name":"legs"}],"dataset":[[33360,"dhan","groww","spot"],[33420,"groww","dhan","chain"]]}"#;
        let events = parse_cross_fill_day_dataset(body).expect("parses");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].cycle_minute_ist, 33_360);
        assert_eq!(events[0].lane, "dhan");
        assert_eq!(events[1].legs, "chain");
        // Malformed/error bodies → None (digest degrades honestly).
        assert!(parse_cross_fill_day_dataset(r#"{"error":"table does not exist"}"#).is_none());
        assert!(parse_cross_fill_day_dataset("not json").is_none());
        assert_eq!(parse_count(r#"{"dataset":[[3]]}"#), Some(3));
        assert_eq!(parse_count(r#"{"dataset":[]}"#), None);
    }

    #[test]
    fn test_build_digest_lines_plain_english() {
        let events = vec![
            CrossFillDigestEvent {
                cycle_minute_ist: 33_360,
                lane: "dhan".into(),
                source_lane: "groww".into(),
                legs: "spot".into(),
            },
            CrossFillDigestEvent {
                cycle_minute_ist: 34_200,
                lane: "groww".into(),
                source_lane: "dhan".into(),
                legs: "spot+chain".into(),
            },
        ];
        let lines = build_digest_lines(&events);
        assert_eq!(
            lines,
            "9:16 AM — Dhan spot price filled from Groww\n\
             9:30 AM — Groww spot price + option chain filled from Dhan"
        );
        // No jargon, no file paths (operator commandments).
        assert!(!lines.contains(".rs"));
        assert!(!lines.contains("cadence"));
    }

    #[test]
    fn test_build_digest_lines_capped_with_more_marker() {
        let events: Vec<CrossFillDigestEvent> = (0..25)
            .map(|i| CrossFillDigestEvent {
                cycle_minute_ist: 33_360 + i * 60,
                lane: "dhan".into(),
                source_lane: "groww".into(),
                legs: "spot".into(),
            })
            .collect();
        let lines = build_digest_lines(&events);
        assert!(lines.ends_with("+5 more"));
        assert_eq!(lines.lines().count(), 21);
    }

    #[test]
    fn test_row_from_event_maps_resolution_by_stage() {
        let base = tickvault_core::cadence::CrossFillAuditEvent {
            ts_ist_nanos: 1,
            trading_date_ist_nanos: 0,
            lane: "dhan",
            source_lane: "groww",
            stage: "cross_fill",
            cycle_minute_ist: 33_360,
            spots: 1,
            chains: 1,
            cycle_latency_ms: 9_000,
            ladder_rung: 1,
            resolved_at_ms_after_close: 9_000,
            resolution: "cross_fill",
            retry_attempts: 0,
        };
        let row = row_from_event(&base);
        assert_eq!(row.resolution, "cross_fill");
        assert_eq!(row.legs, "spot+chain");
        assert_eq!(row.rows_filled, 2);
        assert_eq!(row.retry_attempts, 0);

        let fb = tickvault_core::cadence::CrossFillAuditEvent {
            stage: "groww_fallback",
            lane: "groww",
            source_lane: "groww",
            spots: 0,
            chains: 2,
            resolved_at_ms_after_close: -1,
            resolution: "cross_fill",
            retry_attempts: 0,
            ..base
        };
        let row = row_from_event(&fb);
        assert_eq!(row.resolution, "native_late_retry");
        assert_eq!(row.legs, "chain");
        assert_eq!(row.resolved_at_ms_after_close, -1);
    }
}
