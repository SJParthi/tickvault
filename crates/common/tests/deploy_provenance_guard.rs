//! B9 deploy provenance — source-scan ratchet.
//!
//! Pins every wiring point of the provenance chain documented in
//! `.claude/rules/project/deploy-provenance.md`:
//!
//! ```text
//! TICKVAULT_BUILD_GIT_SHA/git ─▶ build.rs ─▶ BUILD_GIT_SHA ─▶ /health + boot Telegram
//! deploy-aws.yml ─▶ SSM binary-git-sha ─▶ portal footer + watchdog metric
//!                                          ─▶ 24h binary-sha-stale alarm
//! terraform ─▶ portal_git_sha var/output + binary_git_sha_ssm_param output
//! ```
//!
//! Deleting or renaming ANY link in that chain fails the build here first,
//! so a silent partial rollback is impossible (a deliberate rollback must
//! also revert this guard + the rule file in the same commit).

#![cfg(test)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

// Execute the real deployment commands with a clean environment. macOS needs
// its installed GNU coreutils and jq; /usr/bin alone supplies neither. Keep
// these explicit tool locations instead of inheriting credentials or PATH.
#[cfg(target_os = "macos")]
const FIXTURE_TOOL_PATH: &str = "/opt/homebrew/opt/coreutils/libexec/gnubin:/usr/local/opt/coreutils/libexec/gnubin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";
#[cfg(not(target_os = "macos"))]
const FIXTURE_TOOL_PATH: &str = "/usr/bin:/bin";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/common parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// 1+2 — the build script resolves the dedicated TICKVAULT_BUILD_GIT_SHA
/// env var (NEVER GitHub's own GITHUB_SHA — a rerun-if-env-changed on
/// GITHUB_SHA kills the target cache in every CI workflow, 2026-07-03
/// adversarial-review HIGH fix) and embeds TICKVAULT_GIT_SHA, and
/// build_info exposes it as BUILD_GIT_SHA.
#[test]
fn build_script_embeds_git_sha() {
    let root = repo_root();
    let build_rs = read(&root.join("crates/common/build.rs"));
    assert!(
        build_rs.contains("TICKVAULT_BUILD_GIT_SHA"),
        "build.rs must resolve the dedicated TICKVAULT_BUILD_GIT_SHA env var"
    );
    assert!(
        !build_rs.contains("rerun-if-env-changed=GITHUB_SHA"),
        "build.rs must NOT register rerun-if-env-changed on GITHUB_SHA — \
         it changes every commit in EVERY workflow and invalidates the \
         restored target cache (full workspace rebuild per commit)"
    );
    assert!(
        build_rs.contains("TICKVAULT_GIT_SHA"),
        "build.rs must emit cargo:rustc-env=TICKVAULT_GIT_SHA"
    );

    let build_info = read(&root.join("crates/common/src/build_info.rs"));
    assert!(
        build_info.contains("BUILD_GIT_SHA"),
        "build_info.rs must expose BUILD_GIT_SHA"
    );
    assert!(
        build_info.contains("env!(\"TICKVAULT_GIT_SHA\")"),
        "BUILD_GIT_SHA must come from the build-script env"
    );
}

/// 3 — /health reports the embedded SHA.
#[test]
fn health_endpoint_reports_git_sha() {
    let root = repo_root();
    let health = read(&root.join("crates/api/src/handlers/health.rs"));
    assert!(
        health.contains("git_sha"),
        "/health HealthResponse must carry the git_sha field"
    );
    assert!(
        health.contains("BUILD_GIT_SHA"),
        "/health git_sha must be populated from build_info::BUILD_GIT_SHA"
    );
}

/// 4 — the boot Telegram carries the short SHA.
#[test]
fn boot_telegram_carries_build_sha() {
    let root = repo_root();
    let events = read(&root.join("crates/core/src/notification/events.rs"));
    assert!(
        events.contains("build_git_sha_short"),
        "StartupComplete to_message must render build_info::build_git_sha_short()"
    );
    assert!(
        events.contains("Build: {build}"),
        "StartupComplete message must carry the 'Build: <sha>' line"
    );
}

