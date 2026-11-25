//! CRITICAL-severity paging-coverage ratchet (2026-08-11).
//!
//! ## The gap this closes
//!
//! `error_code_paging_filter_drift_guard.rs` (crates/common) checks ONE
//! direction: every terraform `error_code_alerts` entry must have a real
//! emit site (no DEAD filters). It does NOT check the inverse, and the
//! inverse is where the operator actually gets hurt: a
//! `Severity::Critical` code that emits `error!` in production but has NO
//! alarm entry pages **nobody**. A Critical that reaches no human surface
//! is a silent failure by definition — the exact class of the 2026-07-06
//! zero-page incident that created `error-code-alarms.tf` in the first
//! place.
//!
//! A mechanical sweep on 2026-08-11 found 167 `ErrorCode` variants, 29 of
//! them `Severity::Critical`, and only **4** of those 29 carried an alarm
//! entry (DH-901, AUTH-GAP-04, PROC-01, AGGREGATOR-DROP-01). Eleven more
//! had a real ERROR-level emit and no alarm; the remaining fourteen had no
//! `error!` emit site at all.
//!
//! ## What this guard enforces
//!
//! **Every `Severity::Critical` `ErrorCode` with a real ERROR-level emit
//! site in `crates/*/src` must have an `error_code_alerts` entry in
//! `deploy/aws/terraform/error-code-alarms.tf`, or an explicit
//! [`UNWIRED_ALLOWLIST`] row carrying a one-line reason.**
//!
//! The allowlist is a **shrinking ratchet**: [`allowlist_never_grows`]
//! pins its length, so covering a code means deleting its row, and adding
//! a row is a visible, reviewable diff that must state why the code cannot
//! be alarmed.
//!
//! ## What this guard deliberately does NOT do
//!
//! It does not require an alarm for a Critical code with **no** emit site.
//! Alarming an un-emittable code creates a dead monitor that reads as a
//! healthy green alarm forever — the false-OK class this repo has retired
//! repeatedly (ws-reinject-01, tick-conserve-01, feed-stall-01). Those
//! codes are surfaced by [`report_critical_paging_coverage`] as retirement
//! candidates instead, and pinned in count by
//! [`no_emit_site_critical_codes_are_tracked_not_alarmed`].
//!
//! ## Honest limits (no false-OK about the checker itself)
//!
//! - Emit detection is the house heuristic shared with
//!   `error_code_paging_filter_drift_guard.rs`: comment-stripped,
//!   test-module-excised, whitespace-compacted source, with the nearest
//!   preceding tracing-macro opener required to be `error!(`. It matches
//!   the tag-guard convention `ErrorCode::<Variant>.code_str()` only — a
//!   raw-string-literal emit is invisible to it (and is separately banned
//!   by the tag guard).
//! - It proves an alarm ENTRY exists, not that the alarm ever fired in
//!   AWS. Terraform is the source of truth here; live behaviour is not
//!   assertable from this repo.
//! - A `Severity::Critical` code whose only reachable surface is a typed
//!   `NotificationEvent` is treated as covered ONLY if allowlisted with
//!   that reason — the guard cannot resolve event dispatch to codes
//!   mechanically, so those rows are human-asserted and cited.

#![cfg(test)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use tickvault_common::error_code::{ErrorCode, Severity};

