//! ERROR-CODE PAGING DECISION GUARD (2026-10-12, deep audit OBS-7).
//!
//! 137 error codes exist and about a third are referenced by any terraform
//! alarm, metric filter or alarm description. The rest are log-only on
//! purpose (the Dhan noise lock keeps the page set small), but nothing made
//! that a DECISION: a new code landed log-only silently, visible only by
//! searching the cloud log within its retention.
//!
//! `every_error_code_is_alarmed_or_listed_log_only` makes it one. Every
//! `ErrorCode::all()` variant's code string must either appear in
//! `deploy/aws/terraform/*.tf` (an alarm, a filter pattern, or the DO step of
//! a loss-group page) or be listed in `LOG_ONLY_CODES` below. A new code in
//! neither fails the build, so its author decides. A listed code that becomes
//! referenced by terraform, or is deleted, is stale and must leave the list.
//!
//! This adds NO page and NO alarm; a digest page for unalarmed codes still
//! needs a dated noise-lock row first (dhan-rest-only-noise-lock §3). The
//! non-paging way to see these codes today is the logs MCP tool
//! `list_novel_signatures`.
//!
//! HONEST LIMITS: "referenced by terraform" is a text match, so a code named
//! only in a comment or an alarm description counts; it does not prove a
//! filter matches the code. The seeded list below was recorded at guard
//! creation as the codes then unreferenced, not individually reviewed: each
//! is "log-only by the noise lock" until someone gives it a page.

use std::path::{Path, PathBuf};

use tickvault_common::error_code::ErrorCode;

/// Codes deliberately log-only (no terraform reference). Sorted, unique.
/// SHRINK FREELY; ADD only with a reason in the PR that adds the code.
/// Seeded 2026-10-12 from the then-unreferenced set (89 codes).
const LOG_ONLY_CODES: &[&str] = &[
    "AGGREGATOR-HB-01",
    "AGGREGATOR-LAG-01",
    "AGGREGATOR-LATE-01",
    "API-SERVER-01",
    "AUDIT-02",
    "AUDIT-03",
    "AUDIT-04",
    "AUDIT-05",
    "AUTH-GAP-02",
    "BAR-MISMATCH-01",
    "BAR-MISMATCH-02",
    "BAR-MISMATCH-03",
    "DATA-800",
    "DATA-804",
    "DATA-806",
    "DATA-807",
    "DATA-808",
    "DATA-809",
    "DATA-810",
    "DATA-811",
    "DATA-812",
    "DATA-813",
    "DATA-814",
    "DH-902",
    "DH-903",
    "DH-907",
    "DH-908",
    "DH-909",
    "DH-910",
    "DHAN-LIVE-XVERIFY-01",
    "DRAIN-HOLD-01",
    "EXIT-ORDER-01",
    "EXIT-VERIFY-01",
    // 2026-10-28: the daily expiry check; log only (Dhan noise lock §3).
    "EXPIRY-SCOPE-01",
    "FEED-GAP-01",
    "FEED-REJECT-01",
    "FEED-SUPERVISOR-01",
    "FOLD-01",
    "FUTIDX-01",
    "FUTIDX-02",
    "GAP-NET-01",
    "HOT-PATH-01",
    "HOT-PATH-03",
    // 2026-10-30 (plan ITEM 52f): the telemetry thread did not start. One
    // boot line; nothing is lost, only the stall page goes dark. Log only,
    // like HOT-PATH-03.
    "HOT-PATH-04",
    "HTTP-CLIENT-01",
    "I-P0-03",
    "I-P1-05",
    "I-P1-06",
    "I-P1-08",
    "I-P1-11",
    "I-P2-02",
    "INDEX-OHLC-02",
    "LAMBDA-AWS-01",
    "LAMBDA-AWS-02",
    "LAMBDA-CONFIG-01",
    "LAMBDA-MINT-01",
    "LAMBDA-NOTIFY-01",
    "LAMBDA-PORTAL-01",
    "LAMBDA-PROV-01",
    "LAMBDA-START-01",
    "LAMBDA-START-02",
    "OMS-GAP-01",
    "OMS-GAP-02",
    "OMS-GAP-04",
    "OMS-GAP-05",
    "ORDER-EVT-01",
    "ORDER-READY-01",
    "PIPELINE-LAG-01",
    "PREVCLOSE-01",
    "PREVCLOSE-02",
    "PREVCLOSE-03",
    "PREVCLOSE-04",
    "PREVOI-01",
    "PROC-03",
    "RAMSTORE-01",
    "RESOURCE-01",
    "RESOURCE-03",
    "RISK-GAP-02",
    "SELFTEST-01",
    "SELFTEST-02",
    "SPOT1M-02",
    "STORAGE-GAP-02",
    "TELEGRAM-02",
    "TELEGRAM-03",
    "TF-VERIFY-01",
    "TF-VERIFY-02",
    "TICK-GAP-01",
    "WS-GAP-01",
    "WS-GAP-04",
    "WS-GAP-10",
    "WS-SPILL-03",
    "WS-SPILL-04",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("error_code_paging_decision_guard: cannot canonicalize repo root")
}

