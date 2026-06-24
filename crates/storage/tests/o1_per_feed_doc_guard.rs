//! Drift guard for the permanent O(1) per-feed architecture doc.
//!
//! `docs/architecture/o1-per-feed-uniqueness-dedup-mapping.md` is the operator-
//! mandated single source of truth (2026-06-24). This test fails the build if:
//!   1. the doc is moved/deleted (path is pinned),
//!   2. the doc drops a CORE claim (the canonical composite key, the 4 feed-keyed
//!      tables, or the honest envelope), OR
//!   3. the doc gains a DISHONEST claim — it must still carry the honest
//!      space + dedup caveats (anti-hallucination pin), AND
//!   4. the doc drifts from code — each feed-keyed DEDUP constant the doc names
//!      MUST still contain `feed` in the actual storage source.
//!
//! This keeps the doc honest + synced: you cannot weaken the code (remove `feed`
//! from a key) without the dedup meta-guard failing, and you cannot weaken the
//! doc (claim "O(1) space", drop the envelope) without THIS guard failing.

#![cfg(test)]

use std::path::PathBuf;

fn storage_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn read_doc() -> String {
    // crates/storage/ -> ../../docs/architecture/...
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/architecture/o1-per-feed-uniqueness-dedup-mapping.md");
    std::fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "the permanent O(1) per-feed architecture doc MUST exist at {} \
             (operator 2026-06-24 'I need this always'): {e}",
            p.display()
        )
    })
}

fn read_src(file: &str) -> String {
    let p = storage_src_dir().join(file);
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("storage src {} must be readable: {e}", p.display()))
}

#[test]
fn doc_exists_and_names_the_canonical_composite_key() {
    let doc = read_doc();
    assert!(
        doc.contains("(security_id, exchange_segment, feed)"),
        "doc MUST name the canonical per-feed composite key (security_id, exchange_segment, feed)"
    );
}

#[test]
fn doc_names_all_four_feed_keyed_tables() {
    let doc = read_doc();
    for table in ["ticks", "candles_", "prev_day_ohlcv", "ws_event_audit"] {
        assert!(
            doc.contains(table),
            "doc MUST name the feed-keyed table `{table}`"
        );
    }
}

#[test]
fn doc_carries_the_honest_envelope_and_caveats_no_hallucination() {
    let doc = read_doc();
    // Positive honesty pins — the doc must keep the truths the 3-agent
    // verification established. Removing any of these = a dishonest doc.
    let required = [
        // mandated envelope wording (operator-charter §F)
        "100% inside the tested envelope",
        // space is bounded O(N×F), NOT O(1)
        "O(N\u{00d7}F)",
        // dedup is QuestDB-side, not in-memory
        "QuestDB-side",
        // registry is std HashMap, not papaya
        "NOT papaya",
        // coverage is ratcheted floors, not literal 100%
        "ratcheted floors",
    ];
    for needle in required {
        assert!(
            doc.contains(needle),
            "doc MUST keep the honest claim containing `{needle}` — dropping it \
             would let the doc overstate the guarantee (anti-hallucination pin, \
             operator 2026-06-24 'no illusion')."
        );
    }
}

#[test]
fn doc_matches_code_every_feed_keyed_dedup_const_contains_feed() {
    // Bind the doc to reality: the 4 feed-keyed DEDUP constants the doc claims
    // carry `feed` MUST actually carry `feed` in the storage source. (The dedup
    // meta-guard enforces this independently; this re-asserts it from the doc's
    // perspective so doc + code can never silently diverge.)
    let checks = [
        ("tick_persistence.rs", "DEDUP_KEY_TICKS"),
        ("shadow_persistence.rs", "DEDUP_KEY_CANDLES"),
        ("prev_day_ohlcv_persistence.rs", "DEDUP_KEY_PREV_DAY_OHLCV"),
        ("ws_event_audit_persistence.rs", "DEDUP_KEY_WS_EVENT_AUDIT"),
    ];
    for (file, const_name) in checks {
        let src = read_src(file);
        // Find the const declaration, then the first quoted body (handles the
        // rustfmt multi-line `const X: &str =\n    "...";` form).
        let marker = format!("const {const_name}:");
        let pos = src.find(&marker).unwrap_or_else(|| {
            panic!("{const_name} must exist in {file} (doc references it as feed-keyed)")
        });
        let rest = &src[pos..];
        let q1 = rest.find('"').expect("const body opening quote");
        let after = &rest[q1 + 1..];
        let q2 = after.find('"').expect("const body closing quote");
        let body = &after[..q2];
        assert!(
            body.contains("feed"),
            "{const_name} in {file} = \"{body}\" MUST contain `feed` — the doc \
             claims this table is per-feed keyed; code + doc must agree."
        );
    }
}
