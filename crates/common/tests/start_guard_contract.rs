//! Isolated shell/filesystem admission proof, plus installed-unit wiring.
//! This does not invoke AWS, systemd or a real application process.

use std::path::PathBuf;
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn persistent_guard_refuses_unapproved_unsafe_or_fenced_filesystem_states() {
    let output = Command::new("bash")
        .arg(root().join("scripts/test-tickvault-start-guard.sh"))
        .output()
        .expect("run isolated filesystem fixture with the real GNU tools");
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("TV_START_GUARD_FIXTURE_OK cases="));
}

#[test]
fn persistent_dropin_overrides_a_restored_old_unit_executable() {
    let unit = std::fs::read_to_string(root().join("deploy/systemd/tickvault.service")).unwrap();
    let dropin =
        std::fs::read_to_string(root().join("deploy/systemd/tickvault-sequence-guard.conf"))
            .unwrap();
    let commands: Vec<_> = dropin
        .lines()
        .filter_map(|line| line.strip_prefix("ExecStart="))
        .collect();
    assert_eq!(
        commands,
        ["", "/usr/local/libexec/tickvault-start-guard run"],
        "reset the old ExecStart before selecting the guarded execution path"
    );
    assert!(unit.contains("ExecStart=/usr/local/libexec/tickvault-start-guard run"));
    assert!(unit.contains("ExecCondition=/usr/local/libexec/tickvault-start-guard check"));
    assert!(dropin.contains("ExecCondition=/usr/local/libexec/tickvault-start-guard check"));
    assert!(!dropin.contains("/opt/tickvault/repo"));
}

#[test]
fn autopilot_dispatches_only_after_observed_positive_admission() {
    let source = std::fs::read_to_string(root().join("scripts/aws-autopilot.sh")).unwrap();
    let block = source
        .split_once("# PERSISTENT-START-ADMISSION-BEGIN")
        .unwrap()
        .1
        .split_once("# PERSISTENT-START-ADMISSION-END")
        .unwrap()
        .0;
    let stubs = r#"
set -euo pipefail
APP=inactive
ssm_run() {
  case "$1" in
    *"systemctl start tickvault"*) printf 'START_DISPATCHED\n' >&2 ;;
    *"systemctl is-active tickvault"*) printf 'active\n' ;;
    *)
      case "$FIXTURE_ADMISSION" in
        allowed) printf 'TV_START_GUARD_ALLOWED sha256=fixture source_sha=fixture wal_dir=fixture\n' ;;
        fenced) printf 'TV_START_GUARD_REFUSED maintenance_fenced\n' ;;
        unavailable) printf 'guard missing or unreadable\n' ;;
        *) exit 98 ;;
      esac
      ;;
  esac
}
note_ok() { printf 'EXPECTED_STOP\n'; }
note_heal() { printf 'HEAL_OBSERVED\n'; }
note_issue() { printf 'UNVERIFIED_STOP\n'; }
"#;
    for (state, dispatch, verdict) in [
        ("allowed", true, "HEAL_OBSERVED"),
        ("fenced", false, "EXPECTED_STOP"),
        ("unavailable", false, "UNVERIFIED_STOP"),
    ] {
        let result = Command::new("bash")
            .arg("-c")
            .arg(format!("{stubs}\n{block}"))
            .env("FIXTURE_ADMISSION", state)
            .output()
            .unwrap();
        assert!(result.status.success(), "{state}: {result:?}");
        assert_eq!(
            String::from_utf8_lossy(&result.stderr).contains("START_DISPATCHED"),
            dispatch,
            "{state}: no reset/start may be dispatched on a fenced or unknown observation"
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains(verdict));
    }
}
