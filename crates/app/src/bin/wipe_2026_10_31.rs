//! `tv-wipe-2026-10-31` — the one-off full market-data wipe (plan item 67;
//! operator Quote 35, brought forward to the night of Tuesday 2026-10-27 by
//! Quote 42; the name stays).
//!
//! Run only by `.github/workflows/wipe-2026-10-31.yml`. What is deleted and
//! kept, the run window and the reports are in
//! `tickvault_app::wipe_2026_10_31`; the box half is `tickvault wipe-box`
//! (`tickvault_app::wipe_box`), sent over SSM as one fixed command
//! (`tickvault_app::wipe_ssm`).
//!
//! ```text
//! tv-wipe-2026-10-31 --mode dry|apply --phase preflight|box|s3|snapshot|cloud
//!                    --sha <commit> [--confirm WIPE-BEFORE-2026-11-01] [--report <file>]
//! ```
//!
//! | Phase | Credentials | Dry | Apply |
//! |---|---|---|---|
//! | `preflight` | stored keys | instance facts | same (never deletes) |
//! | `box` | stored keys | box inventory over SSM (starts a stopped box, stops it again) | app stopped, old files and partitions deleted on the box, box stopped |
//! | `s3` | one-off role | every version under the exact prefixes, what goes and what stays | deletes by version id, lists again, fails unless nothing in scope is left |
//! | `snapshot` | one-off role | the rebuild's old-root snapshot | deletes it, describes again; before 16:30 IST 28 Oct it is KEPT (inside `cloud`) or refused (alone) |
//!
//! An apply needs the typed confirmation, the 18:00 27 Oct – 08:00 1 Nov IST
//! window (never 09:00–15:45), no deploy queued or running, and a successful `dry all` run of
//! the workflow on the same commit. The S3 and snapshot applies also need the
//! instance stopped (so the app cannot be writing).
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
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use chrono::Utc;
use tickvault_app::wipe_2026_10_31::{
    COLD_BUCKET, DELETE_BATCH_MAX, Mode, OLD_ROOT_TAG_KEY, Phase, S3Plan, SNAPSHOT_DELETE_FROM_IST,
    SNAPSHOT_TAG_KEY, SNAPSHOT_TAG_VALUE, Verdict, VersionEntry, is_commit_sha, now_ist,
    parse_box_report, run_refusal, s3_list_prefixes, snapshot_delete_refusal, snapshot_verdict,
};

const REGION: &str = "ap-south-1";
const INSTANCE_NAME_TAG: &str = "tv-prod-app";
const WORKFLOW_FILE: &str = "wipe-2026-10-31.yml";
const DEPLOY_WORKFLOWS: [&str; 3] = [
    "deploy-aws.yml",
    "deploy-aws-after-close.yml",
    "postmerge-catchup.yml",
];
const KEEP_ALIVE_PARAM: &str = "/tickvault/prod/keep-alive-until";
const KEEP_ALIVE_HOURS: i64 = 6;
const GITHUB_API: &str = "https://api.github.com";
const POLL: Duration = Duration::from_secs(15);
const STATE_WAIT: Duration = Duration::from_secs(20 * 60);
const SSM_ONLINE_WAIT: Duration = Duration::from_secs(20 * 60);
/// SSM agent timeout for the box phase (file deletes, QuestDB truncates and
/// the waits for WAL apply and drop visibility).
const BOX_PHASE_TIMEOUT_SECS: u64 = 3 * 3600;

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn sdk<E: std::error::Error + 'static>(e: E) -> String {
    aws_sdk_ec2::error::DisplayErrorContext(&e).to_string()
}

/// `(instance id, state, account id)` of the one live `tv-prod-app`.
struct Instance {
    id: String,
    state: String,
    account: String,
}

struct Run {
    conf: aws_config::SdkConfig,
    http: reqwest::Client,
    mode: Mode,
    sha: String,
    repo: String,
    gh_token: String,
    log: Vec<String>,
    keep_alive_before: Option<String>,
    keep_alive_set: bool,
    /// The snapshot phase kept the old-root snapshot because its Quote 42
    /// time had not come.
    snapshot_deferred: bool,
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
                    account: res.owner_id().unwrap_or_default().to_string(),
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

