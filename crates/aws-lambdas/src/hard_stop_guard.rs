//! Hard auto-stop guard — Rust port of
//! `deploy/aws/lambda/hard-stop-guard/handler.py` (phase 2b-2 wave 2).
//!
//! Hourly EventBridge cron; the budget/schedule never-cross safety net:
//!
//! * Instance already stopped/stopping -> no-op.
//! * Running OUTSIDE the Mon-Fri 08:30-16:30 IST up-window -> force stop +
//!   Telegram (the legacy never-cross guard, unchanged).
//! * Running INSIDE the up-window:
//!     - MTD spend >= BUDGET_KILL_USD -> stop the instance, DISABLE the
//!       tv-<env>-daily-start EventBridge rule, page the operator (GAP 1:
//!       without the disable, the next morning's start cron restarts the
//!       killed box and runs it daily, unkilled, for the rest of the month).
//!       Evaluated FIRST — the breach stop is NEVER gated on ping state.
//!     - Otherwise the running ping is CHANGE-ONLY (2026-07-09 Telegram
//!       noise N2): one SSM String param holds `{"month","bucket","outcome"}`
//!       and pings fire only on state-missing / new-month / cost-unknown
//!       edge / cost-recovered / 10%-bucket boundary crossings (1-bucket
//!       dips are hysteresis-suppressed).
//!
//! Parity is the contract: same SNS Subject/Message strings (byte-identical),
//! same result-dict shapes, same IST math (fixed UTC+5:30), same fail-safe
//! posture (Cost Explorer errors NEVER stop or disable anything; SSM state
//! errors fail OPEN to a ping — one ping beats a silent guard).
//!
//! The AWS calls are abstracted behind tiny async traits so the unit tests
//! drive the FULL flow with fakes exactly like the Python monkeypatched
//! `boto3.client` fakes did (including the ExplodingSsm "SSM was never
//! touched" ratchets). The real SDK impls are thin, uncovered-by-design
//! shells (UNPROVEN until deploy — a live Lambda invoke is the only probe).

use chrono::{DateTime, Datelike, FixedOffset, Offset, Timelike, Utc};
use lambda_runtime::Error;
use serde_json::{Value, json};
use tracing::{error, info, warn};

/// Python parity: `BUDGET_KILL_USD = float(os.environ.get(..., "55"))`.
/// KEEP IN SYNC with budget.tf `limit_amount` AND `BUDGET_USD` in the daily
/// digest — the three MUST agree on "breached".
pub const DEFAULT_BUDGET_KILL_USD: f64 = 55.0;
/// Python parity default for the change-only ping-state SSM param
/// (NOT under the banned groww/* namespace).
pub const DEFAULT_PING_STATE_PARAM: &str = "/tickvault/prod/budget-guard/ping-state";
/// Spend buckets are 10%-of-stop-budget steps.
pub const BUCKET_PCT_STEP: i64 = 10;
/// Bucket 8 (80%) starts the "approaching stop-budget" subject flavor;
/// bucket 9+ is the red flavor.
pub const APPROACH_BUCKET: i64 = 8;

/// The fixed IST offset (+05:30). 19800s is statically in range, so the
/// fallback arm is unreachable (kept to avoid `unwrap` per the crate lints).
fn ist() -> FixedOffset {
    FixedOffset::east_opt(crate::time::IST_OFFSET_SECS).unwrap_or_else(|| Utc.fix())
}

/// Environment wiring — Python parity: module-level `os.environ.get` reads.
/// NOTE: this lambda's instance env var is `INSTANCE_ID`, NOT
/// `EC2_INSTANCE_ID` (python parity — the two guard lambdas differ).
#[derive(Debug, Clone)]
pub struct GuardEnv {
    pub instance_id: String,
    pub alerts_topic_arn: String,
    pub start_rule_name: String,
    pub budget_kill_usd: f64,
    pub ping_state_param: String,
}

impl GuardEnv {
    pub fn from_process_env() -> Self {
        Self {
            instance_id: std::env::var("INSTANCE_ID").unwrap_or_default(),
            alerts_topic_arn: std::env::var("ALERTS_TOPIC_ARN").unwrap_or_default(),
            start_rule_name: std::env::var("START_RULE_NAME").unwrap_or_default(),
            // Deliberate deviation: python `float(...)` raises at import on a
            // garbage value (Lambda init error). We fail-soft to the default
            // with a loud warn — a mis-typed env var must not kill the
            // never-cross guard entirely.
            budget_kill_usd: match std::env::var("BUDGET_KILL_USD") {
                Ok(raw) => raw.parse::<f64>().unwrap_or_else(|e| {
                    warn!(error = %e, raw = %raw, "BUDGET_KILL_USD unparsable — using default 55");
                    DEFAULT_BUDGET_KILL_USD
                }),
                Err(_) => DEFAULT_BUDGET_KILL_USD,
            },
            ping_state_param: std::env::var("PING_STATE_PARAM")
                .unwrap_or_else(|_| DEFAULT_PING_STATE_PARAM.to_string()),
        }
    }
}

/// One described instance — the fields this guard reads.
#[derive(Debug, Clone, Default)]
pub struct GuardInstance {
    pub state: String,
    pub launch_time: Option<DateTime<Utc>>,
}

/// EC2 surface. `Err` mirrors a raised boto3 exception (uncaught in python —
/// it propagates out of the handler); `Ok(None)` mirrors an empty
/// Reservations/Instances response (python would IndexError — also fatal).
#[allow(async_fn_in_trait)] // APPROVED: handler is generic over the trait (no dyn); futures awaited inline.
pub trait Ec2Api {
    async fn describe(&self, instance_id: &str) -> Result<Option<GuardInstance>, String>;
    async fn stop_instances(&self, instance_id: &str) -> Result<(), String>;
}

/// SNS surface. Python does NOT catch publish exceptions — an `Err` here
/// propagates out of the handler exactly like the raised boto3 error would.
#[allow(async_fn_in_trait)] // APPROVED: handler is generic over the trait (no dyn); futures awaited inline.
pub trait SnsApi {
    async fn publish(&self, topic_arn: &str, subject: &str, message: &str) -> Result<(), String>;
}

/// EventBridge surface — `events:DisableRule` only. Errors are CAUGHT by
/// the breach path (page-don't-crash) and reported honestly in the page.
#[allow(async_fn_in_trait)] // APPROVED: handler is generic over the trait (no dyn); futures awaited inline.
pub trait EventsApi {
    async fn disable_rule(&self, name: &str) -> Result<(), String>;
}

/// Cost Explorer surface — the raw `Amount` strings per result period
/// (python sums `float(d["Total"]["UnblendedCost"]["Amount"])`).
#[allow(async_fn_in_trait)] // APPROVED: handler is generic over the trait (no dyn); futures awaited inline.
pub trait CeApi {
    async fn unblended_amounts(&self, start: &str, end: &str) -> Result<Vec<String>, String>;
}

