//! AUTO-START GUARANTEE ratchet (2026-06-09).
//!
//! Source-scan guard pinning the fix for the 2026-06-09 "will it auto-start
//! tomorrow?" incident class. The `deploy-aws.yml` deploy-failure path used to
//! run BOTH `systemctl stop tickvault` AND `systemctl disable tickvault`. The
//! `disable` was the auto-start killer:
//!
//!  1. A disabled unit does NOT start at the next 08:30 IST boot — systemd only
//!     auto-starts ENABLED units. So one failed evening deploy silently killed
//!     the next morning's auto-start.
//!  2. Worse, `scripts/aws-autopilot.sh` (the every-15-min self-healer) reads a
//!     DISABLED unit as an INTENTIONAL kill-switch and REFUSES to restart it
//!     (see its "unit disabled" branch). So the disable also defeated the one
//!     mechanism that would otherwise have self-healed the box.
//!
//! The fix keeps the `systemctl stop` (which alone breaks the systemd
//! `Restart=always` "Auth OK" Telegram spam loop for the current boot — an
//! explicit stop suppresses Restart=always until the next boot) and DROPS the
//! `systemctl disable`. The unit therefore stays ENABLED on a failed deploy, so
//! systemd auto-starts it next boot and the autopilot self-heals an
//! enabled-but-inactive unit. The ONLY remaining source of a disabled unit is
//! the intentional kill-switch (disable via AWS Control), so the autopilot's
//! `disabled == intentional` semantics stay correct.
//!
//! This guard fails the build if the `disable` ever returns to the deploy
//! failure path, OR if the spam-loop-breaking `stop` is removed, OR if the
//! autopilot's enabled-but-inactive self-heal restart is deleted.

#![cfg(test)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/storage parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Section A — the deploy-failure path MUST NOT disable the tickvault unit.
/// Restoring a captured disabled state is permitted only inside coherent
/// rollback. The generic failure stop must never change enablement.
#[test]
fn deploy_failure_path_never_disables_tickvault_unit() {
    let body = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    let step = workflow_step(&body, "Auto-stop tickvault service on deploy failure");
    let run = step
        .split_once("\n        run: |\n")
        .expect("generic failure stop must have an executable run script")
        .1;
    // The YAML description explicitly documents the forbidden command. Only
    // executable shell lines belong in this guard, not explanatory comments.
    let executable = run
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        "systemctl disable",
        "systemctl enable",
        "systemctl reenable",
        "systemctl mask",
        "systemctl unmask",
    ] {
        assert!(
            !executable.contains(forbidden),
            "generic deploy failure must not change prior enablement ({forbidden}); \
             only coherent rollback may restore a previously disabled unit"
        );
    }
}

/// Section B — the spam-loop-breaking `systemctl stop` MUST stay. Removing it
/// would re-open the systemd Restart=always "Auth OK" Telegram-per-minute loop
/// the disable+stop pair originally fixed.
#[test]
fn deploy_failure_path_still_stops_to_break_restart_spam_loop() {
    let body = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    assert!(
        body.contains("systemctl stop tickvault || true"),
        "deploy-aws.yml failure path must still `systemctl stop tickvault || true` \
         to break the Restart=always spam loop for the current boot"
    );
}

/// Section C — the auto-start guarantee now RELIES on aws-autopilot.sh
/// restarting an enabled-but-inactive unit. Pin that self-heal so a future edit
/// cannot silently delete the mechanism this fix depends on.
#[test]
fn autopilot_still_self_heals_enabled_but_inactive_unit() {
    let body = read(&repo_root().join("scripts/aws-autopilot.sh"));
    assert!(
        body.contains("is-enabled tickvault"),
        "aws-autopilot.sh must still check `systemctl is-enabled tickvault` to \
         distinguish an intentional kill-switch from an enabled-but-inactive unit"
    );
    assert!(
        body.contains("systemctl start tickvault"),
        "aws-autopilot.sh must still `systemctl start tickvault` to self-heal \
         an enabled-but-inactive unit — the auto-start guarantee depends on it. \
         (BP-09: the recovery is now `reset-failed` + `start`, not a bare \
         `restart`, so this pins `start`, the launch step, not `restart`.)"
    );
}

