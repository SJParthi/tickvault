//! Subprocess tests for the three helper binaries in `src/bin/`:
//! `tv-guarantees`, `tv_doctor` and `smoke_test`.
//!
//! Each one is a `main()` with its logic inline, so no unit test in the
//! library can reach it. These tests run the compiled binary
//! (`CARGO_BIN_EXE_*`) the way an operator or CI does and assert what it
//! prints and how it exits. Under `cargo llvm-cov` the child inherits
//! `LLVM_PROFILE_FILE`, so the lines it runs count towards the app crate.
//!
//! Every test that needs a working directory makes its own under the system
//! temp dir and removes it at the end; nothing is written inside the repo.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

const GUARANTEES: &str = env!("CARGO_BIN_EXE_tv-guarantees");
const DOCTOR: &str = env!("CARGO_BIN_EXE_tv_doctor");
const SMOKE: &str = env!("CARGO_BIN_EXE_smoke_test");

/// The four verdict labels exactly as the text report prints them.
const LABELS: [&str; 4] = ["GUARANTEED", "BOUNDED", "IMPOSSIBLE", "** BROKEN **"];

/// The eight section titles of the text report, in order.
const TEXT_SECTIONS: [&str; 8] = [
    "1. ONE LANGUAGE",
    "2. O(1)",
    "3. AUTOMATION",
    "4. TEST SURFACE",
    "5. EVERY VERSION PINNED",
    "6. DATA — STORED, DEDUPED, SEARCHABLE, WATCHED",
    "7. COMMON RUNTIME, DYNAMIC, SCALABLE, INCREMENTAL",
    "8. EXTREME PERMUTATIONS, COMBINATIONS, OUT-OF-BOX",
];

/// The same eight sections as the `--html` page titles them.
const HTML_SECTIONS: [&str; 8] = [
    "1. One language",
    "2. O(1)",
    "3. Automation",
    "4. Test surface",
    "5. Every version pinned",
    "6. Data — stored, deduped, searchable, watched",
    "7. Common runtime, dynamic, scalable, incremental",
    "8. Extreme permutations, combinations, out-of-box",
];

/// A fresh, empty directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "tv-bin-test-{tag}-{}-{n}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn run(bin: &str, args: &[&str], cwd: &Path) -> Output {
    Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("spawn {bin}: {e}"))
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// A git repository with exactly the given files tracked in its index.
fn git_repo_with(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new("git");
    let init = Command::new("git")
        .args(["init", "-q", "."])
        .current_dir(dir.path())
        .output()
        .expect("git must be installed to run these tests");
    assert!(init.status.success(), "git init failed: {}", stderr(&init));
    for (name, body) in files {
        std::fs::write(dir.path().join(name), body).expect("write file");
        let add = Command::new("git")
            .args(["add", name])
            .current_dir(dir.path())
            .output()
            .expect("git add");
        assert!(add.status.success(), "git add failed: {}", stderr(&add));
    }
    dir
}

// ---------------------------------------------------------------------------
// tv-guarantees: parsing helpers
// ---------------------------------------------------------------------------

/// Counts as printed on the SUMMARY line:
/// `  N guaranteed   N bounded   N impossible   N broken`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct Summary {
    guaranteed: usize,
    bounded: usize,
    impossible: usize,
    broken: usize,
}

fn parse_summary(text: &str) -> Summary {
    let line = text
        .lines()
        .find(|l| l.contains(" guaranteed ") && l.contains(" broken"))
        .unwrap_or_else(|| panic!("no SUMMARY counts line in:\n{text}"));
    let nums: Vec<usize> = line
        .split_whitespace()
        .filter_map(|w| w.parse().ok())
        .collect();
    assert_eq!(nums.len(), 4, "summary line must carry 4 counts: {line:?}");
    Summary {
        guaranteed: nums[0],
        bounded: nums[1],
        impossible: nums[2],
        broken: nums[3],
    }
}

