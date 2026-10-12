//! Per-FILE coverage floors (2026-10-12, deep audit PROOF-12).
//!
//! `scripts/coverage-gate.sh` enforces per-CRATE floors only, and it is
//! budget-pinned (shell may only shrink), so it cannot grow a per-file check.
//! The `app` crate floor is 72.6%, and app holds the largest files in the
//! workspace (`main.rs`, `dhan_feed_stack.rs`), so one of them could lose most
//! of its tests while the crate average still passes.
//!
//! Usage: `coverage_file_floors <coverage.json> <crate-coverage-thresholds.toml>`
//!
//! - Reads the `[files]` table of the thresholds file: repo-relative path to
//!   a line-coverage floor, e.g. `"crates/app/src/main.rs" = 70.0`.
//! - Exits 1 when a listed file is below its floor, or is MISSING from the
//!   report (so a renamed file cannot pass vacuously).
//! - Always prints the 15 largest files with their coverage, so floors can be
//!   seeded from a real CI run (measured, rounded down to one decimal, minus
//!   0.3, as the thresholds file prescribes).
//!
//! HONEST LIMIT: the `[files]` table ships EMPTY. No CI coverage report was
//! available when this was written, and a floor guessed from a local run is
//! exactly what the thresholds file warns against. Until floors are seeded
//! this is report-only and gates nothing. It is also NOT run in CI yet: the
//! Coverage & Perf job would build common a second time, uninstrumented, in a
//! job that has run out of runner disk before (see the note in `ci.yml`).

#![cfg_attr(not(test), deny(unsafe_code))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]

use std::collections::BTreeMap;

/// How many of the largest files the report lists.
const REPORT_LARGEST: usize = 15;

/// One file's line coverage from an llvm-cov JSON export.
#[derive(Debug, Clone, PartialEq)]
struct FileCoverage {
    filename: String,
    count: u64,
    covered: u64,
}

impl FileCoverage {
    fn percent(&self) -> f64 {
        if self.count == 0 {
            100.0
        } else {
            // Line counts are far below 2^53, so the conversion is exact.
            self.covered as f64 * 100.0 / self.count as f64
        }
    }

    /// Whether the file meets a floor given in hundredths of a percent, by
    /// exact integer cross-multiplication (the same rule as
    /// `scripts/coverage-gate.sh`), so a file exactly at its floor passes.
    fn meets(&self, floor_centi: u64) -> bool {
        u128::from(self.covered) * 10_000 >= u128::from(floor_centi) * u128::from(self.count)
    }
}

/// A floor as written, kept with its exact value in hundredths of a percent.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Floor {
    centi: u64,
}

impl std::fmt::Display for Floor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{:02}", self.centi / 100, self.centi % 100)
    }
}

/// Converts a TOML floor to hundredths, refusing anything outside 0..=100
/// or with more than two decimals (which an integer compare cannot hold).
fn floor_from(path: &str, value: f64) -> Result<Floor, String> {
    let scaled = value * 100.0;
    let rounded = scaled.round();
    if !(0.0..=10_000.0).contains(&rounded) || (scaled - rounded).abs() > 1e-6 {
        return Err(format!(
            "floor for {path} must be 0 to 100 with at most two decimals, got {value}"
        ));
    }
    // In range and integral, so the cast is exact.
    Ok(Floor {
        centi: rounded as u64,
    })
}

/// Parses the `data[].files[]` entries of an llvm-cov JSON export.
fn parse_report(json: &str) -> Result<Vec<FileCoverage>, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("coverage JSON does not parse: {e}"))?;
    let mut out = Vec::new();
    for block in v["data"]
        .as_array()
        .ok_or("coverage JSON has no data array")?
    {
        for f in block["files"].as_array().into_iter().flatten() {
            let lines = &f["summary"]["lines"];
            out.push(FileCoverage {
                filename: f["filename"].as_str().unwrap_or_default().to_string(),
                count: lines["count"].as_u64().ok_or("a file has no lines.count")?,
                covered: lines["covered"]
                    .as_u64()
                    .ok_or("a file has no lines.covered")?,
            });
        }
    }
    Ok(out)
}

/// Parses the `[files]` table of the thresholds file (absent table = empty).
fn parse_floors(toml_text: &str) -> Result<BTreeMap<String, Floor>, String> {
    let v: toml::Value =
        toml::from_str(toml_text).map_err(|e| format!("thresholds TOML does not parse: {e}"))?;
    let mut out = BTreeMap::new();
    if let Some(files) = v.get("files") {
        let table = files.as_table().ok_or("[files] is not a table")?;
        for (path, floor) in table {
            let f = floor
                .as_float()
                .or_else(|| floor.as_integer().map(|i| i as f64))
                .ok_or_else(|| format!("floor for {path} is not a number"))?;
            out.insert(path.clone(), floor_from(path, f)?);
        }
    }
    Ok(out)
}