/// SSM surface for the change-only ping state (get + put).
#[allow(async_fn_in_trait)] // APPROVED: handler is generic over the trait (no dyn); futures awaited inline.
pub trait SsmApi {
    async fn get_parameter(&self, name: &str) -> Result<String, String>;
    async fn put_parameter(&self, name: &str, value: &str) -> Result<(), String>;
}

/// Python `in_up_window`: Mon-Fri 08:30-16:30 IST (inclusive both ends).
/// The guard NEVER stops the box during the window on schedule grounds —
/// only a budget breach may stop it in-window.
pub fn in_up_window(now_utc: DateTime<Utc>) -> bool {
    let ist_now = now_utc.with_timezone(&ist());
    if ist_now.weekday().num_days_from_monday() >= 5 {
        return false;
    }
    let hhmm = ist_now.hour() * 100 + ist_now.minute();
    (830..=1630).contains(&hhmm)
}

/// Python `mtd_usd` — best-effort Cost Explorer month-to-date total.
/// Returns `None` on ANY error — the caller treats None as fail-safe
/// ("cost_unknown": page, never stop or disable on missing data).
/// Deliberate deviation: python read `datetime.now(timezone.utc)` inside;
/// we take the injected clock so the date window is testable/consistent.
pub async fn mtd_usd<C: CeApi>(ce: &C, now_utc: DateTime<Utc>) -> Option<f64> {
    let today = now_utc.date_naive();
    let Some(start) = today.with_day(1) else {
        // Day 1 always exists in the current month — unreachable, fail-soft.
        warn!("MTD lookup failed (non-fatal): month start unrepresentable");
        return None;
    };
    match ce
        .unblended_amounts(&start.to_string(), &today.to_string())
        .await
    {
        Ok(amounts) => {
            let mut total = 0.0_f64;
            for a in amounts {
                match a.parse::<f64>() {
                    Ok(v) => total += v,
                    Err(e) => {
                        // Python float() raise -> caught by the blanket arm.
                        warn!(error = %e, "MTD lookup failed (non-fatal): bad Amount");
                        return None;
                    }
                }
            }
            Some(total)
        }
        Err(e) => {
            warn!(error = %e, "MTD lookup failed (non-fatal)");
            None
        }
    }
}

/// Python `classify_in_window_action` — pure decision for the in-window arm
/// (GAP 1 fix): breach_stop / cost_unknown (fail-safe) / below_budget.
pub fn classify_in_window_action(mtd: Option<f64>, budget_kill_usd: f64) -> &'static str {
    match mtd {
        None => "cost_unknown",
        Some(v) if v >= budget_kill_usd => "breach_stop",
        Some(_) => "below_budget",
    }
}

/// Python `spend_bucket` — which 10%-of-stop-budget bucket the MTD spend
/// sits in. Total, never panics: unknown/negative spend or a non-positive
/// kill line clamps to bucket 0; the percentage is capped at 999 so a
/// runaway spend cannot overflow anything. NaN parity with the python
/// oracle: python's `int(min(nan, 999.0) // 10)` raised ValueError and the
/// `except` arm returned 0 (Rust `f64::min` ignores NaN, so this is mapped
/// explicitly below); `+inf` takes python's `min(inf, 999.0) == 999.0`
/// path -> bucket 99, which Rust's `min` already matches.
pub fn spend_bucket(mtd: Option<f64>, budget_kill_usd: f64) -> i64 {
    let Some(mtd) = mtd else { return 0 };
    if mtd < 0.0 || budget_kill_usd <= 0.0 {
        return 0;
    }
    let pct = (mtd / budget_kill_usd) * 100.0;
    if pct.is_nan() {
        // Python oracle: int(nan) raises -> except -> 0.
        return 0;
    }
    // APPROVED: floor of a value capped at 999 / BUCKET_PCT_STEP — always fits i64; NaN returned 0 above.
    #[allow(clippy::cast_possible_truncation)]
    let bucket = (pct.min(999.0) / BUCKET_PCT_STEP as f64).floor() as i64;
    bucket
}

/// Python `classify_ping_decision` — pure change-only ping decision
/// (2026-07-09, Telegram noise N2). Returns `(reason, new_state)`;
/// reason ∈ ping_state_missing / ping_new_month / ping_cost_unknown_edge /
/// ping_cost_recovered / ping_bucket_changed / silent. Fail-open by design:
/// a missing or corrupt prev state PINGS. The breach stop never reaches this
/// function — the caller returns via `execute_breach_stop` BEFORE any state
/// read.
pub fn classify_ping_decision(
    prev: Option<&Value>,
    action: &str,
    mtd: Option<f64>,
    budget_kill_usd: f64,
    month: &str,
) -> (&'static str, Value) {
    let bucket = spend_bucket(mtd, budget_kill_usd);
    let mut new_state = json!({"month": month, "bucket": bucket, "outcome": action});
    // cost_unknown first: it has no trustworthy spend figure, so no other
    // reason may render one; edge-latched so repeats stay silent.
    if action == "cost_unknown" {
        if prev.and_then(|p| p.get("outcome")).and_then(Value::as_str) == Some("cost_unknown") {
            return ("silent", new_state);
        }
        return ("ping_cost_unknown_edge", new_state);
    }
    // Python isinstance validation: dict with month:str, bucket:int,
    // outcome:str — anything else is a missing/corrupt state (fail-open).
    let valid = prev.filter(|p| p.is_object()).and_then(|p| {
        Some((
            p.get("month").and_then(Value::as_str)?,
            p.get("bucket").and_then(Value::as_i64)?,
            p.get("outcome").and_then(Value::as_str)?,
        ))
    });
    let Some((prev_month, prev_bucket, prev_outcome)) = valid else {
        return ("ping_state_missing", new_state);
    };
    if prev_month != month {
        return ("ping_new_month", new_state);
    }
    if prev_outcome == "cost_unknown" {
        return ("ping_cost_recovered", new_state);
    }
    if bucket > prev_bucket {
        return ("ping_bucket_changed", new_state);
    }
    if prev_bucket - bucket >= 2 {
        // A >= 2-bucket decrease is a real mid-month credit/refund —
        // rare and genuinely informative.
        return ("ping_bucket_changed", new_state);
    }
    if bucket < prev_bucket {
        // Hysteresis (hostile-review fix 2026-07-09): Cost Explorer
        // UnblendedCost estimates are revised BOTH ways, so spend sitting
        // on a 10% boundary can flip one bucket down and back up every
        // hour. A 1-bucket dip stays SILENT and RETAINS the stored bucket,
        // so the flip back up is silent too.
        new_state["bucket"] = json!(prev_bucket);
        return ("silent", new_state);
    }
    ("silent", new_state)
}

