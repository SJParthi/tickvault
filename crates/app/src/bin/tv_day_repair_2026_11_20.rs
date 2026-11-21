//! `tv-day-repair-2026-11-20` — runs the one-off repair of the held day
//! 2026-11-20 on the box (daily-universe Quote 63, plan item UTC-REPAIR,
//! rust-only lock §0.21).
//!
//! Run only by `.github/workflows/day-repair-2026-11-20.yml`. Every decision
//! about what is copied, dropped and rebuilt is the box's Rust
//! `tickvault day-repair` subcommand (`tickvault_app::day_repair`); this tool
//! only starts a stopped box, sends the one fixed SSM command
//! (`tickvault_app::day_repair_ssm`), prints what the box printed and stops
//! the box again if it started it.
//!
//! ```text
//! tv-day-repair-2026-11-20 --mode dry|apply|verify --sha <commit>
//!                          [--confirm REPAIR-2026-11-20] [--report <file>]
//! ```
//!
//! Refused before any AWS call: a bad argument, `apply` without the typed
//! confirmation, after 2026-11-30 IST, and 08:00 to 16:30 IST on a weekday
//! (the box re-checks with the real trading calendar). Refused before the box
//! is touched: a deploy run queued or running, the commit's binary not in
//! `s3://tv-prod-cold/deploys/<sha>/` (dispatch `deploy-aws` first), and for
//! `apply` no successful `dry` run of this workflow on the same commit.
//!
//! Exit codes: 0 done; 1 refused or failed.

#![cfg_attr(not(test), deny(unsafe_code))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stderr, clippy::dbg_macro)]
#![allow(clippy::print_stdout)] // APPROVED: one-off CLI; its output is the job log

use std::process::ExitCode;
use std::time::{Duration, Instant};

use aws_sdk_ec2::types::Filter;
use chrono::{Datelike, NaiveDate, Utc};
use tickvault_app::day_repair::{Mode, window_refusal};

const REGION: &str = "ap-south-1";
const INSTANCE_NAME_TAG: &str = "tv-prod-app";
const COLD_BUCKET: &str = "tv-prod-cold";
const WORKFLOW_FILE: &str = "day-repair-2026-11-20.yml";
const CONFIRM_WORD: &str = "REPAIR-2026-11-20";
const DEPLOY_WORKFLOWS: [&str; 3] = [
    "deploy-aws.yml",
    "deploy-aws-after-close.yml",
    "postmerge-catchup.yml",
];
const KEEP_ALIVE_PARAM: &str = "/tickvault/prod/keep-alive-until";
const KEEP_ALIVE_HOURS: i64 = 9;
const GITHUB_API: &str = "https://api.github.com";
const POLL: Duration = Duration::from_secs(15);
const STATE_WAIT: Duration = Duration::from_secs(20 * 60);
const SSM_ONLINE_WAIT: Duration = Duration::from_secs(20 * 60);
/// SSM agent timeout on the box: the copies, drops and rebuild of one day.
/// The box stops itself at 08:00 IST on a session day whatever this says.
const BOX_TIMEOUT_SECS: u64 = 8 * 3600;
/// How long this runner waits for the box (under the 6 h job limit). The box
/// keeps working past it; a later `verify` reads the result.
const RUNNER_WAIT: Duration = Duration::from_secs(5 * 3600 + 30 * 60);
/// Last IST date any mode may run (rust-only lock §0.21).
const LAST_RUN_DAY: (i32, u32, u32) = (2026, 11, 30);
const IST_OFFSET_SECS: i64 = 19_800;
/// The marker that opens the error of a runner that stopped waiting while the
/// box still works; nothing restores the keep-alive or stops the box then.
const STILL_RUNNING: &str = "STILL RUNNING ON THE BOX";

fn is_still_running(e: &str) -> bool {
    e.starts_with(STILL_RUNNING)
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn sdk<E: std::error::Error + 'static>(e: E) -> String {
    aws_sdk_ec2::error::DisplayErrorContext(&e).to_string()
}

fn now_ist() -> chrono::NaiveDateTime {
    (Utc::now() + chrono::TimeDelta::seconds(IST_OFFSET_SECS)).naive_utc()
}