/// The report entry for a repo-relative `path` (llvm-cov writes absolute
/// file names, so it matches on a `/`-bounded suffix).
fn find<'a>(report: &'a [FileCoverage], path: &str) -> Option<&'a FileCoverage> {
    let suffix = format!("/{path}");
    report
        .iter()
        .find(|f| f.filename == path || f.filename.ends_with(&suffix))
}

/// Builds the report text and the verdict (`true` = every floor holds).
fn evaluate(report: &[FileCoverage], floors: &BTreeMap<String, Floor>) -> (String, bool) {
    let mut text = String::new();
    let mut ok = true;
    let mut largest: Vec<&FileCoverage> = report.iter().collect();
    largest.sort_by(|a, b| b.count.cmp(&a.count).then(a.filename.cmp(&b.filename)));
    text.push_str(&format!("Largest {REPORT_LARGEST} files by line count:\n"));
    for f in largest.iter().take(REPORT_LARGEST) {
        text.push_str(&format!(
            "  {:>7.2}%  {:>6} lines  {}\n",
            f.percent(),
            f.count,
            f.filename
        ));
    }
    if floors.is_empty() {
        text.push_str("No [files] floors set: report only.\n");
    }
    for (path, floor) in floors {
        match find(report, path) {
            None => {
                ok = false;
                text.push_str(&format!(
                    "MISSING: {path} is listed but not in the report\n"
                ));
            }
            Some(f) if !f.meets(floor.centi) => {
                ok = false;
                text.push_str(&format!(
                    "BELOW FLOOR: {path} {:.2}% < {floor}\n",
                    f.percent()
                ));
            }
            Some(f) => text.push_str(&format!("ok: {path} {:.2}% >= {floor}\n", f.percent())),
        }
    }
    (text, ok)
}

/// Reads both files and evaluates them.
fn run(coverage_path: &str, thresholds_path: &str) -> Result<(String, bool), String> {
    let json = std::fs::read_to_string(coverage_path)
        .map_err(|e| format!("cannot read {coverage_path}: {e}"))?;
    let toml_text = std::fs::read_to_string(thresholds_path)
        .map_err(|e| format!("cannot read {thresholds_path}: {e}"))?;
    let report = parse_report(&json)?;
    if report.is_empty() {
        return Err("coverage JSON lists no files".to_string());
    }
    Ok(evaluate(&report, &parse_floors(&toml_text)?))
}

