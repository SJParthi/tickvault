//! Run the actual on-box recovery script with owned, nonprivileged stubs.
//! All filesystem, service and HTTP commands target synthetic fixture state.
#![cfg(test)]
#![cfg(target_os = "linux")]

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn recovery_script() -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let workflow =
        fs::read_to_string(root.join(".github/workflows/emergency-fs-recover.yml")).unwrap();
    let (_, tail) = workflow
        .split_once("          cat > recover.sh <<'EOS'\n")
        .unwrap();
    let (body, _) = tail.split_once("\n          EOS").unwrap();
    let script = body
        .lines()
        .map(|line| line.strip_prefix("          ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!script.contains("${{"));
    assert_eq!(script.matches("__FORCE_DISK__").count(), 1);
    script.replace("__FORCE_DISK__", "no")
}

fn outer_recovery_script() -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let workflow =
        fs::read_to_string(root.join(".github/workflows/emergency-fs-recover.yml")).unwrap();
    let (_, tail) = workflow
        .split_once(
            "      - name: Push a 60-second key via EC2 Instance Connect and run the recovery",
        )
        .unwrap();
    let step = tail.split("\n      - name:").next().unwrap();
    let (_, body) = step.split_once("\n        run: |\n").unwrap();
    let script = body
        .lines()
        .map(|line| line.strip_prefix("          ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!script.contains("${{"));
    script
}

struct Fixture(PathBuf);
struct Outcome {
    code: Option<i32>,
    state: String,
    calls: String,
    output: String,
}
impl Fixture {
    fn new(initial: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tv-recovery-state-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("bin")).unwrap();
        fs::write(path.join("state"), initial).unwrap();
        let fixture = Self(path);
        fixture.script("systemctl", SYSTEMCTL);
        fixture.script("sudo", SUDO);
        fixture.script("df", "case \"$*\" in '--output=pcent /') printf 'Use%%\\n%s%%\\n' \"$RECOVERY_USED_PCT\";; *) printf 'owned-disk 1G\\n';; esac\n");
        fixture.script("findmnt", "case \"$*\" in '-no SOURCE /') echo /dev/fixture1;; '-no FSTYPE /') echo xfs;; *) exit 96;; esac\n");
        fixture.script("lsblk", "case \"$*\" in '-no PKNAME /dev/fixture1') echo fixture;; '-no KNAME /dev/fixture1') if [ \"$RECOVERY_FAILURE\" = partition ]; then echo invalid; else echo fixture1; fi;; *) exit 96;; esac\n");
        fixture.script(
            "curl",
            "printf call >> \"$FIXTURE/http-attempts\"; [ \"$RECOVERY_FAILURE\" != questdb ]\n",
        );
        fixture.script(
            "seq",
            "for ((n=$1; n<=$2; n++)); do printf '%s\\n' \"$n\"; done\n",
        );
        for name in ["sync", "sleep"] {
            fixture.script(name, "exit 0\n");
        }
        for name in ["aws", "ssh"] {
            fixture.script(
                name,
                "printf forbidden > \"$FIXTURE/forbidden-command\"; exit 99\n",
            );
        }
        fixture
    }
    fn script(&self, name: &str, body: &str) {
        let path = self.0.join("bin").join(name);
        fs::write(&path, format!("#!/bin/bash -p\nset -euo pipefail\n{body}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn run(
        &self,
        initial_rc: &str,
        failure: &str,
        enable: &str,
        restored_read: &str,
        used: &str,
    ) -> Outcome {
        let stdout = self.0.join("stdout");
        let stderr = self.0.join("stderr");
        let script = if self.0.join("outer-mode").exists() {
            outer_recovery_script()
        } else {
            recovery_script()
        };
        let mut command = Command::new("bash");
        command
            .args(["--noprofile", "--norc", "-p", "-c", &script])
            .current_dir(&self.0)
            .env("FIXTURE", &self.0)
            .env("IID", "i-owned-fixture")
            .env("IP", "127.0.0.1")
            .env("AZ", "owned-zone")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.0.join("bin").display()),
            )
            .env("RECOVERY_INITIAL_RC", initial_rc)
            .env("RECOVERY_FAILURE", failure)
            .env("RECOVERY_ENABLE_MODE", enable)
            .env("RECOVERY_RESTORED_READ", restored_read)
            .env("RECOVERY_USED_PCT", used)
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env("TICKVAULT_TEST_REAL_SSM", "0")
            .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()));
        // Retain runtime identity/proxy/CA guards; remove only the approved
        // four provider secrets from this fake-command child. Bash -p also
        // applies to the owned helper scripts, excluding startup injection.
        for key in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "TICKVAULT_OPERATOR_KEY",
        ] {
            command.env_remove(key);
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("owned recovery exceeded ten-second deadline");
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let read = |path: PathBuf| {
            let mut text = String::new();
            if let Ok(file) = fs::File::open(path) {
                file.take(16385).read_to_string(&mut text).unwrap();
            }
            assert!(text.len() <= 16384, "owned output exceeded cap");
            text
        };
        assert!(!self.0.join("forbidden-command").exists());
        let calls = read(self.0.join("calls"));
        assert!(
            !calls.lines().any(|line| matches!(
                line,
                "start tickvault" | "restart tickvault" | "unmask tickvault"
            )),
            "recovery must never activate or unmask the app"
        );
        Outcome {
            code: status.code(),
            state: read(self.0.join("state")),
            calls,
            output: read(stdout) + &read(stderr),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SUDO: &str = r#"
case "$1" in
 systemctl) shift; exec "$FIXTURE/bin/systemctl" "$@";;
 du|journalctl|dnf|growpart|xfs_growfs|resize2fs|docker) printf '%s\n' "$*" >> "$FIXTURE/other-commands";;
 *) printf forbidden > "$FIXTURE/forbidden-command"; exit 99;;
esac
"#;
const SYSTEMCTL: &str = r#"
printf '%s\n' "$*" >> "$FIXTURE/calls"
case "$1" in
 is-enabled)
   n=0; [ ! -f "$FIXTURE/reads" ] || read -r n < "$FIXTURE/reads"
   printf '%s\n' "$((n+1))" > "$FIXTURE/reads"
   state=$(cat "$FIXTURE/state")
   if [ "$n" -gt 0 ] && [ "$RECOVERY_RESTORED_READ" = incoherent ]; then printf '%s\n' "$state"; exit 2; fi
   if [ "$n" -gt 0 ] && [ "$RECOVERY_RESTORED_READ" = unreadable ]; then exit 2; fi
   printf '%s\n' "$state"
   if [ "$n" = 0 ]; then exit "$RECOVERY_INITIAL_RC"; fi
   case "$state" in enabled|enabled-runtime) exit 0;; disabled) exit 1;; *) exit 2;; esac;;
 disable) printf disabled > "$FIXTURE/state";;
 enable)
   case "$RECOVERY_ENABLE_MODE" in fail) exit 1;; noop) exit 0;; esac
   case "$*" in 'enable tickvault') printf enabled > "$FIXTURE/state";; 'enable --runtime tickvault') printf enabled-runtime > "$FIXTURE/state";; *) exit 96;; esac;;
 stop) [ "$*" = 'stop tickvault' ] || exit 96;;
 restart|start|reset-failed)
   case "$*" in *tickvault*) printf forbidden > "$FIXTURE/forbidden-command"; exit 99;; esac;;
 is-active) echo active;;
 *) printf forbidden > "$FIXTURE/forbidden-command"; exit 99;;