fn is_commit_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The checks that need no AWS or GitHub call. `Err` names the refusal.
fn local_refusal(
    mode: Option<&str>,
    sha: &str,
    confirm: &str,
    now: chrono::NaiveDateTime,
) -> Result<Mode, String> {
    let mode = match mode {
        Some("dry") => Mode::Dry,
        Some("apply") => Mode::Apply,
        Some("verify") => Mode::Verify,
        other => {
            return Err(format!(
                "--mode must be dry, apply or verify, got {other:?}"
            ));
        }
    };
    if !is_commit_sha(sha) {
        return Err(format!(
            "--sha must be 40 lowercase hex digits, got {sha:?}"
        ));
    }
    if mode == Mode::Apply && confirm != CONFIRM_WORD {
        return Err(format!("apply needs --confirm {CONFIRM_WORD}"));
    }
    let (y, m, d) = LAST_RUN_DAY;
    let last = NaiveDate::from_ymd_opt(y, m, d).ok_or("bad last run day")?;
    if now.date() > last {
        return Err(format!("refused after {last} IST (rust-only lock §0.21)"));
    }
    // Weekdays count as session days here (a holiday refuses more, never
    // less); the box re-checks with the real calendar.
    let weekday = now.weekday().number_from_monday() <= 5;
    if let Some(why) = window_refusal(now, weekday) {
        return Err(why);
    }
    Ok(mode)
}

struct Instance {
    id: String,
    state: String,
}

struct Run {
    conf: aws_config::SdkConfig,
    http: reqwest::Client,
    /// The GitHub API base (`GITHUB_API`; a local stub in tests).
    github_api: String,
    mode: Mode,
    sha: String,
    repo: String,
    gh_token: String,
    log: Vec<String>,
    keep_alive_before: Option<String>,
    keep_alive_set: bool,
}

impl Run {
    fn say(&mut self, line: impl Into<String>) {
        let line = line.into();
        println!("{line}");
        self.log.push(line);
    }

    fn ec2(&self) -> aws_sdk_ec2::Client {
        aws_sdk_ec2::Client::new(&self.conf)
    }

    fn ssm(&self) -> aws_sdk_ssm::Client {
        aws_sdk_ssm::Client::new(&self.conf)
    }

    async fn instance(&self) -> Result<Instance, String> {
        let resp = self
            .ec2()
            .describe_instances()
            .filters(
                Filter::builder()
                    .name("tag:Name")
                    .values(INSTANCE_NAME_TAG)
                    .build(),
            )
            .send()
            .await
            .map_err(sdk)?;
        let mut found = Vec::new();
        for res in resp.reservations() {
            for i in res.instances() {
                let state = i
                    .state()
                    .and_then(|s| s.name())
                    .map_or("unknown", |n| n.as_str())
                    .to_string();
                if state == "terminated" || state == "shutting-down" {
                    continue;
                }
                found.push(Instance {
                    id: i.instance_id().unwrap_or_default().to_string(),
                    state,
                });
            }
        }
        match found.len() {
            1 => Ok(found.remove(0)),
            n => Err(format!(
                "{n} live instances tagged Name={INSTANCE_NAME_TAG}; expected 1"
            )),
        }
    }

    async fn wait_state(&self, want: &str) -> Result<(), String> {
        let deadline = Instant::now() + STATE_WAIT;
        loop {
            let state = self.instance().await?.state;
            if state == want {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "instance is {state}, not {want}, after {STATE_WAIT:?}"
                ));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn gh_get(&self, path: &str) -> Result<serde_json::Value, String> {
        self.http
            .get(format!("{}{path}", self.github_api))
            .bearer_auth(&self.gh_token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "tv-day-repair-2026-11-20")
            .send()
            .await
            .map_err(|e| format!("GitHub {path}: {e}"))?
            .error_for_status()
            .map_err(|e| format!("GitHub {path}: {e}"))?
            .json()
            .await
            .map_err(|e| format!("GitHub {path}: {e}"))
    }

    async fn busy_deploys(&self) -> Result<Vec<String>, String> {
        let mut busy = Vec::new();
        for wf in DEPLOY_WORKFLOWS {
            for st in ["queued", "in_progress", "waiting", "pending", "requested"] {
                let v = self
                    .gh_get(&format!(
                        "/repos/{}/actions/workflows/{wf}/runs?status={st}&per_page=5",
                        self.repo
                    ))
                    .await?;
                let n = v
                    .get("total_count")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                if n > 0 {
                    busy.push(format!("{wf} has {n} run(s) {st}"));
                }
            }
        }
        Ok(busy)
    }

    async fn dry_run_succeeded_on_this_commit(&self) -> Result<bool, String> {
        let v = self
            .gh_get(&format!(
                "/repos/{}/actions/workflows/{WORKFLOW_FILE}/runs?head_sha={}&status=success&per_page=50",
                self.repo, self.sha
            ))
            .await?;
        Ok(v.get("workflow_runs")
            .and_then(|r| r.as_array())
            .is_some_and(|runs| {
                runs.iter().any(|r| {
                    r.get("display_title")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| t.contains("(dry)"))
                })
            }))
    }

