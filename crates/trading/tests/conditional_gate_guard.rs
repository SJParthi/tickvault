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
//!    assignment (`= true`) AND struct-literal (`: true`) syntaxes both.
//! 3. Every `/alerts` URL-building sender checks the gate BEFORE any
//!    URL/socket work. The URL census is GENERAL: any `/alerts` text in
//!    comment-stripped production code (inline positional, captured-
//!    identifier, and bare-literal format shapes alike) plus any
//!    `DHAN_ALERTS_`-prefixed constant use — so a new sender built from a
//!    NEW constant or a split-argument literal still trips the equality.
//! 4. NO production code calls any of the 6 sender fns (dormancy ratchet —
//!    the activation PR edits THIS test alongside the operator quote).
//! 5. The order-leg segment enums stay equities-only fail-closed.
//! 6. The `/alerts` paths have a single choke point (no rogue sender files).
//! 7. The scanner itself detects planted violations (vacuous-pass defense —
//!    the 2026-07-06 lesson).
//!
//! Pattern: `sandbox_enforcement_guard.rs` (source scanning, not runtime
//! probing — the gate's arm path is `#[cfg(test)]`-scoped, so text is the
//! honest evidence surface).

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

    // The declaration must be preceded by #[cfg(test)] within a bounded
    // window (doc comment + attribute lines).
    let window_start = arm_decl.saturating_sub(400);
    let preceding_window = &source[window_start..arm_decl];
    assert!(
        preceding_window.contains("#[cfg(test)]"),
        "arm_alerts_gate_for_test must be #[cfg(test)]-scoped — a production \
         arm path for the /alerts gate is FORBIDDEN without a dated operator quote."
    );

    // `alerts_gate_armed = true` may appear ONLY inside that fn (never a
    // second arm site).
    let assignments: Vec<usize> = source
        .match_indices("alerts_gate_armed = true")
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        assignments.len(),
        1,
        "exactly ONE `alerts_gate_armed = true` assignment may exist (inside \
         arm_alerts_gate_for_test); found {}",
        assignments.len()
    );
    let assignment = assignments[0];
    assert!(
        assignment > arm_decl && assignment < arm_decl + 400,
        "the single `alerts_gate_armed = true` assignment must live inside \
         arm_alerts_gate_for_test's body"
    );

    // A STRUCT-LITERAL arm (`alerts_gate_armed: true` field init, e.g. a
    // `new_armed(..)` constructor) is equally forbidden — zero occurrences
    // anywhere in the file, test module included (tests arm via
    // arm_alerts_gate_for_test, never a field init). Both fmt'd and
    // unspaced spellings are scanned.
    for field_init in ["alerts_gate_armed: true", "alerts_gate_armed:true"] {
        assert_eq!(
            count_occurrences(&source, field_init),
            0,
            "`{field_init}` field-init arming is FORBIDDEN — the ONLY field \
             init is `alerts_gate_armed: false` in OrderApiClient::new; a \
             production arm path requires a dated operator quote + a \
             .claude/rules/dhan/conditional-trigger.md edit FIRST"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Every /alerts URL-building sender checks the gate
// ---------------------------------------------------------------------------

#[test]
fn test_every_alerts_sender_checks_gate_first() {
    let source = fs::read_to_string(API_CLIENT_RS)
        .expect("api_client.rs must be readable for the alerts-gate guard");
    let production = production_region(&source);

    let gate_calls = count_occurrences(production, "self.require_alerts_gate(");
    assert!(
        gate_calls >= 6,
        "all SIX /alerts senders must call self.require_alerts_gate( FIRST; \
         found only {gate_calls} call sites"
    );

    // URL-building census — GENERAL, not needle-per-shape: after stripping
    // full-line comments, any `/alerts` text left in production code can
    // only live inside a string literal (inline `"{}/alerts`, captured
    // `"{base}/alerts`, and split-argument `"/alerts/..."` shapes all
    // match), plus any `DHAN_ALERTS_`-PREFIXED constant use (so a new
    // constant like DHAN_ALERTS_ORDERS_PATH is counted, not just the one
    // known name). The gate's own refusal message is the single allowed
    // non-URL `/alerts` mention and is excluded by its exact prefix.
    let code = strip_full_line_comments(production);
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

    // ORDERING: in every sender fn body the gate check must precede the
    // FIRST URL/socket token — `require_alerts_gate` doc contract "Checked
    // BEFORE any URL/socket work". A gate call after `.send().await` would
    // otherwise satisfy the count equality.
    for fn_name in ALERTS_SENDER_FNS {
        let region = sender_fn_region(production, fn_name);
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
        // The declaring file is the single gated choke point (its own senders
        // are the definitions, and its call sites are #[cfg(test)]-scoped).
        if path_text.ends_with("trading/src/oms/api_client.rs") {
            continue;
        }
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

    // The struct-literal arm needle trips on a planted field init.
    let planted_field_init = "Self { alerts_gate_armed: true }";
    assert_eq!(
        count_occurrences(planted_field_init, "alerts_gate_armed: true"),
        1,
        "scanner must detect a planted struct-literal arm"
    );

    // enum_body extracts only the first block's body.
    let synthetic_enum =
        "pub enum ConditionalSegment { NseEq, BseEq, IdxI }\npub enum Other { Fno }";
    let body = enum_body(synthetic_enum, "ConditionalSegment");
    assert!(body.contains("IdxI"));
    assert!(!body.contains("Fno"));
}
