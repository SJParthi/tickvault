//! Execute the actual deployment and Terraform stop-fence steps with fake AWS.
//! No cloud calls, activation, inherited-environment clearing, or real host edits.
#![cfg(test)]
#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
static NEXT: AtomicU64 = AtomicU64::new(0);

fn workflow() -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(root.join("../../.github/workflows/deploy-aws.yml")).unwrap()
}

fn step_run(source: &str, marker: &str) -> String {
    let (_, tail) = source.split_once(marker).expect("known deployment step");
    let step = tail.split("\n      - name:").next().unwrap();
    let (_, body) = step.split_once("\n        run: |\n").expect("step script");
    let script = body
        .lines()
        .map(|line| line.strip_prefix("          ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!script.trim().is_empty());
    assert!(!script.contains("${{"), "fixture must execute actual shell");
    script
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "tickvault-deploy-guard-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("bin")).unwrap();
        let fixture = Self(path);
        fixture.script("aws", AWS);
        fixture.script("date", DATE);
        fixture.script("sleep", "exit 0\n");
        fixture
    }
    fn script(&self, name: &str, body: &str) {
        let path = self.0.join("bin").join(name);
        fs::write(&path, format!("#!/bin/bash -p\nset -euo pipefail\n{body}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn command(&self, script: &str) -> Command {
        let mut command = Command::new("bash");
        command
            .args(["--noprofile", "--norc", "-p", "-c", script])
            .current_dir(&self.0)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.0.join("bin").display()),
            )
            .env("FIXTURE", &self.0)
            .env("GITHUB_ENV", self.0.join("environment"))
            .env("GITHUB_OUTPUT", self.0.join("outputs"))
            .env("REGION", "ap-south-1")
            .env("AWS_REGION_EFF", "ap-south-1")
            .env("IID", "i-owned-fixture")
            .env("EVENT_NAME", "workflow_dispatch")
            .env("DEPLOY_SHA", SHA)
            .env("BUDGET_MODE", "absent")
            .env("HOLIDAY_MODE", "absent")
            .env("TICKVAULT_TEST_REAL_SSM", "0")
            .env("AWS_EC2_METADATA_DISABLED", "true");
        // Preserve runtime, identity, proxy and CA guards. Only these four
        // approved provider secrets are removed from the owned child fixture.
        for key in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "TICKVAULT_OPERATOR_KEY",
        ] {
            command.env_remove(key);
        }
        command
    }
    fn ensure(&self, budget: &str, holiday: &str) -> Output {
        self.command(&step_run(
            &workflow(),
            "- name: Ensure instance running (self-start + wait for SSM online)",
        ))
        .env("BUDGET_MODE", budget)
        .env("HOLIDAY_MODE", holiday)
        .output()
        .unwrap()
    }
    fn text(&self, name: &str) -> String {
        fs::read_to_string(self.0.join(name)).unwrap_or_default()
    }
    fn ssm_commands(&self) -> Vec<String> {
        let output = self
            .command(&step_run(&workflow(), "        id: ssm\n"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let payload = self.text("parameters");
        serde_json::from_str(payload.strip_prefix("commands=").unwrap()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const DATE: &str = r#"
case "$*" in
  '+%u') echo 1;;
  '+%H%M') echo 1745;;
  '-u +%Y-%m') echo 2026-11;;
  '+%F') echo 2026-11-02;;
  *) exec /usr/bin/date "$@";;
esac
"#;