    /// `Ok(true)` when the commit's binary is in the deploy prefix; `Ok(false)`
    /// when S3 says it is not; `Err` when S3 could not answer (the box's own
    /// download then decides).
    async fn binary_deployed(&self) -> Result<bool, String> {
        let s3 = aws_sdk_s3::Client::new(&self.conf);
        match s3
            .head_object()
            .bucket(COLD_BUCKET)
            .key(format!("deploys/{}/tickvault", self.sha))
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                let not_found = e
                    .as_service_error()
                    .is_some_and(aws_sdk_s3::operation::head_object::HeadObjectError::is_not_found);
                if not_found {
                    Ok(false)
                } else {
                    Err(aws_sdk_s3::error::DisplayErrorContext(&e).to_string())
                }
            }
        }
    }

    async fn param(&self, name: &str) -> Option<String> {
        self.ssm()
            .get_parameter()
            .name(name)
            .send()
            .await
            .ok()
            .and_then(|r| r.parameter().and_then(|p| p.value()).map(str::to_string))
    }

    async fn put_param(&self, name: &str, value: &str) -> Result<(), String> {
        self.ssm()
            .put_parameter()
            .name(name)
            .value(value)
            .r#type(aws_sdk_ssm::types::ParameterType::String)
            .overwrite(true)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| aws_sdk_ssm::error::DisplayErrorContext(&e).to_string())
    }

    fn keep_alive_value(hours: i64) -> String {
        (now_ist() + chrono::TimeDelta::hours(hours))
            .format("%Y-%m-%dT%H:%M:%S+05:30")
            .to_string()
    }

    async fn keep_alive_on(&mut self) -> Result<(), String> {
        self.keep_alive_before = self.param(KEEP_ALIVE_PARAM).await;
        let v = Self::keep_alive_value(KEEP_ALIVE_HOURS);
        self.put_param(KEEP_ALIVE_PARAM, &v).await?;
        self.keep_alive_set = true;
        self.say(format!("keep-alive set to {v} (the curfew stands aside)"));
        Ok(())
    }

    async fn keep_alive_off(&mut self) {
        if !self.keep_alive_set {
            return;
        }
        let restore = self
            .keep_alive_before
            .clone()
            .unwrap_or_else(|| Self::keep_alive_value(-1));
        match self.put_param(KEEP_ALIVE_PARAM, &restore).await {
            Ok(()) => self.say(format!("keep-alive restored to {restore}")),
            Err(e) => self.say(format!(
                "WARNING keep-alive could not be restored ({e}); it expires on its own"
            )),
        }
        self.keep_alive_set = false;
    }

    async fn wait_ssm_online(&self, id: &str, after: chrono::DateTime<Utc>) -> Result<(), String> {
        let deadline = Instant::now() + SSM_ONLINE_WAIT;
        loop {
            let filter = aws_sdk_ssm::types::InstanceInformationStringFilter::builder()
                .key("InstanceIds")
                .values(id)
                .build()
                .map_err(|e| e.to_string())?;
            if let Ok(resp) = self
                .ssm()
                .describe_instance_information()
                .filters(filter)
                .send()
                .await
            {
                let online = resp.instance_information_list().iter().any(|i| {
                    i.ping_status().is_some_and(|p| p.as_str() == "Online")
                        && i.last_ping_date_time()
                            .is_some_and(|t| t.secs() > after.timestamp())
                });
                if online {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(format!("the SSM agent on {id} did not come online"));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Sends the one command and prints what the box printed. `Ok(true)` when
    /// the box exited 0; `Ok(false)` when it refused or failed.
    async fn send(&mut self, id: &str) -> Result<bool, String> {
        let commands = tickvault_app::day_repair_ssm::day_repair_commands(&self.sha, self.mode)
            .ok_or("command refused: the commit id is not 40 hex digits")?;
        let sent = self
            .ssm()
            .send_command()
            .instance_ids(id)
            .document_name("AWS-RunShellScript")
            .comment(format!("tv-day-repair-2026-11-20 {}", self.mode.as_str()))
            .parameters("commands", commands)
            .parameters("executionTimeout", vec![BOX_TIMEOUT_SECS.to_string()])
            .send()
            .await
            .map_err(|e| aws_sdk_ssm::error::DisplayErrorContext(&e).to_string())?;
        let cmd = sent
            .command()
            .and_then(|c| c.command_id())
            .ok_or("SSM returned no command id")?
            .to_string();
        self.say(format!("{} sent (SSM command {cmd})", self.mode.as_str()));
        let deadline = Instant::now() + RUNNER_WAIT;
        loop {
            tokio::time::sleep(POLL).await;
            let Ok(inv) = self
                .ssm()
                .get_command_invocation()
                .command_id(&cmd)
                .instance_id(id)
                .send()
                .await
            else {
                if Instant::now() >= deadline {
                    return Err(String::from("SSM invocation never appeared"));
                }
                continue;
            };
            let status = inv.status().map_or("", |s| s.as_str()).to_string();
            if matches!(
                status.as_str(),
                "Pending" | "InProgress" | "Delayed" | "Cancelling"
            ) {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "{STILL_RUNNING}: the box is {status} after this runner's wait; it keeps \
                         working (SSM command {cmd}); run `verify` later to read the result"
                    ));
                }
                continue;
            }
            for l in inv.standard_output_content().unwrap_or_default().lines() {
                self.say(format!("  box: {l}"));
            }
            let err = inv.standard_error_content().unwrap_or_default().to_string();
            for l in err.lines().take(40) {
                self.say(format!("  box stderr: {l}"));
            }
            self.say(format!("box status {status}, exit {}", inv.response_code()));
            return Ok(status == "Success");
        }
    }

    async fn run(&mut self) -> Result<(), String> {
        let busy = self.busy_deploys().await?;
        if !busy.is_empty() {
            return Err(format!("a deploy is queued or running: {busy:?}"));
        }
        if self.mode == Mode::Apply && !self.dry_run_succeeded_on_this_commit().await? {
            return Err(String::from(
                "apply needs a successful `dry` run of this workflow on the same commit first",
            ));
        }
        match self.binary_deployed().await {
            Ok(true) => self.say(format!("binary deploys/{}/tickvault found", self.sha)),
            Ok(false) => {
                return Err(format!(
                    "deploys/{}/tickvault is not in s3://{COLD_BUCKET}; dispatch deploy-aws on \
                     main first",
                    self.sha
                ));
            }
            Err(e) => self.say(format!(
                "WARNING could not check the binary ({e}); the box's download decides"
            )),
        }
        let inst = self.instance().await?;
        self.say(format!("instance {} is {}", inst.id, inst.state));
        let started_here = inst.state == "stopped";
        if started_here {
            if let Err(e) = self.keep_alive_on().await {
                self.say(format!(
                    "WARNING keep-alive not set ({e}); the box sets its own once the repair starts"
                ));
            }
            let before = Utc::now();
            self.ec2()
                .start_instances()
                .instance_ids(&inst.id)
                .send()
                .await
                .map_err(sdk)?;
            self.wait_state("running").await?;
            self.say(format!("started {}", inst.id));
            self.wait_ssm_online(&inst.id, before).await?;
        } else if inst.state != "running" {
            return Err(format!(
                "instance is {}; dispatch again once it settles",
                inst.state
            ));
        }
        // A deploy may have started meanwhile (the box start can dispatch one
        // on a weekday): never send into it (2026-11-21 review).
        let result = match self.busy_deploys().await {
            Ok(busy) if !busy.is_empty() => {
                Err(format!("a deploy started after the box did: {busy:?}"))
            }
            Ok(_) => self.send(&inst.id).await,
            Err(e) => Err(e),
        };
        let still_running = result.as_ref().is_err_and(|e| is_still_running(e));
        // Never stop the box inside a trading-day window (the box restarts
        // the app itself when its repair stops at 08:00), and never while the
        // repair is still running there.
        let in_session_window = local_refusal(Some("dry"), &self.sha, "", now_ist()).is_err();
        if started_here && !still_running && !in_session_window {
            match self
                .ec2()
                .stop_instances()
                .instance_ids(&inst.id)
                .send()
                .await
            {
                Ok(_) => match self.wait_state("stopped").await {
                    Ok(()) => self.say(format!("stopped {}", inst.id)),
                    Err(e) => self.say(format!("WARNING stop not confirmed: {e}")),
                },
                Err(e) => self.say(format!("WARNING stop failed: {}", sdk(e))),
            }
        }
        if !still_running {
            self.keep_alive_off().await;
        }
        match result? {
            true => Ok(()),
            false => Err(String::from("the box refused or failed (see above)")),
        }
    }
}