    /// Writes `account=<id>` to the job's outputs for the next job's role ARN.
    fn output_account(&self, account: &str) {
        if account.len() != 12 || !account.bytes().all(|b| b.is_ascii_digit()) {
            return;
        }
        if let Ok(path) = std::env::var("GITHUB_OUTPUT") {
            let _ = std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .and_then(|mut f| {
                    std::io::Write::write_all(&mut f, format!("account={account}\n").as_bytes())
                });
        }
    }

    // ------------------------------ GitHub -------------------------------

    async fn gh_get(&self, path: &str) -> Result<serde_json::Value, String> {
        self.http
            .get(format!("{GITHUB_API}{path}"))
            .bearer_auth(&self.gh_token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "tv-wipe-2026-10-31")
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
                        .is_some_and(|t| t.contains("(dry all)"))
                })
            }))
    }

    // ------------------------------- SSM ---------------------------------

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
        tickvault_app::wipe_2026_10_31::now_ist(Utc::now() + chrono::TimeDelta::hours(hours))
            .format("%Y-%m-%dT%H:%M:%S+05:30")
            .to_string()
    }

    async fn keep_alive_on(&mut self) -> Result<(), String> {
        if !self.keep_alive_set {
            self.keep_alive_before = self.param(KEEP_ALIVE_PARAM).await;
        }
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

    async fn send_box(&mut self, id: &str) -> Result<bool, String> {
        let commands = tickvault_app::wipe_ssm::wipe_box_commands(&self.sha, self.mode)
            .ok_or("command refused: the commit id is not 40 hex digits")?;
        let sent = self
            .ssm()
            .send_command()
            .instance_ids(id)
            .document_name("AWS-RunShellScript")
            .comment(format!("tv-wipe-2026-10-31 box {}", self.mode.as_str()))
            .parameters("commands", commands)
            .parameters("executionTimeout", vec![BOX_PHASE_TIMEOUT_SECS.to_string()])
            .send()
            .await
            .map_err(|e| aws_sdk_ssm::error::DisplayErrorContext(&e).to_string())?;
        let cmd = sent
            .command()
            .and_then(|c| c.command_id())
            .ok_or("SSM returned no command id")?
            .to_string();
        self.say(format!(
            "box {} sent (SSM command {cmd})",
            self.mode.as_str()
        ));
        let deadline = Instant::now() + Duration::from_secs(BOX_PHASE_TIMEOUT_SECS + 900);
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
                    return Err(format!("box phase still {status} past its timeout"));
                }
                continue;
            }
            let out = inv
                .standard_output_content()
                .unwrap_or_default()
                .to_string();
            let Some(rep) = parse_box_report(&out) else {
                return Err(format!(
                    "SSM status {status}; no report line. stderr: {}",
                    inv.standard_error_content()
                        .unwrap_or_default()
                        .chars()
                        .take(800)
                        .collect::<String>()
                ));
            };
            for l in &rep.lines {
                self.say(format!("  box: {l}"));
            }
            for p in &rep.problems {
                self.say(format!("  box PROBLEM: {p}"));
            }
            if rep.problems_total > rep.problems.len() {
                self.say(format!(
                    "  box: {} more problems in the box's report under /var/tmp",
                    rep.problems_total - rep.problems.len()
                ));
            }
            return Ok(rep.ok && status == "Success");
        }
    }

    // ------------------------------ Phases -------------------------------

    async fn preflight(&mut self) -> Result<(), String> {
        let inst = self.instance().await?;
        self.say(format!("instance {} is {}", inst.id, inst.state));
        self.output_account(&inst.account);
        Ok(())
    }

    async fn box_phase(&mut self) -> Result<(), String> {
        let inst = self.instance().await?;
        self.output_account(&inst.account);
        self.say(format!("instance {} is {}", inst.id, inst.state));
        let started_here = inst.state == "stopped";
        if started_here || self.mode == Mode::Apply {
            self.keep_alive_on().await?;
        }
        if started_here {
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
        let result = self.send_box(&inst.id).await;
        if started_here || self.mode == Mode::Apply {
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
        self.keep_alive_off().await;
        match result? {
            true => Ok(()),
            false => Err(String::from("the box reported problems (see above)")),
        }
    }

    async fn require_stopped_for_apply(&mut self) -> Result<(), String> {
        if self.mode == Mode::Dry {
            return Ok(());
        }
        let inst = self.instance().await?;
        if inst.state != "stopped" {
            return Err(format!(
                "the instance is {}; the cloud deletes run only with the box (and so the app) stopped",
                inst.state
            ));
        }
        Ok(())
    }

    async fn list_all(&self, s3: &aws_sdk_s3::Client) -> Result<Vec<VersionEntry>, String> {
        let mut out = Vec::new();
        for prefix in s3_list_prefixes() {
            let mut key_marker: Option<String> = None;
            let mut version_marker: Option<String> = None;
            loop {
                let resp = s3
                    .list_object_versions()
                    .bucket(COLD_BUCKET)
                    .prefix(&prefix)
                    .set_key_marker(key_marker.clone())
                    .set_version_id_marker(version_marker.clone())
                    .send()
                    .await
                    .map_err(|e| format!("listing {prefix}: {}", sdk(e)))?;
                for v in resp.versions() {
                    out.push(VersionEntry {
                        key: v.key().unwrap_or_default().to_string(),
                        version_id: v.version_id().unwrap_or("null").to_string(),
                        bytes: v.size().unwrap_or(0),
                        is_delete_marker: false,
                        last_modified_unix_secs: v.last_modified().map(|t| t.secs()),
                    });
                }
                for m in resp.delete_markers() {
                    out.push(VersionEntry {
                        key: m.key().unwrap_or_default().to_string(),
                        version_id: m.version_id().unwrap_or("null").to_string(),
                        bytes: 0,
                        is_delete_marker: true,
                        last_modified_unix_secs: m.last_modified().map(|t| t.secs()),
                    });
                }
                if resp.is_truncated() != Some(true) {
                    break;
                }
                key_marker = resp.next_key_marker().map(str::to_string);
                version_marker = resp.next_version_id_marker().map(str::to_string);
                if key_marker.is_none() && version_marker.is_none() {
                    return Err(format!(
                        "listing {prefix} was truncated with no next marker"
                    ));
                }
            }
        }
        Ok(out)
    }

    async fn delete_batch(
        s3: &aws_sdk_s3::Client,
        batch: &[VersionEntry],
    ) -> Result<Vec<String>, String> {
        let mut objects = Vec::with_capacity(batch.len());
        for e in batch {
            objects.push(
                ObjectIdentifier::builder()
                    .key(&e.key)
                    .version_id(&e.version_id)
                    .build()
                    .map_err(|e| e.to_string())?,
            );
        }
        let delete = Delete::builder()
            .set_objects(Some(objects))
            .quiet(true)
            .build()
            .map_err(|e| e.to_string())?;
        let resp = s3
            .delete_objects()
            .bucket(COLD_BUCKET)
            .delete(delete)
            .send()
            .await
            .map_err(sdk)?;
        Ok(resp
            .errors()
            .iter()
            .map(|err| {
                format!(
                    "{} (version {}): {} {}",
                    err.key().unwrap_or_default(),
                    err.version_id().unwrap_or_default(),
                    err.code().unwrap_or_default(),
                    err.message().unwrap_or_default()
                )
            })
            .collect())
    }

    async fn s3_phase(&mut self) -> Result<(), String> {
        self.require_stopped_for_apply().await?;
        let s3 = aws_sdk_s3::Client::new(&self.conf);
        let plan = S3Plan::from_entries(self.list_all(&s3).await?);
        let title = match self.mode {
            Mode::Dry => "S3 DRY RUN — nothing deleted",
            Mode::Apply => "S3 APPLY — before deleting",
        };
        let report = plan.render(title);
        self.say(report);
        if self.mode == Mode::Dry {
            return Ok(());
        }
        let mut errors = Vec::new();
        let mut sent = 0usize;
        for batch in plan.delete.chunks(DELETE_BATCH_MAX) {
            match Self::delete_batch(&s3, batch).await {
                Ok(errs) => errors.extend(errs),
                Err(e) => errors.push(format!("batch of {} failed: {e}", batch.len())),
            }
            sent += batch.len();
            println!("sent {sent} of {}", plan.delete.len());
        }
        self.say(format!("sent {sent} deletes"));
        let after = S3Plan::from_entries(self.list_all(&s3).await?);
        self.say(after.render("S3 AFTER — what remains in scope"));
        for e in errors.iter().take(50) {
            self.say(format!("  delete error: {e}"));
        }
        if errors.is_empty() && after.delete.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{} delete error(s); {} entries still in scope",
                errors.len(),
                after.delete.len()
            ))
        }
    }

    async fn rebuild_snapshots(&self) -> Result<Vec<(String, Verdict, i32)>, String> {
        let resp = self
            .ec2()
            .describe_snapshots()
            .owner_ids("self")
            .filters(
                Filter::builder()
                    .name(format!("tag:{SNAPSHOT_TAG_KEY}"))
                    .values(SNAPSHOT_TAG_VALUE)
                    .build(),
            )
            .send()
            .await
            .map_err(sdk)?;
        Ok(resp
            .snapshots()
            .iter()
            .map(|s| {
                let tags: Vec<(String, String)> = s
                    .tags()
                    .iter()
                    .map(|t| {
                        (
                            t.key().unwrap_or_default().to_string(),
                            t.value().unwrap_or_default().to_string(),
                        )
                    })
                    .collect();
                (
                    s.snapshot_id().unwrap_or_default().to_string(),
                    snapshot_verdict(&tags, true),
                    s.volume_size().unwrap_or(0),
                )
            })
            .collect())
    }

    async fn snapshot_phase(&mut self) -> Result<(), String> {
        self.require_stopped_for_apply().await?;
        // Reported, never deleted: Quote 35 names the snapshot, not the volume.
        if let Ok(v) = self
            .ec2()
            .describe_volumes()
            .filters(
                Filter::builder()
                    .name(format!("tag:{OLD_ROOT_TAG_KEY}"))
                    .values(SNAPSHOT_TAG_VALUE)
                    .build(),
            )
            .send()
            .await
        {
            for vol in v.volumes() {
                self.say(format!(
                    "old root volume {} ({} GB, {}) is KEPT: Quote 35 does not name it",
                    vol.volume_id().unwrap_or_default(),
                    vol.size().unwrap_or(0),
                    vol.state().map_or("", |s| s.as_str())
                ));
            }
        }
        let snaps = self.rebuild_snapshots().await?;
        let doomed: Vec<String> = snaps
            .iter()
            .filter(|s| s.1 == Verdict::Delete)
            .map(|s| s.0.clone())
            .collect();
        for (id, verdict, gb) in &snaps {
            self.say(format!("snapshot {id} ({gb} GB): {verdict:?}"));
        }
        if doomed.is_empty() {
            self.say("no old-root snapshot found; nothing to delete");
            return Ok(());
        }
        if self.mode == Mode::Dry {
            self.say(format!("dry: would delete {}", doomed.join(", ")));
            return Ok(());
        }
        if let Some(why) = snapshot_delete_refusal(now_ist(Utc::now())) {
            // Quote 42: the S3 delete of the same `cloud` run still stands;
            // only the snapshot waits. A `--phase snapshot` run alone is
            // refused before any AWS call (`real_main`).
            self.say(format!("snapshot NOT deleted, KEPT: {why}"));
            self.snapshot_deferred = true;
            return Ok(());
        }
        let mut errors = Vec::new();
        for id in &doomed {
            match self.ec2().delete_snapshot().snapshot_id(id).send().await {
                Ok(_) => self.say(format!("deleted snapshot {id}")),
                Err(e) => errors.push(format!("{id}: {}", sdk(e))),
            }
        }
        let left = self
            .rebuild_snapshots()
            .await?
            .into_iter()
            .filter(|s| s.1 == Verdict::Delete)
            .count();
        if errors.is_empty() && left == 0 {
            Ok(())
        } else {
            Err(format!(
                "{}; {left} old-root snapshot(s) left",
                errors.join("; ")
            ))
        }
    }

    async fn phase(&mut self, p: Phase) -> Result<(), String> {
        self.say(format!("== {} ({}) ==", p.as_str(), self.mode.as_str()));
        match p {
            Phase::Preflight => self.preflight().await,
            Phase::Box => self.box_phase().await,
            Phase::S3 => self.s3_phase().await,
            Phase::Snapshot => self.snapshot_phase().await,
        }
    }
}

