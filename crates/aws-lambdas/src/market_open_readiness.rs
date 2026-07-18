//! 08:45 IST market-open readiness pager — daily-universe §19 (Rust port
//! of `deploy/aws/lambda/market-open-readiness/handler.py`, phase 2b-2
//! wave 1).
//!
//! STATE-check, not edge-check: asks "is the box running AND has the app
//! reported boot-complete in the last 10 minutes?" and publishes DIRECTLY
//! to SNS on a bad answer. Cannot be defeated by weekend-latched ALARM
//! states (2026-07-06 incident: 94-min-late launch, zero pages).
//! Fail-toward-paging: an unverifiable morning pages a distinct warning.
//!
//! Separation of duties: this Lambda PAGES; the start-watchdog (same
//! 08:45 cron) HEALS (it holds ec2:StartInstances; this one deliberately
//! does not).
//!
//! Honest residual: the SNS publish path RAISES on failure so a broken
//! page feeds AWS/Lambda Errors -> the readiness-errors alarm ->
//! tv_alerts. A TOTAL SNS outage silences both legs — outside the
//! envelope, stated here.
//!
//! Environment variables (set by Terraform — unchanged from the Python):
//!   EC2_INSTANCE_ID       — the tv-app instance to check
//!   ALERTS_TOPIC_ARN      — operator's tv_alerts SNS topic for Telegram
//!   METRIC_NAMESPACE      — Tickvault/Prod
//!   BOOT_METRIC_NAME      — tv_boot_completed
//!   METRIC_HOST           — tickvault-prod (the scrape host dimension)
//!   BOOT_LOOKBACK_MINUTES — boot-metric freshness window (default 10)
//!   LOG_LEVEL             — INFO (default) / DEBUG / WARNING
//!
//! Parity ledger (every deliberate deviation from the Python original):
//! - python `logger.*` → `tracing::*` (same CloudWatch Logs destination).
//! - env vars are read per invocation instead of at module import — same
//!   effective behavior inside a Lambda container.
//! - the python handler tests monkeypatched `boto3.client`; the Rust seam
//!   injects the two probes + the publish transport into `run` instead
//!   (same observable outcomes, no process-global mutation).
//! - a non-integer BOOT_LOOKBACK_MINUTES raised ValueError at python
//!   import (module load failure); Rust falls back to the default 10 —
//!   fail-toward-working, documented here.

use chrono::{DateTime, Datelike, FixedOffset, Timelike, Utc};
use lambda_runtime::Error;
use serde_json::{Value, json};
use tracing::{error, info};

use crate::time::IST_OFFSET_SECS;

/// Launch today >= 08:25 IST followed by stop is TREATED as a deliberate
/// self-stop and silenced. Honest inventory of the stoppers that can hit
/// the 08:25-08:45 window (round-1 review fix 2026-07-06 — an earlier
/// version claimed the holiday gate was the ONLY one, which the repo's
/// own terraform disproves):
///   1. the NSE-holiday gate (tickvault-holiday-gate.service) — the
///      common case;
///   2. the hourly hard-stop-guard's in-window budget breach_stop (its
///      hourly cron's 03:00 UTC tick IS 08:30 IST; budget-guards.tf) and
///      the SNS-driven budget-killswitch — BOTH page in the SAME
///      invocation, AFTER stopping (stop-first is their deliberate
///      spend-cap ordering), so the operator is normally paged by the
///      stopper itself. Honest residual: if a stopper's post-stop publish
///      fails, that page AND this heuristic are both silent for that
///      window — the boot-heartbeat backstop below covers it;
///   3. a manual portal stop — operator-initiated by definition.
/// EC2 never self-stops on app FAILURE, and the boot-heartbeat gate
/// window (08:50-09:10) is the backstop pager for anything this
/// heuristic misreads. Python parity: `HOLIDAY_SELF_STOP_EARLIEST_IST_MINUTES`.
pub const HOLIDAY_SELF_STOP_EARLIEST_IST_MINUTES: u32 = 8 * 60 + 25;

pub const DEFAULT_BOOT_LOOKBACK_MINUTES: i64 = 10;

// Verdicts (pure-string constants so tests assert on them)
pub const READY_SILENT: &str = "ready-silent";
pub const HOLIDAY_SILENT: &str = "holiday-silent";
pub const NOT_RUNNING_PAGE: &str = "not-running-page";
pub const NOT_BOOTED_PAGE: &str = "not-booted-page";
pub const VERIFY_FAILED_PAGE: &str = "verify-failed-page";
pub const DRILL_PAGE: &str = "drill-page";

