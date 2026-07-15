//! Source-scan ratchet — ws_event_audit loud drops + ILP-HTTP transport (2026-07-05).
//!
//! The operator caught `ws_event_audit` (feed='groww') EMPTY on 2026-07-05
//! (silent false-OK class, audit-findings Rule 11). Two windows were closed:
//! (1) the Groww bridge `try_send` drop arm logged at `debug!` with no counter;
//! (2) the writer flushed over ILP TCP — fire-and-forget, so a server-side
//! reject never surfaced as `Err` and the AUDIT-WS-01 flush arm never paged.
//! This guard fails the build on regression of either fix.

use std::fs;
use std::path::PathBuf;

fn read_repo_file(rel_from_app: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_from_app);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// RETIRED 2026-07-15 (Groww live-feed deletion): test_groww_drop_arm_is_loud
// died with groww_bridge.rs (the app-side drop arms are gone); the storage
// writer ILP-HTTP pin below survives.

#[test]
fn test_ws_event_audit_writer_uses_ilp_http() {
    // The transport ratchet, scanned cross-crate (the storage unit test
    // `test_ilp_http_conf_targets_http_port` pins the helper OUTPUT; this scan
    // pins that the production constructor cannot silently regress to a raw
    // `tcp::addr` format! bypassing the helper).
    let src = read_repo_file("../storage/src/ws_event_audit_persistence.rs");
    assert!(
        src.contains("http::addr="),
        "ws_event_audit_persistence.rs must build an ILP-over-HTTP conf \
         (per-flush server ACK → AUDIT-WS-01 on reject)"
    );
    assert!(
        !src.contains("tcp::addr="),
        "ws_event_audit_persistence.rs must NOT use ILP TCP — fire-and-forget \
         flush restores the 2026-07-05 silent-empty-table window"
    );
    assert!(
        src.contains("fn ilp_http_conf"),
        "the conf string must come from the unit-tested ilp_http_conf helper"
    );
}