// ---------------------------------------------------------------------
// The SHRINKING allowlist: Critical codes with a real ERROR-level emit
// site that are deliberately NOT alarmed. Every row states why. Removing
// a row (by adding the alarm) is the intended direction; adding a row
// requires the stated reason to be defensible in review.
// ---------------------------------------------------------------------
/// `(code_str, reason)`.
const UNWIRED_ALLOWLIST: &[(&str, &str)] = &[
    // --- Reaches a human via a typed NotificationEvent at the emit site.
    // Not duplicated as a CloudWatch alarm on cost discipline (~$0.10/mo
    // each; see aws-budget.md). Each cites the dispatch site.
    (
        "ORPHAN-POSITION-01",
        "reaches human: notify(NotificationEvent::OrphanPositionDetected) at the emit site \
         (crates/app/src/orphan_position_watchdog_boot.rs)",
    ),
    (
        "RESILIENCE-01",
        "reaches human: notify(NotificationEvent::DualInstanceDetected) at the emit site \
         (crates/app/src/dhan_rest_stack.rs)",
    ),
    (
        "RESILIENCE-03",
        "reaches human: notify(NotificationEvent::AuthenticationFailed) at the emit site \
         (crates/core/src/auth/token_manager.rs) — the Dhan noise-lock family-(3) page",
    ),
    (
        "OMS-GAP-03",
        "reaches human: NotificationEvent::CircuitBreakerOpened via the order-execution \
         family (dhan-rest-only-noise-lock-2026-07-14.md §2a)",
    ),
    // --- BLOCKED pending a dated operator quote. Alarm -> SNS -> Telegram,
    // so an alarm here IS a new Dhan-scoped page, which
    // dhan-rest-only-noise-lock-2026-07-14.md §3 REJECTs without a fresh
    // dated operator quote in THAT file first. Covering these means
    // landing the quote there, then deleting the row here.
    (
        "AUTH-GAP-01",
        "BLOCKED: Dhan-scoped — a new page needs a dated operator quote in \
         dhan-rest-only-noise-lock-2026-07-14.md §2 first (its §1 fixes the Dhan alert set \
         at 4 items)",
    ),
    (
        "DATA-805",
        "BLOCKED: Dhan-scoped (Dhan Data-API connection cap) — same §2/§3 noise-lock gate \
         as AUTH-GAP-01",
    ),
];

// ---------------------------------------------------------------------
// Path + IO helpers.
// ---------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/storage") // APPROVED: test
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

// ---------------------------------------------------------------------
// Pure parsers. Each is unit-tested against synthetic input below: a
// checker that cannot detect a PLANTED drift is itself a build failure.
// ---------------------------------------------------------------------

/// Strip `#` comments from an HCL body, STRING-AWARE: a `#` inside a
/// quoted string (descriptions contain them) is kept, so a commented-out
/// entry can never satisfy the coverage check.
fn strip_hcl_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let mut in_str = false;
        let mut esc = false;
        let mut cut = line.len();
        for (i, &b) in line.as_bytes().iter().enumerate() {
            if esc {
                esc = false;
                continue;
            }
            match b {
                b'\\' if in_str => esc = true,
                b'"' => in_str = !in_str,
                b'#' if !in_str => {
                    cut = i;
                    break;
                }
                _ => {}
            }
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

/// Every code string named by a `$.code = "X"` clause in a COMMENT-STRIPPED
/// terraform body, plus every bare-term filter value that is a valid code
/// (the DH-906 tripwire shape). This is deliberately looser than the
/// crates/common drift guard's full map parse: here we only need "is this
/// code covered by some entry", and being loose can only ever make this
/// guard *quieter*, never produce a false failure.
fn covered_codes(tf_stripped: &str) -> BTreeSet<String> {
    let valid: BTreeSet<&str> = ErrorCode::all().iter().map(|c| c.code_str()).collect();
    let mut out = BTreeSet::new();
    // `$.code = \"X\"` (HCL-escaped inside the pattern string).
    let mut rest = tf_stripped;
    while let Some(i) = rest.find("$.code = ") {
        rest = &rest[i + "$.code = ".len()..];
        let after = rest.strip_prefix("\\\"").or_else(|| rest.strip_prefix('"'));
        if let Some(a) = after
            && let Some(end) = a.find(['\\', '"'])
        {
            let code = &a[..end];
            if valid.contains(code) {
                out.insert(code.to_string());
            }
        }
    }
    // Bare term filters: `pattern = "\"DH-906\""`.
    for line in tf_stripped.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("pattern") {
            let v = v.trim_start().trim_start_matches('=').trim();
            // UNESCAPE FIRST, then strip the outer HCL quotes. Doing it the
            // other way round leaves a trailing `\` on the term shape
            // `"\"DH-906\""` (trim_matches eats BOTH closing quotes, then the
            // unescape has nothing to pair with) — caught by
            // `covered_codes_extracts_coded_and_term_filters_and_rejects_unknowns`.
            let unq = v.replace("\\\"", "\"");
            let unq = unq.trim().trim_matches('"').trim().to_string();
            if valid.contains(unq.as_str()) {
                out.insert(unq);
            }
        }
    }
    out
}

