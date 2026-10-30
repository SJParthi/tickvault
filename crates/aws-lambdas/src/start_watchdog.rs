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
//! * mode="daily_stop"   @ 16:30 IST — the scheduled stop (operator Quote 31,
//!   2026-10-21; was an SSM stop automation): stop the box unless a fresh
//!   drain hold says the day's data is still going into the database.
//! * mode="stop_check"   @ 16:45 IST — verify the 16:30 stop worked;
//!   self-stop a box left running since before 16:30; never touch a manual
//!   evening session.
//! * mode="curfew_check" hourly — stop a forgotten out-of-hours box unless
//!   a keep-alive override / launch grace applies (operator directive
//!   2026-07-03: "manual human error is not acceptable").
//!
//! The three stop modes read the drain hold (`drain_hold`): a fresh hold
//! skips the stop silently, and a hold that ended tonight turns the stop
//! into a silent one. The hold is never honoured from 07:30 IST to 16:25 IST.
//!
//! Parity is the contract: same mode dispatch, same SNS Subject/Message
//! strings (byte-identical), same result-dict shapes, same IST math
//! (fixed UTC+5:30), same never-crash catch semantics on every AWS read.
//!
//! The AWS calls are abstracted behind tiny async traits so the unit tests
//! drive the FULL flow with fakes exactly like the legacy monkeypatched
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
/// 2026-08-08 (operator Quote 14): moved from 11:00 UTC / 16:30 IST with the
/// 9-hour window. 2026-10-22 (operator Quote 37, plan item 64): moved back to
/// 11:00 UTC / 16:30 IST. KEEP IN LOCKSTEP with main.tf `daily_stop`
/// schedule_expression and the app's
/// `shutdown_class::SCHEDULED_DAILY_STOP_SECS_OF_DAY_IST`.
pub const STOP_TRIGGER_UTC_HOUR: u32 = 11;
pub const STOP_TRIGGER_UTC_MINUTE: u32 = 0;
/// EC2 start -> running takes <1 min; anything launched more than this many
/// minutes after the trigger means the scheduled start did NOT do it.
pub const LATE_START_GRACE_MINUTES: i64 = 5;

/// Operating hours: 08:00-**16:30** IST, Mon-Fri (curfew guard inactive inside).
///
/// 2026-08-08 (operator Quote 14): close moved 17:00 -> 17:30. LOAD-BEARING:
/// the curfew guard STOPS a box running outside these hours, so a 17:00 close
/// against a 17:30 EventBridge stop would have curfew-killed the box 30 min
/// early every day. The 08:00 open is deliberately UNCHANGED — the start stays
/// 08:30, so the open already has 30 min of slack and no morning schedule moves.
///
/// 2026-10-22 (operator Quote 37, plan item 64): close moved back 17:30 ->
/// 16:30 with the daily stop. The drain hold still keeps a held box up past it.
pub const OPERATING_OPEN_IST_MINUTES: u32 = 8 * 60;
pub const OPERATING_CLOSE_IST_MINUTES: u32 = 16 * 60 + 30;

/// The daily stop as IST seconds of day (16:30 IST = 59,400), derived from
/// [`STOP_TRIGGER_UTC_HOUR`] / [`STOP_TRIGGER_UTC_MINUTE`] plus +05:30. The
/// drain hold's ignored window ends [`DAILY_STOP_JITTER_LEAD_SECS`] before it.
pub const STOP_TRIGGER_IST_SECS_OF_DAY: i64 = (STOP_TRIGGER_UTC_HOUR as i64) * 3600
    + (STOP_TRIGGER_UTC_MINUTE as i64) * 60
    + crate::time::IST_OFFSET_SECS as i64;

/// How long before the daily stop its EventBridge jitter window opens (the
/// app's `shutdown_class::SCHEDULED_STOP_WINDOW_LEAD_SECS`, 300 s).
pub const DAILY_STOP_JITTER_LEAD_SECS: i64 = 300;
/// Grace: never curfew-stop a box within this many minutes of its launch.
pub const CURFEW_START_GRACE_MINUTES: i64 = 45;

/// Legacy parity defaults for the SSM parameter names.
pub const DEFAULT_KEEP_ALIVE_PARAM: &str = "/tickvault/prod/keep-alive-until";
pub const DEFAULT_HOLIDAY_STOP_PARAM: &str = "/tickvault/prod/holiday-stop-date";

/// The fixed IST offset (+05:30). 19800s is statically in range, so the
/// fallback arm is unreachable (kept to avoid `unwrap` per the crate lints).
fn ist() -> FixedOffset {
    FixedOffset::east_opt(crate::time::IST_OFFSET_SECS).unwrap_or_else(|| Utc.fix())
}

/// Environment wiring — legacy parity: module-level `os.environ.get` reads
/// with the same defaults ("" / "" / "3001" / the two SSM param paths).
#[derive(Debug, Clone)]
pub struct WatchdogEnv {
    pub instance_id: String,
    pub alerts_topic_arn: String,
    pub dashboard_port: String,
    pub keep_alive_param: String,
    pub holiday_stop_param: String,
    /// The budget-stop latch (`budget_stop_latch`): the month a budget stop
    /// happened in. While it names this UTC month the 08:45 self-start is
    /// skipped, so the watchdog never undoes a budget stop.
    pub budget_stop_param: String,
    /// The drain hold (`drain_hold`, operator Quote 31, 2026-10-21): while
    /// the app reports the day's data is not yet in the database, the 16:30
    /// daily stop, the 16:45 stop check and the hourly curfew leave the box
    /// running.
    pub drain_hold_param: String,
    /// The rendered Muhurat sessions (`muhurat_window`, plan item J8): the
    /// `muhurat_check` mode starts and stops the box around each session,
    /// and the curfew leaves the box alone inside a session window.
    pub muhurat_sessions: String,
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
            budget_stop_param: std::env::var("BUDGET_STOP_PARAM").unwrap_or_else(|_| {
                crate::budget_stop_latch::DEFAULT_BUDGET_STOP_PARAM.to_string()
            }),
            drain_hold_param: std::env::var("DRAIN_HOLD_PARAM")
                .unwrap_or_else(|_| crate::drain_hold::DEFAULT_DRAIN_HOLD_PARAM.to_string()),
            muhurat_sessions: std::env::var(crate::muhurat_window::MUHURAT_SESSIONS_ENV)
                .unwrap_or_default(),
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
/// the legacy runtime's `.get("Value") or ""`).
#[allow(async_fn_in_trait)] // APPROVED: handler is generic over the trait (no dyn); futures awaited inline.
pub trait SsmApi {
    async fn get_parameter(&self, name: &str) -> Result<String, String>;
}

/// SNS surface. The legacy runtime does NOT catch publish exceptions — an `Err` here
/// propagates out of the handler exactly like the raised boto3 error would.
#[allow(async_fn_in_trait)] // APPROVED: handler is generic over the trait (no dyn); futures awaited inline.
pub trait SnsApi {
    async fn publish(&self, topic_arn: &str, subject: &str, message: &str) -> Result<(), String>;
}

/// Legacy `_is_within_operating_hours`: 08:00-17:00 IST on a Monday-Friday.
pub fn is_within_operating_hours(now_utc: DateTime<Utc>) -> bool {
    let ist_now = now_utc.with_timezone(&ist());
    // Sat=5, Sun=6 — weekends are always curfew.
    if chrono::Datelike::weekday(&ist_now).num_days_from_monday() >= 5 {
        return false;
    }
    let minutes = ist_now.hour() * 60 + ist_now.minute();
    (OPERATING_OPEN_IST_MINUTES..OPERATING_CLOSE_IST_MINUTES).contains(&minutes)
}

/// True when chrono parsed a leap-second time (`:60`), which it represents
/// as `nanosecond() >= 1_000_000_000`. Legacy `fromisoformat` REJECTS
/// second=60 ("ValueError: second must be in 0..59" — oracle-verified), so
/// any leap-second parse must map to None for oracle parity.
fn is_chrono_leap_second(nanos: u32) -> bool {
    nanos >= 1_000_000_000
}

/// A shape-validated, canonicalized keep-alive timestamp, produced by
/// [`normalize_keep_alive_shape`] and consumed by
/// [`parse_keep_alive_timestamp`].
enum KeepAliveShape {
    /// Bare `YYYY-MM-DD` — midnight IST (the operator's date-only paste).
    DateOnly,
    /// `YYYY-MM-DDTHH:MM:SS[.ffffff]` — naive, interpreted as IST.
    Naive(String),
    /// `YYYY-MM-DDTHH:MM:SS[.ffffff]+HH:MM` — offset-aware.
    Aware(String),
}

/// Strict shape gate + canonicalizer for the keep-alive timestamp.
///
/// This runs BEFORE any chrono call so that chrono's lenient parsing can
/// never accept a shape legacy `fromisoformat` rejects: chrono numeric
/// fields match 1..=2 digits (the legacy runtime requires exactly 2 — "…T6:00" must
/// reject), `parse_from_rfc3339` tolerates lowercase `z` (the legacy runtime rejects
/// it — review finding G1-1), and `%z` has its own leniencies. The gate
/// enforces exact digit counts and an UPPERCASE-`Z`-only designator, then
/// emits a fully-canonical string (`T` separator, minute/second expanded
/// to `:00`, fraction truncated to 6 digits, offset as `+HH:MM`) that the
/// caller parses with a single strict chrono format per shape.
///
/// Accepted grammar (legacy-accept subset, oracle-verified 2026-07-18):
/// - date: extended `YYYY-MM-DD` only
/// - separator: ANY single character (legacy 3.11+ accepts any single
///   unicode char — `t`, `x`, even a digit; all oracle-verified)
/// - time: `HH`, `HH:MM`, `HH:MM:SS`, `HH:MM:SS.f{1,}` (dot only; the
///   fraction keeps its first 6 digits — the legacy runtime truncates to microseconds)
/// - offset: none, `Z` (uppercase only), `[+-]HH`, `[+-]HH:MM`, `[+-]HHMM`
///   (offset minutes must be <= 59; hour range is value-checked by chrono)
fn normalize_keep_alive_shape(raw: &str) -> Option<KeepAliveShape> {
    fn two_digits(b: &[u8], at: usize) -> bool {
        b.len() >= at + 2 && b[at].is_ascii_digit() && b[at + 1].is_ascii_digit()
    }
    let b = raw.as_bytes();
    // Extended-format date: YYYY-MM-DD. (Basic "20260703" is a documented
    // fail-closed divergence — see `parse_keep_alive_timestamp`.)
    if b.len() < 10
        || !b[..4].iter().all(u8::is_ascii_digit)
        || b[4] != b'-'
        || !two_digits(b, 5)
        || b[7] != b'-'
        || !two_digits(b, 8)
    {
        return None;
    }
    // the legacy runtime's MINYEAR is 1 — fromisoformat rejects year 0000
    // (oracle-verified); chrono would happily parse it. Fail closed.
    if b[..4] == *b"0000" {
        return None;
    }
    if b.len() == 10 {
        return Some(KeepAliveShape::DateOnly);
    }
    // Consume ONE separator character (any single char — legacy parity;
    // multi-byte safe) and normalize it to 'T' in the canonical output.
    let time = {
        let mut chars = raw[10..].chars();
        let _sep = chars.next()?;
        chars.as_str()
    };
    let tb = time.as_bytes();
    if !two_digits(tb, 0) {
        return None;
    }
    let hour = &time[..2];
    let mut i = 2usize;
    let (mut minute, mut second) = ("00", "00");
    let mut fraction = "";
    if tb.get(i) == Some(&b':') {
        if !two_digits(tb, i + 1) {
            return None;
        }
        minute = &time[i + 1..i + 3];
        i += 3;
        if tb.get(i) == Some(&b':') {
            if !two_digits(tb, i + 1) {
                return None;
            }
            second = &time[i + 1..i + 3];
            i += 3;
            if tb.get(i) == Some(&b'.') {
                let start = i + 1;
                let mut j = start;
                while j < tb.len() && tb[j].is_ascii_digit() {
                    j += 1;
                }
                if j == start {
                    // Bare trailing dot ("06:00:00.") — the legacy runtime rejects too.
                    return None;
                }
                // The legacy runtime truncates fractional seconds to microseconds;
                // keep exactly the first 6 digits for value parity (G1-3).
                fraction = &time[start..(start + 6).min(j)];
                i = j;
            }
        }
    }
    // Offset: '' (naive), 'Z' (UPPERCASE only — the legacy runtime rejects a lowercase
    // 'z' designator in every shape, oracle-verified; G1-1), or a sign
    // followed by HH / HH:MM / HHMM. Anything else rejects the whole input.
    let offset = &time[i..];
    let canonical_offset = match offset.as_bytes() {
        [] => None,
        [b'Z'] => Some("+00:00".to_string()),
        [sign @ (b'+' | b'-'), ..] => {
            let sign = *sign as char;
            let digits = &offset[1..];
            let db = digits.as_bytes();
            let (oh, om) = if db.len() == 2 && two_digits(db, 0) {
                (&digits[..2], "00")
            } else if db.len() == 4 && db.iter().all(u8::is_ascii_digit) {
                (&digits[..2], &digits[2..4])
            } else if db.len() == 5 && db[2] == b':' && two_digits(db, 0) && two_digits(db, 3) {
                (&digits[..2], &digits[3..5])
            } else {
                return None;
            };
            // Fail-closed divergence: the legacy runtime OVERFLOWS offset minutes >= 60
            // into hours ("+05:99" -> +06:39, oracle-verified); we refuse.
            if om.parse::<u32>().ok()? >= 60 {
                return None;
            }
            Some(format!("{sign}{oh}:{om}"))
        }
        _ => return None,
    };
    let date = &raw[..10];
    let mut canon = format!("{date}T{hour}:{minute}:{second}");
    if !fraction.is_empty() {
        canon.push('.');
        canon.push_str(fraction);
    }
    Some(match canonical_offset {
        None => KeepAliveShape::Naive(canon),
        Some(off) => {
            canon.push_str(&off);
            KeepAliveShape::Aware(canon)
        }
    })
}

/// Legacy `datetime.fromisoformat(raw)` + naive-as-IST interpretation.
/// Accepts an offset-aware ISO timestamp (fraction, second, minute, or
/// hour precision), a naive `YYYY-MM-DD[<sep>]HH[:MM[:SS[.f]]]`
/// (interpreted as IST — the operator's plain local paste), or a bare
/// date. Garbage -> None (the guard stays armed — budget protection wins).
///
/// The strict shape gate ([`normalize_keep_alive_shape`]) canonicalizes
/// the input BEFORE any chrono call, so chrono leniencies (1-digit numeric
/// fields, lowercase `z` in RFC3339, `%z` quirks) can never accept a shape
/// the legacy runtime rejects. Chrono then does VALUE validation only (month 13,
/// Feb 30, hour 24, second 61, offset magnitude >= 24h all reject —
/// oracle-parity), plus the explicit leap-second (`:60`) rejection.
///
/// # Deliberate divergences from legacy fromisoformat (fail-closed)
///
/// Legacy 3.12 (the lambda runtime) ACCEPTS all of the following shapes;
/// this parser REJECTS them (None -> the curfew guard stays armed, i.e.
/// every divergence is in the budget-safe direction). Each was
/// oracle-verified against legacy 3.12 on 2026-07-18 via an exhaustive
/// differential sweep (separators x precisions x suffixes grid):
/// - comma fractional-second separator ("…06:00:00,5")
/// - ISO basic-format dates/times ("20260703", "20260703T060708",
///   "2026-07-03T060708", "20260703T06:07:08" and other mixed forms)
/// - ISO week dates ("2026-W27-5", "2026-W27-5T06:07:08")
/// - fractional hours and minutes ("…06.5", "…06:07.5" — the legacy runtime reads
///   06:30:00 / 06:07:30)
/// - offsets with a seconds component ("+05:30:00", "+053015",
///   "+05:30:00.123456")
/// - offset minutes >= 60 ("+05:99" — the legacy runtime overflows to +06:39)
/// - a space between the time and the offset ("…06:00:00 +05:30") — and in
///   fact ANY single character there ("…06:07:08xZ"): the legacy runtime reuses its
///   any-separator rule between the time and the tzinfo part
/// - an empty fraction directly before an offset ("…06:07:08.Z")
/// - fractional-second precision beyond what chrono's grammar shares with
///   the legacy runtime is NOT a divergence: both truncate — the legacy runtime to 6 digits
///   natively, this parser by keeping the first 6 fraction digits (G1-3:
///   the previous nanosecond-preserving behavior is replaced with exact
///   microsecond parity).
pub fn parse_keep_alive_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    match normalize_keep_alive_shape(raw)? {
        KeepAliveShape::DateOnly => {
            let naive = NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))?;
            ist()
                .from_local_datetime(&naive)
                .single()
                .map(|dt| dt.with_timezone(&Utc))
        }
        KeepAliveShape::Naive(canon) => {
            // Operator wrote a naive local (IST) timestamp.
            let naive = NaiveDateTime::parse_from_str(&canon, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .filter(|dt| !is_chrono_leap_second(dt.nanosecond()))?;
            ist()
                .from_local_datetime(&naive)
                .single()
                .map(|dt| dt.with_timezone(&Utc))
        }
        KeepAliveShape::Aware(canon) => {
            let dt = DateTime::parse_from_str(&canon, "%Y-%m-%dT%H:%M:%S%.f%z").ok()?;
            if is_chrono_leap_second(dt.nanosecond()) {
                return None;
            }
            Some(dt.with_timezone(&Utc))
        }
    }
}

