//! Instance start watchdog — Rust port of
//! `deploy/aws/lambda/start-watchdog/handler.py` (phase 2b-2 wave 2).
//!
//! Answers "who monitors the 08:30 start?" and hosts the out-of-hours
//! CURFEW guard. EventBridge schedules invoke with a `mode` input:
//!
//! * mode="ping"         @ 08:30 IST — positive "start triggered" Telegram.
//! * mode="check"        @ 08:45 IST — verify the box started; self-heal
//!   (ec2:StartInstances) + page if not; flag late starts; respect the
//!   NSE-holiday intentional-stop marker (fail-open).
//! * mode="stop_check"   @ 16:45 IST — verify the 16:30 stop worked;
//!   self-stop a box left running since before 16:30; never touch a manual
//!   evening session.
//! * mode="curfew_check" hourly — stop a forgotten out-of-hours box unless
//!   a keep-alive override / launch grace applies (operator directive
//!   2026-07-03: "manual human error is not acceptable").
//!
//! Parity is the contract: same mode dispatch, same SNS Subject/Message
//! strings (byte-identical), same result-dict shapes, same IST math
//! (fixed UTC+5:30), same never-crash catch semantics on every AWS read.
//!
//! The AWS calls are abstracted behind tiny async traits so the unit tests
//! drive the FULL flow with fakes exactly like the Python monkeypatched
//! `boto3.client` fakes did (including "EC2/SSM/SNS were never touched"
//! assertions). The real SDK impls are thin, uncovered-by-design shells
//! (UNPROVEN until deploy — a live Lambda invoke is the only real probe).

use chrono::{
    DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, Offset, TimeZone, Timelike, Utc,
};
use lambda_runtime::Error;
use serde_json::{Value, json};
use tracing::{error, info, warn};

/// The daily_start EventBridge rule fires at 03:00 UTC (= 08:30 IST, Mon-Fri).
pub const START_TRIGGER_UTC_HOUR: u32 = 3;
pub const START_TRIGGER_UTC_MINUTE: u32 = 0;
/// The daily_stop EventBridge rule fires at 11:00 UTC (= 16:30 IST, Mon-Fri).
pub const STOP_TRIGGER_UTC_HOUR: u32 = 11;
pub const STOP_TRIGGER_UTC_MINUTE: u32 = 0;
/// EC2 start -> running takes <1 min; anything launched more than this many
/// minutes after the trigger means the scheduled start did NOT do it.
pub const LATE_START_GRACE_MINUTES: i64 = 5;

/// Operating hours: 08:00-17:00 IST, Mon-Fri (curfew guard inactive inside).
pub const OPERATING_OPEN_IST_MINUTES: u32 = 8 * 60;
pub const OPERATING_CLOSE_IST_MINUTES: u32 = 17 * 60;
/// Grace: never curfew-stop a box within this many minutes of its launch.
pub const CURFEW_START_GRACE_MINUTES: i64 = 45;

/// Python parity defaults for the SSM parameter names.
pub const DEFAULT_KEEP_ALIVE_PARAM: &str = "/tickvault/prod/keep-alive-until";
pub const DEFAULT_HOLIDAY_STOP_PARAM: &str = "/tickvault/prod/holiday-stop-date";

/// The fixed IST offset (+05:30). 19800s is statically in range, so the
/// fallback arm is unreachable (kept to avoid `unwrap` per the crate lints).
fn ist() -> FixedOffset {
    FixedOffset::east_opt(crate::time::IST_OFFSET_SECS).unwrap_or_else(|| Utc.fix())
}

/// Environment wiring — Python parity: module-level `os.environ.get` reads
/// with the same defaults ("" / "" / "3001" / the two SSM param paths).
#[derive(Debug, Clone)]
pub struct WatchdogEnv {
    pub instance_id: String,
    pub alerts_topic_arn: String,
    pub dashboard_port: String,
    pub keep_alive_param: String,
    pub holiday_stop_param: String,
}

impl WatchdogEnv {
    pub fn from_process_env() -> Self {
        Self {
            instance_id: std::env::var("EC2_INSTANCE_ID").unwrap_or_default(),
            alerts_topic_arn: std::env::var("ALERTS_TOPIC_ARN").unwrap_or_default(),
            dashboard_port: std::env::var("DASHBOARD_PORT").unwrap_or_else(|_| "3001".to_string()),
            keep_alive_param: std::env::var("KEEP_ALIVE_PARAM")
                .unwrap_or_else(|_| DEFAULT_KEEP_ALIVE_PARAM.to_string()),
            holiday_stop_param: std::env::var("HOLIDAY_STOP_PARAM")
                .unwrap_or_else(|_| DEFAULT_HOLIDAY_STOP_PARAM.to_string()),
        }
    }
}

/// One described instance — the fields the watchdog reads.
#[derive(Debug, Clone, Default)]
pub struct InstanceDesc {
    pub state: String,
    pub launch_time: Option<DateTime<Utc>>,
    pub public_ip: Option<String>,
}

/// EC2 surface. `Err` mirrors a raised boto3 exception; `Ok(None)` mirrors
/// an empty Reservations/Instances response ("not-found").
#[allow(async_fn_in_trait)] // APPROVED: handler is generic over the trait (no dyn); futures awaited inline.
pub trait Ec2Api {
    async fn describe(&self, instance_id: &str) -> Result<Option<InstanceDesc>, String>;
    async fn start_instances(&self, instance_id: &str) -> Result<(), String>;
    async fn stop_instances(&self, instance_id: &str) -> Result<(), String>;
}

/// SSM surface. `Err` mirrors a raised boto3 exception (ParameterNotFound,
/// AccessDenied, ...); `Ok` returns the raw parameter Value (may be empty —
/// Python's `.get("Value") or ""`).
#[allow(async_fn_in_trait)] // APPROVED: handler is generic over the trait (no dyn); futures awaited inline.
pub trait SsmApi {
    async fn get_parameter(&self, name: &str) -> Result<String, String>;
}

/// SNS surface. Python does NOT catch publish exceptions — an `Err` here
/// propagates out of the handler exactly like the raised boto3 error would.
#[allow(async_fn_in_trait)] // APPROVED: handler is generic over the trait (no dyn); futures awaited inline.
pub trait SnsApi {
    async fn publish(&self, topic_arn: &str, subject: &str, message: &str) -> Result<(), String>;
}

/// Python `_is_within_operating_hours`: 08:00-17:00 IST on a Monday-Friday.
pub fn is_within_operating_hours(now_utc: DateTime<Utc>) -> bool {
    let ist_now = now_utc.with_timezone(&ist());
    // Sat=5, Sun=6 — weekends are always curfew.
    if chrono::Datelike::weekday(&ist_now).num_days_from_monday() >= 5 {
        return false;
    }
    let minutes = ist_now.hour() * 60 + ist_now.minute();
    (OPERATING_OPEN_IST_MINUTES..OPERATING_CLOSE_IST_MINUTES).contains(&minutes)
}