async fn real_main(args: &[String]) -> Result<Vec<String>, (String, Vec<String>)> {
    let mode = flag(args, "--mode")
        .and_then(|m| Mode::parse(&m))
        .ok_or(("--mode must be dry or apply".to_string(), Vec::new()))?;
    let phase_in = flag(args, "--phase").unwrap_or_default();
    let phases = Phase::parse_list(&phase_in)
        .ok_or((format!("unknown --phase {phase_in:?}"), Vec::new()))?;
    let sha = flag(args, "--sha").unwrap_or_default();
    if !is_commit_sha(&sha) {
        return Err((format!("--sha {sha:?} is not a commit id"), Vec::new()));
    }
    let confirm = flag(args, "--confirm").unwrap_or_default();
    let now = now_ist(Utc::now());
    for p in &phases {
        if let Some(why) = run_refusal(*p, mode, &confirm, now) {
            return Err((format!("refused: {why}"), Vec::new()));
        }
    }
    // Quote 42: a snapshot-only apply before its time is refused outright
    // (inside `cloud` the S3 delete still runs and the snapshot is kept).
    if mode == Mode::Apply
        && phases == [Phase::Snapshot]
        && let Some(why) = snapshot_delete_refusal(now)
    {
        return Err((format!("refused: {why}"), Vec::new()));
    }
    let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(REGION))
        .load()
        .await;
    let mut run = Run {
        conf,
        http: reqwest::Client::new(),
        mode,
        sha,
        repo: std::env::var("GITHUB_REPOSITORY").unwrap_or_default(),
        gh_token: std::env::var("GH_TOKEN").unwrap_or_default(),
        log: Vec::new(),
        keep_alive_before: None,
        keep_alive_set: false,
        snapshot_deferred: false,
    };
    if mode == Mode::Apply && phases != [Phase::Preflight] {
        match run.busy_deploys().await {
            Ok(b) if b.is_empty() => run.say("no deploy is queued or running"),
            Ok(b) => return Err((format!("a deploy is busy: {}", b.join("; ")), run.log)),
            Err(e) => return Err((format!("could not check the deploys: {e}"), run.log)),
        }
        match run.dry_run_succeeded_on_this_commit().await {
            Ok(true) => run.say("a successful dry all run exists on this commit"),
            Ok(false) => {
                return Err((
                    String::from(
                        "run mode=dry phase=all successfully on this same commit before apply",
                    ),
                    run.log,
                ));
            }
            Err(e) => return Err((format!("could not check for a dry run: {e}"), run.log)),
        }
    }
    for p in phases {
        if let Err(e) = run.phase(p).await {
            run.say(format!("FAILED in {}: {e}", p.as_str()));
            run.keep_alive_off().await;
            return Err((e, run.log));
        }
    }
    if run.snapshot_deferred {
        run.say(format!(
            "done, except the old-root snapshot, which is KEPT until {SNAPSHOT_DELETE_FROM_IST} IST"
        ));
    } else {
        run.say("done");
    }
    Ok(run.log)
}

