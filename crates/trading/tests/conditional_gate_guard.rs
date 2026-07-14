//! Conditional & Multi Order alerts-gate guard — source-scan ratchets.
//!
//! The `/alerts/*` family (Conditional & Multi Order) is a DORMANT client
//! surface behind the hardcoded `alerts_gate_armed` OFF switch inside
//! `OrderApiClient` (`crates/trading/src/oms/api_client.rs`). This file pins
//! the invariants that must NEVER regress without a dated operator quote +
//! a `.claude/rules/dhan/conditional-trigger.md` edit FIRST:
//!
//! 1. The gate defaults DISARMED in the constructor.
//! 2. The only arm path is `#[cfg(test)]`-scoped (not compiled in prod) —
//!    enforced by a TOTAL census of EVERY `alerts_gate_armed` identifier
//!    site in the file (round-2 hardening): exactly ONE assignment (any
//!    RHS shape — literal, variable, or parameter — is counted; only the
//!    literal-`true` write inside the ATTACHED-`#[cfg(test)]` arm fn is
//!    allowed), exactly TWO colon sites (the `: bool` field decl + the
//!    `: false` constructor init — a `new_armed(.., armed)` parameterized
//!    init is counted and refused), zero compound assignments
//!    (`|=`/`&=`/`^=`), zero `&mut` borrows of the field.
//! 3. Every `/alerts` URL-building sender checks the gate BEFORE any
//!    URL/socket work. BOTH sides of the census count the SAME
//!    comment-stripped production text (a commented-out gate call can
//!    never balance the equality), the URL census is GENERAL (any
//!    `/alerts` string shape + any `DHAN_ALERTS_`-prefixed constant use),
//!    and the SENDER SET IS DERIVED from the code (every `pub async fn`
//!    whose body touches `/alerts` text or a `DHAN_ALERTS_*` constant)
//!    and pinned equal to `ALERTS_SENDER_FNS` — a NEW 7th sender cannot
//!    ship without editing this test and thereby entering the
//!    gate-before-HTTP ordering scan.
//! 4. NO production code calls any of the 6 sender fns (dormancy ratchet)
//!    — api_client.rs' OWN production region INCLUDED (round-2: its
//!    test-module call sites are excluded by the region split, never by a
//!    whole-file skip). The activation PR edits THIS test alongside the
//!    operator quote.
//! 5. The order-leg segment enums stay equities-only fail-closed.
//! 6. The `/alerts` paths have a single choke point (no rogue sender
//!    files). Within the allowlist: constants.rs is under a
//!    `DHAN_ALERTS_`-naming ratchet (every code line carrying `/alerts`
//!    must be a DHAN_ALERTS_*-prefixed constant, so a rogue-named
//!    constant cannot dodge test 3's census), and types.rs /
//!    conditional.rs may mention the paths in DOC COMMENTS ONLY.
//! 7. The scanner itself detects planted violations (vacuous-pass defense
//!    — the 2026-07-06 lesson).
//!
//! Pattern: `sandbox_enforcement_guard.rs` (source scanning, not runtime
//! probing — the gate's arm path is `#[cfg(test)]`-scoped, so text is the
//! honest evidence surface).
//!
//! HONEST ENVELOPE: these are text ratchets. They pin every regression
//! shape surfaced by the round-1/round-2 adversarial reviews (literal and
//! non-literal arms, parameterized constructors, compound assignments,
//! `&mut` borrows, new senders, new constants, comment-inflated counts);
//! they do NOT claim to stop deliberate obfuscation outside those shapes
//! (byte-assembled strings, `unsafe` pointer writes) — such code fails
//! human review + the operator-quote protocol, not this file.

use std::fs;
use std::path::{Path, PathBuf};

const API_CLIENT_RS: &str = "src/oms/api_client.rs";
const CONDITIONAL_RS: &str = "src/oms/conditional.rs";
/// Workspace crates dir, relative to the crate root (test cwd = crates/trading).
const WORKSPACE_CRATES_DIR: &str = "../../crates";

/// The six `/alerts/*` sender fns (5 Phase-6 + place_multi_order 2026-07-14).
const ALERTS_SENDER_FNS: [&str; 6] = [
    "create_conditional_trigger",
    "modify_conditional_trigger",
    "delete_conditional_trigger",
    "get_conditional_trigger",
    "get_all_conditional_triggers",
    "place_multi_order",
];

/// Segment literals that must NEVER appear in conditional.rs' production
/// region (fail-closed equities/indices-only lock).
const FORBIDDEN_SEGMENT_LITERALS: [&str; 4] = ["NSE_FNO", "BSE_FNO", "MCX_COMM", "NSE_COMM"];

// ---------------------------------------------------------------------------
// Scanner primitives (self-tested by test 7)
// ---------------------------------------------------------------------------

