//! Ratchet guard for the B7 coverage-honesty sweep (2026-07-03).
//!
//! The executable truth is `quality/crate-coverage-thresholds.toml` — ratcheted
//! per-crate LINE-coverage floors (app 72.6 … common 99.4; floors only move up,
//! 100% is the target), enforced post-merge by `scripts/coverage-gate.sh`. The
//! B7 directive replaced every FALSE "100% coverage enforced" documentation
//! claim with that honest wording, aligned `make coverage` to the SAME per-crate
//! gate as CI, and gave the QuestDB-backed Groww e2e a CI lane that can never
//! vacuously pass (audit-findings Rule 11: no false-OK signals).
//!
//! These source-scan tests pin all of that so the false claims cannot silently
//! return.

use std::path::PathBuf;

/// Repo root = crates/storage/../../
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {} (B7 honesty guard): {e}", path.display()))
}

/// ci.yml must describe the coverage gate as the ratcheted per-crate floors —
/// never the false "per-crate 100% thresholds" (B7 coverage honesty).
#[test]
fn coverage_claims_in_ci_yml_are_honest() {
    let ci = read(".github/workflows/ci.yml");
    assert!(
        !ci.contains("per-crate 100% thresholds"),
        "ci.yml regressed to the FALSE 'per-crate 100% thresholds' claim — the real \
         gate enforces ratcheted per-crate floors (B7 coverage honesty / audit Rule 11)"
    );
    assert!(
        ci.contains("ratcheted per-crate floors"),
        "ci.yml coverage-and-perf comment must describe the gate honestly as \
         'ratcheted per-crate floors' (B7 coverage honesty)"
    );
}

/// CLAUDE.md must not claim a 100% minimum for all crates — the enforced
/// floors are 72.6–99.4 (target 100%).
#[test]
fn claude_md_does_not_claim_100pct_minimum_for_all_crates() {
    let claude_md = read("CLAUDE.md");
    assert!(
        !claude_md.contains("100% minimum for ALL crates"),
        "CLAUDE.md regressed to the FALSE '100% minimum for ALL crates' coverage \
         claim — the real floors are ratcheted per crate in \
         quality/crate-coverage-thresholds.toml (B7 coverage honesty / audit Rule 11)"
    );
}

/// guarantees.md must state the ratcheted floors and drop the false
/// "100% line coverage required per crate" claim.
#[test]
fn guarantees_doc_states_ratcheted_floors() {
    let doc = read("docs/architecture/guarantees.md");
    assert!(
        !doc.contains("100% line coverage required per crate"),
        "docs/architecture/guarantees.md regressed to the FALSE '100% line coverage \
         required per crate' claim (B7 coverage honesty / audit Rule 11)"
    );
    assert!(
        doc.to_lowercase().contains("ratcheted"),
        "docs/architecture/guarantees.md must describe the coverage gate as \
         RATCHETED per-crate floors (B7 coverage honesty)"
    );
}

/// `make coverage` must run the SAME per-crate gate as CI — never a flat
/// `--fail-under-lines` that contradicts the TOML floors.
#[test]
fn makefile_coverage_target_uses_the_real_per_crate_gate() {
    let makefile = read("Makefile");
    assert!(
        !makefile.contains("--fail-under-lines"),
        "Makefile regressed to a flat `--fail-under-lines` coverage gate — \
         `make coverage` must enforce the SAME per-crate TOML floors as CI via \
         scripts/coverage-gate.sh (B7 coverage honesty)"
    );
    assert!(
        makefile.contains("scripts/coverage-gate.sh"),
        "Makefile `coverage` target must invoke scripts/coverage-gate.sh so local \
         runs enforce the same per-crate floors as CI (B7 coverage honesty)"
    );
}