/// The verdict of every data row in the text report, counted by label.
///
/// A data row is any line that carries a label padded by two spaces on each
/// side (the column separator). The earliest label on the line wins, so a
/// proof text that happens to contain a label word cannot double-count.
fn count_row_verdicts(text: &str) -> Summary {
    let mut s = Summary {
        guaranteed: 0,
        bounded: 0,
        impossible: 0,
        broken: 0,
    };
    for line in text.lines() {
        let hit = LABELS
            .iter()
            .enumerate()
            .filter_map(|(i, l)| line.find(&format!("  {l}  ")).map(|pos| (pos, i)))
            .min();
        match hit {
            Some((_, 0)) => s.guaranteed += 1,
            Some((_, 1)) => s.bounded += 1,
            Some((_, 2)) => s.impossible += 1,
            Some((_, 3)) => s.broken += 1,
            _ => {}
        }
    }
    s
}

/// The row whose WHAT column starts with `what`, as printed.
fn row<'a>(text: &'a str, what: &str) -> &'a str {
    text.lines()
        .find(|l| l.starts_with(what))
        .unwrap_or_else(|| panic!("no row starting {what:?}"))
}

/// The four tile numbers on the HTML page, in page order
/// (guaranteed, bounded, impossible, broken).
fn html_tiles(html: &str) -> Summary {
    let nums: Vec<usize> = html
        .split("<div class=\"n\">")
        .skip(1)
        .filter_map(|rest| rest.split("</div>").next())
        .filter_map(|n| n.parse().ok())
        .collect();
    assert_eq!(nums.len(), 4, "the page must carry exactly 4 tiles");
    Summary {
        guaranteed: nums[0],
        bounded: nums[1],
        impossible: nums[2],
        broken: nums[3],
    }
}

fn html_chips(html: &str) -> Summary {
    Summary {
        guaranteed: html.matches("chip c-g\">Guaranteed<").count(),
        bounded: html.matches("chip c-b\">Bounded<").count(),
        impossible: html.matches("chip c-i\">Impossible<").count(),
        broken: html.matches("chip c-x\">Broken<").count(),
    }
}

// ---------------------------------------------------------------------------
// tv-guarantees: the real working tree
// ---------------------------------------------------------------------------

#[test]
fn guarantees_text_report_on_the_real_tree_holds_and_adds_up() {
    let out = run(GUARANTEES, &[], &workspace_root());
    let text = stdout(&out);
    assert!(
        out.status.success(),
        "tv-guarantees must exit 0 on main's tree (CI runs it as a gate):\n{text}"
    );
    assert!(text.starts_with("TICKVAULT GUARANTEE REPORT\n"));

    // All eight sections, in order, each underlined to its own byte length.
    let mut from = 0;
    for title in TEXT_SECTIONS {
        let at = text[from..]
            .find(&format!("\n{title}\n"))
            .unwrap_or_else(|| panic!("section {title:?} missing or out of order"));
        from += at + 1;
        let underline = text[from..].lines().nth(1).expect("underline line");
        assert_eq!(underline, "=".repeat(title.len()), "underline of {title:?}");
    }

    // Every row is one of the three non-broken verdicts, and the rows add up
    // to the SUMMARY line exactly.
    let summary = parse_summary(&text);
    let rows = count_row_verdicts(&text);
    assert_eq!(rows, summary, "row verdicts must add up to the summary");
    assert_eq!(summary.broken, 0);
    assert!(
        summary.guaranteed > 0,
        "a report with no guarantees is empty"
    );
    assert!(
        summary.impossible >= 1,
        "the IMPOSSIBLE class must never be dropped from the report"
    );
    assert!(text.contains("\n  Every guaranteed row holds.\n"));
    assert!(!text.contains("exiting 1"));

    // A measured row really is measured: the Rust source count is the number
    // of tracked `.rs` files, which this test can see is well above zero.
    let rust = row(&text, "Rust sources");
    assert!(rust.contains("GUARANTEED"), "{rust}");
    let n: usize = rust
        .split_whitespace()
        .find_map(|w| w.parse().ok())
        .expect("a file count");
    assert!(n > 100, "Rust sources measured {n}");
}