esac
"#;

#[test]
fn emergency_recovery_restores_original_enablement_on_success_and_both_failure_paths() {
    for (initial, initial_rc) in [
        ("enabled", "0"),
        ("enabled-runtime", "0"),
        ("disabled", "1"),
    ] {
        for failure in ["none", "partition", "questdb"] {
            let fixture = Fixture::new(initial);
            let out = fixture.run(initial_rc, failure, "ok", "ok", "95");
            assert_eq!(
                out.code,
                Some(i32::from(failure != "none")),
                "initial={initial} failure={failure}"
            );
            assert_eq!(out.state, initial, "initial={initial} failure={failure}");
            assert_eq!(
                out.calls
                    .lines()
                    .filter(|line| line.starts_with("enable "))
                    .count(),
                usize::from(initial != "disabled")
            );
            assert!(out.calls.contains("stop tickvault\n"));
            assert!(out.calls.contains(if initial == "enabled-runtime" {
                "disable --runtime tickvault\n"
            } else {
                "disable tickvault\n"
            }));
            if failure == "questdb" {
                assert_eq!(
                    fs::read(fixture.0.join("http-attempts")).unwrap().len(),
                    90 * 4
                );
            }
        }
    }
}

#[test]
fn emergency_recovery_refuses_unknown_or_incoherent_enablement_before_mutation() {
    for (initial, initial_rc) in [
        ("masked", "1"),
        ("masked-runtime", "1"),
        ("static", "0"),
        ("not-found", "4"),
        ("", "1"),
        ("enabled", "1"),
        ("disabled", "0"),
        ("enabled\nextra", "0"),
        ("enabled", "124"),
    ] {
        let fixture = Fixture::new(initial);
        let out = fixture.run(initial_rc, "none", "ok", "ok", "95");
        assert_eq!(out.code, Some(3), "state={initial:?} status={initial_rc}");
        assert_eq!(out.state, initial);
        assert_eq!(out.calls, "is-enabled tickvault\n");
        assert!(out.output.contains("REFUSED:"));
        assert!(!fixture.0.join("other-commands").exists());
    }
}