/// Legacy `(dt + IST_OFFSET).strftime("%I:%M %p").lstrip("0")` — 12-hour IST
/// clock with the leading zero stripped ("8:43 AM", "11:51 PM").
pub fn format_ist_12h(t: DateTime<Utc>) -> String {
    let s = t.with_timezone(&ist()).format("%I:%M %p").to_string();
    match s.strip_prefix('0') {
        Some(stripped) => stripped.to_string(),
        None => s,
    }
}

/// Legacy `now.replace(hour=h, minute=m, second=0, microsecond=0)`.
fn at_utc_time(now: DateTime<Utc>, hour: u32, minute: u32) -> DateTime<Utc> {
    now.with_hour(hour)
        .and_then(|d| d.with_minute(minute))
        .and_then(|d| d.with_second(0))
        .and_then(|d| d.with_nanosecond(0))
        .unwrap_or(now) // unreachable: 0<=hour<24, 0<=minute<60
}

/// Legacy `_instance_info` — ('unknown', None) on any error; never raises.
pub async fn instance_info<E: Ec2Api>(
    ec2: &E,
    instance_id: &str,
) -> (String, Option<DateTime<Utc>>) {
    match ec2.describe(instance_id).await {
        Err(exc) => {
            error!(code = "LAMBDA-AWS-01", error = %exc, "describe_instances failed");
            ("unknown".to_string(), None)
        }
        Ok(None) => ("not-found".to_string(), None),
        Ok(Some(desc)) => (desc.state, desc.launch_time),
    }
}

/// Legacy `_instance_public_ip` — None on any error / missing IP.
pub async fn instance_public_ip<E: Ec2Api>(ec2: &E, instance_id: &str) -> Option<String> {
    match ec2.describe(instance_id).await {
        Err(exc) => {
            error!(code = "LAMBDA-AWS-01", error = %exc, "describe_instances for public IP failed");
            None
        }
        Ok(desc) => desc.and_then(|d| d.public_ip).filter(|ip| !ip.is_empty()),
    }
}

/// Why a `StartInstances` call was refused.
///
/// This distinction is load-bearing, not cosmetic. Until 2026-08-10 EVERY start
/// failure produced the same page telling the operator to press "Start instance"
/// — advice that is CORRECT for a transient/permissions failure and USELESS for
/// a capacity refusal, because a stopped instance can only restart in its OWN
/// availability zone, so the manual press fails for exactly the same reason.
///
/// This is not hypothetical: on 2026-08-06 → 08 the box sat at 0h CPU for three
/// days on `InsufficientInstanceCapacity` in ap-south-1a, and on 2026-07-20 nine
/// consecutive attempts (auto + watchdog + manual) all failed the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartFailureKind {
    /// AWS has no capacity for this instance type in the instance's zone.
    /// Retrying — by cron or by hand — cannot succeed; only a zone move can.
    CapacityRefusal,
    /// Anything else (permissions, throttling, state conflict, network).
    /// A retry is legitimate here.
    Other,
}

/// Outcome of a self-heal start attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartAttempt {
    Started,
    Failed(StartFailureKind),
}

impl StartAttempt {
    /// True iff the start call was accepted by AWS.
    #[must_use]
    pub fn started(self) -> bool {
        matches!(self, StartAttempt::Started)
    }
}

/// Classify a `StartInstances` error string. Pure — no I/O, no allocation
/// beyond the lowercase copy, and total: every input maps to a variant.
///
/// Matching is case-insensitive substring, because the AWS SDK wraps the API
/// error code in varying envelope text across SDK versions and error shapes.
/// The code string itself (`InsufficientInstanceCapacity`) is stable API
/// surface, so the substring is the durable thing to match on.
#[must_use]
pub fn classify_start_failure(error_text: &str) -> StartFailureKind {
    if error_text
        .to_ascii_lowercase()
        .contains("insufficientinstancecapacity")
    {
        StartFailureKind::CapacityRefusal
    } else {
        StartFailureKind::Other
    }
}

/// Legacy `_try_self_start` — never raises; classifies the refusal so the
/// caller can page with advice that will actually work.
pub async fn try_self_start<E: Ec2Api>(ec2: &E, instance_id: &str) -> StartAttempt {
    match ec2.start_instances(instance_id).await {
        Ok(()) => {
            info!(instance = instance_id, "self-heal start_instances issued");
            StartAttempt::Started
        }
        Err(exc) => {
            let kind = classify_start_failure(&exc);
            match kind {
                StartFailureKind::CapacityRefusal => error!(
                    code = "LAMBDA-START-02",
                    error = %exc,
                    "self-heal start_instances REFUSED — AWS has no capacity for this \
                     instance type in the instance's availability zone. Retrying cannot \
                     succeed: a stopped instance can only restart in its own zone. The \
                     remedy is a zone move (terraform var availability_zone; subnets \
                     already exist in a/b/c), which REPLACES the instance — the root \
                     volume is zone-locked and does not follow, so snapshot first."
                ),
                StartFailureKind::Other => {
                    error!(code = "LAMBDA-START-02", error = %exc, "self-heal start_instances FAILED");
                }
            }
            StartAttempt::Failed(kind)
        }
    }
}

/// Legacy `_try_self_stop` — true iff the call was accepted; never raises.
pub async fn try_self_stop<E: Ec2Api>(ec2: &E, instance_id: &str) -> bool {
    match ec2.stop_instances(instance_id).await {
        Ok(()) => {
            info!(instance = instance_id, "self-heal stop_instances issued");
            true
        }
        Err(exc) => {
            error!(code = "LAMBDA-START-02", error = %exc, "self-heal stop_instances FAILED");
            false
        }
    }
}

/// Legacy `_read_keep_alive_until` — None = no (usable) override. Never
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

/// Legacy `_holiday_stop_is_today` — true iff the holiday gate stamped
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

/// `true` when the budget-stop latch names the current UTC billing month.
/// FAIL-OPEN: an unreadable or missing latch keeps the self-start; the
/// hourly hard-stop guard still stops a breached box from the month-to-date
/// spend (AWS Budgets since 2026-10-22, plan item 63).
pub async fn budget_stop_is_this_month<S: SsmApi>(
    ssm: &S,
    param: &str,
    now_utc: DateTime<Utc>,
) -> bool {
    let raw = match ssm.get_parameter(param).await {
        Ok(raw) => Some(raw),
        Err(exc) => {
            info!(error = %exc, "budget-stop latch unavailable — treating as not latched");
            None
        }
    };
    crate::budget_stop_latch::budget_stop_is_latched(raw.as_deref(), now_utc)
}

