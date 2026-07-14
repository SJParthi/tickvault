//! Paging-filter DRIFT ratchet — the error-code paging chain's three
//! surfaces must never silently diverge (W2#4, 2026-07-10).
//!
//! The three surfaces:
//! 1. The `error_code_alerts` map in
//!    `deploy/aws/terraform/error-code-alarms.tf` (the filters + alarms
//!    that actually page — 9 entries as of 2026-07-09).
//! 2. The documented paging list in
//!    `.claude/rules/project/observability-architecture.md`
//!    ("Which codes page" → the "Filtered+alarmed codes" paragraph).
//! 3. The real `error!` emit sites in `crates/*/src` (the tag-guard
//!    convention: `code = ErrorCode::<Variant>.code_str()`).
//!
//! Drift classes this guard makes build-failing:
//! - a tf map entry whose pattern names a code string that is NOT a valid
//!   `ErrorCode::code_str()` value (typo'd filter — can never fire);
//! - a CODED tf entry with ZERO real emit sites (dead filter — the exact
//!   false-OK class of the 2026-07-06 zero-page incident, inverted);
//! - a tf entry missing from the doc list, or a doc-listed code missing
//!   from the tf map (bidirectional — the operator's "which codes page?"
//!   answer must be the terraform truth);
//! - a coded pattern that deviates from the errors.jsonl line shape
//!   `{ $.code = "X" && $.level = "ERROR" }` (the tracing JSON layer's
//!   `flatten_event(true)` top-level fields — a malformed pattern
//!   silently never matches);
//! - an ALLOWLISTED no-emit tripwire (DH-906) silently GAINING a coded
//!   emit site (the allowlist + the "no coded emit site exists yet" doc
//!   note + the term-match filter would all be stale — retire together).
//!
//! Known deliberate exception (the ONLY allowlist entry): `dh-906` is a
//! plain TERM filter (`"DH-906"`), not a coded JSON filter — documented
//! in error-code-alarms.tf ("DH-906 is a plain TERM filter, not a coded
//! JSON filter: zero coded emit sites exist in the codebase (verified
//! 2026-07-06 …)") and in observability-architecture.md ("DH-906
//! (term-match tripwire — no coded emit site exists yet)"). Each
//! allowlist row below cites those dated lines.
//!
//! House precedents: `error_code_rule_file_crossref.rs` (variant<->rule
//! cross-ref), `cloudwatch_app_alarms_wiring.rs` (alarm<->emit<->EMF
//! three-way drift check + comment-stripping anti-vacuity),
//! `crates/app/tests/seal_drop_paging_wiring_guard.rs` (string-aware HCL
//! comment stripper — reused here because a tf COMMENT mentioning a code
//! or a pattern must never satisfy or pollute the parse).

use std::fs;
use std::path::{Path, PathBuf};

use tickvault_common::error_code::ErrorCode;

// ---------------------------------------------------------------------
// The allowlist: codes whose tf entry is a TERM filter with (by design)
// NO coded emit site. Every row must cite the dated documentation lines
// that make the exception genuine. Adding a row requires the same dated
// documentation in BOTH files — never allowlist around real drift.
// ---------------------------------------------------------------------
/// (map key, code string) pairs. Citations:
/// - deploy/aws/terraform/error-code-alarms.tf: "# DH-906 is a plain TERM
///   filter, not a coded JSON filter: zero coded emit sites exist in the
///   codebase (verified 2026-07-06 …)" + the entry's own desc
///   ("pre-armed tripwire - no coded emit site exists").
/// - .claude/rules/project/observability-architecture.md "Which codes
///   page (2026-07-06)": "DH-906 (term-match tripwire — no coded emit
///   site exists yet)".
const TERM_TRIPWIRE_ALLOWLIST: &[(&str, &str)] = &[("dh-906", "DH-906")];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

// ---------------------------------------------------------------------
// Pure parsers (unit-tested against synthetic inputs below — the
// anti-vacuity contract: a checker that cannot detect a PLANTED drift
// is itself a build failure).
// ---------------------------------------------------------------------