/// The Groww QuestDB e2e CI lane must exist with both anti-false-OK guards:
/// the no-skip grep and the proof-count assertion, run under the
/// require-QuestDB env switch. The pinned strings below are the ACTUAL run-step
/// command lines — they appear ONLY inside the workflow's `run:` steps, never
/// in its header comment, so deleting the real steps cannot pass this guard.
/// 2026-07-15 (Groww live-feed retirement, operator directive — see
/// `merge-gate-lock-2026-07-04.md` dated note + the S-stage PR): the
/// QuestDB-backed Groww live-pipeline E2E (`groww_live_pipeline_e2e.rs`)
/// was DELETED with the live feed, and its CI lane (`groww-e2e.yml`) was
/// retired with it — a lane running `cargo test --test
/// groww_live_pipeline_e2e` against a deleted target would hard-fail every
/// triggering PR. This retirement ratchet replaces the two former
/// existence/shape pins: BOTH artifacts must stay absent; re-introducing a
/// Groww live E2E requires a fresh dated operator quote in the rule file
/// first.
#[test]
fn groww_e2e_lane_retired_with_live_feed() {
    let wf = repo_root().join(".github/workflows/groww-e2e.yml");
    assert!(
        !wf.exists(),
        "groww-e2e.yml was retired 2026-07-15 with the Groww live feed; \
         re-adding it requires a fresh dated operator quote in \
         merge-gate-lock-2026-07-04.md first"
    );
    let e2e = repo_root().join("crates/app/tests/groww_live_pipeline_e2e.rs");
    assert!(
        !e2e.exists(),
        "groww_live_pipeline_e2e.rs was deleted 2026-07-15 with the Groww \
         live feed; a revived live-pipeline E2E needs a fresh dated operator \
         quote first"
    );
}

/// Reads the enforced floors out of `quality/crate-coverage-thresholds.toml`.
///
/// Deliberately a hand-rolled scan rather than a TOML dependency: this crate
/// does not depend on a TOML parser and adding one to satisfy a guard would be
/// a new dependency root for a 20-line job (CLAUDE.md CARGO rule — new deps
/// need operator approval).
fn enforced_floors() -> Vec<(String, f64)> {
    let body = read("quality/crate-coverage-thresholds.toml");
    let mut out = Vec::new();
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        // Strip any trailing inline comment before parsing the number.
        let value = value.split('#').next().unwrap_or("").trim();
        if let Ok(parsed) = value.parse::<f64>() {
            out.push((name.to_string(), parsed));
        }
    }
    assert!(
        out.len() >= 9,
        "coverage-floor parser found only {} entries in \
         quality/crate-coverage-thresholds.toml — expected the default plus 8 \
         crates. A parser that silently finds nothing would let this whole \
         guard pass vacuously (audit-findings Rule 11: no false-OK signals).",
        out.len()
    );
    out
}

