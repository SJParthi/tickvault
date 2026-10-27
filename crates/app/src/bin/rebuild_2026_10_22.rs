//! `tv-rebuild-2026-10-22` — the one-off production rebuild of the night of
//! Thursday 2026-10-22 (plan item 66; operator Quotes 32, 33 and 34).
//!
//! Run only by `.github/workflows/rebuild-2026-10-22.yml`, with the stored
//! EC2-mutation credentials. The steps, the window and the report types are
//! in `tickvault_app::rebuild_2026_10_22`; the box-side work is
//! `tickvault rebuild-box` (`tickvault_app::rebuild_box`), sent over SSM.
//!
//! ```text
//! tv-rebuild-2026-10-22 --mode dry|apply --phase <all|preflight|copy|snapshot|swap|
//!                       prepare-new|deploy|postcheck|rollback> --sha <commit>
//!                       [--confirm <word>] [--manifest <key>] [--report <file>]
//! ```
//!
//! `dry` changes nothing and prints exactly what each phase would copy,
//! verify, snapshot and create. `apply` needs the typed confirmation, the
//! 17:30–08:00 IST weeknight window and a successful dry run of the workflow
//! on the same commit; a failure from `swap` onward rolls back to the old
//! root and type on its own.
//!
//! Exit codes: 0 done; 1 refused or failed (after any rollback).

#![cfg_attr(not(test), deny(unsafe_code))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stderr, clippy::dbg_macro)]
#![allow(clippy::print_stdout)] // APPROVED: one-off CLI; its output is the job log

use std::process::ExitCode;
use std::time::{Duration, Instant};

use aws_sdk_ec2::error::ProvideErrorMetadata as _;
use aws_sdk_ec2::types::{AttributeValue, Filter, ResourceType, Tag, TagSpecification, VolumeType};
use chrono::Utc;
use tickvault_app::rebuild_2026_10_22::{
    APPLY_CONFIRM, BOX_PHASE_TIMEOUT_SECS, DEPLOY_WORKFLOWS, DRAIN_HOLD_PARAM, INSTANCE_NAME_TAG,
    InstanceFacts, KEEP_ALIVE_PARAM, Mode, NEW_INSTANCE_TYPE, NEW_ROOT_IOPS, NEW_ROOT_SIZE_GB,
    NEW_ROOT_THROUGHPUT_MIBPS, OLD_INSTANCE_TYPE, Phase, REBUILD_TAG_KEY, REBUILD_TAG_VALUE,
    ROLLBACK_CONFIRM, SNAPSHOT_KEEP_UNTIL, Stage, VolumeFacts, apply_window, classify_stage,
    drain_hold_active, is_commit_sha, is_manifest_key, keep_alive_value, parse_box_report,
    phases_for, preflight_problems,
};

const REGION: &str = "ap-south-1";
const WORKFLOW_FILE: &str = "rebuild-2026-10-22.yml";
const DEPLOY_WORKFLOW_FILE: &str = "deploy-aws.yml";
const OLD_ROOT_TAG_KEY: &str = "tv-rebuild-old-root";
const GITHUB_API: &str = "https://api.github.com";
const POLL: Duration = Duration::from_secs(15);
const STATE_WAIT: Duration = Duration::from_secs(20 * 60);
const SNAPSHOT_WAIT: Duration = Duration::from_secs(5 * 3600);
const SSM_ONLINE_WAIT: Duration = Duration::from_secs(20 * 60);
const DEPLOY_WAIT: Duration = Duration::from_secs(90 * 60);

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn sdk<E: std::error::Error + 'static>(e: E) -> String {
    aws_sdk_ec2::error::DisplayErrorContext(&e).to_string()
}

/// Everything one run carries.
struct Run {
    ec2: aws_sdk_ec2::Client,
    ssm: aws_sdk_ssm::Client,
    http: reqwest::Client,
    mode: Mode,
    sha: String,
    repo: String,
    gh_token: String,
    manifest: Option<String>,
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

    // ----------------------------- EC2 reads -----------------------------

