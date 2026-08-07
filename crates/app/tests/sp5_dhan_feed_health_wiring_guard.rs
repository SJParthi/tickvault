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

/// SP5.1: the drops dimension is WIRED at the storage terminal-loss site.
/// A dropped live-feed frame records `record_drops` in the `ws_frame_spill`
/// drop arms → `drops>0 → Degraded` → the feed light flips 🟡, closing the
/// SP5 connected+fresh-but-dropping false-OK.
///
/// 2026-07-25: the attribution moved from a hardcoded `Feed::Dhan` to
/// `WsType::owning_feed()`, so that a TrueData frame drop is reported
/// against TrueData rather than Dhan (the WAL record carries no feed field —
/// the transport tag is the only feed evidence that reaches disk). The
/// SP5.1 invariant is UNCHANGED and is now asserted BEHAVIOURALLY below
/// rather than by matching source literals: `owning_feed` is a pure
/// function, so the guard can call it instead of grepping for the shape of
/// its call site.
#[test]
fn test_sp5_1_drops_dimension_wired_in_spill() {
    use tickvault_common::feed::Feed;
    use tickvault_storage::ws_frame_spill::WsType;

    // The SP5.1 invariant itself: a dropped LIVE-FEED frame must attribute
    // to Dhan. Asserted directly, not inferred from source text.
    assert_eq!(
        WsType::LiveFeed.owning_feed(),
        Some(Feed::Dhan),
        "SP5.1 regression: a dropped LIVE-FEED frame no longer attributes to \
         Dhan — the connected+fresh-but-dropping false-OK has re-opened (a \
         dropping Dhan feed would read `ok`, not `degraded`)."
    );
    // ...and must not attribute to the WRONG broker, which is the failure
    // this generalisation exists to prevent.
    assert_eq!(
        WsType::TruedataFeed.owning_feed(),
        Some(Feed::Truedata),
        "a TrueData frame drop must be reported against TrueData — naming the \
         wrong broker sends the operator to investigate a healthy feed while \
         the dropping one goes untouched."
    );
    assert_eq!(
        WsType::OrderUpdate.owning_feed(),
        None,
        "an order-update drop must never flip a MARKET-DATA feed to degraded."
    );

    // The remaining source-scan half: the drop arms must still CALL the
    // recorder. This cannot be asserted behaviourally without a live spill
    // + registry, so the call-site wiring stays a scan.
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/app -> crates
    path.push("storage/src/ws_frame_spill.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(
        src.contains("fh.record_drops(feed, 1)") && src.contains("ws_type.owning_feed()"),
        "SP5.1 regression: the WAL frame spill no longer records a feed drop on \
         a dropped frame — the connected+fresh-but-dropping false-OK has \
         re-opened."
    );
    assert_eq!(
        src.matches("self.record_feed_drop_for_health(ws_type);")
            .count(),
        2,
        "both terminal-loss drop arms (channel Full, writer Disconnected) must \
         record the drop — losing one arm silently re-opens the false-OK for \
         that cause only, which is harder to notice than losing both."
    );
}