async fn real_main(args: &[String]) -> Result<Vec<String>, (String, Vec<String>)> {
    let sha = flag(args, "--sha").unwrap_or_default();
    let confirm = flag(args, "--confirm").unwrap_or_default();
    let mode = local_refusal(flag(args, "--mode").as_deref(), &sha, &confirm, now_ist())
        .map_err(|e| (e, Vec::new()))?;
    let repo = std::env::var("GITHUB_REPOSITORY").unwrap_or_default();
    let gh_token = std::env::var("GH_TOKEN").unwrap_or_default();
    if repo.is_empty() || gh_token.is_empty() {
        return Err((
            String::from("GITHUB_REPOSITORY and GH_TOKEN must be set"),
            Vec::new(),
        ));
    }
    let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(REGION))
        .load()
        .await;
    let mut run = Run {
        conf,
        http: reqwest::Client::new(),
        github_api: GITHUB_API.to_owned(),
        mode,
        sha,
        repo,
        gh_token,
        log: Vec::new(),
        keep_alive_before: None,
        keep_alive_set: false,
    };
    run.say(format!(
        "day repair {} for 2026-11-20 on commit {}",
        mode.as_str(),
        run.sha
    ));
    match run.run().await {
        Ok(()) => Ok(run.log),
        Err(e) => {
            // The repair still runs on the box: its keep-alive must stand
            // (2026-11-21 review: restoring it let the curfew stop the box).
            if !is_still_running(&e) {
                run.keep_alive_off().await;
            }
            Err((e, run.log))
        }
    }
}