#[test]
fn guarantees_html_page_on_the_real_tree_matches_the_text_report() {
    let root = workspace_root();
    let text_out = run(GUARANTEES, &[], &root);
    let html_out = run(GUARANTEES, &["--html"], &root);
    assert!(
        html_out.status.success(),
        "--html must exit 0 on main's tree"
    );
    let html = stdout(&html_out);
    assert!(html.starts_with("<title>TickVault Guarantee Ledger</title>"));
    assert!(html.trim_end().ends_with("</footer></div>"));

    // Same eight sections, as <h2>, in order.
    let mut from = 0;
    for title in HTML_SECTIONS {
        let tag = format!("<h2>{title}</h2>");
        let at = html[from..]
            .find(&tag)
            .unwrap_or_else(|| panic!("{tag} missing or out of order"));
        from += at + tag.len();
    }

    // One measurement pass, two renderings: tiles, chips and the text
    // summary must all agree.
    let text_summary = parse_summary(&stdout(&text_out));
    assert_eq!(html_tiles(&html), text_summary, "tiles vs text summary");
    assert_eq!(html_chips(&html), text_summary, "chips vs text summary");

    // Measured text is escaped: the live-ingest row's `<=256` must not
    // reach the page as a raw `<`, which would swallow the rest of the row.
    assert!(
        html.contains("&lt;=256"),
        "the `<=` in a cell must be escaped"
    );
    assert!(!html.contains("<=256"));
}

// ---------------------------------------------------------------------------
// tv-guarantees: outside a repository, and in a tree that breaks rows
// ---------------------------------------------------------------------------

#[test]
fn guarantees_outside_a_git_repository_exits_1_with_a_reason() {
    let dir = TempDir::new("nogit");
    for args in [&[][..], &["--html"][..]] {
        let out = run(GUARANTEES, args, dir.path());
        assert_eq!(out.status.code(), Some(1), "args {args:?}");
        assert_eq!(
            stdout(&out),
            "tv-guarantees: could not read the git index — run inside the repository.\n",
            "args {args:?}"
        );
    }
}

#[test]
fn guarantees_in_an_empty_git_repository_exits_1() {
    // `git ls-files` succeeds but lists nothing: still no tree to measure.
    let dir = git_repo_with(&[]);
    let out = run(GUARANTEES, &[], dir.path());
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("could not read the git index"));
}

#[test]
fn guarantees_in_a_tree_with_a_tracked_interpreted_file_reports_it_broken() {
    let dir = git_repo_with(&[("a.rs", "fn main() {}\n"), ("run.py", "print(1)\n")]);
    let out = run(GUARANTEES, &[], dir.path());
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "a broken row must fail the run");

    let interpreted = row(&text, "Interpreted runtimes tracked");
    assert!(interpreted.contains("** BROKEN **"), "{interpreted}");
    assert!(interpreted.contains("1 files"), "{interpreted}");
    let rust = row(&text, "Rust sources");
    assert!(rust.contains("1 files"), "{rust}");

    let summary = parse_summary(&text);
    assert_eq!(count_row_verdicts(&text), summary);
    assert!(summary.broken >= 1);
    assert!(
        text.contains(&format!(
            "\n  {} GUARANTEED row(s) BROKEN -- exiting 1.\n",
            summary.broken
        )),
        "the exit line must name the broken count"
    );
    assert!(!text.contains("Every guaranteed row holds."));

    // The page reports the same breakage and fails the same way.
    let html_out = run(GUARANTEES, &["--html"], dir.path());
    assert_eq!(html_out.status.code(), Some(1));
    let html = stdout(&html_out);
    assert_eq!(html_tiles(&html), summary);
    assert_eq!(html_chips(&html).broken, summary.broken);
}

#[test]
fn guarantees_interpreted_row_holds_when_only_rust_is_tracked() {
    // The same tiny tree without the `.py` file: the language row goes back
    // to GUARANTEED with 0 files, so the row measures and does not just
    // print a constant.
    let dir = git_repo_with(&[("a.rs", "fn main() {}\n")]);
    let text = stdout(&run(GUARANTEES, &[], dir.path()));
    let interpreted = row(&text, "Interpreted runtimes tracked");
    assert!(interpreted.contains("GUARANTEED"), "{interpreted}");
    assert!(interpreted.contains("0 files"), "{interpreted}");
}

