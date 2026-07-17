//! RS5 source-scan ratchet (Z+ L4 PREVENT): the REAL cadence broker
//! executors are LIMITER-FREE and GATE-FREE by contract.
//!
//! `cadence-error-codes.md` §0b/§3b (coordinator ruling A, 2026-07-16 +
//! the RS11 direction clause): cadence fires do NOT route through the
//! shared `dhan_data_api_limiter` (the combined cap-5 gate ring is the
//! binding pacing, owned by the RUNNER), and scheduler-driven fires are
//! already acquired/recorded by the runner BEFORE dispatch — an executor
//! that re-acquires or re-records the gates double-consumes the budget.
//! This guard pins BOTH properties at the source level for the two
//! executor files: no limiter mention, no gate-registry handle, no gate
//! acquire/record/reseed call.
//!
//! Comment-stripped scan (the `http_client_fallback_guard.rs` `://`-aware
//! stripper precedent) so a prose comment naming a needle can never trip
//! — and so a needle smuggled in as code can never hide behind review
//! prose claiming it is "just a comment".

use std::fs;
use std::path::PathBuf;

const EXECUTOR_FILES: [&str; 2] = [
    "src/dhan_cadence_executor.rs",
    "src/groww_cadence_executor.rs",
];

/// Forbidden needles in the (comment-stripped) executor sources.
/// `record_chain_moneyness_observability` is a legitimate NON-gate call,
/// so the gate-record ban is expressed via the gate API's ACTUAL fn/type
/// names (`try_acquire*` / `reseed*` / `chain_expiry_stamp` / the gate
/// types + global handle), never a generic `record_` substring.
const FORBIDDEN: [&str; 8] = [
    // The shared legacy-path limiter (either handle form).
    "dhan_data_api_limiter",
    "shared_dhan_data_api_limiter",
    // The process-global gate registry handle (covers init_ too).
    "global_dhan_gates",
    // Gate acquire/record/reseed API (covers try_acquire_chain /
    // try_acquire_spot / try_acquire_expiry / MinSpacingGate::try_acquire
    // and reseed / reseed_all).
    "try_acquire",
    ".reseed",
    "chain_expiry_stamp",
    // The gate types themselves — an executor never constructs one.
    "DhanGates",
    "MinSpacingGate",
];

/// Forbidden CALL needles — the PACED legacy wrappers. An executor calling
/// `spot_1m_fetch_once(` / `chain_fetch_once(` reaches the shared limiter
/// through one hop of indirection with ZERO of the `FORBIDDEN` needles in
/// its own source (the wrappers do the `shared_dhan_data_api_limiter()`
/// acquire internally), so the paced wrapper names are banned too. Matched
/// with an IDENTIFIER-BOUNDARY check (the char before the match must not be
/// `[A-Za-z0-9_]`) so the LEGAL calls survive: `spot_1m_fetch_once_unpaced(`
/// / `chain_fetch_once_unpaced(` (different suffix — no substring match) and
/// the Groww executor's limiter-free `groww_chain_fetch_once(` (contains
/// `chain_fetch_once(` but preceded by `_`).
const FORBIDDEN_PACED_CALLS: [&str; 2] = ["spot_1m_fetch_once(", "chain_fetch_once("];

/// `haystack.contains(needle)` restricted to matches NOT preceded by an
/// identifier character — a call-site match, never a longer-identifier
/// suffix match.
fn contains_call(haystack: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(needle) {
        let abs = from + pos;
        let preceded_by_ident = abs > 0 && {
            let b = haystack.as_bytes()[abs - 1];
            b.is_ascii_alphanumeric() || b == b'_'
        };
        if !preceded_by_ident {
            return true;
        }
        from = abs + 1;
    }
    false
}

