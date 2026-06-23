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

fn read_app_main() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("src/main.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn read_tick_processor() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/app -> crates
    path.push("core/src/pipeline/tick_processor.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// `spawn_pool_watchdog_task` must accept the feed-health registry AND call
/// `set_connected(Feed::Dhan, …)` so the Dhan connected slot is written.
#[test]
fn test_pool_watchdog_sets_dhan_connected() {
    let src = read_app_main();
    assert!(
        src.contains("fn spawn_pool_watchdog_task(")
            && src.contains("feed_health: Option<std::sync::Arc<tickvault_common::feed_health::FeedHealthRegistry>>"),
        "SP5 regression: spawn_pool_watchdog_task no longer takes the feed-health \
         registry — the Dhan connected state can't reach /api/feeds/health."
    );
    assert!(
        src.contains("fh.set_connected(") && src.contains("tickvault_common::feed::Feed::Dhan"),
        "SP5 regression: the watchdog accepts the registry but never calls \
         set_connected(Feed::Dhan, …) — pass-through with no write is not wiring."
    );
}

/// Both `run_tick_processor` calls AND both `spawn_pool_watchdog_task` calls
/// must pass `Some(Arc::clone(&feed_health))` so BOTH boot paths report Dhan.
#[test]
fn test_both_boot_paths_pass_feed_health() {
    let src = read_app_main();
    // Each boot path (fast + slow) clones feed_health twice: once into the
    // tick-processor coroutine (`let feed_health_for_processor = Arc::clone(...)`)
    // and once inline into the watchdog call → ≥4 `Arc::clone(&feed_health)`,
    // plus ≥2 `Some(feed_health_for_processor)` passes to run_tick_processor.
    let clones = src.matches("std::sync::Arc::clone(&feed_health)").count();
    assert!(
        clones >= 4,
        "SP5 regression: expected ≥4 `Arc::clone(&feed_health)` (fast+slow boot × \
         tick_processor clone + watchdog); found {clones}. A missing wiring means \
         one boot path reports Dhan as `unknown` forever."
    );
    let processor_passes = src.matches("Some(feed_health_for_processor)").count();
    assert!(
        processor_passes >= 2,
        "SP5 regression: expected ≥2 `Some(feed_health_for_processor)` passes to \
         run_tick_processor (fast+slow boot); found {processor_passes}."
    );
}

/// The tick processor must record the Dhan tick at the heartbeat block using the
/// WALL-CLOCK receipt time converted to IST nanos (`+ IST_UTC_OFFSET_NANOS`).
/// Recording raw UTC nanos would read ~5.5h in the future → permanent false-fresh.
#[test]
fn test_tick_processor_records_dhan_with_ist_offset() {
    let src = read_tick_processor();
    assert!(
        src.contains(
            "feed_health: Option<std::sync::Arc<tickvault_common::feed_health::FeedHealthRegistry>>"
        ),
        "SP5 regression: run_tick_processor no longer takes the feed-health registry."
    );
    assert!(
        src.contains("fh.record_tick(") && src.contains("tickvault_common::feed::Feed::Dhan"),
        "SP5 regression: run_tick_processor accepts the registry but never records \
         a Dhan tick — Dhan freshness can't reach /api/feeds/health."
    );
    assert!(
        src.contains("saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS)"),
        "SP5 clock regression: the Dhan record must convert UTC received_at to IST \
         nanos via saturating_add(IST_UTC_OFFSET_NANOS) so the endpoint's age math \
         (which uses IST now) is consistent. Raw UTC = permanent false-fresh."
    );
}

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