/// Returns the PRODUCTION region of a source file: everything before the
/// `#[cfg(test)]\nmod tests` module marker. Scanning the whole file would let
/// assertion literals inside test modules satisfy (or violate) the scan
/// vacuously — the 2026-07-06 shadow-writer lesson. (The MODULE marker, not
/// a bare `#[cfg(test)]`, is the split point: api_client.rs legitimately
/// carries a `#[cfg(test)]`-scoped arm fn in its production region.) A file
/// with no test module is scanned whole (conservative for violation hunts).
fn production_region(source: &str) -> &str {
    match source.find("#[cfg(test)]\nmod tests") {
        Some(index) => &source[..index],
        None => source,
    }
}

/// Counts non-overlapping occurrences of `needle` in `haystack`.
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// Strips FULL-LINE comments (`//` / `///` / `//!` lines) so doc-comment
/// mentions of `/alerts` never count as URL sites. Deliberately keeps
/// inline TRAILING comments: a naive `//`-to-EOL strip would corrupt
/// `://` inside string literals (the http_client_fallback_guard lesson),
/// and keeping more text is conservative for a violation hunt (an
/// over-count fails the equality LOUDLY, never silently).
fn strip_full_line_comments(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extracts the body region of a sender fn: from its `pub async fn` token
/// to the next `\n    pub ` item (or end of region). Doc comments of the
/// FOLLOWING item may trail in — harmless for ordering scans (they carry
/// no `self.http.` / `format!(` tokens).
fn sender_fn_region<'a>(production: &'a str, fn_name: &str) -> &'a str {
    let decl = format!("pub async fn {fn_name}(");
    let start = production
        .find(&decl)
        .unwrap_or_else(|| panic!("sender fn {fn_name} must exist in api_client.rs"));
    let after = &production[start + decl.len()..];
    let end = after.find("\n    pub ").unwrap_or(after.len());
    &after[..end]
}

/// True when the production region of `source` calls any alerts sender fn
/// (method-call token `fn_name(`).
fn production_region_calls_alerts_sender(source: &str) -> Option<&'static str> {
    let production = production_region(source);
    ALERTS_SENDER_FNS.iter().find_map(|fn_name| {
        let call_token = format!(".{fn_name}(");
        if production.contains(&call_token) {
            Some(*fn_name)
        } else {
            None
        }
    })
}

/// Recursively collects every `.rs` file under `dir` whose path contains
/// `/src/` (production trees only — `tests/` and `benches/` dirs excluded).
fn collect_src_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if name == "target" || name == "tests" || name == "benches" {
                continue;
            }
            collect_src_rs_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs")
            && path.to_string_lossy().contains("/src/")
        {
            out.push(path);
        }
    }
}

/// Extracts the body of the FIRST `pub enum <name> {` block (unit-variant
/// enums only — first closing brace terminates the body).
fn enum_body<'a>(source: &'a str, enum_name: &str) -> &'a str {
    let decl = format!("pub enum {enum_name} {{");
    let start = source
        .find(&decl)
        .unwrap_or_else(|| panic!("enum {enum_name} declaration must exist"));
    let body_start = start + decl.len();
    let body_len = source[body_start..]
        .find('}')
        .unwrap_or_else(|| panic!("enum {enum_name} body must close"));
    &source[body_start..body_start + body_len]
}

/// The gate field identifier — the census subject of test 2.
const GATE_FIELD_IDENT: &str = "alerts_gate_armed";

/// How a `alerts_gate_armed` identifier site uses the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateSiteKind {
    /// `alerts_gate_armed = <rhs>` / `|=` / `&=` / `^=` — a WRITE.
    Assignment,
    /// `alerts_gate_armed: <rhs>` — field declaration or struct-literal init.
    FieldColon,
    /// `&mut …alerts_gate_armed` — a mutable borrow (mutation vector, e.g.
    /// `mem::replace(&mut self.alerts_gate_armed, true)`).
    MutBorrow,
    /// Everything else (`if self.alerts_gate_armed {`, `== other`, asserts).
    Read,
}

/// One classified occurrence of [`GATE_FIELD_IDENT`].
struct GateSite {
    /// Byte index of the identifier in the scanned source.
    index: usize,
    /// Usage class.
    kind: GateSiteKind,
    /// First identifier-ish token of the RHS (`true`, `false`, `bool`,
    /// `on`, `armed`, `self`, …). Empty for reads/borrows.
    rhs: String,
}

/// First identifier-like token of `text` (post-whitespace).
fn first_rhs_token(text: &str) -> String {
    text.trim_start()
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect()
}

/// True when the identifier at `index` is reached through a `&mut ` borrow
/// (only path segments / whitespace between the `&mut` and the field).
fn preceded_by_mut_borrow(source: &str, index: usize) -> bool {
    let mut window_start = index.saturating_sub(48);
    while !source.is_char_boundary(window_start) {
        window_start += 1;
    }
    let window = &source[window_start..index];
    match window.rfind("&mut ") {
        Some(position) => window[position + "&mut ".len()..].chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character == '_'
                || character == '.'
                || character.is_whitespace()
        }),
        None => false,
    }
}

