//! HTTP-CLIENT-01 ratchet (app crate) — the panic-class reqwest fallback
//! must never reappear on the OMS wiring surface (post-#1562 audit fix,
//! 2026-07-17).
//!
//! The #1562 delta shipped `build_oms_http_client()` ending in
//! `.build().unwrap_or_default()` — `reqwest::Client::default()` delegates
//! to `Client::new()`, which PANICS under exactly the fd/TLS/resolver
//! pressure that makes the builder's `Err` arm reachable. With the
//! workspace release profile's `panic = "abort"` that was a WHOLE-PROCESS
//! abort at every order-runtime (re)spawn ([order_runtime] enabled=true in
//! base.toml), killing the REST capture legs for a dry-run client that
//! never issues HTTP. The storage-crate ratchet
//! (`crates/storage/tests/http_client_fallback_guard.rs`) scans only
//! `crates/storage/src`, so this app-crate copy escaped it; this guard
//! mirrors it, scoped to the OMS wiring surface where the fix landed.
//!
//! See: `.claude/rules/project/http-client-error-codes.md` (§1 site
//! `oms_wiring` + the 2026-07-17 update).

#![cfg(test)]

use std::path::PathBuf;

/// The OMS wiring surface: the shared builder + its two production callers.
/// Deliberately NOT the whole app crate — sibling files carry pre-existing
/// `Client::new()` uses in `#[cfg(test)]` modules and one documented
/// production fallback (`infra.rs::liveness_client`, flagged as a follow-up
/// in the runbook's 2026-07-17 note) that this PR does not own.
const GUARDED_FILES: [&str; 3] = ["oms_wiring.rs", "order_runtime.rs", "trading_pipeline.rs"];

fn app_src(file: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file)
}

/// The production region of a source file: everything above the first
/// column-0 `#[cfg(test)]` line (the house production-region split) —
/// test modules may legitimately use `reqwest::Client::new()`.
fn production_region(content: &str) -> &str {
    match content.find("\n#[cfg(test)]") {
        Some(idx) => &content[..idx],
        None => content,
    }
}

/// Strip the `//`-comment tail of a line (so comments explaining WHY the
/// pattern is banned don't trip the guard), treating `://` (URL scheme
/// separators in string literals) as code — the storage guard's stripper,
/// self-tested below.
fn code_portion(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'/' {
            let preceded_by_colon = i > 0 && bytes[i - 1] == b':';
            if !preceded_by_colon {
                return &line[..i];
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    line
}

fn scan_file_for(file: &str, needles: &[&str]) -> Vec<String> {
    let path = app_src(file);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("guarded file {} must exist: {e}", path.display()));
    let mut violations = Vec::new();
    for (idx, line) in production_region(&content).lines().enumerate() {
        if line.contains("RATCHET-ALLOW:") {
            continue;
        }
        let code = code_portion(line);
        for needle in needles {
            if code.contains(needle) {
                violations.push(format!("{file}:{}: {}", idx + 1, line.trim()));
            }
        }
    }
    violations
}

/// No panic-class `Client::new()` / `Client::default()` and no
/// `unwrap_or_else(|_| Client` fallback in the PRODUCTION region of the
/// OMS wiring surface — every client build must degrade through the typed
/// `HttpClientBuildError` path.
#[test]
fn no_panic_class_client_fallback_on_oms_wiring_surface() {
    let needles = [
        "Client::new()",
        "Client::default()",
        "unwrap_or_else(|_| Client",
        "use reqwest::Client as ",
    ];
    let mut violations = Vec::new();
    for file in GUARDED_FILES {
        violations.extend(scan_file_for(file, &needles));
    }
    assert!(
        violations.is_empty(),
        "HTTP-CLIENT-01 ratchet (app): panic-class reqwest fallback found in a \
         production region — Client::new()/Client::default() panic on \
         TLS/resolver/fd init failure; use oms_wiring::build_oms_http_client's \
         typed Result path instead.\n  {}",
        violations.join("\n  ")
    );
}

/// The exact #1562 regression shape — `unwrap_or_default` anywhere in
/// oms_wiring.rs (the file exists ONLY to build the OMS client, so no
/// legitimate use of the pattern belongs there).
#[test]
fn no_unwrap_or_default_in_oms_wiring() {
    let violations = scan_file_for("oms_wiring.rs", &["unwrap_or_default"]);
    assert!(
        violations.is_empty(),
        "HTTP-CLIENT-01 ratchet (app): `unwrap_or_default` in oms_wiring.rs \
         reintroduces the #1562 panic fallback (Client::default() == \
         Client::new() == panic on builder-failure conditions).\n  {}",
        violations.join("\n  ")
    );
}

/// Stub-guard: the fix must stay the typed degrade — oms_wiring keeps the
/// `HttpClientBuildError` Result shape + the coded emit + the site counter,
/// and BOTH production callers consume the Result (no `.unwrap()` revival).
#[test]
fn oms_wiring_keeps_typed_degrade_shape() {
    let wiring = std::fs::read_to_string(app_src("oms_wiring.rs")).expect("oms_wiring.rs");
    let prod = production_region(&wiring);
    for needle in [
        "Result<reqwest::Client, HttpClientBuildError>",
        "HttpClient01BuildFailed",
        "tv_http_client_build_failed_total",
        r#""site" => "oms_wiring""#,
    ] {
        assert!(
            prod.contains(needle),
            "oms_wiring.rs production region lost the typed HTTP-CLIENT-01 \
             degrade shape — missing: {needle}"
        );
    }
    for caller in ["order_runtime.rs", "trading_pipeline.rs"] {
        let src = std::fs::read_to_string(app_src(caller)).expect(caller);
        let prod = production_region(&src);
        assert!(
            prod.contains("build_oms_http_client()"),
            "{caller} must keep building the OMS client via the shared \
             oms_wiring builder"
        );
        assert!(
            prod.contains("Ok(") && !prod.contains("build_oms_http_client().unwrap"),
            "{caller} must consume build_oms_http_client's Result via a \
             degrade path, never unwrap"
        );
    }
}

/// Self-test for the comment stripper (mirrors the storage guard's pin):
/// `://` in a string literal must not hide a banned pattern later on the
/// same line; a genuine comment must be stripped.
#[test]
fn code_portion_scheme_separator_does_not_hide_banned_pattern() {
    let line = r#"let x = "http://host"; Client::new()"#;
    assert!(code_portion(line).contains("Client::new()"));
    let comment = "// Client::new() in a comment";
    assert!(!code_portion(comment).contains("Client::new()"));
}