    async fn instance(&self) -> Result<InstanceFacts, String> {
        let resp = self
            .ec2
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
        let mut found: Vec<InstanceFacts> = Vec::new();
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
                let root_device_name = i.root_device_name().unwrap_or_default().to_string();
                let root_volume_id = i
                    .block_device_mappings()
                    .iter()
                    .find(|m| m.device_name() == Some(root_device_name.as_str()))
                    .and_then(|m| m.ebs())
                    .and_then(|e| e.volume_id())
                    .map(str::to_string);
                found.push(InstanceFacts {
                    instance_id: i.instance_id().unwrap_or_default().to_string(),
                    state,
                    instance_type: i.instance_type().map_or("", |t| t.as_str()).to_string(),
                    availability_zone: i
                        .placement()
                        .and_then(|p| p.availability_zone())
                        .unwrap_or_default()
                        .to_string(),
                    root_device_name,
                    root_volume_id,
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

    async fn image_id(&self, instance_id: &str) -> Result<String, String> {
        let resp = self
            .ec2
            .describe_instances()
            .instance_ids(instance_id)
            .send()
            .await
            .map_err(sdk)?;
        resp.reservations()
            .iter()
            .flat_map(|r| r.instances())
            .find_map(|i| i.image_id().map(str::to_string))
            .ok_or_else(|| String::from("instance has no image id"))
    }

    async fn volume(&self, id: &str) -> Result<(VolumeFacts, Option<String>), String> {
        let resp = self
            .ec2
            .describe_volumes()
            .volume_ids(id)
            .send()
            .await
            .map_err(sdk)?;
        let v = resp
            .volumes()
            .first()
            .ok_or_else(|| format!("volume {id} not found"))?;
        let tag = |k: &str| {
            v.tags()
                .iter()
                .find(|t| t.key() == Some(k))
                .and_then(|t| t.value())
                .map(str::to_string)
        };
        Ok((
            VolumeFacts {
                volume_id: id.to_string(),
                size_gb: v.size().unwrap_or(0),
                iops: v.iops().unwrap_or(0),
                throughput: v.throughput().unwrap_or(0),
                volume_type: v.volume_type().map_or("", |t| t.as_str()).to_string(),
                state: v.state().map_or("", |s| s.as_str()).to_string(),
                availability_zone: v.availability_zone().unwrap_or_default().to_string(),
                encrypted: v.encrypted().unwrap_or(false),
                rebuild_tag: tag(REBUILD_TAG_KEY),
            },
            v.kms_key_id().map(str::to_string),
        ))
    }

    async fn volumes_tagged(&self, key: &str) -> Result<Vec<String>, String> {
        let resp = self
            .ec2
            .describe_volumes()
            .filters(
                Filter::builder()
                    .name(format!("tag:{key}"))
                    .values(REBUILD_TAG_VALUE)
                    .build(),
            )
            .send()
            .await
            .map_err(sdk)?;
        Ok(resp
            .volumes()
            .iter()
            .filter_map(|v| v.volume_id().map(str::to_string))
            .collect())
    }

    async fn root_modifying(&self, id: &str) -> Result<bool, String> {
        match self
            .ec2
            .describe_volumes_modifications()
            .volume_ids(id)
            .send()
            .await
        {
            Ok(resp) => Ok(resp.volumes_modifications().iter().any(|m| {
                m.modification_state()
                    .is_some_and(|s| s.as_str() == "modifying")
            })),
            // A volume that was never modified answers "not found".
            Err(e) if e.code() == Some("InvalidVolumeModification.NotFound") => Ok(false),
            Err(e) => Err(sdk(e)),
        }
    }

    async fn ami_root_snapshot(&self, image_id: &str, device: &str) -> Result<String, String> {
        let resp = self
            .ec2
            .describe_images()
            .image_ids(image_id)
            .send()
            .await
            .map_err(sdk)?;
        let img = resp
            .images()
            .first()
            .ok_or_else(|| format!("AMI {image_id} not found (deregistered?)"))?;
        img.block_device_mappings()
            .iter()
            .find(|m| m.device_name() == Some(device))
            .and_then(|m| m.ebs())
            .and_then(|e| e.snapshot_id())
            .map(str::to_string)
            .ok_or_else(|| format!("AMI {image_id} has no root snapshot at {device}"))
    }

    async fn wait_instance_state(&self, id: &str, want: &str) -> Result<(), String> {
        let deadline = Instant::now() + STATE_WAIT;
        loop {
            let state = self.instance().await?.state;
            if state == want {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("{id} is {state}, not {want}, after {STATE_WAIT:?}"));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn wait_volume_state(&self, id: &str, want: &str) -> Result<(), String> {
        let deadline = Instant::now() + STATE_WAIT;
        loop {
            let (v, _) = self.volume(id).await?;
            if v.state == want {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("volume {id} is {}, not {want}", v.state));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    // ---------------------------- EC2 writes -----------------------------

    async fn start(&mut self, id: &str) -> Result<(), String> {
        self.ec2
            .start_instances()
            .instance_ids(id)
            .send()
            .await
            .map_err(sdk)?;
        self.wait_instance_state(id, "running").await?;
        self.say(format!("started {id}"));
        Ok(())
    }

    async fn stop(&mut self, id: &str) -> Result<(), String> {
        self.ec2
            .stop_instances()
            .instance_ids(id)
            .send()
            .await
            .map_err(sdk)?;
        self.wait_instance_state(id, "stopped").await?;
        self.say(format!("stopped {id}"));
        Ok(())
    }

    async fn set_type(&mut self, id: &str, t: &str) -> Result<(), String> {
        self.ec2
            .modify_instance_attribute()
            .instance_id(id)
            .instance_type(AttributeValue::builder().value(t).build())
            .send()
            .await
            .map_err(sdk)?;
        self.say(format!("instance type set to {t}"));
        Ok(())
    }

    async fn tag(&self, resource: &str, tags: &[(&str, &str)]) -> Result<(), String> {
        let mut req = self.ec2.create_tags().resources(resource);
        for (k, v) in tags {
            req = req.tags(Tag::builder().key(*k).value(*v).build());
        }
        req.send().await.map_err(sdk).map(|_| ())
    }

    /// `true` when a DryRun of the call says it would be allowed.
    fn dry_ok<T, E: aws_sdk_ec2::error::ProvideErrorMetadata>(r: &Result<T, E>) -> bool {
        matches!(r, Err(e) if e.code() == Some("DryRunOperation"))
    }

    async fn probe_mutations(&mut self, inst: &InstanceFacts, root: &str) -> Vec<String> {
        let id = inst.instance_id.as_str();
        let mut missing = Vec::new();
        let r = self
            .ec2
            .create_snapshot()
            .volume_id(root)
            .dry_run(true)
            .send()
            .await;
        if !Self::dry_ok(&r) {
            missing.push("ec2:CreateSnapshot");
        }
        let r = self
            .ec2
            .create_volume()
            .availability_zone(&inst.availability_zone)
            .size(NEW_ROOT_SIZE_GB)
            .volume_type(VolumeType::Gp3)
            .dry_run(true)
            .send()
            .await;
        if !Self::dry_ok(&r) {
            missing.push("ec2:CreateVolume");
        }
        let r = self
            .ec2
            .stop_instances()
            .instance_ids(id)
            .dry_run(true)
            .send()
            .await;
        if !Self::dry_ok(&r) {
            missing.push("ec2:StopInstances");
        }
        let r = self
            .ec2
            .start_instances()
            .instance_ids(id)
            .dry_run(true)
            .send()
            .await;
        if !Self::dry_ok(&r) {
            missing.push("ec2:StartInstances");
        }
        let r = self
            .ec2
            .modify_instance_attribute()
            .instance_id(id)
            .instance_type(AttributeValue::builder().value(NEW_INSTANCE_TYPE).build())
            .dry_run(true)
            .send()
            .await;
        if !Self::dry_ok(&r) {
            missing.push("ec2:ModifyInstanceAttribute");
        }
        let r = self
            .ec2
            .detach_volume()
            .volume_id(root)
            .dry_run(true)
            .send()
            .await;
        if !Self::dry_ok(&r) {
            missing.push("ec2:DetachVolume");
        }
        let r = self
            .ec2
            .attach_volume()
            .volume_id(root)
            .instance_id(id)
            .device(&inst.root_device_name)
            .dry_run(true)
            .send()
            .await;
        if !Self::dry_ok(&r) {
            missing.push("ec2:AttachVolume");
        }
        // A tag is required: with none, EC2 answers InvalidParameterValue
        // before it checks the permission, so the probe read "missing" even
        // for a key that holds ec2:CreateTags (live, 2026-10-27). DryRun, so
        // nothing is written.
        let r = self
            .ec2
            .create_tags()
            .resources(root)
            .tags(
                Tag::builder()
                    .key(REBUILD_TAG_KEY)
                    .value(REBUILD_TAG_VALUE)
                    .build(),
            )
            .dry_run(true)
            .send()
            .await;
        if !Self::dry_ok(&r) {
            missing.push("ec2:CreateTags");
        }
        missing.into_iter().map(str::to_string).collect()
    }

    // ------------------------------- SSM ---------------------------------

    async fn param(&self, name: &str) -> Option<String> {
        self.ssm
            .get_parameter()
            .name(name)
            .send()
            .await
            .ok()
            .and_then(|r| r.parameter().and_then(|p| p.value()).map(str::to_string))
    }

    async fn put_param(&self, name: &str, value: &str) -> Result<(), String> {
        self.ssm
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

    async fn keep_alive_on(&mut self) -> Result<(), String> {
        if !self.keep_alive_set {
            self.keep_alive_before = self.param(KEEP_ALIVE_PARAM).await;
        }
        let v = keep_alive_value(Utc::now());
        self.put_param(KEEP_ALIVE_PARAM, &v).await?;
        self.keep_alive_set = true;
        self.say(format!(
            "keep-alive set to {v} (curfew and out-of-window stop stand aside)"
        ));
        Ok(())
    }

    async fn keep_alive_off(&mut self) {
        if !self.keep_alive_set {
            return;
        }
        // Back to what it held, or to now (an expired value is no override).
        let restore = self
            .keep_alive_before
            .clone()
            .unwrap_or_else(|| keep_alive_value(Utc::now() - chrono::TimeDelta::hours(13)));
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
            let resp = self
                .ssm
                .describe_instance_information()
                .filters(
                    aws_sdk_ssm::types::InstanceInformationStringFilter::builder()
                        .key("InstanceIds")
                        .values(id)
                        .build()
                        .map_err(|e| e.to_string())?,
                )
                .send()
                .await;
            if let Ok(resp) = resp {
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

    async fn box_phase(
        &mut self,
        id: &str,
        phase: Phase,
        mode: Mode,
    ) -> Result<tickvault_app::rebuild_2026_10_22::BoxReport, String> {
        let commands = tickvault_app::rebuild_ssm::box_phase_commands(
            &self.sha,
            phase,
            mode,
            self.manifest.as_deref(),
        )
        .map_err(|e| format!("command refused: {e:?}"))?;
        let sent = self
            .ssm
            .send_command()
            .instance_ids(id)
            .document_name("AWS-RunShellScript")
            .comment(format!(
                "tv-rebuild-2026-10-22 {} {}",
                phase.as_str(),
                mode.as_str()
            ))
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
            "box {} {} sent (SSM command {cmd})",
            phase.as_str(),
            mode.as_str()
        ));
        let deadline = Instant::now() + Duration::from_secs(BOX_PHASE_TIMEOUT_SECS + 900);
        loop {
            tokio::time::sleep(POLL).await;
            let inv = self
                .ssm
                .get_command_invocation()
                .command_id(&cmd)
                .instance_id(id)
                .send()
                .await;
            let Ok(inv) = inv else {
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
            let report = parse_box_report(&out);
            match report {
                Some(rep) => {
                    for l in &rep.lines {
                        self.say(format!("  box: {l}"));
                    }
                    for p in &rep.problems {
                        self.say(format!("  box PROBLEM: {p}"));
                    }
                    if rep.problems_total > rep.problems.len() {
                        self.say(format!(
                            "  box: {} more problems in the box's local report under /var/tmp",
                            rep.problems_total - rep.problems.len()
                        ));
                    }
                    if status != "Success" && rep.ok {
                        return Err(format!("SSM status {status} with a clean report"));
                    }
                    return Ok(rep);
                }
                None => {
                    let err = inv.standard_error_content().unwrap_or_default();
                    return Err(format!(
                        "SSM status {status}; no report line. stderr: {}",
                        err.chars().take(800).collect::<String>()
                    ));
                }
            }
        }
    }

    // ------------------------------ GitHub -------------------------------

    async fn gh_get(&self, path: &str) -> Result<serde_json::Value, String> {
        self.http
            .get(format!("{GITHUB_API}{path}"))
            .bearer_auth(&self.gh_token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "tv-rebuild-2026-10-22")
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

    async fn dispatch_and_wait_deploy(&mut self) -> Result<(), String> {
        let since = Utc::now() - chrono::TimeDelta::seconds(30);
        self.http
            .post(format!(
                "{GITHUB_API}/repos/{}/actions/workflows/{DEPLOY_WORKFLOW_FILE}/dispatches",
                self.repo
            ))
            .bearer_auth(&self.gh_token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "tv-rebuild-2026-10-22")
            .json(&serde_json::json!({"ref": "main", "inputs": {"confirm_market_hours": "no"}}))
            .send()
            .await
            .map_err(|e| format!("deploy dispatch: {e}"))?
            .error_for_status()
            .map_err(|e| format!("deploy dispatch: {e}"))?;
        self.say("deploy-aws dispatched on main");
        let deadline = Instant::now() + DEPLOY_WAIT;
        let mut run_id: Option<u64> = None;
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            if Instant::now() >= deadline {
                return Err(String::from("deploy-aws did not finish in time"));
            }
            if run_id.is_none() {
                let v = self
                    .gh_get(&format!(
                        "/repos/{}/actions/workflows/{DEPLOY_WORKFLOW_FILE}/runs?event=workflow_dispatch&branch=main&per_page=10",
                        self.repo
                    ))
                    .await?;
                run_id = v
                    .get("workflow_runs")
                    .and_then(|r| r.as_array())
                    .and_then(|runs| {
                        runs.iter().find_map(|r| {
                            let created = r.get("created_at")?.as_str()?;
                            let created = chrono::DateTime::parse_from_rfc3339(created).ok()?;
                            (created >= since).then(|| r.get("id")?.as_u64()).flatten()
                        })
                    });
                if let Some(id) = run_id {
                    self.say(format!("deploy-aws run {id} found"));
                }
                continue;
            }
            let id = run_id.unwrap_or_default();
            let v = self
                .gh_get(&format!("/repos/{}/actions/runs/{id}", self.repo))
                .await?;
            if v.get("status").and_then(|s| s.as_str()) != Some("completed") {
                continue;
            }
            let conclusion = v
                .get("conclusion")
                .and_then(|s| s.as_str())
                .unwrap_or("none");
            return if conclusion == "success" {
                self.say(format!("deploy-aws run {id} succeeded"));
                Ok(())
            } else {
                Err(format!("deploy-aws run {id} concluded {conclusion}"))
            };
        }
    }

    // ------------------------------ Phases -------------------------------

    async fn facts(&self) -> Result<(InstanceFacts, Option<VolumeFacts>, Option<String>), String> {
        let inst = self.instance().await?;
        let (root, kms) = match &inst.root_volume_id {
            Some(id) => {
                let (v, k) = self.volume(id).await?;
                (Some(v), k)
            }
            None => (None, None),
        };
        Ok((inst, root, kms))
    }

    async fn preflight(&mut self) -> Result<(), String> {
        let (inst, root, _) = self.facts().await?;
        self.say(format!(
            "instance {} {} {} in {} (root {} at {})",
            inst.instance_id,
            inst.instance_type,
            inst.state,
            inst.availability_zone,
            inst.root_volume_id.as_deref().unwrap_or("none"),
            inst.root_device_name
        ));
        if let Some(r) = &root {
            self.say(format!(
                "root {}: {} GB {} {} IOPS {} MiB/s, {}, encrypted {}",
                r.volume_id, r.size_gb, r.volume_type, r.iops, r.throughput, r.state, r.encrypted
            ));
        }
        let stage = classify_stage(&inst, root.as_ref());
        self.say(format!("stage: {stage:?}"));
        let modifying = match &inst.root_volume_id {
            Some(id) => self.root_modifying(id).await?,
            None => false,
        };
        let hold = self
            .param(DRAIN_HOLD_PARAM)
            .await
            .is_some_and(|raw| drain_hold_active(&raw, Utc::now().timestamp()));
        let busy = self.busy_deploys().await?;
        let holiday = holiday_today();
        let mut problems =
            preflight_problems(&inst, root.as_ref(), modifying, hold, &busy, holiday);
        let image = self.image_id(&inst.instance_id).await?;
        match self.ami_root_snapshot(&image, &inst.root_device_name).await {
            Ok(snap) => self.say(format!("new root source: AMI {image} root snapshot {snap}")),
            Err(e) => problems.push(e),
        }
        if self.mode == Mode::Apply
            && let Some(rootid) = inst.root_volume_id.clone()
        {
            let missing = self.probe_mutations(&inst, &rootid).await;
            if missing.is_empty() {
                self.say("IAM: every EC2 mutation this run calls passes its DryRun");
            } else {
                problems.push(format!("credentials lack: {}", missing.join(", ")));
            }
        }
        for p in &problems {
            self.say(format!("PREFLIGHT PROBLEM: {p}"));
        }
        if problems.is_empty() {
            self.say("preflight: clear");
            Ok(())
        } else {
            Err(format!("{} preflight problem(s)", problems.len()))
        }
    }

    async fn copy(&mut self) -> Result<(), String> {
        let (inst, root, _) = self.facts().await?;
        if classify_stage(&inst, root.as_ref()) == Stage::Swapped {
            self.say("copy: the box already runs the new root; skipped");
            return Ok(());
        }
        let id = inst.instance_id.clone();
        if self.mode == Mode::Dry {
            if inst.state != "running" {
                self.say(
                    "copy (dry): the box is stopped, so the on-box inventory is skipped. Run the \
                     dry run while the box is up (08:30–17:30 IST) to see every table, partition, \
                     row count and file the apply would copy and verify.",
                );
                return Ok(());
            }
            let rep = self.box_phase(&id, Phase::Copy, Mode::Dry).await?;
            return if rep.ok {
                Ok(())
            } else {
                Err(String::from(
                    "the inventory found problems the apply would stop on",
                ))
            };
        }
        self.keep_alive_on().await?;
        if inst.state == "stopped" {
            let t0 = Utc::now();
            self.start(&id).await?;
            self.wait_ssm_online(&id, t0).await?;
        } else if inst.state != "running" {
            return Err(format!("instance is {}", inst.state));
        }
        let rep = self.box_phase(&id, Phase::Copy, Mode::Apply).await?;
        if !rep.ok {
            return Err(String::from(
                "the copy did not verify; the box is left running, nothing changed",
            ));
        }
        let key = rep.manifest_key.ok_or("the copy wrote no manifest")?;
        if !is_manifest_key(&key) {
            return Err(format!("unexpected manifest key {key:?}"));
        }
        self.say(format!("copy verified; manifest {key}"));
        self.manifest = Some(key);
        self.stop(&id).await?;
        Ok(())
    }

    async fn snapshot(&mut self) -> Result<(), String> {
        let (inst, root, _) = self.facts().await?;
        if classify_stage(&inst, root.as_ref()) == Stage::Swapped {
            self.say("snapshot: already swapped; skipped");
            return Ok(());
        }
        let root = root.ok_or("no root volume")?;
        let existing = self
            .ec2
            .describe_snapshots()
            .owner_ids("self")
            .filters(
                Filter::builder()
                    .name("volume-id")
                    .values(&root.volume_id)
                    .build(),
            )
            .filters(
                Filter::builder()
                    .name(format!("tag:{REBUILD_TAG_KEY}"))
                    .values(REBUILD_TAG_VALUE)
                    .build(),
            )
            .send()
            .await
            .map_err(sdk)?;
        if self.mode == Mode::Dry {
            self.say(format!(
                "snapshot (dry): would snapshot {} ({} GB) tagged {REBUILD_TAG_KEY}={REBUILD_TAG_VALUE}, \
                 tv-delete-after={SNAPSHOT_KEEP_UNTIL}; {} rebuild snapshot(s) already exist",
                root.volume_id,
                root.size_gb,
                existing.snapshots().len()
            ));
            return Ok(());
        }
        if inst.state != "stopped" {
            return Err(format!(
                "the instance must be stopped to snapshot (it is {})",
                inst.state
            ));
        }
        let snap_id = if let Some(s) = existing
            .snapshots()
            .iter()
            .find(|s| s.state().is_some_and(|st| st.as_str() != "error"))
        {
            let id = s.snapshot_id().unwrap_or_default().to_string();
            self.say(format!(
                "snapshot {id} of {} already exists; reused",
                root.volume_id
            ));
            id
        } else {
            let tags = TagSpecification::builder()
                .resource_type(ResourceType::Snapshot)
                .tags(
                    Tag::builder()
                        .key("Name")
                        .value("tv-prod-root-before-rebuild-2026-10-22")
                        .build(),
                )
                .tags(
                    Tag::builder()
                        .key(REBUILD_TAG_KEY)
                        .value(REBUILD_TAG_VALUE)
                        .build(),
                )
                .tags(
                    Tag::builder()
                        .key("tv-delete-after")
                        .value(SNAPSHOT_KEEP_UNTIL)
                        .build(),
                )
                .build();
            let resp = self
                .ec2
                .create_snapshot()
                .volume_id(&root.volume_id)
                .description("tickvault root before the 2026-10-22 rebuild (Quotes 32/33); kept at least 7 days")
                .tag_specifications(tags)
                .send()
                .await
                .map_err(sdk)?;
            let id = resp.snapshot_id().unwrap_or_default().to_string();
            self.say(format!("snapshot {id} of {} started", root.volume_id));
            id
        };
        let deadline = Instant::now() + SNAPSHOT_WAIT;
        loop {
            let r = self
                .ec2
                .describe_snapshots()
                .snapshot_ids(&snap_id)
                .send()
                .await
                .map_err(sdk)?;
            let s = r.snapshots().first().ok_or("snapshot vanished")?;
            let state = s.state().map_or("", |x| x.as_str()).to_string();
            match state.as_str() {
                "completed" => {
                    self.say(format!("snapshot {snap_id} completed"));
                    return Ok(());
                }
                "error" => return Err(format!("snapshot {snap_id} failed")),
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "snapshot {snap_id} still {state} ({}) after {SNAPSHOT_WAIT:?}; nothing else changed",
                    s.progress().unwrap_or("?")
                ));
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }

    async fn swap(&mut self) -> Result<(), String> {
        let (inst, root, kms) = self.facts().await?;
        let stage = classify_stage(&inst, root.as_ref());
        if stage == Stage::Swapped {
            self.say("swap: the new root is already attached; skipped");
            return Ok(());
        }
        let root = root.ok_or("no root volume")?;
        let image = self.image_id(&inst.instance_id).await?;
        let source = self
            .ami_root_snapshot(&image, &inst.root_device_name)
            .await?;
        if self.mode == Mode::Dry {
            self.say(format!(
                "swap (dry): would create a {NEW_ROOT_SIZE_GB} GB gp3 volume ({NEW_ROOT_IOPS} IOPS, \
                 {NEW_ROOT_THROUGHPUT_MIBPS} MiB/s, encrypted with the old root's key) in {} from \
                 {source}; detach {} (kept, tagged {OLD_ROOT_TAG_KEY}); attach the new volume at {}; \
                 set the type {OLD_INSTANCE_TYPE} → {NEW_INSTANCE_TYPE}; start. Same instance, \
                 same zone, same network interface and address.",
                inst.availability_zone, root.volume_id, inst.root_device_name
            ));
            return Ok(());
        }
        if stage != Stage::Untouched {
            return Err(format!("refusing to swap from stage {stage:?}"));
        }
        if inst.state != "stopped" {
            return Err(format!(
                "the instance must be stopped to swap (it is {})",
                inst.state
            ));
        }
        let id = inst.instance_id.clone();
        // New volume (reused if a previous attempt created it).
        let mut new_id = None;
        for v in self.volumes_tagged(REBUILD_TAG_KEY).await? {
            let (f, _) = self.volume(&v).await?;
            if f.state == "available"
                && f.size_gb == NEW_ROOT_SIZE_GB
                && f.availability_zone == inst.availability_zone
            {
                new_id = Some(v);
            }
        }
        let new_id = match new_id {
            Some(v) => {
                self.say(format!("new root {v} already exists; reused"));
                v
            }
            None => {
                let tags = TagSpecification::builder()
                    .resource_type(ResourceType::Volume)
                    .tags(Tag::builder().key("Name").value("tv-prod-root").build())
                    .tags(
                        Tag::builder()
                            .key(REBUILD_TAG_KEY)
                            .value(REBUILD_TAG_VALUE)
                            .build(),
                    )
                    .build();
                let mut req = self
                    .ec2
                    .create_volume()
                    .availability_zone(&inst.availability_zone)
                    .snapshot_id(&source)
                    .size(NEW_ROOT_SIZE_GB)
                    .volume_type(VolumeType::Gp3)
                    .iops(NEW_ROOT_IOPS)
                    .throughput(NEW_ROOT_THROUGHPUT_MIBPS)
                    .encrypted(true)
                    .tag_specifications(tags);
                if let Some(k) = &kms {
                    req = req.kms_key_id(k);
                }
                let v = req
                    .send()
                    .await
                    .map_err(sdk)?
                    .volume_id()
                    .unwrap_or_default()
                    .to_string();
                self.say(format!("new root {v} created"));
                v
            }
        };
        self.wait_volume_state(&new_id, "available").await?;
        // From here every failure rolls back (the caller does it).
        self.tag(
            &root.volume_id,
            &[
                (OLD_ROOT_TAG_KEY, REBUILD_TAG_VALUE),
                ("Name", "tv-prod-root-old-2026-10-22"),
            ],
        )
        .await?;
        self.ec2
            .detach_volume()
            .volume_id(&root.volume_id)
            .instance_id(&id)
            .send()
            .await
            .map_err(sdk)?;
        self.wait_volume_state(&root.volume_id, "available").await?;
        self.say(format!("old root {} detached and kept", root.volume_id));
        self.ec2
            .attach_volume()
            .volume_id(&new_id)
            .instance_id(&id)
            .device(&inst.root_device_name)
            .send()
            .await
            .map_err(sdk)?;
        self.wait_volume_state(&new_id, "in-use").await?;
        self.say(format!(
            "new root {new_id} attached at {}",
            inst.root_device_name
        ));
        self.set_type(&id, NEW_INSTANCE_TYPE).await?;
        let t0 = Utc::now();
        self.start(&id).await?;
        self.wait_ssm_online(&id, t0).await?;
        self.say("the new root booted and the SSM agent is online");
        Ok(())
    }

    async fn prepare_new(&mut self) -> Result<(), String> {
        if self.mode == Mode::Dry {
            self.say("prepare-new (dry): would wait for first-boot provisioning, write QuestDB's compose env for 2 vCPU / 16 GiB, start QuestDB, and restore every keep table and data file from the manifest before the app's first start");
            return Ok(());
        }
        if self.manifest.is_none() {
            return Err(String::from(
                "prepare-new needs the copy manifest (--manifest, or run copy first)",
            ));
        }
        let (inst, root, _) = self.facts().await?;
        if classify_stage(&inst, root.as_ref()) != Stage::Swapped || inst.state != "running" {
            return Err(String::from(
                "prepare-new runs only on the running new root",
            ));
        }
        let rep = self
            .box_phase(&inst.instance_id, Phase::PrepareNew, Mode::Apply)
            .await?;
        if rep.ok {
            Ok(())
        } else {
            Err(String::from("the restore did not verify"))
        }
    }

    async fn deploy(&mut self) -> Result<(), String> {
        if self.mode == Mode::Dry {
            self.say("deploy (dry): would dispatch deploy-aws on main and wait for success");
            return Ok(());
        }
        self.dispatch_and_wait_deploy().await
    }

    async fn postcheck(&mut self) -> Result<(), String> {
        let (inst, root, _) = self.facts().await?;
        if self.mode == Mode::Dry {
            self.say(format!(
                "postcheck (dry): stage {:?}",
                classify_stage(&inst, root.as_ref())
            ));
            if inst.state == "running" && classify_stage(&inst, root.as_ref()) == Stage::Swapped {
                let rep = self
                    .box_phase(&inst.instance_id, Phase::Postcheck, Mode::Dry)
                    .await?;
                return if rep.ok {
                    Ok(())
                } else {
                    Err(String::from("postcheck found problems"))
                };
            }
            return Ok(());
        }
        let rep = self
            .box_phase(&inst.instance_id, Phase::Postcheck, Mode::Apply)
            .await?;
        if !rep.ok {
            return Err(String::from("postcheck found problems"));
        }
        self.stop(&inst.instance_id).await?;
        self.keep_alive_off().await;
        let (inst, root, _) = self.facts().await?;
        if let Some(r) = root {
            self.say(format!(
                "FINAL: {} {} {}, root {} {} GB gp3 {} IOPS {} MiB/s; the 08:30 IST start boots it",
                inst.instance_id,
                inst.instance_type,
                inst.state,
                r.volume_id,
                r.size_gb,
                r.iops,
                r.throughput
            ));
        }
        Ok(())
    }

    /// Back to the old root and the old type; leaves the box stopped.
    async fn rollback(&mut self) -> Result<(), String> {
        let (inst, root, _) = self.facts().await?;
        let old = self.volumes_tagged(OLD_ROOT_TAG_KEY).await?;
        let [old] = old.as_slice() else {
            return Err(format!(
                "expected one volume tagged {OLD_ROOT_TAG_KEY}, found {}",
                old.len()
            ));
        };
        let old = old.clone();
        if self.mode == Mode::Dry {
            self.say(format!(
                "rollback (dry): would stop {}, detach {} from {}, attach {old}, set the type {OLD_INSTANCE_TYPE}, leave it stopped",
                inst.instance_id,
                root.as_ref().map_or("nothing", |r| r.volume_id.as_str()),
                inst.root_device_name
            ));
            return Ok(());
        }
        let id = inst.instance_id.clone();
        if inst.state != "stopped" {
            self.stop(&id).await?;
        }
        if let Some(r) = &root
            && r.volume_id != old
        {
            self.ec2
                .detach_volume()
                .volume_id(&r.volume_id)
                .instance_id(&id)
                .send()
                .await
                .map_err(sdk)?;
            self.wait_volume_state(&r.volume_id, "available").await?;
            self.say(format!("new root {} detached (kept)", r.volume_id));
        }
        let (of, _) = self.volume(&old).await?;
        if of.state != "in-use" {
            self.ec2
                .attach_volume()
                .volume_id(&old)
                .instance_id(&id)
                .device(&inst.root_device_name)
                .send()
                .await
                .map_err(sdk)?;
            self.wait_volume_state(&old, "in-use").await?;
        }
        self.set_type(&id, OLD_INSTANCE_TYPE).await?;
        self.keep_alive_off().await;
        let (inst, root, _) = self.facts().await?;
        let stage = classify_stage(&inst, root.as_ref());
        self.say(format!(
            "ROLLBACK DONE: stage {stage:?}, instance {}",
            inst.state
        ));
        if stage == Stage::Untouched || root.as_ref().is_some_and(|r| r.volume_id == old) {
            Ok(())
        } else {
            Err(format!("rollback ended in stage {stage:?}"))
        }
    }

    async fn phase(&mut self, p: Phase) -> Result<(), String> {
        self.say(format!("== {} ({}) ==", p.as_str(), self.mode.as_str()));
        match p {
            Phase::Preflight => self.preflight().await,
            Phase::Copy => self.copy().await,
            Phase::Snapshot => self.snapshot().await,
            Phase::Swap => self.swap().await,
            Phase::PrepareNew => self.prepare_new().await,
            Phase::Deploy => self.deploy().await,
            Phase::Postcheck => self.postcheck().await,
            Phase::Rollback => self.rollback().await,
        }
    }
}

/// `true` when today (IST) is an NSE holiday by the repo's own calendar (the
/// box's holiday gate would stop it on boot). An unreadable calendar is not
/// a holiday (the gate fails open the same way).
fn holiday_today() -> bool {
    let today = Utc::now()
        .with_timezone(&tickvault_app::rebuild_2026_10_22::ist())
        .date_naive();
    tickvault_app::boot_helpers::layered_config_figment(
        &tickvault_app::boot_helpers::resolve_config_env(),
    )
    .extract::<tickvault_common::config::ApplicationConfig>()
    .ok()
    .and_then(|c| tickvault_common::trading_calendar::TradingCalendar::from_config(&c.trading).ok())
    .is_some_and(|cal| cal.is_holiday(today))
}

async fn real_main() -> Result<Vec<String>, (String, Vec<String>)> {
    let args: Vec<String> = std::env::args().collect();
    let mode = flag(&args, "--mode")
        .and_then(|m| Mode::parse(&m))
        .ok_or(("--mode must be dry or apply".to_string(), Vec::new()))?;
    let phase_in = flag(&args, "--phase").unwrap_or_default();
    let phases =
        phases_for(&phase_in).ok_or((format!("unknown --phase {phase_in:?}"), Vec::new()))?;
    let sha = flag(&args, "--sha").unwrap_or_default();
    if !is_commit_sha(&sha) {
        return Err((format!("--sha {sha:?} is not a commit id"), Vec::new()));
    }
    let manifest = flag(&args, "--manifest").filter(|m| !m.is_empty());
    if let Some(m) = &manifest
        && !is_manifest_key(m)
    {
        return Err((
            format!("--manifest {m:?} is not a rebuild manifest key"),
            Vec::new(),
        ));
    }
    if mode == Mode::Apply {
        let confirm = flag(&args, "--confirm").unwrap_or_default();
        let want = if phases == [Phase::Rollback] {
            ROLLBACK_CONFIRM
        } else {
            APPLY_CONFIRM
        };
        if confirm != want {
            return Err((format!("apply needs --confirm {want}"), Vec::new()));
        }
        if let Err(why) = apply_window(Utc::now()) {
            return Err((format!("refused: {why}"), Vec::new()));
        }
    }
    let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(REGION))
        .load()
        .await;
    let mut run = Run {
        ec2: aws_sdk_ec2::Client::new(&conf),
        ssm: aws_sdk_ssm::Client::new(&conf),
        http: reqwest::Client::new(),
        mode,
        sha,
        repo: std::env::var("GITHUB_REPOSITORY").unwrap_or_default(),
        gh_token: std::env::var("GH_TOKEN").unwrap_or_default(),
        manifest,
        log: Vec::new(),
        keep_alive_before: None,
        keep_alive_set: false,
    };
    if mode == Mode::Apply && phases != [Phase::Rollback] {
        match run.dry_run_succeeded_on_this_commit().await {
            Ok(true) => run.say("a successful dry run of this workflow exists on this commit"),
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
            let after_swap_started = matches!(
                p,
                Phase::Swap | Phase::PrepareNew | Phase::Deploy | Phase::Postcheck
            );
            if mode == Mode::Apply && after_swap_started {
                run.say("rolling back to the old root and type");
                run.mode = Mode::Apply;
                if let Err(rb) = run.rollback().await {
                    run.say(format!("ROLLBACK FAILED: {rb} — the old root is tagged {OLD_ROOT_TAG_KEY}={REBUILD_TAG_VALUE}; re-run with phase=rollback"));
                }
            } else {
                run.keep_alive_off().await;
            }
            return Err((e, run.log));
        }
    }
    run.keep_alive_off().await;
    run.say("done");
    Ok(run.log)
}

fn write_report(path: Option<String>, ok: bool, log: &[String]) {
    let mut md = String::from("## tv-rebuild-2026-10-22\n\n");
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
    match real_main().await {
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