// ---------------------------------------------------------------------------
// tv_doctor
// ---------------------------------------------------------------------------

/// Run `tv_doctor` with no tool on PATH, so every probe takes its
/// not-available fallback and the output is fully deterministic.
fn doctor_without_tools(args: &[&str]) -> Output {
    let empty = TempDir::new("doctor-path");
    Command::new(DOCTOR)
        .args(args)
        .env("PATH", empty.path())
        .output()
        .expect("spawn tv_doctor")
}

const DOCTOR_FALLBACKS: [&str; 5] = [
    "systemctl-not-available",
    "docker-not-available",
    "df-not-available-or-path-missing",
    "metrics-unreachable",
    "journalctl-not-available",
];

#[test]
fn doctor_json_is_valid_and_carries_every_fallback() {
    for args in [&["--format=json"][..], &["--format", "json"][..]] {
        let out = doctor_without_tools(args);
        assert!(out.status.success(), "args {args:?}");
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("args {args:?}: invalid JSON: {e}\n{}", stdout(&out)));
        let obj = v.as_object().expect("a JSON object");
        assert_eq!(obj.len(), 6, "exactly six fields");
        assert_eq!(v["systemd_state"], "systemctl-not-available");
        assert_eq!(v["docker_state"], "docker-not-available");
        assert_eq!(v["disk_usage"], "df-not-available-or-path-missing");
        assert_eq!(v["metrics_snapshot"], "metrics-unreachable");
        assert_eq!(v["journal_tail"], "journalctl-not-available");

        let secs: u64 = v["generated_at"]
            .as_str()
            .and_then(|s| s.strip_prefix("unix_secs="))
            .and_then(|s| s.parse().ok())
            .expect("generated_at is unix_secs=<n>");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        assert!(secs <= now && now - secs < 300, "stamp {secs} vs now {now}");
    }
}

#[test]
fn doctor_markdown_is_the_default_and_has_five_fenced_sections() {
    let out = doctor_without_tools(&["--journal-lines", "5"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.starts_with("## tv-doctor report\n"));
    let headings: Vec<&str> = text.lines().filter(|l| l.starts_with("### ")).collect();
    assert_eq!(
        headings,
        [
            "### 1. systemd state",
            "### 2. Docker services",
            "### 3. Spill disk usage",
            "### 4. Metrics snapshot (tv_*)",
            "### 5. Recent journal (last lines)",
        ]
    );
    assert_eq!(text.matches("```").count(), 10, "five fenced blocks");
    for f in DOCTOR_FALLBACKS {
        assert!(text.contains(&format!("```\n{f}\n```")), "{f} fenced");
    }
}

#[test]
fn doctor_unknown_format_falls_back_to_plain() {
    // An unrecognised format and a malformed journal count both fall back
    // to defaults rather than failing.
    let out = doctor_without_tools(&["--format=yaml", "--journal-lines=abc"]);
    assert!(out.status.success());
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("tv-doctor report (unix_secs="));
    assert_eq!(lines[1], "---");
    assert_eq!(lines[2], "systemd: systemctl-not-available");
    assert_eq!(lines[3], "docker:  docker-not-available");
    assert_eq!(lines[4], "disk:    df-not-available-or-path-missing");
    assert_eq!(lines[6], "metrics:");
    assert_eq!(lines[7], "metrics-unreachable");
    assert_eq!(lines[9], "journal tail:");
    assert_eq!(lines[10], "journalctl-not-available");
    assert!(!text.contains("```"), "plain output has no fences");
    assert!(
        !text.trim_start().starts_with('{'),
        "plain output is not JSON"
    );
}

#[test]
fn doctor_flag_without_a_value_uses_the_default_format() {
    // `--format` as the last argument has no value: markdown, not a crash.
    let out = doctor_without_tools(&["--format"]);
    assert!(out.status.success());
    assert!(stdout(&out).starts_with("## tv-doctor report\n"));
}

// ---------------------------------------------------------------------------
// smoke_test
// ---------------------------------------------------------------------------

fn base_toml() -> String {
    std::fs::read_to_string(workspace_root().join("config/base.toml")).expect("base.toml")
}