/// Section D (BP-09, 2026-07-01) — the self-heal restart MUST `reset-failed`
/// BEFORE it starts the unit. `deploy/systemd/tickvault.service` sets
/// StartLimitBurst=5 / StartLimitIntervalSec=3600 (widened 2026-09-03 — the
/// old 8-in-10-minutes pair could not see a cycle slower than 75s, and the
/// loop that happened ran on ~9-minute cycles), so after > 5 restarts in 60
/// min systemd flips the unit to `failed` and a bare `systemctl restart`/`start`
/// returns "Start request repeated too quickly" and is a NO-OP until the
/// start-limit counter is cleared with `systemctl reset-failed`. Without this,
/// autopilot pages but cannot recover a crash-loop — the box stays down until a
/// human runs reset-failed (BP-09). This guard fails the build if the recovery
/// ever regresses to a bare `restart`/`start` that omits `reset-failed`.
#[test]
fn autopilot_resets_failed_before_start_to_recover_crash_loop() {
    let body = read(&repo_root().join("scripts/aws-autopilot.sh"));
    assert!(
        body.contains("reset-failed tickvault"),
        "aws-autopilot.sh must `systemctl reset-failed tickvault` before starting \
         the unit, or a StartLimit-`failed` crash-loop (StartLimitBurst in \
         tickvault.service) cannot be recovered — a bare restart/start on a \
         `failed` unit is a no-op (BP-09)"
    );
    // reset-failed alone does not launch the app; the recovery MUST also start it.
    assert!(
        body.contains("systemctl start tickvault"),
        "aws-autopilot.sh must `systemctl start tickvault` after `reset-failed` — \
         reset-failed only clears the start-limit counter, it does not launch the \
         unit (BP-09)"
    );
}

fn workflow_step<'a>(body: &'a str, name: &str) -> &'a str {
    body.split_once(&format!("      - name: {name}"))
        .unwrap_or_else(|| panic!("workflow step {name} is missing"))
        .1
        .split("\n      - name: ")
        .next()
        .expect("step body")
}

struct ProbeResult {
    success: bool,
    env: String,
    output: String,
    stdout: String,
}

/// Execute the actual workflow readiness script with synthetic SSM responses.
/// No AWS executable, credentials, service, sleeps or network are used. This
/// verifies shell control flow; it does not establish production readiness.
fn readiness_probe(state: &str, rollback_status: &str, rollback_output: &str) -> ProbeResult {
    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "tickvault-deploy-readiness-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir).expect("create isolated readiness fixture directory");
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());
    let env_path = dir.join("github-env");
    let output_path = dir.join("github-output");
    std::fs::write(&env_path, "").expect("initialize fixture environment file");
    std::fs::write(&output_path, "").expect("initialize fixture output file");

    let body = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    let step = workflow_step(&body, "Verify app readiness");
    let (_, run) = step
        .split_once("\n        run: |\n")
        .expect("readiness run script");
    let run = run
        .lines()
        .map(|line| line.strip_prefix("          ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
        .replace("${{ vars.AWS_REGION || 'ap-south-1' }}", "ap-south-1")
        .replace("${{ github.sha }}", "fixture-commit");
    assert!(
        !run.contains("${{"),
        "unhandled workflow expression in fixture"
    );
    let stub = r#"
sleep() { :; }
aws() {
  case "$2" in
    send-command)
      case "$*" in
        *"printf STATE="*) printf probe ;;
        *"external-stop check"*) printf external ;;
        *"crash diagnostics"*) printf diagnostic ;;
        *"/var/lib/tickvault/deploy-rollback/"*) printf rollback ;;
        *) printf 'unexpected synthetic AWS request\n' >&2; return 97 ;;
      esac ;;
    get-command-invocation)
      case "$*" in
        *"--command-id probe"*) printf 'STATE=%s\nNRESTARTS=0\n' "$TV_TEST_STATE" ;;
        *"--command-id external"*) printf 'ActiveState=failed\nResult=exit-code\n' ;;
        *"--command-id diagnostic"*) printf 'synthetic readiness failure\n' ;;
        *"--command-id rollback"*)
          case "$*" in
            *"--query Status"*) printf '%s\n' "$TV_TEST_ROLLBACK_STATUS" ;;
            *) printf '%s\n' "$TV_TEST_ROLLBACK_OUTPUT" ;;
          esac ;;
        *) printf 'unexpected synthetic AWS response\n' >&2; return 97 ;;
      esac ;;
    *) printf 'unexpected synthetic AWS operation\n' >&2; return 97 ;;
  esac
}
"#;
    let result = Command::new("bash")
        .args(["--noprofile", "--norc", "-c", &format!("{stub}\n{run}")])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("IID", "fixture-instance")
        .env(
            "DEPLOY_ROLLBACK_ID",
            "1111111111111111111111111111111111111111-42-1",
        )
        .env("GITHUB_ENV", &env_path)
        .env("GITHUB_OUTPUT", &output_path)
        .env("READINESS_TIMEOUT_SECS", "0")
        .env("READINESS_POLL_SECS", "0")
        .env("TV_TEST_STATE", state)
        .env("TV_TEST_ROLLBACK_STATUS", rollback_status)
        .env("TV_TEST_ROLLBACK_OUTPUT", rollback_output)
        .output()
        .expect("execute isolated workflow readiness fixture");
    assert!(
        result.stderr.is_empty(),
        "unexpected fixture stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    ProbeResult {
        success: result.status.success(),
        env: read(&env_path),
        output: read(&output_path),
        stdout: String::from_utf8(result.stdout).expect("workflow emits UTF-8"),
    }
}

