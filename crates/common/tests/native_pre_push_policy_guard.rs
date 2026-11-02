//! Execute the real native hook against owned fake command boundaries.
//! No Cargo, advisory server, Git remote or workspace checkout is invoked.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HOOK: &str = include_str!("../../../scripts/git-hooks/pre-push");
const HEAD: &str = "1111111111111111111111111111111111111111";
const PARENT: &str = "2222222222222222222222222222222222222222";
const FIXTURE_DEADLINE: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = (0..32)
            .find_map(|_| {
                let id = NEXT.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("tickvault native push {} {id}", std::process::id()));
                match fs::create_dir(&path) {
                    Ok(()) => Some(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(error) => panic!("exclusive fixture directory: {error}"),
                }
            })
            .expect("exclusive fixture collision budget");
        let fixture = Self(root);
        fixture.executable("hook", HOOK);
        fixture.write("crates/core/src/lib.rs", "// Owned fixture source.\n");
        fixture.write(
            "crates/core/tests/loom_fixture.rs",
            "// Owned Loom target.\n",
        );
        fixture.write(
            "crates/trading/tests/loom_fixture.rs",
            "// Owned Loom target.\n",
        );
        fixture.executable(
            "bin/git",
            r#"#!/bin/bash -p
case "$*" in
  "rev-parse --show-toplevel") printf '%s\n' "$PWD" ;;
  "rev-parse HEAD") printf '%s\n' "1111111111111111111111111111111111111111" ;;
  "rev-parse HEAD~1"|"merge-base origin/main HEAD") printf '%s\n' "2222222222222222222222222222222222222222" ;;
  "diff --name-only "*) printf '%s\n' "crates/core/src/lib.rs" ;;
  *) exit 2 ;;