fn ist() -> FixedOffset {
    // IST is a fixed +05:30 offset — the constant is in-range by
    // construction, so the fallback arm is unreachable.
    FixedOffset::east_opt(IST_OFFSET_SECS)
        .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap_or(Utc.fix()))
}

/// Minutes since IST midnight for a UTC datetime. Python parity:
/// `_ist_minutes_of_day`.
pub fn ist_minutes_of_day(dt_utc: DateTime<Utc>) -> u32 {
    let ist_dt = dt_utc.with_timezone(&ist());
    ist_dt.hour() * 60 + ist_dt.minute()
}

/// True iff `dt_utc` falls on the same IST calendar date as `now_utc`.
/// Python parity: `_is_today_ist`.
pub fn is_today_ist(dt_utc: DateTime<Utc>, now_utc: DateTime<Utc>) -> bool {
    let tz = ist();
    let a = dt_utc.with_timezone(&tz);
    let b = now_utc.with_timezone(&tz);
    (a.year(), a.month(), a.day()) == (b.year(), b.month(), b.day())
}

/// Pure decision core — unit-tested with zero AWS. Python parity:
/// `classify_readiness`.
///
/// Fail TOWARD paging: cannot-verify != OK. A stopped/stopping box whose
/// LaunchTime is TODAY at/after 08:25 IST is the holiday gate's
/// deliberate self-stop (silent); everything else not-running pages.
pub fn classify_readiness(
    state: &str,
    launch_time_utc: Option<DateTime<Utc>>,
    boot_seen: bool,
    probe_error: bool,
    now_utc: DateTime<Utc>,
) -> &'static str {
    if probe_error {
        return VERIFY_FAILED_PAGE; // fail TOWARD paging: cannot-verify != OK
    }
    if state == "running" {
        return if boot_seen {
            READY_SILENT
        } else {
            NOT_BOOTED_PAGE
        };
    }
    if let Some(launch) = launch_time_utc {
        if (state == "stopping" || state == "stopped")
            && is_today_ist(launch, now_utc)
            && ist_minutes_of_day(launch) >= HOLIDAY_SELF_STOP_EARLIEST_IST_MINUTES
        {
            return HOLIDAY_SILENT; // 08:30 start then self-stop = holiday gate
        }
    }
    NOT_RUNNING_PAGE // stopped-old / pending / shutting-down / unknown / not-found
}

/// 10 Telegram commandments: plain English, IST 12h, numbered action
/// verbs, no file paths / library names. Severity emoji leads the MESSAGE
/// body; the SNS Subject is ASCII-only (round-2 review fix 2026-07-06):
/// the SNS Publish API requires Subject to be ASCII text beginning with a
/// letter, number, or punctuation mark — a non-ASCII (emoji-first)
/// Subject is rejected with InvalidParameter, which would degrade every
/// readiness page (incl. the drill) to the generic Lambda-Errors watchman
/// page. House precedent: hard-stop-guard / budget-digest / killswitch
/// all use ASCII subjects with emoji only in the body. Python parity:
/// `SUBJECTS_AND_MESSAGES` (dict → const array + lookup fn).
pub const SUBJECTS_AND_MESSAGES: [(&str, &str, &str); 4] = [
    (
        NOT_RUNNING_PAGE,
        "SOS 8:45 AM: trading computer is NOT running",
        "🆘 At 8:45 AM the trading computer is not running. Market opens 9:15 AM.\n\
         What you need to do RIGHT NOW:\n\
         1. The 8:45 AM watchdog may be starting it - watch for its 'started' message by 8:55 AM.\n\
         2. No message by 8:55 AM: press 'Start instance' on the portal yourself.\n\
         3. After it starts, watch for the 'tickvault started' message within 5 minutes.",
    ),
    (
        NOT_BOOTED_PAGE,
        "SOS 8:45 AM: app NOT ready - market opens 9:15 AM",
        "🆘 The trading computer is ON but the app has NOT reported a finished boot in the last 10 minutes. Market opens 9:15 AM.\n\
         What you need to do RIGHT NOW:\n\
         1. Open the portal and check the app status.\n\
         2. No 'tickvault started' message by 8:55 AM: restart the app from the portal.\n\
         3. Still stuck at 9:05 AM: treat today as a no-trade morning until it is fixed.",
    ),
    (
        VERIFY_FAILED_PAGE,
        "WARNING 8:45 AM: readiness check could not verify the app",
        "⚠️ I could not confirm whether the trading app is ready for the 9:15 AM open (an AWS lookup failed). \
         Treat this as NOT READY until proven otherwise: open the portal now - if the app shows healthy, ignore this.",
    ),
    (
        DRILL_PAGE,
        "[DRILL] readiness pager test - this is only a test",
        "🧪 Manual test of the 8:45 AM readiness pager. Nothing is wrong - no action needed. \
         Seeing this message proves the whole page route (checker to alert system to Telegram) works end-to-end.",
    ),
];