fn write_report(path: Option<String>, ok: bool, log: &[String]) {
    let Some(path) = path else { return };
    let mut body = format!(
        "# Day repair 2026-11-20: {}\n\n```text\n",
        if ok { "OK" } else { "REFUSED OR FAILED" }
    );
    for l in log {
        body.push_str(l);
        body.push('\n');
    }
    body.push_str("```\n");
    if let Err(e) = std::fs::write(&path, body) {
        println!("WARNING could not write the report {path}: {e}");
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let report = flag(&args, "--report");
    match real_main(&args).await {
        Ok(log) => {
            write_report(report, true, &log);
            println!("DONE");
            ExitCode::SUCCESS
        }
        Err((why, mut log)) => {
            println!("REFUSED OR FAILED: {why}");
            log.push(format!("REFUSED OR FAILED: {why}"));
            write_report(report, false, &log);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "d1b4835470000000000000000000000000000000";

    fn ist(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .and_then(|x| x.and_hms_opt(h, mi, 0))
            .expect("valid")
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn test_flag_reads_the_value_after_the_name() {
        let a = args(&["bin", "--mode", "dry", "--sha", SHA]);
        assert_eq!(flag(&a, "--mode").as_deref(), Some("dry"));
        assert_eq!(flag(&a, "--sha").as_deref(), Some(SHA));
        assert_eq!(flag(&a, "--confirm"), None);
    }

    #[test]
    fn test_local_refusal_accepts_a_saturday_evening() {
        let sat = ist(2026, 11, 21, 19, 30);
        assert_eq!(local_refusal(Some("dry"), SHA, "", sat), Ok(Mode::Dry));
        assert_eq!(
            local_refusal(Some("apply"), SHA, CONFIRM_WORD, sat),
            Ok(Mode::Apply)
        );
        assert_eq!(
            local_refusal(Some("verify"), SHA, "", sat),
            Ok(Mode::Verify)
        );
    }

    #[test]
    fn test_local_refusal_refuses_bad_input_and_closed_windows() {
        let sun = ist(2026, 11, 22, 10, 0);
        assert!(local_refusal(Some("nuke"), SHA, "", sun).is_err());
        assert!(local_refusal(None, SHA, "", sun).is_err());
        assert!(local_refusal(Some("dry"), "abc", "", sun).is_err());
        assert!(local_refusal(Some("dry"), &SHA.to_uppercase(), "", sun).is_err());
        assert!(local_refusal(Some("apply"), SHA, "", sun).is_err());
        assert!(local_refusal(Some("apply"), SHA, "yes", sun).is_err());
        // Monday 09:00 IST: inside the trading-day refusal window.
        assert!(local_refusal(Some("dry"), SHA, "", ist(2026, 11, 23, 9, 0)).is_err());
        // After the last run day.
        assert!(local_refusal(Some("dry"), SHA, "", ist(2026, 12, 1, 20, 0)).is_err());
    }

    #[test]
    fn test_still_running_marker_is_only_the_runner_timeout() {
        assert!(is_still_running(&format!(
            "{STILL_RUNNING}: the box is InProgress"
        )));
        assert!(!is_still_running("the box refused or failed (see above)"));
        assert!(!is_still_running("instance is still pending"));
    }

    #[tokio::test]
    async fn test_real_main_refuses_bad_input_before_any_aws_call() {
        let r = real_main(&args(&["bin", "--mode", "dry", "--sha", "nope"])).await;
        assert!(r.is_err_and(|(why, log)| why.contains("--sha") && log.is_empty()));
    }
}

/// The GitHub checks against a local stub API (2026-11-21): busy deploys,
/// the dry-run-on-this-commit rule, the error arm, and the report file.
#[cfg(test)]
mod github_stub_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const SHA: &str = "d1b4835470000000000000000000000000000000";

    /// A stub that answers each request path from `routes` (first substring
    /// match; no match answers 404) and records the paths.
    async fn stub(
        routes: Vec<(String, u16, String)>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = std::sync::Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut part = [0u8; 4096];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut part).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&part[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&req).into_owned();
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split(' ').nth(1))
                    .unwrap_or_default()
                    .to_owned();
                let auth_ok = head
                    .to_ascii_lowercase()
                    .contains("authorization: bearer tok");
                log.lock().expect("lock").push(path.clone());
                let (status, body) = if auth_ok {
                    routes
                        .iter()
                        .find(|(k, _, _)| path.contains(k.as_str()))
                        .map_or((404, String::from("{}")), |(_, s, b)| (*s, b.clone()))
                } else {
                    (401, String::from("{}"))
                };
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn run(github_api: String, mode: Mode, token: &str) -> Run {
        let conf = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(REGION))
            .build();
        Run {
            conf,
            http: reqwest::Client::new(),
            github_api,
            mode,
            sha: SHA.to_owned(),
            repo: "o/r".to_owned(),
            gh_token: token.to_owned(),
            log: Vec::new(),
            keep_alive_before: None,
            keep_alive_set: false,
        }
    }

    fn total(n: u64) -> String {
        format!("{{\"total_count\":{n}}}")
    }

    #[tokio::test]
    async fn test_busy_deploys_names_each_busy_workflow_and_status() {
        let mut routes = vec![(
            "deploy-aws.yml/runs?status=in_progress".to_owned(),
            200,
            total(1),
        )];
        routes.push((
            "postmerge-catchup.yml/runs?status=queued".to_owned(),
            200,
            total(2),
        ));
        routes.push(("/runs?status=".to_owned(), 200, total(0)));
        let (api, seen) = stub(routes).await;
        let busy = run(api, Mode::Dry, "tok")
            .busy_deploys()
            .await
            .expect("busy");
        assert_eq!(
            busy,
            vec![
                "deploy-aws.yml has 1 run(s) in_progress".to_owned(),
                "postmerge-catchup.yml has 2 run(s) queued".to_owned(),
            ]
        );
        // Three workflows, five statuses each.
        assert_eq!(seen.lock().expect("lock").len(), 15);
    }

    #[tokio::test]
    async fn test_dry_run_rule_needs_a_successful_dry_run_on_this_commit() {
        let path = format!("{WORKFLOW_FILE}/runs?head_sha={SHA}&status=success");
        let with_dry = String::from(
            "{\"workflow_runs\":[{\"display_title\":\"day-repair-2026-11-20 (verify)\"},{\"display_title\":\"day-repair-2026-11-20 (dry)\"}]}",
        );
        let (api, seen) = stub(vec![(path.clone(), 200, with_dry)]).await;
        assert!(
            run(api, Mode::Apply, "tok")
                .dry_run_succeeded_on_this_commit()
                .await
                .expect("ok")
        );
        assert!(seen.lock().expect("lock")[0].contains(&format!("head_sha={SHA}")));
        let only_verify = String::from(
            "{\"workflow_runs\":[{\"display_title\":\"day-repair-2026-11-20 (verify)\"}]}",
        );
        let (api, _) = stub(vec![(path.clone(), 200, only_verify)]).await;
        assert!(
            !run(api, Mode::Apply, "tok")
                .dry_run_succeeded_on_this_commit()
                .await
                .expect("ok")
        );
        let (api, _) = stub(vec![(path, 200, String::from("{}"))]).await;
        assert!(
            !run(api, Mode::Apply, "tok")
                .dry_run_succeeded_on_this_commit()
                .await
                .expect("ok")
        );
    }

    #[tokio::test]
    async fn test_gh_get_reports_status_and_bad_json() {
        let (api, _) = stub(vec![
            ("/bad".to_owned(), 200, String::from("not json")),
            ("/ok".to_owned(), 200, String::from("{\"a\":1}")),
        ])
        .await;
        let r = run(api.clone(), Mode::Dry, "tok");
        assert_eq!(r.gh_get("/ok").await.expect("ok")["a"], 1);
        assert!(r.gh_get("/bad").await.expect_err("json").contains("/bad"));
        assert!(r.gh_get("/missing").await.expect_err("404").contains("404"));
        // A wrong token is refused by the API and named.
        let e = run(api, Mode::Dry, "nope")
            .gh_get("/ok")
            .await
            .expect_err("401");
        assert!(e.contains("401"), "{e}");
        // A busy-deploy check that cannot read GitHub refuses (never "not busy").
        let (api, _) = stub(vec![]).await;
        assert!(run(api, Mode::Dry, "tok").busy_deploys().await.is_err());
    }

    #[test]
    fn test_say_logs_and_write_report_writes_the_log() {
        let mut r = run(String::new(), Mode::Dry, "tok");
        r.say("first line");
        r.say(String::from("second"));
        assert_eq!(r.log, vec!["first line".to_owned(), "second".to_owned()]);
        let dir = std::env::temp_dir().join(format!(
            "tv-day-repair-report-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("report.md");
        write_report(Some(path.display().to_string()), true, &r.log);
        let body = std::fs::read_to_string(&path).expect("report");
        assert!(body.starts_with("# Day repair 2026-11-20: OK"));
        assert!(body.contains("first line\nsecond\n"));
        write_report(Some(path.display().to_string()), false, &[]);
        assert!(
            std::fs::read_to_string(&path)
                .expect("report")
                .contains("REFUSED OR FAILED")
        );
        // No path: nothing written; an unwritable path only warns.
        write_report(None, true, &r.log);
        write_report(
            Some(dir.join("no/such/dir/r.md").display().to_string()),
            true,
            &r.log,
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_keep_alive_value_is_an_ist_instant_hours_ahead() {
        let v = Run::keep_alive_value(9);
        assert!(v.ends_with("+05:30"), "{v}");
        let at =
            chrono::NaiveDateTime::parse_from_str(&v[..19], "%Y-%m-%dT%H:%M:%S").expect("parse");
        let ahead = (at - now_ist()).num_minutes();
        assert!((539..=540).contains(&ahead), "{ahead}");
        let past = Run::keep_alive_value(-1);
        let at =
            chrono::NaiveDateTime::parse_from_str(&past[..19], "%Y-%m-%dT%H:%M:%S").expect("parse");
        assert!(at < now_ist());
    }

    #[tokio::test]
    async fn test_real_main_needs_repository_and_token_after_local_checks() {
        // A Saturday-evening-shaped call that passes the local checks still
        // refuses without GITHUB_REPOSITORY and GH_TOKEN (cleared here for
        // this process's test thread only by reading, never writing, env).
        if std::env::var("GITHUB_REPOSITORY").is_ok() || std::env::var("GH_TOKEN").is_ok() {
            return;
        }
        let local = local_refusal(Some("dry"), SHA, "", now_ist());
        let r = real_main(&["bin", "--mode", "dry", "--sha", SHA].map(String::from)).await;
        match local {
            // Inside a closed window the local refusal answers first.
            Err(why) => assert!(r.is_err_and(|(e, _)| e == why)),
            Ok(_) => assert!(r.is_err_and(|(e, _)| e.contains("GITHUB_REPOSITORY"))),
        }
    }
}
