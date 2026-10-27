//! `tickvault holiday-gate`: the boot-time NSE-holiday self-stop gate.
//!
//! Was `deploy/aws/holiday-gate.sh` until 2026-10-01 (audit-plan D6c). The
//! `tickvault-holiday-gate.service` oneshot runs it once per boot, ordered
//! before `tickvault.service`. The EventBridge cron starts the box every
//! weekday, but NSE has weekday holidays on which a boot is a no-op that still
//! bills about eight hours. The gate asks the app's own calendar (the same
//! `config/base.toml` holiday list the app uses) whether today is a trading
//! day, and stops the instance if it is not.
//!
//! FAIL-OPEN by design, exactly as the script was: the gate stops the box ONLY
//! on a definitive non-trading verdict. A config or calendar load error, an
//! IMDS failure, or anything unexpected lets the app start. A real trading day
//! can never be killed by a transient gate failure. A missing binary (first
//! boot) is handled by the unit itself: its `ExecStart=-` ignores the failure.
//!
//! Override: `touch /opt/tickvault/ALLOW_HOLIDAY_RUN` to run on a holiday
//! without the auto-stop.
//!
//! **⚠ CHANGED 2026-10-20 — the gate stops the box ONLY on a weekday NSE
//! holiday, and only at the scheduled start.** Until this date it stopped on
//! any non-trading verdict (`--check-trading-day` exit 75), which covers
//! weekends too. That was harmless while the deploy never enabled the unit;
//! once every deploy enables it, a weekend or after-hours boot (the
//! post-merge catch-up deploy every 30 minutes on all seven days, a console
//! start, a filesystem-recovery or downsize run, a manual Sunday Muhurat
//! start) would be stopped mid-run with a page calling the day an NSE
//! holiday. The verdict is now [`decide`]: stop only when today (IST) is a
//! Monday-to-Friday date in the NSE holiday list with no Muhurat or mock
//! session ([`classify_calendar_day`]), AND the boot falls inside
//! [`SCHEDULED_START_WINDOW_START_SECS_OF_DAY_IST`] ..
//! [`SCHEDULED_START_WINDOW_END_SECS_OF_DAY_IST`] (08:25 to 09:00 IST, around
//! the 08:30 IST weekday start; the end was 09:15 until 2026-10-26). Every other boot starts the app and logs one
//! line saying why. The `--check-trading-day` CLI contract (0 / 75 / 70) is
//! unchanged; the gate no longer reads it.
//!
//! The window START is `shutdown_class`'s 08:25 holiday-stop start (the same
//! constant), which is also the launch time from which the
//! `market_open_readiness` Lambda reads a stopped box as the gate's holiday
//! self-stop (`HOLIDAY_SELF_STOP_EARLIEST_IST_MINUTES`, 08:25; pinned by
//! `holiday_gate_window_start_matches_the_readiness_lambdas_floor` (crates/app/tests/host_tuning_boot_wiring_guard.rs)).
//! The Lambda compares the EC2 LAUNCH time, the gate its own clock a few
//! seconds later in the same boot, so a launch in the last seconds before
//! 08:25 can be stopped here yet read there as launched before the floor
//! (2026-10-20 review note; the scheduled 08:30 start is never that close).
//! `shutdown_class`'s quiet window ENDS at this window's end plus the app's
//! shutdown lag, so every stop the gate issues classifies quiet.
//!
//! **Deploys on a holiday-stop day (2026-10-20).** The gate stamps
//! `/tickvault/<env>/holiday-stop-date` before it stops. The deploy-watchdog
//! Lambda (`deploy_watchdog.rs`) does not dispatch a redeploy while that
//! marker is today's IST date, and `deploy-aws.yml` does not start a box
//! stopped on such a day (`DEPLOY_SKIP_REASON=holiday_stop`, green). Every
//! dispatcher (the watchdog, `deploy-aws-after-close.yml`,
//! `postmerge-catchup.yml`) ends in that one `deploy-aws.yml` check.
//! **Honest limit:** on the instance-start event the watchdog fires within a
//! minute of `running`, and the marker is written by the gate a little later
//! in the same boot, so that one firing can still dispatch; the dispatched
//! deploy then finds the box running or stopping, not stopped, and either
//! skips at the marker once the box is stopped or meets the stop during its
//! verify (reported as an external stop, not a crash).
//!
//! **A second start the same day is honoured (2026-10-20).** Before it stops,
//! the gate reads its own marker. When the marker already holds today's IST
//! date, the gate stopped this box earlier today, so this boot is a deliberate
//! restart (a console start, or a start an operator ran on purpose) and the
//! gate lets the app start with one line saying so
//! ([`decide_second_start`]). Until this date a restart inside 08:25 to 09:15
//! IST (09:00 since 2026-10-26) was stopped again and paged again. A missing parameter (the gate has
//! never stopped a box) is a first boot and stops. A FAILED read (timeout,
//! access denied, throttling) also stops, as before: the read cannot tell a
//! first boot from a second, the first boot of a holiday is the common case
//! and the one that saves the bill, and a restart stopped that way is undone
//! by `ALLOW_HOLIDAY_RUN` or by starting again after 09:00 IST.
//! **Honest limits:** (a) a same-day reboot of a box whose stop CALL failed
//! now starts the app instead of trying the stop again; (b) after a second
//! start the marker still reads today, so the start-watchdog, the
//! deploy-watchdog and the market-hours alarm gate treat the running box as
//! holiday-stopped for the rest of the day (no self-start, no automatic
//! redeploy, no liveness page; `deploy-aws.yml` run by hand still deploys a
//! running box, and refuses to start it again if it stops); (c) when the marker put fails twice (below),
//! a second start the same day is stopped again, which errs toward the stop.
//!
//! Exit codes, consumed by the oneshot unit and journald:
//! - `0` = let the app start (trading day, override, fail-open, or a second
//!   start on a day the gate already stopped the box);
//! - `1` = holiday verdict, stop issued (the app must not start).
//!
//! Before the stop it stamps today's IST date into
//! `/tickvault/<env>/holiday-stop-date` (so the start-watchdog, the autopilot
//! and the market-hours alarm gate recognise the stop as intentional, and a
//! second start today is let through) and publishes one plain-English line to
//! `tv-<env>-alerts`. Both are fail-open: a failure is logged and the stop
//! still proceeds. The marker put is tried [`MARKER_PUT_ATTEMPTS`] times
//! (one retry after [`MARKER_PUT_RETRY_DELAY_SECS`], 2026-10-20), each under
//! [`AWS_CALL_TIMEOUT_SECS`]; with the marker read that adds at most 62 s to
//! the stop path, inside the unit's 300 s `TimeoutStartSec`.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