/// Strip Rust comments (line AND nested block), STRING-AWARE — a needle
/// inside a doc comment must never count as an emit site.
fn strip_rust_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let bytes = body.as_bytes();
    let mut i = 0usize;
    let mut in_str = false;
    let mut esc = false;
    let mut block_depth = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if block_depth > 0 {
            if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                block_depth += 1;
                i += 2;
                continue;
            }
            if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                block_depth -= 1;
                i += 2;
                continue;
            }
            if b == b'\n' {
                out.push('\n');
            }
            i += 1;
            continue;
        }
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            out.push(b as char);
            i += 1;
            continue;
        }
        match b {
            b'"' => {
                in_str = true;
                out.push('"');
                i += 1;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                block_depth = 1;
                i += 2;
            }
            _ => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out
}

fn is_test_cfg(attr: &str) -> bool {
    attr.starts_with("#[cfg(") && attr.contains("test") && !attr.contains("not(test)")
}

/// Skip a module body from its opening line through its matching brace,
/// char-level and string-aware. A body-less `mod tests;` skips one line.
fn skip_module(lines: &[&str], start: usize) -> usize {
    let mut depth = 0i32;
    let mut opened = false;
    let mut in_str = false;
    let mut esc = false;
    let mut k = start;
    while k < lines.len() {
        let line = lines[k];
        if !opened && !in_str && line.contains(';') && !line.contains('{') {
            return k + 1;
        }
        for b in line.bytes() {
            if esc {
                esc = false;
                continue;
            }
            match b {
                b'\\' if in_str => esc = true,
                b'"' => in_str = !in_str,
                b'{' if !in_str => {
                    depth += 1;
                    opened = true;
                }
                b'}' if !in_str => depth -= 1,
                _ => {}
            }
        }
        k += 1;
        if opened && depth <= 0 {
            return k;
        }
    }
    k
}

/// Excise every test-cfg-gated MODULE body, keeping production code both
/// before and after a mid-file test module (the 2026-07-14 lesson that
/// truncating at the first test module reported live emits as dead).
fn excise_test_modules(stripped: &str) -> String {
    let lines: Vec<&str> = stripped.split_inclusive('\n').collect();
    let mut out = String::with_capacity(stripped.len());
    let mut i = 0usize;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if is_test_cfg(trimmed) {
            let same_line_mod = trimmed
                .find(']')
                .map(|c| trimmed[c + 1..].trim_start().starts_with("mod "))
                .unwrap_or(false);
            if same_line_mod {
                i = skip_module(&lines, i);
                continue;
            }
            let mut j = i + 1;
            while j < lines.len() {
                let t = lines[j].trim();
                if t.is_empty() || t.starts_with("#[") {
                    j += 1;
                    continue;
                }
                break;
            }
            if j < lines.len() {
                let t = lines[j].trim();
                if t.starts_with("mod ")
                    || t.starts_with("pub mod ")
                    || (t.starts_with("pub(") && t.contains(" mod "))
                {
                    i = skip_module(&lines, j);
                    continue;
                }
            }
        }
        out.push_str(lines[i]);
        i += 1;
    }
    out
}

fn compact(body: &str) -> String {
    body.chars().filter(|c| !c.is_whitespace()).collect()
}

/// True iff `needle` appears in compacted production source with `error!(`
/// as the nearest preceding tracing-macro opener. A `warn!`/`info!`
/// reference can never fire an alarm whose filter pins
/// `$.level = "ERROR"`, so it must not count.
fn prod_has_error_level_needle(prod_compact: &str, needle: &str) -> bool {
    const OPENERS: &[&str] = &["error!(", "warn!(", "info!(", "debug!(", "trace!("];
    let mut search_from = 0usize;
    while let Some(rel) = prod_compact[search_from..].find(needle) {
        let idx = search_from + rel;
        let window = &prod_compact[idx.saturating_sub(800)..idx];
        let mut last: Option<(usize, &str)> = None;
        for opener in OPENERS {
            if let Some(pos) = window.rfind(opener)
                && last.map(|(p, _)| pos > p).unwrap_or(true)
            {
                last = Some((pos, opener));
            }
        }
        if matches!(last, Some((_, "error!("))) {
            return true;
        }
        search_from = idx + needle.len();
    }
    false
}

