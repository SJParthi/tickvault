//! 29-tf engine Phase 3 commit 3 — cascade wiring source-scan guard.
//!
//! The 1s candle cascade consumer is wired in `crates/app/src/main.rs`
//! slow-boot path. It MUST stay wired:
//!
//! 1. A `CandleEngineMap<Tf1s>` Arc is constructed.
//! 2. A `tick_broadcast_sender.subscribe()` is consumed by the cascade.
//! 3. `tickvault_trading::candles::run_cascade_1s` is spawned.
//!
//! Per plan L12 the cascade is the only path that advances the in-RAM
//! candle state — silently dropping it would make the trading bot read
//! stale candles. This source-scan ratchet fails the build if any of the
//! three pieces disappears.

use std::path::PathBuf;

const APP_MAIN_RS: &str = "src/main.rs";

fn read_main() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push(APP_MAIN_RS);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read {}: {err}", path.display()))
}

#[test]
fn main_rs_constructs_candle_engine_map_1s() {
    let src = read_main();
    assert!(
        src.contains("tickvault_trading::candles::CandleEngineMap")
            && src.contains("tickvault_trading::candles::Tf1s"),
        "main.rs must construct a CandleEngineMap<Tf1s> for the 29-tf engine"
    );
}

#[test]
fn main_rs_spawns_supervised_cascade_with_broadcast_sender() {
    let src = read_main();
    assert!(
        src.contains("tickvault_trading::candles::spawn_supervised_cascade_1s"),
        "main.rs must spawn the SUPERVISED cascade (not the bare \
         run_cascade_1s) so a panic/exit triggers respawn instead of \
         silently freezing the in-RAM candle state (hostile review H3)"
    );
    assert!(
        src.contains("tick_broadcast_sender.clone()"),
        "the supervisor MUST receive a Sender clone so it can call \
         .subscribe() on every respawn — receiving a single Receiver \
         would lose all subscription on first restart"
    );
}

#[test]
fn main_rs_spawns_midnight_rollover_task() {
    let src = read_main();
    assert!(
        src.contains("tickvault_trading::candles::run_midnight_rollover_task"),
        "main.rs MUST spawn the IST midnight rollover task so plan L13 \
         (engine state resets at IST 00:00) is upheld — without it, \
         day-N open bars silently fuse into day-(N+1)'s first bar \
         (hostile review H1)"
    );
}

#[test]
fn main_rs_constructs_cascade_fanout_for_28_derived_tfs() {
    let src = read_main();
    assert!(
        src.contains("tickvault_trading::candles::CascadeFanout"),
        "main.rs MUST construct the 28-TF CascadeFanout (Phase 3 commit 4) \
         so derived engine state (3s, 5s, ..., 1mo) advances on every \
         sealed 1s bar (per plan §2 cascade design)"
    );
    assert!(
        src.contains("cascade_fanout"),
        "main.rs MUST keep the `cascade_fanout` binding so future \
         consumers (RAM /api/movers reader, IST midnight rollover) can \
         clone the same fanout"
    );
}

#[test]
fn main_rs_passes_fanout_to_supervised_cascade() {
    let src = read_main();
    assert!(
        src.contains("Some(supervisor_fanout)"),
        "main.rs MUST pass `Some(fanout)` (not `None`) to \
         `spawn_supervised_cascade_1s` so sealed 1s bars cascade \
         into all 28 derived engines"
    );
}

#[test]
fn main_rs_uses_fanout_aware_midnight_rollover() {
    let src = read_main();
    assert!(
        src.contains("run_midnight_rollover_task_with_fanout"),
        "main.rs MUST use the fanout-aware midnight rollover \
         (`run_midnight_rollover_task_with_fanout`) so all 28 derived \
         engines are sealed at IST 00:00, not just the 1s engine"
    );
}

#[test]
fn main_rs_keeps_candle_engine_map_arc_for_future_consumers() {
    let src = read_main();
    // The Arc<CandleEngineMap<Tf1s>> binding is reused by future
    // consumers (IST midnight rollover, /api/movers v2 reader, etc).
    // Must remain a binding, not be inlined into a single .spawn() call.
    assert!(
        src.contains("candle_engine_map_1s"),
        "main.rs must keep the `candle_engine_map_1s` binding so future \
         consumers (rollover task, RAM movers reader) can clone the same Arc"
    );
}