fn write_report(path: Option<String>, ok: bool, log: &[String]) {
    let mut md = String::from("## tv-wipe-2026-10-31\n\n");
    md.push_str(if ok {
        "Result: **done**\n\n```text\n"
    } else {
        "Result: **FAILED**\n\n```text\n"
    });
    for l in log {
        md.push_str(l);
        md.push('\n');
    }
    md.push_str("```\n");
    if let Some(p) = path {
        let _ = std::fs::write(p, &md);
    }
    if let Ok(summary) = std::env::var("GITHUB_STEP_SUMMARY") {
        let _ = std::fs::OpenOptions::new()
            .append(true)
            .open(summary)
            .and_then(|mut f| std::io::Write::write_all(&mut f, md.as_bytes()));
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // TLS provider for the AWS and GitHub clients.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let args: Vec<String> = std::env::args().collect();
    let report = flag(&args, "--report");
    match real_main(&args).await {
        Ok(log) => {
            write_report(report, true, &log);
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

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn test_flag_reads_the_value_after_the_name() {
        let a = args(&["bin", "--mode", "dry", "--report"]);
        assert_eq!(flag(&a, "--mode").as_deref(), Some("dry"));
        assert_eq!(flag(&a, "--report"), None);
        assert_eq!(flag(&a, "--confirm"), None);
    }

    #[tokio::test]
    async fn test_real_main_refuses_bad_input_before_any_aws_call() {
        let sha = "d1b4835470000000000000000000000000000000";
        for bad in [
            args(&["bin", "--mode", "x", "--phase", "box", "--sha", sha]),
            args(&["bin", "--mode", "dry", "--phase", "all", "--sha", sha]),
            args(&["bin", "--mode", "dry", "--phase", "box", "--sha", "abc"]),
            // An apply without the confirmation word is refused at any hour.
            args(&["bin", "--mode", "apply", "--phase", "s3", "--sha", sha]),
        ] {
            let err = real_main(&bad).await.unwrap_err();
            assert!(err.1.is_empty(), "no AWS call before the refusal: {err:?}");
        }
    }
}