#[test]
fn readiness_timeout_never_reports_success_for_an_unconfirmed_service() {
    for state in [
        "activating",
        "inactive",
        "reloading",
        "unknown",
        "probefailed",
        "",
    ] {
        let result = readiness_probe(state, "Success", "ROLLBACK_RESTORED_STATE=active");
        assert!(!result.success, "unconfirmed state {state:?} passed");
        assert!(result.env.contains("READINESS_UNCONFIRMED=yes"));
        assert!(!result.env.contains("ROLLBACK_RESTORED=yes"));
        assert!(result.output.is_empty());
        assert!(!result.stdout.contains("DEPLOY VERIFIED:"));
    }
    let ready = readiness_probe("active", "Success", "ROLLBACK_RESTORED_STATE=active");
    assert!(ready.success);
    assert!(ready.env.is_empty());
    assert_eq!(ready.output, "verified=true\n");
}

#[test]
fn a_confirmed_rollback_remains_a_failed_deploy_with_a_restored_service() {
    let restored = readiness_probe("failed", "Success", "ROLLBACK_RESTORED_STATE=active");
    assert!(!restored.success);
    assert!(restored.env.contains("ROLLBACK_RESTORED=yes"));
    assert!(restored.output.is_empty());
    let inactive = readiness_probe("failed", "Success", "ROLLBACK_RESTORED_STATE=inactive");
    assert!(!inactive.success);
    assert!(inactive.env.contains("ROLLBACK_RESTORED=yes"));
    for (status, marker) in [
        ("Failed", "ROLLBACK_RESTORED_STATE=active"),
        ("TimedOut", "ROLLBACK_RESTORED_STATE=active"),
        ("Success", "ROLLBACK_NOT_ACTIVE"),
        ("Success", "prefix_ROLLBACK_RESTORED_STATE=active"),
    ] {
        let rejected = readiness_probe("failed", status, marker);
        assert!(!rejected.success);
        assert!(!rejected.env.contains("ROLLBACK_RESTORED=yes"));
        assert!(rejected.output.is_empty());
    }
}

/// The script fixtures above execute shell branches. This additional source
/// guard pins the GitHub Actions wiring between steps; it is not an Actions
/// interpreter or an AWS deployment test.
#[test]
fn deploy_failure_wiring_preserves_recovery_and_gates_provenance() {
    let body = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    let stop = workflow_step(&body, "Auto-stop tickvault service on deploy failure");
    for required in [
        "failure() && env.DEPLOY_SKIP_REASON == ''",
        "env.EXTERNAL_STOP_DETECTED != 'yes'",
        "env.ROLLBACK_RESTORED != 'yes'",
        "env.READINESS_UNCONFIRMED != 'yes'",
        "env.DEPLOY_INSTALL_STARTED == 'yes'",
        "steps.ssm.outputs.command_id != ''",
    ] {
        assert!(stop.contains(required), "auto-stop must retain {required}");
    }
    let provenance = workflow_step(&body, "Record deployed binary git SHA to SSM");
    assert!(provenance.contains("success() && env.DEPLOY_SKIP_REASON == ''"));
    assert!(provenance.contains("steps.readiness.outputs.verified == 'true'"));
    let readiness = workflow_step(&body, "Verify app readiness");
    assert!(readiness.contains("id: readiness"));
}

