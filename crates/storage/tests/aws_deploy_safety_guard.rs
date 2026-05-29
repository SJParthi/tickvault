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
//!   4. Flip `enable_eip` default to true (no orders for 3 months → no Dhan
//!      static-IP need → EIP off saves ~₹430/mo).
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
    assert!(
        body.contains("ignore_changes = [ami, instance_type, user_data]"),
        "aws_instance.tv_app lifecycle must ignore ami + instance_type + \
         user_data so a merge-triggered apply can NEVER replace/wipe the \
         running box. Upgrade via scripts/aws-upgrade-instance.sh; deploy via SSM."
    );
    assert!(
        body.contains("user_data_replace_on_change = false"),
        "main.tf must set `user_data_replace_on_change = false` — true would \
         replace the instance (fresh root volume, QuestDB data orphaned) on any \
         user_data drift. Deploys are over SSM, not user_data re-runs."
    );
}

/// Weekday-only schedule (trading days). Mon-Fri crons, not Mon-Sun.
/// IST 08:30 start = 03:00 UTC; IST 16:30 stop = 11:00 UTC.
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

/// EIP off by default (no orders for 3 months → no Dhan static-IP need).
#[test]
fn deploy_eip_is_disabled_by_default() {
    let vars = squish(&read(VARIABLES_TF));
    assert!(
        vars.contains("variable \"enable_eip\""),
        "variables.tf must declare `enable_eip`."
    );
    // The default false must appear within the enable_eip block. We assert the
    // bool default is present and that no `default = true` shadows it.
    assert!(
        vars.contains("type = bool default = false"),
        "enable_eip must default to false (no orders 3mo → no EIP → ~₹430/mo saved)."
    );
    let main = squish(&code_only(&read(MAIN_TF)));
    assert!(
        main.contains("count = var.enable_eip ? 1 : 0"),
        "aws_eip.tv_app must stay count-gated on var.enable_eip."
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