fn app_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Strip `//` line comments, treating `://` (URL scheme separators inside
/// string literals) as code — the house `http_client_fallback_guard.rs`
/// stripper. Good enough for these files: neither carries a needle-shaped
/// string literal, and block comments are not used for prose here.
fn strip_line_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let bytes = line.as_bytes();
        let mut cut = line.len();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == b'/' && bytes[i + 1] == b'/' && (i == 0 || bytes[i - 1] != b':') {
                cut = i;
                break;
            }
            i += 1;
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

#[test]
fn test_cadence_executors_never_touch_limiter_or_gates() {
    for rel in EXECUTOR_FILES {
        let stripped = strip_line_comments(&app_src(rel));
        for needle in FORBIDDEN {
            assert!(
                !stripped.contains(needle),
                "{rel} mentions forbidden `{needle}` in CODE — the cadence \
                 executors are limiter-free (coordinator ruling A, \
                 2026-07-16) and gate-free (RS11: the runner acquires/records \
                 the gates before dispatch; an executor-side touch \
                 double-consumes the budget). See \
                 .claude/rules/project/cadence-error-codes.md §0b/§3b."
            );
        }
        for needle in FORBIDDEN_PACED_CALLS {
            assert!(
                !contains_call(&stripped, needle),
                "{rel} calls the PACED legacy wrapper `{needle}` — that \
                 wrapper acquires the shared dhan_data_api_limiter \
                 internally, so the call reaches the limiter through one \
                 hop of indirection. The cadence executors must call the \
                 `*_unpaced` inners only (coordinator ruling A, 2026-07-16; \
                 .claude/rules/project/cadence-error-codes.md §0b/§3b)."
            );
        }
    }
}

#[test]
fn test_purity_guard_scan_is_non_vacuous() {
    // Both executor files must exist and be non-trivial — a renamed/moved
    // executor would otherwise make the forbidden-needle scan vacuously
    // green (audit Rule 11, no false-OK).
    for rel in EXECUTOR_FILES {
        let src = app_src(rel);
        assert!(
            src.contains("impl CadenceExecutor for"),
            "{rel} must implement CadenceExecutor — the purity scan targets \
             the real executor files."
        );
    }
}

#[test]
fn test_comment_stripper_self_check() {
    // The stripper must remove a comment-borne needle but keep code and
    // treat `://` as code (a URL in a string literal survives).
    let sample = "let url = \"https://api.dhan.co\"; // dhan_data_api_limiter\nlet x = 1;\n";
    let stripped = strip_line_comments(sample);
    assert!(stripped.contains("https://api.dhan.co"));
    assert!(!stripped.contains("dhan_data_api_limiter"));
    // And a CODE needle survives stripping (would be caught).
    let code = "let g = global_dhan_gates(); // fine\n";
    assert!(strip_line_comments(code).contains("global_dhan_gates"));
}

#[test]
fn test_contains_call_identifier_boundary_self_check() {
    // A bare call matches.
    assert!(contains_call(
        "let r = chain_fetch_once(client);",
        "chain_fetch_once("
    ));
    // Match at position 0 matches.
    assert!(contains_call("chain_fetch_once(x)", "chain_fetch_once("));
    // A longer-identifier suffix does NOT match (the Groww executor's
    // legal limiter-free `groww_chain_fetch_once(`).
    assert!(!contains_call(
        "groww_chain_fetch_once(client, url)",
        "chain_fetch_once("
    ));
    // The `*_unpaced` inner never matches the paced needle (different
    // suffix — no substring at all).
    assert!(!contains_call(
        "spot_1m_fetch_once_unpaced(client)",
        "spot_1m_fetch_once("
    ));
    // Non-ident separators before the match still match (method-position /
    // path-position calls).
    assert!(contains_call(
        "x = (chain_fetch_once(a));",
        "chain_fetch_once("
    ));
    assert!(contains_call(
        "crate::spot_1m_rest_boot::spot_1m_fetch_once(a)",
        "spot_1m_fetch_once("
    ));
}