/// The text to print and the exit code for one command line: 0 every floor
/// holds, 1 a floor failed, 2 bad usage or unreadable input.
fn outcome(args: &[String]) -> (String, i32) {
    match (args.get(1), args.get(2)) {
        (Some(c), Some(t)) => match run(c, t) {
            Ok((text, ok)) => (text, i32::from(!ok)),
            Err(e) => (format!("coverage_file_floors: {e}\n"), 2),
        },
        _ => (
            "usage: coverage_file_floors <coverage.json> <thresholds.toml>\n".to_string(),
            2,
        ),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (text, code) = outcome(&args);
    eprint!("{text}");
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_json() -> &'static str {
        r#"{"data":[{"files":[
            {"filename":"/w/crates/app/src/main.rs","summary":{"lines":{"count":200,"covered":150}}},
            {"filename":"/w/crates/app/src/dhan_feed_stack.rs","summary":{"lines":{"count":1000,"covered":900}}},
            {"filename":"/w/crates/core/src/empty.rs","summary":{"lines":{"count":0,"covered":0}}}
        ]}]}"#
    }

    fn eval(toml_text: &str) -> (String, bool) {
        let report = parse_report(report_json()).unwrap();
        evaluate(&report, &parse_floors(toml_text).unwrap())
    }

    #[test]
    fn file_below_floor_fails() {
        let (text, ok) = eval("[files]\n\"crates/app/src/main.rs\" = 80.0\n");
        assert!(!ok);
        assert!(text.contains("BELOW FLOOR: crates/app/src/main.rs 75.00%"));
    }

    #[test]
    fn listed_file_missing_from_report_fails() {
        let (text, ok) = eval("[files]\n\"crates/app/src/gone.rs\" = 1.0\n");
        assert!(!ok);
        assert!(text.contains("MISSING: crates/app/src/gone.rs"));
    }

    #[test]
    fn all_files_at_or_above_floor_pass() {
        let (text, ok) = eval(
            "[files]\n\"crates/app/src/main.rs\" = 75\n\"crates/app/src/dhan_feed_stack.rs\" = 89.9\n",
        );
        assert!(ok, "{text}");
        assert!(text.contains("ok: crates/app/src/main.rs 75.00% >= 75"));
    }

    #[test]
    fn empty_files_table_passes_and_reports() {
        let (text, ok) = eval("default = 63.0\n[crates]\napp = 72.6\n[files]\n");
        assert!(ok);
        assert!(text.contains("report only"));
        // Largest first.
        let a = text.find("dhan_feed_stack.rs").unwrap();
        let b = text.find("main.rs").unwrap();
        assert!(a < b, "{text}");
        assert!(
            text.contains("100.00%"),
            "an empty file reads as fully covered"
        );
    }

    #[test]
    fn suffix_match_is_bounded_by_a_slash() {
        let report = parse_report(report_json()).unwrap();
        assert!(find(&report, "app/src/main.rs").is_some());
        assert!(find(&report, "pp/src/main.rs").is_none());
    }

    #[test]
    fn bad_inputs_are_errors_not_passes() {
        assert!(parse_report("not json").is_err());
        assert!(parse_report("{}").is_err());
        assert!(
            parse_report(r#"{"data":[{"files":[{"filename":"x","summary":{"lines":{}}}]}]}"#)
                .is_err()
        );
        assert!(parse_floors("[files]\n\"a.rs\" = \"high\"\n").is_err());
        assert!(parse_floors("files = 3\n").is_err());
    }

    #[test]
    fn a_file_exactly_at_its_floor_passes_and_one_line_short_fails() {
        // 150 of 200 is exactly 75.00%; 1000 lines at 90.00% is the edge.
        let (text, ok) = eval(
            "[files]\n\"crates/app/src/main.rs\" = 75.00\n\"crates/app/src/dhan_feed_stack.rs\" = 90.0\n",
        );
        assert!(ok, "{text}");
        let (text, ok) = eval("[files]\n\"crates/app/src/dhan_feed_stack.rs\" = 90.01\n");
        assert!(!ok, "{text}");
        assert!(text.contains("< 90.01"), "{text}");
    }

    #[test]
    fn a_floor_out_of_range_or_finer_than_a_hundredth_is_refused() {
        assert!(parse_floors("[files]\n\"a.rs\" = 100.5\n").is_err());
        assert!(parse_floors("[files]\n\"a.rs\" = -1\n").is_err());
        assert!(parse_floors("[files]\n\"a.rs\" = 72.655\n").is_err());
        let ok = parse_floors("[files]\n\"a.rs\" = 72.65\n").unwrap();
        assert_eq!(ok["a.rs"], Floor { centi: 7265 });
        assert_eq!(ok["a.rs"].to_string(), "72.65");
    }

    #[test]
    fn outcome_maps_every_result_to_its_exit_code() {
        let dir = std::env::temp_dir().join(format!("cov-floors-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cov = dir.join("coverage.json");
        let thr = dir.join("t.toml");
        std::fs::write(&cov, report_json()).unwrap();
        let args = |t: &str| {
            std::fs::write(&thr, t).unwrap();
            vec![
                "bin".to_string(),
                cov.display().to_string(),
                thr.display().to_string(),
            ]
        };
        assert_eq!(outcome(&args("[files]\n")).1, 0);
        assert_eq!(
            outcome(&args("[files]\n\"crates/app/src/main.rs\" = 99\n")).1,
            1
        );
        assert_eq!(outcome(&args("[files]\n\"a.rs\" = \"x\"\n")).1, 2);
        let (text, code) = outcome(&["bin".to_string()]);
        assert_eq!(code, 2);
        assert!(text.starts_with("usage:"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_reads_files_and_rejects_an_empty_report() {
        let dir = std::env::temp_dir().join(format!("cov-floors-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cov = dir.join("coverage.json");
        let thr = dir.join("t.toml");
        std::fs::write(&cov, report_json()).unwrap();
        std::fs::write(&thr, "[files]\n\"crates/app/src/main.rs\" = 70.0\n").unwrap();
        let (_, ok) = run(cov.to_str().unwrap(), thr.to_str().unwrap()).unwrap();
        assert!(ok);
        std::fs::write(&cov, r#"{"data":[{"files":[]}]}"#).unwrap();
        assert!(run(cov.to_str().unwrap(), thr.to_str().unwrap()).is_err());
        assert!(run("/nonexistent/c.json", thr.to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
