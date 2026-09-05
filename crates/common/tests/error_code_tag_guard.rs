//! Meta-guard — every `error!` site that mentions a tracked error code in
//! its message MUST also carry a `code = ...` structured field.
//!
//! Phase 1 of `.claude/plans/active-plan.md`. This lets Loki alert rules
//! and Claude Code triage hooks group events by the stable `code` field
//! instead of regexing the free-text message. The message wording can
//! change over time; the `code` field MUST not.
//!
//! Mechanics:
//! - Scan every `.rs` file under `crates/*/src/` (excluding tests).
//! - For each `error!(...)` / `tracing::error!(...)` block, look at the
//!   string literal (the message). If it contains a tracked code prefix
//!   (like `I-P1-11:`, `OMS-GAP-02:`, etc.), the call MUST also contain
//!   `code =` within the same macro invocation.
//! - Violations are listed with file:line.
//!
//! Escape hatch: preceding `// APPROVED:` comment skips the check.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use tickvault_common::error_code::ErrorCode;

#[test]
fn every_error_macro_tagged_with_a_known_code_carries_code_field() {
    let root = workspace_root();
    let mut violations: Vec<String> = Vec::new();
    let tagged_prefixes = compute_tagged_prefixes();

    // 2026-08-10 anti-vacuity: count what was actually read. Before this, an
    // unreadable or moved directory made scan_dir return silently, so the guard
    // passed having inspected ZERO files while reporting green.
    let mut scanned_files: usize = 0;
    for (_crate_name, src_dir) in crates_with_src(&root) {
        scan_dir(
            &src_dir,
            &tagged_prefixes,
            &mut violations,
            &mut scanned_files,
        );
    }

    // The real corpus is ~291 .rs files across the six crates; 100 leaves 3x
    // headroom while still failing loudly if the walk collapses.
    assert!(
        scanned_files > 100,
        "error-code tag guard scanned only {scanned_files} files — it is \
         effectively enforcing NOTHING. Expected >100. This assert exists \
         because the guard previously passed while scanning zero."
    );

    assert!(
        violations.is_empty(),
        "error!/tracing::error! sites mention a tracked error code in the \
         message but do not carry a `code = ErrorCode::X.code_str()` field:\n{}\n\n\
         Add the `code` field at the top of the macro call, or prefix the \
         preceding line with `// APPROVED: <reason>` to skip this guard.",
        violations.join("\n")
    );
}

fn compute_tagged_prefixes() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for code in ErrorCode::all() {
        out.insert(format!("{}:", code.code_str()));
        out.insert(format!("{} ", code.code_str()));
    }
    out
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        // 2026-08-10: was `.unwrap_or_else(|| PathBuf::from("."))` — a silent CWD
        // fallback. If the layout ever changed, every crate path missed, the walk
        // found nothing, and the guard passed. Fail loudly instead.
        .expect("workspace root must exist above crates/common")
}

fn scan_dir(
    dir: &Path,
    tagged: &BTreeSet<String>,
    violations: &mut Vec<String>,
    scanned: &mut usize,
) {
    // 2026-08-10: both of the swallows below used to convert a corpus-read
    // failure into "nothing to check, pass". A guard cannot report on files it
    // never managed to open, so read failures now panic with the path.
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("error-code tag guard corpus unreadable {dir:?}: {e}"));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, tagged, violations, scanned);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let contents = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("error-code tag guard cannot read {path:?}: {e}"));
        *scanned = scanned.saturating_add(1);
        scan_file(&path, &contents, tagged, violations);
    }
}

fn scan_file(path: &Path, contents: &str, tagged: &BTreeSet<String>, violations: &mut Vec<String>) {
    let lines: Vec<&str> = contents.lines().collect();
    let mut idx = 0;
    while idx < lines.len() {
        let line = lines[idx];
        let is_error_macro = line.contains("error!(") || line.contains("tracing::error!(");
        if !is_error_macro {
            idx += 1;
            continue;
        }
        // Collect the macro body (possibly multi-line) by counting parens.
        let (body, end_idx) = collect_macro_body(&lines, idx);
        let mentions_tagged_code = tagged.iter().any(|p| body.contains(p.as_str()));
        if !mentions_tagged_code {
            idx = end_idx + 1;
            continue;
        }
        let carries_code_field = body.contains("code =") || body.contains("code=");
        if carries_code_field {
            idx = end_idx + 1;
            continue;
        }
        // APPROVED-comment escape hatch on the line immediately preceding
        // the macro start.
        let prev = idx
            .checked_sub(1)
            .and_then(|i| lines.get(i))
            .copied()
            .unwrap_or("");
        if prev.trim_start().starts_with("// APPROVED:") {
            idx = end_idx + 1;
            continue;
        }
        violations.push(format!(
            "  {}:{} — error! contains a tracked code prefix but no `code =` field",
            path.display(),
            idx + 1
        ));
        idx = end_idx + 1;
    }
}

