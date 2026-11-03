//! Behavioral checks for the existing pre-commit scanners, using isolated files.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
const BANNED_SCANNER: &str = include_str!("../../../.claude/hooks/banned-pattern-scanner.sh");
const SECRET_SCANNER: &str = include_str!("../../../.claude/hooks/secret-scanner.sh");

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("tv-scanner-{}-{id}", std::process::id()));
        fs::create_dir(&path).expect("create exclusive scanner fixture");
        Self(path)
    }

    fn write(&self, path: &str, body: &str) {
        let target = self.0.join(path);
        fs::create_dir_all(target.parent().expect("fixture parent")).expect("fixture parents");
        fs::write(target, body).expect("write scanner fixture");
    }

    fn run(&self, scanner: &str, paths: &[&str]) -> Output {
        self.write("scanner.sh", scanner);
        Command::new("bash")
            .args(["--noprofile", "--norc"])
            .arg(self.0.join("scanner.sh"))
            .arg(&self.0)
            .arg(paths.join("\n"))
            .current_dir(&self.0)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .output()
            .expect("run scanner on isolated fixture")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn banned_scanner_preserves_production_and_hot_path_detection() {
    let fixture = Fixture::new();
    fixture.write(
        "crates/common/src/production.rs",
        r#"fn production() { let n = Some(1).unwrap(); }
#[cfg(test)]
use canary::unwrap;
fn after_test_import() { let n = Some(2).unwrap(); }
fn allowed() { let n = Some(4).unwrap(); } // APPROVED: isolated fixture
#[cfg(test)] mod tests { fn unit() { let n = Some(3).unwrap(); } }
"#,
    );
    fixture.write(
        "crates/core/src/pipeline/worker.rs",
        r#"fn pipeline() { let x = value.clone(); std::fs::write("path", b"x"); }
// .clone() in a comment must not count.
// APPROVED: cold allocation
fn exempted() { let x = value.clone(); }
fn literal_control() { let x = value.clone; }
#[cfg(test)] mod tests { fn unit() { let x = value.clone(); } }
"#,
    );
    fixture.write(
        "crates/common/tests/ignored.rs",
        "fn test() { value.unwrap(); }\n",
    );
    fixture.write(
        "crates/trading/src/oms/cold.rs",
        "fn cold() { let x = value.clone(); }\n",
    );
    let output = fixture.run(
        BANNED_SCANNER,
        &[
            "crates/common/src/production.rs",
            "crates/core/src/pipeline/worker.rs",
            "crates/common/tests/ignored.rs",
            "crates/trading/src/oms/cold.rs",
            "missing.rs",
        ],
    );
    let report = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert!(output.stdout.is_empty());
    assert!(
        report.contains("BLOCKED: 4 banned pattern violation(s)"),
        "{report}"
    );
    assert!(report.contains("1: fn production()"), "{report}");
    assert!(report.contains("4: fn after_test_import()"), "{report}");
    assert!(report.contains(".clone() on hot path"), "{report}");
    assert!(report.contains("std::fs::write on hot path"), "{report}");
    assert!(!report.contains("fn exempted()"), "{report}");
    assert!(!report.contains("fn literal_control()"), "{report}");
    assert!(!report.contains("fn unit()"), "{report}");
    assert!(!report.contains("fn allowed()"), "{report}");
    let clean = fixture.run(
        BANNED_SCANNER,
        &[
            "crates/common/tests/ignored.rs",
            "crates/trading/src/oms/cold.rs",
            "missing.rs",
        ],
    );
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );
    assert!(String::from_utf8_lossy(&clean.stderr).contains("CLEAN"));
}

