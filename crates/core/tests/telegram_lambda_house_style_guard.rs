//! Telegram UX Overhaul (2026-07-07) — Lambda house-style ratchet.
//!
//! REPOINTED 2026-07-18 (rust-only phase 2b-2 wave 1): the Lambda is now
//! the Rust module `crates/aws-lambdas/src/telegram_webhook.rs` (the
//! Python `deploy/aws/lambda/telegram-webhook/handler.py` was ported 1:1
//! and DELETED in the same PR). This guard is the INDEPENDENT second
//! layer on top of the crate's own unit tests (the Test matrix
//! `aws-lambdas` lane): those tests live next to the module and can be
//! edited in the same commit as a regression; this guard pins the
//! judge-contract invariants from a DIFFERENT crate on every build:
//!
//! (a) `house_line` / `ist_12h` / `fold_records` exist — the
//!     house-style formatter chain cannot be silently deleted
//! (b) the legacy `Reason: {reason}` raw-threshold line never returns
//!     to the Telegram text path (NewStateReason is tracing-forensics
//!     ONLY — CloudWatch Logs, never the operator surface)
//! (c) `parse_mode` never returns to the payload (plain-text mode kills
//!     the unescaped-Markdown silent-400 drop class)
//! (d) ALARM-state records are NEVER routed through the warm-cache OK
//!     suppression (`should_suppress_ok` is consulted ONLY on the
//!     lone-OK branch — a dropped 🆘 is unacceptable)
//! (e) never-drop delivery boundary: malformed records fail open to the
//!     generic safe line, `ist_12h` degrades instead of crashing on
//!     malformed/edge-dated timestamps, and every folded text is
//!     iterated straight into the Telegram POST (single choke point)
//!
//! House pattern: source-order scans (see `episode_edit_wiring_guard.rs`).

use std::fs;
use std::path::Path;

fn module_src() -> String {
    fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../aws-lambdas/src/telegram_webhook.rs"
    ))
    .expect("crates/aws-lambdas/src/telegram_webhook.rs must exist")
}

/// The PRODUCTION region of the module — everything above the first
/// column-0 `#[cfg(test)]` line (the house production-region split), so
/// test fixtures carrying raw alarm JSON can never satisfy or trip the
/// scans below.
fn production_region(src: &str) -> &str {
    match src.find("\n#[cfg(test)]") {
        Some(cut) => &src[..cut],
        None => src,
    }
}

/// Extracts the region of `src` between `start` (inclusive) and the next
/// occurrence of `end` after it (exclusive). Panics loudly on a missing
/// marker so a rename fails the build instead of vacuously passing.
fn region<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
    let s = src
        .find(start)
        .unwrap_or_else(|| panic!("region start marker missing: {start}"));
    let rest = &src[s..];
    let e = rest[start.len()..]
        .find(end)
        .map(|i| i + start.len())
        .unwrap_or_else(|| panic!("region end marker missing: {end}"));
    &rest[..e]
}

// (a) The house-style formatter chain exists.
#[test]
fn guard_house_line_ist_12h_and_fold_records_present() {
    let src = module_src();
    let prod = production_region(&src);
    for needle in [
        "pub fn house_line(",
        "pub fn ist_12h(",
        "pub fn fold_records(",
        "pub fn should_suppress_ok(",
        "ALARM_PHRASES",
    ] {
        assert!(
            prod.contains(needle),
            "telegram_webhook.rs house-style chain regressed — missing {needle:?}"
        );
    }
    // The fold output is what reaches Telegram (single choke point):
    // `handle` routes the SNS records through fold_records.
    assert!(
        prod.contains("fold_records(&records, now, cache)"),
        "handle must route records through fold_records"
    );
}

