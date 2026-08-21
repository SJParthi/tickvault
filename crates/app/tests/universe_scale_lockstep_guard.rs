//! Pins that every per-instrument capacity in the workspace names the SAME
//! authorized universe size.
//!
//! # The defect this prevents
//!
//! Six independent constants in six different files each hardcode `25_000` —
//! the authorized universe (5 main-feed connections × 5,000 instruments, per
//! the 2026-08-15 full-universe scope grant):
//!
//! | Constant | Crate | What it bounds |
//! |---|---|---|
//! | `MAX_DAILY_UNIVERSE_SIZE` | common | the subscription set |
//! | `MAX_INDICATOR_INSTRUMENTS` | common | indicator slots |
//! | `AGGREGATOR_MAX_SLOTS` | trading | tick→timeframe fold slots |
//! | `DayOhlcTracker::MAX_TRACKED_INSTRUMENTS` | trading | day OHLC map |
//! | `TickGapTracker::MAX_TRACKED_INSTRUMENTS` | trading | gap-detector slots |
//! | `FOLD_MAX_SLOTS` | app | REST fold slots |
//!
//! Nothing made them agree. They agree TODAY by coincidence of authorship.
//!
//! That matters because the failure is silent and asymmetric. Raise the
//! authorized universe to 30,000 and edit four of the six, and the system does
//! not refuse: it subscribes 30,000 instruments, folds 25,000 of them, tracks
//! 25,000 in the gap detector, and computes indicators for 25,000. The other
//! 5,000 tick, are captured, and are simply absent from every derived
//! structure — each cap refusing fail-closed and loudly *in its own file*,
//! while the operator sees a healthy 30,000-instrument subscription.
//!
//! Every one of those caps is individually well-built. The gap is that no
//! single place asserts they describe the same number, so a partial edit
//! produces a partial system rather than a build failure.
//!
//! # Why a test and not a shared constant
//!
//! A shared constant would be better and is the obvious suggestion. It is not
//! taken here because these live in three crates with a one-way dependency
//! (`common` ← `trading` ← `app`), and several carry per-site documentation
//! about what their own exhaustion does — `DayOhlcTracker` refuses a NEW
//! instrument, `TickGapTracker` refuses but never evicts, `FOLD_MAX_SLOTS`
//! fails closed with a coded error. Collapsing them into one name would erase
//! the distinction between "the same number" and "the same meaning", which is
//! exactly the confusion that lets a future edit change one and not the rest.
//!
//! So they stay separate and this test makes them agree.

use tickvault_common::constants::{MAX_DAILY_UNIVERSE_SIZE, MAX_INDICATOR_INSTRUMENTS};
use tickvault_trading::candles::AGGREGATOR_MAX_SLOTS;
use tickvault_trading::in_mem::day_ohlc_tracker::DayOhlcTracker;
use tickvault_trading::risk::tick_gap_tracker::TickGapTracker;

/// The authorized universe: 5 main-feed connections × 5,000 instruments.
///
/// Not a round number chosen for comfort — it is the main-feed subscription
/// capacity, and the 2026-08-15 scope grant names that derivation explicitly.
const AUTHORIZED_UNIVERSE: usize = 25_000;

#[test]
fn every_per_instrument_capacity_names_the_same_authorized_universe() {
    let sites: [(&str, usize); 5] = [
        ("MAX_DAILY_UNIVERSE_SIZE (common)", MAX_DAILY_UNIVERSE_SIZE),
        (
            "MAX_INDICATOR_INSTRUMENTS (common)",
            MAX_INDICATOR_INSTRUMENTS,
        ),
        ("AGGREGATOR_MAX_SLOTS (trading)", AGGREGATOR_MAX_SLOTS),
        (
            "DayOhlcTracker::MAX_TRACKED_INSTRUMENTS (trading)",
            DayOhlcTracker::MAX_TRACKED_INSTRUMENTS,
        ),
        (
            "TickGapTracker::MAX_TRACKED_INSTRUMENTS (trading)",
            TickGapTracker::MAX_TRACKED_INSTRUMENTS,
        ),
    ];

    let disagreeing: Vec<String> = sites
        .iter()
        .filter(|(_, v)| *v != AUTHORIZED_UNIVERSE)
        .map(|(name, v)| format!("{name} = {v}"))
        .collect();

    assert!(
        disagreeing.is_empty(),
        "these per-instrument capacities disagree with the authorized universe \
         ({AUTHORIZED_UNIVERSE}):\n  {}\n\nRaising the universe means raising ALL of them. \
         A partial edit does not fail — it subscribes the new number and then folds, \
         tracks and computes indicators for the OLD one, with each cap refusing \
         correctly in its own file while the operator sees a healthy subscription.",
        disagreeing.join("\n  ")
    );
}

/// The app-crate site, checked by source scan because `rest_candle_fold` is a
/// binary-crate module rather than a library export.
///
/// A scan rather than an import is a real weakness — it matches text, not a
/// value — so it asserts the exact declaration line and would fail on a
/// rename, which is the failure mode that matters here.
#[test]
fn the_rest_fold_slot_cap_also_names_the_authorized_universe() {
    let src = include_str!("../src/rest_candle_fold.rs");
    let expected = format!("pub const FOLD_MAX_SLOTS: usize = {AUTHORIZED_UNIVERSE};");
    let expected_underscored = "pub const FOLD_MAX_SLOTS: usize = 25_000;";
    assert!(
        src.contains(&expected) || src.contains(expected_underscored),
        "FOLD_MAX_SLOTS no longer declares the authorized universe \
         ({AUTHORIZED_UNIVERSE}). If the universe changed, change it here too; \
         if only this changed, the fold silently covers fewer instruments than \
         the lane subscribes."
    );
}

/// Non-vacuity.
///
/// Both tests above pass trivially if `AUTHORIZED_UNIVERSE` were ever set to
/// whatever the constants happen to say. This pins the DERIVATION instead: the
/// number is 5 connections × 5,000 instruments, and if either of those moves,
/// this fails and forces the universe figure to be re-derived rather than
/// re-typed.
#[test]
fn the_authorized_universe_is_the_main_feed_capacity_not_a_round_number() {
    let per_conn = usize::try_from(
        tickvault_core::websocket::pool_budget::MAIN_FEED_INSTRUMENTS_PER_CONNECTION,
    )
    .expect("per-connection cap fits usize");
    let conns = usize::from(tickvault_core::websocket::pool_budget::MAX_MAIN_FEED_CONNECTIONS);

    assert_eq!(
        conns * per_conn,
        AUTHORIZED_UNIVERSE,
        "the authorized universe must remain {conns} connections x {per_conn} \
         instruments. It is not a round number chosen for comfort — it is the \
         main-feed subscription capacity, and a universe larger than what the \
         sockets can carry is refused by plan_pool as a WHOLE pool, taking the \
         lane down rather than truncating."
    );
}