/// Execute the snapshot/restore scripts extracted from the workflow against
/// isolated files and a synthetic systemctl. This covers filesystem/lifecycle
/// rollback, not WAL compatibility, AWS transport, or an actual deployment.
#[cfg(target_os = "linux")]
fn coherent_rollback_case(case: &str) {
    use std::os::unix::fs::PermissionsExt;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "tickvault-coherent-rollback-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir).expect("create isolated rollback fixture");
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());
    let app = dir.join("root/opt/tickvault");
    let units = dir.join("root/etc/systemd/system");
    let mocks = dir.join("mocks");
    let states = dir.join("states");
    let snapshots = dir.join("snapshots");
    std::fs::create_dir(&mocks).expect("create fixture command directory");
    let body = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    let step = workflow_step(&body, "Prepare private coherent rollback snapshot");
    let run = step
        .split_once("        run: |\n")
        .expect("snapshot runner")
        .1
        .lines()
        .map(|line| line.strip_prefix("          ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!run.contains("${{"));
    assert!(run.len() < 21_000);
    let prepare = run
        .split_once("<<'TV_PREPARE_ROLLBACK'\n")
        .expect("embedded snapshot script")
        .1
        .split_once("\nTV_PREPARE_ROLLBACK\n")
        .expect("snapshot script end")
        .0
        .replace(
            "/var/lib/tickvault/deploy-rollback",
            snapshots.to_str().expect("fixture path UTF-8"),
        )
        .replace(
            "/usr/local/libexec/tickvault-start-guard",
            mocks
                .join("tickvault-start-guard")
                .to_str()
                .expect("fixture guard path UTF-8"),
        )
        .replace(
            "opt/tickvault",
            app.to_str()
                .expect("fixture path UTF-8")
                .trim_start_matches('/'),
        )
        .replace(
            "etc/systemd/system",
            units
                .to_str()
                .expect("fixture path UTF-8")
                .trim_start_matches('/'),
        );
    let prepare_path = dir.join("prepare.sh");
    std::fs::write(&prepare_path, prepare).expect("write isolated snapshot script");
    let literal = body
        .split_once("--argjson commands '[")
        .expect("main SSM command array")
        .1
        .split_once("]' '{commands:")
        .expect("main SSM array end")
        .0;
    let commands: Vec<String> = serde_json::from_str(&format!("[{literal}]"))
        .expect("main SSM commands are valid JSON strings");
    let trap = commands
        .iter()
        .position(|line| line == "trap rollback_on_exit EXIT")
        .expect("initial install failure trap");
    let transaction = commands[..=trap].join("\n").replace(
        "/var/lib/tickvault/deploy-rollback",
        snapshots.to_str().expect("fixture path UTF-8"),
    );
    let transaction_path = dir.join("transaction-prefix.sh");
    std::fs::write(&transaction_path, transaction).expect("write exact install trap");
    let arm = commands
        .iter()
        .find(|line| line.starts_with("echo DEPLOY_INSTALL_STARTED=yes"))
        .expect("installation mutation boundary");
    let unarm = commands
        .iter()
        .find(|line| line.as_str() == "TV_ROLLBACK_ARMED=0")
        .expect("uncertain activation boundary");
    let systemctl = r###"#!/bin/bash
set -eu
printf '%s\n' "$*" >> "$FIXTURE_CALLS"
unit=${!#}; unit=${unit%.service}
file=$FIXTURE_UNITS/$unit.service
state=$FIXTURE_STATES/$unit.state
mode=$FIXTURE_STATES/$unit.mode
case "$1" in
show)
 case "$3" in
 LoadState) if [ -e "$file" ] || [ -L "$file" ]; then echo loaded; else echo not-found; fi ;;
 ActiveState) if [ -f "$state" ]; then cat "$state"; else echo inactive; fi ;;
 *) exit 91 ;;
 esac ;;
is-enabled)
 if [ ! -e "$file" ]; then echo not-found; exit 4; fi
 if [ -f "$mode" ]; then cat "$mode"; test "$(cat "$mode")" = enabled; else echo disabled; exit 1; fi ;;
stop)
 if [ "${FAIL_POINT:-}" = stop ]; then echo fixture-stop-failure >&2; exit 82; fi
 echo inactive > "$state" ;;
disable) echo disabled > "$mode" ;;
enable) echo enabled > "$mode" ;;
restart|start)
 if [ "${FAIL_POINT:-}" = start ]; then echo fixture-start-failure >&2; exit 83; fi
 echo active > "$state" ;;