/// Dict-lookup analog: `SUBJECTS_AND_MESSAGES.get(verdict)`.
pub fn subject_and_message(verdict: &str) -> Option<(&'static str, &'static str)> {
    SUBJECTS_AND_MESSAGES
        .iter()
        .find(|(v, _, _)| *v == verdict)
        .map(|(_, s, m)| (*s, *m))
}

/// Injectable readiness core — fold of the python `lambda_handler` with
/// the two probes + the publish transport injected (the Rust analog of
/// the python tests' `monkeypatch.setattr(boto3, "client", ...)`).
///
/// `instance_state` returns (state name, LaunchTime, probe_error);
/// `boot_metric_seen` returns (boot metric >= 1 in lookback, probe_error)
/// and is consulted ONLY for a running box with a clean EC2 probe;
/// `publish` sends one page and its error PROPAGATES (the pager must
/// never die silently — python re-raise semantics).
pub async fn run<SF, SFut, BF, BFut, PF, PFut>(
    event: &Value,
    now_utc: DateTime<Utc>,
    instance_state: SF,
    boot_metric_seen: BF,
    mut publish: PF,
) -> Result<Value, String>
where
    SF: FnOnce() -> SFut,
    SFut: Future<Output = (String, Option<DateTime<Utc>>, bool)>,
    BF: FnOnce() -> BFut,
    BFut: Future<Output = (bool, bool)>,
    PF: FnMut(&'static str, &'static str) -> PFut,
    PFut: Future<Output = Result<(), String>>,
{
    if event.get("mode").and_then(Value::as_str) == Some("drill") {
        // Round-1 review fix (2026-07-06): an "evening stopped-box invoke"
        // CANNOT prove the SNS leg — on any normal trading evening the box
        // was auto-started at 08:30 IST, so LaunchTime is TODAY >= 08:25
        // IST and a stopped box classifies HOLIDAY_SILENT (no publish).
        // This branch force-publishes a clearly-labelled test page through
        // the REAL publish path, so the operator can verify checker → SNS
        // → Telegram end-to-end at any time of day without touching EC2
        // state.
        if let Some((subject, message)) = subject_and_message(DRILL_PAGE) {
            publish(subject, message).await?;
        }
        info!(verdict = DRILL_PAGE, "readiness verdict (manual SNS drill)");
        return Ok(json!({"verdict": DRILL_PAGE}));
    }
    let (state, launch_time, ec2_err) = instance_state().await;
    let (boot_seen, cw_err) = if state == "running" && !ec2_err {
        boot_metric_seen().await
    } else {
        (false, false)
    };
    let verdict = classify_readiness(&state, launch_time, boot_seen, ec2_err || cw_err, now_utc);
    info!(
        verdict,
        state, launch = ?launch_time, boot_seen, ec2_err, cw_err, "readiness verdict"
    );
    if let Some((subject, message)) = subject_and_message(verdict) {
        publish(subject, message).await?;
    }
    Ok(json!({"verdict": verdict, "state": state, "boot_seen": boot_seen}))
}