/// The first argument that selects this subcommand.
pub const HOLIDAY_GATE_SUBCOMMAND: &str = "holiday-gate";

/// The operator's "run today anyway" marker.
const OVERRIDE_MARKER_PATH: &str = "/opt/tickvault/ALLOW_HOLIDAY_RUN";

/// Start (inclusive) of the IST window in which a boot counts as the
/// scheduled weekday start: 08:25:00.
///
/// The EventBridge rule starts the box at `cron(0 3 ? * MON-FRI *)`, 03:00
/// UTC = 08:30 IST (`deploy/aws/terraform/main.tf`). The window allows 5
/// minutes before it for scheduler jitter and 45 after it for a slow boot.
/// It IS `shutdown_class`'s holiday-stop start, not a copy of it: that
/// constant also matches the 08:25 launch floor the `market_open_readiness`
/// Lambda uses to read a stopped box as the gate's self-stop, so all three
/// agree on when the gate's stop can begin. A boot outside this window is a
/// deploy, a console start or a recovery run, never the scheduled start, so
/// the gate lets it run even on a weekday holiday (2026-10-20; the first
/// draft started the window at 08:00, before both of those, and a gate stop
/// between 08:00 and 08:25 would have paged as an external stop).
pub const SCHEDULED_START_WINDOW_START_SECS_OF_DAY_IST: u32 =
    crate::shutdown_class::HOLIDAY_GATE_STOP_WINDOW_START_SECS_OF_DAY_IST;

/// End (exclusive) of the scheduled-start IST window: 09:00:00, the start of
/// the capture window (`TICK_PERSIST_START_SECS_OF_DAY_IST`), 30 minutes
/// after the scheduled start so a slow boot is still stopped. Pinned with the
/// start by `test_scheduled_start_window_is_0825_to_0900_ist_around_the_0830_start`.
///
/// **⚠ CHANGED 2026-10-26 (audit PR L4):** was 09:15, the market open. Capture
/// starts at 09:00, so a holiday verdict between 09:00 and 09:15 (a wrong
/// holiday list, or a slow boot on a day that is trading after all) stopped
/// the box inside the capture window. The stop arm now ends where capture
/// begins; a boot at or after 09:00 always starts the app.
pub const SCHEDULED_START_WINDOW_END_SECS_OF_DAY_IST: u32 =
    tickvault_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST;

// Every stop this gate issues must classify quiet in `shutdown_class`: the
// gate's window must sit INSIDE that quiet window, with the same start (the
// gate reuses the constant) and an end at least the app's shutdown lag
// earlier, because the app's SIGTERM lands after the gate decided. Fails the
// build if either window moves past the other.
const _: () = assert!(
    SCHEDULED_START_WINDOW_START_SECS_OF_DAY_IST
        == crate::shutdown_class::HOLIDAY_GATE_STOP_WINDOW_START_SECS_OF_DAY_IST
        && SCHEDULED_START_WINDOW_START_SECS_OF_DAY_IST
            < SCHEDULED_START_WINDOW_END_SECS_OF_DAY_IST
        && SCHEDULED_START_WINDOW_END_SECS_OF_DAY_IST
            + crate::shutdown_class::HOLIDAY_GATE_STOP_LAG_SECS
            <= crate::shutdown_class::HOLIDAY_GATE_STOP_WINDOW_END_SECS_OF_DAY_IST
);

/// What the gate's calendar says about one IST date (2026-10-20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarDay {
    /// A regular NSE trading day.
    TradingDay,
    /// Saturday or Sunday with no Muhurat or mock session.
    Weekend,
    /// Monday to Friday, in the NSE holiday list, with no Muhurat or mock
    /// session. The only verdict the gate can stop on.
    WeekdayHoliday,
    /// A Muhurat or NSE mock-trading session falls on this date.
    SpecialSession,
    /// The config or the calendar could not be loaded.
    Unreadable,
}

