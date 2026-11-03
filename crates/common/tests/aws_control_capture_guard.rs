//! Exercise aws-control's actual capture-window guard with an owned clock.
//! No AWS actions or credentials are used. The newer Muhurat freeze is retained.
#![cfg(test)]
#![cfg(unix)]

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
const SENSITIVE: [&str; 6] = [
    "stop",
    "reboot",
    "restart-app",
    "stop-app",
    "rotate-ip",
    "restart-questdb",
];

fn workflow() -> String {
    fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/aws-control.yml"),
    )
    .unwrap()
}

fn guard_script(source: &str) -> String {
    let (_, tail) = source
        .split_once("      - name: Market-hours guard (IST 09:00-15:40 Mon-Fri)")
        .unwrap();
    let step = tail.split("\n      - name:").next().unwrap();
    assert!(step.contains("ACTION_INPUT: ${{ github.event.inputs.action }}"));
    assert!(step.contains("FORCE_INPUT: ${{ github.event.inputs.force }}"));
    let (_, run) = step.split_once("\n        run: |\n").unwrap();
    let script = run
        .lines()
        .map(|line| line.strip_prefix("          ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!script.trim().is_empty());
    assert!(
        !script.contains("${{"),
        "dispatch inputs must remain environment values"
    );
    script
}

struct Fixture(PathBuf);
struct Outcome {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}
impl Fixture {
    fn new(muhurat: bool) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "tv-aws-control-guard-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::create_dir(dir.join("bin")).unwrap();
        fs::create_dir(dir.join("config")).unwrap();
        let date = if muhurat { "2026-11-08" } else { "2026-11-09" };
        fs::write(
            dir.join("config/base.toml"),
            format!("[[trading.muhurat_trading_dates]]\ndate = \"{date}\"\n"),
        )
        .unwrap();
        let fixture = Self(dir);
        fixture.script(
            "date",
            r#"case "$*" in
  '-u +%H') printf '%s\n' "$GUARD_TEST_HOUR";;
  '-u +%M') printf '%s\n' "$GUARD_TEST_MINUTE";;
  '-u +%u') printf '%s\n' "$GUARD_TEST_DAY";;
  '+%F') printf '%s\n' '2026-11-08';;
  *) exit 93;;
esac
"#,
        );
        fixture.script("aws", "printf attempted > provider-attempt; exit 94\n");
        fixture
    }
    fn script(&self, name: &str, body: &str) {
        let path = self.0.join("bin").join(name);
        fs::write(&path, format!("#!/bin/bash -p\nset -euo pipefail\n{body}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn run(&self, action: &str, force: &str, hour: &str, minute: &str, day: &str) -> Outcome {
        let stdout_path = self.0.join("stdout");
        let stderr_path = self.0.join("stderr");
        let mut command = Command::new("bash");
        command
            .args([
                "--noprofile",
                "--norc",
                "-p",
                "-c",
                &guard_script(&workflow()),
            ])
            .current_dir(&self.0)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.0.join("bin").display()),
            )
            .env("ACTION_INPUT", action)
            .env("FORCE_INPUT", force)
            .env("GUARD_TEST_HOUR", hour)
            .env("GUARD_TEST_MINUTE", minute)
            .env("GUARD_TEST_DAY", day)
            .env("TICKVAULT_TEST_REAL_SSM", "0")
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .stdout(Stdio::from(fs::File::create(&stdout_path).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr_path).unwrap()));
        // Preserve inherited runtime identity/proxy/CA guards. Bash -p prevents
        // startup scripts and exported functions from hijacking the owned fixture.
        for key in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "TICKVAULT_OPERATOR_KEY",
        ] {
            command.env_remove(key);
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("owned market guard exceeded its five-second deadline");
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let read = |path| {
            let mut bytes = String::new();
            fs::File::open(path)
                .unwrap()
                .take(8193)
                .read_to_string(&mut bytes)
                .unwrap();
            assert!(bytes.len() <= 8192, "owned guard output exceeded its cap");
            bytes
        };
        assert!(
            !self.0.join("provider-attempt").exists(),
            "market guard must not call AWS"
        );
        Outcome {
            status,
            stdout: read(stdout_path),
            stderr: read(stderr_path),
        }
    }
    fn expect(
        &self,
        action: &str,
        force: &str,
        hour: &str,
        minute: &str,
        day: &str,
        blocked: bool,
    ) {
        let out = self.run(action, force, hour, minute, day);
        assert_eq!(
            out.status.code(),
            Some(i32::from(blocked)),
            "action={action:?} force={force:?} clock={hour}:{minute} day={day}"
        );
        assert!(
            out.stderr.is_empty(),
            "owned guard must not produce a shell parsing error"
        );
        assert_eq!(
            out.stdout.contains("blocked during the capture window"),
            blocked
        );
        assert_eq!(out.stdout.contains("guard passed"), !blocked);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn aws_control_capture_guard_covers_all_sensitive_actions_and_inclusive_boundaries() {
    let fixture = Fixture::new(false);
    for action in SENSITIVE {
        for (hour, minute, blocked) in [
            ("03", "29", false),
            ("03", "30", true),
            ("10", "10", true),
            ("10", "11", false),
        ] {
            fixture.expect(action, "no", hour, minute, "1", blocked);
        }
    }
}

#[test]
fn aws_control_capture_guard_handles_decimal_08_and_09_without_shell_errors() {
    let fixture = Fixture::new(false);
    for (hour, minute, day, blocked) in [
        ("08", "08", "1", true),
        ("09", "09", "5", true),
        ("03", "09", "1", false),
        ("10", "09", "5", true),
        ("08", "09", "6", false),
        ("09", "08", "7", false),
    ] {
        fixture.expect("restart-questdb", "no", hour, minute, day, blocked);
        fixture.expect("stop", "no", hour, minute, day, blocked);
    }
}

#[test]
fn aws_control_capture_guard_preserves_weekends_and_exact_force_override() {
    let fixture = Fixture::new(false);
    for action in SENSITIVE {
        fixture.expect(action, "no", "05", "00", "6", false);
        fixture.expect(action, "no", "05", "00", "7", false);
        fixture.expect(action, "yes", "05", "00", "1", false);
        fixture.expect(action, "YES", "05", "00", "1", true);
    }
}

#[test]
fn aws_control_capture_guard_preserves_all_day_muhurat_and_force_semantics() {
    let fixture = Fixture::new(true);
    for action in SENSITIVE {
        fixture.expect(action, "no", "00", "00", "7", true);
        fixture.expect(action, "no", "23", "59", "7", true);
        fixture.expect(action, "yes", "23", "59", "7", false);
    }
}

#[test]
fn aws_control_capture_guard_keeps_noninterrupting_actions_available() {
    for muhurat in [false, true] {
        let fixture = Fixture::new(muhurat);
        for action in ["status", "start", "seed-secrets", "deploy"] {
            fixture.expect(action, "no", "09", "09", "1", false);
        }
    }
}

#[test]
fn aws_control_capture_guard_does_not_execute_shell_text_in_force() {
    let fixture = Fixture::new(false);
    for force in [
        "$(touch input-executed)",
        "`touch input-executed`",
        "\"; touch input-executed; #",
        "yes\ntouch input-executed",
        "yes ",
        "",
    ] {
        fixture.expect("restart-questdb", force, "05", "00", "1", true);
        assert!(!fixture.0.join("input-executed").exists());
    }
}