#[test]
fn emergency_recovery_does_not_claim_success_when_restoration_fails_or_is_unknown() {
    for (enable, restored_read) in [
        ("fail", "ok"),
        ("noop", "ok"),
        ("ok", "incoherent"),
        ("ok", "unreadable"),
    ] {
        let fixture = Fixture::new("enabled");
        let out = fixture.run("0", "none", enable, restored_read, "95");
        assert_eq!(out.code, Some(4), "enable={enable} read={restored_read}");
        assert!(out.output.contains("FAILED:"));
    }
    let fixture = Fixture::new("disabled");
    let out = fixture.run("1", "none", "ok", "unreadable", "95");
    assert_eq!(out.code, Some(4));
    assert_eq!(out.state, "disabled");
    assert!(!out.calls.lines().any(|line| line.starts_with("enable ")));
}

#[test]
fn emergency_recovery_disk_refusal_leaves_services_completely_untouched() {
    for initial in ["enabled", "enabled-runtime", "disabled"] {
        let fixture = Fixture::new(initial);
        let out = fixture.run("0", "none", "ok", "ok", "94");
        assert_eq!(out.code, Some(3));
        assert_eq!(out.state, initial);
        assert!(out.calls.is_empty());
        assert!(!fixture.0.join("other-commands").exists());
    }
}

impl Fixture {
    fn outer(&self, first_restore_fails: bool) {
        fs::write(
            self.0.join("outer-mode"),
            if first_restore_fails {
                "fail-first"
            } else {
                "normal"
            },
        )
        .unwrap();
        fs::write(self.0.join("recover.sh"), recovery_script()).unwrap();
        self.script("aws", "[ \"$1 $2\" = 'ec2-instance-connect send-ssh-public-key' ] || exit 99; printf '{\"Success\": true}\\n'\n");
        self.script("ssh-keygen", ": > eic_key; : > eic_key.pub\n");
        self.script("ssh", r#"
count=0; [ ! -f "$FIXTURE/ssh-attempts" ] || read -r count < "$FIXTURE/ssh-attempts"
count=$((count+1)); printf '%s\n' "$count" > "$FIXTURE/ssh-attempts"
if [ "$count" = 1 ] && [ "$(cat "$FIXTURE/outer-mode")" = fail-first ]; then export RECOVERY_ENABLE_MODE=fail; else export RECOVERY_ENABLE_MODE=ok; fi
exec /bin/bash --noprofile --norc -p -s
"#);
    }
}

#[test]
fn emergency_recovery_outer_retry_cannot_rebase_a_failed_restore_onto_disabled() {
    let fixture = Fixture::new("enabled");
    fixture.outer(true);
    let out = fixture.run("0", "none", "ok", "ok", "95");
    let attempts: usize = fs::read_to_string(fixture.0.join("ssh-attempts"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        (out.code, out.state.as_str(), attempts),
        (Some(1), "disabled", 1),
        "an unconfirmed restore must terminate before a second invocation can call disabled its new original state"
    );
    assert!(out.output.contains("No further SSH attempts will run"));
}

#[test]
fn emergency_recovery_outer_success_preserves_enabled_without_retry_or_start() {
    let fixture = Fixture::new("enabled");
    fixture.outer(false);
    let out = fixture.run("0", "none", "ok", "ok", "95");
    assert_eq!(out.code, Some(0));
    assert_eq!(out.state, "enabled");
    assert_eq!(
        fs::read_to_string(fixture.0.join("ssh-attempts")).unwrap(),
        "1\n"
    );
}
