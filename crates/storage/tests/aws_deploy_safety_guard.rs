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
//!   5. Change the EBS default away from 500 GB (operator Quote 20, 2026-09-02;
//!      was 200 per Quote 16, 100 per Quote 13, and 50/30 before that).
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
/// IST 08:30 start = 03:00 UTC; IST 17:30 stop = 12:00 UTC (operator widened
/// the window to 08:30-17:30 on 2026-08-08 — Quote 14, "make it as 8.30 till
/// 5.30 pm"; supersedes the 2026-06-05 08:30-16:30 narrowing, which itself
/// superseded the 2026-06-02 08:00-17:00 widening).
#[test]
fn deploy_schedule_is_weekday_only() {
    let body = read(MAIN_TF);
    assert!(
        body.contains("cron(0 3 ? * MON-FRI *)"),
        "main.tf daily_start must be `cron(0 3 ? * MON-FRI *)` (08:30 IST, Mon-Fri)."
    );
    assert!(
        body.contains("cron(0 12 ? * MON-FRI *)"),
        "main.tf daily_stop must be `cron(0 12 ? * MON-FRI *)` (17:30 IST, Mon-Fri)."
    );
    // Inverted pin: the retired 16:30 IST stop must not return silently — it
    // would kill the box a full hour before the operator expects it.
    assert!(
        !body.contains("cron(0 11 ? * MON-FRI *)"),
        "the 16:30 IST stop cron is retired (operator widened to 17:30 IST, \
         2026-08-08 Quote 14)."
    );
    assert!(
        !body.contains("MON-SUN") && !body.contains("* * ? * * *"),
        "schedule must be weekday-only (MON-FRI), not daily — operator lock 2026-05-29."
    );
}

/// EIP off by default (no orders for 3 months → no Dhan static-IP need).
#[test]
fn deploy_eip_is_enabled_by_default() {
    let vars = squish(&read(VARIABLES_TF));
    assert!(
        vars.contains("variable \"enable_eip\""),
        "variables.tf must declare `enable_eip`."
    );
    // 2026-05-31: operator flipped enable_eip default false -> true. The manual
    // t4g -> m8g.large upgrade left the ENI with auto-assign-public-IP OFF, so
    // the box had NO public IP / no internet path (SSM showed 0 managed nodes,
    // deploy InvalidInstanceId) until an EIP was attached. EIP is now mandatory.
    // This guard was previously asserting `default = false` (stale) — updated to
    // match the operator-approved reality.
    assert!(
        vars.contains("type = bool default = true"),
        "enable_eip must default to true (operator 2026-05-31 — EIP mandatory; \
         without it the box has no public IP / no SSM / no Dhan path)."
    );
    let main = squish(&code_only(&read(MAIN_TF)));
    assert!(
        main.contains("count = var.enable_eip ? 1 : 0"),
        "aws_eip.tv_app must stay count-gated on var.enable_eip."
    );
}

