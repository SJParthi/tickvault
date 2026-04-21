//! Ratchet test: depth retry cap was raised from 20 to 60 on 2026-04-21.
//!
//! See `.claude/queues/production-fixes-2026-04-21.md` items I1 + I11.
//!
//! Production evidence: all 4 depth-200 connections exhausted the 20-attempt
//! budget during market hours. Dhan repeatedly TCP-reset the sockets with
//! `Protocol(ResetWithoutClosingHandshake)` *despite the strikes being ATM*
//! (Parthiban verified the Python SDK works on the same account with the
//! same strikes — so it is NOT off-ATM / dataPlan / IP). The root protocol
//! diff is tracked separately in queue item I14.
//!
//! This PR raises the tolerance only. Regression = fewer than 60 retries
//! means the process gives up in under 30 minutes of market-hours silence
//! from Dhan, which is the exact failure that hit production today.

use std::fs;
use std::path::PathBuf;

fn repo_path(rel: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push(rel);
    p
}

fn read(rel: &str) -> String {
    let path = repo_path(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn depth_reconnect_max_attempts_is_at_least_60() {
    let src = read("crates/core/src/websocket/depth_connection.rs");
    assert!(
        src.contains("const DEPTH_RECONNECT_MAX_ATTEMPTS: u64 = 60;"),
        "DEPTH_RECONNECT_MAX_ATTEMPTS must be exactly 60 (raised 20→60 \
         on 2026-04-21). Regression = process gives up in <30 min of \
         Dhan TCP-reset storm, which caused the 2026-04-21 outage."
    );
}

#[test]
fn websocket_rule_file_documents_60_cap() {
    let src = read(".claude/rules/project/websocket-enforcement.md");
    assert!(
        src.contains("Depth connections cap at 60 retry attempts"),
        "websocket-enforcement.md rule 14 must document the 60-attempt \
         cap so future editors see the constraint without digging into \
         depth_connection.rs."
    );
    assert!(
        !src.contains("Depth connections cap at 20 retry attempts"),
        "websocket-enforcement.md must NOT still say '20 retry attempts' \
         — the cap was raised to 60 on 2026-04-21."
    );
}

#[test]
fn queue_file_exists_so_future_sessions_pick_up_remaining_items() {
    let src = read(".claude/queues/production-fixes-2026-04-21.md");
    // Sanity: the queue file exists and lists I14 (the root-cause
    // investigation) so no future session loses track of it.
    assert!(
        src.contains("I14"),
        "queue file must list I14 (Python-vs-Rust depth-200 protocol \
         diff) — that's the real root cause, this PR is just tolerance."
    );
    assert!(
        src.contains("I1"),
        "queue file must list I1 (this PR's scope)."
    );
}