/// TOTAL census of every `alerts_gate_armed` identifier site in `source`
/// (identifier-boundary checked, so `alerts_gate_armed_x` never counts).
/// Classification is by the token FOLLOWING the identifier, so a
/// non-literal write (`= on`, `: armed`, `|= flag`) is counted exactly like
/// a literal one — the round-2 hardening the literal-`true` needles lacked.
fn classify_gate_sites(source: &str) -> Vec<GateSite> {
    let bytes = source.as_bytes();
    let mut sites = Vec::new();
    for (index, _) in source.match_indices(GATE_FIELD_IDENT) {
        let boundary_before = index == 0 || {
            let before = bytes[index - 1] as char;
            !(before.is_ascii_alphanumeric() || before == '_')
        };
        let after = index + GATE_FIELD_IDENT.len();
        let boundary_after = after >= bytes.len() || {
            let following = bytes[after] as char;
            !(following.is_ascii_alphanumeric() || following == '_')
        };
        if !boundary_before || !boundary_after {
            continue;
        }
        if preceded_by_mut_borrow(source, index) {
            sites.push(GateSite {
                index,
                kind: GateSiteKind::MutBorrow,
                rhs: String::new(),
            });
            continue;
        }
        let rest = source[after..].trim_start();
        let (kind, rhs_text) = if rest.starts_with("==") || rest.starts_with("::") {
            (GateSiteKind::Read, "")
        } else if let Some(tail) = ["|=", "&=", "^="]
            .iter()
            .find_map(|operator| rest.strip_prefix(operator))
        {
            (GateSiteKind::Assignment, tail)
        } else if let Some(tail) = rest.strip_prefix('=') {
            if tail.starts_with('>') {
                // `=>` match arm — a read position.
                (GateSiteKind::Read, "")
            } else {
                (GateSiteKind::Assignment, tail)
            }
        } else if let Some(tail) = rest.strip_prefix(':') {
            (GateSiteKind::FieldColon, tail)
        } else {
            (GateSiteKind::Read, "")
        };
        sites.push(GateSite {
            index,
            kind,
            rhs: first_rhs_token(rhs_text),
        });
    }
    sites
}