// (b) Raw CloudWatch NewStateReason never reaches the Telegram text path.
#[test]
fn guard_new_state_reason_never_in_telegram_text() {
    let src = module_src();
    let prod = production_region(&src);
    assert!(
        !prod.contains("Reason: {"),
        "the legacy 'Reason: {{reason}}' raw-threshold line must never \
         return to the Telegram text path (house-style contract)"
    );
    // Every executable reference to NewStateReason must live on either
    // (1) the tracing forensics binding (CloudWatch Logs) or (2) the
    // fallback REDACTOR's key constant (MED-1 fix round — the scanner
    // that BLANKS the value when a valid-but->128-deep alarm JSON fails
    // serde_json's recursion limit and takes the plain-SNS fallback).
    // Never in a rendered text. Comments/doc lines may explain in prose.
    let mut forensics_refs = 0usize;
    let mut redactor_key_refs = 0usize;
    for (idx, line) in prod.lines().enumerate() {
        if !line.contains("NewStateReason") {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        if line.contains("forensics_reason") {
            forensics_refs += 1;
        } else if trimmed.starts_with("const KEY:") {
            redactor_key_refs += 1;
        } else {
            panic!(
                "telegram_webhook.rs line {}: NewStateReason outside the \
                 forensics binding / redactor key const: {line:?}",
                idx + 1
            );
        }
    }
    assert_eq!(
        forensics_refs, 1,
        "exactly ONE forensics-binding NewStateReason reference (feeding \
         the alarm-forensics tracing line)"
    );
    assert_eq!(
        redactor_key_refs, 1,
        "exactly ONE redactor key-const NewStateReason reference (the \
         fallback scanner that blanks the value)"
    );
    // The redaction + cap are WIRED into the plain-SNS fallback — the
    // parse-failure arm can never dump raw forensic text into Telegram.
    for needle in [
        "pub fn redact_new_state_reason(",
        "cap_fallback_body(redact_new_state_reason(&raw_body))",
        "PLAIN_FALLBACK_BODY_MAX_CHARS",
    ] {
        assert!(
            prod.contains(needle),
            "telegram_webhook.rs fallback redaction chain regressed — \
             missing {needle:?}"
        );
    }
    // …and the forensics binding feeds ONLY the CloudWatch-Logs tracing
    // event, which exists.
    assert!(
        prod.contains("\"alarm-forensics\""),
        "the alarm-forensics tracing line (CloudWatch Logs destination) \
         must exist"
    );
    assert_eq!(
        prod.matches("forensics_reason").count(),
        2,
        "forensics_reason must appear exactly twice (binding + tracing \
         field) — any further use risks leaking the raw reason into a text"
    );
}

// (c) Plain-text mode: no markup-parsing field in the Telegram payload.
#[test]
fn guard_parse_mode_absent_from_payload() {
    let src = module_src();
    let prod = production_region(&src);
    for (idx, line) in prod.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            // Doc comments may legitimately explain the plain-text
            // decision in prose.
            continue;
        }
        assert!(
            !line.contains("parse_mode"),
            "telegram_webhook.rs line {}: the parse_mode payload key must \
             never return to the Lambda Telegram payload — plain-text mode \
             kills the unescaped-Markdown silent-400 drop class",
            idx + 1
        );
        assert!(
            !line.contains("\"Markdown\""),
            "telegram_webhook.rs line {}: no Markdown mode literal may \
             reappear in the payload",
            idx + 1
        );
    }
    // The payload stays the exact 3-key plain-text form.
    assert!(
        prod.contains("(\"chat_id\", chat_id.to_string())")
            && prod.contains("(\"text\", text.to_string())")
            && prod.contains("(\"disable_web_page_preview\", \"true\".to_string())"),
        "telegram_form_pairs must keep the exact 3-key plain-text payload"
    );
}

