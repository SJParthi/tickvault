//! RULE-CITED GUARD EXISTS (2026-10-12, deep audit TRU-8).
//!
//! The always-loaded rule files cited `resilience_sla_alert_guard.rs` and
//! `operator_health_dashboard_guard.rs` as "Existing" enforcement, two hooks
//! as shipping "in the next maintenance commit", and a `make
//! wave-guarantee-check` CI gate. None of those files or targets existed, so
//! a reader trusting the rules believed a counter without an alarm would fail
//! the build when nothing checked it.
//!
//! `every_guard_cited_in_rules_exists` scans `.claude/rules/project/*.md` and
//! `CLAUDE.md` for three kinds of citation:
//!
//! - a Rust guard file name, `<name>_guard.rs`;
//! - a hook name, `<name>-guard.sh`;
//! - a backticked make command, `` `make <target>` ``.
//!
//! Each must exist (a tracked file with that base name, or a Makefile target)
//! unless the SAME line says it is not live: `NOT ENFORCED`, `NOT BUILT`,
//! `PLANNED`, `DELETED`, `deleted`, `retired`, `RETIRED`, `removed`,
//! `REMOVED`, `CORRECTED`, or a `~~strikethrough~~`.
//!
//! `KNOWN_MISSING` seeds the citations that predate this guard and sit in
//! files outside the TRU-8 scope (each with where it is cited). It is
//! shrink-only: a row whose citation is fixed, or whose file appears, fails
//! as stale and must be removed.
//!
//! HONEST LIMITS: other script names (`*.sh` that is not `-guard.sh`) and
//! guard names written without the `_guard.rs` / `-guard.sh` shape are not
//! checked; the exemption words are matched on the line, so a line that says
//! CORRECTED about something else also passes.
//!
//! ⚠ CORRECTED 2026-11-15 (audit repo-4): the scan scope was not stated. It
//! reads ONLY `.claude/rules/project/*.md` (many are summary stubs) and
//! `CLAUDE.md`. It does NOT read `docs/claude-rules-full/project/*.md`, where
//! the binding REJECT lists and ratchet tables live since 2026-09-26,
//! `.claude/rules/dhan/*.md` or `docs/error-runbooks/*.md`, so a dead citation
//! there passes. The dead ratchet citations the audit found in the
//! daily-universe full text (§18, §19, §25) were corrected by hand the same
//! day; nothing here keeps them correct.
//!
//! ⚠ CHANGED 2026-11-15 (audit repo-4 follow-up): the paragraph above is now
//! history. `SCANNED_DIRS` adds `docs/claude-rules-full/project/*.md` (the
//! binding full rule texts), `.claude/rules/dhan/*.md` and
//! `docs/error-runbooks/*.md` to `.claude/rules/project/*.md` and
//! `CLAUDE.md`, so a dead citation in any of them now fails this guard, and
//! the §18, §19 and §25 citations are held by it. Each scanned folder must
//! hold at least one `.md` file, so a moved folder fails loudly instead of
//! scanning nothing. Still not read: `docs/rules-archive/`,
//! `docs/runbooks/`, `docs/architecture/` and every other doc.
//!
//! ⚠ CHANGED 2026-11-15 (review of the repo-4 follow-up): `KNOWN_MISSING`
//! rows are `(token, file)` and excuse the token in that one file only.
//! Matched on the token alone, they had excused six dead citations in the
//! newly scanned folders (the Groww mint guard in the full minter text, the
//! cadence contract guard in the 1-minute pipeline runbook); those lines now
//! carry dated CORRECTED notes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Citations of files or targets that do not exist, found when this guard was
/// written, in files outside the TRU-8 scope. `(token, file that cites it)`,
/// the file as a path from the repo root. A row excuses the token in THAT
/// file only, so the same dead citation in any other scanned file still
/// fails (⚠ CHANGED 2026-11-15: rows were matched on the token alone, which
/// excused the token in every scanned file). Sorted. SHRINK ONLY.
///
/// Why each is dead: the cadence guards went with the cadence module
/// (2026-09-16), the Grafana guard with Grafana (2026-05-19), the Groww
/// mint guard with the Groww feed (2026-08-21); the other two were never
/// built.
const KNOWN_MISSING: &[(&str, &str)] = &[
    (
        "cadence_composition_contract_guard.rs",
        ".claude/rules/project/cadence-error-codes.md",
    ),
    (
        "cadence_executor_purity_guard.rs",
        ".claude/rules/project/cadence-error-codes.md",
    ),
    (
        "grafana_dashboard_snapshot_filter_guard.rs",
        ".claude/rules/project/audit-findings-2026-04-17.md",
    ),
    (
        "grafana_dashboard_snapshot_filter_guard.rs",
        ".claude/rules/project/gap-enforcement.md",
    ),
    (
        "grafana_dashboard_snapshot_filter_guard.rs",
        ".claude/rules/project/security-id-uniqueness.md",
    ),
    (
        "groww_no_mint_guard.rs",
        ".claude/rules/project/groww-shared-token-minter-2026-07-02.md",
    ),
    (
        "instrument_uniqueness_guard.rs",
        ".claude/rules/project/gap-enforcement.md",
    ),
    (
        "make plan-research",
        ".claude/rules/project/stream-resilience.md",
    ),
];