/// Return (state name, LaunchTime, probe_error) for the instance.
///
/// Any error (incl. not-found) → ("unknown", None, true) — the caller's
/// `classify_readiness` turns a probe error into the verify-failed page.
/// Python parity: `_instance_state`. UNPROVEN until deploy — the live
/// ec2:DescribeInstances leg runs only in a real Lambda.
async fn instance_state(
    ec2: &aws_sdk_ec2::Client,
    instance_id: &str,
) -> (String, Option<DateTime<Utc>>, bool) {
    let resp = match ec2
        .describe_instances()
        .instance_ids(instance_id)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            error!(error = %e, "describe_instances failed");
            return ("unknown".to_string(), None, true);
        }
    };
    let Some(inst) = resp
        .reservations()
        .first()
        .and_then(|r| r.instances().first())
    else {
        return ("unknown".to_string(), None, true);
    };
    let state = inst
        .state()
        .and_then(|s| s.name())
        .map(|n| n.as_str().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let launch = inst
        .launch_time()
        .and_then(|t| DateTime::<Utc>::from_timestamp(t.secs(), t.subsec_nanos()));
    (state, launch, false)
}

/// Return (boot metric >= 1 in the lookback window, probe_error).
/// Python parity: `_boot_metric_seen`. UNPROVEN until deploy.
async fn boot_metric_seen(
    cw: &aws_sdk_cloudwatch::Client,
    now_utc: DateTime<Utc>,
    namespace: &str,
    metric_name: &str,
    host: &str,
    lookback_minutes: i64,
) -> (bool, bool) {
    let metric = aws_sdk_cloudwatch::types::Metric::builder()
        .namespace(namespace)
        .metric_name(metric_name)
        .dimensions(
            aws_sdk_cloudwatch::types::Dimension::builder()
                .name("host")
                .value(host)
                .build(),
        )
        .build();
    let stat = aws_sdk_cloudwatch::types::MetricStat::builder()
        .metric(metric)
        .period(60)
        .stat("Maximum")
        .build();
    let query = aws_sdk_cloudwatch::types::MetricDataQuery::builder()
        .id("boot")
        .metric_stat(stat)
        .return_data(true)
        .build();
    let start = now_utc - chrono::Duration::minutes(lookback_minutes);
    let resp = match cw
        .get_metric_data()
        .metric_data_queries(query)
        .start_time(aws_sdk_cloudwatch::primitives::DateTime::from_secs(
            start.timestamp(),
        ))
        .end_time(aws_sdk_cloudwatch::primitives::DateTime::from_secs(
            now_utc.timestamp(),
        ))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            error!(error = %e, "get_metric_data failed");
            return (false, true);
        }
    };
    for result in resp.metric_data_results() {
        if result.values().iter().any(|v| *v >= 1.0) {
            return (true, false);
        }
    }
    (false, false)
}

/// Entry point — 08:45 IST EventBridge (mode=readiness), manual invoke,
/// or `{"mode": "drill"}` — the SNS end-to-end verification drill.
/// Python parity: `lambda_handler`.
///
/// UNPROVEN until deploy: the live EC2/CloudWatch/SNS legs run only in a
/// real Lambda. A failed publish PROPAGATES (`?`) so the invocation error
/// feeds AWS/Lambda Errors → the readiness-errors alarm → tv_alerts —
/// the python re-raise semantics (the pager must never die silently).
pub async fn handle(event: Value) -> Result<Value, Error> {
    let instance_id = std::env::var("EC2_INSTANCE_ID").unwrap_or_default();
    let topic_arn = std::env::var("ALERTS_TOPIC_ARN").unwrap_or_default();
    let namespace =
        std::env::var("METRIC_NAMESPACE").unwrap_or_else(|_| "Tickvault/Prod".to_string());
    let metric_name =
        std::env::var("BOOT_METRIC_NAME").unwrap_or_else(|_| "tv_boot_completed".to_string());
    let host = std::env::var("METRIC_HOST").unwrap_or_else(|_| "tickvault-prod".to_string());
    let lookback_minutes = std::env::var("BOOT_LOOKBACK_MINUTES")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(DEFAULT_BOOT_LOOKBACK_MINUTES);

    let config = crate::clients::sdk_config().await;
    let ec2 = crate::clients::ec2(&config);
    let cw = crate::clients::cloudwatch(&config);
    let sns = crate::clients::sns(&config);

    let now_utc = Utc::now();
    let result = run(
        &event,
        now_utc,
        || async { instance_state(&ec2, &instance_id).await },
        || async {
            boot_metric_seen(&cw, now_utc, &namespace, &metric_name, &host, lookback_minutes).await
        },
        |subject: &'static str, message: &'static str| {
            let sns = sns.clone();
            let topic_arn = topic_arn.clone();
            async move {
                // Python parity: `_publish` — subject truncated at 99
                // chars; on failure log + RAISE so the Errors alarm pages.
                let subject_capped: String = subject.chars().take(99).collect();
                sns.publish()
                    .topic_arn(&topic_arn)
                    .subject(subject_capped)
                    .message(message)
                    .send()
                    .await
                    .map(|_| ())
                    .map_err(|e| {
                        error!(error = %e, "sns publish FAILED - raising so the Errors alarm pages");
                        e.to_string()
                    })
            }
        },
    )
    .await;
    result.map_err(Error::from)
}