const AWS: &str = r#"
printf '%s\n' "$1 $2" >> "$FIXTURE/calls"
case "$1 $2" in
  'ec2 describe-instances') if [ -f "$FIXTURE/started" ] || [ "${INITIAL_STATE:-stopped}" = running ]; then echo running; else echo stopped; fi;;
  'ec2 start-instances') printf started > "$FIXTURE/started";;
  'ssm describe-instance-information') echo Online;;
  'ssm send-command')
    while [ "$#" -gt 0 ]; do
      if [ "$1" = --parameters ]; then printf '%s' "$2" > "$FIXTURE/parameters"; break; fi
      shift
    done
    echo owned-command;;
  'ssm get-parameter')
    mode="$BUDGET_MODE"; past=2026-10; current=2026-11; invalid=2026-13; five=10000-01
    case "$*" in *holiday-stop-date*) mode="$HOLIDAY_MODE"; past=2026-11-01; current=2026-11-02; invalid=2026-02-30; five=10000-01-01;; esac
    missing='An error occurred (ParameterNotFound) when calling the GetParameter operation: PRIVATE_MARKER'
    case "$mode" in
      absent) echo "$missing" >&2; exit 254;;
      absent_v1) echo "$missing" >&2; exit 255;;
      absent_cli_v2) printf '\naws: [ERROR]: %s\n' "$missing" >&2; exit 254;;
      old) echo "$past";;
      future) echo 2026-12;;
      current) echo "$current";;
      empty) :;;
      none) echo None;;
      whitespace) echo " $past";;
      trailing_newline) printf '%s\n\n' "$past";;
      oversized_newlines) printf '%s' "$past"; printf '%5000s' '' | tr ' ' '\n';;
      oversized_multibyte_error) printf '%s' "$missing" >&2; printf '\303\251%.0s' {1..2100} >&2; exit 254;;
      oversized_missing_newlines) printf '%s' "$missing" >&2; printf '%5000s' '' | tr ' ' '\n' >&2; exit 254;;
      multiple) printf '%s\n%s\n' "$past" "$current";;
      invalid) echo "$invalid";;
      five_digit_year) echo "$five";;
      nul) printf '%s\000' "$past";;
      denied) echo 'An error occurred (AccessDeniedException) when calling the GetParameter operation: PRIVATE_MARKER ParameterNotFound' >&2; exit 254;;
      denied_cli_v2) echo 'aws: [ERROR]: An error occurred (AccessDeniedException) when calling the GetParameter operation: PRIVATE_MARKER ParameterNotFound' >&2; exit 254;;
      throttled) echo 'An error occurred (ThrottlingException) when calling the GetParameter operation: PRIVATE_MARKER' >&2; exit 254;;
      timeout) echo "$missing" >&2; exit 124;;
      terminated) echo "$missing" >&2; exit 143;;
      misleading) echo "$missing" >&2; exit 1;;
      prefix) echo "PRIVATE_MARKER $missing" >&2; exit 254;;
      oversized) printf '%05000d' 0;;
      oversized_error) printf '%s%05000d' "$missing" 0 >&2; exit 254;;
      *) exit 99;;
    esac;;
  *) exit 98;;
esac
"#;

