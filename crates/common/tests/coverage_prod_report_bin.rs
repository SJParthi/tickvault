//! Runs the `coverage_prod_report` binary end to end, so its `main()` (exit
//! codes, stdout, the `GITHUB_STEP_SUMMARY` append) runs under coverage like
//! the sibling bins (`coverage_gaps_common.rs`, `bench_ab_recheck.rs`).

use std::path::PathBuf;
use std::process::Command;

/// Writes `text` at `<temp>/coverage_prod_report_bin_<pid>_<case>/<relative>`.
fn temp_file(case: &str, relative: &str, text: &str) -> PathBuf {
    let path = std::env::temp_dir()
        .join(format!(
            "coverage_prod_report_bin_{}_{case}",
            std::process::id()
        ))
        .join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("temp dir"); // APPROVED: test
    }
    std::fs::write(&path, text).expect("temp file"); // APPROVED: test
    path
}

/// Runs the binary; returns (exit code, stdout).
fn run(args: &[&str], summary: Option<&PathBuf>) -> (i32, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_coverage_prod_report"));
    command.args(args).env_remove("GITHUB_STEP_SUMMARY");
    if let Some(path) = summary {
        command.env("GITHUB_STEP_SUMMARY", path);
    }
    let output = command
        .output()
        .expect("coverage_prod_report binary must run"); // APPROVED: test
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

const LIB: &str =
    "fn prod() {\n    zero();\n}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n";

fn export_for(source: &std::path::Path) -> String {
    format!(
        r#"{{"data":[{{"files":[{{"filename":"{}","segments":[[1,1,4,true,true,false],[2,1,0,true,true,false],[3,1,0,false,false,false],[5,1,9,true,true,false],[6,1,0,true,true,false],[7,1,0,false,false,false]],"summary":{{"lines":{{"count":6,"covered":4}}}}}}]}}]}}"#,
        source.display()
    )
}

#[test]
fn test_binary_prints_the_table_and_appends_the_step_summary() {
    let source = temp_file("ok", "crates/app/src/lib.rs", LIB);
    let json = temp_file("ok", "coverage.json", &export_for(&source));
    let floors = temp_file(
        "ok",
        "floors.toml",
        "default = 63.0\n[crates]\napp = 72.6\n",
    );
    let summary = temp_file("ok", "summary.md", "before\n");
    let (code, stdout) = run(
        &[
            json.to_str().expect("utf-8 path"),
            floors.to_str().expect("utf-8 path"),
        ],
        Some(&summary),
    );
    assert_eq!(code, 0, "{stdout}");
    // lib.rs: lines 1C 2C 3U are production (2 of 3); 5C 6C 7U are the test module.
    let row = stdout
        .lines()
        .find(|line| line.starts_with("| app |"))
        .unwrap_or_default();
    assert!(row.contains("| 66.66 | 2 of 3 | 3 | 72.6 |"), "{stdout}");
    let written = std::fs::read_to_string(&summary).expect("summary readable"); // APPROVED: test
    assert!(written.starts_with("before\n"), "{written}");
    assert!(written.ends_with(&stdout), "{written}");
}

#[test]
fn test_binary_exit_codes_on_bad_input_and_no_summary_on_failure() {
    let (code, stdout) = run(&[], None);
    assert_eq!(code, 2);
    assert!(
        stdout.starts_with("usage: coverage_prod_report"),
        "{stdout}"
    );

    let summary = temp_file("bad", "summary.md", "");
    let malformed = temp_file("bad", "malformed.json", "{\"data\":[");
    let (code, stdout) = run(&[malformed.to_str().expect("utf-8 path")], Some(&summary));
    assert_eq!(code, 1);
    assert!(stdout.starts_with("coverage JSON:"), "{stdout}");

    let empty = temp_file("bad", "empty.json", "{\"data\":[{\"files\":[]}]}");
    let (code, stdout) = run(&[empty.to_str().expect("utf-8 path")], Some(&summary));
    assert_eq!(code, 1);
    assert!(stdout.contains("lists no files"), "{stdout}");

    let (code, _) = run(&["/nonexistent/coverage.json"], Some(&summary));
    assert_eq!(code, 1);
    // A failed run never writes the step summary.
    let written = std::fs::read_to_string(&summary).expect("summary readable"); // APPROVED: test
    assert!(written.is_empty(), "{written}");
}
