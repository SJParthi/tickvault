//! SP5 wiring guard — Dhan live-feed health record-sites.
//!
//! SP5 wires the Dhan feed into the shared `FeedHealthRegistry` so
//! `GET /api/feeds/health` reports Dhan truthfully (it was `unknown` before).
//! Two record-sites + four call-site wirings. A full behavioural test needs a
//! live WebSocket + QuestDB, out of scope for a unit-test crate — so these are
//! source-scan ratchets that fail the build if any record-site or wiring is
//! silently removed (the same pattern as `health_counter_fix7_guard.rs`).

//! ## Dhan health dimensions wired (SP5 + SP5.1)
//!
//! - **connected** (watchdog `set_connected`) — SP5
//! - **freshness** (`record_tick` at the heartbeat) — SP5
//! - **drops** (`record_drops(Feed::Dhan)` at the `ws_frame_spill` terminal-loss
//!   drop arms) — SP5.1: a dropped Dhan live-feed frame now flips the feed light
//!   to `degraded`, closing the SP5 connected+fresh-but-dropping false-OK.
//!
//! Still pending (cosmetic, not a false-OK): `record_candle(Feed::Dhan)` at the
//! Dhan seal site (makes `candles_total` non-zero) — folded into the SP6 page work.

use std::path::PathBuf;

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md "2026-07-13
// Amendment" §B): `test_pool_watchdog_sets_dhan_connected` and
// `test_both_boot_paths_pass_feed_health` pinned the pool watchdog's
// `set_connected(Feed::Dhan, …)` write and the fast+slow boot-path
// feed-health plumbing — the pool watchdog, both boot arms, and the Dhan
// tick-processor spawn are DELETED with the lane. /api/feeds/health now
// honestly reports the Dhan MARKET-DATA feed as not-running (config-off,
// retired).
//
// RETIRED (stage-2 dead-WS sweep, 2026-07-17):
// `test_tick_processor_records_dhan_with_ist_offset` pinned the Dhan
// `record_tick` site inside `tick_processor.rs` — that file was DELETED
// with the dead Dhan tick chain (the PR-C2 "retained pending Phase C"
// caveat above is resolved by deletion), so there is no Dhan freshness
// record-site left to pin. The surviving SP5.1 pin below covers the
// ws_frame_spill drop-dimension record-site, which is live (the WAL is
// KEEP).

/// SP5.1: the drops dimension is now WIRED at the storage terminal-loss site.
/// A dropped Dhan live-feed frame records `record_drops(Feed::Dhan)` in the
/// `ws_frame_spill` drop arms → `drops>0 → Degraded` → the feed light flips 🟡,
/// closing the SP5 connected+fresh-but-dropping false-OK.
#[test]
fn test_sp5_1_drops_dimension_wired_in_spill() {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/app -> crates
    path.push("storage/src/ws_frame_spill.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(
        src.contains("fh.record_drops(tickvault_common::feed::Feed::Dhan")
            && src.contains("matches!(ws_type, WsType::LiveFeed)"),
        "SP5.1 regression: the WAL frame spill no longer records a Dhan drop on a \
         dropped LIVE-FEED frame — the connected+fresh-but-dropping false-OK has \
         re-opened (a dropping Dhan feed would read `ok`, not `degraded`)."
    );
}