/// Returns the full macro body (concatenated) and the 0-based index of the
/// line that closes the macro.
fn collect_macro_body(lines: &[&str], start: usize) -> (String, usize) {
    let mut body = String::new();
    let mut depth: i32 = 0;
    let mut seen_open = false;
    for (offset, raw) in lines.iter().enumerate().skip(start) {
        for ch in raw.chars() {
            match ch {
                '(' => {
                    depth += 1;
                    seen_open = true;
                }
                ')' => depth -= 1,
                _ => {}
            }
        }
        body.push_str(raw);
        body.push('\n');
        if seen_open && depth == 0 {
            return (body, offset);
        }
    }
    (body, lines.len().saturating_sub(1))
}

#[test]
fn tagged_prefix_set_is_non_empty() {
    let tagged = compute_tagged_prefixes();
    assert!(!tagged.is_empty());
    // Every ErrorCode should contribute exactly 2 entries (": " and " ").
    assert_eq!(tagged.len(), ErrorCode::all().len() * 2);
}

/// 2026-08-10. `tagged_prefix_set_is_non_empty` above is a genuine check of the
/// PREFIX SET, but it is NOT a non-vacuity check of this guard: it asserts on the
/// `ErrorCode` enum, which the test itself derives, and would keep passing even
/// if the file walk read nothing at all. That distinction is exactly how a
/// sibling guard in this repo stayed green for an unknown period while scanning
/// zero files.
///
/// This test asserts the thing that actually matters — that the corpus the guard
/// walks EXISTS and is substantial.
#[test]
fn scan_corpus_exists_and_is_substantial() {
    let root = workspace_root();
    let mut total = 0usize;
    for (_crate_name, src_dir) in crates_with_src(&root) {
        let mut violations: Vec<String> = Vec::new();
        let tagged = BTreeSet::new();
        scan_dir(&src_dir, &tagged, &mut violations, &mut total);
    }
    assert!(
        total > 100,
        "scan corpus collapsed to {total} files — the guard would enforce nothing"
    );
}

// ---------------------------------------------------------------------------
// 2026-08-21 — the UNCODED-error ratchet
// ---------------------------------------------------------------------------

/// How many production `error!` sites may still carry no `code =` field.
///
/// # Why this exists
///
/// The guard above only demands a `code =` field when the message ALREADY
/// names a code. An `error!` that mentions no code at all is fully permitted
/// by it — and is invisible to every automated path in this repository, because
/// the CloudWatch metric filters that page an operator match on
/// `$.code = "..."`. An uncoded error reaches the log sink and nothing else.
///
/// So the operator's standing requirement — cover every corner, never fail
/// silently — was satisfied for errors that name themselves and not for the
/// ones that do not. Which is backwards: an error whose author already knew
/// its code is the one least likely to be missed.
///
/// # Why a budget rather than fixing all of them at once
///
/// Each site needs a JUDGEMENT — which code, or whether it deserves a new one
/// — and a mechanical sweep that stamps a plausible code on eighty call sites
/// produces eighty plausible-looking wrong answers. Wrong codes are worse than
/// none: they route a real failure to the wrong runbook, and they do it with
/// the full confidence of a structured field.
///
/// So this pins the number instead. It cannot grow, and every site fixed must
/// lower it in the same change — so the count is always the truth rather than
/// a ceiling somebody padded once and forgot.
///
/// # The number was wrong the first time, by 22%
///
/// This budget was measured at **88** before the walker below was rewritten.
/// That figure came from a hand-rolled production/test split that cut each
/// file at the first `#[cfg(test)]` literal and discarded the rest — so every
/// `error!` sitting after a test module was invisible. A bite-proof plant
/// landed in that discarded tail and the guard reported PASS.
///
/// The real count is **107**. Nineteen uncoded sites were hidden, in files
/// that matter: `oms/engine.rs`, `token_manager.rs`, `ws_frame_spill.rs`,
/// `middleware.rs`, `main.rs`. The fix was not to write a better splitter but
/// to use `source_scan::production_region`, which was written on 2026-07-08
/// for exactly this failure and names two of those same files in its own
/// header. The lesson worth keeping is that a guard measuring the wrong
/// number still reports a number, and nothing about a green run says which.
///
/// # Movement
///
/// 107 (measured) → 91. Sixteen sites coded, all in QuestDB persistence
/// modules and none of them a guess: `order_audit_persistence.rs` already
/// tags its `ensure_ddl` failure with its own table write-failure code, so
/// the seven in `ws_event_audit_persistence.rs` take
/// `AuditWs01EventWriteFailed` and the nine across
/// `index_constituency_persistence.rs` +
/// `instrument_lifecycle_persistence.rs` take
/// `StorageGap03AuditWriteFailed` ("audit-table write failure, any table"),
/// which `pnl_audit_persistence.rs` already uses for the same shape. Neither
/// code is alarmed, so this changes no paging behaviour — only whether a
/// failure can be found by code in triage. The one that most needed it: a
/// failed `DEDUP ENABLE UPSERT KEYS` leaves a SEBI table silently accepting
/// duplicate rows.
/// 91 -> 83 (2026-08-21). Not eight sites coded: eight sites DELETED with the
/// Groww feed and the two-broker comparators that went with it. The budget
/// follows the corpus down because a ratchet allowed to sit above the truth is
/// a ceiling somebody padded once, and it stops ratcheting the moment it does.
///
/// 83 -> 82 (2026-08-26). ONE site coded, and it is the one that mattered
/// most: the `TrySendError::Full` arm in `ws_frame_spill.rs` — the WAL spill
/// channel filling — had no `error!` at all, so the durable floor could be
/// breached with nothing but a counter nobody was reading to say so. It now
/// carries `WS-SPILL-02`. The budget comes down with it, in the same change,
/// per the rule the line above states.
const UNCODED_ERROR_BUDGET: usize = 82;

