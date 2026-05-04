//! Phase 2.14 — security MEDIUM fix: DLQ writer's `reason` field
//! tightened to `&'static str`.
//!
//! What was at risk: the security review noted that the NDJSON DLQ
//! writer accepts `reason: &str`. Today all callers use string literals
//! (`"spill_open_failed"`, `"spill_write_failed"`, test fixtures), so
//! injection is not active. But if a future caller passes a runtime
//! string containing `"` or `\`, the NDJSON line would be malformed
//! and unparseable during audit recovery.
//!
//! The fix: change the signature to `reason: &'static str`. The type
//! system rejects any runtime string at the call site at compile time.
//! This is a no-op runtime change but a hard compile-time contract.
//!
//! All current callers use string literals (which are `&'static str`),
//! so the change is backward compatible and required no call-site
//! updates.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    let me = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    me.parent()
        .expect("tickvault root")
        .parent()
        .expect("workspace root")
        .to_path_buf()
        .join("crates")
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|err| panic!("read {p:?}: {err}"))
}

#[test]
fn phase2_14_write_to_dlq_signature_is_static_str() {
    let src = read("storage/src/tick_persistence.rs");
    assert!(
        src.contains("fn write_to_dlq(&mut self, tick: &ParsedTick, reason: &'static str)"),
        "write_to_dlq must accept `reason: &'static str` (Phase 2.14 security MEDIUM fix)"
    );
}

#[test]
fn phase2_14_legacy_str_signature_removed() {
    let src = read("storage/src/tick_persistence.rs");
    // The pre-Phase-2.14 signature `reason: &str` (without 'static) must
    // not exist. Defence-in-depth: a future commit cannot revert the
    // hardening without tripping this ratchet.
    assert!(
        !src.contains("fn write_to_dlq(&mut self, tick: &ParsedTick, reason: &str)"),
        "write_to_dlq must NOT accept the legacy `reason: &str` (without 'static lifetime)"
    );
}

#[test]
fn phase2_14_doc_comment_explains_security_rationale() {
    let src = read("storage/src/tick_persistence.rs");
    assert!(
        src.contains("Phase 2.14 security MEDIUM fix"),
        "write_to_dlq doc comment must explain the security rationale"
    );
}
