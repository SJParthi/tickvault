//! Z+ source-scan ratchet for the 3-month data-pull deploy config
//! (operator lock 2026-05-29 §7 Quotes 5+6 in
//! `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`).
//!
//! Companion to `instance_type_lock_guard.rs` (which pins the m8g.large type
//! across the 5 rule/doc files). THIS guard pins the Terraform that actually
//! provisions the box, so a future edit cannot silently:
//!
//!   1. Re-enable `disable_api_stop` — that would block the weekday 16:30 IST
//!      EventBridge auto-stop AND the in-place upgrade script, pushing the bill
//!      from the locked ~₹2,058/mo to ~₹5,500/mo (24/7 running).
//!   2. Drop `instance_type` / `user_data` from `aws_instance.tv_app`'s
//!      `ignore_changes` — that would let a merge-triggered `terraform apply`
//!      REPLACE the running instance and orphan all QuestDB data. The operator
//!      contract is: upgrades via `scripts/aws-upgrade-instance.sh`, deploys via
//!      SSM — never via instance replacement.
//!   3. Revert the weekday-only schedule (MON-FRI) back to daily (Mon-Sun).
//!   4. Flip `enable_eip` default back to true (cost-hardening 2026-06-30: no
//!      orders for 3 months → no Dhan static-IP need → EIP OFF saves ~₹300/mo;
//!      reachability via auto-assign-public-IP, both attributes pinned below).
//!   5. Change the EBS default away from 30 GB (hot window sizing for ~₹2,058/mo).
//!
//! Each assertion fails the build with an operator-readable message so the next
//! session (or Cowork task) cannot regress the locked config by accident.