/// Per-crate uncoded-error budgets, for the crates the six-name list never
/// reached.
///
/// ADDED 2026-09-05, and deliberately NOT folded into `UNCODED_ERROR_BUDGET`.
///
/// Widening the corpus admits pre-existing debt. Adding it to the shared
/// number would have raised 82 to 125 — and the doc above says, in its own
/// words, "Do NOT raise the budget", for the good reason that a ratchet
/// allowed to sit above the truth is a ceiling somebody padded once. Keeping
/// the new debt in its own named figure means the six-crate ratchet is
/// untouched and still only shrinks, while the newly visible debt is a number
/// somebody has to look at rather than a rounding error inside a bigger one.
///
/// A crate with no entry here gets a budget of ZERO. That is the fail-closed
/// half: a NEW crate cannot arrive carrying uncoded errors invisibly, which is
/// exactly how these two arrived.
///
/// `aws-lambdas` = 0.
///
/// It was 43 when this table was introduced, hours earlier the same day:
/// 44 production `error!` sites, one of which — `operator_control.rs`
/// `HTTP-CLIENT-01` — already carried a `code =` field. They are not
/// peripheral: `start_watchdog.rs` (11 sites) boots the prod box every
/// morning, `budget_killswitch.rs` (4) and `hard_stop_guard.rs` (3) can STOP
/// it, and `telegram_webhook.rs` (3) is how the operator is reached. Every
/// CloudWatch metric filter pages on `$.code`, so all 43 were failures in the
/// paging machinery that could not themselves page.
///
/// All 43 now carry a `LAMBDA-*` code (nine codes, grouped by operator
/// ACTION rather than by call site — see
/// `docs/error-runbooks/lambda-ops-error-codes.md`). The entry stays at 0
/// rather than being deleted so the `assert_eq!` below keeps proving the
/// count is still zero: removing the row would fall back to the same
/// fail-closed default and enforce the same thing, but the row records that
/// this crate WAS measured and IS at zero, which a missing row cannot say.
///
/// `tickvault-logs-mcp` = 0. It has no `error!` sites today, so it starts at
/// the fail-closed default and can never acquire one silently.
const UNCODED_ERROR_BUDGET_BY_CRATE: [(&str, usize); 2] =
    [("aws-lambdas", 0), ("tickvault-logs-mcp", 0)];

/// The six crates the original hardcoded list covered. Their debt is pooled in
/// `UNCODED_ERROR_BUDGET` — the historical ratchet, preserved exactly.
const ORIGINAL_CORPUS_CRATES: [&str; 6] = ["common", "storage", "core", "trading", "api", "app"];

