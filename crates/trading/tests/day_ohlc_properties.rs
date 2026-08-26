//! Random-input attack on the day OHLC tracker.
//!
//! This is the surface behind the operator's most-repeated requirement:
//! **the 09:12 pre-open equilibrium price is the 09:15 open**, stated three
//! separate times (2026-08-25 twice, 2026-08-26 again) and recorded in
//! `websocket-connection-scope-lock.md`. The mechanism that delivers it is
//! `update_tick_with_exchange_open`, which prefers the exchange's own
//! `day_open` field over the first tick we happened to observe.
//!
//! Two things make it worth attacking with random input rather than examples:
//!
//! - **Order matters and is not ours to control.** Indices publish no
//!   exchange open before 09:15, so the tracker arms from a pre-open print
//!   and must LATER be corrected when the real open arrives. On 2026-08-26
//!   that meant NIFTY's recorded open would have been 24035.25 instead of
//!   24341.95 — wrong by ~307 points on the headline index — if adoption had
//!   been first-write-wins.
//! - **Adoption WIDENS the range irreversibly** until the IST-midnight reset,
//!   so a single implausible value permanently distorts the day's high or low
//!   for that instrument.
//!
//! The generator therefore mixes plausible prices with the exact corrupt
//! shapes the parsers are proven to emit — NaN, infinities, zero, negatives,
//! `f32::MAX` widened, and subnormals — and interleaves them freely.

use proptest::prelude::*;

use tickvault_trading::in_mem::day_ohlc_tracker::{
    DAY_OPEN_MAX_PLAUSIBLE_PRICE, DAY_OPEN_MIN_PLAUSIBLE_PRICE, DayOhlc, is_plausible_price,
};

/// One packet: an LTP and the exchange's day-open field beside it.
///
/// `0.0` in the open slot is not corruption — it is the DOCUMENTED absent
/// sentinel for a Ticker-mode packet, and every index carries it before
/// 09:15. Refusing it must leave the previous open standing, not zero it.
type Packet = (f64, f64);

/// Prices that are real, plus every corrupt shape seen in a live payload.
fn price() -> impl Strategy<Value = f64> {
    prop_oneof![
        60 => (0.05f64..90_000.0),
        4 => Just(0.0),
        4 => Just(f64::NAN),
        3 => Just(f64::INFINITY),
        3 => Just(f64::NEG_INFINITY),
        3 => (-90_000.0f64..0.0),
        3 => Just(f64::from(f32::MAX)),
        3 => Just(f64::MIN_POSITIVE),
        3 => Just(DAY_OPEN_MAX_PLAUSIBLE_PRICE * 2.0),
        2 => Just(DAY_OPEN_MIN_PLAUSIBLE_PRICE),
        2 => Just(DAY_OPEN_MAX_PLAUSIBLE_PRICE),
    ]
}

fn packets() -> impl Strategy<Value = Vec<Packet>> {
    prop::collection::vec((price(), price()), 0..30)
}

/// Replays a packet list and returns the tracker.
fn run(packets: &[Packet]) -> DayOhlc {
    let mut d = DayOhlc::disarmed();
    for (ltp, open) in packets {
        d.update_tick_with_exchange_open(*ltp, *open);
    }
    d
}

/// The index pre-open shape: real prints, and an open slot that is never
/// adoptable.
///
/// `0.0` dominates because that is what an index actually carries before
/// 09:15 — the documented absent sentinel for a packet with no day-open
/// field. The corrupt shapes are mixed in so the fallback is exercised
/// against a bad open as well as an absent one.
fn preopen_packets() -> impl Strategy<Value = Vec<Packet>> {
    let bad_open = prop_oneof![
        6 => Just(0.0),
        1 => Just(f64::NAN),
        1 => Just(f64::INFINITY),
        1 => Just(-1.0),
        1 => Just(f64::from(f32::MAX)),
    ];
    prop::collection::vec((price(), bad_open), 0..30)
}