// (d) ALARM is never routed through the warm-cache OK suppression.
#[test]
fn guard_alarm_never_suppressed_by_warm_cache() {
    let src = module_src();
    let prod = production_region(&src);
    // Exactly ONE call site of should_suppress_ok (plus its def).
    let def_count = prod.matches("pub fn should_suppress_ok(").count();
    let total = prod.matches("should_suppress_ok(").count();
    assert_eq!(def_count, 1, "warm-cache helper must exist exactly once");
    assert_eq!(
        total - def_count,
        1,
        "exactly ONE should_suppress_ok call site (the lone-OK branch)"
    );
    // The call site lives in the lone-OK fold branch…
    let lone_ok = region(prod, "// Lone OK flip", "// ALARM / INSUFFICIENT_DATA");
    assert!(
        lone_ok.contains("should_suppress_ok("),
        "the suppression check must live in the lone-OK branch"
    );
    // …and the ALARM final-state arm never consults the cache to skip.
    let alarm_arm = region(
        prod,
        "// ALARM / INSUFFICIENT_DATA",
        "if !lone_ok_phrases.is_empty()",
    );
    assert!(
        !alarm_arm.contains("should_suppress_ok"),
        "ALARM records must NEVER be routed through the warm-cache suppression"
    );
    assert!(
        alarm_arm.contains("out.push(house_line(alarm));"),
        "ALARM records must always emit an individual house line"
    );
}

// (e) Never-drop delivery boundary + fail-open render chain.
#[test]
fn guard_never_drop_delivery_boundary_and_fail_open_render() {
    let src = module_src();
    let prod = production_region(&src);
    // ist_12h degrades on malformed/edge-dated StateChangeTime instead of
    // crashing (the python OverflowError regression class — the Rust arm
    // falls back to the invocation time).
    let ist = region(prod, "pub fn ist_12h(", "pub fn ");
    assert!(
        ist.contains("unwrap_or_else(|| Utc::now().with_timezone(&ist()))"),
        "ist_12h lost its malformed/edge-dated-timestamp fail-open arm"
    );
    // Malformed SNS records fold to the generic safe line — never a
    // crash, never raw JSON, never a dropped batch.
    assert!(
        prod.contains("plain_texts.push(GENERIC_SAFE_LINE.to_string())"),
        "fold_records lost the malformed-record generic-safe-line fail-open arm"
    );
    // handle: every folded text is iterated straight into the Telegram
    // POST (single choke point — no filtering between fold and POST).
    let send = region(prod, "pub async fn send_texts", "pub async fn deliver");
    assert!(
        send.contains("for text in texts") && send.contains("post(text.clone()).await"),
        "send_texts must POST every folded text — no filtering between \
         fold_records and the Telegram POST"
    );
    assert!(
        prod.contains("send_texts(&texts, records.len()"),
        "handle must deliver through the send_texts choke point"
    );
}

// Bonus pin 1: the crate's own test lane carries the judge-contract test
// names (ported 1:1 from the python suite — the Test matrix aws-lambdas
// lane runs them on every PR).
#[test]
fn guard_rust_test_lane_carries_contract_tests() {
    let src = module_src();
    for name in [
        "test_house_line_no_raw_threshold_json",
        "test_ok_flip_single_line_recovered",
        "test_alarm_ok_pair_in_batch_folds_to_recovered_only",
        "test_ist_12_hour_timestamp",
        "test_alarm_never_suppressed_by_warm_cache",
        "test_unknown_alarm_name_fallback_still_plain_english",
        "test_alarm_record_reaches_telegram_post_through_lambda_handler",
        "test_poisoned_timestamp_record_never_drops_genuine_alarm_in_batch",
        "test_edge_dated_timestamps_fall_back_without_crash",
    ] {
        assert!(
            src.contains(name),
            "telegram_webhook.rs lost the contract test {name:?}"
        );
    }
}

// Bonus pin 2: the ported python handler stays DELETED — a zombie
// handler.py reappearing next to the Rust module would mean two divergent
// Telegram formatters (the rollback path is a full PR revert, never a
// parallel python copy).
#[test]
fn guard_python_handler_stays_deleted() {
    let py = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy/aws/lambda/telegram-webhook/handler.py");
    assert!(
        !py.exists(),
        "deploy/aws/lambda/telegram-webhook/handler.py must stay deleted \
         (ported to crates/aws-lambdas/src/telegram_webhook.rs in phase \
         2b-2 wave 1) — a parallel python formatter would drift from the \
         Rust house-style contract"
    );
}
