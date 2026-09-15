//! Execute the deployment admission seam against isolated files and synthetic
//! inspector/systemd/permit responses. Real authority/policy parser tests live
//! with their owners; this fixture proves ordering and refusal propagation.

use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const SHA: &str = "1234567890abcdef1234567890abcdef12345678";
#[cfg(target_os = "macos")]
const TOOL_PATH: &str = "/opt/homebrew/opt/coreutils/libexec/gnubin:/usr/local/opt/coreutils/libexec/gnubin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";
#[cfg(not(target_os = "macos"))]
const TOOL_PATH: &str = "/usr/bin:/bin";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}
fn workflow() -> String {
    std::fs::read_to_string(root().join(".github/workflows/deploy-aws.yml")).unwrap()
}
fn block(source: &str, begin: &str, end: &str) -> String {
    source
        .split_once(begin)
        .unwrap()
        .1
        .split_once(end)
        .unwrap()
        .0
        .lines()
        .map(|line| line.strip_prefix("          ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}
fn write(path: &Path, body: &str, mode: u32) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}
fn digest(path: &Path) -> String {
    let output = Command::new("sha256sum")
        .arg(path)
        .env("PATH", TOOL_PATH)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned()
}
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tv-wal-deploy-gate-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn deployment_admission_refuses_unready_namespace_or_unapproved_bytes_without_service_changes() {
    let source = workflow();
    let original = block(
        &source,
        "# WAL-DEPLOY-VERIFY-BEGIN\n",
        "# WAL-DEPLOY-VERIFY-END",
    );
    for (case, success, admission_called) in [
        ("valid", true, true),
        ("permit_refused", false, true),
        ("migration_required", false, false),
        ("unverified", false, false),
        ("corrupt_authority", false, false),
        ("fresh_empty", false, false),
        ("false_authority", false, false),
        ("wrong_namespace", false, false),
        ("missing_bound", false, false),
        ("changed_guard", false, false),
        ("missing_guard", false, false),
        ("writable_guard", false, false),
        ("changed_app", false, false),
        ("unloaded_guard", false, false),
        ("unloaded_condition", false, false),
        ("wrong_runtime_user", false, false),
        ("runtime_unreadable", false, false),
        ("active_namespace_unknown", false, false),
    ] {
        let fixture = Fixture::new();
        let stage = fixture.0.join(format!("{SHA}-42-1"));
        let app = fixture.0.join("app");
        let wal = app.join("data/ws_wal");
        let mocks = fixture.0.join("mocks");
        let guard = fixture.0.join("tickvault-start-guard");
        let dropin = fixture.0.join("guard.conf");
        for dir in [&stage, &app.join("bin"), &wal, &mocks] {
            std::fs::create_dir_all(dir).unwrap();
        }
        write(&app.join("bin/tickvault"), "previous-application", 0o755);
        write(&wal.join("retained.wal"), "retained-history", 0o600);
        write(&stage.join("tickvault"), "candidate-application", 0o755);
        write(&stage.join("smoke_test"), "candidate-smoke", 0o755);
        write(
            &stage.join("tv-wal-sequence-migrate"),
            r#"#!/bin/bash
printf '%s\n' "$TV_REPORT"
exit "$TV_INSPECT_EXIT"
"#,
            0o755,
        );
        write(
            &guard,
            r#"#!/bin/bash
set -eu
if [ "$#" = 1 ] && [ "$1" = check ]; then
  [ "$TV_CASE" != runtime_unreadable ]
  exit
fi
[ "$#" = 9 ] && [ "$1" = admit ]
[ "$2" = --binary ] && [ "$3" = "$TV_STAGE/tickvault" ]
[ "$4" = --sha256 ] && [ "$5" = "$TV_APP_SHA" ]
[ "$6" = --source-sha ] && [ "$7" = "$TV_SOURCE_SHA" ]
[ "$8" = --wal-dir ] && [ "$9" = "$TV_WAL_DIR" ]
printf called > "$TV_FIXTURE/admit.called"
[ "$TV_CASE" != permit_refused ]
"#,
            0o755,
        );
        write(&dropin, "persistent-independent-guard", 0o644);
        let app_sha = digest(&stage.join("tickvault"));
        let smoke_sha = digest(&stage.join("smoke_test"));
        let migration_sha = digest(&stage.join("tv-wal-sequence-migrate"));
        write(
            &stage.join("artifacts.sha256"),
            &format!(
                "{app_sha}  tickvault\n{smoke_sha}  smoke_test\n{migration_sha}  tv-wal-sequence-migrate\n"
            ),
            0o600,
        );
        write(
            &stage.join("start-guard.sha256"),
            &format!(
                "{}  {}\n{}  {}\n",
                digest(&guard),
                guard.display(),
                digest(&dropin),
                dropin.display()
            ),
            0o600,
        );
        let script = original
            .replace(
                "/usr/local/libexec/tickvault-start-guard",
                guard.to_str().unwrap(),
            )
            .replace(
                "/etc/systemd/system/tickvault.service.d/90-sequence-authority-guard.conf",
                dropin.to_str().unwrap(),
            )
            .replace("/opt/tickvault", app.to_str().unwrap());
        write(&stage.join("verify.sh"), &script, 0o700);
        write(
            &mocks.join("systemctl"),
            r#"#!/bin/bash
printf '%s\n' "$*" >> "$TV_FIXTURE/service.calls"
[ "$1" = show ] || { printf forbidden > "$TV_FIXTURE/service.mutated"; exit 91; }
case "$3" in
  ExecStart)
    if [ "$TV_CASE" = unloaded_guard ]; then printf 'path=/old ;'; else printf '{ path=%s ; argv[]=%s run ; }\n' "$TV_GUARD" "$TV_GUARD"; fi ;;
  ExecCondition)
    if [ "$TV_CASE" = unloaded_condition ]; then printf ''; else printf '{ path=%s ; argv[]=%s check ; }\n' "$TV_GUARD" "$TV_GUARD"; fi ;;
  User) if [ "$TV_CASE" = wrong_runtime_user ]; then echo root; else echo ec2-user; fi ;;
  MainPID) if [ "$TV_CASE" = active_namespace_unknown ]; then echo 99999999; else echo 0; fi ;;
  *) exit 92 ;;