is-active) test "$(cat "$state")" = active ;;
daemon-reload)
 if [ "${FAIL_POINT:-}" = reload ]; then echo fixture-reload-failure >&2; exit 84; fi ;;
reset-failed) : ;;
*) echo unexpected-systemctl >&2; exit 92 ;;
esac
"###;
    let tar = r###"#!/bin/bash
if [ "${FAIL_POINT:-}" = extract ] && [ "$1" = --extract ]; then echo fixture-extract-failure >&2; exit 73; fi
exec /usr/bin/tar "$@"
"###;
    let start_guard = r###"#!/bin/bash
set -eu
printf 'guard %s\n' "$*" >> "$FIXTURE_CALLS"
case "$1" in
  check)
    test "$(cat "$FIXTURE_APP/bin/tickvault")" = old-binary
    test "$(cat "$FIXTURE_STATES/tickvault.state")" = inactive
    [ "$CASE" != incompatible ] ;;
  fence)
    [ "$2" = --reason ] && [ "$3" = rollback_incompatible ]
    printf fenced > "$FIXTURE_DIR/maintenance"
    echo TV_START_GUARD_FENCED ;;
  *) exit 91 ;;
esac
"###;
    for (name, script) in [
        ("systemctl", systemctl),
        ("tar", tar),
        ("sleep", "#!/bin/sh\nexit 0\n"),
        ("tickvault-start-guard", start_guard),
    ] {
        let path = mocks.join(name);
        std::fs::write(&path, script).expect("write mock command");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make fixture command executable");
    }
    let harness = r###"set -euo pipefail
mkdir -p "$FIXTURE_APP/bin" "$FIXTURE_APP/config" "$FIXTURE_UNITS" "$FIXTURE_STATES"
mkdir -p "$FIXTURE_APP/data" "$FIXTURE_APP/logs" "$FIXTURE_APP/repo"
chmod 0755 "$FIXTURE_APP" "$FIXTURE_APP/bin"
chmod 0770 "$FIXTURE_APP/config" "$FIXTURE_APP/data" "$FIXTURE_APP/logs" "$FIXTURE_APP/repo"
stat -c '%u:%g:%a' "$FIXTURE_APP" "$FIXTURE_APP/bin" > "$FIXTURE_DIR/binary-parent-metadata"
stat -c '%u:%g:%a' "$FIXTURE_APP/config" > "$FIXTURE_DIR/config-metadata"
stat -c '%u:%g:%a' "$FIXTURE_APP/data" "$FIXTURE_APP/logs" "$FIXTURE_APP/repo" > "$FIXTURE_DIR/runtime-child-metadata"
for child in data logs repo; do printf retained-child > "$FIXTURE_APP/$child/retained.keep"; done
mkdir -p "$FIXTURE_UNITS/tickvault.service.d"
printf permanent-start-guard > "$FIXTURE_UNITS/tickvault.service.d/90-sequence-authority-guard.conf"
: > "$FIXTURE_CALLS"
printf unrelated > "$FIXTURE_APP/unrelated.keep"
prior=active
case "$CASE" in inactive|absent) prior=inactive ;; esac
if [ "$CASE" != absent ]; then
  printf old-binary > "$FIXTURE_APP/bin/tickvault"
  chmod 0751 "$FIXTURE_APP/bin/tickvault"
  printf old-private-config > "$FIXTURE_APP/config/production.toml"
  chmod 0640 "$FIXTURE_APP/config/production.toml"
  ln -s production.toml "$FIXTURE_APP/config/link.toml"
  printf old-app-unit > "$FIXTURE_UNITS/tickvault.service"
  printf old-host-unit > "$FIXTURE_UNITS/tickvault-host-tuning.service"
  printf '%s\n' "$prior" > "$FIXTURE_STATES/tickvault.state"
  if [ "$prior" = inactive ]; then printf disabled; else printf enabled; fi > "$FIXTURE_STATES/tickvault.mode"
  printf inactive > "$FIXTURE_STATES/tickvault-host-tuning.state"
  printf disabled > "$FIXTURE_STATES/tickvault-host-tuning.mode"
fi
if [ "$CASE" = external_symlink ]; then
  rm "$FIXTURE_APP/config/link.toml"
  ln -s "$FIXTURE_APP/unrelated.keep" "$FIXTURE_APP/config/link.toml"
  if bash "$PREPARE" > "$FIXTURE_DIR/prepare.output" 2>&1; then exit 104; fi
  test ! -f "$SNAPSHOT/ready"
  test "$(cat "$FIXTURE_APP/unrelated.keep")" = unrelated
  exit 0
