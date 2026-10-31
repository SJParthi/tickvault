//! Run the actual secret-scanner hook against isolated, synthetic input.
//! Detection and blocking must survive without copying source into reports.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct ScanDirectory(PathBuf);

impl ScanDirectory {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("tv-secret-scanner-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("config")).unwrap();
        Self(path)
    }

    fn scan(&self, content: &str) -> Output {
        fs::write(self.0.join("config/service.toml"), content).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(".claude/hooks/secret-scanner.sh");
        Command::new("bash")
            .args(["--noprofile", "--norc"])
            .arg(script)
            .arg(&self.0)
            .arg("config/service.toml")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .output()
            .unwrap()
    }
}

impl Drop for ScanDirectory {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.0));
    }
}

#[test]
fn secret_scanner_blocks_credentials_without_printing_source() {
    // Synthetic canaries deliberately avoid the scanner's fixture-word filters.
    let canaries = ["Q7m4R9v2C6n8D3s5", "Z8k5F2h9L4r6M3n7", "P3d7W9b2T6j8H4v5"];
    let input = format!(
        "region = \"ap-south-1\"\naccess_token = \"{}\"\npassword = \"{}\"\napi_key = \"{}\"\n",
        canaries[0], canaries[1], canaries[2]
    );
    let output = ScanDirectory::new().scan(&input);
    assert_eq!(output.status.code(), Some(2), "credentials must block");
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for canary in canaries {
        assert!(!report.contains(canary), "report copied a credential value");
    }
    for source in ["access_token =", "password =", "api_key ="] {
        assert!(!report.contains(source), "report copied matched source");
    }
    assert!(report.contains("BLOCKED: 3 potential secret(s)"));
    for category in [
        "Hardcoded access token",
        "Hardcoded password",
        "Hardcoded API key",
    ] {
        assert!(
            report.contains(&format!("[SECRET] {category} in config/service.toml:")),
            "report must retain each detection category and path"
        );
    }
    for line in [2, 3, 4] {
        assert!(report.contains(&format!("line {line} (source omitted)")));
    }
}

#[test]
fn secret_scanner_still_accepts_clean_input() {
    let output = ScanDirectory::new().scan("region = \"ap-south-1\"\nretries = 3\n");
    assert!(output.status.success(), "clean input must remain accepted");
    assert!(String::from_utf8_lossy(&output.stderr).contains("Secret scan: CLEAN"));
}