/// Python `render_running_ping` — pure `(subject, message)` for a
/// change-only running ping. Strings are byte-identical to python.
pub fn render_running_ping(
    reason: &str,
    mtd: Option<f64>,
    bucket: i64,
    budget_kill_usd: f64,
    hrs_up: f64,
    instance_id: &str,
) -> (String, String) {
    if reason == "ping_cost_unknown_edge" {
        return (
            "[BUDGET] cost check failed — box left running".to_string(),
            format!(
                "🟡 Could not read this month's spend (fail-safe: box NOT \
stopped, auto-start NOT disabled).\n\
_instance_: `{instance_id}` — up {hrs_up:.1}h this boot.\n\
Will report again when the cost check recovers.\n"
            ),
        );
    }
    let mtd_val = mtd.unwrap_or(0.0);
    let pct = if budget_kill_usd > 0.0 {
        (mtd_val / budget_kill_usd) * 100.0
    } else {
        0.0
    };
    let spend_line =
        format!("Month spend ~${mtd_val:.2} of ${budget_kill_usd:.0} stop-budget ({pct:.0}%).");
    if reason == "ping_cost_recovered" {
        return (
            "[BUDGET] cost check recovered".to_string(),
            format!(
                "🟢 Cost check is working again. {spend_line}\n\
_instance_: `{instance_id}` — up {hrs_up:.1}h this boot.\n\
Next update only when spend crosses the next 10% step.\n"
            ),
        );
    }
    let subject = if reason == "ping_new_month" {
        "[BUDGET] new month — spend counter reset".to_string()
    } else if reason == "ping_state_missing" {
        "[BUDGET] box running — first spend status".to_string()
    } else if bucket > APPROACH_BUCKET {
        "[BUDGET] approaching stop-budget — 90% used".to_string()
    } else if bucket == APPROACH_BUCKET {
        "[BUDGET] approaching stop-budget — 80% used".to_string()
    } else {
        format!(
            "[BUDGET] spend crossed {}% of stop-budget",
            bucket * BUCKET_PCT_STEP
        )
    };
    let (emoji, tail) = if bucket >= APPROACH_BUCKET {
        (
            if bucket > APPROACH_BUCKET {
                "🔴"
            } else {
                "⚠️"
            },
            format!(
                "At ${budget_kill_usd:.0} the box STOPS and the morning \
auto-start is DISABLED. If this month's spend is expected, \
raise the budget before it trips.\n"
            ),
        )
    } else {
        (
            "🟡",
            "Box is in the trading window and left running. Next update \
only when spend crosses the next 10% step.\n"
                .to_string(),
        )
    };
    (
        subject,
        format!(
            "{emoji} {spend_line}\n\
_instance_: `{instance_id}` — up {hrs_up:.1}h this boot.\n\
{tail}"
        ),
    )
}

/// Python `load_ping_state` — best-effort SSM state read; ANY error returns
/// `None` (fail-open: the classifier then pings; one ping beats a silent
/// guard). Non-object JSON also reads as None (python `isinstance(dict)`).
pub async fn load_ping_state<P: SsmApi>(ssm: &P, param: &str) -> Option<Value> {
    match ssm.get_parameter(param).await {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(v) if v.is_object() => Some(v),
            Ok(_) => None,
            Err(e) => {
                warn!(error = %e, "ping-state read failed (fail-open to ping)");
                None
            }
        },
        Err(e) => {
            warn!(error = %e, "ping-state read failed (fail-open to ping)");
            None
        }
    }
}

/// Python `save_ping_state` — best-effort SSM state write; a failed write is
/// logged and the handler still returns ok (worst case: one duplicate
/// ping/hour until the write lands — duplicate-over-silent).
pub async fn save_ping_state<P: SsmApi>(ssm: &P, param: &str, state: &Value) -> bool {
    match ssm.put_parameter(param, &state.to_string()).await {
        Ok(()) => true,
        Err(e) => {
            warn!(error = %e, "ping-state write failed (next hour may re-ping)");
            false
        }
    }
}

/// Python `_execute_breach_stop` — stop the box + disable the morning start
/// rule + page (GAP 1). Ordering is deliberate: stop FIRST (caps spend even
/// if the disable fails), then disable the start rule, then page — the page
/// honestly reports whether the disable landed so the operator can finish
/// by hand.
pub async fn execute_breach_stop<E: Ec2Api, N: SnsApi, V: EventsApi>(
    env: &GuardEnv,
    ec2: &E,
    sns: &N,
    events: &V,
    mtd: f64,
    was_state: &str,
) -> Result<Value, String> {
    ec2.stop_instances(&env.instance_id).await?;

    let mut disable_ok = false;
    let disable_err;
    if env.start_rule_name.is_empty() {
        disable_err = "START_RULE_NAME env var empty".to_string();
    } else {
        match events.disable_rule(&env.start_rule_name).await {
            Ok(()) => {
                disable_ok = true;
                disable_err = String::new();
            }
            Err(e) => {
                // Python: page-don't-crash (logger.exception + continue).
                error!(error = %e, rule = %env.start_rule_name, "events:DisableRule failed");
                disable_err = e;
            }
        }
    }

    let (subject, disable_line) = if disable_ok {
        (
            "[BUDGET] breached — box stopped + auto-start disabled".to_string(),
            format!(
                "Morning auto-start rule `{}` DISABLED.",
                env.start_rule_name
            ),
        )
    } else {
        (
            "[BUDGET] breached — box stopped; auto-start disable FAILED".to_string(),
            format!(
                "⚠ could NOT disable the morning auto-start ({disable_err}) — \
disable rule `{}` manually NOW or the box \
restarts at 8:30 AM tomorrow.",
                env.start_rule_name
            ),
        )
    };

    // Python `subject[:99]` — char-boundary-safe truncation.
    let subject_capped: String = subject.chars().take(99).collect();
    let message = format!(
        "🛑 *Budget breached — box stopped + morning auto-start DISABLED \
until operator re-enables*\n\
_instance_: `{instance_id}`\n\
_was_state_: {was_state}\n\
_MTD spend_: ~${mtd:.2} >= ${kill:.0} stop-budget\n\
{disable_line}\n\
Why: the native AWS Budget stop-actions fire only ONCE per\n\
month-crossing. Without disabling the start rule, tomorrow's\n\
8:30 AM auto-start would restart the box and it would run\n\
every day for the rest of the month, unkilled.\n\
After investigating the spend, re-enable with:\n  aws events enable-rule --name {rule}\n",
        instance_id = env.instance_id,
        kill = env.budget_kill_usd,
        rule = env.start_rule_name,
    );
    sns.publish(&env.alerts_topic_arn, &subject_capped, &message)
        .await?;
    Ok(json!({
        "ok": true,
        "noop": false,
        "breach": true,
        "disable_ok": disable_ok,
        "was_state": was_state,
    }))
}

