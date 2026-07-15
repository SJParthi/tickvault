//! C2 convergence ratchet — the ordered per-tick consumer sequence
//! (enrich → persist → aggregate handoff) exists in EXACTLY ONE place —
//! `pipeline::feed_consumer::consume_feed_tick` — and BOTH live-feed consumer
//! loops delegate to it.
//!
//! Mirrors the C1 guard (`crates/storage/tests/feed_tick_writer_convergence_guard.rs`):
//! a source-scan that fails the build if a future refactor silently re-forks the
//! consumer loop — the mechanical proof that the two feeds can never diverge on
//! the persist-then-aggregate order or the greeks-for-Dhan-only gate.
//!
//! ## The pinned honest scope (do not widen silently)
//!
//! - The Dhan `Tick` arm (Quote/Ticker packets — the production path under the
//!   Quote-mode locked universe) delegates. ✔
//! - The Groww `drain_new_data` per-line body delegates. ✔
//! - The Dhan `TickWithDepth` (Full-packet) arm does NOT delegate — its
//!   aggregate handoff (the broadcast) is separated from persist by depth-frame
//!   gating whose loop-`continue`s cannot route through the shared core without
//!   changing which frames are broadcast. That exception is PINNED below by the
//!   persist-call-site counts: adding ANY further hand-rolled persist site fails
//!   this guard.

use std::fs;
use std::path::{Path, PathBuf};

/// Read a source file relative to the core crate root. Panics with a clear
/// message if missing so a future rename surfaces loudly.
fn read_rel(rel: &str) -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Strip the `#[cfg(test)] mod tests { ... }` tail so the scan only sees the
/// PRODUCTION region (test bodies legitimately mention these patterns).
fn production_region(src: &str) -> &str {
    src.split("mod tests {")
        .next()
        .expect("file always has a head before any tests module")
}

#[test]
fn test_one_consumer_loop_path_guard() {
    // 1. The shared core is defined EXACTLY ONCE, in feed_consumer.rs.
    let core_mod = read_rel("src/pipeline/feed_consumer.rs");
    let core_prod = production_region(&core_mod);
    assert_eq!(
        core_prod.matches("pub fn consume_feed_tick").count(),
        1,
        "expected exactly ONE `pub fn consume_feed_tick` definition in feed_consumer.rs"
    );
    // The greeks-for-Dhan-only gate lives in the core as an EXHAUSTIVE match —
    // enrichment must run for Dhan and be structurally impossible for Groww.
    assert!(
        core_prod.contains("Feed::Dhan => enrich(tick)"),
        "the shared core must gate the enrich stage on Feed::Dhan"
    );
    assert!(
        core_prod.contains("Feed::Groww => {}"),
        "the shared core must have an explicit no-enrich arm for Feed::Groww \
         (exhaustive match — a new feed forces a decision)"
    );
    // The load-bearing order: persist(tick) precedes aggregate(tick) in the
    // core body (behavior additionally proven by
    // `test_consumer_persist_then_aggregate_order_preserved`).
    let persist_pos = core_prod
        .find("persist(tick);")
        .expect("core must call persist(tick)");
    let aggregate_pos = core_prod
        .find("aggregate(tick);")
        .expect("core must call aggregate(tick)");
    assert!(
        persist_pos < aggregate_pos,
        "persist must precede aggregate in the shared core"
    );

    // 2. The Dhan consumer loop delegates: the Tick arm routes its
    // enrich/persist/broadcast through the shared core.
    let dhan = read_rel("src/pipeline/tick_processor.rs");
    let dhan_prod = production_region(&dhan);
    assert!(
        dhan_prod.contains("feed_consumer::consume_feed_tick("),
        "run_tick_processor's Tick arm must delegate to the shared consume_feed_tick"
    );
    assert_eq!(
        dhan_prod.matches("consume_feed_tick(").count(),
        1,
        "tick_processor must call consume_feed_tick exactly once (the Tick arm); \
         the TickWithDepth arm is the documented depth-gating exception"
    );
    // No new `pub fn consume_feed_tick` fork in the processor file.
    assert_eq!(
        dhan_prod.matches("pub fn consume_feed_tick").count(),
        0,
        "tick_processor must not re-define the shared core"
    );
    // Pin the documented TickWithDepth exception: exactly TWO
    // `append_tick_enriched_with_seq(` persist sites exist in tick_processor —
    // one inside the consume_feed_tick persist closure (Tick arm), one in the
    // Full-packet depth arm. A THIRD hand-rolled persist site = a re-fork.
    assert_eq!(
        dhan_prod.matches(".append_tick_enriched_with_seq(").count(),
        2,
        "tick_processor persist sites re-forked — expected exactly 2 \
         (Tick-arm closure + the documented TickWithDepth exception)"
    );
    assert_eq!(
        dhan_prod.matches(".append_tick_with_seq(").count(),
        2,
        "tick_processor legacy-persist sites re-forked — expected exactly 2 \
         (Tick-arm closure + the documented TickWithDepth exception)"
    );

    // 3. The Groww consumer loop delegates: drain_new_data routes its
    // persist + 21-TF fold through the SAME shared core.
    let groww = read_rel("../app/src/groww_bridge.rs");
    let groww_prod = production_region(&groww);
    assert!(
        groww_prod.contains("tickvault_core::pipeline::feed_consumer::consume_feed_tick("),
        "run_groww_bridge (drain_new_data) must delegate to the shared consume_feed_tick"
    );
    assert_eq!(
        groww_prod.matches("consume_feed_tick(").count(),
        1,
        "the Groww bridge must have exactly ONE consume_feed_tick call site"
    );
    // The Groww TICK persist call exists ONLY inside the shared-core closure.
    // Pin the documented FeedGapAuditWriter exception: exactly TWO
    // `.append_row(` sites exist in the bridge's production region — the tick
    // persist inside the consume_feed_tick closure, plus the gap-episode
    // FeedGapAuditRow write (an audit-table row, not a tick persist — the
    // 2026-07-14 feed-gap tracker). A THIRD site = a re-forked persist path.
    assert_eq!(
        groww_prod.matches(".append_row(").count(),
        2,
        "the Groww bridge persist sites re-forked — expected exactly 2 \
         (the consume_feed_tick tick-persist closure + the documented \
         FeedGapAuditWriter gap-episode audit exception)"
    );
    // The tick-persist site itself stays singular: exactly ONE
    // `live_writer.append_row(` in the production region.
    assert_eq!(
        groww_prod.matches("live_writer.append_row(").count(),
        1,
        "the Groww bridge must persist TICKS through exactly ONE \
         live_writer.append_row( site (inside the consume_feed_tick persist \
         closure)"
    );
    // The Groww fold exists ONLY inside the shared-core closure: one
    // per-tick `.consume_tick(` site in the bridge's production region (the
    // catch-up/force-seal tasks route through catch_up_seal_all/force_seal_all,
    // not consume_tick).
    assert_eq!(
        groww_prod.matches(".consume_tick(").count(),
        1,
        "the Groww bridge must fold through exactly ONE .consume_tick( site \
         (inside the consume_feed_tick aggregate closure)"
    );
}