/// Pure: classify one IST date from the calendar's answers. A Muhurat or
/// mock session wins over a holiday or a weekend, so a Muhurat start on a
/// holiday (Diwali) is never stopped.
pub fn classify_calendar_day(
    date: chrono::NaiveDate,
    is_nse_holiday: bool,
    is_special_session: bool,
) -> CalendarDay {
    use chrono::Datelike as _;
    if is_special_session {
        return CalendarDay::SpecialSession;
    }
    if matches!(date.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun) {
        return CalendarDay::Weekend;
    }
    if is_nse_holiday {
        CalendarDay::WeekdayHoliday
    } else {
        CalendarDay::TradingDay
    }
}

/// True when `secs_of_day_ist` falls inside the scheduled-start window.
fn in_scheduled_start_window(secs_of_day_ist: u32) -> bool {
    (SCHEDULED_START_WINDOW_START_SECS_OF_DAY_IST..SCHEDULED_START_WINDOW_END_SECS_OF_DAY_IST)
        .contains(&secs_of_day_ist)
}

/// `HH:MM` for an IST seconds-of-day, for the log line.
fn hh_mm(secs_of_day: u32) -> String {
    format!("{:02}:{:02}", secs_of_day / 3600, (secs_of_day % 3600) / 60)
}

/// What this subcommand exits with.
const EXIT_LET_APP_START: i32 = 0;
const EXIT_STOP_ISSUED: i32 = 1;

/// Instance metadata service, IMDSv2 (token required).
const IMDS_BASE_URL: &str = "http://169.254.169.254/latest";
const IMDS_TOKEN_TTL_HEADER: &str = "X-aws-ec2-metadata-token-ttl-seconds";
const IMDS_TOKEN_TTL_SECS: &str = "120";
const IMDS_TOKEN_HEADER: &str = "X-aws-ec2-metadata-token";
/// Per-request ceiling, the script's `curl -m 5`.
const IMDS_TIMEOUT_SECS: u64 = 5;

/// Ceiling on each AWS call, so a hung endpoint can never hold the boot.
const AWS_CALL_TIMEOUT_SECS: u64 = 30;

/// Tries at the holiday-stop marker put before the stop (2026-10-20): the
/// first and one retry, never more. Bounded so a dead SSM endpoint cannot
/// hold the boot: two puts of at most [`AWS_CALL_TIMEOUT_SECS`] each plus
/// [`MARKER_PUT_RETRY_DELAY_SECS`].
const MARKER_PUT_ATTEMPTS: u32 = 2;

/// Pause before the one marker-put retry, so a brief throttle can clear.
const MARKER_PUT_RETRY_DELAY_SECS: u64 = 2;

/// Environment variable naming the deployment, `prod` when unset.
const ENVIRONMENT_VAR: &str = "TV_ENVIRONMENT";
const DEFAULT_ENVIRONMENT: &str = "prod";

const SNS_SUBJECT: &str = "🔔 Holiday stop";

/// True when the process was started as `tickvault holiday-gate`.
pub fn is_invocation(args: &[String]) -> bool {
    args.get(1).map(String::as_str) == Some(HOLIDAY_GATE_SUBCOMMAND)
}

/// One line to stdout, which the unit sends to journald. A failed write is
/// ignored: there is nowhere else to report it.
fn emit(line: &str) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "holiday-gate: {line}").unwrap_or_default();
}

/// What the gate does with a calendar verdict.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Let the app start, with the reason to log.
    Start(String),
    /// Today is a weekday NSE holiday and this is the scheduled start: stop
    /// the instance, unless the gate's own marker shows it already stopped
    /// this box today ([`decide_second_start`], 2026-10-20).
    Stop,
}

/// Pure decision (2026-10-20): the override marker wins; otherwise stop ONLY
/// on a weekday NSE holiday ([`CalendarDay::WeekdayHoliday`]) at a boot
/// inside the scheduled-start window. A weekend, a Muhurat or mock session, a
/// trading day, an unreadable calendar, or a boot outside the window all let
/// the app start, each with the reason to log. `calendar_day` is called only
/// when no override marker is present.
fn decide(
    override_present: bool,
    now_ist: chrono::NaiveDateTime,
    calendar_day: impl FnOnce(chrono::NaiveDate) -> CalendarDay,
) -> Verdict {
    use chrono::Timelike as _;
    if override_present {
        return Verdict::Start(format!(
            "override marker present ({OVERRIDE_MARKER_PATH}) — skipping holiday check, app starts"
        ));
    }
    let today = now_ist.date();
    let clock = now_ist.format("%H:%M IST");
    match calendar_day(today) {
        CalendarDay::TradingDay => Verdict::Start("trading day — app start proceeds".to_string()),
        CalendarDay::Weekend => Verdict::Start(format!(
            "weekend ({today}) — the scheduled start never runs on a weekend, so this boot is a \
             deploy or a manual start; app starts"
        )),
        CalendarDay::SpecialSession => Verdict::Start(format!(
            "Muhurat or mock trading session on {today} — app starts"
        )),
        CalendarDay::Unreadable => Verdict::Start(
            "config or calendar could not be loaded (not a definitive holiday) — fail-open, app starts"
                .to_string(),
        ),
        CalendarDay::WeekdayHoliday => {
            if in_scheduled_start_window(now_ist.num_seconds_from_midnight()) {
                Verdict::Stop
            } else {
                Verdict::Start(format!(
                    "weekday NSE holiday ({today}), but this boot at {clock} is outside the \
                     scheduled start window ({} to {} IST), so it is a deploy or a manual \
                     start; app starts",
                    hh_mm(SCHEDULED_START_WINDOW_START_SECS_OF_DAY_IST),
                    hh_mm(SCHEDULED_START_WINDOW_END_SECS_OF_DAY_IST)
                ))
            }
        }
    }
}