/// Python `lambda_handler` — hourly EventBridge cron entry (event unused).
pub async fn run_guard<E: Ec2Api, N: SnsApi, V: EventsApi, C: CeApi, P: SsmApi>(
    env: &GuardEnv,
    ec2: &E,
    sns: &N,
    events: &V,
    ce: &C,
    ssm: &P,
    now_utc: DateTime<Utc>,
) -> Result<Value, String> {
    if env.instance_id.is_empty() {
        error!("INSTANCE_ID env var is empty — cannot guard instance");
        return Ok(json!({"ok": false, "reason": "missing INSTANCE_ID"}));
    }
    if env.alerts_topic_arn.is_empty() {
        error!("ALERTS_TOPIC_ARN env var is empty — cannot page operator");
        return Ok(json!({"ok": false, "reason": "missing ALERTS_TOPIC_ARN"}));
    }

    // Python indexes Reservations[0].Instances[0] — an absent instance
    // raises (IndexError) and fails the invoke; `Err` here mirrors that.
    let inst = ec2
        .describe(&env.instance_id)
        .await?
        .ok_or_else(|| "describe_instances returned no instance".to_string())?;
    let state = inst.state.clone();

    if matches!(
        state.as_str(),
        "stopped" | "stopping" | "shutting-down" | "terminated"
    ) {
        info!(instance = %env.instance_id, state = %state, "already stopped, no-op");
        return Ok(json!({"ok": true, "noop": true, "state": state}));
    }

    if in_up_window(now_utc) {
        // Running DURING the trading window — expected. GAP 1 check first:
        // even in-window, a breached budget stops the box + kills the
        // morning restart. The breach stop is evaluated BEFORE any ping
        // state read — it can NEVER be gated on ping-state problems.
        let usd = mtd_usd(ce, now_utc).await;
        let action = classify_in_window_action(usd, env.budget_kill_usd);
        if action == "breach_stop" {
            // breach_stop implies usd is Some; 0.0 fallback is unreachable.
            return execute_breach_stop(env, ec2, sns, events, usd.unwrap_or(0.0), &state).await;
        }

        // 2026-07-09 (operator escalation — Telegram noise N2): the running
        // ping is CHANGE-ONLY — never an identical hourly repeat. The
        // 17:30 IST daily digest remains the end-of-day summary.
        let hrs_up = inst
            .launch_time
            .map(|l| (now_utc - l).num_milliseconds() as f64 / 3_600_000.0)
            .unwrap_or(0.0);
        let prev = load_ping_state(ssm, &env.ping_state_param).await;
        let month = now_utc.format("%Y-%m").to_string();
        let (reason, new_state) =
            classify_ping_decision(prev.as_ref(), action, usd, env.budget_kill_usd, &month);
        let mut state_saved = true;
        if reason != "silent" {
            let bucket = new_state.get("bucket").and_then(Value::as_i64).unwrap_or(0);
            let (subject, message) = render_running_ping(
                reason,
                usd,
                bucket,
                env.budget_kill_usd,
                hrs_up,
                &env.instance_id,
            );
            let subject_capped: String = subject.chars().take(99).collect();
            sns.publish(&env.alerts_topic_arn, &subject_capped, &message)
                .await?;
            // Save only when something was said — a silent hour by
            // construction leaves the state semantically unchanged.
            state_saved = save_ping_state(ssm, &env.ping_state_param, &new_state).await;
        }
        let bucket = new_state.get("bucket").and_then(Value::as_i64).unwrap_or(0);
        info!(action, reason, bucket, "in-window budget check");
        return Ok(json!({
            "ok": true,
            "noop": true,
            "in_window": true,
            "state": state,
            "budget_action": action,
            "ping_reason": reason,
            "state_saved": state_saved,
        }));
    }

    // Running OUTSIDE the up-window — force stop + alert (the legacy
    // never-cross guard, unchanged by GAP 1).
    ec2.stop_instances(&env.instance_id).await?;
    sns.publish(
        &env.alerts_topic_arn,
        "[BUDGET] hard auto-stop guard fired",
        &format!(
            "🔴 *Hard Auto-Stop Guard Fired*\n\
_instance_: `{}`\n\
_was_state_: {state}\n\
Hourly out-of-window guard found the box running OUTSIDE the\n\
08:30-16:30 IST Mon-Fri window. EventBridge 16:30 stop (or a\n\
manual start) left it up. Now stopped to protect the budget.\n",
            env.instance_id
        ),
    )
    .await?;
    Ok(json!({"ok": true, "noop": false, "was_state": state}))
}

// ---------------------------------------------------------------------------
// Real SDK impls — thin, uncovered-by-design (UNPROVEN until deploy).
// ---------------------------------------------------------------------------

pub struct SdkEc2 {
    client: aws_sdk_ec2::Client,
}