/// 5 — the deploy workflow records the deployed SHA to SSM after a
/// verified swap (non-fatally), and the OIDC role can write it.
#[test]
fn deploy_workflow_records_binary_sha_to_ssm() {
    let root = repo_root();
    let workflow = read(&root.join(".github/workflows/deploy-aws.yml"));
    assert!(
        workflow.contains("/tickvault/prod/deploy/binary-git-sha"),
        "deploy-aws.yml must write the binary-git-sha SSM param post-swap"
    );
    assert!(
        workflow.contains("ssm put-parameter"),
        "deploy-aws.yml must use aws ssm put-parameter for the provenance write"
    );
    assert!(
        workflow.contains("provenance param write failed"),
        "the SSM provenance write must be NON-FATAL with a visible warning"
    );
    assert!(
        workflow.contains("TICKVAULT_BUILD_GIT_SHA"),
        "deploy-aws.yml build steps must set TICKVAULT_BUILD_GIT_SHA so the \
         shipped + smoke binaries embed the exact deploy sha"
    );
    assert!(
        workflow.contains("[0-9a-f]{40}"),
        "the SSM provenance write must hex-validate GITHUB_SHA before \
         recording it (refuse non-hex, still non-fatal)"
    );

    let oidc = read(&root.join("deploy/aws/terraform/oidc.tf"));
    assert!(
        oidc.contains("ssm:PutParameter") && oidc.contains("deploy/binary-git-sha"),
        "oidc.tf must grant ssm:PutParameter on exactly the binary-git-sha param"
    );
}

fn deploy_command_array(workflow: &str) -> Vec<String> {
    let (_, literal) = workflow
        .split_once("--argjson commands '[")
        .expect("literal deployment command array");
    let (literal, _) = literal
        .split_once("]' '{commands:")
        .expect("end of literal deployment command array");
    serde_json::from_str(&format!("[{literal}]"))
        .expect("deployment commands must be valid JSON strings")
}

struct DeployFixture(PathBuf);

impl DeployFixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tickvault-deploy-provenance-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("create isolated deployment fixture");
        Self(path)
    }
}