/// Read the drain hold (operator Quote 31). A failed read is no hold: the
/// stop paths then behave exactly as before (the cost-safe direction).
pub async fn read_drain_hold<S: SsmApi>(
    ssm: &S,
    param: &str,
    now_utc: DateTime<Utc>,
) -> crate::drain_hold::DrainHold {
    let raw = match ssm.get_parameter(param).await {
        Ok(raw) => Some(raw),
        Err(exc) => {
            info!(error = %exc, "drain hold unavailable — treating as no hold");
            None
        }
    };
    crate::drain_hold::drain_hold_verdict(raw.as_deref(), now_utc.timestamp())
}

/// Legacy `_publish` — skips (with a warning) when the topic ARN is unset.
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

/// A box launched this long before the Muhurat box window opened is not one
/// `muhurat_check` started, so `muhurat_check` never stops it (the curfew's
/// own rules then apply).
pub const MUHURAT_OWN_START_SLACK_MINUTES: i64 = 15;

/// What `muhurat_check` does with a box after the Muhurat window, given what
/// it read. Pure, so every branch is tested without fakes.
#[must_use]
pub fn muhurat_after_window_should_stop(
    state: &str,
    launch_time: Option<DateTime<Utc>>,
    box_start: DateTime<Utc>,
    keep_alive_active: bool,
    drain_hold_active: bool,
) -> bool {
    if state != "running" || keep_alive_active || drain_hold_active {
        return false;
    }
    // Only a box launched inside (or just before) this window, i.e. the one
    // this mode started. Unknown launch time: leave it to the curfew.
    launch_time
        .is_some_and(|lt| lt >= box_start - Duration::minutes(MUHURAT_OWN_START_SLACK_MINUTES))
}

/// `mode = "muhurat_check"` (plan item J8): fired every 15 minutes on each
/// configured Muhurat date by a dated EventBridge rule. Starts the box inside
/// the session window, stops the box it started once the window has passed,
/// and fails closed (coded error, no start) when the session times are not
/// set. Honours the budget-stop latch, the holiday-stop marker, the
/// keep-alive override and the drain hold.
///
/// **⚠ CHANGED 2026-10-26 (audit PR L4):** a Muhurat date whose times are unset
/// or malformed now returns `Err` after its coded `LAMBDA-CONFIG-01` line, so
/// the invocation fails and the Lambda's existing Errors alarm fires. Before,
/// it returned `Ok`, the box was not started, and nothing paged: a missed
/// Muhurat session was silent. The box is still never started on that path.
async fn run_muhurat_check<E: Ec2Api, S: SsmApi, N: SnsApi>(
    env: &WatchdogEnv,
    ec2: &E,
    ssm: &S,
    sns: &N,
    now: DateTime<Utc>,
) -> Result<Value, String> {
    use crate::muhurat_window::{MuhuratPhase, muhurat_phase, parse_muhurat_sessions};
    let schedule = parse_muhurat_sessions(&env.muhurat_sessions);
    match muhurat_phase(now, &schedule) {
        MuhuratPhase::NotToday => {
            info!("muhurat_check — no Muhurat session today; nothing to do");
            Ok(json!({"mode": "muhurat_check", "phase": "not_today", "started": false}))
        }
        MuhuratPhase::TimingUnset { date } => {
            error!(
                code = "LAMBDA-CONFIG-01",
                %date,
                "muhurat_check — today is a Muhurat date but its NSE session times are \
                 not set (terraform var muhurat_sessions); the box is NOT started. Fill \
                 the times from NSE's Muhurat circular and apply terraform."
            );
            Err(format!(
                "muhurat_check phase=timing_unset date={date}: Muhurat session times not set; box NOT started"
            ))
        }
        MuhuratPhase::Malformed { entries } => {
            error!(
                code = "LAMBDA-CONFIG-01",
                ?entries,
                "muhurat_check — MUHURAT_SESSIONS has unreadable entries and none names \
                 today with valid times; the box is NOT started."
            );
            Err(format!(
                "muhurat_check phase=malformed entries={entries:?}: no valid Muhurat session for today; box NOT started"
            ))
        }
        MuhuratPhase::BeforeWindow { box_start } => {
            info!(%box_start, "muhurat_check — before today's Muhurat box window");
            Ok(json!({"mode": "muhurat_check", "phase": "before", "started": false}))
        }
        MuhuratPhase::InWindow { box_start, .. } => {
            let (state, _) = instance_info(ec2, &env.instance_id).await;
            if matches!(state.as_str(), "running" | "pending" | "stopping") {
                info!(state = %state, "muhurat_check — box already up (or stopping); nothing to do");
                return Ok(json!({
                    "mode": "muhurat_check", "phase": "in_window", "state": state,
                    "started": false
                }));
            }
            if budget_stop_is_this_month(ssm, &env.budget_stop_param, now).await {
                info!("muhurat_check — budget-stop latch names this month; not starting");
                return Ok(json!({
                    "mode": "muhurat_check", "phase": "in_window", "state": state,
                    "started": false, "skipped": "budget_stop"
                }));
            }
            if holiday_stop_is_today(ssm, &env.holiday_stop_param, now).await {
                info!("muhurat_check — holiday-stop marker is today; not starting");
                return Ok(json!({
                    "mode": "muhurat_check", "phase": "in_window", "state": state,
                    "started": false, "skipped": "holiday_stop"
                }));
            }
            let attempt = try_self_start(ec2, &env.instance_id).await;
            let started = attempt.started();
            if started {
                info!(%box_start, "muhurat_check — box start issued for today's Muhurat session");
            } else if now - box_start < Duration::minutes(15) {
                // One page, at the first attempt only: later 15-minute
                // retries log (LAMBDA-START-02) without paging again.
                publish(
                    sns,
                    env,
                    "Muhurat auto-start FAILED — box did not start",
                    "🆘 Today is an NSE Muhurat trading day and the box did NOT start \
for the special session. I will retry every 15 minutes until the session \
ends. If it keeps failing, press 'Start instance' on the portal.",
                )
                .await?;
            }
            Ok(json!({
                "mode": "muhurat_check", "phase": "in_window", "state": state,
                "started": started
            }))
        }
        MuhuratPhase::AfterWindow { box_start, .. } => {
            let (state, launch_time) = instance_info(ec2, &env.instance_id).await;
            if state != "running" {
                return Ok(json!({
                    "mode": "muhurat_check", "phase": "after", "state": state,
                    "stopped": false
                }));
            }
            let keep_alive_active = read_keep_alive_until(ssm, &env.keep_alive_param)
                .await
                .is_some_and(|until| now < until);
            let drain_hold_active = matches!(
                read_drain_hold(ssm, &env.drain_hold_param, now).await,
                crate::drain_hold::DrainHold::Active { .. }
            );
            if !muhurat_after_window_should_stop(
                &state,
                launch_time,
                box_start,
                keep_alive_active,
                drain_hold_active,
            ) {
                info!(
                    keep_alive_active,
                    drain_hold_active,
                    "muhurat_check — after the window; box left running (held, kept alive, \
                     or not started by this mode)"
                );
                return Ok(json!({
                    "mode": "muhurat_check", "phase": "after", "state": state,
                    "stopped": false
                }));
            }
            let stopped = try_self_stop(ec2, &env.instance_id).await;
            info!(
                stopped,
                "muhurat_check — Muhurat window over; box stop issued"
            );
            Ok(json!({
                "mode": "muhurat_check", "phase": "after", "state": state,
                "stopped": stopped
            }))
        }
    }
}