impl Ec2Api for SdkEc2 {
    async fn describe(&self, instance_id: &str) -> Result<Option<GuardInstance>, String> {
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
        Ok(Some(GuardInstance { state, launch_time }))
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

pub struct SdkEvents {
    client: aws_sdk_eventbridge::Client,
}

impl EventsApi for SdkEvents {
    async fn disable_rule(&self, name: &str) -> Result<(), String> {
        self.client
            .disable_rule()
            .name(name)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

pub struct SdkCe {
    client: aws_sdk_costexplorer::Client,
}

impl CeApi for SdkCe {
    async fn unblended_amounts(&self, start: &str, end: &str) -> Result<Vec<String>, String> {
        let interval = aws_sdk_costexplorer::types::DateInterval::builder()
            .start(start)
            .end(end)
            .build()
            .map_err(|e| e.to_string())?;
        let r = self
            .client
            .get_cost_and_usage()
            .time_period(interval)
            .granularity(aws_sdk_costexplorer::types::Granularity::Monthly)
            .metrics("UnblendedCost")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let mut out = Vec::with_capacity(r.results_by_time().len());
        for period in r.results_by_time() {
            // Python `d["Total"]["UnblendedCost"]["Amount"]` — a missing key
            // raises inside mtd_usd's blanket catch -> None; Err mirrors it.
            let amount = period
                .total()
                .and_then(|t| t.get("UnblendedCost"))
                .and_then(|m| m.amount())
                .ok_or_else(|| "missing Total.UnblendedCost.Amount".to_string())?;
            out.push(amount.to_string());
        }
        Ok(out)
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
        Ok(resp
            .parameter()
            .and_then(|p| p.value())
            .unwrap_or_default()
            .to_string())
    }

    async fn put_parameter(&self, name: &str, value: &str) -> Result<(), String> {
        self.client
            .put_parameter()
            .name(name)
            .value(value)
            .r#type(aws_sdk_ssm::types::ParameterType::String)
            .overwrite(true)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Entry point for the thin bin. UNPROVEN until deploy: the live AWS legs
/// run only in a real Lambda invoke.
pub async fn handle(_event: Value) -> Result<Value, Error> {
    let env = GuardEnv::from_process_env();
    let config = crate::clients::sdk_config().await;
    let ec2 = SdkEc2 {
        client: crate::clients::ec2(&config),
    };
    let sns = SdkSns {
        client: crate::clients::sns(&config),
    };
    let events = SdkEvents {
        client: crate::clients::eventbridge(&config),
    };
    let ce = SdkCe {
        client: crate::clients::cost_explorer_us_east_1().await,
    };
    let ssm = SdkSsm {
        client: crate::clients::ssm(&config),
    };
    run_guard(&env, &ec2, &sns, &events, &ce, &ssm, Utc::now())
        .await
        .map_err(Error::from)
}

// ---------------------------------------------------------------------------
// Tests — 1:1 port of deploy/aws/lambda/hard-stop-guard/test_handler.py
// (34 tests; same names, same fixtures, same assertions).
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

    // 2026-07-07 is a Tuesday. 04:30 UTC = 10:00 IST (inside 08:30-16:30).
    fn in_window() -> DateTime<Utc> {
        utc(2026, 7, 7, 4, 30)
    }
    // Tuesday 14:00 UTC = 19:30 IST (outside window).
    fn out_of_window() -> DateTime<Utc> {
        utc(2026, 7, 7, 14, 0)
    }
    // 2026-07-04 is a Saturday, 10:00 IST — weekday gate must reject.
    fn saturday() -> DateTime<Utc> {
        utc(2026, 7, 4, 4, 30)
    }

    struct FakeEc2 {
        state: String,
        stopped: RefCell<Vec<Vec<String>>>,
    }

    impl FakeEc2 {
        fn new(state: &str) -> Self {
            Self {
                state: state.to_string(),
                stopped: RefCell::new(Vec::new()),
            }
        }
    }

    impl Ec2Api for FakeEc2 {
        async fn describe(&self, _instance_id: &str) -> Result<Option<GuardInstance>, String> {
            Ok(Some(GuardInstance {
                state: self.state.clone(),
                // Python fake: LaunchTime = 2026-07-07 03:00 UTC.
                launch_time: Some(utc(2026, 7, 7, 3, 0)),
            }))
        }

        async fn stop_instances(&self, instance_id: &str) -> Result<(), String> {
            self.stopped
                .borrow_mut()
                .push(vec![instance_id.to_string()]);
            Ok(())
        }
    }

    struct Published {
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
            _topic_arn: &str,
            subject: &str,
            message: &str,
        ) -> Result<(), String> {
            self.published.borrow_mut().push(Published {
                subject: subject.to_string(),
                message: message.to_string(),
            });
            Ok(())
        }
    }

    struct FakeEvents {
        fail: bool,
        disabled: RefCell<Vec<String>>,
    }

    impl FakeEvents {
        fn new() -> Self {
            Self {
                fail: false,
                disabled: RefCell::new(Vec::new()),
            }
        }
        fn failing() -> Self {
            Self {
                fail: true,
                disabled: RefCell::new(Vec::new()),
            }
        }
    }

    impl EventsApi for FakeEvents {
        async fn disable_rule(&self, name: &str) -> Result<(), String> {
            if self.fail {
                return Err("AccessDeniedException".to_string());
            }
            self.disabled.borrow_mut().push(name.to_string());
            Ok(())
        }
    }

    /// Cost Explorer fake — returns a fixed MTD or raises.
    struct FakeCe {
        amount: Option<f64>,
    }

    impl CeApi for FakeCe {
        async fn unblended_amounts(&self, _start: &str, _end: &str) -> Result<Vec<String>, String> {
            match self.amount {
                Some(a) => Ok(vec![format!("{a}")]),
                None => Err("CE throttled".to_string()),
            }
        }
    }

    /// SSM fake for the change-only ping state (2026-07-09). read_fail /
    /// write_fail simulate outages; EVERY call is recorded so the
    /// breach-path test can prove SSM was never touched.
    struct FakeSsm {
        value: RefCell<Option<String>>,
        read_fail: bool,
        write_fail: bool,
        reads: RefCell<u32>,
        writes: RefCell<Vec<String>>,
    }

    impl FakeSsm {
        fn new(value: Option<&str>) -> Self {
            Self {
                value: RefCell::new(value.map(str::to_string)),
                read_fail: false,
                write_fail: false,
                reads: RefCell::new(0),
                writes: RefCell::new(Vec::new()),
            }
        }
        fn read_failing(value: Option<&str>) -> Self {
            Self {
                read_fail: true,
                ..Self::new(value)
            }
        }
        fn write_failing() -> Self {
            Self {
                write_fail: true,
                ..Self::new(None)
            }
        }
    }

    impl SsmApi for FakeSsm {
        async fn get_parameter(&self, _name: &str) -> Result<String, String> {
            *self.reads.borrow_mut() += 1;
            let value = self.value.borrow().clone();
            match value {
                Some(v) if !self.read_fail => Ok(v),
                _ => Err("ParameterNotFound".to_string()),
            }
        }

        async fn put_parameter(&self, _name: &str, value: &str) -> Result<(), String> {
            if self.write_fail {
                return Err("AccessDeniedException".to_string());
            }
            self.writes.borrow_mut().push(value.to_string());
            Ok(())
        }
    }

    /// An SSM client whose ANY use panics — proves a path never touches it.
    struct ExplodingSsm;

    impl SsmApi for ExplodingSsm {
        async fn get_parameter(&self, name: &str) -> Result<String, String> {
            panic!("SSM must never be used on this path: get_parameter {name}");
        }
        async fn put_parameter(&self, name: &str, _value: &str) -> Result<(), String> {
            panic!("SSM must never be used on this path: put_parameter {name}");
        }
    }

    fn test_env() -> GuardEnv {
        GuardEnv {
            instance_id: "i-tvapp".to_string(),
            alerts_topic_arn: "arn:aws:sns:ap-south-1:111:tv-prod-alerts".to_string(),
            start_rule_name: "tv-prod-daily-start".to_string(),
            budget_kill_usd: 55.0,
            ping_state_param: DEFAULT_PING_STATE_PARAM.to_string(),
        }
    }

    fn state_json(month: &str, bucket: i64) -> String {
        json!({"month": month, "bucket": bucket, "outcome": "below_budget"}).to_string()
    }

    // --- ClassifyInWindowAction --------------------------------------------

    #[test]
    fn test_none_mtd_is_cost_unknown() {
        assert_eq!(classify_in_window_action(None, 55.0), "cost_unknown");
    }

    #[test]
    fn test_below_threshold_is_below_budget() {
        assert_eq!(classify_in_window_action(Some(54.99), 55.0), "below_budget");
    }

    #[test]
    fn test_exactly_at_threshold_is_breach() {
        assert_eq!(classify_in_window_action(Some(55.0), 55.0), "breach_stop");
    }

    #[test]
    fn test_above_threshold_is_breach() {
        assert_eq!(classify_in_window_action(Some(57.31), 55.0), "breach_stop");
    }

    #[test]
    fn test_zero_spend_is_below_budget() {
        assert_eq!(classify_in_window_action(Some(0.0), 55.0), "below_budget");
    }

    // --- InUpWindow --------------------------------------------------------

    #[test]
    fn test_tuesday_10am_ist_is_in_window() {
        assert!(in_up_window(in_window()));
    }

    #[test]
    fn test_tuesday_evening_ist_is_out_of_window() {
        assert!(!in_up_window(out_of_window()));
    }

    #[test]
    fn test_saturday_is_out_of_window() {
        assert!(!in_up_window(saturday()));
    }

    #[test]
    fn test_0829_ist_is_out_and_0830_is_in() {
        // 02:59 UTC = 08:29 IST; 03:00 UTC = 08:30 IST (Tuesday).
        assert!(!in_up_window(utc(2026, 7, 7, 2, 59)));
        assert!(in_up_window(utc(2026, 7, 7, 3, 0)));
    }

    #[test]
    fn test_1630_ist_is_in_and_1631_is_out() {
        // 11:00 UTC = 16:30 IST; 11:01 UTC = 16:31 IST (Tuesday).
        assert!(in_up_window(utc(2026, 7, 7, 11, 0)));
        assert!(!in_up_window(utc(2026, 7, 7, 11, 1)));
    }

    // --- MtdUsd ------------------------------------------------------------

    #[tokio::test]
    async fn test_returns_sum_from_ce() {
        let got = mtd_usd(
            &FakeCe {
                amount: Some(57.31),
            },
            in_window(),
        )
        .await;
        assert!((got.expect("Some") - 57.31).abs() < 1e-9);
    }

    #[tokio::test]
    async fn test_returns_none_on_ce_error() {
        assert!(
            mtd_usd(&FakeCe { amount: None }, in_window())
                .await
                .is_none()
        );
    }

    // --- LambdaHandlerEnvGuards --------------------------------------------

    #[tokio::test]
    async fn test_missing_instance_id_returns_not_ok() {
        let env = GuardEnv {
            instance_id: String::new(),
            ..test_env()
        };
        let out = run_guard(
            &env,
            &FakeEc2::new("running"),
            &FakeSns::default(),
            &FakeEvents::new(),
            &FakeCe { amount: Some(1.0) },
            &FakeSsm::new(None),
            in_window(),
        )
        .await
        .expect("ok");
        assert_eq!(out["ok"], json!(false));
        assert!(out["reason"].as_str().expect("str").contains("INSTANCE_ID"));
    }

    #[tokio::test]
    async fn test_missing_topic_arn_returns_not_ok() {
        let env = GuardEnv {
            alerts_topic_arn: String::new(),
            ..test_env()
        };
        let out = run_guard(
            &env,
            &FakeEc2::new("running"),
            &FakeSns::default(),
            &FakeEvents::new(),
            &FakeCe { amount: Some(1.0) },
            &FakeSsm::new(None),
            in_window(),
        )
        .await
        .expect("ok");
        assert_eq!(out["ok"], json!(false));
        assert!(
            out["reason"]
                .as_str()
                .expect("str")
                .contains("ALERTS_TOPIC_ARN")
        );
    }

    // --- HandlerScenarios (the three GAP 1 contract scenarios + legacy) ----

    async fn run_scenario(
        ec2: &FakeEc2,
        sns: &FakeSns,
        events: &FakeEvents,
        ce: &FakeCe,
        now: DateTime<Utc>,
        ssm: &FakeSsm,
    ) -> Value {
        run_guard(&test_env(), ec2, sns, events, ce, ssm, now)
            .await
            .expect("run_guard ok")
    }

    #[tokio::test]
    async fn test_breach_in_window_stops_disables_and_pages() {
        let (ec2, sns, events) = (
            FakeEc2::new("running"),
            FakeSns::default(),
            FakeEvents::new(),
        );
        let ssm = FakeSsm::new(None);
        let out = run_scenario(
            &ec2,
            &sns,
            &events,
            &FakeCe {
                amount: Some(57.31),
            },
            in_window(),
            &ssm,
        )
        .await;
        assert_eq!(out["breach"], json!(true));
        assert_eq!(out["disable_ok"], json!(true));
        assert_eq!(*ec2.stopped.borrow(), vec![vec!["i-tvapp".to_string()]]);
        assert_eq!(
            *events.disabled.borrow(),
            vec!["tv-prod-daily-start".to_string()]
        );
        let published = sns.published.borrow();
        assert_eq!(published.len(), 1);
        let msg = &published[0].message;
        assert!(msg.contains("Budget breached"));
        assert!(msg.contains("auto-start DISABLED"));
        assert!(msg.contains("aws events enable-rule --name tv-prod-daily-start"));
        // Byte-exact python-oracle parity (rendered by RUNNING
        // `_execute_breach_stop` from handler.py @ 9b1c2e6a1^ with the same
        // inputs: INSTANCE_ID=i-tvapp, START_RULE_NAME=tv-prod-daily-start,
        // BUDGET_KILL_USD=55, mtd=57.31, was_state=running, disable ok).
        // Fix round F1: the 2-space indent before the enable-rule command
        // must survive — a `\n\` source continuation stripped it.
        assert_eq!(
            msg.as_str(),
            "🛑 *Budget breached — box stopped + morning auto-start DISABLED \
until operator re-enables*\n\
_instance_: `i-tvapp`\n\
_was_state_: running\n\
_MTD spend_: ~$57.31 >= $55 stop-budget\n\
Morning auto-start rule `tv-prod-daily-start` DISABLED.\n\
Why: the native AWS Budget stop-actions fire only ONCE per\n\
month-crossing. Without disabling the start rule, tomorrow's\n\
8:30 AM auto-start would restart the box and it would run\n\
every day for the rest of the month, unkilled.\n\
After investigating the spend, re-enable with:\n  aws events enable-rule --name tv-prod-daily-start\n"
        );
        assert!(published[0].subject.chars().count() <= 99);
    }

    #[tokio::test]
    async fn test_below_threshold_in_window_first_run_pings_then_same_bucket_silent() {
        // First run (no state): fail-open ping + state seeded. The next
        // hour in the SAME bucket is SILENT (2026-07-09 change-only pings).
        let (ec2, sns, events) = (
            FakeEc2::new("running"),
            FakeSns::default(),
            FakeEvents::new(),
        );
        let ssm = FakeSsm::new(None);
        let out = run_scenario(
            &ec2,
            &sns,
            &events,
            &FakeCe {
                amount: Some(31.20),
            },
            in_window(),
            &ssm,
        )
        .await;
        assert_eq!(out["noop"], json!(true));
        assert_eq!(out["budget_action"], json!("below_budget"));
        assert_eq!(out["ping_reason"], json!("ping_state_missing"));
        assert!(ec2.stopped.borrow().is_empty());
        assert!(events.disabled.borrow().is_empty());
        assert_eq!(sns.published.borrow().len(), 1);
        assert!(sns.published.borrow()[0].message.contains("$31.20"));
        assert_eq!(
            ssm.writes.borrow().len(),
            1,
            "state seeded on the first ping"
        );
        // Hour 2, same bucket -> silent, no extra write.
        let seeded = ssm.writes.borrow().last().expect("seeded").clone();
        *ssm.value.borrow_mut() = Some(seeded);
        let out2 = run_scenario(
            &ec2,
            &sns,
            &events,
            &FakeCe {
                amount: Some(32.50),
            },
            in_window(),
            &ssm,
        )
        .await;
        assert_eq!(out2["ping_reason"], json!("silent"));
        assert_eq!(
            sns.published.borrow().len(),
            1,
            "identical hour stays silent"
        );
        assert_eq!(ssm.writes.borrow().len(), 1);
    }

    #[tokio::test]
    async fn test_ce_error_in_window_fail_safe_pages_without_disable() {
        let (ec2, sns, events) = (
            FakeEc2::new("running"),
            FakeSns::default(),
            FakeEvents::new(),
        );
        let ssm = FakeSsm::new(None);
        let out = run_scenario(
            &ec2,
            &sns,
            &events,
            &FakeCe { amount: None },
            in_window(),
            &ssm,
        )
        .await;
        assert_eq!(out["noop"], json!(true));
        assert_eq!(out["budget_action"], json!("cost_unknown"));
        assert_eq!(out["ping_reason"], json!("ping_cost_unknown_edge"));
        // FAIL-SAFE: no stop, no disable on a Cost Explorer failure.
        assert!(ec2.stopped.borrow().is_empty());
        assert!(events.disabled.borrow().is_empty());
        assert_eq!(sns.published.borrow().len(), 1);
        assert!(sns.published.borrow()[0].message.contains("box NOT"));
    }

    #[tokio::test]
    async fn test_disable_failure_still_stops_and_pages_honestly() {
        let (ec2, sns) = (FakeEc2::new("running"), FakeSns::default());
        let events = FakeEvents::failing();
        let ssm = FakeSsm::new(None);
        let out = run_scenario(
            &ec2,
            &sns,
            &events,
            &FakeCe { amount: Some(60.0) },
            in_window(),
            &ssm,
        )
        .await;
        assert_eq!(out["breach"], json!(true));
        assert_eq!(out["disable_ok"], json!(false));
        // Stop still landed even though the disable failed.
        assert_eq!(*ec2.stopped.borrow(), vec![vec!["i-tvapp".to_string()]]);
        assert!(sns.published.borrow()[0].subject.contains("disable FAILED"));
        assert!(sns.published.borrow()[0].message.contains("manually NOW"));
    }

    #[tokio::test]
    async fn test_out_of_window_force_stop_never_disables() {
        let (ec2, sns, events) = (
            FakeEc2::new("running"),
            FakeSns::default(),
            FakeEvents::new(),
        );
        let ssm = FakeSsm::new(None);
        let out = run_scenario(
            &ec2,
            &sns,
            &events,
            &FakeCe {
                amount: Some(57.31),
            },
            out_of_window(),
            &ssm,
        )
        .await;
        assert_eq!(out["noop"], json!(false));
        assert_eq!(*ec2.stopped.borrow(), vec![vec!["i-tvapp".to_string()]]);
        // Legacy out-of-window arm preserved: stop + page, NO rule disable.
        assert!(events.disabled.borrow().is_empty());
        assert!(
            sns.published.borrow()[0]
                .message
                .contains("Hard Auto-Stop Guard Fired")
        );
    }

    #[tokio::test]
    async fn test_already_stopped_is_noop() {
        let (ec2, sns, events) = (
            FakeEc2::new("stopped"),
            FakeSns::default(),
            FakeEvents::new(),
        );
        let ssm = FakeSsm::new(None);
        let out = run_scenario(
            &ec2,
            &sns,
            &events,
            &FakeCe {
                amount: Some(57.31),
            },
            in_window(),
            &ssm,
        )
        .await;
        assert_eq!(out["noop"], json!(true));
        assert!(ec2.stopped.borrow().is_empty());
        assert!(events.disabled.borrow().is_empty());
        assert!(sns.published.borrow().is_empty());
    }

    // --- ChangeOnlyPingClassifier (pure classifier tests, 2026-07-09) ------

    #[test]
    fn test_spend_bucket_steps_and_clamps() {
        assert_eq!(spend_bucket(Some(0.0), 55.0), 0);
        assert_eq!(spend_bucket(Some(33.4), 55.0), 6); // 60.7%
        assert_eq!(spend_bucket(Some(44.0), 55.0), 8); // 80.0%
        assert_eq!(spend_bucket(Some(49.8), 55.0), 9); // 90.5%
        assert_eq!(spend_bucket(None, 55.0), 0);
        assert_eq!(spend_bucket(Some(-1.0), 55.0), 0);
        assert_eq!(spend_bucket(Some(10.0), 0.0), 0);
        assert_eq!(spend_bucket(Some(1e12), 55.0), 99); // capped
        // Python-oracle parity (verified: python3 try/except spend_bucket):
        // nan -> 0 (int(nan) raised -> except -> 0), +inf -> 99
        // (min(inf, 999.0) == 999.0), -inf -> 0 (mtd < 0), inf/inf -> 0
        // (nan pct -> raised -> 0).
        assert_eq!(spend_bucket(Some(f64::NAN), 55.0), 0);
        assert_eq!(spend_bucket(Some(f64::INFINITY), 55.0), 99);
        assert_eq!(spend_bucket(Some(f64::NEG_INFINITY), 55.0), 0);
        assert_eq!(spend_bucket(Some(f64::INFINITY), f64::INFINITY), 0);
    }

    #[test]
    fn test_same_bucket_silent() {
        let prev = json!({"month": "2026-07", "bucket": 6, "outcome": "below_budget"});
        let (reason, _) =
            classify_ping_decision(Some(&prev), "below_budget", Some(33.9), 55.0, "2026-07");
        assert_eq!(reason, "silent");
    }

    #[test]
    fn test_bucket_increase_pings() {
        let prev = json!({"month": "2026-07", "bucket": 5, "outcome": "below_budget"});
        let (reason, new_state) =
            classify_ping_decision(Some(&prev), "below_budget", Some(33.4), 55.0, "2026-07");
        assert_eq!(reason, "ping_bucket_changed");
        assert_eq!(new_state["bucket"], json!(6));
    }

    #[test]
    fn test_bucket_decrease_pings() {
        // A >= 2-bucket mid-month credit/refund decrease is informative —
        // ping (bucket 6 -> 3 here).
        let prev = json!({"month": "2026-07", "bucket": 6, "outcome": "below_budget"});
        let (reason, _) =
            classify_ping_decision(Some(&prev), "below_budget", Some(20.0), 55.0, "2026-07");
        assert_eq!(reason, "ping_bucket_changed");
    }

    #[test]
    fn test_bucket_decrease_one_step_silent_hysteresis() {
        // Hostile-review fix 2026-07-09: spend sitting on a 10% boundary
        // with Cost Explorer estimate jitter (revised DOWN one bucket, then
        // back up) must not re-open hourly repeats. The 1-bucket dip is
        // silent AND retains the stored bucket, so the flip back up is
        // silent too.
        let prev = json!({"month": "2026-07", "bucket": 6, "outcome": "below_budget"});
        // 32.9/55 = 59.8% -> bucket 5: one step down -> silent, bucket kept.
        let (reason, new_state) =
            classify_ping_decision(Some(&prev), "below_budget", Some(32.9), 55.0, "2026-07");
        assert_eq!(reason, "silent");
        assert_eq!(new_state["bucket"], json!(6), "stored bucket retained");
        // Next hour the estimate flips back up to bucket 6 -> still silent.
        let (reason2, _) = classify_ping_decision(
            Some(&new_state),
            "below_budget",
            Some(33.1),
            55.0,
            "2026-07",
        );
        assert_eq!(reason2, "silent");
    }

    #[test]
    fn test_month_rollover_pings_once() {
        let prev = json!({"month": "2026-07", "bucket": 9, "outcome": "below_budget"});
        let (reason, new_state) =
            classify_ping_decision(Some(&prev), "below_budget", Some(0.5), 55.0, "2026-08");
        assert_eq!(reason, "ping_new_month");
        // The seeded state silences the following identical hour.
        let (reason2, _) =
            classify_ping_decision(Some(&new_state), "below_budget", Some(0.6), 55.0, "2026-08");
        assert_eq!(reason2, "silent");
    }

    #[test]
    fn test_cost_unknown_edge_latched() {
        let prev = json!({"month": "2026-07", "bucket": 6, "outcome": "below_budget"});
        let (reason, new_state) =
            classify_ping_decision(Some(&prev), "cost_unknown", None, 55.0, "2026-07");
        assert_eq!(reason, "ping_cost_unknown_edge");
        // Repeats stay silent while latched.
        let (reason2, _) =
            classify_ping_decision(Some(&new_state), "cost_unknown", None, 55.0, "2026-07");
        assert_eq!(reason2, "silent");
    }

    #[test]
    fn test_cost_recovered_pings() {
        let prev = json!({"month": "2026-07", "bucket": 6, "outcome": "cost_unknown"});
        let (reason, _) =
            classify_ping_decision(Some(&prev), "below_budget", Some(33.4), 55.0, "2026-07");
        assert_eq!(reason, "ping_cost_recovered");
    }

    #[test]
    fn test_state_missing_pings_and_seeds() {
        let prevs: Vec<Option<Value>> = vec![
            None,
            Some(json!("garbage")),
            Some(json!({"month": 7})),
            Some(json!({"bucket": "x"})),
            Some(json!({})),
        ];
        for prev in prevs {
            let (reason, new_state) =
                classify_ping_decision(prev.as_ref(), "below_budget", Some(33.4), 55.0, "2026-07");
            assert_eq!(reason, "ping_state_missing", "prev={prev:?}");
            assert_eq!(new_state["month"], json!("2026-07"));
            assert_eq!(new_state["bucket"], json!(6));
        }
    }

    #[test]
    fn test_approaching_wording_at_bucket_8_and_9() {
        let (subj8, msg8) =
            render_running_ping("ping_bucket_changed", Some(44.0), 8, 55.0, 2.0, "i-x");
        assert!(subj8.contains("approaching stop-budget — 80% used"));
        assert!(msg8.contains("STOPS"));
        let (subj9, msg9) =
            render_running_ping("ping_bucket_changed", Some(49.8), 9, 55.0, 2.0, "i-x");
        assert!(subj9.contains("approaching stop-budget — 90% used"));
        assert!(msg9.contains("🔴"));
        let (subj6, msg6) =
            render_running_ping("ping_bucket_changed", Some(33.4), 6, 55.0, 2.0, "i-x");
        assert!(subj6.contains("spend crossed 60% of stop-budget"));
        assert!(msg6.contains("next 10% step"));
    }

    // --- ChangeOnlyPingHandler (handler-level SSM failure posture) ---------

    #[tokio::test]
    async fn test_ssm_read_failure_fails_open_to_ping() {
        // An unreadable state must PING (one ping beats a silent guard).
        let ssm = FakeSsm::read_failing(Some(&state_json("2026-07", 6)));
        let (ec2, sns, events) = (
            FakeEc2::new("running"),
            FakeSns::default(),
            FakeEvents::new(),
        );
        let out = run_scenario(
            &ec2,
            &sns,
            &events,
            &FakeCe { amount: Some(33.4) },
            in_window(),
            &ssm,
        )
        .await;
        assert_eq!(out["ping_reason"], json!("ping_state_missing"));
        assert_eq!(sns.published.borrow().len(), 1);
    }

    #[tokio::test]
    async fn test_ssm_write_failure_still_returns_ok() {
        // A failed state write is logged; the handler stays ok. Worst case:
        // one duplicate ping next hour (duplicate-over-silent).
        let ssm = FakeSsm::write_failing();
        let (ec2, sns, events) = (
            FakeEc2::new("running"),
            FakeSns::default(),
            FakeEvents::new(),
        );
        let out = run_scenario(
            &ec2,
            &sns,
            &events,
            &FakeCe { amount: Some(33.4) },
            in_window(),
            &ssm,
        )
        .await;
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["state_saved"], json!(false));
        assert_eq!(sns.published.borrow().len(), 1);
    }

    #[tokio::test]
    async fn test_breach_stop_ignores_ping_state() {
        // RATCHET: the breach stop is evaluated BEFORE any state read — an
        // SSM outage (or poisoned state) can never gate the stop.
        let (ec2, sns, events) = (
            FakeEc2::new("running"),
            FakeSns::default(),
            FakeEvents::new(),
        );
        let out = run_guard(
            &test_env(),
            &ec2,
            &sns,
            &events,
            &FakeCe {
                amount: Some(57.31),
            },
            &ExplodingSsm,
            in_window(),
        )
        .await
        .expect("run_guard ok");
        assert_eq!(out["breach"], json!(true));
        assert_eq!(*ec2.stopped.borrow(), vec![vec!["i-tvapp".to_string()]]);
        assert_eq!(
            *events.disabled.borrow(),
            vec!["tv-prod-daily-start".to_string()]
        );
    }

    #[tokio::test]
    async fn test_out_of_window_force_stop_never_touches_ssm() {
        // The legacy out-of-window arm is untouched — no state read/write.
        let (ec2, sns, events) = (
            FakeEc2::new("running"),
            FakeSns::default(),
            FakeEvents::new(),
        );
        let out = run_guard(
            &test_env(),
            &ec2,
            &sns,
            &events,
            &FakeCe { amount: Some(10.0) },
            &ExplodingSsm,
            out_of_window(),
        )
        .await
        .expect("run_guard ok");
        assert_eq!(out["noop"], json!(false));
        assert_eq!(*ec2.stopped.borrow(), vec![vec!["i-tvapp".to_string()]]);
    }
}