esac
"#,
            0o755,
        );
        write(
            &mocks.join("sudo"),
            r#"#!/bin/bash
set -eu
[ "$#" = 5 ] && [ "$1" = -u ] && [ "$2" = ec2-user ] && [ "$3" = -- ]
[ "$4" = "$TV_GUARD" ] && [ "$5" = check ]
exec "$4" "$5"
"#,
            0o755,
        );
        // Model trusted root ownership without needing sudo on a Mac. Real
        // path alias/link/type/permission checks still execute against disk;
        // this seam test does not claim to validate policy-owner enforcement.
        write(
            &mocks.join("stat"),
            r#"#!/bin/bash
if [ "$1" = -c ] && [ "$2" = %u ]; then echo 0; else exec "$TV_REAL_STAT" "$@"; fi
"#,
            0o755,
        );
        let stat = Command::new("/bin/bash")
            .args(["-c", "command -v stat"])
            .env("PATH", TOOL_PATH)
            .output()
            .unwrap();
        assert!(stat.status.success());
        let real_stat = String::from_utf8(stat.stdout).unwrap().trim().to_owned();
        let mut report = json!({"mode":"inspect","wal_directory":wal,"namespace_state":"valid_durable_authority","authority_structurally_valid":true,"reserved_through":"1000000","migration_applied":false,"sequence_authority_written":false,"deployment_authorized":false});
        let mut inspect_exit = "0";
        match case {
            "migration_required" | "unverified" | "corrupt_authority" => {
                report["namespace_state"] = json!(case);
                inspect_exit = "2";
            }
            "fresh_empty" => report["namespace_state"] = json!("fresh_empty"),
            "false_authority" => report["authority_structurally_valid"] = json!(false),
            "wrong_namespace" => report["wal_directory"] = json!("/another/namespace"),
            "missing_bound" => report["reserved_through"] = serde_json::Value::Null,
            "changed_guard" => write(&guard, "changed-guard", 0o755),
            "missing_guard" => std::fs::remove_file(&guard).unwrap(),
            "writable_guard" => {
                std::fs::set_permissions(&guard, std::fs::Permissions::from_mode(0o777)).unwrap()
            }
            "changed_app" => write(&stage.join("tickvault"), "changed-app", 0o755),
            _ => {}
        }
        let out = Command::new("/bin/bash")
            .arg(stage.join("verify.sh"))
            .args([SHA, &app_sha, &migration_sha, &smoke_sha])
            .env_clear()
            .env("PATH", format!("{}:{TOOL_PATH}", mocks.display()))
            .env("TV_REAL_STAT", real_stat)
            .env("TV_FIXTURE", &fixture.0)
            .env("TV_CASE", case)
            .env("TV_STAGE", &stage)
            .env("TV_GUARD", &guard)
            .env("TV_WAL_DIR", &wal)
            .env("TV_APP_SHA", &app_sha)
            .env("TV_SOURCE_SHA", SHA)
            .env("TV_REPORT", report.to_string())
            .env("TV_INSPECT_EXIT", inspect_exit)
            .output()
            .unwrap();
        assert_eq!(out.status.success(), success, "{case}: {out:?}");
        assert_eq!(
            fixture.0.join("admit.called").exists(),
            admission_called,
            "{case}"
        );
        assert!(!fixture.0.join("service.mutated").exists(), "{case}");
        assert_eq!(
            std::fs::read_to_string(app.join("bin/tickvault")).unwrap(),
            "previous-application",
            "{case}"
        );
        assert_eq!(
            std::fs::read_to_string(wal.join("retained.wal")).unwrap(),
            "retained-history",
            "{case}"
        );
    }
}