fi
if [ "$CASE" = nonquiescent ]; then
  printf activating > "$FIXTURE_STATES/tickvault.state"
  if bash "$PREPARE" > "$FIXTURE_DIR/prepare.output" 2>&1; then exit 94; fi
  test ! -f "$SNAPSHOT/ready"
  test "$(cat "$FIXTURE_APP/bin/tickvault")" = old-binary
  exit 0
fi
bash "$PREPARE" > "$FIXTURE_DIR/prepare.output"
test "$(stat -c %a "$SNAPSHOT")" = 700
if tar --list --file "$SNAPSHOT/previous.tar" | grep -Eqx "${FIXTURE_APP#/}/?|${FIXTURE_APP#/}/bin/?"; then exit 109; fi
bash "$SNAPSHOT/restore.sh" --check-current > "$FIXTURE_DIR/baseline.output"
if [ "$CASE" = reuse ]; then
  digest=$(sha256sum "$SNAPSHOT/previous.tar")
  if bash "$PREPARE" > "$FIXTURE_DIR/reuse.output" 2>&1; then exit 95; fi
  test "$(sha256sum "$SNAPSHOT/previous.tar")" = "$digest"
  exit 0
fi
if [ "$CASE" = lifecycle_drift ]; then
  printf inactive > "$FIXTURE_STATES/tickvault.state"
  : > "$FIXTURE_CALLS"
  if bash "$SNAPSHOT/restore.sh" --check-current > "$FIXTURE_DIR/drift.output" 2>&1; then exit 96; fi
  test "$(cat "$FIXTURE_APP/bin/tickvault")" = old-binary
  if grep -qx 'stop tickvault' "$FIXTURE_CALLS"; then exit 97; fi
  exit 0
fi
printf new-binary > "$FIXTURE_APP/bin/tickvault"
printf new-smoke > "$FIXTURE_APP/bin/smoke_test"
printf new-migration > "$FIXTURE_APP/bin/tv-wal-sequence-migrate"
printf new-private-config > "$FIXTURE_APP/config/production.toml"
printf new-only > "$FIXTURE_APP/config/new-only.toml"
printf new-app-unit > "$FIXTURE_UNITS/tickvault.service"
printf new-host-unit > "$FIXTURE_UNITS/tickvault-host-tuning.service"
printf new-holiday-unit > "$FIXTURE_UNITS/tickvault-holiday-gate.service"
for unit in tickvault tickvault-host-tuning; do
  printf active > "$FIXTURE_STATES/$unit.state"
  printf enabled > "$FIXTURE_STATES/$unit.mode"
done
: > "$FIXTURE_CALLS"
# A stale snapshot must fail before installation if owned files changed.
if bash "$SNAPSHOT/restore.sh" --check-current > "$FIXTURE_DIR/drift.output" 2>&1; then exit 98; fi
test "$(cat "$FIXTURE_APP/bin/tickvault")" = new-binary
if grep -qx 'stop tickvault' "$FIXTURE_CALLS"; then exit 99; fi
case "$CASE" in
  corrupt) printf bad >> "$SNAPSHOT/previous.tar" ;;
  missing_ready) rm "$SNAPSHOT/ready" ;;
  extract|reload|stop|start) export FAIL_POINT=$CASE ;;
esac
restore_success_exit=0
case "$CASE" in
  install_failure|probe_unknown|pre_install_failure)
    cat "$TRANSACTION_PREFIX" > "$FIXTURE_DIR/transaction.sh"
    if [ "$CASE" != pre_install_failure ]; then printf '\n%s\n' "$TRANSACTION_ARM" >> "$FIXTURE_DIR/transaction.sh"; fi
    if [ "$CASE" = probe_unknown ]; then printf '%s\n' "$TRANSACTION_UNARM" >> "$FIXTURE_DIR/transaction.sh"; fi
    printf '\nexit 71\n' >> "$FIXTURE_DIR/transaction.sh"
    set +e
    bash "$FIXTURE_DIR/transaction.sh" > "$FIXTURE_DIR/restore.output" 2> "$FIXTURE_DIR/restore.stderr"
    rc=$?
    set -e
    test "$rc" -eq 71
    if [ "$CASE" != install_failure ]; then
      test "$(cat "$FIXTURE_APP/bin/tickvault")" = new-binary
      if grep -q ROLLBACK_RESTORED_STATE= "$FIXTURE_DIR/restore.output"; then exit 105; fi
      if grep -qx 'stop tickvault' "$FIXTURE_CALLS"; then exit 106; fi
      if [ "$CASE" = probe_unknown ]; then grep -qx READINESS_UNCONFIRMED=yes "$FIXTURE_DIR/restore.output"; fi
      exit 0
    fi
    restore_success_exit=71
    ;;
  *)
    set +e
    bash "$SNAPSHOT/restore.sh" > "$FIXTURE_DIR/restore.output" 2> "$FIXTURE_DIR/restore.stderr"
    rc=$?
    set -e
    ;;