/// Legacy `lambda_handler` — the full mode dispatch, driven through the
/// injectable AWS surfaces so tests exercise the exact flow.
pub async fn run_watchdog<E: Ec2Api, S: SsmApi, N: SnsApi>(
    event: &Value,
    env: &WatchdogEnv,
    ec2: &E,
    ssm: &S,
    sns: &N,
    now: DateTime<Utc>,
) -> Result<Value, String> {
    // legacy: `(event or {}).get("mode", "check")` — a missing/non-string
    // mode falls through to the default "check" branch.
    let mode = event.get("mode").and_then(Value::as_str).unwrap_or("check");
    info!(mode, instance = %env.instance_id, "start-watchdog invoked");

    if mode == "muhurat_check" {
        return run_muhurat_check(env, ec2, ssm, sns, now).await;
    }

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

    if mode == "daily_stop" {
        // @ 16:30 IST (operator Quote 31, 2026-10-21): the scheduled stop.
        // Until then EventBridge fired the SSM stop automation directly,
        // which could not see the drain hold. Now: a fresh hold keeps the box
        // running until the app reports the day's data is in the database;
        // otherwise stop it, with no page (the old automation sent none). A
        // failed stop is left to the 16:45 stop_check, which pages. An
        // unknown state (describe failed) still attempts the stop: a stop is
        // idempotent, and the old SSM automation retried the same way.
        // 2026-10-26 (audit PR L4): a configured Muhurat window that covers
        // 16:30 IST keeps the box up, as the curfew and the hard-stop guard
        // already do; before, the scheduled stop ended a weekday session.
        if crate::muhurat_window::in_muhurat_up_window_raw(now, &env.muhurat_sessions) {
            info!("daily_stop — inside a Muhurat session window; box kept running");
            return Ok(json!({"mode": "daily_stop", "stopped": false, "skipped": "muhurat"}));
        }
        let (state, _) = instance_info(ec2, &env.instance_id).await;
        if state != "running" && state != "unknown" {
            info!(state = %state, "daily_stop — instance not running; nothing to do");
            return Ok(json!({"mode": "daily_stop", "state": state, "stopped": false}));
        }
        if let crate::drain_hold::DrainHold::Active { until_unix } =
            read_drain_hold(ssm, &env.drain_hold_param, now).await
        {
            info!(
                until_unix,
                "daily_stop — drain hold active, box kept running"
            );
            return Ok(json!({
                "mode": "daily_stop", "state": state, "stopped": false,
                "skipped": "drain_hold"
            }));
        }
        let stopped = try_self_stop(ec2, &env.instance_id).await;
        if !stopped {
            error!(
                code = "LAMBDA-START-02",
                "daily_stop — stop attempt FAILED; the 16:45 stop_check will retry and page"
            );
        }
        return Ok(json!({"mode": "daily_stop", "state": state, "stopped": stopped}));
    }

    if mode == "stop_check" {
        // @ 16:45 IST: verify the 16:30 daily_stop actually stopped the box.
        // Self-heal rule: stop ONLY a box launched BEFORE today's 16:30
        // trigger; a box launched AFTER it is a deliberate manual evening
        // session and is NEVER touched.
        // 2026-10-26 (audit PR L4): inside a Muhurat window the box is up on
        // purpose (daily_stop skipped it); never stop or page.
        if crate::muhurat_window::in_muhurat_up_window_raw(now, &env.muhurat_sessions) {
            info!("stop_check — inside a Muhurat session window; box deliberately running");
            return Ok(json!({"mode": "stop_check", "alerted": false, "skipped": "muhurat"}));
        }
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
        // Operator Quote 31 (2026-10-21): read the drain hold before any
        // stop or page. A fresh hold means the 16:30 stop was deliberately
        // skipped while the day's data drains — silent.
        let drain_hold = read_drain_hold(ssm, &env.drain_hold_param, now).await;
        if let crate::drain_hold::DrainHold::Active { until_unix } = drain_hold {
            info!(
                until_unix,
                "stop_check — drain hold active; box deliberately still running"
            );
            return Ok(json!({
                "mode": "stop_check", "state": state, "alerted": false,
                "skipped": "drain_hold"
            }));
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
            error!(
                code = "LAMBDA-START-01",
                "stop_check FAILED — running, launch_time unknown — paged"
            );
            return Ok(json!({
                "mode": "stop_check", "state": state, "alerted": true, "self_stopped": false
            }));
        };
        let self_stopped = try_self_stop(ec2, &env.instance_id).await;
        // The box was held tonight, was already running when the hold was
        // written, and the hold has ended: a SUCCESSFUL stop here is the
        // expected end of the drain, so no page. A failed stop still pages.
        if self_stopped && drain_hold.silences_stop_of(launch_time.map(|t| t.timestamp())) {
            info!("stop_check — drain hold ended; stopped without a page");
            return Ok(json!({
                "mode": "stop_check", "state": state, "alerted": false,
                "self_stopped": true, "drain_hold": "ended"
            }));
        }
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
4:30 PM stop failed: AWS console → CloudWatch → Log groups → \
/aws/lambda/tv-prod-start-watchdog (the daily_stop run at 4:30 PM).",
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
            code = "LAMBDA-START-01",
            self_stopped, "stop_check FAILED — instance running past stop — paged"
        );
        return Ok(json!({
            "mode": "stop_check", "state": state, "alerted": true, "self_stopped": self_stopped
        }));
    }

    if mode == "curfew_check" {
        // Hourly, active OUTSIDE 08:00-17:00 IST Mon-Fri (and all weekend).
        // Plan item J8: a configured Muhurat session window counts as
        // operating hours, so the curfew never stops the box mid-session.
        if is_within_operating_hours(now)
            || crate::muhurat_window::in_muhurat_up_window_raw(now, &env.muhurat_sessions)
        {
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
        // Operator Quote 31 (2026-10-21): a fresh drain hold keeps the box
        // running while the day's data drains (never from 07:30 IST, see
        // `drain_hold`), exactly like the keep-alive above.
        let drain_hold = read_drain_hold(ssm, &env.drain_hold_param, now).await;
        if let crate::drain_hold::DrainHold::Active { until_unix } = drain_hold {
            info!(
                until_unix,
                "curfew_check — drain hold active; leaving the box alone"
            );
            return Ok(json!({
                "mode": "curfew_check", "state": state, "stopped": false,
                "skipped": "drain_hold"
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
            error!(
                code = "LAMBDA-START-01",
                "curfew_check — running, launch_time unknown — paged, not stopped"
            );
            return Ok(json!({
                "mode": "curfew_check", "state": state, "stopped": false, "alerted": true
            }));
        };
        let self_stopped = try_self_stop(ec2, &env.instance_id).await;
        // The box was held tonight, was already running when the hold was
        // written, and the hold has ended (or 07:30 IST passed): a SUCCESSFUL
        // stop here is the expected end of the drain, so no page. A failed
        // stop still takes the manual-stop page below.
        if self_stopped && drain_hold.silences_stop_of(launch_time.map(|t| t.timestamp())) {
            info!(
                now_ist = %now_ist,
                "curfew_check — drain hold ended; stopped without a page"
            );
            return Ok(json!({
                "mode": "curfew_check", "state": state, "stopped": true,
                "alerted": false, "drain_hold": "ended"
            }));
        }
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
            error!(
                code = "LAMBDA-START-02",
                "curfew_check — stop attempt FAILED — manual stop paged"
            );
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
            error!(code = "LAMBDA-START-01", launch_time = %lt, "check — running but launched LATE — paged");
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
            "check — box is stopped but the holiday-stop marker is TODAY (NSE-holiday self-stop by the holiday gate); staying silent"
        );
        return Ok(json!({
            "mode": "check", "state": state, "alerted": false, "self_started": false,
            "skipped": "holiday_stop"
        }));
    }

    // Budget stop (audit PR30): the hourly guard or the AWS-Budgets
    // kill-switch stopped the box this billing month and latched it.
    // Starting it would only run until the guard stops it again, paging
    // both ways every morning. So no self-start, and one short note each
    // trading morning instead of the Critical "box down" page, so a stopped
    // box on a trading day is never silent.
    if budget_stop_is_this_month(ssm, &env.budget_stop_param, now).await {
        info!(
            state = %state,
            "check — box is stopped and the budget-stop latch names this month; not self-starting"
        );
        let month = crate::budget_stop_latch::billing_month_utc(now);
        let message = format!(
            "⏸ *Box is off today: the monthly budget stop for {month} holds*\n\
The box stays off until the next billing month. Nothing to do unless you want it \
back sooner: delete the parameter {param} and enable the daily-start rule.\n",
            param = env.budget_stop_param,
        );
        let noted = match publish(
            sns,
            env,
            "[BUDGET] box off today — budget stop holds",
            &message,
        )
        .await
        {
            Ok(()) => true,
            Err(e) => {
                warn!(error = %e, "could not send the budget-stop morning note");
                false
            }
        };
        return Ok(json!({
            "mode": "check", "state": state, "alerted": noted, "self_started": false,
            "skipped": "budget_stop"
        }));
    }

    // Fix first, page second.
    let attempt = try_self_start(ec2, &env.instance_id).await;

    // A capacity refusal gets its OWN page. The generic "press Start instance"
    // advice below is actively WRONG here: a stopped instance can only restart
    // in its own availability zone, so the manual press fails identically. The
    // operator needs the zone-move remedy, and needs to know the disk does not
    // follow. Evidence: 2026-08-06 -> 08, three days at 0h CPU; 2026-07-20,
    // nine consecutive refusals across auto, watchdog and manual attempts.
    if attempt == StartAttempt::Failed(StartFailureKind::CapacityRefusal) {
        publish(
            sns,
            env,
            "08:30 auto-start REFUSED — AWS has no machines free in our zone",
            &format!(
                "🆘 The box did NOT start, and this one is NOT fixable by pressing \
'Start instance' — that will fail the same way. Amazon currently has no \
machine of our size free in the zone our box lives in, and a stopped box \
can only restart in its own zone. Market opens 9:15 AM.\n\n\
What actually fixes it: move the box to another zone (we already have \
space reserved in all three). Be aware this REPLACES the machine and the \
disk does NOT move with it — take a snapshot first.\n\n\
Box: {}. This has happened before: three full days lost in August, and \
nine failed attempts in a row in July. Retrying is not the answer.",
                env.instance_id
            ),
        )
        .await?;
        return Ok(json!({
            "action": "start_refused_no_capacity",
            "instance": env.instance_id,
            "remedy": "move_availability_zone",
        }));
    }

    if attempt.started() {
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
    let self_started = attempt.started();
    error!(code = "LAMBDA-START-01", state = %state, self_started, "check FAILED — operator paged");
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
    /// `pub(crate)` since 2026-10-20 so the deploy-watchdog reads the same
    /// holiday-stop marker through [`holiday_stop_is_today`] instead of a copy.
    pub(crate) client: aws_sdk_ssm::Client,
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
        // legacy: `resp.get("Parameter", {}).get("Value") or ""`.
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
        start_error_text: String,
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
                start_error_text: "AccessDenied".to_string(),
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
        /// Refuse the start with a specific AWS error text, so tests can drive
        /// the capacity-refusal branch with the real API error code.
        fn start_raises_with(mut self, error_text: &str) -> Self {
            self.start_raises = true;
            self.start_error_text = error_text.to_string();
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
                return Err(self.start_error_text.clone());
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

    /// SSM fake keyed on the parameter NAME (2026-10-21), so the keep-alive,
    /// holiday, budget-stop and drain-hold parameters are distinct: a value
    /// set for one is never what another read receives.
    struct FakeSsm {
        values: std::collections::HashMap<String, String>,
        raises: bool,
    }

    impl FakeSsm {
        /// No parameter exists (every read is ParameterNotFound).
        fn empty() -> Self {
            Self {
                values: std::collections::HashMap::new(),
                raises: false,
            }
        }
        fn raising() -> Self {
            Self {
                raises: true,
                ..Self::empty()
            }
        }
        fn set(mut self, name: &str, value: &str) -> Self {
            self.values.insert(name.to_string(), value.to_string());
            self
        }
        fn keep_alive(self, value: &str) -> Self {
            self.set(DEFAULT_KEEP_ALIVE_PARAM, value)
        }
        fn holiday(self, value: &str) -> Self {
            self.set(DEFAULT_HOLIDAY_STOP_PARAM, value)
        }
        fn budget(self, value: &str) -> Self {
            self.set(crate::budget_stop_latch::DEFAULT_BUDGET_STOP_PARAM, value)
        }
        fn drain_hold(self, value: &str) -> Self {
            self.set(crate::drain_hold::DEFAULT_DRAIN_HOLD_PARAM, value)
        }
    }

    impl SsmApi for FakeSsm {
        async fn get_parameter(&self, name: &str) -> Result<String, String> {
            if self.raises {
                return Err("AccessDenied".to_string());
            }
            self.values
                .get(name)
                .cloned()
                .ok_or_else(|| "ParameterNotFound".to_string())
        }
    }

    fn test_env() -> WatchdogEnv {
        WatchdogEnv {
            instance_id: "i-0test".to_string(),
            alerts_topic_arn: "arn:aws:sns:ap-south-1:1:tv-prod-alerts".to_string(),
            dashboard_port: "3001".to_string(),
            keep_alive_param: DEFAULT_KEEP_ALIVE_PARAM.to_string(),
            holiday_stop_param: DEFAULT_HOLIDAY_STOP_PARAM.to_string(),
            budget_stop_param: crate::budget_stop_latch::DEFAULT_BUDGET_STOP_PARAM.to_string(),
            drain_hold_param: crate::drain_hold::DEFAULT_DRAIN_HOLD_PARAM.to_string(),
            muhurat_sessions: String::new(),
        }
    }

    /// The legacy `_wire` fake wired boto3.client("ssm") to the SNS fake, so
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

    #[test]
    fn test_classify_start_failure_detects_capacity_refusal() {
        // The bare API error code.
        assert_eq!(
            classify_start_failure("InsufficientInstanceCapacity"),
            StartFailureKind::CapacityRefusal
        );
        // Wrapped in SDK envelope text, which is how it actually arrives.
        assert_eq!(
            classify_start_failure(
                "service error: unhandled error: InsufficientInstanceCapacity: We currently \
                 do not have sufficient t4g.medium capacity in the Availability Zone you \
                 requested (ap-south-1a)."
            ),
            StartFailureKind::CapacityRefusal
        );
        // Case-insensitive: SDK versions differ in how they echo the code.
        assert_eq!(
            classify_start_failure("insufficientinstancecapacity"),
            StartFailureKind::CapacityRefusal
        );
    }

    #[test]
    fn test_classify_start_failure_other_errors_are_not_capacity() {
        // These are RETRYABLE — misclassifying them as capacity would send the
        // operator chasing a zone move for a permissions problem.
        for other in [
            "AccessDenied",
            "UnauthorizedOperation",
            "RequestLimitExceeded",
            "IncorrectInstanceState",
            "connection timed out",
            "",
        ] {
            assert_eq!(
                classify_start_failure(other),
                StartFailureKind::Other,
                "{other:?} must NOT be classified as a capacity refusal"
            );
        }
    }

    #[test]
    fn test_start_attempt_started_predicate() {
        assert!(StartAttempt::Started.started());
        assert!(!StartAttempt::Failed(StartFailureKind::CapacityRefusal).started());
        assert!(!StartAttempt::Failed(StartFailureKind::Other).started());
    }

    #[tokio::test]
    async fn test_check_stopped_capacity_refusal_pages_zone_move_not_press_start() {
        // Regression: 2026-08-10 — a capacity refusal used to page "press
        // 'Start instance'", which fails identically because a stopped
        // instance can only restart in its own zone. Three days of downtime
        // (2026-08-06 -> 08) were spent following that advice.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("stopped")).start_raises_with(
            "InsufficientInstanceCapacity: We currently do not have sufficient capacity \
             in the Availability Zone you requested.",
        );
        let out = run(
            json!({"mode": "check"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            in_window_now(),
        )
        .await;

        assert_eq!(out["action"], json!("start_refused_no_capacity"));
        assert_eq!(out["remedy"], json!("move_availability_zone"));

        let published = sns.published.borrow();
        assert_eq!(
            published.len(),
            1,
            "exactly one page for a capacity refusal"
        );
        let page = &published[0];
        assert!(
            page.subject.contains("no machines free"),
            "subject must name the real cause, got: {}",
            page.subject
        );
        // The whole point: it must NOT repeat the useless advice.
        assert!(
            !page.message.contains("start-instances"),
            "must NOT tell the operator to retry the start — it cannot succeed"
        );
        assert!(
            page.message.contains("zone"),
            "must name the zone move as the remedy"
        );
        assert!(
            page.message.contains("snapshot"),
            "must warn that the disk does not follow the move"
        );
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
        let ssm = FakeSsm::empty().holiday("2026-06-10");
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
        let ssm = FakeSsm::empty().holiday("2026-06-09"); // yesterday's holiday
        let out = run(json!({"mode": "check"}), &ec2, &ssm, &sns, in_window_now()).await;
        assert_eq!(out["alerted"], json!(true));
        assert_eq!(out["self_started"], json!(true));
        assert_eq!(*ec2.start_calls.borrow(), vec!["i-0test".to_string()]);
    }

    /// 2026-10-20: `holiday_stop_is_today` directly, the read the
    /// deploy-watchdog now shares (moved here from `deploy_watchdog.rs`, next
    /// to the checks above that drive it through `run`). The marker blocks
    /// only on today's IST date; an older, empty, missing or unreadable
    /// marker fails open.
    #[tokio::test]
    async fn holiday_marker_blocks_only_on_todays_ist_date() {
        // 2026-10-20 03:10 UTC = 08:40 IST on the NSE holiday.
        let now = utc(2026, 10, 20, 3, 10);
        for (ssm, blocks) in [
            (FakeSsm::empty().holiday("2026-10-20"), true),
            (FakeSsm::empty().holiday(" 2026-10-20\n"), true),
            (FakeSsm::empty().holiday("2026-10-19"), false),
            (FakeSsm::empty().holiday("2026-10-02"), false),
            (FakeSsm::empty().holiday(""), false),
            (FakeSsm::empty(), false),
            (FakeSsm::raising(), false),
        ] {
            assert_eq!(
                holiday_stop_is_today(&ssm, DEFAULT_HOLIDAY_STOP_PARAM, now).await,
                blocks,
                "marker {:?} (raises: {})",
                ssm.values,
                ssm.raises
            );
        }
    }

    // -----------------------------------------------------------------------
    // Budget-stop latch (audit PR30). FakeSsm is keyed on the parameter
    // name (2026-10-21), so "2026-06" is the latch for June and only the
    // latch read receives it.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_budget_stop_is_this_month_latch_skips_the_self_start() {
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("stopped"));
        let ssm = FakeSsm::empty().budget("2026-06");
        let out = run(json!({"mode": "check"}), &ec2, &ssm, &sns, in_window_now()).await;
        assert_eq!(out["alerted"], json!(true));
        assert_eq!(out["self_started"], json!(false));
        assert_eq!(out["skipped"], json!("budget_stop"));
        assert!(ec2.start_calls.borrow().is_empty()); // the budget stop holds
        // One short note, never the Critical "box down" page.
        let published = sns.published.borrow();
        assert_eq!(published.len(), 1);
        assert!(published[0].subject.contains("budget stop holds"));
        assert!(
            published[0]
                .message
                .contains("budget stop for 2026-06 holds")
        );
    }

    #[tokio::test]
    async fn test_budget_stop_is_this_month_last_months_latch_still_self_starts() {
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("stopped"));
        let ssm = FakeSsm::empty().budget("2026-05");
        let out = run(json!({"mode": "check"}), &ec2, &ssm, &sns, in_window_now()).await;
        assert_eq!(out["self_started"], json!(true));
        assert_eq!(*ec2.start_calls.borrow(), vec!["i-0test".to_string()]);
    }

    #[tokio::test]
    async fn test_budget_stop_is_this_month_fails_open_on_a_read_error() {
        assert!(
            !budget_stop_is_this_month(
                &FakeSsm::raising(),
                crate::budget_stop_latch::DEFAULT_BUDGET_STOP_PARAM,
                in_window_now()
            )
            .await
        );
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
    // 2026-08-08 (Quote 14): the stop moved to 12:00 UTC (17:30 IST) and the
    // stop_check to 12:15 UTC (17:45 IST). These fixtures moved with it — at
    // the old values, 11:46 UTC (17:16 IST) is now BEFORE the stop trigger, so
    // a deliberate manual evening session would have been misread as a box
    // left running since morning and stopped.
    // 2026-10-22 (Quote 37, plan item 64): back to 11:00 UTC (16:30 IST) and
    // 11:15 UTC (16:45 IST); the fixtures move with them again.
    fn stop_check_now() -> DateTime<Utc> {
        utc(2026, 6, 10, 11, 15) // 16:45 IST — the stop_check slot
    }
    fn morning_launch() -> DateTime<Utc> {
        utc(2026, 6, 10, 3, 13) // 08:43 IST — since morning
    }
    fn evening_launch() -> DateTime<Utc> {
        utc(2026, 6, 10, 11, 16) // manual 16:46 IST — AFTER the 16:30 stop
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
        // 2026-06-10 incident: the scheduled stop silently failed; box (up since
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
        // launched AFTER the stop trigger must be left alone, silently.
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
            &FakeSsm::empty(),
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
        let ssm = FakeSsm::empty().keep_alive("2026-07-03T06:00:00+05:30"); // tomorrow 6 AM IST
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
        let ssm = FakeSsm::empty().keep_alive("2026-07-02T20:00:00+05:30"); // 8 PM IST — past
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
        let ssm = FakeSsm::empty().keep_alive("2026-07-03T06:00:00"); // naive -> IST -> future
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
    fn test_parse_keep_alive_minute_precision_matches_legacy_oracle() {
        // Oracle: the legacy runtime datetime.fromisoformat (lambda runtime legacy 3.12)
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
    fn test_parse_keep_alive_hour_only_matches_legacy_312_oracle() {
        // Oracle: legacy 3.11+/3.12 fromisoformat ALSO accepts hour-only
        // times (rejected in <=3.10; the lambda runtime is legacy 3.12) —
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
            // Leap-second `:60` — legacy fromisoformat raises
            // "ValueError: second must be in 0..59" (oracle-verified);
            // chrono would otherwise accept it as nanosecond >= 1e9.
            "2026-07-03T23:59:60",
            "2026-06-30 23:59:60.5",
        ] {
            assert_eq!(parse_keep_alive_timestamp(garbage), None, "{garbage:?}");
        }
    }

    #[test]
    fn test_parse_keep_alive_accepted_parity_vectors_match_legacy_oracle() {
        // Differential-sweep ACCEPTED-PARITY set (legacy 3.12 oracle,
        // 2026-07-18): every shape below is accepted by BOTH legacy
        // fromisoformat and this parser, with identical values.
        let ist_0030_utc = utc(2026, 7, 3, 0, 30);
        for accepted in [
            // Any single separator char (legacy 3.11+ rule) — lowercase
            // 't', 'x', even a digit — all normalize to the same instant.
            "2026-07-03t06:00",
            "2026-07-03x06:00",
            "2026-07-03606:00",
        ] {
            assert_eq!(
                parse_keep_alive_timestamp(accepted),
                Some(ist_0030_utc),
                "{accepted:?}"
            );
        }
        // Hour-only offset "+05" (the legacy runtime reads +05:00).
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03T06:00:00+05"),
            Some(utc(2026, 7, 3, 1, 0))
        );
        // Basic 4-digit offset without a colon, negative sign.
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03T06:00:00-0530"),
            Some(utc(2026, 7, 3, 11, 30))
        );
        // "-0000" == UTC (legacy-accepted; RFC3339 would reject it).
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03T06:00:00-0000"),
            Some(utc(2026, 7, 3, 6, 0))
        );
        // Bare date -> midnight IST == 18:30 UTC the previous day.
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03"),
            Some(utc(2026, 7, 2, 18, 30))
        );
    }

    #[test]
    fn test_parse_keep_alive_fraction_truncates_to_microseconds_like_legacy() {
        // G1-3: the legacy runtime truncates fractional seconds to microseconds
        // (".9999999" -> .999999, ".123456789012" -> .123456 —
        // oracle-verified 2026-07-18). This parser keeps exactly the first
        // 6 fraction digits so the values match legacy EXACTLY (the
        // previous nanosecond-preserving behavior diverged sub-micro).
        let expect = ist()
            .with_ymd_and_hms(2026, 7, 3, 6, 0, 0)
            .unwrap()
            .with_nanosecond(999_999_000)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03T06:00:00.9999999"),
            Some(expect)
        );
        let expect = utc(2026, 7, 3, 6, 0).with_nanosecond(123_456_000).unwrap();
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03T06:00:00.123456789012Z"),
            Some(expect)
        );
    }

    #[test]
    fn test_parse_keep_alive_lowercase_z_rejected_like_legacy() {
        // G1-1: legacy fromisoformat REJECTS a lowercase 'z' designator in
        // EVERY shape (oracle-verified 2026-07-18) — chrono's RFC3339 arm
        // and the old ['Z', 'z'] normalization used to accept these four,
        // silently suppressing the curfew. The shape gate now refuses them
        // before any chrono call.
        for lower_z in [
            "2026-07-03T06:00:00z",
            "2026-07-03T06:00z",
            "2026-07-03T06z",
            "2026-07-03 06:00z",
            "2026-07-03T06:00:00.5z",
        ] {
            assert_eq!(parse_keep_alive_timestamp(lower_z), None, "{lower_z:?}");
        }
        // The SEPARATOR position still accepts 'z' (the legacy runtime accepts any
        // single separator char there — parity, not a designator).
        assert_eq!(
            parse_keep_alive_timestamp("2026-07-03z06:00"),
            Some(utc(2026, 7, 3, 0, 30))
        );
    }

    #[test]
    fn test_parse_keep_alive_legacy_rejected_shapes_stay_rejected() {
        // Differential-sweep UNSAFE-DIRECTION set: the legacy runtime REJECTS all of
        // these (oracle-verified 2026-07-18); accepting any of them would
        // suppress the curfew on a shape the legacy lambda never honored.
        // chrono's lenient 1..=2-digit numeric parsing (and year-0 support)
        // would accept several without the strict shape gate.
        for rejected in [
            "2026-07-03T6:00",           // 1-digit hour
            "2026-07-03T06:0",           // 1-digit minute
            "2026-7-03T06:00",           // 1-digit month
            "2026-07-3T06:00",           // 1-digit day
            "126-07-03T06:00",           // 3-digit year
            "02026-07-03T06:00",         // 5-digit year
            "0000-01-01T00:00",          // legacy MINYEAR is 1
            "2026-07-03T 06:00",         // space after the separator
            "2026-07-03T06:00:00+5:30",  // 1-digit offset hour
            "2026-07-03T06:00:00+24:00", // offset magnitude >= 24h
            "2026-07-03T06:00:00.",      // bare trailing fraction dot
        ] {
            assert_eq!(parse_keep_alive_timestamp(rejected), None, "{rejected:?}");
        }
    }

    #[test]
    fn test_parse_keep_alive_documented_failsafe_divergences_reject() {
        // Deliberate fail-closed divergences: legacy 3.12 ACCEPTS every
        // shape below (oracle-verified 2026-07-18); this parser REJECTS
        // them (None -> the curfew guard stays armed — the budget-safe
        // direction). Full list on `parse_keep_alive_timestamp`'s doc.
        for divergent in [
            "2026-07-03T06:00:00,5",        // comma fraction
            "20260703",                     // basic-format date
            "20260703T060708",              // basic date+time
            "2026-07-03T060708",            // basic time, extended date
            "20260703T06:07:08",            // extended time, basic date
            "2026-W27-5",                   // ISO week date
            "2026-W27-5T06:07:08",          // ISO week date + time
            "2026-07-03T06.5",              // fractional hour
            "2026-07-03T06:07.5",           // fractional minute
            "2026-07-03T06:00:00+05:30:00", // offset with seconds
            "2026-07-03T06:00:00+053015",   // basic offset with seconds
            "2026-07-03T06:00:00+05:99",    // offset minutes >= 60
            "2026-07-03T06:00:00 +05:30",   // space before the offset
            "2026-07-03T06:07:08xZ",        // arbitrary char before tz
            "2026-07-03T06:07:08.Z",        // empty fraction before tz
        ] {
            assert_eq!(parse_keep_alive_timestamp(divergent), None, "{divergent:?}");
        }
    }

    #[tokio::test]
    async fn test_curfew_malformed_keep_alive_treated_as_no_override() {
        // Garbage in the param must not silently disable budget protection.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(curfew_old_launch());
        let ssm = FakeSsm::empty().keep_alive("not-a-timestamp");
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
            &FakeSsm::empty(),
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
            &FakeSsm::empty(),
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
            &FakeSsm::empty(),
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
            &FakeSsm::empty(),
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
            &FakeSsm::empty(),
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
            &FakeSsm::empty(),
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
        // Pin the exact window: [08:00, 16:30) IST, Mon-Fri only.
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
        // 2026-08-08 (Quote 14): close moved 17:00 -> 17:30, so 17:00 and
        // 17:29 are now INSIDE and only 17:30 is out. The 17:00 assert is kept
        // (inverted) as the regression pin: if the close slips back, the curfew
        // guard stops the box 30 min before its scheduled 17:30 stop.
        // 2026-10-22 (Quote 37, plan item 64): close moved back to 16:30, so
        // 16:29 is the last inside minute and 16:30 and 17:00 are out.
        assert!(is_within_operating_hours(at(2026, 7, 1, 16, 29)));
        assert!(!is_within_operating_hours(at(2026, 7, 1, 16, 30)));
        assert!(!is_within_operating_hours(at(2026, 7, 1, 17, 0)));
        // Sunday midday is never operating hours.
        assert!(!is_within_operating_hours(at(2026, 7, 5, 12, 0)));
    }

    // -----------------------------------------------------------------------
    // Drain hold (operator Quote 31, 2026-10-21): the 16:30 daily_stop, the
    // 16:45 stop_check and the hourly curfew leave the box running while the
    // app holds it, and stop it WITHOUT a page once a hold has ended.
    // -----------------------------------------------------------------------

    fn hold(state: &str, until: DateTime<Utc>, written: DateTime<Utc>) -> String {
        format!(
            r#"{{"v":1,"state":"{state}","until_unix":{},"written_unix":{},"left":"12 buckets"}}"#,
            until.timestamp(),
            written.timestamp()
        )
    }

    // Wed 2026-06-10 16:30 IST — the daily_stop slot (17:30 until 2026-10-22).
    fn daily_stop_now() -> DateTime<Utc> {
        utc(2026, 6, 10, 11, 0)
    }

    #[tokio::test]
    async fn test_daily_stop_stopped_box_is_a_silent_noop() {
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("stopped"));
        let out = run(
            json!({"mode": "daily_stop"}),
            &ec2,
            &FakeSsm::empty(),
            &sns,
            daily_stop_now(),
        )
        .await;
        assert_eq!(out["mode"], json!("daily_stop"));
        assert_eq!(out["stopped"], json!(false));
        assert!(ec2.stop_calls.borrow().is_empty());
        assert!(sns.published.borrow().is_empty());
    }

    // Regression: 2026-10-26 (audit PR L4) — the 16:30 daily_stop and the
    // 16:45 stop_check did not skip a Muhurat window (only the curfew and the
    // hard-stop guard did), so a weekday Muhurat session was stopped mid-run.
    #[tokio::test]
    async fn test_regression_daily_stop_and_stop_check_skip_a_muhurat_window() {
        // Monday 9 Nov 2026, session 15:00-16:30 IST: box window 14:00-17:30.
        let raw = "2026-11-09@15:00-16:30";
        for (mode, now) in [
            ("daily_stop", utc(2026, 11, 9, 11, 0)),  // 16:30 IST
            ("stop_check", utc(2026, 11, 9, 11, 15)), // 16:45 IST
        ] {
            let sns = FakeSns::default();
            let ec2 = FakeEc2::new(Some("running")).launch(utc(2026, 11, 9, 8, 30));
            let out = run_watchdog(
                &json!({ "mode": mode }),
                &muhurat_env(raw),
                &ec2,
                &FakeSsm::empty(),
                &sns,
                now,
            )
            .await
            .expect("run_watchdog");
            assert_eq!(out["skipped"], json!("muhurat"), "{mode}");
            assert!(ec2.stop_calls.borrow().is_empty(), "{mode} must not stop");
            assert!(sns.published.borrow().is_empty(), "{mode} must not page");
        }
        // Outside the window (a later date) the same daily_stop stops.
        let ec2 = FakeEc2::new(Some("running")).launch(utc(2026, 11, 10, 3, 0));
        let out = run_watchdog(
            &json!({"mode": "daily_stop"}),
            &muhurat_env(raw),
            &ec2,
            &FakeSsm::empty(),
            &FakeSns::default(),
            utc(2026, 11, 10, 11, 0),
        )
        .await
        .expect("run_watchdog");
        assert_eq!(out["stopped"], json!(true));
    }

    #[tokio::test]
    async fn test_daily_stop_with_no_hold_stops_silently() {
        // The old SSM stop automation sent no page; neither does this.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(morning_launch());
        let out = run(
            json!({"mode": "daily_stop"}),
            &ec2,
            &FakeSsm::empty(),
            &sns,
            daily_stop_now(),
        )
        .await;
        assert_eq!(out["stopped"], json!(true));
        assert_eq!(*ec2.stop_calls.borrow(), vec!["i-0test".to_string()]);
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_daily_stop_with_an_unreadable_hold_still_stops() {
        // Fail toward stopping: an SSM error is no hold.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(morning_launch());
        let out = run(
            json!({"mode": "daily_stop"}),
            &ec2,
            &FakeSsm::raising(),
            &sns,
            daily_stop_now(),
        )
        .await;
        assert_eq!(out["stopped"], json!(true));
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_daily_stop_with_an_active_hold_keeps_the_box_running() {
        let now = daily_stop_now();
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(morning_launch());
        let ssm = FakeSsm::empty().drain_hold(&hold(
            "holding",
            now + Duration::minutes(10),
            now - Duration::minutes(1),
        ));
        let out = run(json!({"mode": "daily_stop"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["stopped"], json!(false));
        assert_eq!(out["skipped"], json!("drain_hold"));
        assert!(ec2.stop_calls.borrow().is_empty());
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_daily_stop_with_a_released_hold_stops_silently() {
        let now = daily_stop_now();
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(morning_launch());
        let ssm = FakeSsm::empty().drain_hold(&hold(
            "released",
            now - Duration::minutes(1),
            now - Duration::minutes(1),
        ));
        let out = run(json!({"mode": "daily_stop"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["stopped"], json!(true));
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_daily_stop_failure_is_left_to_the_stop_check_without_a_page() {
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running"))
            .launch(morning_launch())
            .stop_raises();
        let out = run(
            json!({"mode": "daily_stop"}),
            &ec2,
            &FakeSsm::empty(),
            &sns,
            daily_stop_now(),
        )
        .await;
        assert_eq!(out["stopped"], json!(false));
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_stop_check_active_hold_is_silent_and_never_stops() {
        let now = stop_check_now();
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(morning_launch());
        let ssm = FakeSsm::empty().drain_hold(&hold(
            "holding",
            now + Duration::minutes(20),
            now - Duration::minutes(2),
        ));
        let out = run(json!({"mode": "stop_check"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["alerted"], json!(false));
        assert_eq!(out["skipped"], json!("drain_hold"));
        assert!(ec2.stop_calls.borrow().is_empty());
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_stop_check_active_hold_also_covers_an_unknown_launch_time() {
        let now = stop_check_now();
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running"));
        let ssm = FakeSsm::empty().drain_hold(&hold("holding", now + Duration::minutes(20), now));
        let out = run(json!({"mode": "stop_check"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["skipped"], json!("drain_hold"));
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_stop_check_ended_hold_stops_without_a_page() {
        let now = stop_check_now();
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(morning_launch());
        // An expired "holding" record written minutes ago: the hold lapsed.
        let ssm = FakeSsm::empty().drain_hold(&hold(
            "holding",
            now - Duration::minutes(1),
            now - Duration::minutes(4),
        ));
        let out = run(json!({"mode": "stop_check"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["alerted"], json!(false));
        assert_eq!(out["self_stopped"], json!(true));
        assert_eq!(out["drain_hold"], json!("ended"));
        assert_eq!(*ec2.stop_calls.borrow(), vec!["i-0test".to_string()]);
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_stop_check_ended_hold_with_unknown_launch_still_pages_without_stopping() {
        // The existing never-kill-a-possible-manual-session rule still wins.
        let now = stop_check_now();
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running"));
        let ssm = FakeSsm::empty().drain_hold(&hold("released", now, now));
        let out = run(json!({"mode": "stop_check"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["alerted"], json!(true));
        assert!(ec2.stop_calls.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_stop_check_stale_hold_keeps_the_existing_page() {
        // A record from yesterday is no hold: unchanged stop + page.
        let now = stop_check_now();
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(morning_launch());
        let ssm = FakeSsm::empty().drain_hold(&hold(
            "released",
            now - Duration::hours(20),
            now - Duration::hours(20),
        ));
        let out = run(json!({"mode": "stop_check"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["alerted"], json!(true));
        assert_eq!(out["self_stopped"], json!(true));
        assert!(
            sns.published.borrow()[0]
                .subject
                .contains("stopped the box itself")
        );
    }

    #[tokio::test]
    async fn test_curfew_active_hold_leaves_the_box_alone() {
        let now = curfew_night_now();
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(curfew_old_launch());
        let ssm = FakeSsm::empty().drain_hold(&hold(
            "holding",
            now + Duration::minutes(15),
            now - Duration::minutes(3),
        ));
        let out = run(json!({"mode": "curfew_check"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["stopped"], json!(false));
        assert_eq!(out["skipped"], json!("drain_hold"));
        assert!(ec2.stop_calls.borrow().is_empty());
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_curfew_ended_hold_stops_without_a_page() {
        let now = curfew_night_now();
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(curfew_old_launch());
        let ssm = FakeSsm::empty().drain_hold(&hold(
            "released",
            now - Duration::minutes(40),
            now - Duration::minutes(40),
        ));
        let out = run(json!({"mode": "curfew_check"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["stopped"], json!(true));
        assert_eq!(out["alerted"], json!(false));
        assert_eq!(out["drain_hold"], json!("ended"));
        assert_eq!(*ec2.stop_calls.borrow(), vec!["i-0test".to_string()]);
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_curfew_at_0735_ist_stops_a_box_still_held_overnight() {
        // The hard bound: from 07:30 IST a hold is ignored, so the 07:35
        // curfew stops the box (silently, it was held tonight) and the 08:30
        // start boots it fresh.
        let now = utc(2026, 7, 3, 2, 5); // Fri 07:35 IST
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(curfew_old_launch());
        let ssm = FakeSsm::empty().drain_hold(&hold(
            "holding",
            now + Duration::minutes(30),
            now - Duration::minutes(5),
        ));
        let out = run(json!({"mode": "curfew_check"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["stopped"], json!(true));
        assert_eq!(out["alerted"], json!(false));
        assert_eq!(*ec2.stop_calls.borrow(), vec!["i-0test".to_string()]);
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_curfew_keep_alive_and_drain_hold_are_distinct_parameters() {
        // A drain-hold value is never read as the keep-alive, and a stale
        // drain hold leaves the existing curfew page in place.
        let now = curfew_night_now();
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(curfew_old_launch());
        let ssm = FakeSsm::empty()
            .keep_alive("2026-07-02T20:00:00+05:30") // expired
            .drain_hold(&hold(
                "released",
                now - Duration::days(2),
                now - Duration::days(2),
            ));
        let out = run(json!({"mode": "curfew_check"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["stopped"], json!(true));
        assert_eq!(out["alerted"], json!(true));
        assert!(
            sns.published.borrow()[0]
                .subject
                .contains("Curfew auto-stop")
        );
    }

    #[tokio::test]
    async fn test_read_drain_hold_reads_its_own_parameter_and_fails_toward_no_hold() {
        use crate::drain_hold::{DEFAULT_DRAIN_HOLD_PARAM, DrainHold};
        let now = curfew_night_now();
        let raw = hold("holding", now + Duration::minutes(5), now);
        let active = FakeSsm::empty().drain_hold(&raw);
        assert_eq!(
            read_drain_hold(&active, DEFAULT_DRAIN_HOLD_PARAM, now).await,
            DrainHold::Active {
                until_unix: (now + Duration::minutes(5)).timestamp()
            }
        );
        // The same record under ANOTHER name is not the drain hold.
        let elsewhere = FakeSsm::empty().keep_alive(&raw);
        assert_eq!(
            read_drain_hold(&elsewhere, DEFAULT_DRAIN_HOLD_PARAM, now).await,
            DrainHold::None
        );
        assert_eq!(
            read_drain_hold(&FakeSsm::raising(), DEFAULT_DRAIN_HOLD_PARAM, now).await,
            DrainHold::None
        );
    }

    // --- Review fixes (2026-10-21): silence only a SUCCESSFUL stop of the box
    // that was running when the hold was written; daily_stop on an unknown
    // state still attempts the stop.

    #[tokio::test]
    async fn test_stop_check_ended_hold_failed_stop_still_pages_manual() {
        let now = stop_check_now();
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running"))
            .launch(morning_launch())
            .stop_raises();
        let ssm = FakeSsm::empty().drain_hold(&hold(
            "released",
            now - Duration::minutes(2),
            now - Duration::minutes(2),
        ));
        let out = run(json!({"mode": "stop_check"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["alerted"], json!(true));
        assert_eq!(out["self_stopped"], json!(false));
        assert!(
            sns.published.borrow()[0]
                .subject
                .contains("manual stop NEEDED")
        );
    }

    #[tokio::test]
    async fn test_stop_check_ended_hold_does_not_silence_a_later_session() {
        // Launched 16:20 IST, after the 16:10 IST hold write: another session.
        let now = stop_check_now();
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running")).launch(utc(2026, 6, 10, 10, 50));
        let ssm = FakeSsm::empty().drain_hold(&hold(
            "released",
            utc(2026, 6, 10, 10, 40),
            utc(2026, 6, 10, 10, 40),
        ));
        let out = run(json!({"mode": "stop_check"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["alerted"], json!(true));
        assert_eq!(out["self_stopped"], json!(true));
        assert!(
            sns.published.borrow()[0]
                .subject
                .contains("stopped the box itself")
        );
    }

    #[tokio::test]
    async fn test_curfew_ended_hold_failed_stop_still_pages_manual() {
        let now = curfew_night_now();
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("running"))
            .launch(curfew_old_launch())
            .stop_raises();
        let ssm = FakeSsm::empty().drain_hold(&hold(
            "released",
            now - Duration::minutes(40),
            now - Duration::minutes(40),
        ));
        let out = run(json!({"mode": "curfew_check"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["stopped"], json!(false));
        assert_eq!(out["alerted"], json!(true));
        assert!(
            sns.published.borrow()[0]
                .subject
                .contains("manual stop NEEDED")
        );
    }

    #[tokio::test]
    async fn test_curfew_ended_hold_does_not_silence_a_later_session() {
        // Box launched 50 min ago (past the 45-min grace), 5 min AFTER the
        // hold record was written: a different session, so it pages.
        let now = curfew_night_now();
        let sns = FakeSns::default();
        let launch = now - Duration::minutes(50);
        let ec2 = FakeEc2::new(Some("running")).launch(launch);
        let written = launch - Duration::minutes(5);
        let ssm = FakeSsm::empty().drain_hold(&hold("released", written, written));
        let out = run(json!({"mode": "curfew_check"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["stopped"], json!(true));
        assert_eq!(out["alerted"], json!(true));
        assert!(
            sns.published.borrow()[0]
                .subject
                .contains("Curfew auto-stop")
        );
    }

    #[tokio::test]
    async fn test_daily_stop_unknown_state_still_attempts_the_stop() {
        // describe failed: the stop is idempotent, so attempt it.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(None);
        let out = run(
            json!({"mode": "daily_stop"}),
            &ec2,
            &FakeSsm::empty(),
            &sns,
            daily_stop_now(),
        )
        .await;
        assert_eq!(out["state"], json!("unknown"));
        assert_eq!(out["stopped"], json!(true));
        assert_eq!(*ec2.stop_calls.borrow(), vec!["i-0test".to_string()]);
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_daily_stop_unknown_state_with_an_active_hold_is_skipped() {
        let now = daily_stop_now();
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(None);
        let ssm = FakeSsm::empty().drain_hold(&hold("holding", now + Duration::minutes(10), now));
        let out = run(json!({"mode": "daily_stop"}), &ec2, &ssm, &sns, now).await;
        assert_eq!(out["skipped"], json!("drain_hold"));
        assert!(ec2.stop_calls.borrow().is_empty());
    }

    // ---- plan item J8: Muhurat session start/stop (muhurat_check) --------

    /// 2026-11-08 (Sunday) 14:05-15:30 IST: box window 13:05-16:30 IST =
    /// 07:35-11:00 UTC.
    const MUHURAT: &str = "2026-11-08@14:05-15:30";

    fn muhurat_env(raw: &str) -> WatchdogEnv {
        WatchdogEnv {
            muhurat_sessions: raw.to_string(),
            ..test_env()
        }
    }

    async fn run_muhurat(
        raw: &str,
        ec2: &FakeEc2,
        ssm: &FakeSsm,
        sns: &FakeSns,
        now: DateTime<Utc>,
    ) -> Value {
        run_watchdog(
            &json!({"mode": "muhurat_check"}),
            &muhurat_env(raw),
            ec2,
            ssm,
            sns,
            now,
        )
        .await
        .expect("run_watchdog")
    }

    #[tokio::test]
    async fn test_muhurat_check_starts_a_stopped_box_inside_the_window() {
        let (ec2, sns) = (FakeEc2::new(Some("stopped")), FakeSns::default());
        let out = run_muhurat(
            MUHURAT,
            &ec2,
            &FakeSsm::empty(),
            &sns,
            utc(2026, 11, 8, 7, 45),
        )
        .await;
        assert_eq!(out["phase"], json!("in_window"));
        assert_eq!(out["started"], json!(true));
        assert_eq!(*ec2.start_calls.borrow(), vec!["i-0test".to_string()]);
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_muhurat_check_leaves_a_running_box_alone() {
        let ec2 = FakeEc2::new(Some("running"));
        let out = run_muhurat(
            MUHURAT,
            &ec2,
            &FakeSsm::empty(),
            &FakeSns::default(),
            utc(2026, 11, 8, 8, 0),
        )
        .await;
        assert_eq!(out["started"], json!(false));
        assert!(ec2.start_calls.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_muhurat_check_before_window_and_other_day_never_start() {
        for (raw, now, phase) in [
            (MUHURAT, utc(2026, 11, 8, 7, 30), "before"), // 13:00 IST
            (MUHURAT, utc(2026, 11, 9, 8, 0), "not_today"),
            ("", utc(2026, 11, 8, 8, 0), "not_today"),
        ] {
            let ec2 = FakeEc2::new(Some("stopped"));
            let out = run_muhurat(raw, &ec2, &FakeSsm::empty(), &FakeSns::default(), now).await;
            assert_eq!(out["phase"], json!(phase), "{raw} {now}");
            assert!(ec2.start_calls.borrow().is_empty(), "{raw} {now}");
        }
    }

    // Regression: 2026-10-26 (audit PR L4) — an unset or malformed Muhurat
    // session returned Ok, so the box stayed off and no alarm fired. It now
    // returns Err (the Lambda Errors alarm pages) and still never starts.
    #[tokio::test]
    async fn test_regression_muhurat_config_error_returns_err_and_never_starts() {
        for (raw, phase) in [
            ("2026-11-08@-", "timing_unset"),
            ("2026-11-08@15:30-14:05", "malformed"),
        ] {
            let ec2 = FakeEc2::new(Some("stopped"));
            let out = run_watchdog(
                &json!({"mode": "muhurat_check"}),
                &muhurat_env(raw),
                &ec2,
                &FakeSsm::empty(),
                &FakeSns::default(),
                utc(2026, 11, 8, 8, 0),
            )
            .await;
            let err = out.expect_err("a Muhurat config error must fail the invocation");
            assert!(err.contains(phase), "{raw}: {err}");
            assert!(ec2.start_calls.borrow().is_empty(), "{raw}");
        }
    }

    #[tokio::test]
    async fn test_muhurat_check_respects_the_budget_stop_and_holiday_marker() {
        let now = utc(2026, 11, 8, 8, 0);
        let budget = FakeSsm::empty().budget("2026-11");
        let ec2 = FakeEc2::new(Some("stopped"));
        let out = run_muhurat(MUHURAT, &ec2, &budget, &FakeSns::default(), now).await;
        assert_eq!(out["skipped"], json!("budget_stop"));
        assert!(ec2.start_calls.borrow().is_empty());

        let holiday = FakeSsm::empty().holiday("2026-11-08");
        let ec2 = FakeEc2::new(Some("stopped"));
        let out = run_muhurat(MUHURAT, &ec2, &holiday, &FakeSns::default(), now).await;
        assert_eq!(out["skipped"], json!("holiday_stop"));
        assert!(ec2.start_calls.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_muhurat_check_failed_start_pages_once_at_the_first_attempt() {
        let ec2 = FakeEc2::new(Some("stopped")).start_raises();
        let sns = FakeSns::default();
        let out = run_muhurat(
            MUHURAT,
            &ec2,
            &FakeSsm::empty(),
            &sns,
            utc(2026, 11, 8, 7, 35),
        )
        .await;
        assert_eq!(out["started"], json!(false));
        assert_eq!(sns.published.borrow().len(), 1);
        assert!(sns.published.borrow()[0].subject.contains("Muhurat"));
        // A later retry logs, no second page.
        let sns = FakeSns::default();
        let ec2 = FakeEc2::new(Some("stopped")).start_raises();
        run_muhurat(
            MUHURAT,
            &ec2,
            &FakeSsm::empty(),
            &sns,
            utc(2026, 11, 8, 8, 5),
        )
        .await;
        assert!(sns.published.borrow().is_empty());
    }

    #[tokio::test]
    async fn test_muhurat_check_stops_its_own_box_after_the_window() {
        let ec2 = FakeEc2::new(Some("running")).launch(utc(2026, 11, 8, 7, 36));
        let sns = FakeSns::default();
        let out = run_muhurat(
            MUHURAT,
            &ec2,
            &FakeSsm::empty(),
            &sns,
            utc(2026, 11, 8, 11, 15),
        )
        .await;
        assert_eq!(out["phase"], json!("after"));
        assert_eq!(out["stopped"], json!(true));
        assert_eq!(*ec2.stop_calls.borrow(), vec!["i-0test".to_string()]);
        assert!(
            sns.published.borrow().is_empty(),
            "the planned stop pages nobody"
        );
    }

    #[tokio::test]
    async fn test_muhurat_check_after_window_honours_hold_and_keep_alive() {
        let now = utc(2026, 11, 8, 11, 15); // 16:45 IST, the hold counts again
        let launched = utc(2026, 11, 8, 7, 36);
        let held = FakeSsm::empty().drain_hold(&hold("holding", now + Duration::minutes(10), now));
        let ec2 = FakeEc2::new(Some("running")).launch(launched);
        let out = run_muhurat(MUHURAT, &ec2, &held, &FakeSns::default(), now).await;
        assert_eq!(out["stopped"], json!(false));
        assert!(ec2.stop_calls.borrow().is_empty());

        let kept = FakeSsm::empty().keep_alive("2026-11-08T23:00:00+05:30");
        let ec2 = FakeEc2::new(Some("running")).launch(launched);
        let out = run_muhurat(MUHURAT, &ec2, &kept, &FakeSns::default(), now).await;
        assert_eq!(out["stopped"], json!(false));
        assert!(ec2.stop_calls.borrow().is_empty());
    }

    #[test]
    fn test_muhurat_after_window_should_stop_table() {
        let box_start = utc(2026, 11, 8, 7, 35);
        let ours = Some(box_start + Duration::minutes(1));
        let slack_edge = Some(box_start - Duration::minutes(MUHURAT_OWN_START_SLACK_MINUTES));
        let earlier = Some(box_start - Duration::minutes(MUHURAT_OWN_START_SLACK_MINUTES + 1));
        assert!(muhurat_after_window_should_stop(
            "running", ours, box_start, false, false
        ));
        assert!(muhurat_after_window_should_stop(
            "running", slack_edge, box_start, false, false
        ));
        assert!(!muhurat_after_window_should_stop(
            "running", earlier, box_start, false, false
        ));
        assert!(!muhurat_after_window_should_stop(
            "running", None, box_start, false, false
        ));
        assert!(!muhurat_after_window_should_stop(
            "stopped", ours, box_start, false, false
        ));
        assert!(!muhurat_after_window_should_stop(
            "running", ours, box_start, true, false
        ));
        assert!(!muhurat_after_window_should_stop(
            "running", ours, box_start, false, true
        ));
    }

    #[tokio::test]
    async fn test_curfew_never_stops_the_box_inside_a_muhurat_window() {
        // Sunday 2026-11-08 14:35 IST, launched 13:06 IST: outside the weekday
        // operating hours and past the 45-minute grace, but inside the window.
        let ec2 = FakeEc2::new(Some("running")).launch(utc(2026, 11, 8, 7, 36));
        let out = run_watchdog(
            &json!({"mode": "curfew_check"}),
            &muhurat_env(MUHURAT),
            &ec2,
            &FakeSsm::empty(),
            &FakeSns::default(),
            utc(2026, 11, 8, 9, 5),
        )
        .await
        .expect("run_watchdog");
        assert_eq!(out["skipped"], json!("operating_hours"));
        assert!(ec2.stop_calls.borrow().is_empty());
        // Without the session configured the same box is curfew-stopped.
        let ec2 = FakeEc2::new(Some("running")).launch(utc(2026, 11, 8, 7, 36));
        let out = run_watchdog(
            &json!({"mode": "curfew_check"}),
            &muhurat_env("2026-11-08@-"),
            &ec2,
            &FakeSsm::empty(),
            &FakeSns::default(),
            utc(2026, 11, 8, 9, 5),
        )
        .await
        .expect("run_watchdog");
        assert_eq!(out["stopped"], json!(true));
    }
}

// ---------------------------------------------------------------------------
// The live SDK adapters and `handle`, driven through the REAL AWS SDK against
// a loopback account (crate::test_support). The decision logic above is
// covered with fakes; these prove the adapters turn real EC2 / SSM / SNS
// replies into the values that logic reads.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod sdk_loopback_tests {
    use super::*;
    use crate::test_support::{
        EnvVarGuard, Fleet, aws_env_guards, env_lock, loopback_sdk_config, operations, recorded,
        start_fleet,
    };

    async fn adapters(
        fleet: Fleet,
    ) -> (
        SdkEc2,
        SdkSsm,
        SdkSns,
        std::sync::Arc<std::sync::Mutex<Vec<crate::test_support::LoopbackRequest>>>,
    ) {
        let (endpoint, requests) = start_fleet(fleet).await;
        let config = loopback_sdk_config(&endpoint).await;
        (
            SdkEc2 {
                client: aws_sdk_ec2::Client::new(&config),
            },
            SdkSsm {
                client: aws_sdk_ssm::Client::new(&config),
            },
            SdkSns {
                client: aws_sdk_sns::Client::new(&config),
            },
            requests,
        )
    }

    #[tokio::test]
    async fn test_sdk_ec2_describe_reads_state_launch_time_and_public_ip() {
        let (ec2, _, _, requests) = adapters(Fleet::default()).await;
        let desc = ec2
            .describe("i-0loopback")
            .await
            .expect("describe succeeds")
            .expect("one instance");
        assert_eq!(desc.state, "running");
        assert_eq!(desc.public_ip.as_deref(), Some("203.0.113.7"));
        assert_eq!(
            desc.launch_time.map(|t| t.to_rfc3339()),
            Some("2026-10-30T03:00:00+00:00".to_string())
        );
        let seen = recorded(&requests);
        assert_eq!(
            seen[0].form_value("InstanceId.1").as_deref(),
            Some("i-0loopback")
        );
    }

    #[tokio::test]
    async fn test_sdk_ec2_describe_with_no_reservation_is_not_found() {
        let (ec2, _, _, _) = adapters(Fleet {
            instance_state: None,
            ..Fleet::default()
        })
        .await;
        assert!(
            ec2.describe("i-0gone")
                .await
                .expect("describe succeeds")
                .is_none()
        );
        assert_eq!(instance_info(&ec2, "i-0gone").await.0, "not-found");
    }

    #[tokio::test]
    async fn test_sdk_ec2_errors_surface_as_text() {
        let (ec2, _, _, _) = adapters(Fleet {
            fail: vec!["DescribeInstances", "StartInstances", "StopInstances"],
            ..Fleet::default()
        })
        .await;
        assert!(ec2.describe("i-0x").await.is_err());
        assert!(ec2.start_instances("i-0x").await.is_err());
        assert!(ec2.stop_instances("i-0x").await.is_err());
    }

    #[tokio::test]
    async fn test_sdk_ec2_start_and_stop_send_the_instance_id() {
        let (ec2, _, _, requests) = adapters(Fleet::default()).await;
        ec2.start_instances("i-0loopback")
            .await
            .expect("start succeeds");
        ec2.stop_instances("i-0loopback")
            .await
            .expect("stop succeeds");
        assert_eq!(
            operations(&requests),
            vec!["StartInstances", "StopInstances"]
        );
        assert!(
            recorded(&requests)
                .iter()
                .all(|r| r.form_value("InstanceId.1").as_deref() == Some("i-0loopback"))
        );
    }

    #[tokio::test]
    async fn test_sdk_ssm_get_parameter_reads_the_value_and_surfaces_a_miss() {
        let mut ssm_values = std::collections::HashMap::new();
        ssm_values.insert(
            DEFAULT_KEEP_ALIVE_PARAM.to_string(),
            "2026-10-30T18:00:00+05:30".to_string(),
        );
        let (_, ssm, _, _) = adapters(Fleet {
            ssm: ssm_values,
            ..Fleet::default()
        })
        .await;
        assert_eq!(
            ssm.get_parameter(DEFAULT_KEEP_ALIVE_PARAM).await.as_deref(),
            Ok("2026-10-30T18:00:00+05:30")
        );
        assert!(ssm.get_parameter("/tickvault/prod/absent").await.is_err());
    }

    #[tokio::test]
    async fn test_sdk_sns_publish_sends_topic_subject_and_message() {
        let (_, _, sns, requests) = adapters(Fleet::default()).await;
        sns.publish("arn:aws:sns:ap-south-1:1:tv", "subject line", "body text")
            .await
            .expect("publish succeeds");
        let seen = recorded(&requests);
        assert_eq!(
            seen[0].form_value("TopicArn").as_deref(),
            Some("arn:aws:sns:ap-south-1:1:tv")
        );
        assert_eq!(
            seen[0].form_value("Subject").as_deref(),
            Some("subject line")
        );
        assert_eq!(seen[0].form_value("Message").as_deref(), Some("body text"));
    }

    #[tokio::test]
    async fn test_sdk_sns_publish_failure_is_an_error() {
        let (_, _, sns, _) = adapters(Fleet {
            fail: vec!["Publish"],
            ..Fleet::default()
        })
        .await;
        assert!(sns.publish("arn:x", "s", "m").await.is_err());
    }

    #[test]
    fn test_watchdog_env_from_process_env_reads_overrides_and_defaults() {
        let _lock = env_lock();
        let keys = [
            "EC2_INSTANCE_ID",
            "ALERTS_TOPIC_ARN",
            "DASHBOARD_PORT",
            "KEEP_ALIVE_PARAM",
            "HOLIDAY_STOP_PARAM",
            "BUDGET_STOP_PARAM",
            "DRAIN_HOLD_PARAM",
            crate::muhurat_window::MUHURAT_SESSIONS_ENV,
        ];
        {
            let _unset: Vec<EnvVarGuard> = keys.iter().map(|k| EnvVarGuard::set(k, None)).collect();
            let env = WatchdogEnv::from_process_env();
            assert_eq!(env.instance_id, "");
            assert_eq!(env.alerts_topic_arn, "");
            assert_eq!(env.dashboard_port, "3001");
            assert_eq!(env.keep_alive_param, DEFAULT_KEEP_ALIVE_PARAM);
            assert_eq!(env.holiday_stop_param, DEFAULT_HOLIDAY_STOP_PARAM);
            assert_eq!(
                env.budget_stop_param,
                crate::budget_stop_latch::DEFAULT_BUDGET_STOP_PARAM
            );
            assert_eq!(
                env.drain_hold_param,
                crate::drain_hold::DEFAULT_DRAIN_HOLD_PARAM
            );
            assert_eq!(env.muhurat_sessions, "");
        }
        let _set: Vec<EnvVarGuard> = keys
            .iter()
            .map(|k| EnvVarGuard::set(k, Some(&format!("v-{k}"))))
            .collect();
        let env = WatchdogEnv::from_process_env();
        assert_eq!(env.instance_id, "v-EC2_INSTANCE_ID");
        assert_eq!(env.alerts_topic_arn, "v-ALERTS_TOPIC_ARN");
        assert_eq!(env.dashboard_port, "v-DASHBOARD_PORT");
        assert_eq!(env.keep_alive_param, "v-KEEP_ALIVE_PARAM");
        assert_eq!(env.holiday_stop_param, "v-HOLIDAY_STOP_PARAM");
        assert_eq!(env.budget_stop_param, "v-BUDGET_STOP_PARAM");
        assert_eq!(env.drain_hold_param, "v-DRAIN_HOLD_PARAM");
        assert_eq!(
            env.muhurat_sessions,
            format!("v-{}", crate::muhurat_window::MUHURAT_SESSIONS_ENV)
        );
    }

    #[tokio::test]
    async fn test_handle_ping_describes_the_instance_and_publishes_the_dashboard_link() {
        let (endpoint, requests) = start_fleet(Fleet::default()).await;
        let _lock = env_lock();
        let _aws = aws_env_guards(&endpoint);
        let _id = EnvVarGuard::set("EC2_INSTANCE_ID", Some("i-0loopback"));
        let _topic = EnvVarGuard::set("ALERTS_TOPIC_ARN", Some("arn:aws:sns:ap-south-1:1:tv"));
        let _port = EnvVarGuard::set("DASHBOARD_PORT", Some("3001"));

        let out = handle(json!({"mode": "ping"}))
            .await
            .expect("ping succeeds");
        assert!(out.is_object(), "{out}");
        let ops = operations(&requests);
        assert!(ops.iter().any(|o| o == "DescribeInstances"), "{ops:?}");
        let publish = recorded(&requests)
            .into_iter()
            .find(|r| r.query_action().as_deref() == Some("Publish"))
            .expect("the ping publishes");
        let message = publish.form_value("Message").unwrap_or_default();
        assert!(
            message.contains("http://203.0.113.7:3001/feeds"),
            "the start message carries the dashboard link: {message}"
        );
    }
}