/// A temp working dir holding `config/base.toml` with the given body.
fn smoke_dir(body: &str) -> TempDir {
    let dir = TempDir::new("smoke");
    std::fs::create_dir_all(dir.path().join("config")).expect("mkdir config");
    std::fs::write(dir.path().join("config/base.toml"), body).expect("write config");
    dir
}

#[test]
fn smoke_passes_on_the_shipped_config_and_leaves_no_probe() {
    let dir = smoke_dir(&base_toml());
    for args in [&[][..], &["--duration", "5"][..], &["--duration=7"][..]] {
        let out = run(SMOKE, args, dir.path());
        let err = stderr(&out);
        assert!(out.status.success(), "args {args:?}:\n{err}");
        let budget = match args {
            [] => 30,
            [_, v] => v.parse().expect("number"),
            [a] => a.trim_start_matches("--duration=").parse().expect("number"),
            _ => unreachable!(),
        };
        assert!(
            err.contains(&format!("starting (budget {budget}s)")),
            "args {args:?}: {err}"
        );
        for step in 1..=5 {
            assert!(err.contains(&format!("[{step}/5]")), "step {step} missing");
        }
        assert!(err.contains("binary is safe to start"));
    }
    // The spill dir was created and the write probe was removed again.
    let spill = dir.path().join("data/spill");
    assert!(spill.is_dir());
    assert_eq!(std::fs::read_dir(&spill).expect("read spill").count(), 0);
}

#[test]
fn smoke_bad_duration_falls_back_to_the_default_budget() {
    let dir = smoke_dir(&base_toml());
    let out = run(SMOKE, &["--duration", "soon"], dir.path());
    assert!(out.status.success());
    assert!(stderr(&out).contains("starting (budget 30s)"));
}

#[test]
fn smoke_fails_without_a_config_file() {
    let dir = TempDir::new("smoke-empty");
    let out = run(SMOKE, &[], dir.path());
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("SMOKE FAIL: config/base.toml not found"),
        "{err}"
    );
    assert!(!err.contains("[1/5]"), "must stop before step 1 reports");
}

#[test]
fn smoke_fails_on_unparseable_config() {
    let dir = smoke_dir("garbage = [\n");
    let out = run(SMOKE, &[], dir.path());
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("[1/5] config file exists"), "{err}");
    assert!(err.contains("SMOKE FAIL: config parse failed"), "{err}");
    assert!(!err.contains("[2/5]"), "must stop before step 2 reports");
}

#[test]
fn smoke_fails_validation_for_live_mode_inside_the_sandbox_window() {
    // Flip only the [strategy] mode to live; the shipped cutoff is far in
    // the future, so today is inside the sandbox-only window. Config
    // validation refuses it first, so the run stops at step 2 and never
    // reaches the step-3 window check.
    let body = base_toml();
    let start = body.find("[strategy]").expect("[strategy] section");
    let rel = body[start..]
        .find("mode = \"sandbox\"")
        .expect("strategy mode line");
    let mut live = body.clone();
    live.replace_range(
        start + rel..start + rel + "mode = \"sandbox\"".len(),
        "mode = \"live\"",
    );
    let dir = smoke_dir(&live);
    let out = run(SMOKE, &[], dir.path());
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("[1/5] config file exists"), "{err}");
    assert!(
        err.contains("SMOKE FAIL: config validation failed"),
        "{err}"
    );
    assert!(err.contains("live trading mode is locked until"), "{err}");
    assert!(!err.contains("[2/5]"), "must stop before step 2 reports");
}

#[test]
fn smoke_fails_when_the_spill_path_is_not_a_directory() {
    let dir = smoke_dir(&base_toml());
    std::fs::create_dir_all(dir.path().join("data")).expect("mkdir data");
    // A FILE where the spill dir should be: it exists, so the binary does
    // not create it, and the probe write into it must fail loudly.
    std::fs::write(dir.path().join("data/spill"), b"not a dir").expect("write file");
    let out = run(SMOKE, &[], dir.path());
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("[3/5]"), "{err}");
    assert!(err.contains("SMOKE FAIL: spill dir not writable"), "{err}");
    assert!(!err.contains("[4/5]"));
}