impl Drop for DeployFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Exercise the actual runner script with a synthetic AWS function. Its command
/// array is captured, never dispatched: no AWS access, service changes, or WAL
/// migration occurs. Invalid input must fail before calling even the stub.
#[test]
fn deploy_sha_transport_is_literal_and_rejects_non_commit_input() {
    let workflow = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    let (_, step) = workflow.split_once("        id: ssm\n").expect("SSM step");
    let (step, _) = step.split_once("\n      - name:").expect("next step");
    let (_, run) = step.split_once("        run: |\n").expect("SSM script");
    let run = run
        .lines()
        .map(|line| line.strip_prefix("          ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!run.contains("${{"), "large SSM script must use env values");
    let stub = r#"
aws() {
  while [ "$#" -gt 0 ]; do
    if [ "$1" = --parameters ]; then
      shift
      printf '%s' "$1" > "$TV_CAPTURE"
      printf fixture-command
      return 0
    fi
    shift
  done
  return 91
}
"#;
    let valid_sha = "1234567890abcdef1234567890abcdef12345678";
    let valid_rollback = format!("{valid_sha}-42-1");
    let valid_digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    for (sha, rollback_id, digest, accepted) in [
        (valid_sha, valid_rollback.as_str(), valid_digest, true),
        ("main", valid_rollback.as_str(), valid_digest, false),
        ("", valid_rollback.as_str(), valid_digest, false),
        (
            "$(touch forbidden)",
            valid_rollback.as_str(),
            valid_digest,
            false,
        ),
        (valid_sha, "", valid_digest, false),
        (
            valid_sha,
            "0000000000000000000000000000000000000000-42-1",
            valid_digest,
            false,
        ),
        (valid_sha, "$(touch forbidden)", valid_digest, false),
        (valid_sha, valid_rollback.as_str(), "", false),
        (
            valid_sha,
            valid_rollback.as_str(),
            "$(touch forbidden)",
            false,
        ),
    ] {
        let fixture = DeployFixture::new();
        let capture = fixture.0.join("parameters.json");
        let result = Command::new("bash")
            .args(["--noprofile", "--norc", "-c", &format!("{stub}\n{run}")])
            .current_dir(&fixture.0)
            .env_clear()
            .env("PATH", FIXTURE_TOOL_PATH)
            .env("DEPLOY_SHA", sha)
            .env("DEPLOY_ROLLBACK_ID", rollback_id)
            .env("DEPLOY_TICKVAULT_SHA256", digest)
            .env("DEPLOY_SMOKE_SHA256", valid_digest)
            .env("DEPLOY_MIGRATION_SHA256", valid_digest)
            .env("AWS_REGION_EFF", "ap-south-1")
            .env("IID", "fixture-instance")
            .env("GITHUB_OUTPUT", fixture.0.join("output"))
            .env("TV_CAPTURE", &capture)
            .output()
            .expect("execute isolated deployment transport fixture");
        assert_eq!(result.status.success(), accepted);
        assert_eq!(capture.exists(), accepted);
        assert!(!fixture.0.join("forbidden").exists());
        if accepted {
            let payload: serde_json::Value =
                serde_json::from_str(&read(&capture)).expect("SSM parameter JSON");
            let commands: Vec<String> = serde_json::from_value(payload["commands"].clone())
                .expect("SSM string command array");
            assert_eq!(commands[0], format!("DEPLOY_SHA={valid_sha}"));
            assert_eq!(commands[1], format!("ROLLBACK_ID={valid_rollback}"));
            assert_eq!(
                commands[2],
                format!("EXPECTED_TICKVAULT_SHA256={valid_digest}")
            );
            assert_eq!(commands[3], format!("EXPECTED_SMOKE_SHA256={valid_digest}"));
            assert_eq!(
                commands[4],
                format!("EXPECTED_MIGRATION_SHA256={valid_digest}")
            );
            assert_eq!(&commands[5..], deploy_command_array(&workflow).as_slice());
        }
    }
}

/// Run the exact fetched-object/checkout commands against a local Git fixture
/// whose main has advanced. A missing commit must fail without moving HEAD.
#[test]
fn deployed_config_uses_requested_commit_even_after_main_advances() {
    let workflow = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    let commands = deploy_command_array(&workflow);
    let fetch = commands
        .iter()
        .position(|line| line.contains("git -C repo fetch --depth 1 origin"))
        .expect("exact commit fetch");
    let verified = commands
        .iter()
        .position(|line| line.starts_with("echo DEPLOY-SOURCE:"))
        .expect("checked commit and tree");
    let copy = commands
        .iter()
        .position(|line| line == "cp -f repo/config/*.toml config/")
        .expect("config install");
    assert!(fetch < verified && verified < copy);
    let pin = commands[fetch..=verified].join("\n");
    assert!(!pin.contains("origin/main"));
    let fixture = DeployFixture::new();
    let script = format!(
        r#"
set -euo pipefail
git init --quiet --initial-branch=main origin
git -C origin config user.email fixture@example.invalid
git -C origin config user.name Fixture
mkdir origin/config
printf requested > origin/config/production.toml
git -C origin add config/production.toml
git -C origin commit --quiet -m requested
REQUESTED=$(git -C origin rev-parse HEAD)
REQUESTED_TREE=$(git -C origin rev-parse HEAD^{{tree}})
printf advanced > origin/config/production.toml
git -C origin commit --quiet -am advanced
ADVANCED=$(git -C origin rev-parse HEAD)
git clone --quiet --no-checkout "file://$PWD/origin" repo
sudo() {{ test "$1" = -u; shift 2; "$@"; }}
DEPLOY_SHA=$REQUESTED
{pin}
test "$(git -C repo rev-parse HEAD)" = "$REQUESTED"
test "$(git -C repo rev-parse HEAD^{{tree}})" = "$REQUESTED_TREE"
test "$(cat repo/config/production.toml)" = requested
test "$(git -C origin rev-parse main)" = "$ADVANCED"
set +e
(
  set -e
  DEPLOY_SHA=0000000000000000000000000000000000000000
  {pin}
)
REJECTED=$?
set -e
test "$REJECTED" -ne 0
test "$(git -C repo rev-parse HEAD)" = "$REQUESTED"
"#
    );
    let result = Command::new("bash")
        .args(["--noprofile", "--norc", "-c", &script])
        .current_dir(&fixture.0)
        .env_clear()
        .env("PATH", FIXTURE_TOOL_PATH)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("execute isolated local Git deployment fixture");
    assert!(
        result.status.success(),
        "source pinning fixture failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn staged_artifact_digests_are_checked_before_any_binary_executes() {
    let workflow = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    let commands = deploy_command_array(&workflow);
    let check = commands
        .iter()
        .position(|line| line.contains("FATAL: staged binary differs"))
        .expect("staged artifact digest gate");
    let execute = commands
        .iter()
        .position(|line| line.contains("timeout 10 bin/tv-wal-sequence-migrate.new --help"))
        .expect("first staged binary execution");
    assert!(check < execute);
    let fixture = DeployFixture::new();
    let script = format!(
        r#"
set -euo pipefail
mkdir bin
printf app > bin/tickvault.new
printf smoke > bin/smoke_test.new
printf migrate > bin/tv-wal-sequence-migrate.new
EXPECTED_TICKVAULT_SHA256=$(sha256sum bin/tickvault.new | cut -d ' ' -f 1)
EXPECTED_SMOKE_SHA256=$(sha256sum bin/smoke_test.new | cut -d ' ' -f 1)
EXPECTED_MIGRATION_SHA256=$(sha256sum bin/tv-wal-sequence-migrate.new | cut -d ' ' -f 1)
{guard}
for binary in tickvault smoke_test tv-wal-sequence-migrate; do
  cp "bin/$binary.new" original
  printf replaced > "bin/$binary.new"
  if ( {guard} ); then exit 90; fi
  cp original "bin/$binary.new"
done
rm bin/tickvault.new
if ( {guard} ); then exit 91; fi
"#,
        guard = commands[check]
    );
    let result = Command::new("bash")
        .args(["--noprofile", "--norc", "-c", &script])
        .current_dir(&fixture.0)
        .env_clear()
        .env("PATH", FIXTURE_TOOL_PATH)
        .output()
        .expect("execute isolated staged binary identity fixture");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn installed_base_and_production_must_match_the_commit_blobs() {
    let workflow = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    let commands = deploy_command_array(&workflow);
    let guard = commands
        .iter()
        .find(|line| line.starts_with("for CONFIG_FILE in base.toml production.toml;"))
        .expect("both installed config files must be verified");
    let fixture = DeployFixture::new();
    let script = format!(
        r#"
set -euo pipefail
git init --quiet --initial-branch=main repo
git -C repo config user.email fixture@example.invalid
git -C repo config user.name Fixture
mkdir repo/config config
printf base > repo/config/base.toml
printf production > repo/config/production.toml
git -C repo add config
git -C repo commit --quiet -m requested
DEPLOY_SHA=$(git -C repo rev-parse HEAD)
cp repo/config/*.toml config/
sudo() {{ test "$1" = -u; shift 2; "$@"; }}
{guard}
for config_file in base.toml production.toml; do
  cp "config/$config_file" original
  printf stale > "config/$config_file"
  # Even a matching drifted worktree file cannot replace the pinned blob.
  printf stale > "repo/config/$config_file"
  if ( {guard} ); then exit 90; fi
  cp original "config/$config_file"
done
"#
    );
    let result = Command::new("bash")
        .args(["--noprofile", "--norc", "-c", &script])
        .current_dir(&fixture.0)
        .env_clear()
        .env("PATH", FIXTURE_TOOL_PATH)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("execute isolated installed config fixture");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn running_release_identity_refuses_mismatch_and_distinguishes_uncertainty() {
    let workflow = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    let (_, function) = workflow
        .split_once("# RELEASE-IDENTITY-VERIFY-BEGIN\n")
        .expect("release identity verifier marker");
    let (function, _) = function
        .split_once("# RELEASE-IDENTITY-VERIFY-END")
        .expect("release identity verifier end marker");
    let function = function
        .lines()
        .map(|line| line.strip_prefix("          ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    for (case, expected) in [
        ("correct", 0),
        ("wrong_installed", 2),
        ("wrong_running", 2),
        ("wrong_smoke", 2),
        ("wrong_migration", 2),
        ("wrong_health_sha", 2),
        ("wrong_base", 2),
        ("wrong_production", 2),
        ("health_malformed", 3),
        ("health_unavailable", 3),
        ("health_degraded", 3),
        ("health_status_missing", 3),
        ("health_status_unknown", 3),
        ("pid_changed", 3),
        ("pid_changed_late", 3),
        ("pid_zero", 3),
        ("inactive", 3),
        ("running_unreadable", 3),
    ] {
        let fixture = DeployFixture::new();
        let script = format!(
            r#"
set -euo pipefail
mkdir -p bin config proc/123 repo/config
git -C repo init --quiet --initial-branch=main
git -C repo config user.email fixture@example.invalid
git -C repo config user.name Fixture
printf base > repo/config/base.toml
printf production > repo/config/production.toml
git -C repo add config
git -C repo commit --quiet -m fixture
DEPLOY_SHA=$(git -C repo rev-parse HEAD)
cp repo/config/*.toml config/
printf app > bin/tickvault
printf smoke > bin/smoke_test
printf migrate > bin/tv-wal-sequence-migrate
cp bin/tickvault proc/123/exe
EXPECTED_TICKVAULT_SHA256=$(sha256sum bin/tickvault | cut -d ' ' -f 1)
EXPECTED_SMOKE_SHA256=$(sha256sum bin/smoke_test | cut -d ' ' -f 1)
EXPECTED_MIGRATION_SHA256=$(sha256sum bin/tv-wal-sequence-migrate | cut -d ' ' -f 1)
case "$CASE" in
  wrong_installed) printf wrong > bin/tickvault ;;
  wrong_running) printf wrong > proc/123/exe ;;
  wrong_smoke) printf wrong > bin/smoke_test ;;
  wrong_migration) printf wrong > bin/tv-wal-sequence-migrate ;;
  wrong_base) printf wrong > config/base.toml ;;
  wrong_production) printf wrong > config/production.toml ;;
  running_unreadable) rm proc/123/exe ;;
esac
systemctl() {{
  if [ "$1" = is-active ]; then [ "$CASE" != inactive ]; return; fi
  if [ "$CASE" = pid_zero ]; then printf '0\n'; return; fi
  local probe_count=0
  if [ -f probed ]; then read -r probe_count < probed; fi
  probe_count=$((probe_count + 1))
  printf '%s\n' "$probe_count" > probed
  if [ "$CASE" = pid_changed ] && [ "$probe_count" -ge 2 ]; then printf '456\n'; return; fi
  if [ "$CASE" = pid_changed_late ] && [ "$probe_count" -ge 3 ]; then printf '456\n'; return; fi
  printf '123\n'
}}
sha256sum() {{
  if [ "$#" -eq 0 ]; then command sha256sum; return; fi
  case "$1" in
    /opt/tickvault/*) command sha256sum "${{1#/opt/tickvault/}}" ;;
    /proc/*) command sha256sum "${{1#/}}" ;;
    *) return 94 ;;
  esac
}}
sudo() {{ test "$1" = -u; shift 2; "$@"; }}
git() {{
  if [ "$1" = -C ] && [ "$2" = /opt/tickvault/repo ]; then
    shift 2
    command git -C repo "$@"
  else return 95; fi
}}
curl() {{
  case "$CASE" in
    health_unavailable) return 7 ;;
    health_malformed) printf 'not-json'; return ;;
    wrong_health_sha) printf '{{"status":"healthy","git_sha":"0000000000000000000000000000000000000000"}}'; return ;;
    health_degraded) printf '{{"status":"degraded","git_sha":"%s"}}' "$DEPLOY_SHA"; return ;;
    health_status_missing) printf '{{"git_sha":"%s"}}' "$DEPLOY_SHA"; return ;;
    health_status_unknown) printf '{{"status":"unknown","git_sha":"%s"}}' "$DEPLOY_SHA"; return ;;
  esac
  printf '{{"status":"healthy","git_sha":"%s"}}' "$DEPLOY_SHA"
}}
{function}
if verify_release_identity; then exit 0; else exit "$?"; fi
"#
        );
        let result = Command::new("bash")
            .args(["--noprofile", "--norc", "-c", &script])
            .current_dir(&fixture.0)
            .env_clear()
            .env("PATH", FIXTURE_TOOL_PATH)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("CASE", case)
            .output()
            .expect("execute isolated running artifact identity fixture");
        assert_eq!(
            result.status.code(),
            Some(expected),
            "case={case} stdout={} stderr={}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&result.stdout).contains("RELEASE_IDENTITY_VERIFIED"),
            expected == 0
        );
    }
    let (_, promotion) = workflow
        .split_once("      - name: Record deployed binary git SHA to SSM")
        .expect("provenance promotion");
    let (promotion, _) = promotion
        .split_once("        run: |")
        .expect("promotion condition");
    assert!(promotion.contains("steps.readiness.outputs.verified == 'true'"));
    assert!(promotion.contains("steps.release_identity.outputs.verified == 'true'"));
}

#[test]
fn release_identity_runner_never_promotes_partial_or_unavailable_evidence() {
    let workflow = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    let (_, step) = workflow
        .split_once("        id: release_identity\n")
        .expect("release identity step");
    let (step, _) = step.split_once("\n      - name:").expect("next step");
    let (_, run) = step
        .split_once("        run: |\n")
        .expect("identity runner");
    let run = run
        .lines()
        .map(|line| line.strip_prefix("          ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!run.contains("${{"));
    let sha = "1234567890abcdef1234567890abcdef12345678";
    let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let success = format!("RELEASE_IDENTITY_VERIFIED commit={sha} pid=123 binary_sha256={digest}");
    let mismatch = "RELEASE_IDENTITY_MISMATCH=yes";
    let restored = format!("{mismatch}\nROLLBACK_RESTORED_STATE=active");
    let stub = r#"
aws() {
  if [ "$2" = send-command ]; then printf fixture-command; return; fi
  while [ "$#" -gt 0 ]; do
    if [ "$1" = --query ]; then
      case "$2" in
        Status) printf '%s' "$FIXTURE_STATUS"; return ;;
        StandardOutputContent) printf '%s' "$FIXTURE_OUTPUT"; return ;;
      esac
    fi
    shift
  done
  return 91
}
sleep() { :; }
"#;
    for (status, output, accepted, restored_state, uncertain) in [
        ("Success", success.as_str(), true, false, false),
        ("Success", "unrelated output", false, false, true),
        ("Failed", restored.as_str(), false, true, true),
        ("Failed", mismatch, false, false, false),
        ("InProgress", mismatch, false, false, true),
        ("InProgress", restored.as_str(), false, false, true),
        ("TimedOut", mismatch, false, false, true),
        ("Cancelled", restored.as_str(), false, false, true),
        ("Unavailable", success.as_str(), false, false, true),
    ] {
        let fixture = DeployFixture::new();
        let env_path = fixture.0.join("environment");
        let output_path = fixture.0.join("outputs");
        let result = Command::new("bash")
            .args(["--noprofile", "--norc", "-c", &format!("{stub}\n{run}")])
            .current_dir(&fixture.0)
            .env_clear()
            .env("PATH", FIXTURE_TOOL_PATH)
            .env("RUNNER_TEMP", &fixture.0)
            .env("GITHUB_ENV", &env_path)
            .env("GITHUB_OUTPUT", &output_path)
            .env("DEPLOY_SHA", sha)
            .env("DEPLOY_ROLLBACK_ID", format!("{sha}-42-1"))
            .env("DEPLOY_TICKVAULT_SHA256", digest)
            .env("DEPLOY_SMOKE_SHA256", digest)
            .env("DEPLOY_MIGRATION_SHA256", digest)
            .env("AWS_REGION_EFF", "ap-south-1")
            .env("IID", "fixture-instance")
            .env("FIXTURE_STATUS", status)
            .env("FIXTURE_OUTPUT", output)
            .output()
            .expect("execute isolated release identity transport fixture");
        assert_eq!(result.status.success(), accepted, "status={status}");
        let environment = read(&env_path);
        assert_eq!(
            environment.contains("ROLLBACK_RESTORED=yes"),
            restored_state
        );
        assert_eq!(
            environment
                .lines()
                .rev()
                .find(|line| line.starts_with("READINESS_UNCONFIRMED=")),
            Some(if uncertain {
                "READINESS_UNCONFIRMED=yes"
            } else {
                "READINESS_UNCONFIRMED=no"
            })
        );
        assert_eq!(output_path.exists(), accepted);
    }
}

#[test]
fn migration_binary_is_shipped_but_never_applied_by_deploy() {
    let workflow = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    for required in [
        "--bin tv-wal-sequence-migrate -p tickvault-storage",
        r#"cp "$BIN_DIR/tv-wal-sequence-migrate" dist/tv-wal-sequence-migrate"#,
        "aws s3 cp ./dist/tv-wal-sequence-migrate",
        "s3://tv-prod-cold/deploys/${{ github.sha }}/tv-wal-sequence-migrate",
    ] {
        assert!(
            workflow.contains(required),
            "missing binary handoff: {required}"
        );
    }
    let commands = deploy_command_array(&workflow);
    let position = |needle: &str| {
        commands
            .iter()
            .position(|line| line == needle)
            .unwrap_or_else(|| panic!("missing staged binary command: {needle}"))
    };
    let download = position(
        "timeout 300 aws s3 cp s3://tv-prod-cold/deploys/$DEPLOY_SHA/tv-wal-sequence-migrate bin/tv-wal-sequence-migrate.new",
    );
    let help = position("timeout 10 bin/tv-wal-sequence-migrate.new --help >/dev/null");
    let smoke = position("TV_ENVIRONMENT=prod timeout 40 bin/smoke_test.new --duration 30");
    let install = position("mv bin/tv-wal-sequence-migrate.new bin/tv-wal-sequence-migrate");
    assert!(download < help && help < smoke && smoke < install);
    for command in commands {
        if command.contains("tv-wal-sequence-migrate") {
            assert!(!command.contains("--apply"));
            assert!(!command.contains("--wal-dir"));
            assert!(!command.contains("--evidence"));
        }
    }
}

/// 6 — the operator portal renders the provenance footer.
/// (2026-07-18 rust-only phase 2b-3: the legacy handler.py is DELETED — the
/// pins now target the Rust port, same contract, never weakened.)
#[test]
fn portal_footer_renders_provenance_line() {
    let root = repo_root();
    let handler = read(&root.join("crates/aws-lambdas/src/operator_control.rs"));
    assert!(
        handler.contains("· portal") && handler.contains("· main"),
        "portal handler must format 'binary <7> · portal <7> · main <7>'"
    );
    assert!(
        handler.contains("fn provenance_line"),
        "portal handler must keep the pure provenance_line formatter"
    );
    assert!(
        handler.contains("^[0-9a-f]{7,40}$") && handler.contains("fn safe_provenance_sha"),
        "portal handler must hex-validate provenance SHAs (anchored \
         HEX_SHA_RE via safe_provenance_sha) before rendering — a poisoned \
         SSM param / GitHub response must never smuggle markup into the \
         footer"
    );

    let tf = read(&root.join("deploy/aws/terraform/operator-control-lambda.tf"));
    assert!(
        tf.contains("PORTAL_GIT_SHA") && tf.contains("BINARY_SHA_PARAM"),
        "operator-control terraform must inject PORTAL_GIT_SHA + BINARY_SHA_PARAM env"
    );
}

/// 7 — the watchdog publishes the mismatch metric, and terraform pins the
/// 24h staleness alarm on it.
#[test]
fn watchdog_publishes_binary_mismatch_metric() {
    let root = repo_root();
    // Repointed 2026-07-18 (rust-only phase 2b-2 wave 1): the watchdog is
    // now the Rust module (the legacy handler.py was ported 1:1 + deleted).
    let handler = read(&root.join("crates/aws-lambdas/src/deploy_watchdog.rs"));
    assert!(
        handler.contains("tv_binary_main_sha_mismatch"),
        "deploy-watchdog must publish the tv_binary_main_sha_mismatch metric"
    );
    assert!(
        handler.contains("binary_mismatch_value"),
        "deploy-watchdog must keep the pure binary_mismatch_value decision fn"
    );

    let tf = read(&root.join("deploy/aws/terraform/deploy-watchdog-lambda.tf"));
    assert!(
        tf.contains("tv_binary_main_sha_mismatch"),
        "deploy-watchdog terraform must alarm on tv_binary_main_sha_mismatch"
    );
    assert!(
        tf.contains("binary-sha-stale"),
        "the tv-<env>-binary-sha-stale alarm must exist"
    );
    assert!(
        tf.contains("notBreaching"),
        "the staleness alarm must treat missing data as notBreaching (box off / weekends never page)"
    );
}

/// 8 — terraform exposes the provenance outputs + CI wires the variable.
#[test]
fn terraform_outputs_expose_provenance() {
    let root = repo_root();
    let outputs = read(&root.join("deploy/aws/terraform/outputs.tf"));
    assert!(
        outputs.contains("portal_git_sha"),
        "outputs.tf must expose portal_git_sha"
    );
    assert!(
        outputs.contains("binary_git_sha_ssm_param"),
        "outputs.tf must expose binary_git_sha_ssm_param"
    );

    let variables = read(&root.join("deploy/aws/terraform/variables.tf"));
    assert!(
        variables.contains("portal_git_sha"),
        "variables.tf must define portal_git_sha (default \"unknown\")"
    );

    let tf_apply = read(&root.join(".github/workflows/terraform-apply.yml"));
    assert!(
        tf_apply.contains("TF_VAR_portal_git_sha"),
        "terraform-apply.yml must export TF_VAR_portal_git_sha for plan + apply"
    );
}

/// 9 — the rule file documenting the chain exists and names the chain.
#[test]
fn rule_file_exists_and_documents_chain() {
    let root = repo_root();
    let path = root.join(".claude/rules/project/deploy-provenance.md");
    assert!(
        path.exists(),
        ".claude/rules/project/deploy-provenance.md must exist"
    );
    let body = read(&path);
    for needle in [
        "TICKVAULT_GIT_SHA",
        "BUILD_GIT_SHA",
        "binary-git-sha",
        "tv_binary_main_sha_mismatch",
        "portal_git_sha",
    ] {
        assert!(
            body.contains(needle),
            "deploy-provenance.md must document '{needle}'"
        );
    }
}