/// Run the gate. `calendar_day` is the app's own calendar (the same layered
/// config the app boots with), asked about today's IST date only when no
/// override marker is present.
pub async fn run(calendar_day: impl FnOnce(chrono::NaiveDate) -> CalendarDay) -> i32 {
    let now = ist_now();
    match decide(Path::new(OVERRIDE_MARKER_PATH).exists(), now, calendar_day) {
        Verdict::Start(reason) => {
            emit(&reason);
            EXIT_LET_APP_START
        }
        Verdict::Stop => {
            emit(&format!(
                "weekday NSE holiday ({}) at the scheduled start ({}) — stopping this instance \
                 unless the gate already stopped it today",
                now.date(),
                now.format("%H:%M IST")
            ));
            stop_this_instance(IMDS_BASE_URL).await
        }
    }
}

/// The instance facts the stop needs, read from IMDSv2.
struct InstanceFacts {
    token: String,
    instance_id: String,
    region: String,
}

fn imds_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(IMDS_TIMEOUT_SECS))
        .build()
        .ok()
}

async fn imds_token(client: &reqwest::Client, base: &str) -> Option<String> {
    let response = client
        .put(format!("{base}/api/token"))
        .header(IMDS_TOKEN_TTL_HEADER, IMDS_TOKEN_TTL_SECS)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let token = response.text().await.ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

async fn imds_get(client: &reqwest::Client, base: &str, token: &str, path: &str) -> Option<String> {
    let response = client
        .get(format!("{base}/{path}"))
        .header(IMDS_TOKEN_HEADER, token)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let body = response.text().await.ok()?;
    let body = body.trim();
    (!body.is_empty()).then(|| body.to_string())
}

/// `accountId` from the instance identity document, digits only.
fn account_id_from_identity_document(document: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(document).ok()?;
    let account = value.get("accountId")?.as_str()?;
    (!account.is_empty() && account.bytes().all(|b| b.is_ascii_digit()))
        .then(|| account.to_string())
}

fn holiday_stop_param(environment: &str) -> String {
    format!("/tickvault/{environment}/holiday-stop-date")
}

/// What the gate's read of its own holiday-stop marker returned (2026-10-20).
#[derive(Debug, PartialEq, Eq)]
enum MarkerRead {
    /// The parameter holds this value, as read (untrimmed).
    Value(String),
    /// The parameter does not exist: the gate has never stopped a box.
    Absent,
    /// The read failed or timed out; the reason, for the log line.
    Failed(String),
}

/// What the gate does at the scheduled start of a weekday holiday, given its
/// own marker (2026-10-20).
#[derive(Debug, PartialEq, Eq)]
enum SecondStart {
    /// No stop recorded for today, or the marker could not be read: stop.
    Stop,
    /// The gate already stopped this box today, so this boot is a deliberate
    /// second start: let the app run.
    AlreadyStoppedToday,
}

/// Pure (2026-10-20): a marker holding today's IST date (whitespace trimmed,
/// as every other reader of it trims) means the gate already stopped this
/// box today, so the boot is let through. Anything else stops: a missing
/// parameter (first ever stop), an older date (an earlier holiday), an empty
/// value, and a FAILED read. Stopping on a failed read is deliberate: the
/// read cannot tell a first boot from a second, and the first boot of a
/// holiday is the common case and the one that saves the bill.
fn decide_second_start(marker: &MarkerRead, today_ist: &str) -> SecondStart {
    match marker {
        MarkerRead::Value(value) if value.trim() == today_ist => SecondStart::AlreadyStoppedToday,
        MarkerRead::Value(_) | MarkerRead::Absent | MarkerRead::Failed(_) => SecondStart::Stop,
    }
}

/// Read the gate's own holiday-stop marker, bounded by
/// [`AWS_CALL_TIMEOUT_SECS`]. `ParameterNotFound` is [`MarkerRead::Absent`];
/// it is matched on the full error context, as `instance_lock` does, because
/// the SDK error's bare `Display` hides the error code.
async fn read_holiday_marker(ssm: &aws_sdk_ssm::Client, name: &str) -> MarkerRead {
    let get = ssm.get_parameter().name(name).send();
    match tokio::time::timeout(Duration::from_secs(AWS_CALL_TIMEOUT_SECS), get).await {
        Err(_) => MarkerRead::Failed(format!("no answer within {AWS_CALL_TIMEOUT_SECS}s")),
        Ok(Err(err)) => {
            let reason = format!("{}", aws_sdk_ssm::error::DisplayErrorContext(&err));
            if reason.contains("ParameterNotFound") {
                MarkerRead::Absent
            } else {
                MarkerRead::Failed(reason)
            }
        }
        Ok(Ok(out)) => match out.parameter().and_then(|p| p.value()) {
            Some(value) => MarkerRead::Value(value.to_string()),
            None => MarkerRead::Absent,
        },
    }
}

fn alerts_topic_arn(region: &str, account: &str, environment: &str) -> String {
    format!("arn:aws:sns:{region}:{account}:tv-{environment}-alerts")
}

/// The page's text. `marker_written` (2026-10-20) says whether today's stop
/// was recorded: only then is a second start today let through, so only then
/// does the page promise it.
fn holiday_stop_message(ist_now: &str, today_ist: &str, marker_written: bool) -> String {
    let second_start = if marker_written {
        "If you start it again today, it stays on: the box switches itself off only once a day."
    } else {
        "Today's stop could not be recorded, so if you start it again before 9:15 AM it may switch itself off again; start it after 9:15 AM to keep it on."
    };
    format!(
        "🔔 The trading box is switching itself OFF at {ist_now}: today ({today_ist}) is an NSE holiday, so no market data will be captured and no bill is run up. If today IS a trading day, this is WRONG — start the box from the console and check the holiday list. {second_start}"
    )
}

/// IST wall clock now.
fn ist_now() -> chrono::NaiveDateTime {
    (chrono::Utc::now()
        + chrono::TimeDelta::seconds(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64))
    .naive_utc()
}

async fn with_timeout<T, E>(call: impl std::future::Future<Output = Result<T, E>>) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(AWS_CALL_TIMEOUT_SECS), call).await,
        Ok(Ok(_))
    )
}

