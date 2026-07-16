//! Moneyness RAM-first ratchet (2026-07-14) — the chain-moneyness decision
//! surface is RAM, the DB column is a write-only audit mirror.
//!
//! Operator directive 2026-07-14: moneyness is computed in the RAM hot path
//! AND stored precomputed in the DB — the DB is audit-only; RAM is the
//! decision source of truth. This guard fails the build if:
//!
//! 1. Any DB/HTTP read machinery (`SELECT`, `questdb`, `reqwest`,
//!    `http://`, `/exec`) enters the pure-math module
//!    (`crates/common/src/moneyness.rs`) or the RAM snapshot module
//!    (`crates/core/src/pipeline/chain_snapshot.rs`) — a future consumer
//!    "just querying the table" through these modules would silently
//!    demote RAM from source-of-truth to cache.
//! 2. The banned-pattern scanner loses its RAM-first category (Cat-10) or
//!    stops treating `crates/core/src/pipeline/` as hot path — the
//!    process-level guards this module relies on.
//! 3. The three boot legs stop CALLING the classification / publish
//!    surface (the wiring half — every `pub fn` here must keep its
//!    production call site), or the contract leg starts publishing a
//!    snapshot it was never authorized to own.
//!
//! Style: the comment-aware `code_portion` stripper is the
//! `http_client_fallback_guard.rs` house pattern (a `//` NOT preceded by
//! `:` opens a comment, so `"http://host"` string literals never truncate
//! the scan).

#![cfg(test)]

use std::path::{Path, PathBuf};

/// CARGO_MANIFEST_DIR = crates/core — hop to the repo root / sibling crates.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ dir")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn guarded_module_paths() -> [PathBuf; 2] {
    let root = repo_root();
    [
        root.join("crates/common/src/moneyness.rs"),
        root.join("crates/core/src/pipeline/chain_snapshot.rs"),
    ]
}

/// Strip the `//`-comment tail of a line so doc/inline comments explaining
/// WHY a token is banned don't trip the guard — EXCEPT that a URL scheme
/// separator (`://`, as in a `"http://host"` string literal) must NOT be
/// treated as a comment start: a naive `split("//")` would truncate the
/// scan at the URL and miss a banned token appearing LATER on the line.
fn code_portion(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'/' {
            let preceded_by_colon = i > 0 && bytes[i - 1] == b':';
            if !preceded_by_colon {
                return &line[..i];
            }
            // `://` — a URL scheme separator inside a string literal, not
            // a comment. Skip past it and keep scanning.
            i += 2;
        } else {
            i += 1;
        }
    }
    line
}

/// Case-insensitive forbidden tokens — any DB/HTTP read machinery.
/// `"select "` carries the trailing space so `tokio::select!` and
/// `select_nth_unstable` never false-positive.
const FORBIDDEN_TOKENS: [&str; 5] = ["select ", "questdb", "reqwest", "http://", "/exec"];

/// Scan a file's CODE (comments stripped, lowercased) for the forbidden
/// tokens; returns `path:line: content` violation strings.
fn scan_source(path: &Path, content: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if line.contains("RATCHET-ALLOW:") {
            continue;
        }
        let code = code_portion(line).to_ascii_lowercase();
        for token in FORBIDDEN_TOKENS {
            if code.contains(token) {
                violations.push(format!(
                    "{}:{}: [{}] {}",
                    path.display(),
                    idx + 1,
                    token,
                    line.trim()
                ));
            }
        }
    }
    violations
}

/// The pure-math + RAM-snapshot modules must never grow a DB/HTTP read.
#[test]
fn moneyness_and_chain_snapshot_are_ram_only() {
    let mut violations = Vec::new();
    for path in guarded_module_paths() {
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("guarded module {} must exist: {e}", path.display()));
        violations.extend(scan_source(&path, &content));
    }
    assert!(
        violations.is_empty(),
        "RAM-FIRST ratchet: DB/HTTP read machinery entered the moneyness \
         decision surface — the DB column is a write-only audit mirror; \
         consumers read the RAM snapshot (chain_snapshot::load_chain_snapshot), \
         never the table.\n  {}",
        violations.join("\n  ")
    );
}

/// Self-test for the scanner itself: a seeded violation MUST fire; a
/// comment-only mention must NOT; a `://` string literal must not hide a
/// later token on the same line.
#[test]
fn scanner_self_test_detects_seeded_violation() {
    let seeded = r#"let x = questdb_query("SELECT close FROM candles_1m");"#;
    let hits = scan_source(Path::new("seeded.rs"), seeded);
    assert!(
        hits.len() >= 2,
        "scanner must flag both `questdb` and `select ` in seeded code, got: {hits:?}"
    );

    let comment_only = "// questdb SELECT reads are banned here (see rule file)";
    assert!(
        scan_source(Path::new("comment.rs"), comment_only).is_empty(),
        "comment-only mentions must not fire"
    );

    let url_hides_nothing = r#"let u = "http://host"; let c = reqwest_client();"#;
    let url_hits = scan_source(Path::new("url.rs"), url_hides_nothing);
    assert!(
        url_hits.iter().any(|h| h.contains("[reqwest]")),
        "a `://` string literal must not truncate the scan before a later \
         token: {url_hits:?}"
    );
    assert!(
        url_hits.iter().any(|h| h.contains("[http://]")),
        "the URL literal itself must be flagged: {url_hits:?}"
    );

    // Bare `select!` / `select_nth_unstable` (no trailing space) never fire.
    let macro_line = "tokio::select! { _ = a => {}, } ; v.select_nth_unstable(3);";
    assert!(
        scan_source(Path::new("macro.rs"), macro_line).is_empty(),
        "`select!`/`select_nth_unstable` must not false-positive"
    );
}