/// Sites where an `error!` carries no code and that is CORRECT.
///
/// Empty today, deliberately: nothing has yet been examined closely enough to
/// earn an exemption. When one does, it belongs here with its reason, not in a
/// raised budget.
const UNCODED_ERROR_ALLOWLIST: [&str; 0] = [];

#[test]
fn uncoded_error_sites_may_only_shrink() {
    let root = workspace_root();
    let mut original_corpus: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    // Per-crate for the newly covered ones, so each carries its own number and
    // the six-crate ratchet below is untouched by the widening.
    let mut per_crate: Vec<(String, Vec<String>)> = Vec::new();

    for (crate_name, src_dir) in crates_with_src(&root) {
        if ORIGINAL_CORPUS_CRATES.contains(&crate_name.as_str()) {
            collect_uncoded(&src_dir, &mut original_corpus, &mut scanned);
        } else {
            let mut sites: Vec<String> = Vec::new();
            collect_uncoded(&src_dir, &mut sites, &mut scanned);
            per_crate.push((crate_name, sites));
        }
    }

    assert!(
        scanned > 100,
        "the uncoded-error ratchet scanned only {scanned} files — it would \
         enforce nothing. This assert exists because a sibling guard in this \
         repo once passed while reading zero."
    );

    // --- the six-crate historical ratchet, unchanged ------------------------
    let count = original_corpus.len();
    assert!(
        count <= UNCODED_ERROR_BUDGET,
        "{count} production `error!` site(s) carry NO `code =` field, over the \
         budget of {UNCODED_ERROR_BUDGET}.\n\n\
         An uncoded error reaches the log sink and NOTHING else: every \
         CloudWatch metric filter that pages an operator matches on \
         `$.code`, so these failures cannot fire an alarm, cannot be counted, \
         and cannot be found by code in triage.\n\n\
         Give the site the ErrorCode that fits, or — if it genuinely should \
         not have one — add it to UNCODED_ERROR_ALLOWLIST with the reason. Do \
         NOT raise the budget.\n\nSites:\n{}",
        original_corpus.join("\n")
    );

    assert_eq!(
        count, UNCODED_ERROR_BUDGET,
        "the budget is {UNCODED_ERROR_BUDGET} but only {count} uncoded site(s) \
         remain. Lower the budget to {count} in this same change — a ratchet \
         that is allowed to sit above the truth is a ceiling somebody padded \
         once, and it stops ratcheting the moment it does."
    );

    // --- the newly covered crates, each against its own budget --------------
    //
    // A crate with no entry gets ZERO. That is the fail-closed half: this is
    // exactly how `aws-lambdas` and `tickvault-logs-mcp` sat unscanned for
    // months behind a hardcoded six-name list while the guard reported success.
    for (crate_name, sites) in &per_crate {
        let budget = UNCODED_ERROR_BUDGET_BY_CRATE
            .iter()
            .find(|(name, _)| *name == crate_name.as_str())
            .map_or(0, |(_, b)| *b);
        let found = sites.len();

        assert!(
            found <= budget,
            "crate `{crate_name}` has {found} production `error!` site(s) with \
             NO `code =` field, over its budget of {budget}.\n\n\
             If this crate is new: it starts at ZERO by design — an uncoded \
             error reaches the log sink and nothing else, because every \
             CloudWatch metric filter that pages an operator matches on \
             `$.code`. Give each site the ErrorCode that fits. Adding an entry \
             to UNCODED_ERROR_BUDGET_BY_CRATE is admitting debt, so it needs a \
             dated reason at the constant saying why the debt is acceptable \
             today.\n\nSites:\n{}",
            sites.join("\n")
        );

        assert_eq!(
            found, budget,
            "crate `{crate_name}` is budgeted {budget} uncoded `error!` site(s) \
             but only {found} remain. Lower it to {found} in this same change — \
             same rule as the pooled budget above."
        );
    }

    // Anti-vacuity on the widening itself: every name in the by-crate table
    // must correspond to a crate that actually exists, or the table is
    // recording a budget for something that cannot be measured.
    for (name, _) in UNCODED_ERROR_BUDGET_BY_CRATE {
        assert!(
            per_crate.iter().any(|(c, _)| c == name),
            "UNCODED_ERROR_BUDGET_BY_CRATE names `{name}`, which discovery did \
             not find under crates/*/src. Remove the stale entry — a budget \
             for a crate that no longer exists enforces nothing and hides the \
             fact that nothing is being enforced."
        );
    }
}