#[test]
fn secret_scanner_preserves_all_families_and_comment_boundaries() {
    let fixture = Fixture::new();
    fixture.write(
        "crates/common/src/credentials.rs",
        r#"api_key = "A1B2C3D4E5" // stub for tests
API_SECRET = "A1B2C3D4E5"
access_token = "A1B2C3D4E5" # dummy annotation
auth_token = "A1B2C3D4E5"
aws_key = "AKIA1234567890ABCDEF"
aws_secret_access_key = "A1B2C3D4E5"
client_id = "1234567890"
dhan_token = "eyJSyntheticCanary"
password = "A1B2C3D4E5"
passwd = "A1B2C3D4E5"
private_key = "A1B2C3D4E5"
-----BEGIN PRIVATE KEY-----
Authorization: Bearer AbCdEfGhIjKlMnOpQrStUvWxYz
bot12345678:synthetic-canary
postgresql://alice:canary@db:5432/db
redis://:canary@db:6379
password = "REDACTED"
password = "allowlisted" // secret-scan-ignore
access_token = "https://host.invalid/A1B2C3D4#fragment" // stub for tests
"#,
    );
    for path in [
        "nested/Dockerfile",
        "spaced path/MakeFILE",
        "crates/common/tests/ignored.rs",
        "crates/common/fixtures/ignored.toml",
        "secret-scan.yml",
        "Cargo.lock",
        "assets/a.png",
        "notes.md",
    ] {
        fixture.write(path, "ENV password = \"A1B2C3D4E5\"\n");
    }
    let output = fixture.run(
        SECRET_SCANNER,
        &[
            "crates/common/src/credentials.rs",
            "nested/Dockerfile",
            "spaced path/MakeFILE",
            "crates/common/tests/ignored.rs",
            "crates/common/fixtures/ignored.toml",
            "secret-scan.yml",
            "Cargo.lock",
            "assets/a.png",
            "notes.md",
            "missing.rs",
            "crates/common/src/credentials.rs",
        ],
    );
    let report = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert!(output.stdout.is_empty());
    assert!(
        report.contains("BLOCKED: 19 potential secret(s)"),
        "{report}"
    );
    for family in [
        "API key",
        "API secret",
        "access token",
        "auth token",
        "AWS Access Key ID",
        "AWS Secret Access Key",
        "Dhan client ID",
        "Dhan JWT token",
        "password",
        "private key",
        "Embedded private key",
        "bearer token",
        "Telegram bot token",
        "Database connection string",
        "Redis connection string",
    ] {
        assert!(report.contains(family), "missing {family}: {report}");
    }
    // The URL and comment-bearing credentials must still be detected, but the
    // security report must contain locations only, never matched source values.
    let source = "crates/common/src/credentials.rs";
    let mut expected: Vec<(&str, &str, usize)> = [
        ("Hardcoded API key", 1),
        ("Hardcoded API secret", 2),
        ("Hardcoded access token", 3),
        ("Hardcoded auth token", 4),
        ("AWS Access Key ID", 5),
        ("AWS Secret Access Key", 6),
        ("Hardcoded Dhan client ID", 7),
        ("Hardcoded Dhan JWT token", 8),
        ("Hardcoded password", 9),
        ("Hardcoded password", 10),
        ("Hardcoded private key", 11),
        ("Embedded private key", 12),
        ("Hardcoded bearer token", 13),
        ("Hardcoded Telegram bot token", 14),
        ("Database connection string with password", 15),
        ("Redis connection string with password", 16),
        ("Hardcoded access token", 19),
    ]
    .into_iter()
    .map(|(family, line)| (family, source, line))
    .collect();
    expected.extend([
        ("Hardcoded password", "nested/Dockerfile", 1),
        ("Hardcoded password", "spaced path/MakeFILE", 1),
    ]);
    let mut actual = Vec::new();
    let mut context = None;
    for line in report.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix("[SECRET] ") {
            context = header.strip_suffix(':').and_then(|s| s.rsplit_once(" in "));
        } else if let Some(location) = line
            .strip_prefix("line ")
            .and_then(|s| s.strip_suffix(" (source omitted)"))
        {
            let (family, file) = context.expect("each location has a category and path");
            actual.push((
                family,
                file,
                location.parse::<usize>().expect("source line"),
            ));
        }
    }
    expected.sort_unstable();
    actual.sort_unstable();
    assert_eq!(
        actual, expected,
        "every unique finding appears exactly once"
    );
    for canary in [
        "A1B2C3D4E5",
        "AKIA1234567890ABCDEF",
        "eyJSyntheticCanary",
        "AbCdEfGhIjKlMnOpQrStUvWxYz",
        "synthetic-canary",
        "postgresql://alice:canary@db:5432/db",
        "redis://:canary@db:6379",
        "https://host.invalid/A1B2C3D4#fragment",
    ] {
        assert!(!report.contains(canary), "source canary leaked: {report}");
    }
    assert!(report.contains("spaced path/MakeFILE"), "{report}");
    assert!(!report.contains("REDACTED"), "{report}");
    assert!(!report.contains("allowlisted"), "{report}");
    assert!(!report.contains("stub for tests"), "{report}");
    assert!(!report.contains("ignored.rs"), "{report}");
    let clean = fixture.run(
        SECRET_SCANNER,
        &[
            "crates/common/tests/ignored.rs",
            "crates/common/fixtures/ignored.toml",
            "secret-scan.yml",
            "Cargo.lock",
            "assets/a.png",
            "notes.md",
            "missing.rs",
        ],
    );
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );
    assert!(String::from_utf8_lossy(&clean.stderr).contains("CLEAN"));
}
