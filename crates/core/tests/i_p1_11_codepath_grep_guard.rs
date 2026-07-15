//! Audit Finding #8 (2026-05-03): the I-P1-11 composite-key rule MUST
//! be cited in the actual enforcement code/test files, not only in the
//! `.claude/rules/project/security-id-uniqueness.md` rule document.
//!
//! Prior state: `I-P1-11` appeared in code/test comments but a reviewer
//! grepping the source tree could miss the link from rule → enforcement.
//! This test ratchets the cross-reference so any future refactor that
//! removes the I-P1-11 mention from the enforcement files fails the build.
//!
//! Files that MUST cite "I-P1-11":
//! - (tick_gap_detector.rs entry RETIRED in PR-C3, 2026-07-14 — the
//!   detector was deleted with the Dhan WS lane per operator Q4-ii
//!   2026-07-13; its composite-key pin died with the module)
//! - `crates/storage/tests/dedup_segment_meta_guard.rs` — meta-guard that
//!   scans every DEDUP UPSERT KEYS for segment qualification
//! - `crates/common/src/instrument_registry.rs` — composite_index,
//!   `get_with_segment`
//!
//! Without this guard, the audit could happen again: a reviewer
//! greps for I-P1-11 and finds only docs, then concludes (wrongly)
//! that the rule is not enforced.

use std::fs;
use std::path::PathBuf;

const ENFORCEMENT_FILES: &[&str] = &[
    // tick_gap_detector.rs entry retired in PR-C3 (2026-07-14).
    "../../crates/storage/tests/dedup_segment_meta_guard.rs",
    "../../crates/common/src/instrument_registry.rs",
];

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

#[test]
fn i_p1_11_appears_in_every_enforcement_file_not_just_docs() {
    for rel in ENFORCEMENT_FILES {
        let text = read(rel);
        assert!(
            text.contains("I-P1-11"),
            "{rel} must cite `I-P1-11` so a reviewer grepping for the \
             rule identifier finds the enforcement, not just the docs. \
             Audit Finding #8 (2026-05-03)."
        );
    }
}

#[test]
fn i_p1_11_appears_in_security_id_uniqueness_rule_doc() {
    // Sanity: the rule doc itself must still cite I-P1-11. Catches an
    // accidental rename of the identifier in the source-of-truth doc.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.claude/rules/project/security-id-uniqueness.md");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    assert!(
        text.contains("I-P1-11"),
        "security-id-uniqueness.md must remain the source-of-truth for \
         the I-P1-11 identifier."
    );
}

// `tick_gap_detector_uses_composite_key_tuple` RETIRED in PR-C3
// (2026-07-14): the detector module it read was deleted with the Dhan WS
// lane (operator Q4-ii 2026-07-13). The composite-key discipline stays
// pinned by the surviving ENFORCEMENT_FILES entries above.