/// Strip `#`-comments from an HCL body, STRING-AWARE: a `#` inside a
/// double-quoted string (e.g. a description's "drop #1") is kept; a
/// comment mentioning a code or a whole fake entry is removed before
/// parsing (mirror of `seal_drop_paging_wiring_guard.rs`).
fn strip_hcl_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let bytes = line.as_bytes();
        let mut in_str = false;
        let mut esc = false;
        let mut cut = line.len();
        for (i, &b) in bytes.iter().enumerate() {
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

/// Strip Rust comments — BOTH `//` line comments AND `/* … */` block
/// comments — STRING-AWARE: `//` or `/*` inside a string literal (e.g.
/// a URL `https://…` or an assertion needle) is kept as code. Being
/// string-aware makes the `://` special-case of the
/// `http_client_fallback_guard.rs` precedent unnecessary (URLs live in
/// strings), and closes the block-comment false-pass hole the
/// 2026-07-10 hostile review flagged (a needle inside `/* … */` in a
/// production region must never count as an emit site). Nested block
/// comments are handled by depth counting; char/lifetime `'` quoting is
/// not tracked (no needle contains one).
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
                out.push('\n'); // keep line structure
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
                // Line comment: skip to end of line (newline kept).
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

/// The production view of a Rust source file: comments stripped, then
/// every test-`cfg`-gated MODULE **excised** (its whole `{ … }` body
/// removed), everything else kept.
///
/// History of this function's shape:
/// - NOT a naive split at the first `#[cfg(test)]` token (the
///   seal_drop_paging_wiring_guard shape): real production files carry
///   early `#[cfg(test)]` items that are NOT modules — e.g.
///   `crates/core/src/websocket/connection.rs:20` gates a test-only
///   static near the TOP of the file, and a naive split there hid the
///   real WS-GAP-07 emit at line ~1853 (a FALSE "dead filter" found
///   while bringing this guard up on 2026-07-10).
/// - NOT a truncation at the first test-gated MODULE either (the
///   2026-07-10 shape of this function): `crates/app/src/
///   cross_verify_1m_boot.rs` carries a MID-FILE `#[cfg(test)] mod
///   start_decision_tests` at line ~134 with the real
///   CROSS-VERIFY-1M-01/-02 `error!` emits AFTER it — truncating there
///   reported both codes as dead filters (a FALSE dead found on
///   2026-07-14 while adding their paging entries). Test-gated modules
///   are now EXCISED via string-aware brace matching, so production
///   code before AND after a mid-file test module stays visible while
///   needles inside any test module never count.
///
/// A test-gated module is a `#[cfg(…test…)]` attribute line (NOT
/// `not(test)` — that gates PRODUCTION code) whose gated item is a
/// `mod` — same-line (`#[cfg(test)] mod tests {`) or as the next
/// non-attribute line, covering `#[cfg(all(test, feature = …))] mod`
/// too. Test-gated NON-module items (statics, fns, fields) stay in the
/// scanned region — at worst a few test-only items are visible, never
/// is a production emit hidden. Brace matching is string-aware (quotes +
/// escapes tracked); TWO documented residuals (2026-07-14 mutation
/// review): (a) char literals / lifetimes are not tracked (no real test
/// module carries an unbalanced brace char literal — same as the comment
/// stripper's `'` note); (b) RAW string literals (`r#"…"#`) are not
/// specially tracked — a raw string containing an ODD number of `"`
/// characters would desync the quote-parity state and could mis-scope a
/// module boundary (a possible silent false-ALIVE on the emit-site
/// check). No `.rs` file scanned by this guard currently carries an
/// odd-quote-parity raw string inside or around a test module; if one
/// ever appears, upgrade the scanner to a raw-string-aware lexer in the
/// same PR.
fn production_view(body: &str) -> String {
    excise_test_modules(&strip_rust_comments(body))
}

/// Remove every test-cfg-gated module body from COMMENT-STRIPPED Rust
/// source (see [`production_view`]).
fn excise_test_modules(stripped: &str) -> String {
    let lines: Vec<&str> = stripped.split_inclusive('\n').collect();
    let mut out = String::with_capacity(stripped.len());
    let mut i = 0usize;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if is_test_cfg(trimmed) {
            // Same-line form: `#[cfg(test)] mod tests {`
            let same_line_mod = trimmed
                .find(']')
                .map(|c| trimmed[c + 1..].trim_start().starts_with("mod "))
                .unwrap_or(false);
            if same_line_mod {
                i = skip_module(&lines, i);
                continue;
            }
            // Next-non-attribute-line form.
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

/// Skip from the module-opening line at `start` through its closing
/// brace; returns the index of the first line AFTER the module.
/// CHAR-LEVEL string-aware depth tracking (a per-LINE delta cannot see a
/// single-line `mod t { … }` whose braces balance to 0 on its own line —
/// found while pinning the 2026-07-14 mid-file-module regression). A
/// body-less `mod tests;` declaration skips just its own line. String
/// state persists across lines (Rust string literals may span lines).
fn skip_module(lines: &[&str], start: usize) -> usize {
    let mut depth = 0i32;
    let mut opened = false;
    let mut in_str = false;
    let mut esc = false;
    let mut k = start;
    while k < lines.len() {
        let line = lines[k];
        if !opened && !in_str && line.contains(';') && !line.contains('{') {
            return k + 1; // `mod tests;` — declaration only
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

fn compact(body: &str) -> String {
    body.chars().filter(|c| !c.is_whitespace()).collect()
}

/// String-aware per-line brace-depth delta for HCL (braces inside quoted
/// strings — every filter pattern contains `{`/`}` — must NOT count).
fn hcl_brace_delta(line: &str) -> i32 {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for b in line.bytes() {
        if esc {
            esc = false;
            continue;
        }
        match b {
            b'\\' if in_str => esc = true,
            b'"' => in_str = !in_str,
            b'{' if !in_str => depth += 1,
            b'}' if !in_str => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// Decode an HCL quoted string starting at `s[0] == '"'`: returns the
/// unescaped content (escape-aware — `\"` never terminates the scan).
fn scan_hcl_string(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut out = String::new();
    let mut esc = false;
    for &b in &bytes[1..] {
        if esc {
            out.push(b as char);
            esc = false;
            continue;
        }
        match b {
            b'\\' => esc = true,
            b'"' => return Some(out),
            _ => out.push(b as char),
        }
    }
    None
}

/// One parsed `error_code_alerts` map entry.
#[derive(Debug, PartialEq)]
struct AlertEntry {
    key: String,
    /// The pattern attribute's decoded string value (HCL `\"` unescaped
    /// to `"`), e.g. `{ $.code = "PROC-01" && $.level = "ERROR" }` or
    /// the bare term `"DH-906"`.
    pattern: String,
}

/// Parse the `error_code_alerts = { … }` map out of a COMMENT-STRIPPED
/// HCL body. Line-oriented with string-aware brace depth: an entry opens
/// at map-relative depth 1 with `"<key>" = {`; its `pattern` attribute is
/// read at entry depth.
fn parse_error_code_alerts(tf_stripped: &str) -> Vec<AlertEntry> {
    let mut entries = Vec::new();
    let mut lines = tf_stripped.lines();
    // Seek the map opener.
    let mut depth: i32 = 0;
    let mut found = false;
    for line in lines.by_ref() {
        if line.contains("error_code_alerts") && line.contains('=') && line.contains('{') {
            depth = hcl_brace_delta(line); // normally +1
            found = true;
            break;
        }
    }
    if !found {
        return entries;
    }
    let mut current_key: Option<String> = None;
    for line in lines {
        let trimmed = line.trim();
        if depth == 1 {
            // Entry opener: "key" = {   (multi-line, terraform-fmt form)
            if let Some(rest) = trimmed.strip_prefix('"')
                && let Some(end) = rest.find('"')
                && rest[end + 1..].trim_start().starts_with('=')
            {
                if trimmed.ends_with('{') {
                    current_key = Some(rest[..end].to_string());
                } else if hcl_brace_delta(trimmed) == 0 && rest[end + 1..].contains('{') {
                    // Single-line entry `"k" = { … }` (2026-07-10
                    // hostile-review MEDIUM: previously invisible —
                    // net brace delta 0 never entered entry depth, so
                    // a hand-added one-liner evaded every check).
                    // Best-effort pattern extraction; a one-liner
                    // WITHOUT an extractable pattern is pushed with an
                    // empty pattern → PatternShape::Malformed → the
                    // shape assertion fails LOUDLY (run terraform fmt).
                    let key = rest[..end].to_string();
                    let body_part = &rest[end + 1..];
                    let pattern = body_part
                        .find("pattern")
                        .map(|p| &body_part[p + "pattern".len()..])
                        .and_then(|after| {
                            let after = after.trim_start().strip_prefix('=')?.trim_start();
                            scan_hcl_string(after)
                        })
                        .unwrap_or_default();
                    entries.push(AlertEntry { key, pattern });
                }
            }
        } else if depth == 2
            && current_key.is_some()
            && let Some(rest) = trimmed.strip_prefix("pattern")
        {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim_start();
                // Value spans from the first `"` to the LAST `"` on the
                // line; interior quotes are HCL-escaped (`\"`).
                if let Some(first) = rest.find('"')
                    && let Some(last) = rest.rfind('"')
                    && last > first
                {
                    let raw = &rest[first + 1..last];
                    let decoded = raw.replace("\\\"", "\"");
                    entries.push(AlertEntry {
                        key: current_key.clone().unwrap_or_default(),
                        pattern: decoded,
                    });
                }
            }
        }
        depth += hcl_brace_delta(line);
        if depth <= 0 {
            break; // map closed
        }
        if depth == 1 {
            // An entry just closed; require a fresh opener for the next.
            if trimmed == "}" {
                current_key = None;
            }
        }
    }
    entries
}

/// The pinned errors.jsonl-shape classification of a filter pattern.
#[derive(Debug, PartialEq)]
enum PatternShape {
    /// `{ $.code = "X" && $.level = "ERROR" }` — the canonical coded
    /// JSON filter matching the tracing JSON layer's flattened
    /// top-level `code` + `level` fields.
    Coded(String),
    /// A bare quoted term (`"DH-906"`) — the tripwire class; only legal
    /// for allowlisted codes.
    Term(String),
    /// Anything else — malformed; would silently never match the
    /// errors.jsonl line shape.
    Malformed,
}

fn classify_pattern(pattern: &str) -> PatternShape {
    // Coded shape, pinned EXACTLY (spacing included — the 9 live entries
    // and the emit convention both use this byte shape).
    if let Some(rest) = pattern.strip_prefix("{ $.code = \"")
        && let Some(code_end) = rest.find('"')
    {
        let code = &rest[..code_end];
        if rest[code_end..] == *"\" && $.level = \"ERROR\" }" && !code.is_empty() {
            return PatternShape::Coded(code.to_string());
        }
        return PatternShape::Malformed;
    }
    // Bare term shape: "X" (exactly one quoted token, nothing else).
    if let Some(rest) = pattern.strip_prefix('"')
        && let Some(end) = rest.find('"')
        && rest[end + 1..].is_empty()
        && !rest[..end].is_empty()
    {
        return PatternShape::Term(rest[..end].to_string());
    }
    PatternShape::Malformed
}

/// Extract the documented paging-code list from the
/// observability-architecture.md "Which codes page" section: tokens in
/// the paragraph between "Filtered+alarmed codes" and "Everything else"
/// that are VALID `ErrorCode::code_str()` values (prose words never
/// match the code grammar + validity filter).
fn parse_doc_paging_codes(md: &str) -> Vec<String> {
    let section_start = match md.find("### Which codes page") {
        Some(i) => i,
        None => return Vec::new(),
    };
    let section = &md[section_start..];
    let para_start = match section.find("Filtered+alarmed codes") {
        Some(i) => i,
        None => return Vec::new(),
    };
    let para = &section[para_start..];
    let para_end = para.find("Everything else").unwrap_or(para.len());
    let para = &para[..para_end];
    let valid: Vec<&str> = ErrorCode::all().iter().map(|c| c.code_str()).collect();
    let mut out: Vec<String> = Vec::new();
    for token in para.split(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-')) {
        let token = token.trim_matches('-');
        if token.contains('-') && valid.contains(&token) && !out.iter().any(|t| t == token) {
            out.push(token.to_string());
        }
    }
    out
}

/// Map a code string to its `ErrorCode` variant's Debug name (the
/// greppable emit-site identifier), if the code is a real variant.
fn variant_name_for(code: &str) -> Option<String> {
    ErrorCode::all()
        .iter()
        .find(|c| c.code_str() == code)
        .map(|c| format!("{c:?}"))
}

/// Collect every `.rs` file under `crates/*/src` (emit sites live in
/// production source; `tests/` sibling dirs are excluded by only
/// walking `src`). Additionally excluded (2026-07-10 hostile-review
/// HIGH-2): `crates/common/src/error_code.rs` itself (it DEFINES
/// `code_str`), and src-RESIDENT test files that carry no in-file
/// `#[cfg(test)] mod` header because they are gated at the PARENT
/// `mod` declaration — `tests.rs`, `*_tests.rs`, `test_support.rs`
/// (e.g. `crates/trading/src/indicator/tests.rs`): a needle string
/// literal inside one of those must never fake an emit site.
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
    let Ok(crates) = fs::read_dir(&crates_dir) else {
        return out;
    };
    for c in crates.flatten() {
        let src = c.path().join("src");
        if src.is_dir() {
            walk(&src, &mut out);
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

/// True iff some production source contains an ERROR-LEVEL emit of the
/// variant: an `ErrorCode::<Variant>.code_str()` reference (the
/// tag-guard convention; matches bare and fully-qualified paths — the
/// needle is a suffix of both — across rustfmt wrapping, comments and
/// test regions stripped) whose NEAREST preceding tracing-macro opener
/// in the compacted text is `error!(`.
///
/// LEVEL-AWARENESS (2026-07-10 hostile-review HIGH-1): the tf filters
/// require `$.level = "ERROR"`, and errors.jsonl is ERROR-only — a
/// `warn!`-level reference (e.g. FEED-STALL-01's per-restart warn! in
/// groww_sidecar_supervisor.rs, which coexists with the storm error!)
/// can never fire the filter, so it must not count. Heuristic: for each
/// needle occurrence, the LAST `error!(`/`warn!(`/`info!(`/`debug!(`/
/// `trace!(` opener in the preceding 800 compacted chars must be
/// `error!(` (fields between the opener and the code field are
/// tolerated; 800 compact chars is ~an order of magnitude beyond any
/// real emit's opener-to-code-field distance).
fn has_error_level_emit_site(variant_name: &str, sources: &[PathBuf]) -> bool {
    let needle = format!("ErrorCode::{variant_name}.code_str()");
    for path in sources {
        let Ok(body) = fs::read_to_string(path) else {
            continue;
        };
        let prod = compact(&production_view(&body));
        if prod_has_error_level_needle(&prod, &needle) {
            return true;
        }
    }
    false
}

/// Pure core of [`has_error_level_emit_site`], unit-tested below.
fn prod_has_error_level_needle(prod_compact: &str, needle: &str) -> bool {
    const OPENERS: &[&str] = &["error!(", "warn!(", "info!(", "debug!(", "trace!("];
    let mut search_from = 0usize;
    while let Some(rel) = prod_compact[search_from..].find(needle) {
        let idx = search_from + rel;
        let window_start = idx.saturating_sub(800);
        let window = &prod_compact[window_start..idx];
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

fn live_tf_entries() -> Vec<AlertEntry> {
    let tf = strip_hcl_comments(&read("deploy/aws/terraform/error-code-alarms.tf"));
    parse_error_code_alerts(&tf)
}

// ---------------------------------------------------------------------
// The ratchet assertions (real files).
// ---------------------------------------------------------------------

#[test]
fn every_tf_pattern_extracts_a_valid_known_code_with_pinned_shape() {
    let entries = live_tf_entries();
    assert!(
        entries.len() >= 9,
        "parser self-check: expected >= 9 error_code_alerts entries, got {} — \
         either entries were removed (update this ratchet with a dated note) or \
         the parser broke on a formatting change: {entries:?}",
        entries.len()
    );
    for e in &entries {
        match classify_pattern(&e.pattern) {
            PatternShape::Coded(code) => {
                assert!(
                    variant_name_for(&code).is_some(),
                    "tf entry `{}` filter names code string `{code}` which is NOT a \
                     valid ErrorCode::code_str() value — the filter can never match \
                     a tag-guard-conformant emit (typo'd / renamed code = dead pager)",
                    e.key
                );
            }
            PatternShape::Term(code) => {
                assert!(
                    TERM_TRIPWIRE_ALLOWLIST.contains(&(e.key.as_str(), code.as_str())),
                    "tf entry `{}` uses a bare TERM filter (`\"{code}\"`) but is NOT \
                     on the documented tripwire allowlist — term filters are only \
                     legal for codes with a dated no-coded-emit-site note in BOTH \
                     error-code-alarms.tf and observability-architecture.md",
                    e.key
                );
                assert!(
                    variant_name_for(&code).is_some(),
                    "allowlisted term filter `{}` names `{code}` which is not a \
                     valid ErrorCode::code_str() value",
                    e.key
                );
            }
            PatternShape::Malformed => panic!(
                "tf entry `{}` pattern `{}` matches NEITHER the pinned coded shape \
                 `{{ $.code = \"X\" && $.level = \"ERROR\" }}` (the errors.jsonl \
                 flatten_event(true) line shape) NOR the allowlisted bare-term \
                 shape — a malformed pattern silently never fires",
                e.key, e.pattern
            ),
        }
    }
}

#[test]
fn every_coded_tf_entry_has_a_real_emit_site() {
    let entries = live_tf_entries();
    let sources = collect_prod_rs_sources();
    assert!(
        sources.len() > 50,
        "source-walk self-check: expected >50 production .rs files, got {}",
        sources.len()
    );
    let mut dead: Vec<String> = Vec::new();
    for e in &entries {
        if let PatternShape::Coded(code) = classify_pattern(&e.pattern) {
            let variant = variant_name_for(&code).unwrap_or_else(|| panic!("unknown code {code}")); // APPROVED: test
            if !has_error_level_emit_site(&variant, &sources) {
                dead.push(format!("{} (code {code}, variant {variant})", e.key));
            }
        }
    }
    assert!(
        dead.is_empty(),
        "DEAD PAGING FILTERS: these error_code_alerts entries have a coded JSON \
         filter but ZERO ERROR-LEVEL `error!(… ErrorCode::<Variant>.code_str() …)` \
         emit in any crates/*/src production region — the alarm can never fire \
         (the tf filter requires $.level = ERROR and errors.jsonl is ERROR-only; \
         a warn!-level reference does not count — the inverse of the 2026-07-06 \
         zero-page incident). Either the error! emit was removed/renamed/ \
         downgraded (restore it or retire the map entry + doc line with a dated \
         note), or the emit uses a raw string literal instead of the tag-guard \
         enum convention (convert it). Dead: {dead:?}"
    );
}

#[test]
fn allowlisted_tripwires_still_have_no_coded_emit_site() {
    // The DH-906 allowlist row is honest ONLY while zero coded emit
    // sites exist (the tf comment + doc line say exactly that, dated
    // 2026-07-06). If a coded emit lands, the term filter must be
    // converted to a coded filter and the allowlist row retired — this
    // test forces that cleanup instead of letting the docs go stale.
    let sources = collect_prod_rs_sources();
    for (key, code) in TERM_TRIPWIRE_ALLOWLIST {
        let variant = variant_name_for(code)
            .unwrap_or_else(|| panic!("allowlisted code {code} is not a real variant")); // APPROVED: test
        assert!(
            !has_error_level_emit_site(&variant, &sources),
            "allowlist row (`{key}`, `{code}`) is STALE: a coded emit site for \
             ErrorCode::{variant} now exists. Convert the tf term filter to the \
             coded `{{ $.code = … }}` shape, update the dated notes in \
             error-code-alarms.tf + observability-architecture.md, and retire \
             this allowlist row in the same PR."
        );
    }
}

#[test]
fn tf_map_and_doc_paging_list_agree_bidirectionally() {
    let entries = live_tf_entries();
    let mut tf_codes: Vec<String> = entries
        .iter()
        .filter_map(|e| match classify_pattern(&e.pattern) {
            PatternShape::Coded(c) | PatternShape::Term(c) => Some(c),
            PatternShape::Malformed => None,
        })
        .collect();
    tf_codes.sort();
    tf_codes.dedup();
    let mut doc_codes =
        parse_doc_paging_codes(&read(".claude/rules/project/observability-architecture.md"));
    doc_codes.sort();
    assert!(
        doc_codes.len() >= 9,
        "doc parser self-check: expected >= 9 codes in the 'Filtered+alarmed \
         codes' paragraph, got {doc_codes:?} — the section moved or the parser broke"
    );
    let missing_in_doc: Vec<&String> = tf_codes.iter().filter(|c| !doc_codes.contains(c)).collect();
    let missing_in_tf: Vec<&String> = doc_codes.iter().filter(|c| !tf_codes.contains(c)).collect();
    assert!(
        missing_in_doc.is_empty() && missing_in_tf.is_empty(),
        "PAGING LIST DRIFT between deploy/aws/terraform/error-code-alarms.tf and \
         observability-architecture.md 'Which codes page': in tf but not \
         documented = {missing_in_doc:?}; documented as filtered+alarmed but no \
         tf entry = {missing_in_tf:?}. Update BOTH in the same PR with a dated \
         note (append-only house style) — the doc list is the operator's answer \
         to 'which codes page?' and must be the terraform truth."
    );
}

// ---------------------------------------------------------------------
// Anti-vacuity: the checker functions must DETECT planted drift in
// synthetic inputs (a guard that cannot fail is a false-OK, Rule 11).
// ---------------------------------------------------------------------

const SYNTH_TF: &str = r#"
locals {
  # comment with a fake entry that must NOT parse:
  # "fake-in-comment" = { pattern = "{ $.code = \"FAKE-IN-COMMENT\" && $.level = \"ERROR\" }" }
  error_code_alerts = {
    "good-code" = {
      pattern     = "{ $.code = \"PROC-01\" && $.level = \"ERROR\" }"
      period      = 300
      ok_recovery = false
      desc        = "desc with brace { and hash # inside a string"
    }
    "term-entry" = {
      pattern = "\"DH-906\""
      desc    = "term tripwire"
    }
    "bad-shape" = {
      pattern = "{ $.code = \"PROC-01\" }"
    }
  }
}
resource "aws_cloudwatch_log_metric_filter" "error_code" {
  pattern = "{ $.code = \"NOT-IN-THE-MAP\" && $.level = \"ERROR\" }"
}
"#;

#[test]
fn synthetic_parser_finds_exactly_the_map_entries_and_decodes_patterns() {
    let entries = parse_error_code_alerts(&strip_hcl_comments(SYNTH_TF));
    let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(
        keys,
        vec!["good-code", "term-entry", "bad-shape"],
        "parser must find exactly the real map entries — not the comment-planted \
         fake, not resource blocks after the map: {entries:?}"
    );
    assert_eq!(
        entries[0].pattern, r#"{ $.code = "PROC-01" && $.level = "ERROR" }"#,
        "HCL escape decoding broke"
    );
    // Single-line entries must be VISIBLE (2026-07-10 hostile-review
    // MEDIUM: previously a one-liner evaded every check) — with a
    // pattern extracted when present, and an empty (→ Malformed → loud
    // failure) pattern when not.
    let single = "error_code_alerts = {\n\
                  \"one-liner\" = { pattern = \"{ $.code = \\\"PROC-01\\\" && $.level = \\\"ERROR\\\" }\" }\n\
                  \"one-liner-no-pattern\" = { period = 300 }\n\
                  }\n";
    let entries = parse_error_code_alerts(single);
    assert_eq!(
        entries.len(),
        2,
        "single-line entries must not be invisible: {entries:?}"
    );
    assert_eq!(
        entries[0].pattern,
        r#"{ $.code = "PROC-01" && $.level = "ERROR" }"#
    );
    assert_eq!(
        classify_pattern(&entries[1].pattern),
        PatternShape::Malformed,
        "a one-liner without an extractable pattern must classify Malformed (loud)"
    );
}

#[test]
fn synthetic_shape_classifier_detects_planted_drift() {
    assert_eq!(
        classify_pattern(r#"{ $.code = "PROC-01" && $.level = "ERROR" }"#),
        PatternShape::Coded("PROC-01".into())
    );
    assert_eq!(
        classify_pattern(r#""DH-906""#),
        PatternShape::Term("DH-906".into())
    );
    // Planted drift: missing the level clause → must classify Malformed.
    assert_eq!(
        classify_pattern(r#"{ $.code = "PROC-01" }"#),
        PatternShape::Malformed
    );
    // Planted drift: level != ERROR → Malformed (warn!-level lines never
    // reach the ERROR-only errors.jsonl sink).
    assert_eq!(
        classify_pattern(r#"{ $.code = "PROC-01" && $.level = "WARN" }"#),
        PatternShape::Malformed
    );
    // A typo'd code is Coded(...) but must fail validity:
    assert!(variant_name_for("PROC-99-TYPO").is_none());
    assert!(variant_name_for("PROC-01").is_some());
}

#[test]
fn synthetic_doc_parser_detects_a_missing_code() {
    let doc = "### Which codes page (2026-07-06)\n\nFiltered+alarmed codes: \
               REST-CANARY-01, DH-901, PROC-01. **Everything else** is log-sink-only.\n";
    let codes = parse_doc_paging_codes(doc);
    assert_eq!(codes, vec!["REST-CANARY-01", "DH-901", "PROC-01"]);
    // A tf set containing AGGREGATOR-DROP-01 vs this doc must diff:
    assert!(
        !codes.contains(&"AGGREGATOR-DROP-01".to_string()),
        "synthetic doc must NOT contain the planted-missing code"
    );
    // Prose words / dates / lowercase metric names never match:
    let noisy = "### Which codes page (2026-07-06)\n\n\
                 Filtered+alarmed codes: PROC-01 (added 2026-07-09, see \
                 tv_seal_writer_drain_total{kind=\"dropped\"} and SNS). \
                 **Everything else**";
    assert_eq!(parse_doc_paging_codes(noisy), vec!["PROC-01"]);
    // A doc with no section header parses to empty (and the live
    // assertion's >=9 self-check would fail loudly, never silently).
    assert!(parse_doc_paging_codes("no section here PROC-01").is_empty());
}

/// Run a synthetic source through the SAME pipeline the real detector
/// uses and ask whether the given variant counts as an error-level emit.
fn synth_detect(src: &str, variant: &str) -> bool {
    let prod = compact(&production_view(src));
    prod_has_error_level_needle(&prod, &format!("ErrorCode::{variant}.code_str()"))
}

#[test]
fn synthetic_emit_detector_ignores_comments_and_test_regions() {
    // Needle in a line comment, a block comment, or a #[cfg(test)]
    // module must not count; a real rustfmt-wrapped error! emit must.
    let src = "// mention: ErrorCode::Proc01OomKillDetected.code_str() in error!(\n\
               /* block mention: error!(code = ErrorCode::Proc01OomKillDetected.code_str()) */\n\
               fn real() { let _ = 1; }\n\
               #[cfg(test)]\n\
               mod tests { fn f() { tracing::error!(code = \
               ErrorCode::Proc01OomKillDetected.code_str(), \"x\"); } }\n";
    assert!(
        !synth_detect(src, "Proc01OomKillDetected"),
        "comment/test-region mentions leaked into the production scan"
    );
    let real = "fn emit() { tracing::error!(code = ErrorCode::Proc01OomKillDetected\n\
                .code_str(), \"x\"); }\n";
    assert!(
        synth_detect(real, "Proc01OomKillDetected"),
        "a real rustfmt-wrapped error! emit site must be found after compaction"
    );
    // Regression pin (2026-07-10, the WS-GAP-07 false-dead bug found
    // while bringing this guard up): an EARLY `#[cfg(test)]` on a
    // non-module item (connection.rs:20 gates a test-only static) must
    // NOT truncate the region — the real emit further down must still
    // be visible; the trailing `#[cfg(test)] mod tests` still must.
    let early_cfg = "#[cfg(test)]\n\
                     static TEST_ONLY: bool = true;\n\
                     fn emit() { tracing::error!(code = \
                     ErrorCode::Proc01OomKillDetected.code_str(), \"x\"); }\n\
                     #[cfg(test)]\n\
                     mod tests { fn f() { tracing::error!(code = \
                     ErrorCode::WsReinject01Aborted.code_str(), \"x\"); } }\n";
    assert!(
        synth_detect(early_cfg, "Proc01OomKillDetected"),
        "an early cfg(test)-gated STATIC must not hide the later real emit \
         (the WS-GAP-07 false-dead regression)"
    );
    assert!(
        !synth_detect(early_cfg, "WsReinject01Aborted"),
        "the trailing cfg(test) mod tests region must still be excluded"
    );
    // cfg(all(test, …)) module + same-line `#[cfg(test)] mod` forms also
    // truncate; cfg(not(test)) gates PRODUCTION and must NOT truncate
    // (2026-07-10 hostile-review MEDIUM).
    let all_form = "fn keep() {}\n\
                    #[cfg(all(test, feature = \"x\"))]\n\
                    mod t { fn f() { tracing::error!(code = \
                    ErrorCode::WsReinject01Aborted.code_str(), \"x\"); } }\n";
    assert!(!synth_detect(all_form, "WsReinject01Aborted"));
    let same_line = "#[cfg(test)] mod tests { fn f() { tracing::error!(code = \
                     ErrorCode::WsReinject01Aborted.code_str(), \"x\"); } }\n";
    assert!(!synth_detect(same_line, "WsReinject01Aborted"));
    let not_test = "#[cfg(not(test))]\n\
                    mod prod { fn f() { tracing::error!(code = \
                    ErrorCode::Proc01OomKillDetected.code_str(), \"x\"); } }\n";
    assert!(
        synth_detect(not_test, "Proc01OomKillDetected"),
        "cfg(not(test)) gates PRODUCTION code — it must never truncate"
    );
    // Regression pin (2026-07-14, the CROSS-VERIFY-1M false-dead found
    // while adding its paging entry): a MID-FILE cfg(test) module —
    // cross_verify_1m_boot.rs's `mod start_decision_tests` at ~line 134 —
    // must be EXCISED, never TRUNCATED AT: the real production emit AFTER
    // it must stay visible, while a needle INSIDE the mid-file module
    // still never counts. Both directions:
    let mid_file_mod = "fn head() {}\n\
                        #[cfg(test)]\n\
                        mod start_decision_tests { fn f() { tracing::error!(code = \
                        ErrorCode::WsReinject01Aborted.code_str(), \"x\"); } }\n\
                        fn emit() { tracing::error!(code = \
                        ErrorCode::Proc01OomKillDetected.code_str(), \"x\"); }\n";
    assert!(
        synth_detect(mid_file_mod, "Proc01OomKillDetected"),
        "a mid-file cfg(test) module must not hide the LATER production emit \
         (the 2026-07-14 CROSS-VERIFY-1M false-dead regression)"
    );
    assert!(
        !synth_detect(mid_file_mod, "WsReinject01Aborted"),
        "a needle inside a mid-file cfg(test) module must still be excised"
    );
    // Multi-line mid-file module + a body-less `mod x;` declaration:
    let multiline = "#[cfg(test)]\n\
                     mod t {\n\
                     fn f() {\n\
                     tracing::error!(code = ErrorCode::WsReinject01Aborted.code_str(), \"x\");\n\
                     }\n\
                     }\n\
                     mod real_decl;\n\
                     fn emit() { tracing::error!(code = \
                     ErrorCode::Proc01OomKillDetected.code_str(), \"x\"); }\n";
    assert!(synth_detect(multiline, "Proc01OomKillDetected"));
    assert!(!synth_detect(multiline, "WsReinject01Aborted"));
    let decl_only = "#[cfg(test)]\n\
                     mod tests;\n\
                     fn emit() { tracing::error!(code = \
                     ErrorCode::Proc01OomKillDetected.code_str(), \"x\"); }\n";
    assert!(
        synth_detect(decl_only, "Proc01OomKillDetected"),
        "a body-less `mod tests;` declaration must skip only its own line"
    );
}

#[test]
fn synthetic_emit_detector_is_level_aware() {
    // 2026-07-10 hostile-review HIGH-1: the tf filters require
    // $.level = "ERROR" and errors.jsonl is ERROR-only — a warn!-level
    // reference can never fire the filter and must NOT count as an
    // emit site, even when it is the ONLY reference in the file.
    let warn_only = "fn w() { tracing::warn!(code = \
                     ErrorCode::Proc01OomKillDetected.code_str(), \"x\"); }\n";
    assert!(
        !synth_detect(warn_only, "Proc01OomKillDetected"),
        "a warn!-level reference must not satisfy the ERROR-only filter check"
    );
    // The FEED-STALL-01 shape: warn! (per-restart) AND error! (storm)
    // coexist — the error! arm must satisfy; deleting it (leaving only
    // the warn!) must fail. Both directions:
    let both = "fn w() { tracing::warn!(code = \
                ErrorCode::Proc01OomKillDetected.code_str(), \"per-restart\"); }\n\
                fn e() { tracing::error!(code = \
                ErrorCode::Proc01OomKillDetected.code_str(), \"storm\"); }\n";
    assert!(
        synth_detect(both, "Proc01OomKillDetected"),
        "the coexisting error! arm must satisfy the check"
    );
    // Fields between the opener and the code field are tolerated:
    let fields_first = "fn e() { tracing::error!(target: \"x\", dropped = n, code = \
                        ErrorCode::Proc01OomKillDetected.code_str(), \"m\"); }\n";
    assert!(synth_detect(fields_first, "Proc01OomKillDetected"));
}