esac
case "$CASE" in
  incompatible)
    test "$rc" -ne 0
    test "$(cat "$FIXTURE_APP/bin/tickvault")" = old-binary
    test "$(cat "$FIXTURE_STATES/tickvault.state")" = inactive
    test "$(cat "$FIXTURE_DIR/maintenance")" = fenced
    grep -q '^ROLLBACK_FENCED_INCOMPLETE ' "$SNAPSHOT/restore.status"
    if grep -Eqx '(start|restart) tickvault' "$FIXTURE_CALLS"; then exit 107; fi
    if grep -q ROLLBACK_RESTORED_STATE= "$FIXTURE_DIR/restore.output"; then exit 108; fi
    ;;
  corrupt|extract|reload|stop|start|missing_ready)
    test "$rc" -ne 0
    if grep -q ROLLBACK_RESTORED_STATE= "$FIXTURE_DIR/restore.output"; then exit 100; fi
    test -f "$SNAPSHOT/previous.tar"
    grep -q '^ROLLBACK_FAILED' "$SNAPSHOT/restore.status"
    case "$CASE" in corrupt|missing_ready|stop) test "$(cat "$FIXTURE_APP/bin/tickvault")" = new-binary ;; esac
    case "$CASE" in corrupt|missing_ready) if grep -qx 'stop tickvault' "$FIXTURE_CALLS"; then exit 101; fi ;; esac
    ;;
  *)
    test "$rc" -eq "$restore_success_exit"
    grep -qx "ROLLBACK_RESTORED_STATE=$prior" "$FIXTURE_DIR/restore.output"
    test ! -e "$FIXTURE_APP/bin/smoke_test"
    test ! -e "$FIXTURE_APP/bin/tv-wal-sequence-migrate"
    test ! -e "$FIXTURE_APP/config/new-only.toml"
    test ! -e "$FIXTURE_UNITS/tickvault-holiday-gate.service"
    if [ "$CASE" != absent ]; then
      test "$(cat "$FIXTURE_APP/bin/tickvault")" = old-binary
      test "$(stat -c %a "$FIXTURE_APP/bin/tickvault")" = 751
      test "$(cat "$FIXTURE_APP/config/production.toml")" = old-private-config
      test "$(stat -c %a "$FIXTURE_APP/config/production.toml")" = 640
      test -L "$FIXTURE_APP/config/link.toml"
      test "$(readlink "$FIXTURE_APP/config/link.toml")" = production.toml
      test "$(cat "$FIXTURE_UNITS/tickvault.service")" = old-app-unit
      test "$(cat "$FIXTURE_UNITS/tickvault-host-tuning.service")" = old-host-unit
      test "$(cat "$FIXTURE_STATES/tickvault.state")" = "$prior"
      if [ "$prior" = active ]; then expected=enabled; else expected=disabled; fi
      test "$(cat "$FIXTURE_STATES/tickvault.mode")" = "$expected"
    else
      test ! -e "$FIXTURE_APP/bin/tickvault"
      test ! -e "$FIXTURE_UNITS/tickvault.service"
    fi
    if [ "$prior" = inactive ]; then
      if grep -Eqx '(start|restart) tickvault' "$FIXTURE_CALLS"; then exit 102; fi
    fi
    if [ "$CASE" = repeat ]; then
      bash "$SNAPSHOT/restore.sh" > "$FIXTURE_DIR/repeat.output"
      grep -qx "ROLLBACK_RESTORED_STATE=$prior" "$FIXTURE_DIR/repeat.output"
    fi
    ;;