/// Folders whose `.md` files are scanned, beside `CLAUDE.md`. Each must hold
/// at least one `.md` file.
const SCANNED_DIRS: &[&str] = &[
    ".claude/rules/project",
    ".claude/rules/dhan",
    "docs/claude-rules-full/project",
    "docs/error-runbooks",
];

const NOT_LIVE_MARKERS: &[&str] = &[
    "NOT ENFORCED",
    "NOT BUILT",
    "PLANNED",
    "DELETED",
    "deleted",
    "retired",
    "RETIRED",
    "removed",
    "REMOVED",
    "CORRECTED",
    "~~",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("rule_cited_guard_exists_guard: cannot canonicalize repo root")
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
}

/// Every `<name><suffix>` token in `line` whose name is made of `allowed`
/// bytes, bounded by a non-name byte on the left.
fn tokens_with_suffix(line: &str, suffix: &str, allowed: fn(u8) -> bool) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = line[from..].find(suffix) {
        let end = from + rel;
        let mut start = end;
        while start > 0 && allowed(bytes[start - 1]) {
            start -= 1;
        }
        // The byte after the suffix must not continue a name.
        let after = end + suffix.len();
        let right_ok = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
        if start < end && right_ok {
            out.push(line[start..after].to_string());
        }
        from = after;
    }
    out
}

/// The citations on one line: guard files, guard hooks, `make` targets.
fn citations(line: &str) -> Vec<String> {
    let mut out = tokens_with_suffix(line, "_guard.rs", |b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'
    });
    out.extend(tokens_with_suffix(line, "-guard.sh", is_name_byte));
    let mut rest = line;
    while let Some(i) = rest.find("`make ") {
        let after = &rest[i + "`make ".len()..];
        let name: String = after
            .bytes()
            .take_while(|b| is_name_byte(*b))
            .map(char::from)
            .collect();
        if !name.is_empty() && after[name.len()..].starts_with('`') {
            out.push(format!("make {name}"));
        }
        rest = after;
    }
    out
}

fn line_says_not_live(line: &str) -> bool {
    NOT_LIVE_MARKERS.iter().any(|m| line.contains(m))
}

fn tracked_base_names(root: &Path) -> BTreeSet<String> {
    let out = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()
        .expect("git ls-files runs");
    assert!(out.status.success(), "git ls-files failed");
    out.stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let p = String::from_utf8_lossy(s).into_owned();
            p.rsplit('/').next().unwrap_or(&p).to_string()
        })
        .collect()
}

fn makefile_targets(makefile: &str) -> BTreeSet<String> {
    makefile
        .lines()
        .filter(|l| !l.starts_with('\t') && !l.starts_with('#'))
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            (!rest.starts_with('=') && !name.is_empty() && name.bytes().all(is_name_byte))
                .then(|| name.to_string())
        })
        .collect()
}

/// `(token, file:line)` for every live citation that names nothing.
fn missing_citations(
    docs: &[(String, String)],
    files: &BTreeSet<String>,
    targets: &BTreeSet<String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (path, text) in docs {
        for (i, line) in text.lines().enumerate() {
            if line_says_not_live(line) {
                continue;
            }
            for tok in citations(line) {
                let exists = match tok.strip_prefix("make ") {
                    Some(t) => targets.contains(t),
                    None => files.contains(&tok),
                };
                if !exists {
                    out.push((tok, format!("{path}:{}", i + 1)));
                }
            }
        }
    }
    out
}

/// Splits the missing citations against the `(token, file)` rows of `known`.
/// Returns the citations no row excuses (a row excuses its token in its own
/// file only) and the rows that excuse nothing (stale).
fn split_known<'a>(
    missing: &'a [(String, String)],
    known: &'a [(&'a str, &'a str)],
) -> (Vec<&'a (String, String)>, Vec<(&'a str, &'a str)>) {
    let file_of = |at: &'a str| at.rsplit_once(':').map_or(at, |(p, _)| p);
    let excused =
        |tok: &str, at: &'a str| known.iter().any(|(k, f)| *k == tok && *f == file_of(at));
    let unexpected = missing
        .iter()
        .filter(|(tok, at)| !excused(tok, at))
        .collect();
    let stale = known
        .iter()
        .copied()
        .filter(|(k, f)| {
            !missing
                .iter()
                .any(|(tok, at)| tok == k && file_of(at) == *f)
        })
        .collect();
    (unexpected, stale)
}