/// Collect production `.rs` sources under `crates/*/src`, excluding the
/// enum definition itself and src-resident test files gated at the parent
/// `mod` (they carry no in-file `#[cfg(test)]` header).
fn collect_prod_rs_sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    let crates_dir = workspace_root().join("crates");
    if let Ok(crates) = fs::read_dir(&crates_dir) {
        for c in crates.flatten() {
            let src = c.path().join("src");
            if src.is_dir() {
                walk(&src, &mut out);
            }
        }
    }
    out.retain(|p| {
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        !p.ends_with("common/src/error_code.rs")
            && stem != "tests"
            && stem != "test_support"
            && !stem.ends_with("_tests")
    });
    out
}

fn has_error_level_emit_site(variant_name: &str, sources: &[PathBuf]) -> bool {
    let needle = format!("ErrorCode::{variant_name}.code_str()");
    sources.iter().any(|path| {
        fs::read_to_string(path)
            .map(|body| {
                prod_has_error_level_needle(
                    &compact(&excise_test_modules(&strip_rust_comments(&body))),
                    &needle,
                )
            })
            .unwrap_or(false)
    })
}

fn critical_codes() -> Vec<ErrorCode> {
    ErrorCode::all()
        .iter()
        .copied()
        .filter(|c| c.severity() == Severity::Critical)
        .collect()
}

// ---------------------------------------------------------------------
// The ratchet assertions (real files).
// ---------------------------------------------------------------------

#[test]
fn every_critical_code_with_an_emit_site_is_alarmed_or_allowlisted() {
    let covered = covered_codes(&strip_hcl_comments(&read(
        "deploy/aws/terraform/error-code-alarms.tf",
    )));
    let sources = collect_prod_rs_sources();
    assert!(
        sources.len() > 50,
        "source-walk self-check: expected >50 production .rs files, got {}",
        sources.len()
    );
    assert!(
        !covered.is_empty(),
        "tf-parse self-check: extracted ZERO covered codes from error-code-alarms.tf — \
         the parser broke on a formatting change (a vacuous pass would hide every gap)"
    );

    let mut silent: Vec<String> = Vec::new();
    for code in critical_codes() {
        let cs = code.code_str();
        if covered.contains(cs) || UNWIRED_ALLOWLIST.iter().any(|(c, _)| *c == cs) {
            continue;
        }
        if has_error_level_emit_site(&format!("{code:?}"), &sources) {
            silent.push(format!("{cs} (variant {code:?})"));
        }
    }

    assert!(
        silent.is_empty(),
        "SILENT CRITICAL CODES: these Severity::Critical codes emit `error!` in production \
         but have NO `error_code_alerts` entry in deploy/aws/terraform/error-code-alarms.tf \
         and no UNWIRED_ALLOWLIST row — they page NOBODY, which is a silent failure by \
         definition (the 2026-07-06 zero-page incident class). Fix by EITHER adding a map \
         entry (mirror an existing one; ~$0.10/mo each — record it in aws-budget.md and add \
         the code to the 'Filtered+alarmed codes' list in \
         .claude/rules/project/observability-architecture.md, which \
         error_code_paging_filter_drift_guard.rs checks bidirectionally) OR adding an \
         UNWIRED_ALLOWLIST row stating why it cannot be alarmed. Silent: {silent:?}"
    );
}

#[test]
fn allowlist_never_grows() {
    // Shrinking ratchet. Covering a code = DELETE its row and lower this
    // number in the same PR. Raising it requires a stated reason in the
    // row itself and review sign-off.
    const MAX_UNWIRED: usize = 6;
    assert!(
        UNWIRED_ALLOWLIST.len() <= MAX_UNWIRED,
        "UNWIRED_ALLOWLIST grew to {} (ratchet ceiling {MAX_UNWIRED}) — this allowlist may \
         only SHRINK. Wire the code to an alarm instead of allowlisting around the gap.",
        UNWIRED_ALLOWLIST.len()
    );
}