esac
test "$(cat "$FIXTURE_APP/unrelated.keep")" = unrelated
stat -c '%u:%g:%a' "$FIXTURE_APP" "$FIXTURE_APP/bin" | cmp -s - "$FIXTURE_DIR/binary-parent-metadata"
if [ "$CASE" != extract ]; then stat -c '%u:%g:%a' "$FIXTURE_APP/config" | cmp -s - "$FIXTURE_DIR/config-metadata"; fi
stat -c '%u:%g:%a' "$FIXTURE_APP/data" "$FIXTURE_APP/logs" "$FIXTURE_APP/repo" | cmp -s - "$FIXTURE_DIR/runtime-child-metadata"
for child in data logs repo; do test "$(cat "$FIXTURE_APP/$child/retained.keep")" = retained-child; done
test "$(cat "$FIXTURE_UNITS/tickvault.service.d/90-sequence-authority-guard.conf")" = permanent-start-guard
if grep -q private-config "$FIXTURE_DIR/restore.output" "$FIXTURE_DIR/restore.stderr"; then exit 103; fi
"###;
    let rollback_id = "1111111111111111111111111111111111111111-42-1";
    let result = Command::new("bash")
        .args(["--noprofile", "--norc", "-c", harness])
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", mocks.display()))
        .env("CASE", case)
        .env("FIXTURE_DIR", &dir)
        .env("FIXTURE_APP", &app)
        .env("FIXTURE_UNITS", &units)
        .env("FIXTURE_STATES", &states)
        .env("FIXTURE_CALLS", dir.join("calls"))
        .env("PREPARE", &prepare_path)
        .env("TRANSACTION_PREFIX", &transaction_path)
        .env("TRANSACTION_ARM", arm)
        .env("TRANSACTION_UNARM", unarm)
        .env("SNAPSHOT", snapshots.join(rollback_id))
        .env("ROLLBACK_ID", rollback_id)
        .output()
        .expect("execute isolated coherent rollback fixture");
    assert!(
        result.status.success(),
        "coherent rollback case {case} failed: status={:?}, stdout={}, stderr={}",
        result.status.code(),
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
#[cfg(target_os = "linux")]
fn coherent_rollback_restores_owned_files_and_prior_lifecycle() {
    for case in [
        "active",
        "inactive",
        "absent",
        "repeat",
        "reuse",
        "install_failure",
    ] {
        coherent_rollback_case(case);
    }
}

#[test]
#[cfg(target_os = "linux")]
fn coherent_rollback_refuses_stale_missing_or_corrupt_evidence() {
    for case in [
        "corrupt",
        "missing_ready",
        "nonquiescent",
        "lifecycle_drift",
        "external_symlink",
        "probe_unknown",
        "pre_install_failure",
    ] {
        coherent_rollback_case(case);
    }
}

#[test]
#[cfg(target_os = "linux")]
fn coherent_rollback_preserves_failure_evidence_without_claiming_success() {
    for case in ["extract", "reload", "stop", "start", "incompatible"] {
        coherent_rollback_case(case);
    }
}

#[test]
fn coherent_rollback_is_bound_to_each_install_and_readiness_failure() {
    let body = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    let prepare = body.find("id: rollback").expect("private snapshot step");
    let install = body.find("id: ssm").expect("install step");
    assert!(prepare < install);
    let install_step = workflow_step(&body, "Run SSM command");
    let run = install_step.split_once("        run: |\n").unwrap().1;
    assert!(!run.contains("${{"));
    assert!(run.len() < 21_000);
    assert!(run.contains("DEPLOY_ROLLBACK_ID"));
    let check = run
        .find("--check-current")
        .expect("baseline must still match");
    let mutation = run
        .find("echo DEPLOY_INSTALL_STARTED=yes")
        .expect("explicit mutation boundary");
    let copy = run
        .find("cp -f repo/config/*.toml config/")
        .expect("config install");
    assert!(check < mutation && mutation < copy);
    assert!(run.contains("trap rollback_on_exit EXIT"));
    assert!(run.contains("READINESS_UNCONFIRMED=yes"));
    let readiness = workflow_step(&body, "Verify app readiness");
    assert!(readiness.contains("/var/lib/tickvault/deploy-rollback/"));
    assert!(readiness.contains("ROLLBACK_RESTORED_STATE=(active|inactive)"));
    assert!(!readiness.contains("cp -f bin/tickvault.backup bin/tickvault"));
    let fetch = workflow_step(&body, "Fetch SSM command output");
    assert!(fetch.contains("ROLLBACK_RESTORED=yes"));
    assert!(fetch.contains("DEPLOY_INSTALL_STARTED=yes"));
}