#[test]
fn every_guard_cited_in_rules_exists() {
    let rows: Vec<(&str, &str)> = KNOWN_MISSING.to_vec();
    let mut sorted = rows.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(rows, sorted, "KNOWN_MISSING must be sorted and unique");

    let root = repo_root();
    let mut docs = Vec::new();
    for dir in SCANNED_DIRS {
        let before = docs.len();
        let rules = root.join(dir);
        for entry in std::fs::read_dir(&rules)
            .unwrap_or_else(|e| panic!("scanned folder {dir} unreadable: {e}"))
            .flatten()
        {
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "md") {
                let text = std::fs::read_to_string(&p).expect("rule file readable");
                let rel = p.strip_prefix(&root).unwrap_or(&p).display().to_string();
                docs.push((rel, text));
            }
        }
        assert!(
            docs.len() > before,
            "scanned folder {dir} holds no .md file; was it moved?"
        );
    }
    docs.push((
        "CLAUDE.md".to_string(),
        std::fs::read_to_string(root.join("CLAUDE.md")).expect("CLAUDE.md"),
    ));
    assert!(docs.len() > 20, "only {} docs scanned", docs.len());

    let files = tracked_base_names(&root);
    let targets =
        makefile_targets(&std::fs::read_to_string(root.join("Makefile")).expect("Makefile"));
    assert!(
        targets.contains("help") && targets.len() > 50,
        "Makefile target parse is broken: {} targets",
        targets.len()
    );
    // Anti-vacuity: the scan does see real citations that exist.
    let total: usize = docs
        .iter()
        .map(|(_, t)| t.lines().map(|l| citations(l).len()).sum::<usize>())
        .sum();
    assert!(
        total > 30,
        "only {total} citations found; the scanner is broken"
    );

    let missing = missing_citations(&docs, &files, &targets);
    let (unexpected, stale) = split_known(&missing, KNOWN_MISSING);
    assert!(
        unexpected.is_empty(),
        "rules cite guards or make targets that do not exist, as if they enforce \
         something: {unexpected:?}. Write the guard, or mark the line NOT ENFORCED / \
         NOT BUILT / PLANNED / DELETED with a dated CORRECTED note."
    );
    assert!(
        stale.is_empty(),
        "STALE KNOWN_MISSING row(s) {stale:?}: the citation was fixed or the file now \
         exists. Remove the row in the same PR."
    );
}

/// The binding full rule texts, the Dhan rules and the runbooks stay in the
/// scan (2026-11-15). Removing any folder from `SCANNED_DIRS` fails here.
#[test]
fn scan_scope_covers_full_rule_texts_dhan_rules_and_runbooks() {
    for dir in [
        ".claude/rules/project",
        ".claude/rules/dhan",
        "docs/claude-rules-full/project",
        "docs/error-runbooks",
    ] {
        assert!(
            SCANNED_DIRS.contains(&dir),
            "{dir} must stay in SCANNED_DIRS"
        );
    }
}

#[test]
fn cited_guard_detector_bites() {
    let files: BTreeSet<String> = ["real_guard.rs", "real-guard.sh"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let targets: BTreeSet<String> = ["help".to_string()].into_iter().collect();
    let docs = vec![(
        "x.md".to_string(),
        [
            "| a | Existing `ghost_guard.rs` blocks this |",
            "| b | `real_guard.rs` and `real-guard.sh` |",
            "| c | `phantom-guard.sh` runs pre-PR |",
            "| d | run `make ghost-target` in CI |",
            "| e | `make help` |",
            "| f | `gone_guard.rs` NOT ENFORCED |",
            "| g | a path crates/x/tests/real_guard.rs resolves by base name |",
        ]
        .join("\n"),
    )];
    let got: Vec<String> = missing_citations(&docs, &files, &targets)
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    assert_eq!(
        got,
        vec![
            "ghost_guard.rs".to_string(),
            "phantom-guard.sh".to_string(),
            "make ghost-target".to_string(),
        ],
        "the detector must flag exactly the three fabricated citations"
    );
    assert_eq!(
        makefile_targets("a: b\n\tcmd\nX := 1\nwave-x: ## y\n.PHONY: a\n"),
        ["a", "wave-x"].iter().map(|s| (*s).to_string()).collect()
    );
}

/// A `KNOWN_MISSING` row excuses its token in its own file only (2026-11-15).
/// Before, rows matched the token alone, so a dead citation copied into any
/// other scanned file passed unflagged.
#[test]
fn split_known_excuses_a_token_only_in_its_own_file() {
    let missing = vec![
        ("old_guard.rs".to_string(), "a/known.md:10".to_string()),
        ("old_guard.rs".to_string(), "b/other.md:3".to_string()),
        ("new_guard.rs".to_string(), "a/known.md:11".to_string()),
    ];
    let known = [
        ("old_guard.rs", "a/known.md"),
        ("old_guard.rs", "z/gone.md"),
    ];
    let (unexpected, stale) = split_known(&missing, &known);
    let flagged: Vec<(&str, &str)> = unexpected
        .iter()
        .map(|(t, at)| (t.as_str(), at.as_str()))
        .collect();
    assert_eq!(
        flagged,
        vec![
            ("old_guard.rs", "b/other.md:3"),
            ("new_guard.rs", "a/known.md:11"),
        ],
        "a known token cited in another file must still be flagged"
    );
    assert_eq!(
        stale,
        vec![("old_guard.rs", "z/gone.md")],
        "a row whose file no longer cites the token is stale"
    );
}
