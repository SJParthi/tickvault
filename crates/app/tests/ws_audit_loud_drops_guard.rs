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

#[test]
fn test_groww_drop_arm_is_loud() {
    let src = read_repo_file("src/groww_bridge.rs");
    let lines: Vec<&str> = src.lines().collect();
    let mut markers = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if !line.contains("row dropped") {
            continue;
        }
        markers += 1;
        let start = i.saturating_sub(12);
        let end = (i + 4).min(lines.len());
        // CODE lines only — `//` comments may legitimately mention `debug!`
        // (e.g. "upgraded from debug!"); only executable regressions fail.
        let region = lines[start..end]
            .iter()
            .filter(|l| !l.trim_start().starts_with("//"))
            .copied()
            .collect::<Vec<&str>>()
            .join("\n");
        assert!(
            region.contains("error!") && !region.contains("debug!"),
            "groww_bridge.rs: drop arm near line {} must log at error! (not debug!) — \
             silent drops are the 2026-07-05 regression class",
            i + 1
        );
        assert!(
            region.contains("AuditWs01EventWriteFailed"),
            "groww_bridge.rs: drop arm near line {} must carry code = AUDIT-WS-01",
            i + 1
        );
    }
    assert!(
        markers >= 1,
        "groww_bridge.rs: expected at least one ws_event_audit drop-arm marker"
    );
    assert!(
        src.contains("tv_ws_event_audit_dropped_total"),
        "groww_bridge.rs: the drop arm must increment tv_ws_event_audit_dropped_total{{reason}}"
    );
}

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