/// Resolve the instance from IMDS, check the marker for a second start today,
/// stamp the marker, page, and stop. Returns `0` (fail-open) when the
/// instance cannot be resolved safely, and `0` when the gate already stopped
/// this box today (2026-10-20, [`decide_second_start`]); otherwise `1`
/// whether or not the stop call itself succeeded, as the script did: the OS is
/// expected to go down. A failed stop call is tried again at the next
/// scheduled start; a reboot the same day starts the app, because the marker
/// already reads today (the module doc's honest limit (a)).
async fn stop_this_instance(imds_base: &str) -> i32 {
    let Some(client) = imds_client() else {
        emit("could not build the metadata client — fail-open");
        return EXIT_LET_APP_START;
    };
    let Some(token) = imds_token(&client, imds_base).await else {
        emit("IMDSv2 token fetch failed — fail-open (cannot resolve instance-id safely)");
        return EXIT_LET_APP_START;
    };
    let instance_id = imds_get(&client, imds_base, &token, "meta-data/instance-id").await;
    let region = imds_get(&client, imds_base, &token, "meta-data/placement/region").await;
    let (Some(instance_id), Some(region)) = (instance_id, region) else {
        emit("instance-id/region resolve failed — fail-open");
        return EXIT_LET_APP_START;
    };
    let facts = InstanceFacts {
        token,
        instance_id,
        region,
    };

    let environment =
        std::env::var(ENVIRONMENT_VAR).unwrap_or_else(|_| DEFAULT_ENVIRONMENT.to_string());
    let now = ist_now();
    let today_ist = now.format("%F").to_string();
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(facts.region.clone()))
        .load()
        .await;

    let marker = holiday_stop_param(&environment);
    let ssm = aws_sdk_ssm::Client::new(&config);

    // 2026-10-20: a second start the same day is honoured. The gate stamps
    // the marker before every stop, so a marker already holding today's date
    // means it stopped this box earlier today and this boot is a deliberate
    // restart. A failed read stops as before (see `decide_second_start`).
    let marker_read = read_holiday_marker(&ssm, &marker).await;
    match decide_second_start(&marker_read, &today_ist) {
        SecondStart::AlreadyStoppedToday => {
            emit(&format!(
                "the gate already stopped this box today ({marker} = {today_ist}), so this boot \
                 is a deliberate second start — app starts, no stop and no page"
            ));
            return EXIT_LET_APP_START;
        }
        SecondStart::Stop => {
            if let MarkerRead::Failed(reason) = &marker_read {
                emit(&format!(
                    "holiday-stop marker read FAILED ({marker}: {reason}) — cannot tell a second \
                     start from the first, so the stop proceeds"
                ));
            }
        }
    }

    // The intentional-stop marker, BEFORE the stop: without it the
    // start-watchdog, the autopilot and the market-hours alarm gate fight the
    // stop all day, and a second start today is stopped again. A stale marker
    // is harmless (consumers compare it against today's IST date). One retry
    // (2026-10-20), bounded by `MARKER_PUT_ATTEMPTS`.
    let mut marker_written = false;
    for attempt in 1..=MARKER_PUT_ATTEMPTS {
        if attempt > 1 {
            tokio::time::sleep(Duration::from_secs(MARKER_PUT_RETRY_DELAY_SECS)).await;
        }
        let put = ssm
            .put_parameter()
            .name(&marker)
            .value(&today_ist)
            .r#type(aws_sdk_ssm::types::ParameterType::String)
            .overwrite(true)
            .send();
        if with_timeout(put).await {
            marker_written = true;
            emit(&format!(
                "holiday-stop marker written: {marker} = {today_ist}"
            ));
            break;
        }
        emit(&format!(
            "holiday-stop marker put FAILED ({marker}), try {attempt} of {MARKER_PUT_ATTEMPTS}"
        ));
    }
    if !marker_written {
        emit(&format!(
            "holiday-stop marker NOT written ({marker}) — restarters may fight today's stop, and \
             a second start today will be stopped again"
        ));
    }

    // One plain-English line to the operator, so a wrong verdict on a real
    // trading day is noticed at 08:31 IST rather than at the 09:20 alarm gate.
    let account = imds_get(
        &client,
        imds_base,
        &facts.token,
        "dynamic/instance-identity/document",
    )
    .await
    .as_deref()
    .and_then(account_id_from_identity_document);
    match account {
        Some(account) => {
            let sns = aws_sdk_sns::Client::new(&config);
            let publish = sns
                .publish()
                .topic_arn(alerts_topic_arn(&facts.region, &account, &environment))
                .subject(SNS_SUBJECT)
                .message(holiday_stop_message(
                    &now.format("%I:%M %p IST").to_string(),
                    &today_ist,
                    marker_written,
                ))
                .send();
            if with_timeout(publish).await {
                emit(&format!(
                    "holiday-stop page published to tv-{environment}-alerts"
                ));
            } else {
                emit("holiday-stop page publish FAILED — the stop still proceeds");
            }
        }
        None => emit("account id unresolved — holiday-stop page skipped (fail-open)"),
    }

    emit(&format!(
        "stopping instance {} in {} (NSE holiday — saves ~8h of billing)",
        facts.instance_id, facts.region
    ));
    let ec2 = aws_sdk_ec2::Client::new(&config);
    let stop = ec2.stop_instances().instance_ids(&facts.instance_id).send();
    if !with_timeout(stop).await {
        emit(
            "aws ec2 stop-instances call failed (the next scheduled start tries again; a reboot \
             today starts the app, the marker already reads today)",
        );
    }
    // Abort the app start: the OS is going down.
    EXIT_STOP_ISSUED
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[test]
    fn test_is_invocation_matches_only_the_first_argument() {
        let args = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert!(is_invocation(&args(&["tickvault", "holiday-gate"])));
        assert!(!is_invocation(&args(&["tickvault"])));
        assert!(!is_invocation(&args(&["tickvault", "--check-trading-day"])));
        assert!(!is_invocation(&args(&["tickvault", "x", "holiday-gate"])));
        assert!(!is_invocation(&[]));
    }

    fn date(y: i32, m: u32, d: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(y, m, d).expect("valid date") // APPROVED: test
    }

    fn at(day: chrono::NaiveDate, h: u32, m: u32) -> chrono::NaiveDateTime {
        day.and_hms_opt(h, m, 0).expect("valid time") // APPROVED: test
    }

    /// A calendar with one NSE holiday (Tuesday 2026-10-20) and one Muhurat
    /// session (Sunday 2026-11-08), classified the way `main.rs` does.
    fn calendar(day: chrono::NaiveDate) -> CalendarDay {
        let holiday = day == date(2026, 10, 20);
        let muhurat = day == date(2026, 11, 8);
        classify_calendar_day(day, holiday, muhurat)
    }

    #[test]
    fn test_override_marker_wins_and_skips_the_check() {
        let now = at(date(2026, 10, 20), 8, 30);
        let verdict = decide(true, now, |_| panic!("the calendar must not be consulted"));
        assert!(matches!(verdict, Verdict::Start(ref r) if r.contains("override marker")));
    }

    #[test]
    fn test_scheduled_start_window_is_0825_to_0900_ist_around_the_0830_start() {
        assert_eq!(
            SCHEDULED_START_WINDOW_START_SECS_OF_DAY_IST,
            8 * 3600 + 25 * 60
        );
        assert_eq!(SCHEDULED_START_WINDOW_END_SECS_OF_DAY_IST, 9 * 3600);
        // The EventBridge start, 03:00 UTC = 08:30 IST, sits inside it.
        assert!(in_scheduled_start_window(8 * 3600 + 30 * 60));
        assert!(in_scheduled_start_window(
            SCHEDULED_START_WINDOW_START_SECS_OF_DAY_IST
        ));
        assert!(!in_scheduled_start_window(
            SCHEDULED_START_WINDOW_START_SECS_OF_DAY_IST - 1
        ));
        assert!(in_scheduled_start_window(
            SCHEDULED_START_WINDOW_END_SECS_OF_DAY_IST - 1
        ));
        assert!(!in_scheduled_start_window(
            SCHEDULED_START_WINDOW_END_SECS_OF_DAY_IST
        ));
        // It sits inside shutdown_class's quiet holiday-stop window, same
        // start, end plus the shutdown lag inside (also checked at compile
        // time, next to the constants).
        assert_eq!(
            SCHEDULED_START_WINDOW_START_SECS_OF_DAY_IST,
            crate::shutdown_class::HOLIDAY_GATE_STOP_WINDOW_START_SECS_OF_DAY_IST
        );
        assert!(
            SCHEDULED_START_WINDOW_END_SECS_OF_DAY_IST
                + crate::shutdown_class::HOLIDAY_GATE_STOP_LAG_SECS
                <= crate::shutdown_class::HOLIDAY_GATE_STOP_WINDOW_END_SECS_OF_DAY_IST
        );
        assert_eq!(hh_mm(SCHEDULED_START_WINDOW_START_SECS_OF_DAY_IST), "08:25");
        assert_eq!(hh_mm(SCHEDULED_START_WINDOW_END_SECS_OF_DAY_IST), "09:00");
    }

    #[test]
    fn test_classify_calendar_day() {
        // Tuesday, not a holiday.
        assert_eq!(
            classify_calendar_day(date(2026, 10, 21), false, false),
            CalendarDay::TradingDay
        );
        // Tuesday holiday.
        assert_eq!(
            classify_calendar_day(date(2026, 10, 20), true, false),
            CalendarDay::WeekdayHoliday
        );
        // Saturday and Sunday, listed as a holiday or not.
        for (day, holiday) in [
            (date(2026, 10, 24), false),
            (date(2026, 10, 25), false),
            (date(2026, 10, 24), true),
        ] {
            assert_eq!(
                classify_calendar_day(day, holiday, false),
                CalendarDay::Weekend
            );
        }
        // A Muhurat or mock session wins over a weekend and over a holiday.
        assert_eq!(
            classify_calendar_day(date(2026, 11, 8), false, true),
            CalendarDay::SpecialSession
        );
        assert_eq!(
            classify_calendar_day(date(2026, 10, 20), true, true),
            CalendarDay::SpecialSession
        );
    }

    #[test]
    fn test_weekday_holiday_inside_the_scheduled_start_window_stops() {
        for (h, m) in [(8, 25), (8, 30), (8, 31), (8, 59)] {
            assert_eq!(
                decide(false, at(date(2026, 10, 20), h, m), calendar),
                Verdict::Stop,
                "{h:02}:{m:02} IST on a weekday holiday is the scheduled start"
            );
        }
    }

    #[test]
    fn test_weekday_holiday_outside_the_window_starts() {
        for (h, m) in [
            (10, 0),
            (8, 0),
            (8, 24),
            (9, 0),
            (9, 15),
            (13, 0),
            (17, 50),
            (0, 5),
        ] {
            assert!(
                matches!(
                    decide(false, at(date(2026, 10, 20), h, m), calendar),
                    Verdict::Start(ref r) if r.contains("outside the scheduled start window (08:25 to 09:00 IST)")
                ),
                "{h:02}:{m:02} IST on a weekday holiday is a deploy or manual start"
            );
        }
    }

    // Regression: 2026-10-26 (audit PR L4) — the stop arm ran until 09:15, so
    // a holiday verdict at 09:05 stopped the box inside the 09:00-15:40 IST
    // capture window.
    #[test]
    fn test_regression_holiday_verdict_at_0905_does_not_stop() {
        for (h, m) in [(9, 0), (9, 5), (9, 14)] {
            assert!(
                matches!(
                    decide(false, at(date(2026, 10, 20), h, m), calendar),
                    Verdict::Start(_)
                ),
                "{h:02}:{m:02} IST is inside the capture window; the gate must not stop"
            );
        }
        assert_eq!(
            SCHEDULED_START_WINDOW_END_SECS_OF_DAY_IST,
            tickvault_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST
        );
    }

    #[test]
    fn test_weekend_boots_start_at_any_hour() {
        for day in [date(2026, 10, 24), date(2026, 10, 25)] {
            for (h, m) in [(8, 30), (10, 0), (22, 0)] {
                assert!(
                    matches!(
                        decide(false, at(day, h, m), calendar),
                        Verdict::Start(ref r) if r.contains("weekend")
                    ),
                    "{day} {h:02}:{m:02} must start"
                );
            }
        }
    }

    #[test]
    fn test_sunday_muhurat_starts() {
        for (h, m) in [(8, 30), (17, 0)] {
            assert!(matches!(
                decide(false, at(date(2026, 11, 8), h, m), calendar),
                Verdict::Start(ref r) if r.contains("Muhurat")
            ));
        }
    }

    #[test]
    fn test_trading_day_starts_inside_and_outside_the_window() {
        for (h, m) in [(8, 30), (12, 0)] {
            assert!(matches!(
                decide(false, at(date(2026, 10, 21), h, m), calendar),
                Verdict::Start(ref r) if r.contains("trading day")
            ));
        }
    }

    #[test]
    fn test_unreadable_calendar_fails_open_even_at_the_scheduled_start() {
        assert!(matches!(
            decide(false, at(date(2026, 10, 20), 8, 30), |_| CalendarDay::Unreadable),
            Verdict::Start(ref r) if r.contains("fail-open")
        ));
    }

    #[test]
    fn test_account_id_parse() {
        let doc = r#"{"accountId" : "123456789012", "region": "ap-south-1"}"#;
        assert_eq!(
            account_id_from_identity_document(doc).as_deref(),
            Some("123456789012")
        );
        assert_eq!(account_id_from_identity_document("not json"), None);
        assert_eq!(
            account_id_from_identity_document(r#"{"accountId": ""}"#),
            None
        );
        assert_eq!(
            account_id_from_identity_document(r#"{"accountId": "12x4"}"#),
            None
        );
        assert_eq!(
            account_id_from_identity_document(r#"{"accountId": 12}"#),
            None
        );
        assert_eq!(account_id_from_identity_document("{}"), None);
    }

    #[test]
    fn test_names_match_the_script() {
        assert_eq!(
            holiday_stop_param("prod"),
            "/tickvault/prod/holiday-stop-date"
        );
        assert_eq!(
            alerts_topic_arn("ap-south-1", "123456789012", "prod"),
            "arn:aws:sns:ap-south-1:123456789012:tv-prod-alerts"
        );
        let message = holiday_stop_message("08:31 AM IST", "2026-10-02", true);
        assert!(message.starts_with("🔔 The trading box is switching itself OFF at 08:31 AM IST"));
        assert!(message.contains("today (2026-10-02) is an NSE holiday"));
        assert!(message.contains("If today IS a trading day, this is WRONG"));
        // 2026-10-20: the page says a second start today is honoured, and
        // promises it only when today's stop was recorded.
        assert!(message.contains("If you start it again today, it stays on"));
        let unrecorded = holiday_stop_message("08:31 AM IST", "2026-10-02", false);
        assert!(!unrecorded.contains("it stays on"));
        assert!(unrecorded.contains("could not be recorded"));
        assert!(unrecorded.contains("start it after 9:15 AM to keep it on"));
    }

    /// 2026-10-20: the gate's own marker decides whether a holiday-window boot
    /// is the first (stop) or a deliberate second start today (let through).
    #[test]
    fn test_decide_second_start_lets_only_a_same_day_restart_through() {
        let today = "2026-10-20";
        let value = |v: &str| MarkerRead::Value(v.to_string());
        // First boot ever: the parameter does not exist.
        assert_eq!(
            decide_second_start(&MarkerRead::Absent, today),
            SecondStart::Stop
        );
        // Second boot the same day: the gate stamped today before its stop.
        assert_eq!(
            decide_second_start(&value("2026-10-20"), today),
            SecondStart::AlreadyStoppedToday
        );
        assert_eq!(
            decide_second_start(&value(" 2026-10-20\n"), today),
            SecondStart::AlreadyStoppedToday
        );
        // An earlier holiday's marker (yesterday, or 2026-10-02): first boot today.
        for older in ["2026-10-19", "2026-10-02", ""] {
            assert_eq!(
                decide_second_start(&value(older), today),
                SecondStart::Stop,
                "marker {older:?}"
            );
        }
        // A failed read cannot tell a first boot from a second: stop, as before.
        assert_eq!(
            decide_second_start(&MarkerRead::Failed("AccessDenied".to_string()), today),
            SecondStart::Stop
        );
    }

    /// 2026-10-20: the marker put is retried exactly once, never more, so a
    /// dead SSM endpoint costs at most two call ceilings plus the pause, well
    /// inside the unit's 300 s `TimeoutStartSec`.
    #[test]
    fn test_marker_put_is_tried_twice_and_bounded() {
        assert_eq!(MARKER_PUT_ATTEMPTS, 2);
        let worst = u64::from(MARKER_PUT_ATTEMPTS) * AWS_CALL_TIMEOUT_SECS
            + MARKER_PUT_RETRY_DELAY_SECS
            + AWS_CALL_TIMEOUT_SECS; // the marker read before it
        assert_eq!(worst, 92);
        // The unit's TimeoutStartSec (300 s) bounds it from outside: pinned in
        // crates/app/tests/host_tuning_boot_wiring_guard.rs, which reads unit
        // files (no self-source scan here).
    }

    #[test]
    fn test_ist_now_is_ahead_of_utc_by_five_and_a_half_hours() {
        let delta = ist_now() - chrono::Utc::now().naive_utc();
        let secs = delta.num_seconds();
        assert!((19_799..=19_801).contains(&secs), "IST offset was {secs}s");
    }

    /// A one-request HTTP responder on 127.0.0.1: returns `status` and `body`,
    /// and hands back the raw request it saw.
    async fn serve_once(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0_u8; 4096];
            let n = socket.read(&mut buf).await.expect("read");
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.expect("write");
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (base, handle)
    }

    #[tokio::test]
    async fn test_imds_token_sends_the_ttl_header_and_trims() {
        let (base, seen) = serve_once("200 OK", " tok-123\n").await;
        let client = imds_client().expect("client");
        assert_eq!(imds_token(&client, &base).await.as_deref(), Some("tok-123"));
        let request = seen.await.expect("join").to_ascii_lowercase();
        assert!(request.starts_with("put /api/token "));
        assert!(request.contains("x-aws-ec2-metadata-token-ttl-seconds: 120"));
    }

    #[tokio::test]
    async fn test_imds_get_sends_the_token_and_rejects_errors_and_empty() {
        let (base, seen) = serve_once("200 OK", "i-0abc\n").await;
        let client = imds_client().expect("client");
        assert_eq!(
            imds_get(&client, &base, "tok", "meta-data/instance-id")
                .await
                .as_deref(),
            Some("i-0abc")
        );
        let request = seen.await.expect("join").to_ascii_lowercase();
        assert!(request.starts_with("get /meta-data/instance-id "));
        assert!(request.contains("x-aws-ec2-metadata-token: tok"));

        let (base, _seen) = serve_once("404 Not Found", "nope").await;
        assert_eq!(imds_get(&client, &base, "tok", "x").await, None);
        let (base, _seen) = serve_once("200 OK", "  ").await;
        assert_eq!(imds_get(&client, &base, "tok", "x").await, None);
    }

    #[tokio::test]
    async fn test_unreachable_imds_fails_open_without_touching_aws() {
        // Nothing listens on this port, so the token fetch fails and the gate
        // must let the app start without calling any AWS API.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        drop(listener);
        assert_eq!(stop_this_instance(&base).await, EXIT_LET_APP_START);
    }

    #[tokio::test]
    async fn test_imds_token_refused_fails_open() {
        let (base, _seen) = serve_once("403 Forbidden", "").await;
        assert_eq!(stop_this_instance(&base).await, EXIT_LET_APP_START);
    }
}