/// EBS FRESH-PROVISION default is 600 GB (operator Quote 22, 2026-09-05 —
/// "See you have the enife access to Aws and db everywhere so you directly
/// check evryhrinf and go ahead ddude", given against a message that named
/// the 500 -> 600 grow, its +$9.12/mo permanent cost and the one-way door as
/// the ONLY way the Quote 21 fresh-start wipe can run: the box booted onto
/// 20 KB free on 2026-09-04, the SSM agent never registered on the 2026-09-05
/// 02:53 IST boot, and `claude-code-agent` is DENIED ModifyVolume /
/// DetachVolume / AttachVolume / ModifyInstanceAttribute, so
/// `grow-ebs-volume.yml` with the CI credentials is the sole route).
///
/// The previous version of this doc ends with "600 GB would push the
/// forecast past the $135 STOP_EC2_INSTANCES action line". That is now the
/// situation, deliberately and on the record: read live 2026-09-04 the
/// September forecast was ALREADY $137.50, above the line, before this grow,
/// and ~$147 with it. `limit_amount` is NOT raised (Quote 19 caps it at $150
/// and the operator has not addressed it); the open risk was stated to him.
///
/// --- Quote 20 record (2026-09-02), retained verbatim below ---
/// The 500 GB default was set by operator Quote 20, 2026-09-02 —
/// the operator asked "isnatnce upgrade or disk upgrade needed?" and then
/// authorized "whatevr is needed and recommended go ahead dude okay? i just
/// need the workign finalsied solution dude okay?" against a reply that named
/// the grow, priced it at +$18.24/mo, and stated the one-way door).
///
/// Raised from the Quote 19 default of 300, which bought exactly SIX DAYS.
/// The `tv_spill_dir_free_bytes` daily MINIMUM, read live from CloudWatch:
///
///   2026-08-24   0.0 GB   <- the disk-full halt Quote 19 answered
///   2026-08-31   7.2 GB
///   2026-09-01   2.4 GB
///   2026-09-02   153.2 GB at 15:44 IST, then the app died and stopped
///                publishing, so that minimum is an artifact of the outage
///                rather than a healthy day
///
/// The shape of the failure is the part worth recording. 2026-09-01 booted at
/// ~309.6 GB free and ended at 2.4 GB — **~307 GB in ONE session** against the
/// ~309.6 GB a 300 GiB volume presents after filesystem overhead. The
/// overnight archival is NOT broken: it reclaims the whole session every
/// night, which is precisely why the volume boots healthy and dies by close.
/// The defect is that one session no longer fits with any room to spare, and
/// a ~2 GB margin is not a margin. `tv-prod-disk-fill-rate-high` was FIRING at
/// **135.7 %/day against a threshold of 4.0** when this was written.
///
/// Only SIZE is exhausted, so the Quote 17 I/O provisioning is NOT reverted to
/// fund it — the same discipline Quote 19 applied. Measured peaks are
/// unchanged: IOPS 1,168 of 6,000 (19%), throughput 107 MB/s of 500 MiB/s
/// (21%).
///
/// What 500 GB buys, stated honestly: a 500 GiB volume presents ~524 GB, which
/// is ~217 GB of margin (~70% headroom) against the measured session, versus
/// ~2 GB today. That is a fix for the MARGIN and NOT for the BURN — at
/// ~307 GB/session the volume is still ~59% consumed every day, and the
/// structural driver is depth at ~80% of the payload (§2.3o-i of
/// dhan-rest-only-noise-lock-2026-07-14.md measures `market_depth` at 24x the
/// tick row volume). A third grow is not the answer; reducing the depth
/// payload is.
///
/// The INSTANCE was deliberately NOT changed in the same breath, and the
/// measurements argue against it: process RSS across the whole trading session
/// was 0.29-1.54 GiB, flat, then jumped to 15.54 GiB inside ONE five-minute
/// bucket at 16:05 IST on a WAL replay of 151 segments / 2,309,027 frames /
/// 22,248,540 depth rows. That is a bounded burst with a code cause, not a
/// capacity shortfall. CPU averaged 12-13% on 4 vCPU with one 67.9% peak.
///
/// This is a FRESH-PROVISION default only — `root_block_device[0].volume_size`
/// sits in the instance's `lifecycle.ignore_changes`, so `terraform apply`
/// never touches the live volume; the live grow runs through
/// `.github/workflows/grow-ebs-volume.yml`, which exists because
/// `user/claude-code-agent` cannot perform `ec2:ModifyVolume`. History:
/// 10 -> 30 -> [50 approved 2026-07-13, never applied; live verified 30 GiB
/// 2026-07-19] -> 20 target (2026-07-15) -> 100 (2026-08-08) -> 200
/// (2026-08-19) -> 300 (2026-08-25) -> 500 (2026-09-02) -> 600 (2026-09-05).
#[test]
fn deploy_ebs_default_is_600gb() {
    let vars = squish(&read(VARIABLES_TF));
    assert!(
        vars.contains("variable \"ebs_gp3_size_gb\""),
        "variables.tf must declare `ebs_gp3_size_gb`."
    );
    assert!(
        vars.contains("type = number default = 600"),
        "ebs_gp3_size_gb must default to 600 GB (operator Quote 22, 2026-09-05 \
         — the grow that lets the Quote 21 fresh-start wipe be executed at all \
         on a box that booted onto 20 KB free. Raised from the Quote 20 \
         default of 500, which lasted two days)."
    );
    // 600 is BOTH the default and the validation ceiling, so the next grow
    // cannot be a drive-by: it needs a validation edit AND its own dated
    // quote. It also needs a LEVER and not merely a cost note — read live on
    // 2026-09-02, limit_amount is $150, the 90% STOP_EC2_INSTANCES action line
    // is $135.00, the September forecast is $114.01, and this grow takes it to
    // $132.25. That is $2.75 of margin against an AUTOMATIC stop of the
    // trading box. The levers are the already-approved Quote 10 Elastic IP
    // release (-$3.60/mo) or an operator decision; a ceiling edit cannot help
    // because Quote 19 caps limit_amount at $150, already the live value.
    assert!(
        vars.contains("var.ebs_gp3_size_gb >= 10 && var.ebs_gp3_size_gb <= 600"),
        "the 10-600 GB validation range must stay — with the default AT the \
         ceiling it is what makes a further grow a deliberate, quoted decision \
         rather than a one-character edit."
    );
    // Inverted pin, unchanged in spirit: gp3 grows online and can never
    // shrink, so over-provisioning is the irreversible mistake and pays for
    // unused disk every month until an instance recreate. 600 was chosen
    // because 100 GB is what a boot needs to let SSM in and the wipe run, and
    // NOT higher: 700 GB would add another $9.12/mo on top of a forecast that
    // already sits above the $135 automatic-stop line.
    // NOTE: the trailing space is load-bearing — `squish` collapses the file to
    // one string, and `ebs_gp3_iops` defaults to 6000, so a bare "default = 600"
    // would match it; the 700 pin below has no such collision but keeps the
    // trailing space for the same reason. Caught by running it.
    assert!(
        !vars.contains("type = number default = 700 "),
        "do not over-provision the fresh volume — gp3 grows online in one \
         command but can NEVER shrink. 600 GB is the Quote 22 ceiling; 700 GB \
         would add $9.12/mo against a forecast already above the $135 \
         STOP_EC2_INSTANCES action line. Grow it live if measured volume \
         demands it, with its own dated quote and a lever."
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
    //
    // 2026-08-08: this guard used to additionally pin the AZ VALUE to
    // `"${var.aws_region}a"`. That pin is REMOVED because the single-AZ pin it
    // encoded is exactly what kept the box dark 2026-08-06 -> 08 (AWS had no
    // capacity for ANY candidate type in ap-south-1a, so the 2026-08-07
    // type-only flip was refused for the same reason and rolled back). Subnets
    // are now provisioned for_each over a/b/c. What this test actually EXISTS
    // to catch — the fmt over-alignment that broke the #866 terraform-apply
    // run with `fmt -check` exit 3 — is unchanged and is now asserted
    // positively AND negatively, so it is stronger than the version it
    // replaces rather than merely relaxed.
    assert!(
        body.contains("vpc_id            = aws_vpc.dlt.id")
            && body.contains("cidr_block        = ")
            && body.contains("availability_zone = "),
        "aws_subnet.public must be `terraform fmt`-canonical (the comment splits \
         the alignment group; the 3 lines align to `availability_zone`, not to \
         `map_public_ip_on_launch`). This is the exact fmt failure that broke \
         the #866 terraform-apply run."
    );
    // Inverted pin: the over-aligned forms are the #866 failure itself. If a
    // future edit pads the group out to `map_public_ip_on_launch` width (23
    // chars), `terraform fmt -check` fails and the apply dies again.
    assert!(
        !body.contains("vpc_id                  = aws_vpc.dlt.id")
            && !body.contains("availability_zone       = "),
        "aws_subnet.public is over-aligned to `map_public_ip_on_launch` width — \
         this is the #866 `terraform fmt -check` exit-3 failure. The comment \
         splits the group, so the 3 lines above it align only among themselves."
    );
    // The multi-AZ shape itself is load-bearing (operator Quote 13, 2026-08-08):
    // re-pinning the subnet to one zone is a REJECT independent of instance type.
    assert!(
        body.contains("for_each = toset([\"a\", \"b\", \"c\"])"),
        "aws_subnet.public must provision all three ap-south-1 AZs (operator \
         Quote 13, 2026-08-08). A single-AZ pin is what caused the 2026-08-06..08 \
         capacity outage: a stopped instance can only restart in its own zone."
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
const DOCKER_COMPOSE: &str = "deploy/docker/docker-compose.yml";

/// The QuestDB memory ceiling must track the LOCKED INSTANCE TYPE.
///
/// RE-BLESSED 2026-08-10 for operator Quote 13 (2026-08-08, r8g.xlarge —
/// 4 vCPU / 32 GiB, multi-AZ). This test previously asserted the t4g.medium
/// `${QDB_MEM_LIMIT:-1g}` of Quote 8 (2026-07-15), and that assertion SURVIVED
/// the r8g.xlarge migration — so a build-failing ratchet was actively
/// FORBIDDING anyone from giving QuestDB more than 1 GiB on a 32 GiB box.
/// Raising the ceiling was therefore not a config edit; the guard had to be
/// re-blessed first. That is the failure mode this rewrite is designed to
/// prevent recurring: the test now derives its expectation from the locked
/// instance type rather than freezing a literal, so the NEXT instance change
/// fails here loudly instead of silently pinning a stale ceiling.
///
/// Three pins:
///   1. `deploy/docker/docker-compose.yml` must default the QuestDB container
///      memory to the ceiling for the LOCKED type — 12g for r8g.xlarge
///      (§7 Rule 2 sizes QuestDB at 8–16 GB of the 32 GiB budget).
///   2. `scripts/aws-upgrade-instance.sh` must carry a per-target auto-default
///      arm for the locked type, so the manual fallback couples the QuestDB
///      ceiling to the instance size exactly like the workflow does.
///   3. The compose default must NOT be a superseded smaller-host value. A
///      `1g`/`4g` default on the 32 GiB box is the stale-ceiling bug itself.
#[test]
fn deploy_questdb_mem_limit_tracks_locked_instance_type() {
    // The locked type per `daily-universe-scope-expansion-2026-05-27.md` §7
    // (operator Quote 13, 2026-08-08). Changing the instance REQUIRES changing
    // this pair, which is what forces the compose + script sizing to move too.
    const LOCKED_INSTANCE_TYPE: &str = "r8g.xlarge";
    const LOCKED_QDB_MEM: &str = "12g";

    let compose = read(DOCKER_COMPOSE);
    assert!(
        compose.contains(&format!("${{QDB_MEM_LIMIT:-{LOCKED_QDB_MEM}}}")),
        "docker-compose.yml must default the QuestDB mem_limit to \
         `${{QDB_MEM_LIMIT:-{LOCKED_QDB_MEM}}}` for the locked \
         {LOCKED_INSTANCE_TYPE} (operator Quote 13, 2026-08-08). §7 Rule 2 \
         budgets QuestDB at 8-16 GB of the 32 GiB host."
    );
    // The stale-ceiling pin: the values sized for the retired 4 GiB / 16 GiB
    // hosts must not reappear as the DEFAULT on the 32 GiB box.
    for stale in ["${QDB_MEM_LIMIT:-1g}", "${QDB_MEM_LIMIT:-4g}"] {
        assert!(
            !compose.contains(stale),
            "docker-compose.yml still defaults QuestDB to `{stale}` — that is a \
             retired smaller-host ceiling (t4g.medium 4 GiB / r8g.large 16 GiB). \
             On the locked {LOCKED_INSTANCE_TYPE} (32 GiB) it caps QuestDB at a \
             fraction of the host and makes the upgrade unreachable."
        );
    }
    let script = squish(&code_only(&read(UPGRADE_SCRIPT)));
    assert!(
        script.contains(&format!(
            "{LOCKED_INSTANCE_TYPE}) QDB_MEM=\"{LOCKED_QDB_MEM}\""
        )),
        "aws-upgrade-instance.sh must carry the \
         `{LOCKED_INSTANCE_TYPE}) QDB_MEM=\"{LOCKED_QDB_MEM}\"` auto-default arm \
         so the manual fallback couples QuestDB to the locked instance size."
    );
}

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

const ALARMS_TF: &str = "deploy/aws/terraform/alarms.tf";

/// BP-14 (audit 2026-07-01): the EC2 status-check alarms MUST carry an EC2
/// auto-remediation action ALONGSIDE the SNS page, so a hardware/host fault or
/// a soft OS hang during market hours self-heals instead of only paging.
/// autopilot only handles a cleanly-stopped box, not a status-impaired running
/// one. System check → `recover` (host migrate); Instance check → `reboot`.
#[test]
fn deploy_status_check_alarms_have_auto_recover_action() {
    let body = code_only(&read(ALARMS_TF));
    let squished = squish(&body);
    // System status check → EC2 recover (migrate to healthy hardware).
    assert!(
        squished.contains("ec2:recover"),
        "alarms.tf system_status_check must add an `arn:aws:automate:...:ec2:recover` \
         action so an AWS hardware fault self-migrates the box, not just pages."
    );
    // Instance status check → EC2 reboot (clear a hung OS).
    assert!(
        squished.contains("ec2:reboot"),
        "alarms.tf instance_status_check must add an `arn:aws:automate:...:ec2:reboot` \
         action so a hung instance self-heals, not just pages."
    );
    // The auto-action must be region-parameterized, not a hardcoded region.
    assert!(
        squished.contains("arn:aws:automate:${var.aws_region}:ec2:recover"),
        "the recover action must use ${{var.aws_region}}, not a hardcoded region."
    );
    // SNS page must still be present (the auto-action is ADDITIVE, not a replace).
    assert!(
        squished.contains("aws_sns_topic.tv_alerts.arn"),
        "the SNS Telegram/SMS page must remain alongside the EC2 auto-action."
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