#[test]
fn test_regression_deploy_confirmed_absence_and_valid_past_markers_allow_one_start() {
    for mode in ["absent", "absent_v1", "absent_cli_v2", "old"] {
        let fixture = Fixture::new();
        let output = fixture.ensure(mode, mode);
        assert!(output.status.success(), "{mode}: {output:?}");
        assert_eq!(
            fixture.text("calls").matches("ec2 start-instances").count(),
            1
        );
        assert!(
            fixture
                .text("environment")
                .contains("DEPLOY_STARTED_STOPPED_BOX=yes")
        );
        assert!(!fixture.text("environment").contains("DEPLOY_SKIP_REASON"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE_MARKER"));
    }
}

#[test]
fn test_regression_deploy_matching_stop_markers_preserve_stopped_state() {
    for (budget, holiday, reason) in [
        ("current", "absent", "budget_stop"),
        ("old", "current", "holiday_stop"),
    ] {
        let fixture = Fixture::new();
        let output = fixture.ensure(budget, holiday);
        assert!(output.status.success(), "{output:?}");
        assert!(!fixture.0.join("started").exists());
        assert!(
            fixture
                .text("environment")
                .contains(&format!("DEPLOY_SKIP_REASON={reason}"))
        );
        assert!(
            !fixture
                .text("environment")
                .contains("DEPLOY_STARTED_STOPPED_BOX")
        );
    }
}

#[test]
fn test_regression_deploy_unknown_or_malformed_marker_never_starts_or_logs_body() {
    for mode in [
        "empty",
        "none",
        "trailing_newline",
        "oversized_newlines",
        "oversized_missing_newlines",
        "oversized_multibyte_error",
        "invalid",
        "five_digit_year",
        "nul",
        "denied",
        "denied_cli_v2",
        "throttled",
        "timeout",
        "terminated",
        "misleading",
        "prefix",
        "oversized",
        "oversized_error",
    ] {
        assert!(
            AWS.contains(&format!("{mode})")),
            "fixture must generate {mode}"
        );
        for holiday in [false, true] {
            let fixture = Fixture::new();
            let output = if holiday {
                fixture.ensure("old", mode)
            } else {
                fixture.ensure(mode, "old")
            };
            assert!(
                !output.status.success(),
                "{mode}, holiday={holiday}: {output:?}"
            );
            assert!(
                !fixture.0.join("started").exists(),
                "{mode}, holiday={holiday}"
            );
            assert!(
                !fixture
                    .text("environment")
                    .contains("DEPLOY_STARTED_STOPPED_BOX")
            );
            let logs = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(logs.contains("DEPLOY_FENCE"));
            assert!(
                !logs.contains("PRIVATE_MARKER"),
                "raw provider error reached logs"
            );
        }
    }
}

#[test]
fn test_regression_running_or_automatic_off_hours_does_not_claim_a_start() {
    let script = step_run(
        &workflow(),
        "- name: Ensure instance running (self-start + wait for SSM online)",
    );
    for running in [false, true] {
        let fixture = Fixture::new();
        let output = fixture
            .command(&script)
            .env("INITIAL_STATE", if running { "running" } else { "stopped" })
            .env("EVENT_NAME", "push")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(!fixture.0.join("started").exists());
        assert!(
            !fixture
                .text("environment")
                .contains("DEPLOY_STARTED_STOPPED_BOX")
        );
        assert!(!fixture.text("calls").contains("get-parameter"));
        assert_eq!(
            fixture
                .text("environment")
                .contains("DEPLOY_SKIP_REASON=intentional_stopped"),
            !running
        );
    }
}

#[test]
fn test_regression_deploy_invalid_sha_fails_before_ssm_and_no_shell_expansion() {
    let script = step_run(&workflow(), "        id: ssm\n");
    for sha in [
        "",
        "short",
        "0123456789ABCDEF0123456789ABCDEF01234567",
        "$(touch injected)",
        "a; touch injected",
    ] {
        let fixture = Fixture::new();
        let output = fixture
            .command(&script)
            .env("DEPLOY_SHA", sha)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{sha:?}: {output:?}");
        assert!(fixture.text("calls").is_empty());
        assert!(!fixture.0.join("injected").exists());
    }
}

#[test]
fn test_regression_deploy_snapshots_before_overwrite_and_pins_source_and_binaries() {
    let fixture = Fixture::new();
    let commands = fixture.ssm_commands();
    assert_eq!(commands[0], format!("set -euo pipefail; DEPLOY_SHA={SHA}"));
    let position = |needle: &str| {
        commands
            .iter()
            .position(|line| line.contains(needle))
            .expect(needle)
    };
    let backup = position("bin/.next/smoke_test --preserve-deploy");
    let config = position("cp -f repo/config/*.toml config/");
    assert!(position("TVS3=s3://tv-prod-cold/deploys/$DEPLOY_SHA") < backup);
    for needle in ["$TVS3/tickvault", "$TVS3/smoke_test", "$TVS3/tv-movers"] {
        assert!(position(needle) < backup);
    }
    for needle in [
        "chown -R ec2-user:ec2-user /opt/tickvault",
        "git -C repo reset --hard",
        "cp -f repo/config/",
        "/etc/systemd/system/tickvault-host-tuning.service",
        "mv tickvault smoke_test tv-movers",
        "TV_QDB_HTTP_BIND=",
    ] {
        assert!(
            backup < position(needle),
            "overwrite before private snapshot: {needle}"
        );
    }
    assert!(commands[backup].contains("timeout -k 5 120"));
    assert!(config < position("bin/.next/smoke_test --duration 30"));
    assert!(
        position("bin/.next/smoke_test --duration 30")
            < position("mv tickvault smoke_test tv-movers")
    );
    let checkout = &commands[position("git -C repo fetch")];
    assert!(checkout.contains("fetch --depth 1 origin $DEPLOY_SHA"));
    assert!(checkout.contains("reset --hard $DEPLOY_SHA"));
    assert!(checkout.contains("test \"$TV_REV\" = $DEPLOY_SHA || exit 1"));
    assert!(!checkout.contains("origin/main"));
}

#[test]
fn test_regression_deploy_fetch_reset_or_identity_failure_prevents_config_copy() {
    for mode in ["success", "fetch", "reset", "identity", "rev_parse"] {
        let fixture = Fixture::new();
        let commands = fixture.ssm_commands();
        let checkout = commands
            .iter()
            .find(|line| line.contains("git -C repo fetch"))
            .unwrap();
        fixture.script("sudo", "[ \"$1\" = -u ]; shift 2; exec \"$@\"\n");
        fixture.script("git", r#"
case "$3" in
  fetch) [ "$MODE" != fetch ];;
  reset) [ "$MODE" != reset ];;
  rev-parse) [ "$MODE" != rev_parse ]; if [ "$MODE" = identity ]; then echo 9999999999999999999999999999999999999999; else echo "$DEPLOY_SHA"; fi;;
  *) exit 90;;
esac
"#);
        let output = fixture
            .command(&format!(
                "set -euo pipefail\n{checkout}\nprintf copied > copied\n"
            ))
            .env("MODE", mode)
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            mode == "success",
            "{mode}: {output:?}"
        );
        assert_eq!(fixture.0.join("copied").exists(), mode == "success");
    }
}