/// Whether this packet's open would be adopted: the LTP must be accepted (so
/// the packet is trusted at all) and the open must be a plausible price.
fn adopts(ltp: f64, open: f64) -> bool {
    is_plausible_price(ltp) && is_plausible_price(open)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// The range invariant, which every consumer of this struct assumes.
    #[test]
    fn low_is_never_above_open_and_open_is_never_above_high(p in packets()) {
        let d = run(&p);
        if !d.is_armed() {
            return Ok(());
        }
        prop_assert!(
            d.day_low <= d.day_open,
            "low {} > open {}", d.day_low, d.day_open
        );
        prop_assert!(
            d.day_open <= d.day_high,
            "open {} > high {}", d.day_open, d.day_high
        );
        prop_assert!(
            d.day_low <= d.day_close && d.day_close <= d.day_high,
            "close {} outside [{}, {}]", d.day_close, d.day_low, d.day_high
        );
    }

    /// Nothing corrupt survives into a field. An armed tracker whose fields
    /// are NaN would poison every downstream comparison silently — the
    /// absorbing-NaN class this repository has already been bitten by in the
    /// indicator engine.
    #[test]
    fn an_armed_tracker_never_holds_a_non_finite_field(p in packets()) {
        let d = run(&p);
        if !d.is_armed() {
            return Ok(());
        }
        for (name, v) in [
            ("open", d.day_open),
            ("high", d.day_high),
            ("low", d.day_low),
            ("close", d.day_close),
        ] {
            prop_assert!(v.is_finite(), "{} is {}", name, v);
            prop_assert!(v > 0.0, "{} is {}", name, v);
        }
    }

    /// THE OPERATOR'S RULE. A late exchange open corrects the pre-open print
    /// we fell back to — every time, at any position in the stream, however
    /// many prints came first.
    ///
    /// This is the shape indices hit every single morning: no exchange open
    /// exists before 09:15, so the tracker arms from a pre-open tick and the
    /// real open arrives later. First-write-wins here would have recorded
    /// NIFTY's open ~307 points low on 2026-08-26 while every test then in
    /// the file still passed.
    #[test]
    fn the_open_always_ends_on_the_last_exchange_open_that_was_offered(p in packets()) {
        let d = run(&p);
        let last_adopted = p.iter().rev().find(|(l, o)| adopts(*l, *o)).map(|(_, o)| *o);
        if let Some(expected) = last_adopted {
            prop_assert_eq!(
                d.day_open, expected,
                "open {} but the last adoptable exchange open was {}",
                d.day_open, expected
            );
        }
    }

    /// When NO packet ever carried an adoptable open — the whole pre-open
    /// window for an index — the open is the first accepted LTP. That is the
    /// fallback, and removing it would leave a Ticker-mode instrument with no
    /// open at all.
    ///
    /// The pre-open shape is GENERATED rather than filtered for. A
    /// `prop_assume!` over the general generator rejected 1,024 inputs to
    /// find 74 usable ones and then aborted, because most random packet lists
    /// contain at least one adoptable pair. Filtering to reach a case the
    /// generator rarely produces is how a property ends up testing almost
    /// nothing while still reporting green.
    #[test]
    fn with_no_exchange_open_the_first_accepted_tick_is_the_open(p in preopen_packets()) {
        let d = run(&p);
        let first_accepted = p.iter().find(|(l, _)| is_plausible_price(*l)).map(|(l, _)| *l);
        match first_accepted {
            Some(expected) => {
                prop_assert!(d.is_armed());
                prop_assert_eq!(d.day_open, expected);
            }
            None => prop_assert!(!d.is_armed(), "armed with no accepted price"),
        }
    }

    /// A packet whose LTP is corrupt contributes NOTHING — not its price and
    /// not its open. Adopting one field of a packet whose other field is
    /// proven garbage is the trust the ingest gate exists to withhold, and
    /// the parsers are proven to emit NaN LTP on a real packet shape.
    #[test]
    fn a_packet_with_a_corrupt_price_contributes_neither_field(
        p in packets(),
        bad_open in price(),
    ) {
        let before = run(&p);
        let mut after = run(&p);
        // Feed a packet with a REFUSED price but a perfectly good open.
        after.update_tick_with_exchange_open(f64::NAN, if is_plausible_price(bad_open) {
            bad_open
        } else {
            123.45
        });
        prop_assert_eq!(after.is_armed(), before.is_armed());
        if before.is_armed() {
            prop_assert_eq!(after.day_open, before.day_open, "a refused packet moved the open");
            prop_assert_eq!(after.day_high, before.day_high);
            prop_assert_eq!(after.day_low, before.day_low);
        }
    }

    /// The range only ever grows within a day. A high that could fall, or a
    /// low that could rise, would mean an earlier real print had been
    /// forgotten.
    #[test]
    fn the_range_never_narrows_within_a_day(p in packets()) {
        let mut d = DayOhlc::disarmed();
        let mut high = f64::NEG_INFINITY;
        let mut low = f64::INFINITY;
        for (ltp, open) in &p {
            d.update_tick_with_exchange_open(*ltp, *open);
            if !d.is_armed() {
                continue;
            }
            prop_assert!(d.day_high >= high, "high fell {} -> {}", high, d.day_high);
            prop_assert!(d.day_low <= low, "low rose {} -> {}", low, d.day_low);
            high = d.day_high;
            low = d.day_low;
        }
    }

    /// Every accepted price is inside the recorded range. A high that missed
    /// a print it saw is a lost extreme, which is what the per-minute
    /// high/low capture the operator asked for is built on.
    #[test]
    fn every_accepted_price_is_inside_the_recorded_range(p in packets()) {
        let d = run(&p);
        if !d.is_armed() {
            return Ok(());
        }
        for (ltp, _) in &p {
            if is_plausible_price(*ltp) {
                prop_assert!(*ltp <= d.day_high, "{} above high {}", ltp, d.day_high);
                prop_assert!(*ltp >= d.day_low, "{} below low {}", ltp, d.day_low);
            }
        }
    }

    /// Replaying the same packets twice gives the same answer. A tracker that
    /// depended on process state would make the day's recorded open a
    /// function of when the process started.
    #[test]
    fn replaying_the_same_packets_gives_the_same_ohlc(p in packets()) {
        let a = run(&p);
        let b = run(&p);
        prop_assert_eq!(a.is_armed(), b.is_armed());
        if a.is_armed() {
            prop_assert_eq!(a.day_open, b.day_open);
            prop_assert_eq!(a.day_high, b.day_high);
            prop_assert_eq!(a.day_low, b.day_low);
            prop_assert_eq!(a.day_close, b.day_close);
        }
    }

    /// The close is the most recent accepted price, never an exchange open.
    #[test]
    fn the_close_is_the_last_accepted_price(p in packets()) {
        let d = run(&p);
        let last_accepted = p.iter().rev().find(|(l, _)| is_plausible_price(*l)).map(|(l, _)| *l);
        match last_accepted {
            Some(expected) => {
                prop_assert!(d.is_armed());
                prop_assert_eq!(d.day_close, expected);
            }
            None => prop_assert!(!d.is_armed()),
        }
    }

    /// A daily reset really clears everything, so tomorrow cannot inherit
    /// today's open — the failure that would make every morning's headline
    /// number yesterday's.
    #[test]
    fn a_daily_reset_leaves_nothing_behind(p in packets()) {
        let mut d = run(&p);
        d.reset_daily();
        prop_assert!(!d.is_armed());
        let mut fresh = DayOhlc::disarmed();
        fresh.update_tick_with_exchange_open(101.5, 100.0);
        d.update_tick_with_exchange_open(101.5, 100.0);
        prop_assert_eq!(d.day_open, fresh.day_open);
        prop_assert_eq!(d.day_high, fresh.day_high);
        prop_assert_eq!(d.day_low, fresh.day_low);
        prop_assert_eq!(d.day_close, fresh.day_close);
    }

    /// It never panics, on any interleaving of any of these shapes.
    #[test]
    fn it_never_panics(p in packets()) {
        let mut d = run(&p);
        d.reset_daily();
        let _ = run(&p);
    }
}