esac
"#,
        );
        fixture.executable(
            "bin/cargo",
            r#"#!/bin/bash -p
printf '%s\n' "$*" >> "$NATIVE_PUSH_CARGO_LOG"
if [ "$*" = "$NATIVE_PUSH_FAIL_CARGO" ]; then exit 101; fi
exit 0
"#,
        );
        fixture.executable(
            "bin/timeout",
            r#"#!/bin/bash -p
budget=$1
shift
printf '%s|%s\n' "$budget" "$*" >> "$NATIVE_PUSH_TIMEOUT_LOG"
if [ "$NATIVE_PUSH_FAKE_TIMEOUT" = 1 ] && [ "$*" = "cargo test --workspace --locked" ]; then exit 124; fi
exec "$@"
"#,
        );
        // Make optional-tool discovery deterministic; the hook calls the
        // owned fake `cargo` above for their checks, never these executables.
        fixture.executable("bin/cargo-audit", "#!/bin/bash -p\nexit 0\n");
        fixture.executable("bin/cargo-deny", "#!/bin/bash -p\nexit 0\n");
        for guard in [
            "plan-gate.sh",
            "banned-pattern-scanner.sh",
            "test-count-guard.sh",
            "data-integrity-guard.sh",
            "pub-fn-test-guard.sh",
            "financial-test-guard.sh",
        ] {
            fixture.executable(
                &format!(".claude/hooks/{guard}"),
                r#"#!/bin/bash -p
guard=${0##*/}
printf '%s\n' "$guard" >> "$NATIVE_PUSH_GUARD_LOG"
if [ "$guard" = "$NATIVE_PUSH_FAIL_GUARD" ]; then exit 2; fi
exit 0
"#,
            );
        }
        fixture
    }

    fn write(&self, name: &str, body: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("fixture parents");
        fs::write(path, body).expect("fixture file");
    }

    fn executable(&self, name: &str, body: &str) {
        self.write(name, body);
        fs::set_permissions(self.0.join(name), fs::Permissions::from_mode(0o700))
            .expect("fixture executable");
    }

    fn receipt(&self, body: &str) {
        self.write(".claude/hooks/.last-quality-pass", body);
    }

    fn run(
        &self,
        full_qa: Option<&str>,
        fail_cargo: &str,
        fail_guard: &str,
        workspace_timeout: bool,
    ) -> Observation {
        self.run_inner(
            full_qa,
            fail_cargo,
            fail_guard,
            workspace_timeout,
            (false, false),
        )
    }

    fn run_inner(
        &self,
        full_qa: Option<&str>,
        fail_cargo: &str,
        fail_guard: &str,
        workspace_timeout: bool,
        hostile_shell: (bool, bool),
    ) -> Observation {
        let paths = [
            self.0.join("bin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ];
        let output_path = self.0.join("output.log");
        let output = fs::File::create(&output_path).expect("fixture output");
        let mut command = Command::new("bash");
        command
            // Privileged Bash here prevents imported functions and startup
            // hooks; this owned fixture does not change production privileges.
            .args(["--noprofile", "--norc", "-p"])
            .arg(self.0.join("hook"))
            .current_dir(&self.0)
            .env("PATH", std::env::join_paths(paths).expect("fixture PATH"))
            .env("NATIVE_PUSH_CARGO_LOG", self.0.join("cargo.log"))
            .env("NATIVE_PUSH_TIMEOUT_LOG", self.0.join("timeout.log"))
            .env("NATIVE_PUSH_GUARD_LOG", self.0.join("guards.log"))
            .env("NATIVE_PUSH_FAIL_CARGO", fail_cargo)
            .env("NATIVE_PUSH_FAIL_GUARD", fail_guard)
            .env(
                "NATIVE_PUSH_FAKE_TIMEOUT",
                if workspace_timeout { "1" } else { "0" },
            )
            .env_remove("FULL_QA")
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .env_remove("SHELLOPTS")
            .env_remove("BASHOPTS")
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone().expect("output clone")))
            .stderr(Stdio::from(output));
        if let Some(value) = full_qa {
            command.env("FULL_QA", value);
        }
        if hostile_shell.0 {
            // Harmless child-only startup witness. The inherited startup
            // configuration was removed above; this owned replacement
            // verifies that -p also rejects a supplied startup file.
            self.write(
                "hostile-startup",
                "printf startup > \"$NATIVE_PUSH_STARTUP_WITNESS\"\nexit 97\n",
            );
            command.env("BASH_ENV", self.0.join("hostile-startup")).env(
                "NATIVE_PUSH_STARTUP_WITNESS",
                self.0.join("startup-witness"),
            );
        }
        if hostile_shell.1 {
            // `timeout` is a direct invocation; cargo is behind its exec.
            // `printf` also witnesses imported functions inside helper shells.
            // Owned helper shebangs use bash -p as well as the initial child.
            command
                .env(
                    "NATIVE_PUSH_FUNCTION_WITNESS",
                    self.0.join("function-witness"),
                )
                .env(
                    "BASH_FUNC_cargo%%",
                    "() { printf function > \"$NATIVE_PUSH_FUNCTION_WITNESS\"; return 97; }",
                )
                .env(
                    "BASH_FUNC_timeout%%",
                    "() { printf function > \"$NATIVE_PUSH_FUNCTION_WITNESS\"; return 97; }",
                )
                .env(
                    "BASH_FUNC_printf%%",
                    "() { builtin printf function > \"$NATIVE_PUSH_FUNCTION_WITNESS\"; return 97; }",
                );
        }
        // Inherit proxy/CA/runtime guards. The fake commands cannot reach a
        // provider; credentials are additionally removed only in this child.
        for key in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "TICKVAULT_OPERATOR_KEY",
        ] {
            command.env_remove(key);
        }
        command.env("TICKVAULT_TEST_REAL_SSM", "0");
        command.env("AWS_EC2_METADATA_DISABLED", "true");
        let mut child = command.spawn().expect("owned native hook fixture");
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().expect("fixture wait") {
                break status;
            }
            if started.elapsed() >= FIXTURE_DEADLINE {
                let _ = child.kill();
                let _ = child.wait();
                panic!("owned hook fixture exceeded {FIXTURE_DEADLINE:?}");
            }
            std::thread::sleep(POLL_INTERVAL);
        };
        Observation {
            status,
            output: read(&output_path),
            cargo: read(&self.0.join("cargo.log")),
            timeouts: read(&self.0.join("timeout.log")),
            guards: read(&self.0.join("guards.log")),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

struct Observation {
    status: ExitStatus,
    output: String,
    cargo: String,
    timeouts: String,
    guards: String,
}

fn assert_first_five_reached(observed: &Observation) {
    assert!(
        observed
            .cargo
            .lines()
            .any(|line| line == "fmt --all -- --check")
    );
    assert!(
        observed
            .cargo
            .lines()
            .any(|line| { line == "clippy --workspace --all-targets --locked -- -D warnings" })
    );
    assert!(
        observed
            .timeouts
            .lines()
            .any(|line| line == "1800|cargo test --workspace --locked")
    );
    assert!(
        observed
            .guards
            .lines()
            .any(|line| line == "banned-pattern-scanner.sh")
    );
    assert!(
        observed
            .guards
            .lines()
            .any(|line| line == "test-count-guard.sh")
    );
    assert!(!observed.output.contains("SKIP: Already verified"));
}

#[test]
fn default_and_full_qa_never_trust_shared_receipts_to_hide_failure_or_timeout() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("epoch")
        .as_secs();
    let receipts = [
        format!("{HEAD} {now} 123\n"),
        format!("{PARENT} {now} 123\n"),
        format!("{HEAD} {} 123\n", now.saturating_sub(600)),
        format!("{HEAD} {} 123\n", now + 600),
        "not a valid receipt\n".to_owned(),
    ];
    for full_qa in [None, Some("0"), Some("1")] {
        for receipt in &receipts {
            for workspace_timeout in [false, true] {
                let fixture = Fixture::new();
                fixture.receipt(receipt);
                let observed =
                    fixture.run(full_qa, "test --workspace --locked", "", workspace_timeout);
                assert_eq!(observed.status.code(), Some(1), "{}", observed.output);
                assert_first_five_reached(&observed);
                assert!(observed.output.contains("PUSH BLOCKED"));
                assert_eq!(
                    read(&fixture.0.join(".claude/hooks/.last-quality-pass")),
                    *receipt,
                    "a failed run must not write a success receipt"
                );
                if workspace_timeout {
                    assert!(observed.output.contains("1800s"));
                    assert!(observed.output.contains("verification incomplete"));
                } else {
                    assert!(
                        observed
                            .cargo
                            .lines()
                            .any(|line| line == "test --workspace --locked")
                    );
                    assert!(observed.output.contains("cargo test failed"));
                }
            }
        }
    }
}