#[test]
fn allowlist_rows_are_real_critical_codes_with_real_emit_sites() {
    // Anti-staleness in BOTH directions: an allowlist row for a code that
    // is no longer Critical, no longer exists, or no longer emits is a
    // stale excuse that would silently absorb a future real gap. A row
    // whose code IS alarmed is also stale (delete it — that is the
    // ratchet working).
    let sources = collect_prod_rs_sources();
    let critical: BTreeSet<&str> = critical_codes().iter().map(|c| c.code_str()).collect();
    let covered = covered_codes(&strip_hcl_comments(&read(
        "deploy/aws/terraform/error-code-alarms.tf",
    )));

    for (code, reason) in UNWIRED_ALLOWLIST {
        assert!(
            !reason.trim().is_empty(),
            "allowlist row `{code}` has an empty reason — every row must state why the code \
             cannot be alarmed"
        );
        let Some(variant) = ErrorCode::all().iter().find(|c| c.code_str() == *code) else {
            panic!("allowlist row `{code}` is not a valid ErrorCode::code_str() value"); // APPROVED: test
        };
        assert!(
            critical.contains(code),
            "allowlist row `{code}` is no longer Severity::Critical ({:?}) — this guard only \
             governs Critical codes; delete the stale row",
            variant.severity()
        );
        assert!(
            !covered.contains(*code),
            "allowlist row `{code}` is STALE: the code now HAS an alarm entry in \
             error-code-alarms.tf. Delete the row and lower MAX_UNWIRED in the same PR — \
             that is the shrinking ratchet doing its job."
        );
        assert!(
            has_error_level_emit_site(&format!("{variant:?}"), &sources),
            "allowlist row `{code}` names a code with NO ERROR-level emit site — it belongs \
             in the no-emit-site (retirement-candidate) set, not this allowlist. Either the \
             emit was deleted (retire the enum variant) or it was downgraded below error!."
        );
    }
}

#[test]
fn no_emit_site_critical_codes_are_tracked_not_alarmed() {
    // The MORE interesting finding: a Critical variant with a runbook and
    // NO emitter reads as coverage that does not exist. These must never
    // gain an alarm (dead monitor), and the count is pinned so the set
    // cannot silently grow — a NEW no-emit Critical means someone deleted
    // an emit site without retiring the variant.
    const MAX_NO_EMIT_CRITICAL: usize = 13;
    let sources = collect_prod_rs_sources();
    let covered = covered_codes(&strip_hcl_comments(&read(
        "deploy/aws/terraform/error-code-alarms.tf",
    )));

    let mut no_emit: Vec<String> = Vec::new();
    for code in critical_codes() {
        if !has_error_level_emit_site(&format!("{code:?}"), &sources) {
            no_emit.push(code.code_str().to_string());
            assert!(
                !covered.contains(code.code_str()),
                "DEAD MONITOR: Critical code `{}` has an alarm entry but NO ERROR-level emit \
                 site — the alarm can never fire and reads as a permanently-healthy green \
                 alarm (the ws-reinject-01 / tick-conserve-01 false-OK class). Retire the \
                 map entry, or restore the emit.",
                code.code_str()
            );
        }
    }

    assert!(
        no_emit.len() <= MAX_NO_EMIT_CRITICAL,
        "the set of Severity::Critical codes with NO production emit site GREW to {} \
         (ceiling {MAX_NO_EMIT_CRITICAL}): {no_emit:?}. A Critical variant with a runbook \
         and no emitter advertises coverage that does not exist. Either restore the emit \
         site or retire the enum variant + its rule-file entry. This ceiling may only go \
         DOWN.",
        no_emit.len()
    );
}

/// Not an assertion — the operator-facing classification table. Run with
/// `--ignored --nocapture` to regenerate it.
#[test]
#[ignore = "reporting aid, not an assertion — run with --ignored --nocapture"]
fn report_critical_paging_coverage() {
    let covered = covered_codes(&strip_hcl_comments(&read(
        "deploy/aws/terraform/error-code-alarms.tf",
    )));
    let sources = collect_prod_rs_sources();
    let crits = critical_codes();
    println!(
        "total variants={} critical={} alarm-covered codes={}",
        ErrorCode::all().len(),
        crits.len(),
        covered.len()
    );
    println!("| CODE | EMIT | ALARM | CLASS |");
    for code in crits {
        let cs = code.code_str();
        let emit = has_error_level_emit_site(&format!("{code:?}"), &sources);
        let alarm = covered.contains(cs);
        let class = match (emit, alarm) {
            (false, _) => "DEAD-NO-EMIT-SITE",
            (true, true) => "REACHES-HUMAN (alarm)",
            (true, false) if UNWIRED_ALLOWLIST.iter().any(|(c, _)| *c == cs) => {
                "REACHES-HUMAN/BLOCKED (allowlisted)"
            }
            (true, false) => "SILENT",
        };
        println!("| {cs} | {emit} | {alarm} | {class} |");
    }
}