fn is_code_byte(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-'
}

/// True when `code` appears in `text` as a whole code (not inside a longer one).
fn text_references_code(text: &str, code: &str) -> bool {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(rel) = text[from..].find(code) {
        let start = from + rel;
        let end = start + code.len();
        let left_ok = start == 0 || !is_code_byte(bytes[start - 1]);
        let right_ok = end >= bytes.len() || !is_code_byte(bytes[end]);
        if left_ok && right_ok {
            return true;
        }
        from = end;
    }
    false
}

fn terraform_text() -> String {
    let dir = repo_root().join("deploy/aws/terraform");
    let mut out = String::new();
    let mut n = 0;
    for entry in std::fs::read_dir(&dir).expect("terraform dir").flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "tf") {
            out.push_str(&std::fs::read_to_string(&p).expect("tf readable"));
            out.push('\n');
            n += 1;
        }
    }
    assert!(n > 20, "only {n} terraform files read");
    out
}

#[test]
fn every_error_code_is_alarmed_or_listed_log_only() {
    let mut sorted = LOG_ONLY_CODES.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted, LOG_ONLY_CODES,
        "LOG_ONLY_CODES must be sorted and unique"
    );

    let tf = terraform_text();
    let all: Vec<&str> = ErrorCode::all().iter().map(|c| c.code_str()).collect();
    assert!(all.len() > 100, "only {} error codes", all.len());

    let referenced: Vec<&str> = all
        .iter()
        .copied()
        .filter(|c| text_references_code(&tf, c))
        .collect();
    // Anti-vacuity: the alarmed set is real.
    assert!(
        referenced.len() > 20,
        "only {} codes referenced by terraform; the matcher is broken",
        referenced.len()
    );

    let undecided: Vec<&str> = all
        .iter()
        .copied()
        .filter(|c| !referenced.contains(c) && !LOG_ONLY_CODES.contains(c))
        .collect();
    assert!(
        undecided.is_empty(),
        "error code(s) {undecided:?} have no terraform alarm or filter and are not in LOG_ONLY_CODES. Decide: add the code to LOG_ONLY_CODES (it stays log-only, the noise-lock default), or give it an alarm after a dated noise-lock row."
    );

    let stale: Vec<&str> = LOG_ONLY_CODES
        .iter()
        .copied()
        .filter(|c| !all.contains(c) || referenced.contains(c))
        .collect();
    assert!(
        stale.is_empty(),
        "STALE LOG_ONLY_CODES row(s) {stale:?}: the code was deleted, or terraform now references it. Remove the row in the same PR."
    );
}

#[test]
fn code_reference_matcher_bites() {
    let tf = "pattern = \"{ $.code = \\\"HOT-PATH-02\\\" }\" # see WS-GAP-03 and AUDIT-06X";
    assert!(text_references_code(tf, "HOT-PATH-02"));
    assert!(text_references_code(tf, "WS-GAP-03"));
    assert!(
        !text_references_code(tf, "AUDIT-06"),
        "a longer code must not match"
    );
    assert!(
        !text_references_code(tf, "PATH-02"),
        "a suffix must not match"
    );
    assert!(
        !text_references_code(tf, "WS-GAP-0"),
        "a prefix must not match"
    );
}
