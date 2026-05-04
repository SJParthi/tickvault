//! Phase 2.15 — security MEDIUM fix: bound the QuestDB response
//! body size in `prev_oi_cache::load_from_questdb`.
//!
//! What was at risk: the security review noted that
//! `response.text().await` reads the entire response body into a
//! `String` with no size limit. QuestDB is an internal trusted
//! service today, so a hostile response is improbable; but a
//! misconfigured QuestDB or matview-chain expansion could return
//! an unbounded body and OOM the loader.
//!
//! The fix:
//! - New `PREV_OI_CACHE_MAX_RESPONSE_BYTES` = 5 MB const
//! - Pre-check `response.content_length()` before allocating
//! - Post-check `body.len()` to catch chunked transfers without
//!   Content-Length
//! - Reject with new `LoadError::ResponseTooLarge { bytes, limit }`
//!   variant
//! - Phase 2.9 hard-fail then keeps the boot gate at
//!   AwaitingOiCache, so an attacker-controlled QuestDB cannot
//!   force boot to proceed with poisoned state

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
fn phase2_15_max_response_bytes_constant_pinned_at_5mb() {
    use tickvault_core::pipeline::prev_oi_cache::PREV_OI_CACHE_MAX_RESPONSE_BYTES;
    assert_eq!(
        PREV_OI_CACHE_MAX_RESPONSE_BYTES,
        5 * 1024 * 1024,
        "PREV_OI_CACHE_MAX_RESPONSE_BYTES must be 5 MB exactly"
    );
}

#[test]
fn phase2_15_response_too_large_error_variant_exists() {
    let src = read("core/src/pipeline/prev_oi_cache.rs");
    assert!(
        src.contains("ResponseTooLarge { bytes: u64, limit: u64 }")
            || src.contains("ResponseTooLarge {\n        bytes: u64,\n        limit: u64,\n    }"),
        "LoadError must declare ResponseTooLarge {{ bytes, limit }} variant"
    );
}

#[test]
fn phase2_15_load_from_questdb_checks_content_length() {
    let src = read("core/src/pipeline/prev_oi_cache.rs");
    assert!(
        src.contains("response.content_length()"),
        "load_from_questdb must check content_length() before allocating the body String"
    );
    assert!(
        src.contains("PREV_OI_CACHE_MAX_RESPONSE_BYTES"),
        "load_from_questdb must compare against PREV_OI_CACHE_MAX_RESPONSE_BYTES"
    );
}

#[test]
fn phase2_15_load_from_questdb_post_check_handles_chunked_transfer() {
    let src = read("core/src/pipeline/prev_oi_cache.rs");
    assert!(
        src.contains("body.len() as u64) > PREV_OI_CACHE_MAX_RESPONSE_BYTES")
            || src.contains("(body.len() as u64) > PREV_OI_CACHE_MAX_RESPONSE_BYTES"),
        "load_from_questdb must post-check body.len() to catch chunked transfers without Content-Length"
    );
}

#[test]
fn phase2_15_response_too_large_emits_warn_log_with_diagnostic() {
    let src = read("core/src/pipeline/prev_oi_cache.rs");
    assert!(
        src.contains("exceeds bound"),
        "ResponseTooLarge path must emit a warn log mentioning 'exceeds bound' for operator visibility"
    );
}