#[test]
fn format_clippy_banned_and_count_failures_still_block_native_push() {
    for (cargo, guard) in [
        ("fmt --all -- --check", ""),
        (
            "clippy --workspace --all-targets --locked -- -D warnings",
            "",
        ),
        ("", "banned-pattern-scanner.sh"),
        ("", "test-count-guard.sh"),
    ] {
        let fixture = Fixture::new();
        let observed = fixture.run(None, cargo, guard, false);
        assert_eq!(observed.status.code(), Some(1), "{}", observed.output);
        assert_first_five_reached(&observed);
        assert!(!fixture.0.join(".claude/hooks/.last-quality-pass").exists());
    }
}

#[test]
fn successful_native_push_runs_all_checks_and_only_then_writes_its_receipt() {
    let fixture = Fixture::new();
    let observed = fixture.run(None, "", "", false);
    assert!(observed.status.success(), "{}", observed.output);
    assert_first_five_reached(&observed);
    for command in [
        "test --workspace --locked",
        "audit --deny yanked",
        "deny check",
        "test --locked -p tickvault-core --features loom --test loom_fixture",
        "test --locked -p tickvault-trading --features loom --test loom_fixture",
    ] {
        assert!(
            observed.cargo.lines().any(|line| line == command),
            "missing {command}"
        );
    }
    for guard in [
        "plan-gate.sh",
        "data-integrity-guard.sh",
        "pub-fn-test-guard.sh",
        "financial-test-guard.sh",
    ] {
        assert!(
            observed.guards.lines().any(|line| line == guard),
            "missing {guard}"
        );
    }
    assert!(read(&fixture.0.join(".claude/hooks/.last-quality-pass")).starts_with(HEAD));
    assert!(observed.output.contains("ALL 11 GATES PASSED"));
}

#[test]
fn inherited_shell_startup_and_exported_functions_cannot_escape_the_owned_stubs() {
    for hostile in [(true, false), (false, true), (true, true)] {
        let fixture = Fixture::new();
        let observed = fixture.run_inner(None, "", "", false, hostile);
        assert!(observed.status.success(), "{}", observed.output);
        assert_first_five_reached(&observed);
        assert!(!fixture.0.join("startup-witness").exists());
        assert!(!fixture.0.join("function-witness").exists());
        assert!(
            observed
                .cargo
                .lines()
                .any(|line| line == "test --workspace --locked")
        );
    }
}
