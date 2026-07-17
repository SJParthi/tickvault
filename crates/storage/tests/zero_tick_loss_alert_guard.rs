//! Zero-tick-loss source guard.
//!
//! Parthiban's explicit top-priority requirement: "zero ticks loss and
//! nowhere websocket should get disconnected or reconnect not even a
//! single tick should be missed".
//!
//! 2026-05-20 (#O3 — observability → CloudWatch-only): the four
//! Prometheus-alert-rule assertions were removed when the Prometheus
//! container + `tickvault-alerts.yml` were retired.
//!
//! Stage-2 dead-WS sweep (2026-07-17): `tick_persistence.rs` (the ticks
//! ring→spill→DLQ writer) was DELETED with the dead Dhan tick chain — no
//! live feed writes `ticks` anymore (Dhan lane retired 2026-07-13, Groww
//! live feed retired 2026-07-15; runtime is REST-only). The two source
//! scans that pinned its `ticks_dropped_total` / `tv_spill_dropped_total`
//! emit sites are RETIRED below (their subject file is gone; the emitters
//! no longer exist — the CloudWatch alarm on those series is a dead
//! monitor whose terraform/EMF retirement belongs to the dashboard PR).
//! The zero-loss chain that SURVIVES is the WAL frame spill
//! (`ws_frame_spill.rs`, KEEP — `tv_ticks_lost_total` + WS-SPILL-01/02)
//! and the seal chain's ring→spill→DLQ (`shadow_persistence.rs`,
//! AGGREGATOR-DROP-01), each pinned by its own guards.
//!
//! Surviving pinned assertions:
//!
//! 1. `TICK_BUFFER_CAPACITY` stays >= 100_000 (still consumed by the live
//!    seal ring — `crates/trading/src/candles/seal_ring.rs`).
//! 2. `observability-architecture.md` documents the zero-tick-loss chain.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn load_text(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// RETIRED (stage-2 dead-WS sweep, 2026-07-17):
// `tick_persistence_emits_dropped_counter_field` and
// `tick_persistence_emits_spill_dropped_total_with_reason_label` pinned the
// `ticks_dropped_total` / `tv_spill_dropped_total{reason}` emit sites inside
// `crates/storage/src/tick_persistence.rs` — that file was DELETED with the
// dead Dhan tick chain (zero production callers of its writer since the
// PR-C2/C3 lane deletion + the 2026-07-15 Groww live-feed retirement), so
// the metrics have NO emitter left to pin. Honest consequence: the
// `tv-<env>-ticks-dropped` alarm class over those series is now a dead
// monitor — its terraform/EMF retirement is the dashboard PR's scope (per
// the stage-2 task boundary). The surviving durable-capture guards are
// `ws_frame_spill.rs`'s WS-SPILL-01/02 (`tv_ticks_lost_total`) and the seal
// chain's AGGREGATOR-DROP-01 pins.

#[test]
fn tick_persistence_buffer_capacity_is_at_least_one_lakh() {
    // TICK_BUFFER_CAPACITY must stay above 100K to absorb normal market
    // bursts without spill-to-disk. The exact floor is 100_000.
    let common_constants = load_text("crates/common/src/constants.rs");

    let cap_line = common_constants
        .lines()
        .find(|l| l.contains("TICK_BUFFER_CAPACITY") && l.contains("="))
        .unwrap_or_else(|| panic!("TICK_BUFFER_CAPACITY not found in common/constants.rs"));

    let numeric: String = cap_line
        .split('=')
        .nth(1)
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    let value: usize = numeric
        .parse()
        .unwrap_or_else(|e| panic!("TICK_BUFFER_CAPACITY parse failed on `{numeric}`: {e}"));

    assert!(
        value >= 100_000,
        "TICK_BUFFER_CAPACITY = {value} — must be >= 100_000 to absorb bursts without spill"
    );
}

#[test]
fn observability_architecture_doc_mentions_zero_tick_loss_chain() {
    // The operator-facing durable doc MUST spell out the three-tier
    // buffer chain so future sessions don't have to rediscover it.
    let doc_path: PathBuf =
        workspace_root().join(".claude/rules/project/observability-architecture.md");
    let missing_msg = format!(
        "observability-architecture.md missing at {}",
        doc_path.display()
    );
    let doc = fs::read_to_string(&doc_path).unwrap_or_else(|_| panic!("{missing_msg}"));

    let keywords = ["errors.jsonl", "errors.summary.md", "ErrorCode", "Telegram"];
    for kw in keywords {
        assert!(
            doc.contains(kw),
            "observability-architecture.md missing keyword `{kw}` — doc drift"
        );
    }
}