// Local `use` for the Utc.fix() fallback in `ist()`.
use chrono::Offset;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn ist_tz() -> FixedOffset {
        ist()
    }

    fn at_ist(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        ist_tz()
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("valid fixture datetime")
            .with_timezone(&Utc)
    }

    use chrono::TimeZone;

    // 08:45 IST on Mon 2026-07-06 = 03:15 UTC — the scheduled invocation
    // instant (python fixture NOW).
    fn now() -> DateTime<Utc> {
        at_ist(2026, 7, 6, 8, 45)
    }
    fn launch_yesterday() -> DateTime<Utc> {
        at_ist(2026, 7, 5, 9, 0)
    }
    fn launch_today_0831() -> DateTime<Utc> {
        at_ist(2026, 7, 6, 8, 31)
    }
    fn launch_today_0832() -> DateTime<Utc> {
        at_ist(2026, 7, 6, 8, 32)
    }
    fn launch_today_0700() -> DateTime<Utc> {
        at_ist(2026, 7, 6, 7, 0)
    }
    fn launch_today_0825() -> DateTime<Utc> {
        at_ist(2026, 7, 6, 8, 25)
    }
    fn launch_today_0824() -> DateTime<Utc> {
        at_ist(2026, 7, 6, 8, 24)
    }

    // ------------------------------------------------------------------
    // Pure decision core — classify_readiness
    // ------------------------------------------------------------------

    #[test]
    fn test_running_and_booted_is_ready_silent() {
        let verdict = classify_readiness("running", Some(launch_today_0831()), true, false, now());
        assert_eq!(verdict, READY_SILENT);
        // A silent verdict must never map to a page message.
        assert!(subject_and_message(verdict).is_none());
    }

    #[test]
    fn test_running_without_boot_metric_pages_not_booted() {
        // The 2026-07-06 10:04 class once the box IS up: running but the
        // app never reported boot-complete → page, do not trust the green
        // box.
        let verdict = classify_readiness("running", Some(launch_today_0831()), false, false, now());
        assert_eq!(verdict, NOT_BOOTED_PAGE);
    }

    #[test]
    fn test_stopped_with_stale_launch_pages_not_running() {
        // The 2026-07-06 never-started case: still stopped at 08:45, last
        // LaunchTime is yesterday's session → page.
        let verdict = classify_readiness("stopped", Some(launch_yesterday()), false, false, now());
        assert_eq!(verdict, NOT_RUNNING_PAGE);
    }

    #[test]
    fn test_stopped_after_holiday_gate_self_stop_is_silent() {
        // NSE holiday: 08:30 cron started the box, the holiday gate
        // self-stopped it (~08:31 IST launch then stop) → deliberate,
        // stay silent.
        let verdict = classify_readiness("stopped", Some(launch_today_0831()), false, false, now());
        assert_eq!(verdict, HOLIDAY_SILENT);
    }

    #[test]
    fn test_stopping_after_holiday_gate_self_stop_is_silent() {
        let verdict =
            classify_readiness("stopping", Some(launch_today_0832()), false, false, now());
        assert_eq!(verdict, HOLIDAY_SILENT);
    }

    #[test]
    fn test_stopped_with_early_launch_pages_not_running() {
        // A launch BEFORE the 08:25 IST cutoff cannot be the holiday gate
        // (its earliest possible start is the 08:30 cron) → page.
        let verdict = classify_readiness("stopped", Some(launch_today_0700()), false, false, now());
        assert_eq!(verdict, NOT_RUNNING_PAGE);
    }

    #[test]
    fn test_pending_pages_not_running() {
        // `pending` at 08:45 (start-watchdog self-heal race) still pages —
        // the message wording tells the operator to watch for the
        // watchdog's own 'started' message rather than double-starting.
        let verdict = classify_readiness("pending", None, false, false, now());
        assert_eq!(verdict, NOT_RUNNING_PAGE);
        let (_, message) = subject_and_message(verdict).expect("page verdict has a message");
        assert!(message.contains("watchdog"));
    }

    #[test]
    fn test_probe_error_pages_verify_failed() {
        // Fail TOWARD paging: an AWS lookup failure (either probe) is NOT
        // an OK.
        for state in ["running", "stopped", "unknown"] {
            let verdict = classify_readiness(state, None, false, true, now());
            assert_eq!(verdict, VERIFY_FAILED_PAGE);
        }
    }

    #[test]
    fn test_subjects_are_ascii_and_within_sns_limit() {
        // SNS Subject hard limit is 100 chars (we truncate at 99) and the
        // SNS Publish API requires ASCII text beginning with a letter,
        // number, or punctuation mark — a non-ASCII (emoji-first) Subject
        // is REJECTED with InvalidParameter (round-2 review fix
        // 2026-07-06). The severity emoji leads the MESSAGE body instead
        // (house precedent: hard-stop-guard).
        for (_, subject, message) in SUBJECTS_AND_MESSAGES {
            assert!(
                subject.chars().count() <= 99,
                "subject too long: {subject:?}"
            );
            assert!(
                subject.is_ascii(),
                "SNS rejects non-ASCII subjects: {subject:?}"
            );
            let first = subject.chars().next().expect("non-empty subject");
            assert!(
                first.is_ascii_alphanumeric() || first.is_ascii_punctuation(),
                "SNS subject must begin with a letter/number/punctuation: {subject:?}"
            );
            assert!(!message.is_empty()); // never an empty page body
            assert!(
                !message.is_ascii(),
                "message body must carry the severity emoji the subject cannot: {message:?}"
            );
        }
    }

    #[test]
    fn test_holiday_cutoff_boundary_0825_ist() {
        // Pin the exact cutoff: a launch at exactly 08:25 IST is
        // holiday-eligible; 08:24 is not.
        assert!(ist_minutes_of_day(launch_today_0825()) >= HOLIDAY_SELF_STOP_EARLIEST_IST_MINUTES);
        assert!(ist_minutes_of_day(launch_today_0824()) < HOLIDAY_SELF_STOP_EARLIEST_IST_MINUTES);
        assert_eq!(
            classify_readiness("stopped", Some(launch_today_0825()), false, false, now()),
            HOLIDAY_SILENT
        );
        assert_eq!(
            classify_readiness("stopped", Some(launch_today_0824()), false, false, now()),
            NOT_RUNNING_PAGE
        );
    }

    // ------------------------------------------------------------------
    // Handler wiring — injected probes + publish, no AWS (the python
    // fake-boto3 suite ported onto the `run` seam)
    // ------------------------------------------------------------------

    struct FakeSns {
        raises: bool,
        published: RefCell<Vec<(String, String)>>,
    }

    impl FakeSns {
        fn new(raises: bool) -> Self {
            Self {
                raises,
                published: RefCell::new(Vec::new()),
            }
        }
    }

    async fn run_with(
        event: Value,
        state: Option<(&str, Option<DateTime<Utc>>)>,
        cw: Result<Vec<f64>, ()>,
        sns: &FakeSns,
    ) -> Result<Value, String> {
        run(
            &event,
            now(),
            || async move {
                match state {
                    Some((s, launch)) => (s.to_string(), launch, false),
                    None => ("unknown".to_string(), None, true),
                }
            },
            || async move {
                match cw {
                    Ok(values) => (values.iter().any(|v| *v >= 1.0), false),
                    Err(()) => (false, true),
                }
            },
            |subject, message| async move {
                if sns.raises {
                    return Err("sns down".to_string());
                }
                sns.published
                    .borrow_mut()
                    .push((subject.to_string(), message.to_string()));
                Ok(())
            },
        )
        .await
    }

    #[tokio::test]
    async fn test_handler_healthy_morning_stays_silent() {
        let sns = FakeSns::new(false);
        let out = run_with(
            json!({"mode": "readiness"}),
            Some(("running", Some(launch_today_0831()))),
            Ok(vec![1.0]),
            &sns,
        )
        .await
        .expect("healthy run succeeds");
        assert_eq!(out["verdict"], json!(READY_SILENT));
        assert_eq!(out["boot_seen"], json!(true));
        assert!(sns.published.borrow().is_empty()); // no spam on a healthy box
    }

    #[tokio::test]
    async fn test_handler_running_unbooted_pages() {
        let sns = FakeSns::new(false);
        let out = run_with(
            json!({"mode": "readiness"}),
            Some(("running", Some(launch_today_0831()))),
            Ok(vec![]),
            &sns,
        )
        .await
        .expect("page run succeeds");
        assert_eq!(out["verdict"], json!(NOT_BOOTED_PAGE));
        let published = sns.published.borrow();
        assert_eq!(published.len(), 1);
        assert!(published[0].0.contains("NOT ready"));
    }

    #[tokio::test]
    async fn test_handler_cw_probe_error_pages_verify_failed() {
        let sns = FakeSns::new(false);
        let out = run_with(
            json!({"mode": "readiness"}),
            Some(("running", Some(launch_today_0831()))),
            Err(()),
            &sns,
        )
        .await
        .expect("verify-failed run succeeds");
        assert_eq!(out["verdict"], json!(VERIFY_FAILED_PAGE));
        assert!(sns.published.borrow()[0].0.contains("could not verify"));
    }

    #[tokio::test]
    async fn test_handler_drill_mode_force_publishes_test_page() {
        // Round-1 review fix: the SNS end-to-end drill. An evening
        // stopped-box invoke classifies HOLIDAY_SILENT (LaunchTime =
        // today's 08:30 auto-start), so it can NEVER prove the page
        // route. {"mode": "drill"} must publish a clearly-labelled test
        // page regardless of EC2/boot state.
        let sns = FakeSns::new(false);
        // This EC2/CW shape would be HOLIDAY_SILENT on a readiness run —
        // the drill must page anyway (it never consults the probes).
        let out = run_with(
            json!({"mode": "drill"}),
            Some(("stopped", Some(launch_today_0831()))),
            Ok(vec![]),
            &sns,
        )
        .await
        .expect("drill run succeeds");
        assert_eq!(out["verdict"], json!(DRILL_PAGE));
        let published = sns.published.borrow();
        assert_eq!(published.len(), 1);
        assert!(published[0].0.to_lowercase().contains("test"));
        assert!(published[0].1.contains("no action needed"));
    }

    #[test]
    fn test_drill_verdict_never_returned_by_classifier() {
        // DRILL_PAGE exists only for the manual drill branch —
        // classify_readiness can never emit it (its 5 real verdicts are
        // pinned by the tests above).
        for state in ["running", "stopped", "stopping", "pending", "unknown"] {
            for boot_seen in [true, false] {
                for probe_error in [true, false] {
                    let verdict = classify_readiness(
                        state,
                        Some(launch_today_0831()),
                        boot_seen,
                        probe_error,
                        now(),
                    );
                    assert_ne!(verdict, DRILL_PAGE);
                }
            }
        }
    }

    #[tokio::test]
    async fn test_sns_publish_failure_propagates() {
        // The pager must never die silently: a failed publish RAISES so
        // the invocation error feeds AWS/Lambda Errors → the
        // readiness-errors alarm.
        let sns = FakeSns::new(true);
        let err = run_with(
            json!({"mode": "readiness"}),
            Some(("stopped", Some(launch_yesterday()))),
            Ok(vec![]),
            &sns,
        )
        .await
        .expect_err("publish failure must propagate");
        assert!(err.contains("sns down"));
    }

    // ---- Rust-side additions beyond the Python suite ----

    #[test]
    fn test_is_today_ist_splits_on_ist_midnight_not_utc() {
        // 2026-07-05 23:30 IST and 2026-07-06 00:30 IST are the SAME UTC
        // calendar day (18:00 / 19:00 UTC on the 5th) but DIFFERENT IST
        // days — the holiday heuristic must split on IST midnight.
        let before = at_ist(2026, 7, 5, 23, 30);
        let after = at_ist(2026, 7, 6, 0, 30);
        assert!(!is_today_ist(before, now()));
        assert!(is_today_ist(after, now()));
    }

    #[test]
    fn test_stopped_with_no_launch_time_pages_not_running() {
        // A stopped box the API returns without LaunchTime can never be
        // proven a holiday self-stop → fail toward paging.
        let verdict = classify_readiness("stopped", None, false, false, now());
        assert_eq!(verdict, NOT_RUNNING_PAGE);
    }
}