/// Every documented coverage floor must equal the enforced floor.
///
/// WHY THIS EXISTS (2026-09-06). The `app` floor was ratcheted 68.3 -> 72.6 in
/// this PR, and the sweep that preceded it found **nine stale floor numbers
/// across three live documents**, some of them two ratchets behind:
///
/// | document | app | core | storage | common |
/// |---|---|---|---|---|
/// | `guarantees.md` | ok | ok | ok | 99.5 (stale since 2026-08-18) |
/// | `100-percent-compliance-audit.md` | 63.3 | 90.2 | 91.2 | 99.5 |
/// | `testing.md` | 63.3 | 90.2 | 91.2 | 99.5 |
///
/// Nothing checked them, so each ratchet since 2026-07-17 left the prose
/// further behind the gate. This repository has already paid for that failure
/// mode twice in adjacent files — the $130 budget ceiling quoted in fifteen
/// places after it moved to $150, and the user-data byte budget cited as a live
/// blocker four times after it was freed — and the lesson recorded both times is
/// the same: a number that is only ever quoted is not evidence.
///
/// The check tolerates DATED HISTORY. A line carrying `->` or the unicode arrow
/// is recording a supersession (e.g. testing.md's "storage 91.2 -> 90.1 on
/// 2026-07-17: truthful re-baseline"), which is house convention and must not be
/// rewritten; only CURRENT claims are compared.
/// Extract every CURRENT `<crate> <number>` floor claim from one line of prose.
///
/// Split out of the doc scan so the self-test below can drive it with fixtures.
/// A guard whose parser is only exercised by the real documents cannot be shown
/// to reject anything -- it passes for as long as the documents happen to agree.
fn floor_claims_on_line(raw: &str, floors: &[(String, f64)]) -> Vec<(String, f64)> {
    // Markdown decoration is stripped BEFORE scanning so a pipe-table row, a
    // blockquote and a bolded cell all reduce to the same `<crate> <number>`
    // shape. `->` is folded to the unicode arrow FIRST, because blanking `>`
    // would otherwise turn `->` into `-` and destroy the supersession marker
    // this scan depends on.
    let line = raw
        .replace("->", "\u{2192}")
        .replace(['|', '>', '*', '`'], " ");
    let line = line.as_str();
    let mut out = Vec::new();

    for (crate_name, _) in floors {
        let needle = format!("{crate_name} ");
        let mut from = 0usize;
        while let Some(hit) = line[from..].find(&needle) {
            let start = from + hit;
            from = start + needle.len();
            // Require a word boundary on the left so `api 98.6` inside
            // `aws-lambdas api 98.6` style prose cannot double-match, and
            // `core` cannot match inside `tickvault-core`.
            if start > 0 {
                let prev = line[..start].chars().next_back().unwrap_or(' ');
                if prev.is_alphanumeric() || prev == '-' || prev == '_' {
                    continue;
                }
            }
            // `trim_start` because the markdown normalisation above turns
            // `| app | 72.6 |` into a run of spaces: the needle consumes
            // exactly ONE, so without this the reader lands on a space, parses
            // no number, and the row is silently not compared. That is how
            // adding CLAUDE.md first raised the count by ONE instead of by its
            // nine table rows.
            let rest = line[from..].trim_start();
            let num: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            // Only a `<crate> <number>` shape is a floor claim.
            let Ok(claimed) = num.parse::<f64>() else {
                continue;
            };
            if !num.contains('.') {
                continue;
            }
            // A dated supersession is house convention and must not be
            // rewritten: `storage 91.2 -> 90.1 on 2026-07-17` records that 91.2
            // USED to be the floor. Skip only THAT match -- the number
            // immediately followed by an arrow -- never the whole line.
            //
            // The line-wide skip this replaces was exploitable, and the exploit
            // is the reason the check moved: any line carrying an arrow
            // ANYWHERE was skipped entire, so `app 11.1 (ratchet 60.0 -> 70.0)`
            // -- a current claim wrong by 61 points, beside an unrelated arrow
            // -- passed green.
            if rest[num.len()..].trim_start().starts_with('\u{2192}') {
                continue;
            }
            out.push((crate_name.clone(), claimed));
        }
    }
    out
}

/// The parser must find real claims, tolerate real history, and reject neither
/// by accident.
#[test]
fn floor_claim_parser_self_test() {
    let floors = enforced_floors();
    let claims = |l: &str| floor_claims_on_line(l, &floors);

    // MUST be seen -- every documented shape in the four scanned files.
    assert_eq!(claims("- app 72.6"), vec![("app".into(), 72.6)]);
    assert_eq!(claims("> | common | 99.4 |"), vec![("common".into(), 99.4)]);
    assert_eq!(
        claims("> | **app** | **72.6** |"),
        vec![("app".into(), 72.6)]
    );
    assert_eq!(
        claims("| tickvault-logs-mcp | 87.3 |"),
        vec![("tickvault-logs-mcp".into(), 87.3)]
    );

    // MUST be seen -- the exploit the line-wide arrow skip let through.
    assert_eq!(
        claims("app 11.1 (ratchet 60.0 -> 70.0)"),
        vec![("app".into(), 11.1)],
        "a current claim beside an UNRELATED arrow must still be compared -- \
         skipping the whole line is what let `app 11.1` pass green"
    );

    // MUST NOT be seen -- dated supersessions, both arrow spellings. These are
    // house convention (testing.md carries one verbatim) and rewriting them
    // would destroy the audit trail this repository keeps on purpose.
    assert!(claims("storage 91.2 \u{2192} 90.1 on 2026-07-17").is_empty());
    assert!(claims("app 68.3 -> 72.6 in this PR").is_empty());

    // MUST be seen -- the unnamed-crate row. `default` is a real key in
    // crate-coverage-thresholds.toml, so the fallback floor is validated too.
    // Asserted here because the first version of this self-test expected the
    // OPPOSITE and the parser was right: worth pinning the true behaviour so
    // nobody "fixes" it back.
    assert_eq!(
        claims("| *(any crate not listed)* | *default 63.0* |"),
        vec![("default".into(), 63.0)]
    );

    // MUST NOT be seen -- prose that merely contains a crate name.
    assert!(claims("the app is the largest crate in the workspace").is_empty());
    assert!(
        claims("tickvault-core 91.6").is_empty(),
        "the left word boundary must stop `core` matching inside a crate path"
    );
}