fn forbidden_deployment_mutation(source: &str) -> Option<&'static str> {
    // Drop only complete comment lines. Preserve every executable line,
    // including JSON-wrapped commands and lines with trailing comments.
    for line in source
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
    {
        for forbidden in [
            "--apply",
            "approve-artifact",
            " clear-maintenance",
            " unfence",
            " rm -f /var/lib/tickvault/start-guard",
        ] {
            if line.contains(forbidden) {
                return Some(forbidden);
            }
        }
    }
    None
}

#[test]
fn deployment_mutation_guard_ignores_full_comments_but_refuses_commands() {
    assert_eq!(
        forbidden_deployment_mutation(
            "  # normal deployment never installs it, issues policy, or unfences.\n# no --apply or approve-artifact commands\n"
        ),
        None
    );
    for command in [
        "timeout 15 /root/tv-wal-sequence-migrate --apply --wal-dir /data",
        "sudo /usr/local/libexec/tickvault-start-guard-admin approve-artifact",
        "if true; then guard clear-maintenance; fi",
        "guard unfence # an inline comment must not hide the command",
        "sudo rm -f /var/lib/tickvault/start-guard/maintenance",
        r#"              "guard unfence","#,
    ] {
        assert!(
            forbidden_deployment_mutation(command).is_some(),
            "executable mutation escaped the guard: {command}"
        );
    }
}

#[test]
fn deployment_and_rollback_keep_the_permanent_guard_outside_owned_paths() {
    let source = workflow();
    let preflight = source.find("id: wal_admission").unwrap();
    let snapshot = source.find("id: rollback").unwrap();
    let install = source.find("id: ssm").unwrap();
    assert!(preflight < snapshot && snapshot < install);
    let owned = source
        .split_once("<<'TV_OWNED_PATHS'\n")
        .unwrap()
        .1
        .split_once("\n          TV_OWNED_PATHS")
        .unwrap()
        .0;
    for forbidden in [
        "start-guard",
        "tickvault.service.d",
        "ws_wal",
        "sequence.tvsq",
    ] {
        assert!(!owned.contains(forbidden));
    }
    let literal = source
        .split_once("--argjson commands '[")
        .unwrap()
        .1
        .split_once("]' '{commands:")
        .unwrap()
        .0;
    let commands: Vec<String> = serde_json::from_str(&format!("[{literal}]")).unwrap();
    let admit = commands
        .iter()
        .position(|s| s.starts_with("bash \"$STAGE/verify.sh\""))
        .unwrap();
    let mutation = commands
        .iter()
        .position(|s| s.starts_with("echo DEPLOY_INSTALL_STARTED=yes"))
        .unwrap();
    assert!(admit < mutation);
    assert!(
        !commands
            .iter()
            .any(|s| s.contains("chown -R ec2-user:ec2-user /opt/tickvault"))
    );
    assert!(commands.iter().any(|s| {
        s.contains("chown root:root bin/tickvault bin/smoke_test bin/tv-wal-sequence-migrate")
    }));
    let restore = block(
        &source,
        "# COHERENT-ROLLBACK-RESTORE-BEGIN\n",
        "# COHERENT-ROLLBACK-RESTORE-END",
    );
    assert!(
        restore.find("tickvault-start-guard check").unwrap()
            < restore.find("systemctl restart \"$unit\"").unwrap()
    );
    assert!(restore.contains("tickvault-start-guard fence --reason rollback_incompatible"));
    assert!(restore.contains("ROLLBACK_FENCED_INCOMPLETE"));
    assert_eq!(
        forbidden_deployment_mutation(&source),
        None,
        "no implicit migration or permission in executable deployment content"
    );
    assert!(
        source.contains("sha256sum \"/proc/$pid_before/exe\""),
        "verify running inode bytes even when ExecStart uses a file descriptor"
    );
}
