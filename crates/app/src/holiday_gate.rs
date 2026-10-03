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
//! Exit codes, consumed by the oneshot unit and journald:
//! - `0` = let the app start (trading day, override, or fail-open);
//! - `1` = holiday verdict, stop issued (the app must not start).
//!
//! Before the stop it stamps today's IST date into
//! `/tickvault/<env>/holiday-stop-date` (so the start-watchdog, the autopilot
//! and the market-hours alarm gate recognise the stop as intentional) and
//! publishes one plain-English line to `tv-<env>-alerts`. Both are fail-open:
//! a failure is logged and the stop still proceeds.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

/// The first argument that selects this subcommand.
pub const HOLIDAY_GATE_SUBCOMMAND: &str = "holiday-gate";

/// The operator's "run today anyway" marker.
const OVERRIDE_MARKER_PATH: &str = "/opt/tickvault/ALLOW_HOLIDAY_RUN";

/// `--check-trading-day` contract (see `main.rs`): 0 = trading day,
/// 75 = weekend or NSE holiday, anything else = could not decide.
const TRADING_DAY_EXIT: i32 = 0;
const HOLIDAY_EXIT: i32 = 75;

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
    /// Today is definitively not a trading day: stop the instance.
    Stop,
}

/// Pure decision: the override marker wins, then only exit 75 stops.
fn decide(override_present: bool, check_code: impl FnOnce() -> i32) -> Verdict {
    if override_present {
        return Verdict::Start(format!(
            "override marker present ({OVERRIDE_MARKER_PATH}) — skipping holiday check, app starts"
        ));
    }
    match check_code() {
        TRADING_DAY_EXIT => Verdict::Start("trading day — app start proceeds".to_string()),
        HOLIDAY_EXIT => Verdict::Stop,
        // 70 (config/calendar load error) or anything unexpected: fail open.
        other => Verdict::Start(format!(
            "check-trading-day returned {other} (not a definitive holiday) — fail-open, app starts"
        )),
    }
}

/// Run the gate. `check_trading_day` is the app's own calendar check, called
/// only when no override marker is present.
pub async fn run(check_trading_day: impl FnOnce() -> i32) -> i32 {
    match decide(Path::new(OVERRIDE_MARKER_PATH).exists(), check_trading_day) {
        Verdict::Start(reason) => {
            emit(&reason);
            EXIT_LET_APP_START
        }
        Verdict::Stop => {
            emit("NON-TRADING day verdict (exit 75) — stopping this instance");
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

fn alerts_topic_arn(region: &str, account: &str, environment: &str) -> String {
    format!("arn:aws:sns:{region}:{account}:tv-{environment}-alerts")
}

fn holiday_stop_message(ist_now: &str, today_ist: &str) -> String {
    format!(
        "🔔 The trading box is switching itself OFF at {ist_now}: today ({today_ist}) is an NSE holiday, so no market data will be captured and no bill is run up. If today IS a trading day, this is WRONG — start the box from the console and check the holiday list."
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

/// Resolve the instance from IMDS, stamp the marker, page, and stop. Returns
/// `0` (fail-open) when the instance cannot be resolved safely, otherwise `1`
/// whether or not the stop call itself succeeded, as the script did: the OS is
/// expected to go down, and a failed stop is retried on the next boot.
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

    // The intentional-stop marker, BEFORE the stop: without it the
    // start-watchdog, the autopilot and the market-hours alarm gate fight the
    // stop all day. A stale marker is harmless (consumers compare it against
    // today's IST date).
    let marker = holiday_stop_param(&environment);
    let ssm = aws_sdk_ssm::Client::new(&config);
    let put = ssm
        .put_parameter()
        .name(&marker)
        .value(&today_ist)
        .r#type(aws_sdk_ssm::types::ParameterType::String)
        .overwrite(true)
        .send();
    if with_timeout(put).await {
        emit(&format!(
            "holiday-stop marker written: {marker} = {today_ist}"
        ));
    } else {
        emit(&format!(
            "holiday-stop marker put FAILED ({marker}) — restarters may fight today's stop"
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
        emit("aws ec2 stop-instances call failed (will retry on next boot)");
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

    #[test]
    fn test_override_marker_wins_and_skips_the_check() {
        let verdict = decide(true, || panic!("the calendar must not be consulted"));
        assert!(matches!(verdict, Verdict::Start(ref r) if r.contains("override marker")));
    }

    #[test]
    fn test_only_exit_75_stops() {
        assert!(matches!(decide(false, || 0), Verdict::Start(ref r) if r.contains("trading day")));
        assert_eq!(decide(false, || 75), Verdict::Stop);
        for code in [70, 1, 2, -1, 74, 76, 255] {
            assert!(
                matches!(decide(false, || code), Verdict::Start(ref r) if r.contains("fail-open")),
                "exit {code} must fail open"
            );
        }
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
        let message = holiday_stop_message("08:31 AM IST", "2026-10-02");
        assert!(message.starts_with("🔔 The trading box is switching itself OFF at 08:31 AM IST"));
        assert!(message.contains("today (2026-10-02) is an NSE holiday"));
        assert!(message.contains("If today IS a trading day, this is WRONG"));
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
