//! LIMIT+1 truncation-probe convention ratchet (2026-07-18).
//!
//! Every QuestDB `/exec` COMPLETENESS probe in the app crate follows ONE
//! convention (the #1630 spot_crossverify / market_ram_store PR-2 round-1
//! shape): the SQL builder fetches `cap + 1` rows, the parser flags
//! `truncated` ONLY when the dataset carries MORE than `cap` rows
//! (`len > cap`), and an exact-boundary `len == cap` dataset is trusted as
//! legitimately complete. The pre-2026-07-18 drift sites
//! (`rest_candle_fold` + `tf_consistency_boot`) queried exactly `LIMIT cap`
//! and refused at `len >= cap` — a zero-headroom false-PARTIAL on any
//! legitimate exactly-cap day (the class #1630 fixed for the spot
//! cross-check: a healthy 4-index day is exactly 1,500 rows).
//!
//! Deliberately OUT of scope (a different class, not a query completeness
//! probe): the brutex_crossverify CSV streaming accept-cap
//! (`out.rows.len() >= max_rows` → stop accepting further rows — a bound on
//! rows ACCEPTED while streaming, where `>=` is the correct stop condition)
//! and the RAM-store chain_row_cap / minute-cap drops (row caps).
//!
//! See: `.claude/rules/project/rest-candle-fold-error-codes.md` +
//! `.claude/rules/project/tf-consistency-error-codes.md` (dated 2026-07-18
//! notes) and `.claude/rules/project/ram-store-error-codes.md`
//! (`rehydrate_truncated` — the convention's original wording).

#![cfg(test)]

use std::path::PathBuf;

/// The five `/exec` completeness-probe files.
const PROBE_FILES: [&str; 5] = [
    "rest_candle_fold.rs",
    "tf_consistency_boot.rs",
    "market_ram_store_boot.rs",
    "spot_crossverify_boot.rs",
    "groww_spot_1m_boot.rs",
];

fn app_src(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The production region of a source file: everything above the first
/// column-0 `#[cfg(test)]` line (the house production-region split) — test
/// modules may legitimately carry old-shape literals in fixtures/comments.
fn production_region(content: &str) -> &str {
    match content.find("\n#[cfg(test)]") {
        Some(idx) => &content[..idx],
        None => content,
    }
}

/// Strip `//`-comment tails so prose explaining the banned shape can never
/// trip (or satisfy) the scanner; `://` (URL schemes in string literals) is
/// treated as code — the house stripper, self-tested below.
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
            continue;
        }
        i += 1;
    }
    line
}

fn stripped_production(file: &str) -> String {
    let content = app_src(file);
    production_region(&content)
        .lines()
        .map(code_portion)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The refuse-at-cap comparison shapes banned from probe production code.
fn banned_needles() -> [&'static str; 3] {
    [".len() >= limit", ".len() >= cap", ".len() >= fetch_limit"]
}

/// Positive pins: every probe site fetches `cap + 1`.
#[test]
fn probe_sites_fetch_limit_plus_one() {
    let pins: [(&str, &[&str]); 5] = [
        (
            "rest_candle_fold.rs",
            &["let fetch_limit = limit.saturating_add(1);"],
        ),
        (
            "tf_consistency_boot.rs",
            &[
                "TF_VERIFY_1M_ROW_LIMIT.saturating_add(1)",
                "TF_VERIFY_TF_UNION_ROW_LIMIT.saturating_add(1)",
                "TF_VERIFY_DISCOVERY_ROW_LIMIT.saturating_add(1)",
            ],
        ),
        ("market_ram_store_boot.rs", &["limit.saturating_add(1)"]),
        ("spot_crossverify_boot.rs", &["SPOT_XVERIFY_ROW_LIMIT + 1"]),
        (
            "groww_spot_1m_boot.rs",
            &["GROWW_SPOT1M_PRE_BOOT_READ_CAP + 1"],
        ),
    ];
    for (file, needles) in pins {
        let prod = stripped_production(file);
        for needle in needles {
            assert!(
                prod.contains(needle),
                "{file}: LIMIT+1 probe fetch needle missing: `{needle}` — \
                 every /exec completeness probe must query cap + 1 rows \
                 (2026-07-18 convention)"
            );
        }
    }
}

/// Positive pins: every probe parser flags strictly OVER the cap.
#[test]
fn probe_sites_flag_strictly_over_limit() {
    let pins: [(&str, &[&str]); 5] = [
        (
            "rest_candle_fold.rs",
            &["let truncated = dataset.len() > limit;"],
        ),
        (
            "tf_consistency_boot.rs",
            &["let truncated = rows.len() > limit;"],
        ),
        (
            "market_ram_store_boot.rs",
            &["let truncated = dataset.len() > limit;"],
        ),
        (
            "spot_crossverify_boot.rs",
            &["let truncated = rows.len() > limit;"],
        ),
        ("groww_spot_1m_boot.rs", &["if rows.len() > cap {"]),
    ];
    for (file, needles) in pins {
        let prod = stripped_production(file);
        for needle in needles {
            assert!(
                prod.contains(needle),
                "{file}: strict `> cap` truncation flag missing: `{needle}` — \
                 an exact-boundary dataset must read COMPLETE (2026-07-18 \
                 convention)"
            );
        }
    }
}

/// Negative scan: the refuse-at-cap shape must never return to any probe
/// file's production region.
#[test]
fn no_refuse_at_limit_comparison_remains() {
    for file in PROBE_FILES {
        let prod = stripped_production(file);
        for needle in banned_needles() {
            assert!(
                !prod.contains(needle),
                "{file}: banned refuse-at-cap comparison `{needle}` found in \
                 the production region — the LIMIT+1 probe convention flags \
                 truncation ONLY at `len > cap` (query cap + 1 rows); an \
                 exactly-cap dataset is a legitimately complete day"
            );
        }
    }
}

/// Scanner self-test: a planted violation must be detected, comment-only
/// mentions must not, and the URL-scheme carve-out holds.
#[test]
fn scanner_self_test_detects_violation() {
    let violating = "fn parse(d: &[u8], limit: usize) -> bool {\n    d.len() >= limit\n}\n";
    let stripped: String = violating
        .lines()
        .map(code_portion)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        banned_needles().iter().any(|n| stripped.contains(n)),
        "scanner must detect a planted `.len() >= limit` violation"
    );

    let comment_only = "// the old shape was d.len() >= limit — banned\nlet x = 1;\n";
    let stripped: String = comment_only
        .lines()
        .map(code_portion)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !banned_needles().iter().any(|n| stripped.contains(n)),
        "comment-only mentions must not trip the scanner"
    );

    // URL-scheme carve-out: `://` is code, not a comment start.
    assert_eq!(
        code_portion(r#"let url = "http://x"; // tail"#),
        r#"let url = "http://x"; "#
    );

    // Test-region carve-out: the split drops everything below the first
    // column-0 #[cfg(test)] line.
    let with_tests = "let a = 1;\n#[cfg(test)]\nmod tests { let b_len_ge = 2; }\n";
    assert_eq!(production_region(with_tests), "let a = 1;");
}