// ---------------------------------------------------------------------
// Parser self-tests (anti-vacuity): a checker that cannot see a PLANTED
// drift is itself a build failure.
// ---------------------------------------------------------------------

#[test]
fn hcl_comment_stripper_hides_commented_entries_but_keeps_strings() {
    let src = "  # \"ghost\" = { pattern = \"{ $.code = \\\"PROC-01\\\" }\" }\n\
               \"real\" = { pattern = \"{ $.code = \\\"DH-901\\\" }\" } # trailing\n\
               desc = \"a # inside a string is kept\"\n";
    let stripped = strip_hcl_comments(src);
    assert!(
        !stripped.contains("ghost"),
        "commented entry survived: {stripped}"
    );
    assert!(stripped.contains("DH-901"), "real entry lost: {stripped}");
    assert!(
        stripped.contains("a # inside a string is kept"),
        "string-internal # was wrongly treated as a comment: {stripped}"
    );
}

#[test]
fn covered_codes_extracts_coded_and_term_filters_and_rejects_unknowns() {
    let src = "pattern = \"{ $.code = \\\"PROC-01\\\" && $.level = \\\"ERROR\\\" }\"\n\
               pattern = \"\\\"DH-906\\\"\"\n\
               pattern = \"{ $.code = \\\"NOT-A-REAL-CODE-99\\\" }\"\n";
    let got = covered_codes(src);
    assert!(got.contains("PROC-01"), "coded filter missed: {got:?}");
    assert!(got.contains("DH-906"), "term filter missed: {got:?}");
    assert!(
        !got.contains("NOT-A-REAL-CODE-99"),
        "a non-ErrorCode token was accepted as covered: {got:?}"
    );
}

#[test]
fn emit_detection_is_level_aware_and_ignores_comments_and_test_modules() {
    let needle = "ErrorCode::Proc01OomKillDetected.code_str()";

    let doc_only = "//! mention: error!(code = ErrorCode::Proc01OomKillDetected.code_str())\n\
                    /* block: error!(code = ErrorCode::Proc01OomKillDetected.code_str()) */\n\
                    fn f() {}\n";
    assert!(
        !prod_has_error_level_needle(
            &compact(&excise_test_modules(&strip_rust_comments(doc_only))),
            needle
        ),
        "a comment-only mention counted as an emit site"
    );

    let warn_level =
        "fn f() { warn!(code = ErrorCode::Proc01OomKillDetected.code_str(), \"x\"); }\n";
    assert!(
        !prod_has_error_level_needle(
            &compact(&excise_test_modules(&strip_rust_comments(warn_level))),
            needle
        ),
        "a warn!-level reference counted as an ERROR-level emit site"
    );

    let in_test_mod = "#[cfg(test)]\nmod tests {\n\
                       fn t() { error!(code = ErrorCode::Proc01OomKillDetected.code_str(), \"x\"); }\n}\n";
    assert!(
        !prod_has_error_level_needle(
            &compact(&excise_test_modules(&strip_rust_comments(in_test_mod))),
            needle
        ),
        "an emit inside a #[cfg(test)] module counted as production"
    );

    let real = "fn f() { error!(code = ErrorCode::Proc01OomKillDetected.code_str(), \"x\"); }\n\
                #[cfg(test)]\nmod tests { fn t() {} }\n";
    assert!(
        prod_has_error_level_needle(
            &compact(&excise_test_modules(&strip_rust_comments(real))),
            needle
        ),
        "a real production error! emit was NOT detected — the guard would pass vacuously"
    );

    let after_mid_file_test_mod = "#[cfg(test)]\nmod early { fn t() {} }\n\
                                   fn f() { error!(code = ErrorCode::Proc01OomKillDetected.code_str(), \"x\"); }\n";
    assert!(
        prod_has_error_level_needle(
            &compact(&excise_test_modules(&strip_rust_comments(
                after_mid_file_test_mod
            ))),
            needle
        ),
        "a production emit AFTER a mid-file test module was hidden (the 2026-07-14 regression)"
    );
}