/// The process-level RAM-first guards this module relies on must stay in
/// the banned-pattern scanner: Cat-10 (SELECT on strategy/indicator/risk
/// hot paths) and the pipeline dir in the hot-path include regex (which
/// keeps `chain_snapshot.rs` under the Cat-2 zero-alloc scan).
#[test]
fn banned_pattern_scanner_keeps_ram_first_category() {
    let hook = repo_root().join(".claude/hooks/banned-pattern-scanner.sh");
    let content = std::fs::read_to_string(&hook)
        .unwrap_or_else(|e| panic!("{} must exist: {e}", hook.display()));

    for needle in [
        "scan_ram_first_hot_path",
        "crates/trading/src/strategy/",
        "crates/trading/src/indicator/",
        "risk_check",
        r"\bSELECT\b",
    ] {
        assert!(
            content.contains(needle),
            "banned-pattern-scanner.sh lost its RAM-first (Cat-10) needle \
             {needle:?} — the SELECT-on-hot-path ban is load-bearing for the \
             moneyness RAM-decision contract"
        );
    }
    assert!(
        content.contains("^crates/core/src/pipeline/"),
        "banned-pattern-scanner.sh HOT_PATH_INCLUDE_REGEX must keep \
         `^crates/core/src/pipeline/` — chain_snapshot.rs relies on the \
         Cat-2 zero-alloc scan"
    );
}

/// The PRODUCTION region of a source file: everything before the first
/// `#[cfg(test)]` marker (the shadow-writer ratchet precedent). Scanning
/// the whole file would let a `#[cfg(test)]` module's own call to
/// `classify_chain_legs(` satisfy the wiring assertion while the
/// production call site was deleted — a vacuous pass (audit Rule 11).
fn production_region(content: &str) -> &str {
    content.split("#[cfg(test)]").next().unwrap_or(content)
}

/// Wiring half: all three boot legs must keep CALLING the classification
/// surface, both chain legs must keep PUBLISHING the RAM snapshot, and the
/// contract leg (DB-audit-only by design) must NOT publish one. Scans the
/// PRODUCTION region only — test-only code can neither satisfy the
/// positive assertions nor trip the negative one.
#[test]
fn boot_legs_keep_moneyness_wiring() {
    let app_src = repo_root().join("crates/app/src");

    let dhan_chain = std::fs::read_to_string(app_src.join("option_chain_1m_boot.rs"))
        .expect("option_chain_1m_boot.rs must exist");
    let groww_chain = std::fs::read_to_string(app_src.join("groww_option_chain_1m_boot.rs"))
        .expect("groww_option_chain_1m_boot.rs must exist");
    let groww_contract = std::fs::read_to_string(app_src.join("groww_contract_1m_boot.rs"))
        .expect("groww_contract_1m_boot.rs must exist");

    for (name, content) in [
        ("option_chain_1m_boot.rs", &dhan_chain),
        ("groww_option_chain_1m_boot.rs", &groww_chain),
    ] {
        let prod = production_region(content);
        assert!(
            prod.contains("classify_chain_legs("),
            "{name} must classify every chain leg via \
             classify_chain_legs() in PRODUCTION code (the #[cfg(test)] \
             region cannot satisfy this) — the shared common-math glue"
        );
        assert!(
            prod.contains("publish_chain_moneyness_snapshot("),
            "{name} must publish the RAM chain snapshot after classification \
             in PRODUCTION code — the decision surface future strategy \
             consumers read"
        );
    }

    let contract_prod = production_region(&groww_contract);
    assert!(
        contract_prod.contains("classify_moneyness_for("),
        "groww_contract_1m_boot.rs must classify each contract row via \
         classify_moneyness_for() in PRODUCTION code (chain-anchor spot + \
         parsed strike + leg)"
    );
    assert!(
        !contract_prod.contains("publish_chain_moneyness_snapshot("),
        "the contract leg is DB-audit-only by design — its PRODUCTION code \
         must NOT publish a chain snapshot (the chain legs own the RAM \
         surface)"
    );
}

/// Self-test for the production-region split: a needle that appears ONLY
/// inside a `#[cfg(test)]` module must NOT be visible to the scan, and a
/// needle in production code must be — so the wiring assertions above can
/// never be satisfied (or tripped) by test-only code.
#[test]
fn production_region_split_excludes_test_only_code() {
    let test_only =
        "fn real() {}\n#[cfg(test)]\nmod tests {\n    fn t() { classify_chain_legs(); }\n}\n";
    assert!(
        !production_region(test_only).contains("classify_chain_legs("),
        "a test-module-only call must be invisible to the production scan"
    );

    let prod_call = "fn real() { classify_chain_legs(); }\n#[cfg(test)]\nmod tests {}\n";
    assert!(
        production_region(prod_call).contains("classify_chain_legs("),
        "a production call must remain visible to the scan"
    );

    let no_test_module = "fn real() { classify_chain_legs(); }\n";
    assert!(
        production_region(no_test_module).contains("classify_chain_legs("),
        "a file with no #[cfg(test)] marker scans in full"
    );
}