/// Expand a python-3.11+-accepted HOUR-only ISO time ("2026-07-03T06",
/// "2026-07-03 06+05:30") into minute precision ("…T06:00…") so the chrono
/// format ladder can parse it. Returns None when the shape is not the
/// hour-only pattern (date + separator + exactly 2 hour digits, then end
/// or an offset start) — the caller falls through with the original string.
fn expand_hour_only_time(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    if bytes.len() < 13 || !(bytes[10] == b'T' || bytes[10] == b' ') {
        return None;
    }
    if !(bytes[11].is_ascii_digit() && bytes[12].is_ascii_digit()) {
        return None;
    }
    match bytes.get(13) {
        None => Some(format!("{raw}:00")),
        Some(b'+' | b'-' | b'Z' | b'z') => Some(format!("{}:00{}", &raw[..13], &raw[13..])),
        Some(_) => None,
    }
}

/// Python `datetime.fromisoformat(raw)` + naive-as-IST interpretation.
/// Accepts an offset-aware ISO timestamp (seconds, minute, or hour
/// precision), a naive `YYYY-MM-DD[T ]HH[:MM[:SS[.f]]]` (interpreted as
/// IST — the operator's plain local paste), or a bare date.
/// Garbage -> None (the guard stays armed — budget protection wins).
pub fn parse_keep_alive_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&Utc));
    }
    // Python 3.11+ fromisoformat (the lambda runtime is python3.12) accepts
    // HOUR-only times ("2026-07-03T06", with or without an offset) —
    // oracle-verified. chrono has no hour-only arm that defaults the minute,
    // so expand "…THH" to "…THH:00" and fall through the same ladder.
    let expanded = expand_hour_only_time(raw);
    let raw = expanded.as_deref().unwrap_or(raw);
    // chrono's `%z` does not accept the trailing `Z` python fromisoformat
    // does ("2026-07-03T06:00Z" — oracle-verified); normalize it to an
    // explicit +00:00 offset. (Seconds-precision `…SSZ` was already handled
    // by the RFC3339 arm above, so this only serves the short forms.)
    let zulu = raw
        .strip_suffix(['Z', 'z'])
        .map(|head| format!("{head}+00:00"));
    let raw = zulu.as_deref().unwrap_or(raw);
    // Python fromisoformat also accepts offsets without a colon (+0530), a
    // trailing Z, a space separator, and MINUTE-precision times without
    // seconds ("2026-07-03T06:00" — oracle-verified on python3.12); tolerate
    // all of these forms.
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f%z",
        "%Y-%m-%d %H:%M:%S%.f%z",
        "%Y-%m-%dT%H:%M%z",
        "%Y-%m-%d %H:%M%z",
    ] {
        if let Ok(dt) = DateTime::parse_from_str(raw, fmt) {
            return Some(dt.with_timezone(&Utc));
        }
    }
    let naive = [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ]
    .iter()
    .find_map(|fmt| NaiveDateTime::parse_from_str(raw, fmt).ok())
    .or_else(|| {
        NaiveDate::parse_from_str(raw, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
    })?;
    // Operator wrote a naive local (IST) timestamp.
    ist()
        .from_local_datetime(&naive)
        .single()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Python `(dt + IST_OFFSET).strftime("%I:%M %p").lstrip("0")` — 12-hour IST
/// clock with the leading zero stripped ("8:43 AM", "11:51 PM").
pub fn format_ist_12h(t: DateTime<Utc>) -> String {
    let s = t.with_timezone(&ist()).format("%I:%M %p").to_string();
    match s.strip_prefix('0') {
        Some(stripped) => stripped.to_string(),
        None => s,
    }
}

/// Python `now.replace(hour=h, minute=m, second=0, microsecond=0)`.
fn at_utc_time(now: DateTime<Utc>, hour: u32, minute: u32) -> DateTime<Utc> {
    now.with_hour(hour)
        .and_then(|d| d.with_minute(minute))
        .and_then(|d| d.with_second(0))
        .and_then(|d| d.with_nanosecond(0))
        .unwrap_or(now) // unreachable: 0<=hour<24, 0<=minute<60
}

/// Python `_instance_info` — ('unknown', None) on any error; never raises.
pub async fn instance_info<E: Ec2Api>(
    ec2: &E,
    instance_id: &str,
) -> (String, Option<DateTime<Utc>>) {
    match ec2.describe(instance_id).await {
        Err(exc) => {
            error!(error = %exc, "describe_instances failed");
            ("unknown".to_string(), None)
        }
        Ok(None) => ("not-found".to_string(), None),
        Ok(Some(desc)) => (desc.state, desc.launch_time),
    }
}

/// Python `_instance_public_ip` — None on any error / missing IP.
pub async fn instance_public_ip<E: Ec2Api>(ec2: &E, instance_id: &str) -> Option<String> {
    match ec2.describe(instance_id).await {
        Err(exc) => {
            error!(error = %exc, "describe_instances for public IP failed");
            None
        }
        Ok(desc) => desc.and_then(|d| d.public_ip).filter(|ip| !ip.is_empty()),
    }
}

/// Python `_try_self_start` — true iff the call was accepted; never raises.
pub async fn try_self_start<E: Ec2Api>(ec2: &E, instance_id: &str) -> bool {
    match ec2.start_instances(instance_id).await {
        Ok(()) => {
            info!(instance = instance_id, "self-heal start_instances issued");
            true
        }
        Err(exc) => {
            error!(error = %exc, "self-heal start_instances FAILED");
            false
        }
    }
}

/// Python `_try_self_stop` — true iff the call was accepted; never raises.
pub async fn try_self_stop<E: Ec2Api>(ec2: &E, instance_id: &str) -> bool {
    match ec2.stop_instances(instance_id).await {
        Ok(()) => {
            info!(instance = instance_id, "self-heal stop_instances issued");
            true
        }
        Err(exc) => {
            error!(error = %exc, "self-heal stop_instances FAILED");
            false
        }
    }
}

/// Python `_read_keep_alive_until` — None = no (usable) override. Never
/// raises; fail-open here would silently disable budget protection, so any
/// failure is treated as "no override" (the guard stays armed).
pub async fn read_keep_alive_until<S: SsmApi>(ssm: &S, param: &str) -> Option<DateTime<Utc>> {
    match ssm.get_parameter(param).await {
        Ok(raw) => {
            let parsed = parse_keep_alive_timestamp(&raw);
            if parsed.is_none() && !raw.trim().is_empty() {
                info!("keep-alive param unavailable/unparseable — no override");
            }
            parsed
        }
        Err(exc) => {
            info!(error = %exc, "keep-alive param unavailable/unparseable — no override");
            None
        }
    }
}

/// Python `_holiday_stop_is_today` — true iff holiday-gate.sh stamped
/// TODAY's IST date into the marker. FAIL-OPEN False on any error so a real
/// trading day can never lose the 08:45 rescue.
pub async fn holiday_stop_is_today<S: SsmApi>(
    ssm: &S,
    param: &str,
    now_utc: DateTime<Utc>,
) -> bool {
    match ssm.get_parameter(param).await {
        Ok(raw) => raw.trim() == crate::time::today_ist_string(now_utc),
        Err(exc) => {
            info!(error = %exc, "holiday-stop marker unavailable — treating as trading day");
            false
        }
    }
}

/// Python `_publish` — skips (with a warning) when the topic ARN is unset.
async fn publish<N: SnsApi>(
    sns: &N,
    env: &WatchdogEnv,
    subject: &str,
    message: &str,
) -> Result<(), String> {
    if env.alerts_topic_arn.is_empty() {
        warn!(subject, "ALERTS_TOPIC_ARN unset — cannot publish");
        return Ok(());
    }
    sns.publish(&env.alerts_topic_arn, subject, message).await
}

/// Python `lambda_handler` — the full mode dispatch, driven through the
/// injectable AWS surfaces so tests exercise the exact flow.
pub async fn run_watchdog<E: Ec2Api, S: SsmApi, N: SnsApi>(
    event: &Value,
    env: &WatchdogEnv,
    ec2: &E,
    ssm: &S,
    sns: &N,
    now: DateTime<Utc>,
) -> Result<Value, String> {
    // Python: `(event or {}).get("mode", "check")` — a missing/non-string
    // mode falls through to the default "check" branch.
    let mode = event.get("mode").and_then(Value::as_str).unwrap_or("check");
    info!(mode, instance = %env.instance_id, "start-watchdog invoked");

    if mode == "ping" {
        let mut message = "🟢 8:30 AM IST instance start triggered (Mon-Fri). \
The app should report live in ~3 minutes. \
If you do NOT get a 'tickvault started' message by 8:40 AM, the \
8:45 AM watchdog will check and start the box itself if needed."
            .to_string();
        // Append a tappable Feed Control link (IP changes each start — EIP
        // detached); if it isn't ready yet, degrade gracefully — the
        // start-triggered message must always send.
        match instance_public_ip(ec2, &env.instance_id).await {
            Some(public_ip) => {
                info!(public_ip = %public_ip, "ping — resolved public IP for dashboard link");
                message.push_str(&format!(
                    "\n📊 Dashboard: http://{public_ip}:{}/feeds",
                    env.dashboard_port
                ));
            }
            None => {
                info!("ping — public IP not ready; sending without dashboard link");
                message.push_str("\n📊 Dashboard link not ready yet — try again in a minute.");
            }
        }
        publish(sns, env, "Instance start triggered", &message).await?;
        return Ok(json!({"mode": "ping", "published": true}));
    }

    if mode == "stop_check" {
        // @ 16:45 IST: verify the 16:30 daily_stop actually stopped the box.
        // Self-heal rule: stop ONLY a box launched BEFORE today's 16:30
        // trigger; a box launched AFTER it is a deliberate manual evening
        // session and is NEVER touched.
        let (state, launch_time) = instance_info(ec2, &env.instance_id).await;
        if state != "running" {
            info!(state = %state, "stop_check OK — instance not running; staying silent");
            return Ok(json!({"mode": "stop_check", "state": state, "alerted": false}));
        }
        let stop_trigger = at_utc_time(now, STOP_TRIGGER_UTC_HOUR, STOP_TRIGGER_UTC_MINUTE);
        if let Some(lt) = launch_time
            && lt >= stop_trigger
        {
            info!(
                launch_time = %lt,
                "stop_check — running but launched after 16:30 IST: manual evening session, leaving it alone"
            );
            return Ok(json!({"mode": "stop_check", "state": state, "alerted": false}));
        }
        let Some(_) = launch_time else {
            // Can't prove it isn't a manual session — page, don't stop.
            publish(
                sns,
                env,
                "16:30 auto-stop FAILED — box still running",
                "🆘 The 4:30 PM IST auto-stop did NOT stop the box and I could \
not read its launch time, so I won't risk killing a manual \
session. If you are NOT using it right now, press \
'Stop instance' on the portal — it bills every hour it runs.",
            )
            .await?;
            error!("stop_check FAILED — running, launch_time unknown — paged");
            return Ok(json!({
                "mode": "stop_check", "state": state, "alerted": true, "self_stopped": false
            }));
        };
        let self_stopped = try_self_stop(ec2, &env.instance_id).await;
        if self_stopped {
            publish(
                sns,
                env,
                "16:30 auto-stop FAILED — watchdog stopped the box itself",
                "🆘 The 4:30 PM IST auto-stop did NOT stop the box (it had been \
running since before 4:30 PM). I have sent the stop command \
myself. If you WANTED it running this evening, just press \
'Start instance' on the portal — a box you start manually \
after 4:30 PM is never auto-stopped. Also investigate why the \
4:30 PM stop failed: AWS console → Systems Manager → \
Automation executions for AWS-StopEC2Instance.",
            )
            .await?;
        } else {
            publish(
                sns,
                env,
                "16:30 auto-stop FAILED — manual stop NEEDED",
                &format!(
                    "🆘 The 4:30 PM IST auto-stop did NOT stop the box — and the \
watchdog's own stop attempt ALSO failed. Press 'Stop instance' \
on the portal, or run \
`aws ec2 stop-instances --instance-ids {} \
--region ap-south-1`. It bills every hour it runs.",
                    env.instance_id
                ),
            )
            .await?;
        }
        error!(
            self_stopped,
            "stop_check FAILED — instance running past stop — paged"
        );
        return Ok(json!({
            "mode": "stop_check", "state": state, "alerted": true, "self_stopped": self_stopped
        }));
    }

    if mode == "curfew_check" {
        // Hourly, active OUTSIDE 08:00-17:00 IST Mon-Fri (and all weekend).
        if is_within_operating_hours(now) {
            info!("curfew_check — inside operating hours; guard inactive");
            return Ok(json!({
                "mode": "curfew_check", "skipped": "operating_hours", "stopped": false
            }));
        }
        let (state, launch_time) = instance_info(ec2, &env.instance_id).await;
        if state != "running" {
            info!(state = %state, "curfew_check OK — instance not running; staying silent");
            return Ok(json!({"mode": "curfew_check", "state": state, "stopped": false}));
        }
        let keep_alive_until = read_keep_alive_until(ssm, &env.keep_alive_param).await;
        if let Some(until) = keep_alive_until
            && now < until
        {
            info!(
                keep_alive_until = %until,
                "curfew_check — keep-alive override active; leaving the box alone"
            );
            return Ok(json!({
                "mode": "curfew_check", "state": state, "stopped": false,
                "skipped": "keep_alive"
            }));
        }
        if let Some(lt) = launch_time
            && now.signed_duration_since(lt) < Duration::minutes(CURFEW_START_GRACE_MINUTES)
        {
            info!(
                launch_time = %lt,
                grace_minutes = CURFEW_START_GRACE_MINUTES,
                "curfew_check — fresh manual start, grace window"
            );
            return Ok(json!({
                "mode": "curfew_check", "state": state, "stopped": false, "skipped": "grace"
            }));
        }
        let now_ist = format_ist_12h(now);
        let Some(_) = launch_time else {
            // Can't prove the box wasn't started seconds ago — page, don't
            // stop. The next hourly check retries.
            publish(
                sns,
                env,
                "Curfew: box running out of hours — could not verify start time",
                &format!(
                    "⚠️ At {now_ist} IST the box is running outside operating hours \
with no keep-alive, but I could not read when it was started, \
so I won't stop it this hour. The next hourly check will retry. \
If you are NOT using it, press 'Stop instance' on the portal — \
it bills every hour it runs."
                ),
            )
            .await?;
            error!("curfew_check — running, launch_time unknown — paged, not stopped");
            return Ok(json!({
                "mode": "curfew_check", "state": state, "stopped": false, "alerted": true
            }));
        };
        let self_stopped = try_self_stop(ec2, &env.instance_id).await;
        if self_stopped {
            publish(
                sns,
                env,
                "Curfew auto-stop — box stopped to protect budget",
                &format!(
                    "💤 Curfew auto-stop: box was left running at {now_ist} IST \
with no keep-alive — stopped to protect budget. \
To run overnight set keep-alive. If you were using it right \
now, press 'Start instance' on the portal and set the \
keep-alive so I don't stop it again."
                ),
            )
            .await?;
            info!(now_ist = %now_ist, "curfew_check — out-of-hours box stopped");
        } else {
            publish(
                sns,
                env,
                "Curfew auto-stop FAILED — manual stop NEEDED",
                &format!(
                    "🆘 The box is running at {now_ist} IST outside operating hours \
with no keep-alive, and my stop attempt FAILED. Press \
'Stop instance' on the portal, or run \
`aws ec2 stop-instances --instance-ids {} \
--region ap-south-1`. It bills every hour it runs.",
                    env.instance_id
                ),
            )
            .await?;
            error!("curfew_check — stop attempt FAILED — manual stop paged");
        }
        return Ok(json!({
            "mode": "curfew_check", "state": state, "stopped": self_stopped, "alerted": true
        }));
    }

    // mode == "check" (default): verify the box actually started — and fix it.
    let (state, launch_time) = instance_info(ec2, &env.instance_id).await;

    if state == "running" {
        // Late-start detection: only meaningful inside the scheduled check
        // window (03:00-04:00 UTC). A manual `mode=check` at another hour
        // must not misread an afternoon manual start as "late".
        let in_window = now.hour() == START_TRIGGER_UTC_HOUR;
        let deadline = at_utc_time(now, START_TRIGGER_UTC_HOUR, START_TRIGGER_UTC_MINUTE)
            + Duration::minutes(LATE_START_GRACE_MINUTES);
        if let Some(lt) = launch_time
            && in_window
            && lt > deadline
        {
            let launched_ist = format_ist_12h(lt);
            publish(
                sns,
                env,
                "08:30 auto-start FAILED — box was started late",
                &format!(
                    "⚠️ The box is running now, but it only came up at {launched_ist} IST — \
the scheduled 8:30 AM start did NOT work and something (or someone) \
started it later. Trading is OK for today. Investigate why the 8:30 \
start failed: AWS console → Systems Manager → Automation executions \
for AWS-StartEC2Instance, and the EventBridge rule's FailedInvocations."
                ),
            )
            .await?;
            error!(launch_time = %lt, "check — running but launched LATE — paged");
            return Ok(json!({
                "mode": "check", "state": state, "alerted": true, "self_started": false
            }));
        }
        info!("check OK — instance is running; staying silent");
        return Ok(json!({
            "mode": "check", "state": state, "alerted": false, "self_started": false
        }));
    }

    // Box is NOT running at 08:45 IST. NSE-holiday intentional stop: marker
    // == today -> the stop is intentional: stay silent, do NOT self-start.
    // FAIL-OPEN: a missing/stale/unreadable marker keeps the self-heal.
    if holiday_stop_is_today(ssm, &env.holiday_stop_param, now).await {
        info!(
            state = %state,
            "check — box is stopped but the holiday-stop marker is TODAY (NSE-holiday self-stop by holiday-gate.sh); staying silent"
        );
        return Ok(json!({
            "mode": "check", "state": state, "alerted": false, "self_started": false,
            "skipped": "holiday_stop"
        }));
    }

    // Fix first, page second.
    let self_started = try_self_start(ec2, &env.instance_id).await;
    if self_started {
        publish(
            sns,
            env,
            "08:30 auto-start FAILED — watchdog started the box itself",
            &format!(
                "🆘 The 8:30 AM IST auto-start did NOT bring the box up — it was \
'{state}' at 8:45 AM. I have sent the start command myself; expect the \
'tickvault started' message by about 8:52 AM. Market opens 9:15 AM. \
If no started-message arrives by 8:55 AM, press 'Start instance' on the \
portal. Also investigate why the 8:30 start failed: AWS console → \
Systems Manager → Automation executions for AWS-StartEC2Instance."
            ),
        )
        .await?;
    } else {
        publish(
            sns,
            env,
            "08:30 auto-start FAILED — manual start NEEDED NOW",
            &format!(
                "🆘 The 8:30 AM IST auto-start did NOT bring the box up — it is \
'{state}' at 8:45 AM — and the watchdog's own start attempt ALSO \
failed. Market opens 9:15 AM. Start it NOW: portal 'Start instance' or \
`aws ec2 start-instances --instance-ids {} \
--region ap-south-1`.",
                env.instance_id
            ),
        )
        .await?;
    }
    error!(state = %state, self_started, "check FAILED — operator paged");
    Ok(json!({
        "mode": "check", "state": state, "alerted": true, "self_started": self_started
    }))
}

// ---------------------------------------------------------------------------
// Real SDK impls — thin, uncovered-by-design (UNPROVEN until deploy).
// ---------------------------------------------------------------------------

pub struct SdkEc2 {
    client: aws_sdk_ec2::Client,
}

impl Ec2Api for SdkEc2 {
    async fn describe(&self, instance_id: &str) -> Result<Option<InstanceDesc>, String> {
        let resp = self
            .client
            .describe_instances()
            .instance_ids(instance_id)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let Some(reservation) = resp.reservations().first() else {
            return Ok(None);
        };
        let Some(inst) = reservation.instances().first() else {
            return Ok(None);
        };
        let state = inst
            .state()
            .and_then(|s| s.name())
            .map(|n| n.as_str().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let launch_time = inst
            .launch_time()
            .and_then(|t| DateTime::from_timestamp(t.secs(), t.subsec_nanos()));
        let public_ip = inst.public_ip_address().map(str::to_string);
        Ok(Some(InstanceDesc {
            state,
            launch_time,
            public_ip,
        }))
    }

    async fn start_instances(&self, instance_id: &str) -> Result<(), String> {
        self.client
            .start_instances()
            .instance_ids(instance_id)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn stop_instances(&self, instance_id: &str) -> Result<(), String> {
        self.client
            .stop_instances()
            .instance_ids(instance_id)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

pub struct SdkSsm {
    client: aws_sdk_ssm::Client,
}

impl SsmApi for SdkSsm {
    async fn get_parameter(&self, name: &str) -> Result<String, String> {
        let resp = self
            .client
            .get_parameter()
            .name(name)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        // Python: `resp.get("Parameter", {}).get("Value") or ""`.
        Ok(resp
            .parameter()
            .and_then(|p| p.value())
            .unwrap_or_default()
            .to_string())
    }
}

pub struct SdkSns {
    client: aws_sdk_sns::Client,
}

impl SnsApi for SdkSns {
    async fn publish(&self, topic_arn: &str, subject: &str, message: &str) -> Result<(), String> {
        self.client
            .publish()
            .topic_arn(topic_arn)
            .subject(subject)
            .message(message)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Entry point for the thin bin. UNPROVEN until deploy: the live AWS legs
/// run only in a real Lambda invoke.
pub async fn handle(event: Value) -> Result<Value, Error> {
    let env = WatchdogEnv::from_process_env();
    let config = crate::clients::sdk_config().await;
    let ec2 = SdkEc2 {
        client: crate::clients::ec2(&config),
    };
    let ssm = SdkSsm {
        client: crate::clients::ssm(&config),
    };
    let sns = SdkSns {
        client: crate::clients::sns(&config),
    };
    run_watchdog(&event, &env, &ec2, &ssm, &sns, Utc::now())
        .await
        .map_err(Error::from)
}

// ---------------------------------------------------------------------------
// Tests — 1:1 port of deploy/aws/lambda/start-watchdog/test_handler.py
// (31 tests; same names, same fixtures, same assertions).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::cell::RefCell;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("valid test timestamp")
    }

    // Inside the scheduled check window (03:00-04:00 UTC = 08:30-09:30 IST).
    fn in_window_now() -> DateTime<Utc> {
        utc(2026, 6, 10, 3, 15)
    }
    fn on_time_launch() -> DateTime<Utc> {
        utc(2026, 6, 10, 3, 1)
    }
    fn late_launch() -> DateTime<Utc> {
        utc(2026, 6, 10, 3, 13) // 08:43 IST
    }

    struct Published {
        topic: String,
        subject: String,
        message: String,
    }

    #[derive(Default)]
    struct FakeSns {
        published: RefCell<Vec<Published>>,
    }

    impl SnsApi for FakeSns {
        async fn publish(
            &self,
            topic_arn: &str,
            subject: &str,
            message: &str,
        ) -> Result<(), String> {
            self.published.borrow_mut().push(Published {
                topic: topic_arn.to_string(),
                subject: subject.to_string(),
                message: message.to_string(),
            });
            Ok(())
        }
    }

    struct FakeEc2 {
        state: Option<String>,
        launch_time: Option<DateTime<Utc>>,
        start_raises: bool,
        stop_raises: bool,
        public_ip: Option<String>,
        start_calls: RefCell<Vec<String>>,
        stop_calls: RefCell<Vec<String>>,
    }

    impl FakeEc2 {
        fn new(state: Option<&str>) -> Self {
            Self {
                state: state.map(str::to_string),
                launch_time: None,
                start_raises: false,
                stop_raises: false,
                public_ip: None,
                start_calls: RefCell::new(Vec::new()),
                stop_calls: RefCell::new(Vec::new()),
            }
        }
        fn launch(mut self, t: DateTime<Utc>) -> Self {
            self.launch_time = Some(t);
            self
        }
        fn ip(mut self, ip: &str) -> Self {
            self.public_ip = Some(ip.to_string());
            self
        }
        fn start_raises(mut self) -> Self {
            self.start_raises = true;
            self
        }
        fn stop_raises(mut self) -> Self {
            self.stop_raises = true;
            self
        }
    }

    impl Ec2Api for FakeEc2 {
        async fn describe(&self, _instance_id: &str) -> Result<Option<InstanceDesc>, String> {
            let Some(state) = &self.state else {
                return Err("boom".to_string());
            };
            Ok(Some(InstanceDesc {
                state: state.clone(),
                launch_time: self.launch_time,
                public_ip: self.public_ip.clone(),
            }))
        }

        async fn start_instances(&self, instance_id: &str) -> Result<(), String> {
            if self.start_raises {
                return Err("AccessDenied".to_string());
            }
            self.start_calls.borrow_mut().push(instance_id.to_string());
            Ok(())
        }

        async fn stop_instances(&self, instance_id: &str) -> Result<(), String> {
            if self.stop_raises {
                return Err("AccessDenied".to_string());
            }
            self.stop_calls.borrow_mut().push(instance_id.to_string());
            Ok(())
        }
    }

    struct FakeSsm {
        value: Option<String>,
        raises: bool,
    }

    impl FakeSsm {
        fn with(value: Option<&str>) -> Self {
            Self {
                value: value.map(str::to_string),
                raises: false,
            }
        }
        fn raising() -> Self {
            Self {
                value: None,
                raises: true,
            }
        }
    }

    impl SsmApi for FakeSsm {
        async fn get_parameter(&self, _name: &str) -> Result<String, String> {
            if self.raises || self.value.is_none() {
                return Err("ParameterNotFound".to_string());
            }
            Ok(self.value.clone().unwrap_or_default())
        }
    }

    fn test_env() -> WatchdogEnv {
        WatchdogEnv {
            instance_id: "i-0test".to_string(),
            alerts_topic_arn: "arn:aws:sns:ap-south-1:1:tv-prod-alerts".to_string(),
            dashboard_port: "3001".to_string(),
            keep_alive_param: DEFAULT_KEEP_ALIVE_PARAM.to_string(),
            holiday_stop_param: DEFAULT_HOLIDAY_STOP_PARAM.to_string(),
        }
    }

    /// The Python `_wire` fake wired boto3.client("ssm") to the SNS fake, so
    /// SSM reads in those tests raised AttributeError -> the fail-open catch
    /// arm. A raising FakeSsm reproduces that exactly.
    async fn run(
        event: Value,
        ec2: &FakeEc2,
        ssm: &FakeSsm,
        sns: &FakeSns,
        now: DateTime<Utc>,
    ) -> Value {
        run_watchdog(&event, &test_env(), ec2, ssm, sns, now)
            .await
            .expect("run_watchdog")
    }

    #[tokio::test]
    async fn test_ping_publishes_positive_signal() {
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).ip("13.200.1.2");
        let out = run(
            json!({"mode": "ping"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            in_window_now(),
        )
        .await;
        assert_eq!(out["published"], json!(true));
        let published = sns.published.borrow();
        assert_eq!(published.len(), 1);
        assert!(
            published[0]
                .subject
                .to_lowercase()
                .contains("start triggered")
        );
        assert_eq!(
            published[0].topic,
            "arn:aws:sns:ap-south-1:1:tv-prod-alerts"
        );
        // Stale-wording regression lock: the schedule is 08:30 IST, not 08:00.
        assert!(published[0].message.contains("8:30"));
        assert!(!published[0].message.contains("08:00"));
    }

    #[tokio::test]
    async fn test_ping_includes_dashboard_link() {
        // The operator's 08:30 start ping must carry a tappable Feed Control
        // link (http://<public_ip>:3001/feeds) built from the live public IP.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).ip("13.200.1.2");
        run(
            json!({"mode": "ping"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            in_window_now(),
        )
        .await;
        let published = sns.published.borrow();
        let msg = &published[0].message;
        assert!(msg.contains("/feeds"));
        assert!(msg.contains("3001"));
        assert!(msg.contains("http://13.200.1.2:3001/feeds"));
        // The original start-triggered text must remain intact.
        assert!(msg.to_lowercase().contains("start triggered"));
    }

    #[tokio::test]
    async fn test_ping_missing_public_ip_degrades_gracefully() {
        // No public IP yet -> the start message STILL sends, with no link and
        // a clear 'not ready' line — never a silent omission, never a crash.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("pending")); // box up but no IP yet
        let out = run(
            json!({"mode": "ping"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            in_window_now(),
        )
        .await;
        assert_eq!(out["published"], json!(true));
        let published = sns.published.borrow();
        let msg = &published[0].message;
        assert!(!msg.contains("http://"));
        assert!(!msg.contains("3001/feeds"));
        assert!(msg.contains("not ready yet"));
        assert!(msg.contains("8:30")); // start signal preserved
    }

    #[tokio::test]
    async fn test_ping_describe_error_degrades_gracefully() {
        // describe_instances raising must not crash the ping — it still
        // sends the start message without a dashboard link.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(None); // describe raises
        let out = run(
            json!({"mode": "ping"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            in_window_now(),
        )
        .await;
        assert_eq!(out["published"], json!(true));
        let published = sns.published.borrow();
        let msg = &published[0].message;
        assert!(!msg.contains("http://"));
        assert!(msg.contains("not ready yet"));
    }

    #[tokio::test]
    async fn test_check_running_on_time_stays_silent() {
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(on_time_launch());
        let out = run(
            json!({"mode": "check"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            in_window_now(),
        )
        .await;
        assert_eq!(out["alerted"], json!(false));
        assert!(sns.published.borrow().is_empty()); // no spam on a healthy box
    }

    #[tokio::test]
    async fn test_check_running_late_launch_pages_warning() {
        // 2026-06-10 masking case: manual 08:43 start beat the 08:45 check —
        // the box is running, but the 08:30 auto-start FAILED.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(late_launch());
        let out = run(
            json!({"mode": "check"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            in_window_now(),
        )
        .await;
        assert_eq!(out["alerted"], json!(true));
        assert_eq!(out["self_started"], json!(false));
        assert!(ec2.start_calls.borrow().is_empty()); // running box never re-started
        assert!(sns.published.borrow()[0].subject.contains("started late"));
    }

    #[tokio::test]
    async fn test_check_running_late_launch_outside_window_stays_silent() {
        // A manual `mode=check` in the afternoon must not misread an
        // afternoon manual start as 'late' — the window is 03:xx UTC only.
        let sns = FakeSns::default();
        let afternoon = utc(2026, 6, 10, 12, 0);
        let ec2 = FakeEc2::new(Some("running")).launch(utc(2026, 6, 10, 11, 30));
        let out = run(
            json!({"mode": "check"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            afternoon,
        )
        .await;
        assert_eq!(out["alerted"], json!(false));
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_check_stopped_self_starts_and_pages() {
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("stopped"));
        let out = run(
            json!({"mode": "check"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            in_window_now(),
        )
        .await;
        assert_eq!(out["alerted"], json!(true));
        assert_eq!(out["state"], json!("stopped"));
        assert_eq!(out["self_started"], json!(true));
        assert_eq!(*ec2.start_calls.borrow(), vec!["i-0test".to_string()]);
        let published = sns.published.borrow();
        assert!(published[0].subject.contains("FAILED"));
        assert!(published[0].subject.contains("started the box itself"));
    }

    #[tokio::test]
    async fn test_check_stopped_self_start_failure_still_pages_manual() {
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("stopped")).start_raises();
        let out = run(
            json!({"mode": "check"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            in_window_now(),
        )
        .await;
        assert_eq!(out["alerted"], json!(true));
        assert_eq!(out["self_started"], json!(false));
        let published = sns.published.borrow();
        assert!(published[0].subject.contains("manual start NEEDED NOW"));
        assert!(published[0].message.contains("start-instances"));
    }

    #[tokio::test]
    async fn test_check_describe_error_treated_as_not_running() {
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(None); // describe raises
        let out = run(
            json!({"mode": "check"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            in_window_now(),
        )
        .await;
        // An error means we can't confirm 'running' -> must alert, not stay
        // silent.
        assert_eq!(out["alerted"], json!(true));
        assert_eq!(out["state"], json!("unknown"));
    }

    // -----------------------------------------------------------------------
    // NSE-holiday intentional-stop marker (2026-07-07 round-3 review fix).
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_check_stopped_holiday_marker_today_stays_silent_no_start() {
        // Marker == today's IST date -> intentional holiday stop: no
        // self-start, no Critical page. in_window_now is 08:45 IST 2026-06-10.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("stopped"));
        let ssm = FakeSsm::with(Some("2026-06-10"));
        let out = run(json!({"mode": "check"}), &ec2, &ssm, &sns, in_window_now()).await;
        assert_eq!(out["alerted"], json!(false));
        assert_eq!(out["self_started"], json!(false));
        assert_eq!(out["skipped"], json!("holiday_stop"));
        assert!(ec2.start_calls.borrow().is_empty()); // the restart war never begins
        assert!(sns.published.borrow().is_empty()); // no false Critical page
    }

    #[tokio::test]
    async fn test_check_stopped_stale_holiday_marker_still_self_starts() {
        // A previous holiday's marker never matches today -> normal self-heal.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("stopped"));
        let ssm = FakeSsm::with(Some("2026-06-09")); // yesterday's holiday
        let out = run(json!({"mode": "check"}), &ec2, &ssm, &sns, in_window_now()).await;
        assert_eq!(out["alerted"], json!(true));
        assert_eq!(out["self_started"], json!(true));
        assert_eq!(*ec2.start_calls.borrow(), vec!["i-0test".to_string()]);
    }

    #[tokio::test]
    async fn test_check_stopped_marker_read_error_still_self_starts() {
        // FAIL-OPEN: an unreadable marker (missing param / AccessDenied) must
        // never suppress the trading-day 08:45 rescue.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("stopped"));
        let ssm = FakeSsm::raising();
        let out = run(json!({"mode": "check"}), &ec2, &ssm, &sns, in_window_now()).await;
        assert_eq!(out["alerted"], json!(true));
        assert_eq!(out["self_started"], json!(true));
        assert_eq!(*ec2.start_calls.borrow(), vec!["i-0test".to_string()]);
        assert!(
            sns.published.borrow()[0]
                .subject
                .contains("started the box itself")
        );
    }

    // Stop-check window: 16:45 IST = 11:15 UTC; the stop trigger is 11:00 UTC.
    fn stop_check_now() -> DateTime<Utc> {
        utc(2026, 6, 10, 11, 15)
    }
    fn morning_launch() -> DateTime<Utc> {
        utc(2026, 6, 10, 3, 13) // since morning
    }
    fn evening_launch() -> DateTime<Utc> {
        utc(2026, 6, 10, 11, 46) // manual 17:16 IST
    }

    #[tokio::test]
    async fn test_stop_check_stopped_box_stays_silent() {
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("stopped"));
        let out = run(
            json!({"mode": "stop_check"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            stop_check_now(),
        )
        .await;
        assert_eq!(out["alerted"], json!(false));
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_stop_check_running_since_morning_self_stops_and_pages() {
        // 2026-06-10 incident: 16:30 stop silently failed; box (up since
        // 08:43) still running. The 16:45 stop_check must stop it + page.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(morning_launch());
        let out = run(
            json!({"mode": "stop_check"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            stop_check_now(),
        )
        .await;
        assert_eq!(out["alerted"], json!(true));
        assert_eq!(out["self_stopped"], json!(true));
        assert_eq!(*ec2.stop_calls.borrow(), vec!["i-0test".to_string()]);
        assert!(
            sns.published.borrow()[0]
                .subject
                .contains("stopped the box itself")
        );
    }

    #[tokio::test]
    async fn test_stop_check_manual_evening_session_is_never_touched() {
        // Operator lock: out-of-window runs are manual + deliberate. A box
        // launched AFTER the 16:30 trigger must be left alone, silently.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(evening_launch());
        let out = run(
            json!({"mode": "stop_check"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            stop_check_now(),
        )
        .await;
        assert_eq!(out["alerted"], json!(false));
        assert!(ec2.stop_calls.borrow().is_empty());
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_stop_check_unknown_launch_pages_but_never_stops() {
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running"));
        let out = run(
            json!({"mode": "stop_check"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            stop_check_now(),
        )
        .await;
        assert_eq!(out["alerted"], json!(true));
        assert_eq!(out["self_stopped"], json!(false));
        // fail-safe: never kill a possible manual session
        assert!(ec2.stop_calls.borrow().is_empty());
        assert!(sns.published.borrow()[0].subject.contains("still running"));
    }

    #[tokio::test]
    async fn test_stop_check_self_stop_failure_pages_manual() {
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running"))
            .launch(morning_launch())
            .stop_raises();
        let out = run(
            json!({"mode": "stop_check"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            stop_check_now(),
        )
        .await;
        assert_eq!(out["alerted"], json!(true));
        assert_eq!(out["self_stopped"], json!(false));
        assert!(
            sns.published.borrow()[0]
                .subject
                .contains("manual stop NEEDED")
        );
    }

    #[tokio::test]
    async fn test_default_mode_is_check() {
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(on_time_launch());
        let out = run(json!({}), &ec2, &FakeSsm::raising(), &sns, in_window_now()).await;
        assert_eq!(out["mode"], json!("check"));
    }

    // -----------------------------------------------------------------------
    // Curfew guard (mode="curfew_check") — operator directive 2026-07-03.
    // -----------------------------------------------------------------------

    // Thu 2026-07-02 23:51 IST = 18:21 UTC — the operator's real incident.
    fn curfew_night_now() -> DateTime<Utc> {
        utc(2026, 7, 2, 18, 21)
    }
    // Launched hours earlier (17:30 IST) — well past the 45-min grace.
    fn curfew_old_launch() -> DateTime<Utc> {
        utc(2026, 7, 2, 12, 0)
    }
    // Launched 20 min before now — inside the 45-min grace window.
    fn curfew_fresh_launch() -> DateTime<Utc> {
        utc(2026, 7, 2, 18, 1)
    }
    // Tue 2026-06-30 10:00 IST = 04:30 UTC — mid-market, guard must not act.
    fn market_hours_now() -> DateTime<Utc> {
        utc(2026, 6, 30, 4, 30)
    }
    // Sat 2026-07-04 11:00 IST = 05:30 UTC — weekend, guard active all day.
    fn weekend_now() -> DateTime<Utc> {
        utc(2026, 7, 4, 5, 30)
    }

    #[tokio::test]
    async fn test_curfew_running_no_override_out_of_hours_stops_and_pages() {
        // The 23:51 incident: box left running at night, no keep-alive -> STOP.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(curfew_old_launch());
        let out = run(
            json!({"mode": "curfew_check"}),
            &ec2,
            &FakeSsm::with(None),
            &sns,
            curfew_night_now(),
        )
        .await;
        assert_eq!(out["stopped"], json!(true));
        assert_eq!(*ec2.stop_calls.borrow(), vec!["i-0test".to_string()]);
        let published = sns.published.borrow();
        assert!(published[0].subject.contains("Curfew auto-stop"));
        assert!(published[0].message.contains("💤"));
        assert!(published[0].message.contains("keep-alive"));
    }

    #[tokio::test]
    async fn test_curfew_keep_alive_override_skips() {
        // Explicit keep-alive in the future -> the operator WANTS it running.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(curfew_old_launch());
        let ssm = FakeSsm::with(Some("2026-07-03T06:00:00+05:30")); // tomorrow 6 AM IST
        let out = run(
            json!({"mode": "curfew_check"}),
            &ec2,
            &ssm,
            &sns,
            curfew_night_now(),
        )
        .await;
        assert_eq!(out["stopped"], json!(false));
        assert_eq!(out["skipped"], json!("keep_alive"));
        assert!(ec2.stop_calls.borrow().is_empty());
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_curfew_expired_keep_alive_still_stops() {
        // A keep-alive that already lapsed no longer protects the box.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(curfew_old_launch());
        let ssm = FakeSsm::with(Some("2026-07-02T20:00:00+05:30")); // 8 PM IST — past
        let out = run(
            json!({"mode": "curfew_check"}),
            &ec2,
            &ssm,
            &sns,
            curfew_night_now(),
        )
        .await;
        assert_eq!(out["stopped"], json!(true));
        assert_eq!(*ec2.stop_calls.borrow(), vec!["i-0test".to_string()]);
    }

    #[tokio::test]
    async fn test_curfew_naive_keep_alive_is_interpreted_as_ist() {
        // Operator pastes a plain IST timestamp with no offset — honored.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(curfew_old_launch());
        let ssm = FakeSsm::with(Some("2026-07-03T06:00:00")); // naive -> IST -> future
        let out = run(
            json!({"mode": "curfew_check"}),
            &ec2,
            &ssm,
            &sns,
            curfew_night_now(),
        )
        .await;
        assert_eq!(out["stopped"], json!(false));
        assert_eq!(out["skipped"], json!("keep_alive"));
    }

    #[test]
    fn test_parse_keep_alive_minute_precision_matches_python_oracle() {
        // Oracle: python3 datetime.fromisoformat (lambda runtime python3.12)
        // ACCEPTS minute-precision forms — verified 2026-07-18:
        //   '2026-07-03T06:00'        -> datetime(2026, 7, 3, 6, 0)
        //   '2026-07-03 06:00'        -> datetime(2026, 7, 3, 6, 0)
        //   '2026-07-03T06:00+05:30'  -> datetime(..., tzinfo=+05:30)
        //   '2026-07-03T06:00+0530'   -> datetime(..., tzinfo=+05:30)
        //   '2026-07-03T06:00Z'       -> datetime(..., tzinfo=UTC)
        // Naive forms are the operator's plain IST paste -> 06:00 IST
        // == 00:30 UTC.
        let ist_0030_utc = utc(2026, 7, 3, 0, 30);
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03T06:00"),
            Some(ist_0030_utc)
        );
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03 06:00"),
            Some(ist_0030_utc)
        );
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03T06:00+05:30"),
            Some(ist_0030_utc)
        );
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03 06:00+0530"),
            Some(ist_0030_utc)
        );
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03T06:00Z"),
            Some(utc(2026, 7, 3, 6, 0))
        );
    }

    #[test]
    fn test_parse_keep_alive_hour_only_matches_python312_oracle() {
        // Oracle: python3.11+/3.12 fromisoformat ALSO accepts hour-only
        // times (rejected in <=3.10; the lambda runtime is python3.12) —
        // verified 2026-07-18:
        //   '2026-07-03T06'        -> datetime(2026, 7, 3, 6, 0)
        //   '2026-07-03 06'        -> datetime(2026, 7, 3, 6, 0)
        //   '2026-07-03T06+05:30'  -> datetime(..., tzinfo=+05:30)
        //   '2026-07-03T06Z'       -> datetime(..., tzinfo=UTC)
        let ist_0030_utc = utc(2026, 7, 3, 0, 30);
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03T06"),
            Some(ist_0030_utc)
        );
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03 06"),
            Some(ist_0030_utc)
        );
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03T06+05:30"),
            Some(ist_0030_utc)
        );
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03T06Z"),
            Some(utc(2026, 7, 3, 6, 0))
        );
    }

    #[test]
    fn test_parse_keep_alive_garbage_still_rejected() {
        // Garbage must keep returning None — the curfew guard stays armed.
        for garbage in [
            "not-a-timestamp",
            "2026-07-03Tgarbage",
            "2026-07-03T99:99",
            "2026-13-40T06:00",
            "T06:00",
            "",
        ] {
            assert_eq!(parse_keep_alive_timestamp(garbage), None, "{garbage:?}");
        }
    }

    #[tokio::test]
    async fn test_curfew_malformed_keep_alive_treated_as_no_override() {
        // Garbage in the param must not silently disable budget protection.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(curfew_old_launch());
        let ssm = FakeSsm::with(Some("not-a-timestamp"));
        let out = run(
            json!({"mode": "curfew_check"}),
            &ec2,
            &ssm,
            &sns,
            curfew_night_now(),
        )
        .await;
        assert_eq!(out["stopped"], json!(true));
    }

    #[tokio::test]
    async fn test_curfew_grace_window_skips_fresh_manual_start() {
        // Launched 20 min ago (<45 min grace) -> operator actively working.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(curfew_fresh_launch());
        let out = run(
            json!({"mode": "curfew_check"}),
            &ec2,
            &FakeSsm::with(None),
            &sns,
            curfew_night_now(),
        )
        .await;
        assert_eq!(out["stopped"], json!(false));
        assert_eq!(out["skipped"], json!("grace"));
        assert!(ec2.stop_calls.borrow().is_empty());
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_curfew_never_fires_during_operating_hours() {
        // 08:00-17:00 IST Mon-Fri belongs to the normal schedule — the
        // curfew guard must return WITHOUT touching EC2, SSM, or SNS.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(curfew_old_launch());
        let out = run(
            json!({"mode": "curfew_check"}),
            &ec2,
            &FakeSsm::with(None),
            &sns,
            market_hours_now(),
        )
        .await;
        assert_eq!(out["skipped"], json!("operating_hours"));
        assert_eq!(out["stopped"], json!(false));
        assert!(ec2.stop_calls.borrow().is_empty());
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_curfew_active_on_weekend_midday() {
        // Saturday 11:00 IST is OUTSIDE operating hours (Mon-Fri only) —
        // a forgotten weekend box is stopped.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(curfew_old_launch());
        let out = run(
            json!({"mode": "curfew_check"}),
            &ec2,
            &FakeSsm::with(None),
            &sns,
            weekend_now(),
        )
        .await;
        assert_eq!(out["stopped"], json!(true));
        assert_eq!(*ec2.stop_calls.borrow(), vec!["i-0test".to_string()]);
    }

    #[tokio::test]
    async fn test_curfew_stopped_box_stays_silent() {
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("stopped"));
        let out = run(
            json!({"mode": "curfew_check"}),
            &ec2,
            &FakeSsm::with(None),
            &sns,
            curfew_night_now(),
        )
        .await;
        assert_eq!(out["stopped"], json!(false));
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_curfew_unknown_launch_time_pages_but_never_stops() {
        // Fail-safe (same idiom as stop_check): can't prove it isn't a
        // seconds-old manual start -> page, don't stop; next hour retries.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running"));
        let out = run(
            json!({"mode": "curfew_check"}),
            &ec2,
            &FakeSsm::with(None),
            &sns,
            curfew_night_now(),
        )
        .await;
        assert_eq!(out["stopped"], json!(false));
        assert_eq!(out["alerted"], json!(true));
        assert!(ec2.stop_calls.borrow().is_empty());
        assert!(
            sns.published.borrow()[0]
                .subject
                .contains("could not verify start time")
        );
    }

    #[tokio::test]
    async fn test_curfew_stop_failure_pages_manual() {
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running"))
            .launch(curfew_old_launch())
            .stop_raises();
        let out = run(
            json!({"mode": "curfew_check"}),
            &ec2,
            &FakeSsm::with(None),
            &sns,
            curfew_night_now(),
        )
        .await;
        assert_eq!(out["stopped"], json!(false));
        assert_eq!(out["alerted"], json!(true));
        let published = sns.published.borrow();
        assert!(published[0].subject.contains("manual stop NEEDED"));
        assert!(published[0].message.contains("stop-instances"));
    }

    #[test]
    fn test_operating_hours_boundaries() {
        // Pin the exact window: [08:00, 17:00) IST, Mon-Fri only.
        let ist_tz = FixedOffset::east_opt(crate::time::IST_OFFSET_SECS).expect("ist offset");
        let at = |y: i32, mo: u32, d: u32, h: u32, mi: u32| {
            ist_tz
                .with_ymd_and_hms(y, mo, d, h, mi, 0)
                .single()
                .expect("valid IST timestamp")
                .with_timezone(&Utc)
        };
        // Wed 2026-07-01 boundaries.
        assert!(!is_within_operating_hours(at(2026, 7, 1, 7, 59)));
        assert!(is_within_operating_hours(at(2026, 7, 1, 8, 0)));
        assert!(is_within_operating_hours(at(2026, 7, 1, 16, 59)));
        assert!(!is_within_operating_hours(at(2026, 7, 1, 17, 0)));
        // Sunday midday is never operating hours.
        assert!(!is_within_operating_hours(at(2026, 7, 5, 12, 0)));
    }
}