#[test]
fn every_documented_coverage_floor_matches_the_enforced_floor() {
    const DOCS: &[&str] = &[
        "docs/architecture/guarantees.md",
        "docs/architecture/100-percent-compliance-audit.md",
        ".claude/rules/project/testing.md",
        // CLAUDE.md carries its OWN eight-crate floor table, and it is the file
        // the SESSION PROTOCOL makes every session read at startup -- so a stale
        // floor there is the one most likely to be believed. It was unguarded
        // until 2026-09-06 for a purely mechanical reason: its table is markdown
        // pipe rows inside a blockquote (`> | **app** | **72.6** |`), which the
        // old `<crate> <number>` scan could not see through.
        "CLAUDE.md",
    ];

    let floors = enforced_floors();
    let mut compared = 0usize;
    let mut wrong: Vec<String> = Vec::new();

    for doc in DOCS {
        let body = read(doc);
        for (lineno, raw) in body.lines().enumerate() {
            for (crate_name, claimed) in floor_claims_on_line(raw, &floors) {
                let floor = floors
                    .iter()
                    .find(|(n, _)| *n == crate_name)
                    .map(|(_, f)| *f)
                    .unwrap_or_default();
                compared += 1;
                if (claimed - floor).abs() > f64::EPSILON {
                    wrong.push(format!(
                        "{doc}:{} claims `{crate_name} {claimed}` but the enforced \
                         floor is {floor}",
                        lineno + 1
                    ));
                }
            }
        }
    }

    // 31 is the MEASURED count on 2026-09-06, not a round number, and it is
    // deliberately not lower.
    //
    // The rule this floor follows: it must sit ABOVE the count that survives
    // losing a whole input, never equal to it. Measured both ways today —
    // 31 with CLAUDE.md, 22 without it — so a silent loss of the largest
    // contributor cannot slide under the bar.
    //
    // The previous floor of 18 failed exactly that rule. An adversarial sweep
    // showed it sat at "still passes after losing the crate this guard was
    // written for": `app` supplied 4 of the 22, every one preceded by `(`, so a
    // single plausible-looking tightening of the boundary check dropped all
    // four, left compared at exactly 18, and let `app 11.1` -- wrong by 61
    // points -- pass green.
    //
    // An EXACT figure is the point, not brittleness: a doc edit that removes a
    // floor claim should fail this build and be re-measured deliberately, which
    // is the whole reason this guard exists.
    assert!(
        compared >= 31,
        "coverage-floor doc scan compared only {compared} claims across {} \
         documents — expected at least 31 (each carries a full floor list). A \
         scan that matches nothing passes vacuously, which is the exact \
         false-OK this guard exists to prevent.",
        DOCS.len()
    );
    assert!(
        wrong.is_empty(),
        "documented coverage floors have drifted from \
         quality/crate-coverage-thresholds.toml (the executable truth). Ratcheting \
         a floor means updating the prose in the SAME PR:\n  {}",
        wrong.join("\n  ")
    );
}
