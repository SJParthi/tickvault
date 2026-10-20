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
//!
//! 2026-10-20 (plan item D2, `.claude/plans/active-plan-audit-2026-10-20.md`):
//! `every_production_error_carries_a_code` demands a `code` field on EVERY
//! production `error!` in every crate, message or no message, with a
//! per-file allowlist that names only the frozen indicator/strategy lines.
//! It matches the field NAME, not the substring `code =`: the substring
//! matched `exchange_segment_code =` and `segment_code =` and let two
//! uncoded production lines pass as coded.
//!
//! 2026-10-20 (hostile review of D2): the field match is now the tracing
//! form only, `code =` not followed by `=` or `>`. It also took `code:` (a
//! struct-literal field inside an argument) and `code =>` (a match arm), and
//! neither is a structured field on the event. The first test below, which
//! still matched the substring `code =` (a message reading "code = 3" passed
//! as the field), now uses the same scanner; and a char literal such as
//! `'"'` no longer reads as the start of a string in either scan.

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
        // 2026-10-20: the shared field scanner, not the substring `code =`,
        // which a message ("… code = 3 …") or `segment_code =` satisfied.
        if carries_code_field(&body) {
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
///
/// 82 -> 77 (2026-09-09). FIVE sites coded in one change, all in
/// `dhan_contract_universe::fetch_spot_prices` — the QuestDB spot backstop's
/// client-build, non-2xx, unreadable-body, send and unparseable-response arms.
/// They mattered because that function is the fallback the contract selector
/// reaches for when RAM has no price, and an UNCODED `error!` is invisible to
/// all 27 coded metric filters: the database could be refusing every read and
/// the only surface was ladders attributed to `without_spot`, which reads as
/// "this stock did not trade" rather than "we could not price it". Each now
/// carries `WS-GAP-03` with a `source` naming which arm failed. The budget
/// comes down with them, in the same change, per the rule two lines above.
///
/// 77 -> 76 (2026-09-16). NOT a site coded — a site DELETED, the same shape
/// as the 91 -> 83 Groww step above. The operator's SOCKETS-ONLY directive
/// (`no-rest-except-live-feed-2026-06-27.md` §12, narrowed by §12.10 to
/// MARKET DATA + VERIFICATION) removed the per-minute Dhan REST legs and the
/// 15:41 cross-verification, and one uncoded `error!` went with them. The
/// budget follows the corpus down in the same change, per the rule stated at
/// the 91 -> 83 step: a ratchet allowed to sit above the truth is a ceiling
/// somebody padded once, and it stops ratcheting the moment it does.
///
/// 76 -> 71 (2026-09-28, audit D3c-1). Five sites CODED: the daily rider's
/// constituent-unresolved, gate-closed, persist-failed, task-died and
/// still-failing lines in `dhan_universe.rs` now carry `WS-GAP-03` with a
/// `source` naming the arm.
///
/// 71 -> 61 (2026-10-04, workspace audit M3). Ten sites CODED with existing
/// codes, none of them paged: the order-update broadcast drop
/// (`ORDER-EVT-01`, stage `broadcast_no_receiver`), the order-update WAL drop
/// and the boot WAL replay failure (`WS-SPILL-02`, with a `source`), the
/// order-update server auth/API error (`WS-GAP-01`), the two token-renewal
/// give-up lines and the token publish failure (`AUTH-GAP-01`), the two
/// static-IP boot-check lines (`GAP-NET-01`), and the seal-writer construct
/// failure (`AGGREGATOR-SEAL-01`).
///
/// 61 -> 10 (2026-10-05, workspace audit M3, second pass). Fifty-one sites
/// CODED, each with an existing code and a `source` naming the arm, and each
/// code picked so no CloudWatch filter matches it (the filters match
/// `WS-SPILL-0x`, `RISK-GAP-03`, `BOOT-02/03`, `PROC-01`, `RESOURCE-02`,
/// `STORAGE-GAP-05` and others on any source, and `WS-GAP-02/03` and
/// `OMS-GAP-06` only on named sources none of these use). So this step adds
/// no page. **Correction to the step above:** "none of them paged" was wrong
/// for the two `WS-SPILL-02` lines; that code pages on any ERROR line, so
/// those two now reach the existing frame-drop alarm (a real loss, an
/// existing alarm, no new alarm).
///
/// The ten left, and why: three in `crates/trading/src/indicator/` and
/// `strategy/` (frozen area, `daily-universe-scope-expansion` §28); the two
/// `tick_gap_tracker.rs` lines and the trading-pipeline lag line (the only
/// fitting code, `RISK-GAP-03`, pages per instrument); the WAL corrupt-segment
/// skip (the only fitting code, `WS-SPILL-02`, pages, and a torn segment after
/// any crash would page at boot); the WAL init failure, the panic hook and the
/// API server exit (no existing code fits; each needs its own code and
/// runbook).
///
/// 10 -> 3 (2026-10-06, workspace audit M3, third pass). The seven above
/// that sit outside the frozen area are CODED, with six new Medium codes
/// that no CloudWatch filter matches (checked against every `pattern` in
/// `deploy/aws/terraform/*.tf`: the filters match exact code strings, and
/// none names these), so this step adds no page:
/// `WS-SPILL-03` (WAL init failure; the halted boot is still paged by the
/// existing boot-heartbeat and liveness alarms), `WS-SPILL-04` (unreadable
/// WAL segment skipped at replay), `PROC-03` (the panic hook),
/// `API-SERVER-01` (the API server exit), `TICK-GAP-01` (both tick-gap
/// tracker lines, `source` = `instrument_gap` / `reconnect_backfill_window`)
/// and `PIPELINE-LAG-01` (the trading pipeline's broadcast lag). Runbook:
/// `docs/error-runbooks/audit-m3-error-codes.md`.
///
/// The three left are the frozen-area lines (`indicator/engine.rs` and two
/// in `strategy/hot_reload.rs`); they change only with a fresh dated
/// operator approval under `daily-universe-scope-expansion` §28.
///
/// 3 -> 3 (2026-10-20, plan item D2). The number did not move and the count
/// behind it was wrong. The walker treated `body.contains("code =")` as "has
/// a code field", which is also true of `exchange_segment_code = …` and
/// `segment_code = …`. So the stale-LTP line in `tick_gap_tracker.rs` and the
/// slot-exhausted line in `multi_tf_aggregator.rs` carried NO code and were
/// counted as coded, while two needle strings inside guard tests
/// (`src.matches("tracing::error!(")`, `helper.contains("error!(")`) were
/// counted as `error!` sites. True count before this change: 5. Both lines
/// are coded in the same change (`TICK-GAP-01` source `stale_ltp`,
/// `AGGREGATOR-DROP-01` source `slot_exhausted`), and the walker now uses
/// `production_error_sites` (field-name match, string-aware), so the count is
/// 3 again and this time it is the true 3.
const UNCODED_ERROR_BUDGET: usize = 3;

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
        // 2026-10-20: both steps, and the field match, now live in
        // `production_error_sites`, shared with
        // `every_production_error_carries_a_code`, so the two tests cannot
        // count the same corpus two different ways.
        for (line, coded) in production_error_sites(&contents) {
            let site = format!("{}:{}", path.display(), line);
            let allowed = UNCODED_ERROR_ALLOWLIST.iter().any(|a| site.ends_with(a));
            if !coded && !allowed {
                out.push(format!("  {site}"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2026-10-20 — EVERY production `error!` carries a code (plan item D2)
// ---------------------------------------------------------------------------

/// Production `error!` sites that may carry NO `code` field, per file, with
/// the exact count. Today: only the frozen indicator/strategy area.
///
/// Both files sit under `daily-universe-scope-expansion-2026-05-27.md` §28
/// ("as of now don't even touch indicators and strategies area dude okay?"):
/// a change to `crates/trading/src/indicator/` or `crates/trading/src/strategy/`
/// needs a fresh dated operator approval, and none exists for coding these
/// lines. Measured 2026-10-20 by reading the code:
/// - `indicator/engine.rs`: the indicator slot-capacity line;
/// - `strategy/hot_reload.rs`: the file-watcher error line and the config
///   reload failure line.
///
/// Only these prefixes may appear here (`FROZEN_AREA_PREFIXES`). The count is
/// exact: coding one of these lines (after the approval) must lower or remove
/// its entry in the same change.
const FROZEN_UNCODED_ERROR_SITES: [(&str, usize); 2] = [
    ("crates/trading/src/indicator/engine.rs", 1),
    ("crates/trading/src/strategy/hot_reload.rs", 2),
];

/// The only directories `FROZEN_UNCODED_ERROR_SITES` may name.
const FROZEN_AREA_PREFIXES: [&str; 2] = [
    "crates/trading/src/indicator/",
    "crates/trading/src/strategy/",
];

/// Anti-vacuity floor on the number of production `error!` sites seen.
/// Measured 2026-10-20: 812 sites in 353 files under `crates/*/src`. The
/// floor sits well under that so deleting code does not trip it, and far
/// above what a walk that silently lost a crate would see.
const MIN_PRODUCTION_ERROR_SITES: usize = 600;

#[test]
fn every_production_error_carries_a_code() {
    let root = workspace_root();
    let mut files: Vec<PathBuf> = Vec::new();
    for (_crate_name, src_dir) in crates_with_src(&root) {
        collect_rs_files(&src_dir, &mut files);
    }
    files.sort();

    assert!(
        files.len() > 100,
        "the every-error-is-coded guard found only {} .rs files under \
         crates/*/src; it would enforce nothing.",
        files.len()
    );

    let mut total_sites = 0usize;
    let mut uncoded: Vec<(String, Vec<usize>)> = Vec::new();
    for path in &files {
        let contents = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("every-error-is-coded guard cannot read {path:?}: {e}"));
        let sites = production_error_sites(&contents);
        total_sites += sites.len();
        let lines: Vec<usize> = sites
            .iter()
            .filter(|(_, coded)| !coded)
            .map(|(line, _)| *line)
            .collect();
        if !lines.is_empty() {
            uncoded.push((relative_path(&root, path), lines));
        }
    }

    assert!(
        total_sites >= MIN_PRODUCTION_ERROR_SITES,
        "the every-error-is-coded guard saw only {total_sites} production \
         `error!` sites (floor {MIN_PRODUCTION_ERROR_SITES}, measured 812 on \
         2026-10-20). Either the walk lost files or the detector stopped \
         matching; it would be passing by seeing nothing."
    );

    let mut violations: Vec<String> = Vec::new();
    for (file, lines) in &uncoded {
        let listed = FROZEN_UNCODED_ERROR_SITES
            .iter()
            .find(|(allowed, _)| allowed == file);
        let where_ = lines
            .iter()
            .map(|line| format!("{file}:{line}"))
            .collect::<Vec<_>>()
            .join(", ");
        match listed {
            None => violations.push(format!(
                "  {where_} — production `error!` with no `code` field"
            )),
            Some((_, expected)) if lines.len() > *expected => violations.push(format!(
                "  {where_} — {} uncoded, allowlisted {expected}: a new uncoded \
                 `error!` in a frozen file",
                lines.len()
            )),
            Some((_, expected)) if lines.len() < *expected => violations.push(format!(
                "  {file} — {} uncoded, allowlisted {expected}: lower the \
                 FROZEN_UNCODED_ERROR_SITES entry to {} in this change",
                lines.len(),
                lines.len()
            )),
            Some(_) => {}
        }
    }
    for (allowed, expected) in FROZEN_UNCODED_ERROR_SITES {
        if !FROZEN_AREA_PREFIXES
            .iter()
            .any(|prefix| allowed.starts_with(prefix))
        {
            violations.push(format!(
                "  {allowed} — FROZEN_UNCODED_ERROR_SITES may name only the \
                 frozen indicator/strategy area; give this file's lines a code"
            ));
        }
        if !uncoded.iter().any(|(file, _)| file == allowed) {
            violations.push(format!(
                "  {allowed} — allowlisted {expected} uncoded, found 0 (or the \
                 file is gone): remove the stale FROZEN_UNCODED_ERROR_SITES entry"
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "every production `error!` must carry a structured `code` field \
         (`code = ErrorCode::X.code_str()`). Without one the line reaches the \
         log files and nothing else: every CloudWatch metric filter matches \
         `$.code`, and triage groups by it.\n\n\
         Pick the existing ErrorCode that fits (read its runbook first; a code \
         whose filter pages on every line is the wrong home for a \
         per-instrument line). Only the frozen indicator/strategy lines \
         (daily-universe-scope-expansion §28) are allowlisted.\n\n{}",
        violations.join("\n")
    );
}

/// Every `.rs` file under `dir`, recursively. Panics on an unreadable
/// directory: a guard must not pass on files it never opened.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("every-error-is-coded corpus unreadable {dir:?}: {e}"));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// `path` relative to the workspace root, with `/` separators, so the
/// allowlist reads the same on every platform.
fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Every `error!` invocation in the PRODUCTION region of one source file:
/// `(1-based line, carries a code field)`.
///
/// Comments are blanked and the `#[cfg(test)] mod tests` block is excised by
/// `tickvault_common::source_scan`, the same carve the budget ratchet above
/// has used since its own walker hid nineteen sites. A file with no test
/// module is production end to end.
///
/// Honest limits: only the first `error!(` on a line is seen, and a second
/// one opening on the line where the first closes is skipped (none exist
/// today); a raw string (`r#"…"#`) inside the call is read as an ordinary
/// string (none exist today); `#[cfg(test)]` items OUTSIDE `mod tests` are
/// scanned as production, which errs toward demanding a code.
fn production_error_sites(contents: &str) -> Vec<(usize, bool)> {
    let stripped = tickvault_common::source_scan::strip_rust_comments(contents);
    let production =
        tickvault_common::source_scan::production_region(&stripped).unwrap_or(stripped);
    let lines: Vec<&str> = production.lines().collect();
    let mut sites = Vec::new();
    let mut idx = 0usize;
    while idx < lines.len() {
        let Some(column) = error_macro_column(lines[idx]) else {
            idx += 1;
            continue;
        };
        let (body, end_idx) = macro_invocation(&lines, idx, column);
        sites.push((idx + 1, carries_code_field(&body)));
        idx = end_idx + 1;
    }
    sites
}

/// Byte offset of the first `error!(` on `line` that sits in CODE, not inside
/// a string literal opened earlier on the same line. A guard that greps for
/// its own needle (`src.matches("tracing::error!(")`) is not an emit site,
/// and taking it for one also makes the body walk swallow the lines after it
/// until its unbalanced `(` closes.
fn error_macro_column(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = line[search_from..].find("error!(") {
        let at = search_from + rel;
        search_from = at + "error!(".len();
        // A qualified path (`tracing::error!`) is fine; the name must not be
        // the tail of a longer identifier (`my_error!(` is another macro).
        if at > 0 && is_ident_byte(bytes[at - 1]) {
            continue;
        }
        // 2026-10-20: walk the strings rather than count quotes, so a char
        // literal `'"'` before the call does not read as an open string.
        let mut in_str = false;
        let mut idx = 0usize;
        while idx < at {
            let byte = bytes[idx];
            if in_str {
                match byte {
                    b'\\' => idx += 1,
                    b'"' => in_str = false,
                    _ => {}
                }
                idx += 1;
                continue;
            }
            if let Some(len) = char_literal_len(bytes, idx) {
                idx += len;
                continue;
            }
            if byte == b'"' {
                in_str = true;
            }
            idx += 1;
        }
        if !in_str {
            return Some(at);
        }
    }
    None
}

/// Byte length of the char (or byte) literal opening at `bytes[idx]`: 3 for
/// `'x'`, 4 for an escape such as `'\''`, `'\"'` or `'\\'`. `None` for a
/// lifetime (`'a`), for a byte that is not `'`, and for a literal this scan
/// does not need to measure (`'\u{…}'`, `'\x7f'`, a multi-byte char): none of
/// those can hold a `"`, `(` or `)`, which is all the scans here care about.
fn char_literal_len(bytes: &[u8], idx: usize) -> Option<usize> {
    if bytes.get(idx) != Some(&b'\'') {
        return None;
    }
    match (bytes.get(idx + 1), bytes.get(idx + 2), bytes.get(idx + 3)) {
        (Some(b'\\'), Some(_), Some(b'\'')) => Some(4),
        (Some(b), Some(b'\''), _) if *b != b'\\' && b.is_ascii() => Some(3),
        _ => None,
    }
}

/// The text of the `error!(…)` call that opens at `lines[start][column..]`,
/// and the index of the line that closes it.
///
/// Parentheses count only in code: a `(` inside the message string or a
/// `'('` char literal does not move the depth. `collect_macro_body` above
/// counts every paren on the line, which is why it is not reused here.
fn macro_invocation(lines: &[&str], start: usize, column: usize) -> (String, usize) {
    let mut body = String::new();
    let mut depth = 0i32;
    let mut seen_open = false;
    let mut in_str = false;
    for (offset, raw) in lines.iter().enumerate().skip(start) {
        let text = if offset == start { &raw[column..] } else { raw };
        let bytes = text.as_bytes();
        let mut idx = 0usize;
        while idx < bytes.len() {
            let byte = bytes[idx];
            if in_str {
                match byte {
                    b'\\' => idx += 1,
                    b'"' => in_str = false,
                    _ => {}
                }
                idx += 1;
                continue;
            }
            match byte {
                b'"' => in_str = true,
                b'\'' => {
                    // Skip `'('`, `'"'` and `'\''`; a lifetime is not a char
                    // literal. 2026-10-20: the escape form is checked first
                    // (`char_literal_len`), where `'\''` used to match the
                    // three-byte form and leave its last quote behind.
                    if let Some(len) = char_literal_len(bytes, idx) {
                        idx += len - 1;
                    }
                }
                b'(' => {
                    depth += 1;
                    seen_open = true;
                }
                b')' => depth -= 1,
                _ => {}
            }
            idx += 1;
            if seen_open && depth == 0 {
                body.push_str(&text[..idx]);
                return (body, offset);
            }
        }
        body.push_str(text);
        body.push('\n');
    }
    (body, lines.len().saturating_sub(1))
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `body` with the CONTENT of every string literal replaced by spaces
/// (quotes and newlines kept), so a message reading "code = 7" cannot pass
/// for a field. Char literals are blanked the same way (2026-10-20): `'"'`
/// used to open a string, hiding the code that followed it.
fn blank_string_contents(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut in_str = false;
    let mut escaped = false;
    let mut skip_to = 0usize;
    for (at, ch) in body.char_indices() {
        if at < skip_to {
            continue;
        }
        if !in_str && let Some(len) = char_literal_len(bytes, at) {
            out.push('\'');
            out.extend(std::iter::repeat_n(' ', len - 2));
            out.push('\'');
            skip_to = at + len;
            continue;
        }
        if in_str {
            if escaped {
                escaped = false;
                out.push(if ch == '\n' { '\n' } else { ' ' });
                continue;
            }
            match ch {
                '\\' => {
                    escaped = true;
                    out.push(' ');
                }
                '"' => {
                    in_str = false;
                    out.push('"');
                }
                '\n' => out.push('\n'),
                _ => out.push(' '),
            }
            continue;
        }
        if ch == '"' {
            in_str = true;
        }
        out.push(ch);
    }
    out
}

/// True when the call passes a structured field named exactly `code` in the
/// form tracing takes: `code = …`, whitespace and newlines allowed around `=`.
///
/// The name must start after a non-identifier, non-`.` byte (`segment_code`
/// and the dotted field `http.code` are other fields) and end at a
/// non-identifier byte (`codes` is another field). `code ==` is an
/// expression and `code =>` a match arm, not fields. `code: …` is NOT
/// accepted (2026-10-20): inside a tracing call it can only be a
/// struct-literal field of some argument, never a field of the event. String
/// and char literal contents never count.
fn carries_code_field(body: &str) -> bool {
    let code_only = blank_string_contents(body);
    let bytes = code_only.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = code_only[search_from..].find("code") {
        let at = search_from + rel;
        search_from = at + "code".len();
        if at > 0 && (is_ident_byte(bytes[at - 1]) || bytes[at - 1] == b'.') {
            continue;
        }
        let mut next = at + "code".len();
        if bytes.get(next).is_some_and(|b| is_ident_byte(*b)) {
            continue;
        }
        while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
            next += 1;
        }
        if bytes.get(next) == Some(&b'=') && !matches!(bytes.get(next + 1), Some(b'=' | b'>')) {
            return true;
        }
    }
    false
}

/// The scanner bites: an uncoded call is flagged even when another field
/// ends in `code`, the message says "code =", or a paren in the message is
/// left open; a needle string in a guard is not a site; test-module calls
/// are not production.
#[test]
fn production_error_scanner_bites() {
    let src = r##"fn a() {
    error!(
        exchange_segment_code = 1,
        segment_code = 2,
        "stale LTP, code = 3"
    );
    tracing::error!(code = ErrorCode::X.code_str(), "coded");
    error!(
        code =
            ErrorCode::Y.code_str(),
        "multi-line, left open ("
    );
    let n = src.matches("tracing::error!(").count();
    error!(?err, "file watcher error");
}

#[cfg(test)]
mod tests {
    fn t() {
        error!("test-only, never counted");
    }
}
"##;
    assert_eq!(
        production_error_sites(src),
        vec![(2, false), (7, true), (8, true), (14, false)],
        "the scanner must flag lines 2 and 14, accept lines 7 and 8, skip the \
         needle string on line 13 and the test module"
    );
}

#[test]
fn carries_code_field_matches_only_the_field_named_code() {
    assert!(carries_code_field("error!(code = X, \"m\")"));
    assert!(carries_code_field("error!(\n    code\n        = X, \"m\")"));
    // 2026-10-20: a struct-literal field and a match arm are not the event's
    // field; both used to pass.
    assert!(!carries_code_field("error!(code: X)"));
    assert!(!carries_code_field(
        "error!(err = ?Status { code: 1 }, \"m\")"
    ));
    assert!(!carries_code_field(
        "error!(kind = match c { code => 1 }, \"m\")"
    ));
    assert!(carries_code_field("error!(code = %c, x = 1 == 2, \"m\")"));
    assert!(!carries_code_field(
        "error!(exchange_segment_code = 1, \"m\")"
    ));
    assert!(!carries_code_field("error!(segment_code = 1)"));
    assert!(!carries_code_field("error!(http.code = 1)"));
    assert!(!carries_code_field("error!(codes = 1)"));
    assert!(!carries_code_field("error!(x = code::Y)"));
    assert!(!carries_code_field("error!(ok = code == 1)"));
    assert!(!carries_code_field("error!(\"code = 1 in the message\")"));
    assert!(!carries_code_field("error!(\"escaped \\\" code = 1\")"));
}

/// 2026-10-20. A char literal (`'x'`, `'\''`, `'"'`) is not a string. Before,
/// `'"'` ahead of a call on the same line read as an open string and the call
/// was missed; inside a call it hid the field after it, or exposed a
/// message's "code = 1" as one. Lifetimes are left alone.
#[test]
fn char_literals_do_not_open_strings() {
    assert_eq!(
        error_macro_column("let q = '\"'; error!(code = X, \"m\");"),
        Some(13)
    );
    assert_eq!(
        error_macro_column("let q = '\\''; error!(\"m\");"),
        Some(14)
    );
    assert_eq!(
        error_macro_column("let q = '\\\"'; error!(\"m\");"),
        Some(14)
    );
    assert_eq!(
        error_macro_column("let q = \"'\"; error!(\"m\");"),
        Some(13)
    );
    assert_eq!(error_macro_column("let q = \"error!(\";"), None);
    assert!(carries_code_field("error!(sep = '\"', code = X, \"m\")"));
    assert!(carries_code_field("error!(sep = '\\'', code = X, \"m\")"));
    assert!(!carries_code_field("error!(sep = '\"', \"code = 1\")"));
    assert_eq!(
        blank_string_contents("f('\"', \"ab\", '\\'', '\\\\', 'x', &'a str)"),
        "f(' ', \"  \", '  ', '  ', ' ', &'a str)"
    );
    // A call after a `'"'` on its line is seen, and a `')'` inside it does
    // not close it early.
    let src = "fn a() {\n    let q = '\"'; error!(sep = ')', \"m\");\n    \
               error!(code = X, \"n\");\n}\n";
    assert_eq!(production_error_sites(src), vec![(2, false), (3, true)]);
}

/// 2026-10-20. The tagged-code test reads the field with the same scanner:
/// the substring `code =` it used before was satisfied by a message reading
/// "code = 3" and by `segment_code =`.
#[test]
fn tagged_code_scan_needs_the_field_not_the_words() {
    let tagged = compute_tagged_prefixes();
    let code = ErrorCode::all()[0].code_str();
    let src = format!(
        "fn a() {{\n    error!(\"{code}: stale, code = 3\");\n    \
         error!(segment_code = 1, \"{code}: x\");\n    \
         error!(code = X, \"{code}: y\");\n}}\n"
    );
    let mut violations = Vec::new();
    scan_file(Path::new("bite.rs"), &src, &tagged, &mut violations);
    assert_eq!(
        violations.len(),
        2,
        "lines 2 and 3 carry no `code` field and must be flagged: {violations:?}"
    );
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