/// Derives the alerts-sender fn set from `code` (comment-stripped
/// production text): every `pub async fn` whose body region touches
/// `/alerts` text or a `DHAN_ALERTS_`-prefixed constant. Returns sorted,
/// deduped names — pinned against [`ALERTS_SENDER_FNS`] so a NEW sender
/// cannot ship without editing this test.
fn derive_alerts_sender_fns(code: &str) -> Vec<String> {
    const DECL: &str = "pub async fn ";
    let mut names: Vec<String> = Vec::new();
    let mut search_from = 0;
    while let Some(position) = code[search_from..].find(DECL) {
        let name_start = search_from + position + DECL.len();
        let name: String = code[name_start..]
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .collect();
        search_from = name_start;
        if name.is_empty() {
            continue;
        }
        let region = sender_fn_region(code, &name);
        if region.contains("/alerts") || region.contains("DHAN_ALERTS_") {
            names.push(name);
        }
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// Lines of (comment-stripped) `code` that mention `/alerts` WITHOUT being
/// a `DHAN_ALERTS_`-prefixed constant — the constants.rs naming ratchet
/// (a rogue-named constant carrying the path would be invisible to the
/// api_client.rs census, which counts `DHAN_ALERTS_` by prefix).
fn alerts_naming_violations(code: &str) -> Vec<String> {
    code.lines()
        .filter(|line| line.contains("/alerts") && !line.contains("DHAN_ALERTS_"))
        .map(|line| line.trim().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Gate defaults DISARMED in the constructor
// ---------------------------------------------------------------------------

#[test]
fn test_alerts_gate_defaults_disarmed_in_constructor() {
    let source = fs::read_to_string(API_CLIENT_RS)
        .expect("api_client.rs must be readable for the alerts-gate guard");
    let new_start = source
        .find("pub fn new(")
        .expect("OrderApiClient::new must exist");
    let window_end = (new_start + 800).min(source.len());
    let constructor_window = &source[new_start..window_end];
    assert!(
        constructor_window.contains("alerts_gate_armed: false"),
        "OrderApiClient::new must initialize `alerts_gate_armed: false` — the \
         /alerts family ships DISARMED. Arming the default requires a dated \
         operator quote + a .claude/rules/dhan/conditional-trigger.md edit FIRST."
    );
}

// ---------------------------------------------------------------------------
// 2. The only arm path is #[cfg(test)]-scoped
// ---------------------------------------------------------------------------

#[test]
fn test_alerts_gate_arm_is_cfg_test_only() {
    let source = fs::read_to_string(API_CLIENT_RS)
        .expect("api_client.rs must be readable for the alerts-gate guard");
    let arm_decl = source
        .find("fn arm_alerts_gate_for_test")
        .expect("arm_alerts_gate_for_test must exist (the test-only arm path)");

    // The #[cfg(test)] attribute must be ATTACHED to the arm fn: walk the
    // lines immediately above the declaration line — only attribute/comment
    // lines may intervene, and one of them must be exactly `#[cfg(test)]`
    // (a stray attribute elsewhere in a 400-char window is NOT accepted —
    // the round-2 hardening of the proximity check).
    let decl_line_start = source[..arm_decl]
        .rfind('\n')
        .map_or(0, |position| position + 1);
    let mut attached_cfg_test = false;
    let mut cursor = decl_line_start;
    while cursor > 0 {
        let previous_line_start = source[..cursor - 1]
            .rfind('\n')
            .map_or(0, |position| position + 1);
        let line = source[previous_line_start..cursor].trim();
        if line == "#[cfg(test)]" {
            attached_cfg_test = true;
            break;
        }
        if line.starts_with("#[") || line.starts_with("//") {
            cursor = previous_line_start;
            continue;
        }
        break;
    }
    assert!(
        attached_cfg_test,
        "arm_alerts_gate_for_test must carry an ATTACHED #[cfg(test)] \
         attribute (only attribute/comment lines may sit between it and the \
         declaration) — a production arm path for the /alerts gate is \
         FORBIDDEN without a dated operator quote."
    );

    // TOTAL identifier census (whole file, test module included — the arm
    // fn is the ONLY write allowed anywhere). Classification is by the
    // token AFTER the identifier, so a non-literal write (`= on`,
    // `: armed`, `|= flag`, `&mut …`) is counted exactly like a literal
    // one — the round-1 literal-`true` needles missed those shapes.
    let sites = classify_gate_sites(&source);
    assert!(
        !sites.is_empty(),
        "the census must see the alerts_gate_armed field (rename requires \
         updating this guard in the same PR)"
    );

    let assignments: Vec<&GateSite> = sites
        .iter()
        .filter(|site| site.kind == GateSiteKind::Assignment)
        .collect();
    assert_eq!(
        assignments.len(),
        1,
        "exactly ONE `alerts_gate_armed = …` assignment (ANY right-hand \
         side — literal, variable, parameter, or compound `|=`/`&=`/`^=`) \
         may exist in the whole file; a production setter like \
         `set_alerts_gate(on)` is a forbidden arm path requiring a dated \
         operator quote + a .claude/rules/dhan/conditional-trigger.md edit \
         FIRST. Found {}",
        assignments.len()
    );
    assert_eq!(
        assignments[0].rhs, "true",
        "the single assignment must be the literal `= true` inside \
         arm_alerts_gate_for_test — found RHS token `{}`",
        assignments[0].rhs
    );
    assert!(
        assignments[0].index > arm_decl && assignments[0].index < arm_decl + 400,
        "the single `alerts_gate_armed = true` assignment must live inside \
         arm_alerts_gate_for_test's body"
    );

    // Colon sites: exactly the `: bool` field DECLARATION and the `: false`
    // constructor init. ANY other field-init — `: true`, `:true`, OR a
    // parameterized `alerts_gate_armed: armed` (`new_armed(.., armed)`
    // constructor) — is a forbidden arm path.
    let mut colon_rhs: Vec<&str> = sites
        .iter()
        .filter(|site| site.kind == GateSiteKind::FieldColon)
        .map(|site| site.rhs.as_str())
        .collect();
    colon_rhs.sort_unstable();
    assert_eq!(
        colon_rhs,
        ["bool", "false"],
        "exactly TWO `alerts_gate_armed:` sites may exist — the `: bool` \
         field declaration and the `: false` init in OrderApiClient::new. \
         Any other field init (literal OR variable RHS) is a production \
         arm path requiring a dated operator quote + a \
         .claude/rules/dhan/conditional-trigger.md edit FIRST"
    );

    // No mutable borrow of the field anywhere (`mem::replace`,
    // `&mut client.alerts_gate_armed`, …).
    assert!(
        sites
            .iter()
            .all(|site| site.kind != GateSiteKind::MutBorrow),
        "no `&mut …alerts_gate_armed` borrow may exist — a mutable borrow \
         is an arm vector (e.g. mem::replace) requiring a dated operator \
         quote FIRST"
    );
}

// ---------------------------------------------------------------------------
// 3. Every /alerts URL-building sender checks the gate
// ---------------------------------------------------------------------------

#[test]
fn test_every_alerts_sender_checks_gate_first() {
    let source = fs::read_to_string(API_CLIENT_RS)
        .expect("api_client.rs must be readable for the alerts-gate guard");
    let production = production_region(&source);

    // BOTH census sides count the SAME comment-stripped text (round-2: a
    // full-line comment containing `self.require_alerts_gate(` must never
    // inflate the gate side and balance a genuinely ungated URL site).
    let code = strip_full_line_comments(production);

    let gate_calls = count_occurrences(&code, "self.require_alerts_gate(");
    assert_eq!(
        gate_calls,
        ALERTS_SENDER_FNS.len(),
        "exactly {} require_alerts_gate call sites must exist (one per \
         sender) — a NEW sender must be added to ALERTS_SENDER_FNS in this \
         test alongside a dated operator quote; found {gate_calls}",
        ALERTS_SENDER_FNS.len()
    );

    // URL-building census — GENERAL, not needle-per-shape: after stripping
    // full-line comments, any `/alerts` text left in production code can
    // only live inside a string literal (inline `"{}/alerts`, captured
    // `"{base}/alerts`, and split-argument `"/alerts/..."` shapes all
    // match), plus any `DHAN_ALERTS_`-PREFIXED constant use (so a new
    // constant like DHAN_ALERTS_ORDERS_PATH is counted, not just the one
    // known name). The gate's own refusal message is the single allowed
    // non-URL `/alerts` mention and is excluded by its exact prefix.
    let gate_refusal_mentions = count_occurrences(&code, "alerts gate DISARMED: /alerts");
    assert_eq!(
        gate_refusal_mentions, 1,
        "exactly ONE `alerts gate DISARMED: /alerts` refusal message may \
         exist (inside require_alerts_gate) — a second copy could hide a \
         URL site from the census"
    );
    let literal_url_sites = count_occurrences(&code, "/alerts") - gate_refusal_mentions;
    let constant_url_sites = count_occurrences(&code, "DHAN_ALERTS_");
    let url_sites = literal_url_sites + constant_url_sites;
    assert_eq!(
        url_sites, gate_calls,
        "every /alerts URL-building site ({url_sites}) must be matched by a \
         require_alerts_gate call ({gate_calls}) — an ungated /alerts sender \
         cannot be added silently (any string shape, any DHAN_ALERTS_* \
         constant)"
    );

    // The sender set is DERIVED from the code, never assumed (round-2): a
    // NEW `pub async fn` touching `/alerts` text or a DHAN_ALERTS_*
    // constant — even one that balances the count equality with its own
    // late gate call — fails this set comparison until it is added to
    // ALERTS_SENDER_FNS, which puts it under the ordering scan below.
    let derived_senders = derive_alerts_sender_fns(&code);
    let mut expected_senders: Vec<String> = ALERTS_SENDER_FNS
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    expected_senders.sort_unstable();
    assert_eq!(
        derived_senders, expected_senders,
        "the DERIVED /alerts sender set must equal ALERTS_SENDER_FNS — a \
         new/renamed sender requires editing this test alongside a dated \
         operator quote (and thereby enters the gate-before-HTTP ordering \
         scan)"
    );

    // ORDERING: in every DERIVED sender fn body (comment-stripped) the gate
    // check must precede the FIRST URL/socket token — `require_alerts_gate`
    // doc contract "Checked BEFORE any URL/socket work". A gate call after
    // `.send().await` would otherwise satisfy the count equality.
    for fn_name in &derived_senders {
        let region = sender_fn_region(&code, fn_name);
        let gate = region
            .find("self.require_alerts_gate(")
            .unwrap_or_else(|| panic!("{fn_name} must call require_alerts_gate"));
        let http = region
            .find("self.http.")
            .unwrap_or_else(|| panic!("{fn_name} must perform HTTP via self.http"));
        assert!(
            gate < http,
            "{fn_name}: require_alerts_gate must precede the first \
             self.http. token (gate BEFORE socket work)"
        );
        if let Some(url_build) = region.find("format!(") {
            assert!(
                gate < url_build,
                "{fn_name}: require_alerts_gate must precede the first \
                 format!( token (gate BEFORE URL work)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Dormancy ratchet — zero production callers of the 6 sender fns
// ---------------------------------------------------------------------------

#[test]
fn test_no_production_caller_of_alerts_sender_fns() {
    let crates_dir = Path::new(WORKSPACE_CRATES_DIR);
    assert!(
        crates_dir.is_dir(),
        "workspace crates dir must be reachable from crates/trading"
    );
    let mut files = Vec::new();
    collect_src_rs_files(crates_dir, &mut files);
    assert!(
        files.len() > 50,
        "scanner must see the workspace production tree (found {} files)",
        files.len()
    );

    let mut violations = Vec::new();
    for file in files {
        let path_text = file.to_string_lossy().replace('\\', "/");
        // api_client.rs is deliberately NOT skipped (round-2 hardening):
        // its sender DECLARATIONS never match the `.name(` call token, and
        // its test-module call sites are excluded by the production-region
        // split — so an in-file production wrapper (`autofire()` calling
        // `self.place_multi_order(..)`) is caught like any other caller.
        let Ok(source) = fs::read_to_string(&file) else {
            continue;
        };
        if let Some(fn_name) = production_region_calls_alerts_sender(&source) {
            violations.push(format!("{path_text}: calls {fn_name}("));
        }
    }
    assert!(
        violations.is_empty(),
        "the /alerts surface is DORMANT — no production code may call the \
         sender fns. The activation PR must edit THIS test alongside a dated \
         operator quote. Violations:\n{}",
        violations.join("\n")
    );
}

// ---------------------------------------------------------------------------
// 5. Segment enums stay fail-closed (equities-only legs)
// ---------------------------------------------------------------------------

#[test]
fn test_leg_segment_enums_are_fail_closed() {
    let source = fs::read_to_string(CONDITIONAL_RS)
        .expect("conditional.rs must be readable for the segment-lock guard");
    let production = production_region(&source);

    // Order-leg segments: equities ONLY.
    let leg_body = enum_body(production, "ConditionalLegSegment");
    assert!(leg_body.contains("NseEq"), "leg enum must keep NseEq");
    assert!(leg_body.contains("BseEq"), "leg enum must keep BseEq");
    for forbidden in ["IdxI", "Fno", "Comm", "Currency"] {
        assert!(
            !leg_body.contains(forbidden),
            "ConditionalLegSegment must NOT gain a `{forbidden}` variant — \
             widening requires a dated operator quote + \
             .claude/rules/dhan/conditional-trigger.md rule edit FIRST"
        );
    }

    // Condition segments: exactly the three docs-verbatim variants.
    let condition_body = enum_body(production, "ConditionalSegment");
    for required in ["NseEq", "BseEq", "IdxI"] {
        assert!(
            condition_body.contains(required),
            "ConditionalSegment must keep the docs-verbatim `{required}` variant"
        );
    }
    for forbidden in ["Fno", "Comm", "Currency"] {
        assert!(
            !condition_body.contains(forbidden),
            "ConditionalSegment must NOT gain a `{forbidden}` variant"
        );
    }

    // The forbidden wire literals must appear NOWHERE in the production
    // region (not even in doc comments — raw source text is the ratchet).
    for literal in FORBIDDEN_SEGMENT_LITERALS {
        assert!(
            !production.contains(literal),
            "the literal `{literal}` must not appear in conditional.rs' \
             production region (fail-closed segment lock)"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. Single choke point for the /alerts paths
// ---------------------------------------------------------------------------

#[test]
fn test_alerts_paths_single_choke_point() {
    let crates_dir = Path::new(WORKSPACE_CRATES_DIR);
    let mut files = Vec::new();
    collect_src_rs_files(crates_dir, &mut files);

    // The ONLY production files allowed to mention the /alerts paths:
    // constants.rs (the path constant), api_client.rs (the 6 senders),
    // types.rs + conditional.rs (endpoint doc comments).
    let allowlist = [
        "common/src/constants.rs",
        "trading/src/oms/api_client.rs",
        "trading/src/oms/types.rs",
        "trading/src/oms/conditional.rs",
    ];

    let mut violations = Vec::new();
    for file in files {
        let path_text = file.to_string_lossy().replace('\\', "/");
        let Ok(source) = fs::read_to_string(&file) else {
            continue;
        };
        let mentions_alerts = source.contains("alerts/orders") || source.contains("alerts/multi");
        if !mentions_alerts {
            continue;
        }
        if !allowlist.iter().any(|allowed| path_text.ends_with(allowed)) {
            violations.push(path_text);
        }
    }
    assert!(
        violations.is_empty(),
        "the /alerts paths have a single gated choke point — no new file may \
         mention alerts/orders or alerts/multi. Violations:\n{}",
        violations.join("\n")
    );

    // Round-2 hardening: the allowlisted files carry TIGHTENED contracts,
    // so an allowlisted file cannot smuggle a path literal past test 3's
    // api_client.rs census.
    //
    // (a) constants.rs naming ratchet: every CODE line mentioning `/alerts`
    //     must be a DHAN_ALERTS_*-prefixed constant — the api_client census
    //     counts alerts constants by the `DHAN_ALERTS_` prefix, so a
    //     rogue-named `DHAN_COND_ORDERS = "/alerts/orders"` constant would
    //     otherwise be invisible to it.
    let constants_source = fs::read_to_string("../common/src/constants.rs")
        .expect("constants.rs must be readable for the alerts naming ratchet");
    let constants_code = strip_full_line_comments(production_region(&constants_source));
    let naming_violations = alerts_naming_violations(&constants_code);
    assert!(
        naming_violations.is_empty(),
        "every constants.rs code line carrying `/alerts` must be a \
         DHAN_ALERTS_*-prefixed constant (test 3's census counts that \
         prefix). Violations:\n{}",
        naming_violations.join("\n")
    );

    // (b) types.rs + conditional.rs may mention the /alerts paths in DOC
    //     COMMENTS ONLY — zero comment-stripped code mentions, so neither
    //     file can carry a path literal for a new sender to consume.
    for path in ["src/oms/types.rs", CONDITIONAL_RS] {
        let source = fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("{path} must be readable for the choke-point guard"));
        let file_code = strip_full_line_comments(production_region(&source));
        for token in ["alerts/orders", "alerts/multi"] {
            assert_eq!(
                count_occurrences(&file_code, token),
                0,
                "{path} may mention `{token}` in doc comments ONLY — a code \
                 (string-literal) mention is a rogue path source outside the \
                 gated api_client.rs choke point"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 7. Scanner self-test (vacuous-pass defense)
// ---------------------------------------------------------------------------

#[test]
fn test_gate_guard_scanner_self_test() {
    // production_region must EXCLUDE test-module text (a planted violation
    // after the test-module marker is invisible — one before it is visible).
    let synthetic =
        "fn prod() {}\n#[cfg(test)]\nmod tests { fn planted() { x.place_multi_order(1); } }";
    assert!(
        !production_region(synthetic).contains("place_multi_order"),
        "production_region must stop at the test-module marker"
    );
    let planted_before_marker = "fn prod() { x.place_multi_order(1); }\n#[cfg(test)]\nmod tests {}";
    assert!(
        production_region(planted_before_marker).contains("place_multi_order"),
        "production_region must keep pre-marker text visible"
    );
    // A #[cfg(test)]-scoped fn ATTRIBUTE (no test module after it) must NOT
    // truncate the scan — the whole file stays visible.
    let attribute_only = "#[cfg(test)]\nfn helper() {}\nfn prod() { x.place_multi_order(1); }";
    assert!(
        production_region(attribute_only).contains("place_multi_order"),
        "a bare #[cfg(test)] fn attribute must not truncate the production region"
    );

    // A planted production caller IS detected.
    let planted_caller = "fn wire() { client.place_multi_order(token, &req); }";
    assert_eq!(
        production_region_calls_alerts_sender(planted_caller),
        Some("place_multi_order"),
        "scanner must detect a planted production sender call"
    );
    let planted_getter = "fn wire() { client.get_all_conditional_triggers(token); }";
    assert_eq!(
        production_region_calls_alerts_sender(planted_getter),
        Some("get_all_conditional_triggers"),
        "scanner must detect every sender token, GETs included"
    );

    // A clean source is NOT flagged.
    assert_eq!(production_region_calls_alerts_sender("fn ok() {}"), None);

    // The forbidden-literal check trips on a planted segment literal.
    let planted_segment =
        "pub enum ConditionalLegSegment { NseEq, BseEq }\nconst BAD: &str = \"NSE_FNO\";";
    assert!(
        FORBIDDEN_SEGMENT_LITERALS
            .iter()
            .any(|literal| production_region(planted_segment).contains(literal)),
        "scanner must detect a planted forbidden segment literal"
    );

    // The URL census detects EVERY string shape once comments are stripped:
    // inline positional, captured-identifier, and split-argument literals.
    let inline_shape = "let url = format!(\"{}/alerts/orders\", base);";
    assert_eq!(
        count_occurrences(&strip_full_line_comments(inline_shape), "/alerts"),
        1
    );
    let captured_shape = "let url = format!(\"{base}/alerts/orders\");";
    assert_eq!(
        count_occurrences(&strip_full_line_comments(captured_shape), "/alerts"),
        1
    );
    let split_arg_shape = "let url = format!(\"{}{}\", base, \"/alerts/orders\");";
    assert_eq!(
        count_occurrences(&strip_full_line_comments(split_arg_shape), "/alerts"),
        1
    );
    // A NEW DHAN_ALERTS_* constant (the flagged inline-path retrofit
    // follow-up) is counted by prefix, not by one hardcoded name.
    let new_constant_shape =
        "let url = format!(\"{}{}\", base, constants::DHAN_ALERTS_ORDERS_PATH);";
    assert_eq!(count_occurrences(new_constant_shape, "DHAN_ALERTS_"), 1);

    // strip_full_line_comments removes doc mentions but keeps code —
    // including string literals carrying `://` (never a comment start).
    let doc_only = "/// POST /v2/alerts/orders doc";
    assert_eq!(
        count_occurrences(&strip_full_line_comments(doc_only), "/alerts"),
        0
    );
    let doc_plus_code = "/// see /alerts docs\nlet u = \"https://x/alerts\";";
    assert_eq!(
        count_occurrences(&strip_full_line_comments(doc_plus_code), "/alerts"),
        1
    );

    // sender_fn_region + the ordering scan: a gate call AFTER the HTTP work
    // is detected (gate index > http index).
    let out_of_order = "    pub async fn place_multi_order(&self) {\n        \
                        let x = self.http.post(url);\n        \
                        self.require_alerts_gate(\"late\");\n    }\n    pub fn next() {}";
    let region = sender_fn_region(out_of_order, "place_multi_order");
    let gate = region
        .find("self.require_alerts_gate(")
        .expect("planted gate call visible");
    let http = region
        .find("self.http.")
        .expect("planted http call visible");
    assert!(
        gate > http,
        "self-test plant must model a LATE gate (gate after http)"
    );

    // ------------------------------------------------------------------
    // classify_gate_sites — the round-2 total-write census must detect
    // every planted arm SHAPE the literal-`true` needles missed.
    // ------------------------------------------------------------------

    // (1) Non-literal production setter (`= on`) IS an assignment.
    let planted_setter = "pub fn set_alerts_gate(&mut self, on: bool) { \
                          self.alerts_gate_armed = on; }";
    let sites = classify_gate_sites(planted_setter);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].kind, GateSiteKind::Assignment);
    assert_eq!(sites[0].rhs, "on", "non-literal RHS must be captured");

    // (2) Literal arm classifies as an assignment with RHS `true`.
    let planted_literal_arm = "self.alerts_gate_armed = true;";
    let sites = classify_gate_sites(planted_literal_arm);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].kind, GateSiteKind::Assignment);
    assert_eq!(sites[0].rhs, "true");

    // (3) Parameterized constructor init (`: armed`) IS a colon site with a
    //     non-`false` RHS (the `new_armed(.., armed)` bypass).
    let planted_param_init = "Self { alerts_gate_armed: armed }";
    let sites = classify_gate_sites(planted_param_init);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].kind, GateSiteKind::FieldColon);
    assert_eq!(sites[0].rhs, "armed");

    // (4) Literal struct-literal arm (`: true`, spaced or not) is a colon
    //     site with RHS `true`.
    for planted_field_init in [
        "Self { alerts_gate_armed: true }",
        "Self{alerts_gate_armed:true}",
    ] {
        let sites = classify_gate_sites(planted_field_init);
        assert_eq!(sites.len(), 1, "planted field init must be seen");
        assert_eq!(sites[0].kind, GateSiteKind::FieldColon);
        assert_eq!(sites[0].rhs, "true");
    }

    // (5) Compound assignment (`|=`) IS an assignment.
    let planted_compound = "self.alerts_gate_armed |= flag;";
    let sites = classify_gate_sites(planted_compound);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].kind, GateSiteKind::Assignment);

    // (6) `&mut` borrow of the field is its own mutation class.
    let planted_borrow = "std::mem::replace(&mut self.alerts_gate_armed, true)";
    let sites = classify_gate_sites(planted_borrow);
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].kind, GateSiteKind::MutBorrow);

    // (7) Reads never count as writes: bare read, `==` compare, match arm.
    for read_shape in [
        "if self.alerts_gate_armed { }",
        "if self.alerts_gate_armed == other { }",
        "assert!(client.alerts_gate_armed);",
        "match x { alerts_gate_armed => {} }",
    ] {
        let sites = classify_gate_sites(read_shape);
        assert_eq!(sites.len(), 1, "read shape must be seen: {read_shape}");
        assert_eq!(
            sites[0].kind,
            GateSiteKind::Read,
            "read shape must classify Read: {read_shape}"
        );
    }

    // (8) Identifier boundary: a LONGER identifier never counts.
    assert!(
        classify_gate_sites("let alerts_gate_armed_x = true;").is_empty(),
        "boundary check must skip longer identifiers"
    );
    // The field declaration classifies as a colon site with RHS `bool`.
    let decl = "    alerts_gate_armed: bool,";
    let sites = classify_gate_sites(decl);
    assert_eq!(sites[0].kind, GateSiteKind::FieldColon);
    assert_eq!(sites[0].rhs, "bool");

    // ------------------------------------------------------------------
    // derive_alerts_sender_fns — a NEW /alerts-touching pub async fn is
    // derived (even with a LATE gate call), a non-alerts fn is not.
    // ------------------------------------------------------------------
    let synthetic_api = "    pub async fn cancel_multi_order(&self) {\n        \
                         let x = self.http.post(url).send();\n        \
                         self.require_alerts_gate(\"late\");\n        \
                         let url = format!(\"{}/alerts/multi\", base);\n    }\n    \
                         pub async fn place_order(&self) {\n        \
                         let url = format!(\"{}/orders\", base);\n    }\n";
    assert_eq!(
        derive_alerts_sender_fns(synthetic_api),
        vec!["cancel_multi_order".to_string()],
        "sender derivation must catch a NEW /alerts fn and skip non-alerts fns"
    );
    let constant_backed_sender = "    pub async fn bulk(&self) {\n        \
                                  let url = format!(\"{}{}\", b, constants::DHAN_ALERTS_X);\n    }\n";
    assert_eq!(
        derive_alerts_sender_fns(constant_backed_sender),
        vec!["bulk".to_string()],
        "sender derivation must catch a DHAN_ALERTS_*-constant-backed fn"
    );

    // ------------------------------------------------------------------
    // Comment-stripped gate-call counting — a full-line comment carrying
    // the gate token must NOT inflate the census (round-2 asymmetry fix).
    // ------------------------------------------------------------------
    let commented_gate = "// callers must self.require_alerts_gate(..) first\n\
                          let url = \"/alerts/orders\";";
    assert_eq!(
        count_occurrences(
            &strip_full_line_comments(commented_gate),
            "self.require_alerts_gate("
        ),
        0,
        "a commented-out gate call must not count as a gate site"
    );

    // ------------------------------------------------------------------
    // alerts_naming_violations — a rogue-named constant carrying /alerts
    // is flagged; the DHAN_ALERTS_* spelling is not.
    // ------------------------------------------------------------------
    let rogue_constant = "pub const DHAN_COND_ORDERS: &str = \"/alerts/orders\";";
    assert_eq!(
        alerts_naming_violations(rogue_constant).len(),
        1,
        "a /alerts constant without the DHAN_ALERTS_ prefix must be flagged"
    );
    let proper_constant =
        "pub const DHAN_ALERTS_MULTI_ORDERS_PATH: &str = \"/alerts/multi/orders\";";
    assert!(
        alerts_naming_violations(proper_constant).is_empty(),
        "a DHAN_ALERTS_*-prefixed constant is the allowed spelling"
    );

    // enum_body extracts only the first block's body.
    let synthetic_enum =
        "pub enum ConditionalSegment { NseEq, BseEq, IdxI }\npub enum Other { Fno }";
    let body = enum_body(synthetic_enum, "ConditionalSegment");
    assert!(body.contains("IdxI"));
    assert!(!body.contains("Fno"));
}
