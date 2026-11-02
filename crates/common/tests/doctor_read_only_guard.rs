//! The health checker may compile/probe, but a warning must never start setup.
//!
//! Run the repository's script in an isolated fixture: all process/service tools
//! are harmless stubs, credentials are fake, and no inherited environment or
//! repository metadata is available. This reproduces shell expansion behavior
//! without starting the application, installing tools, or contacting a service.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct DoctorFixture(PathBuf);

impl DoctorFixture {
    fn new() -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("tv-doctor-read-only-{}-{id}", std::process::id()));
        fs::create_dir(&path).expect("create unique doctor fixture");
        Self(path)
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().expect("fixture file has a parent"))
            .expect("create fixture directories");
        fs::write(path, contents).expect("write fixture file");
    }

    fn executable(&self, relative: &str, contents: &str) {
        self.write(relative, contents);
        fs::set_permissions(self.0.join(relative), fs::Permissions::from_mode(0o700))
            .expect("make fixture stub executable");
    }
}

impl Drop for DoctorFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct DoctorResult {
    output: Output,
    read_calls: String,
    mutation_calls: String,
}

fn run_doctor(user_set: bool, password_set: bool, endpoint_healthy: bool) -> DoctorResult {
    let fixture = DoctorFixture::new();
    fixture.write(
        "scripts/doctor.sh",
        include_str!("../../../scripts/doctor.sh"),
    );
    fixture.write("read-calls", "");
    fixture.write("mutation-calls", "");
    fixture.executable(
        "scripts/validate-automation.sh",
        "#!/bin/bash\nprintf '%s\\n' validate-automation >> \"$DOCTOR_READ_CALLS\"\n",
    );
    fixture.executable(".claude/hooks/error-triage.sh", "#!/bin/bash\nexit 0\n");
    fixture.write("data/logs/machine/app.fixture.log", "fixture\n");
    fixture.write("data/logs/machine/errors.jsonl", "");
    fixture.write(
        "data/logs/machine/errors.summary.md",
        "Zero ERROR-level events\n",
    );
    fixture.executable(
        "bin/cargo",
        r#"#!/bin/bash
if [[ "$*" == 'check --workspace' ]]; then
    printf '%s\n' "cargo $*" >> "$DOCTOR_READ_CALLS"
else
    printf '%s\n' "cargo $*" >> "$DOCTOR_MUTATION_CALLS"
fi
"#,
    );
    fixture.executable(
        "bin/docker",
        r#"#!/bin/bash
if [[ "$*" == 'compose -f deploy/docker/docker-compose.yml ps --services --filter status=running' ]]; then
    printf '%s\n' tv-questdb
else
    printf '%s\n' "docker $*" >> "$DOCTOR_MUTATION_CALLS"
fi
"#,
    );
    fixture.executable(
        "bin/curl",
        r#"#!/bin/bash
printf '%s\n' "curl $*" >> "$DOCTOR_READ_CALLS"
[[ "$DOCTOR_ENDPOINT_HEALTHY" == 1 ]]
"#,
    );
    fixture.executable(
        "bin/date",
        "#!/bin/bash\nprintf '%s\\n' '2026-11-02T00:00:00Z'\n",
    );
    for tool in [
        "make",
        "aws",
        "git",
        "sudo",
        "systemctl",
        "open",
        "xdg-open",
    ] {
        fixture.executable(
            &format!("bin/{tool}"),
            r#"#!/bin/bash
printf '%s\n' "${0##*/} $*" >> "$DOCTOR_MUTATION_CALLS"
"#,
        );
    }
    symlink("/bin/bash", fixture.0.join("bin/bash")).expect("fixture bash");
    symlink("/usr/bin/grep", fixture.0.join("bin/grep")).expect("fixture grep");
    let mut command = Command::new("bash");
    command
        .args(["--noprofile", "--norc"])
        .arg("scripts/doctor.sh")
        .current_dir(&fixture.0)
        .env_clear()
        .env("PATH", fixture.0.join("bin"))
        .env("DOCTOR_READ_CALLS", fixture.0.join("read-calls"))
        .env("DOCTOR_MUTATION_CALLS", fixture.0.join("mutation-calls"))
        .env(
            "DOCTOR_ENDPOINT_HEALTHY",
            if endpoint_healthy { "1" } else { "0" },
        );
    if user_set {
        command.env("TV_QUESTDB_PG_USER", "doctor-user-canary");
    }
    if password_set {
        command.env("TV_QUESTDB_PG_PASSWORD", "doctor-secret-canary-$(make run)");
    }
    let output = command.output().expect("run isolated doctor fixture");
    DoctorResult {
        output,
        read_calls: fs::read_to_string(fixture.0.join("read-calls")).expect("read permitted calls"),
        mutation_calls: fs::read_to_string(fixture.0.join("mutation-calls"))
            .expect("read startup/repair calls"),
    }
}

#[test]
fn doctor_credential_warnings_never_execute_startup_or_disclose_values() {
    for (user_set, password_set) in [(false, false), (true, false), (false, true), (true, true)] {
        let result = run_doctor(user_set, password_set, true);
        let stdout = String::from_utf8_lossy(&result.output.stdout);
        assert!(result.output.status.success(), "{stdout}");
        assert!(
            result.mutation_calls.is_empty(),
            "{}",
            result.mutation_calls
        );
        assert!(result.read_calls.contains("cargo check --workspace\n"));
        assert!(result.read_calls.contains("validate-automation\n"));
        for (variable, is_set) in [
            ("TV_QUESTDB_PG_USER", user_set),
            ("TV_QUESTDB_PG_PASSWORD", password_set),
        ] {
            let expected = if is_set {
                format!("{variable} is set")
            } else {
                format!("{variable} not set (blocks LIVE mode boot — set before `make run`)")
            };
            assert!(stdout.contains(&expected), "{expected}: {stdout}");
        }
        assert!(!stdout.contains("doctor-user-canary"), "{stdout}");
        assert!(!stdout.contains("doctor-secret-canary"), "{stdout}");
    }
}

#[test]
fn doctor_unhealthy_endpoint_still_fails_without_attempting_setup() {
    let result = run_doctor(false, false, false);
    let stdout = String::from_utf8_lossy(&result.output.stdout);
    assert_eq!(result.output.status.code(), Some(1), "{stdout}");
    assert!(
        result.mutation_calls.is_empty(),
        "{}",
        result.mutation_calls
    );
    assert!(stdout.lines().any(|line| {
        line.starts_with("[FAIL]") && line.contains("endpoint") && line.contains("QuestDB HTTP")
    }));
    assert!(!stdout.contains("All health checks green"), "{stdout}");
}