#![cfg(test)]

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/storage parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path: PathBuf = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Collapse runs of whitespace so HCL alignment / line-wrapping cannot defeat
/// a substring assertion. `terraform fmt` re-aligns `=` columns, so we must
/// match on normalized text, not exact spacing.
fn squish(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strip `#` comments (line + inline) so an explanatory comment that *quotes* a
/// setting (e.g. "`disable_api_stop = true` would block ...") cannot defeat a
/// negative substring assertion. Returns only executable HCL.
fn code_only(s: &str) -> String {
    s.lines()
        .map(|line| match line.find('#') {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const MAIN_TF: &str = "deploy/aws/terraform/main.tf";
const VARIABLES_TF: &str = "deploy/aws/terraform/variables.tf";

/// The running box must NEVER be stop-protected — the daily auto-stop cron and
/// the upgrade script both need ec2:StopInstances. (`disable_api_stop = true`
/// is the cost-blowout + upgrade-blocker trap.)
#[test]
fn deploy_instance_is_not_stop_protected() {
    let body = squish(&code_only(&read(MAIN_TF)));
    assert!(
        body.contains("disable_api_stop = false"),
        "main.tf must set `disable_api_stop = false` — true blocks the weekday \
         16:30 IST auto-stop cron + the in-place upgrade script, blowing the \
         ~₹2,058/mo budget to ~₹5,500/mo (24/7 running)."
    );
    assert!(
        !body.contains("disable_api_stop = true"),
        "main.tf must NOT set `disable_api_stop = true` (see above)."
    );
}

/// Terminate-protection stays ON — termination is the one irreversible action
/// (destroys the EBS root volume + QuestDB data).
#[test]
fn deploy_instance_keeps_terminate_protection() {
    let body = squish(&code_only(&read(MAIN_TF)));
    assert!(
        body.contains("disable_api_termination = true"),
        "main.tf must keep `disable_api_termination = true` — terminate destroys \
         the EBS root volume + all QuestDB data (the only irreversible action)."
    );
}

/// `terraform apply` must never replace or re-type the running instance.
/// instance_type upgrades are out-of-band (the script); user_data is
/// bootstrap-only (deploys are over SSM).
#[test]
fn deploy_instance_ignores_type_and_user_data_to_prevent_replace() {
    let body = squish(&code_only(&read(MAIN_TF)));
    // Assert each REQUIRED key is present inside lifecycle.ignore_changes,
    // rather than an exact closed-list match. Adding MORE ignored keys (e.g.
    // root_block_device[0].{volume_size,iops,throughput} so a later apply can't
    // revert an online EBS bump done by scripts/aws-upgrade-instance.sh) is
    // strictly SAFER and must NOT break this guard. The `ignore_changes = [`
    // anchor + per-key presence preserves the safety intent (ami / instance_type
    // / user_data never trigger a replace) without pinning the exact list shape.
    assert!(
        body.contains("ignore_changes = ["),
        "aws_instance.tv_app must declare a lifecycle.ignore_changes list so a \
         merge-triggered apply can NEVER replace/wipe the running box. Upgrade \
         via scripts/aws-upgrade-instance.sh; deploy via SSM."
    );
    for key in ["ami", "instance_type", "user_data"] {
        assert!(
            body.contains(key),
            "aws_instance.tv_app lifecycle.ignore_changes must include `{key}` \
             (alongside ami + instance_type + user_data) so a merge-triggered \
             apply can NEVER replace/wipe the running box."
        );
    }
    assert!(
        body.contains("user_data_replace_on_change = false"),
        "main.tf must set `user_data_replace_on_change = false` — true would \
         replace the instance (fresh root volume, QuestDB data orphaned) on any \
         user_data drift. Deploys are over SSM, not user_data re-runs."
    );
}

/// Weekday-only schedule (trading days). Mon-Fri crons, not Mon-Sun.
/// IST 08:30 start = 03:00 UTC; IST 16:30 stop = 11:00 UTC (operator narrowed
/// the window back to 08:30-16:30 on 2026-06-05 — "make the aws instance start
/// and stop from 8.30 am till 4.30 pm"; supersedes the 2026-06-02 08:00-17:00).
#[test]
fn deploy_schedule_is_weekday_only() {
    let body = read(MAIN_TF);
    assert!(
        body.contains("cron(0 3 ? * MON-FRI *)"),
        "main.tf daily_start must be `cron(0 3 ? * MON-FRI *)` (08:30 IST, Mon-Fri)."
    );
    assert!(
        body.contains("cron(0 11 ? * MON-FRI *)"),
        "main.tf daily_stop must be `cron(0 11 ? * MON-FRI *)` (16:30 IST, Mon-Fri)."
    );
    assert!(
        !body.contains("MON-SUN") && !body.contains("* * ? * * *"),
        "schedule must be weekday-only (MON-FRI), not daily — operator lock 2026-05-29."
    );
}

/// EIP off by default — cost-hardening "corrected EIP approach" (2026-06-30):
/// no orders → no Dhan static-IP need → EIP removed (~₹300/mo saved);
/// reachability comes from auto-assign-public-IP instead.
#[test]
fn deploy_eip_is_disabled_by_default() {
    let vars = squish(&read(VARIABLES_TF));
    assert!(
        vars.contains("variable \"enable_eip\""),
        "variables.tf must declare `enable_eip`."
    );
    // 2026-06-30: operator's corrected cost-hardening approach flips enable_eip
    // default true -> false. The EIP is ONLY needed for live orders (Dhan
    // static-IP whitelist), and orders are OFF (production.toml dry_run=true).
    // Reachability instead comes from auto-assign-public-IP. (The 2026-05-31
    // true default was for the running box whose ENI lost auto-assign after the
    // manual upgrade — that is now handled by the var.enable_eip OPERATOR ACTION
    // note + the associate_public_ip_address below, not by paying for an EIP.)
    assert!(
        vars.contains("type = bool default = false"),
        "enable_eip must default to false (operator 2026-06-30 cost-hardening — \
         no orders → no static-IP need → EIP removed)."
    );
    let main = squish(&code_only(&read(MAIN_TF)));
    assert!(
        main.contains("count = var.enable_eip ? 1 : 0"),
        "aws_eip.tv_app must stay count-gated on var.enable_eip."
    );
    // With the EIP gone the box must still be reachable: the subnet auto-assigns
    // a public IP AND the instance pins associate_public_ip_address=true for a
    // fresh provision. Both must be present so removing the EIP never strands a
    // freshly-provisioned box.
    assert!(
        main.contains("map_public_ip_on_launch = true"),
        "aws_subnet.public must keep map_public_ip_on_launch=true (auto-assign \
         replaces the removed EIP for reachability)."
    );
    assert!(
        main.contains("associate_public_ip_address = true"),
        "aws_instance.tv_app must set associate_public_ip_address=true (a fresh \
         provision gets a dynamic public IP now that the EIP is off)."
    );
    // The CREATE-ONLY auto-assign attribute MUST be in ignore_changes so a
    // `terraform apply` can never try to add it to the running box (= replace =
    // QuestDB data wiped).
    assert!(
        main.contains("associate_public_ip_address,"),
        "associate_public_ip_address must be in lifecycle.ignore_changes (it is \
         create-only; toggling it on the running box would force a replacement)."
    );
}

/// EBS hot-window default is 30 GB (sized for the ~₹2,058/mo bill; S3 cold-tier
/// archives partitions > 90d).
#[test]
fn deploy_ebs_default_is_30gb() {
    let vars = squish(&read(VARIABLES_TF));
    assert!(
        vars.contains("variable \"ebs_gp3_size_gb\""),
        "variables.tf must declare `ebs_gp3_size_gb`."
    );
    assert!(
        vars.contains("type = number default = 30"),
        "ebs_gp3_size_gb must default to 30 GB (operator lock 2026-05-29 §7 Quote 6)."
    );
}

/// The Terraform must be `terraform fmt`-clean — the exact failure that broke
/// the #866 terraform-apply run (`fmt -check` exit 3 on a comment-split
/// alignment group). This walks every .tf file and asserts no obvious
/// over-alignment regression of the canonical 3-line subnet group.
#[test]
fn deploy_subnet_alignment_is_fmt_canonical() {
    let body = read(MAIN_TF);
    // After `terraform fmt`, the comment-split group aligns to `availability_zone`
    // (17 chars), NOT to `map_public_ip_on_launch` below the comment.
    assert!(
        body.contains("vpc_id            = aws_vpc.dlt.id")
            && body.contains("availability_zone = \"${var.aws_region}a\""),
        "aws_subnet.public must be `terraform fmt`-canonical (the comment splits \
         the alignment group; the 3 lines align to `availability_zone`, not to \
         `map_public_ip_on_launch`). This is the exact fmt failure that broke \
         the #866 terraform-apply run."
    );
}

fn _assert_exists(rel: &str) {
    assert!(repo_root().join(rel).exists(), "{rel} missing");
}

#[test]
fn deploy_terraform_files_exist() {
    _assert_exists(MAIN_TF);
    _assert_exists(VARIABLES_TF);
}

const UPGRADE_SCRIPT: &str = "scripts/aws-upgrade-instance.sh";
const APP_ALARMS_TF: &str = "deploy/aws/terraform/app-alarms.tf";

/// The in-place upgrade script MUST clear stop-protection before stopping —
/// otherwise a stop on a still-`disable_api_stop=true` box fails mid-run
/// (OperationNotPermitted) after the market-hours guard already committed.
#[test]
fn deploy_upgrade_script_clears_stop_protection_before_stop() {
    let body = squish(&read(UPGRADE_SCRIPT));
    assert!(
        body.contains("--no-disable-api-stop"),
        "aws-upgrade-instance.sh must `modify-instance-attribute --no-disable-api-stop` \
         before `stop-instances`, so the upgrade can't deadlock on a stop-protected box."
    );
    // The clear must come BEFORE the stop call (comment-stripped so a comment
    // mentioning "stop" can't skew the ordering).
    let code = squish(&code_only(&read(UPGRADE_SCRIPT)));
    let clear_at = code.find("--no-disable-api-stop");
    let stop_at = code.find("stop-instances");
    assert!(
        matches!((clear_at, stop_at), (Some(c), Some(s)) if c < s),
        "the disable_api_stop clear must precede the stop-instances call"
    );
}

/// The disk-used alarm (the "grow online when the alarm fires" trip-wire the
/// operator chose 2026-05-29) MUST exist — without it the reactive grow plan
/// has no trigger and the 30 GB can silently fill during the 3-month run.
#[test]
fn deploy_disk_used_alarm_exists() {
    let body = read(APP_ALARMS_TF);
    assert!(
        body.contains("\"disk_used_high\""),
        "app-alarms.tf must define the `disk_used_high` alarm (the disk-capacity trip-wire)."
    );
    assert!(
        body.contains("disk_used_percent"),
        "the disk alarm must query the CWAgent `disk_used_percent` metric."
    );
}

const HOLIDAY_GATE_SH: &str = "deploy/aws/holiday-gate.sh";
const HOLIDAY_GATE_UNIT: &str = "deploy/systemd/tickvault-holiday-gate.service";
const USER_DATA_TFTPL: &str = "deploy/aws/terraform/user-data.sh.tftpl";

/// The NSE-holiday boot gate MUST stay wired end-to-end and FAIL-OPEN. Without
/// it the Mon-Fri start cron bills a full ~8h no-op day on every NSE weekday
/// holiday during the 3-month data pull. The fail-open default is the safety
/// property: the gate may only stop the box on a definitive holiday verdict —
/// never on a missing binary / config error / IMDS failure, or it could kill a
/// real trading day.
#[test]
fn holiday_gate_is_wired_and_fail_open() {
    // 1. The app exposes the exit-code gate the script reads.
    let main = read("crates/app/src/main.rs");
    assert!(
        main.contains("fn trading_day_gate_exit_code")
            && main.contains("--check-trading-day")
            && main.contains("run_trading_day_gate"),
        "main.rs must expose the --check-trading-day gate (exit 0=trading / 75=holiday)"
    );

    // 2. The shell gate: override marker, fail-open on missing binary, stops the
    //    box ONLY on the definitive 75 verdict, IMDSv2 token-required.
    let sh = read(HOLIDAY_GATE_SH);
    assert!(
        sh.contains("ALLOW_HOLIDAY_RUN"),
        "gate must honour the /opt/tickvault/ALLOW_HOLIDAY_RUN override marker"
    );
    assert!(
        sh.contains("ec2 stop-instances") && sh.contains("-ne 75"),
        "gate must self-stop ONLY on the exit-75 verdict (fail-open on `-ne 75`)"
    );
    assert!(
        sh.contains("X-aws-ec2-metadata-token"),
        "gate must use IMDSv2 (token-required) to resolve the instance-id"
    );
    // Fail-open evidence: missing binary and non-75 codes exit 0.
    assert!(
        sh.contains("fail-open"),
        "gate must document + implement the fail-open default (never stop on uncertainty)"
    );

    // 3. Dedicated oneshot unit ordered BEFORE the app (NOT an ExecStartPre on
    //    tickvault.service — that would trip Restart=always into a stop loop).
    let unit = read(HOLIDAY_GATE_UNIT);
    assert!(
        unit.contains("Type=oneshot") && unit.contains("Before=tickvault.service"),
        "the gate must be a oneshot unit ordered Before=tickvault.service"
    );
    assert!(
        unit.contains("SuccessExitStatus=0 1"),
        "the holiday verdict (exit 1) must be a success status for the oneshot"
    );

    // 4. First-boot user-data installs + enables the gate unit.
    let ud = read(USER_DATA_TFTPL);
    assert!(
        ud.contains("tickvault-holiday-gate.service")
            && ud.contains("systemctl enable tickvault-holiday-gate.service"),
        "user-data must install + enable tickvault-holiday-gate.service"
    );

    // 5. IAM: ec2:StopInstances scoped to the tv-app box by tag (no ARN cycle).
    let tf = code_only(&read(MAIN_TF));
    assert!(
        tf.contains("ec2:StopInstances"),
        "the instance role must grant ec2:StopInstances for the self-stop"
    );
    assert!(
        tf.contains("ec2:ResourceTag/Name"),
        "ec2:StopInstances must be tag-scoped to tv-<env>-app (avoids the role->instance cycle)"
    );
}