#[test]
fn test_regression_backup_branch_precedes_all_normal_smoke_actions() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(root.join("../app/src/bin/smoke_test.rs")).unwrap();
    let main = source.split_once("fn main()").expect("smoke entry point").1;
    let branch = main.find("Some(\"--preserve-deploy\")").unwrap();
    let preserve = main
        .find("return deploy_backup::preserve_deployment")
        .unwrap();
    let normal = main.find("let start = Instant::now()").unwrap();
    assert!(branch < preserve && preserve < normal);
    assert!(main[..normal].contains("std::env::args().count() != 2"));
    let ensure = step_run(
        &workflow(),
        "- name: Ensure instance running (self-start + wait for SSM online)",
    );
    assert!(ensure.contains("timeout -k 5 20 \"$@\""));
    assert!(ensure.contains("head -c 4097"));
    assert!(ensure.contains("rc=$?; printf '.'; exit \"$rc\""));
    assert!(ensure.contains("LC_ALL=C expected=10"));
}

fn terraform_budget_steps() -> Vec<String> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source =
        fs::read_to_string(root.join("../../.github/workflows/terraform-apply.yml")).unwrap();
    let marker = "- name: Read the budget-stop latch";
    let steps: Vec<_> = source
        .split(marker)
        .skip(1)
        .map(|tail| step_run(&format!("{marker}{tail}"), marker))
        .collect();
    assert_eq!(
        steps.len(),
        2,
        "both planning and applying must retain the fence"
    );
    steps
}

#[test]
fn test_regression_terraform_budget_confirmed_absence_and_valid_nonmatching_keep_default() {
    for step in terraform_budget_steps() {
        for mode in ["absent", "absent_v1", "absent_cli_v2", "old", "future"] {
            let fixture = Fixture::new();
            let output = fixture
                .command(&step)
                .env("BUDGET_MODE", mode)
                .output()
                .unwrap();
            assert!(output.status.success(), "{mode}: {:?}", output.status);
            assert!(
                fixture.text("environment").is_empty(),
                "no unrequested enable/disable override"
            );
            assert_eq!(fixture.text("calls"), "ssm get-parameter\n");
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(text.contains("TF_BUDGET_FENCE confirmed absent or nonmatching"));
            assert!(!text.contains("PRIVATE_MARKER"));
        }
    }
}

#[test]
fn test_regression_terraform_budget_current_month_disables_daily_start_in_both_jobs() {
    for step in terraform_budget_steps() {
        let fixture = Fixture::new();
        let output = fixture
            .command(&step)
            .env("BUDGET_MODE", "current")
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.status);
        assert_eq!(
            fixture.text("environment"),
            "TF_VAR_daily_start_enabled=false\n"
        );
        assert_eq!(fixture.text("calls"), "ssm get-parameter\n");
    }
}

#[test]
fn test_regression_terraform_budget_unknown_or_malformed_never_reaches_plan_or_apply() {
    for step in terraform_budget_steps() {
        for mode in [
            "empty",
            "none",
            "whitespace",
            "trailing_newline",
            "oversized_newlines",
            "oversized_missing_newlines",
            "oversized_multibyte_error",
            "multiple",
            "invalid",
            "five_digit_year",
            "nul",
            "denied",
            "denied_cli_v2",
            "throttled",
            "timeout",
            "terminated",
            "misleading",
            "prefix",
            "oversized",
            "oversized_error",
        ] {
            let fixture = Fixture::new();
            let script = format!("{step}\nprintf reached > next-step\n");
            let output = fixture
                .command(&script)
                .env("BUDGET_MODE", mode)
                .output()
                .unwrap();
            assert!(!output.status.success(), "{mode}: {:?}", output.status);
            assert!(fixture.text("environment").is_empty());
            assert!(
                !fixture.0.join("next-step").exists(),
                "{mode} crossed the fence"
            );
            assert_eq!(fixture.text("calls"), "ssm get-parameter\n");
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(text.contains("::error::TF_BUDGET_FENCE"), "{mode}");
            assert!(
                !text.contains("PRIVATE_MARKER"),
                "{mode} leaked provider body"
            );
        }
    }
}

#[test]
fn test_regression_terraform_budget_read_retains_bounded_single_parameter_surface() {
    for step in terraform_budget_steps() {
        assert!(step.contains("set -euo pipefail; export LC_ALL=C"));
        assert!(step.contains("timeout -k 5 20 aws ssm get-parameter"));
        assert!(step.contains("head -c 4097"));
        assert!(step.contains("RC=$?; printf '.'; exit \"$RC\""));
        assert_eq!(
            step.matches("/tickvault-guard/prod/budget-stop-month")
                .count(),
            1
        );
        assert!(!step.contains("--with-decryption"));
        assert!(!step.contains("TF_VAR_daily_start_enabled=true"));
    }
}