/// Walk `dir`, recording every production `error!` with no `code =` field.
///
/// The production half is carved by [`tickvault_common::source_scan`], NOT by
/// a hand-rolled split. That is deliberate and was learned the expensive way:
/// the first version of this walker cut the file at the first `#[cfg(test)]`
/// literal and threw away everything after it. A bite-proof plant landed in
/// that discarded tail and the guard reported PASS — and a follow-up scan
/// found **34 files** with real top-level production items sitting after a
/// `#[cfg(test)]`, among them `oms/engine.rs`, `token_manager.rs`,
/// `ws_frame_spill.rs`, `middleware.rs` and `main.rs`.
///
/// `source_scan` was written on 2026-07-08 to close exactly those two holes,
/// and its own header names `token_manager.rs` (~100 lines) and `main.rs`
/// (~460 lines) as the victims. Reaching for it rather than re-deriving it
/// also buys comment-blanking, which removes a false-positive class this scan
/// would otherwise have: an `error!` named inside a doc comment.
fn collect_uncoded(dir: &Path, out: &mut Vec<String>, scanned: &mut usize) {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("uncoded-error corpus unreadable {dir:?}: {e}"));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_uncoded(&path, out, scanned);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let contents = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("uncoded-error guard cannot read {path:?}: {e}"));
        *scanned = scanned.saturating_add(1);

        // Comments first (an `error!` mentioned in prose is not an emit site),
        // then blank the `#[cfg(test)] mod tests` block while KEEPING every
        // production line that follows it. A file with no test module is
        // production end to end, which is what the fallback says.
        let stripped = tickvault_common::source_scan::strip_rust_comments(&contents);
        let production =
            tickvault_common::source_scan::production_region(&stripped).unwrap_or(stripped);

        let lines: Vec<&str> = production.lines().collect();
        let mut idx = 0;
        while idx < lines.len() {
            let line = lines[idx];
            if !(line.contains("error!(") || line.contains("tracing::error!(")) {
                idx += 1;
                continue;
            }
            let (body, end_idx) = collect_macro_body(&lines, idx);
            let coded = body.contains("code =") || body.contains("code=");
            let site = format!("{}:{}", path.display(), idx + 1);
            let allowed = UNCODED_ERROR_ALLOWLIST.iter().any(|a| site.ends_with(a));
            if !coded && !allowed {
                out.push(format!("  {site}"));
            }
            idx = end_idx + 1;
        }
    }
}
/// Every crate under `crates/` that has a `src/` directory, discovered from the
/// filesystem rather than listed.
///
/// ADDED 2026-09-05. All three tests in this file previously walked the SAME
/// hardcoded array — `["common","storage","core","trading","api","app"]` — and
/// the workspace has EIGHT crates. `aws-lambdas` and `tickvault-logs-mcp` were
/// therefore never scanned, and nothing said so: the guard reported success
/// having never looked at them, which is worse than failing, because the output
/// asserted completeness.
///
/// aws-lambdas is not a peripheral crate. It holds the Dhan token minter, the
/// budget kill-switch that can STOP THE PROD BOX, the start watchdog that boots
/// it each morning, and the Telegram webhook. Measured on the day this landed:
/// **44 production `error!` sites, exactly ONE of which carried a `code =`
/// field.** Every CloudWatch metric filter that pages an operator matches on
/// `$.code`, so 43 failures in the components that page the operator could not
/// themselves page anyone.
///
/// Deriving the list from the filesystem means a crate cannot be omitted by
/// forgetting to add it; a new crate arrives with a budget of ZERO (see
/// `uncoded_budget_for`) instead of vanishing. Same shape as the fail-closed
/// discovery in `scripts/test-coverage-guard.sh`.
fn crates_with_src(root: &Path) -> Vec<(String, PathBuf)> {
    let crates_dir = root.join("crates");
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(&crates_dir)
        .unwrap_or_else(|e| panic!("cannot read {crates_dir:?}: {e}"))
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let src = path.join("src");
            if !src.is_dir() {
                return None;
            }
            let name = path.file_name()?.to_str()?.to_string();
            Some((name, src))
        })
        .collect();
    out.sort();

    // Anti-vacuity: the workspace has eight crates and six were being scanned
    // when this was written. A discovery that returns fewer than the six it
    // replaced would be a silent regression to a narrower corpus.
    assert!(
        out.len() >= 6,
        "crate discovery found only {} crate(s) with a src/ dir under {crates_dir:?} — \
         the guard would enforce almost nothing. This assert exists because the \
         hardcoded list it replaced was silently short by two.",
        out.len()
    );
    out
}
