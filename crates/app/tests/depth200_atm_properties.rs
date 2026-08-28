//! Properties of the state machine that keeps the 200-level sockets at the
//! money.
//!
//! # Why this surface
//!
//! This is the operator's 2026-08-26 rule in code: *"for evry one minute
//! alwyas espeiclaly for dpeth 200 … nifty atm ce atm pe always … even for
//! bancknfity atm ce atm pe also"*. It runs every minute for the whole
//! session, it holds state across minutes, and it decides whether a LIVE
//! subscription moves.
//!
//! That combination is the dangerous one. A stateless function that gets a
//! minute wrong is wrong for a minute; a state machine that gets it wrong can
//! swap the same two contracts back and forth for six hours while every
//! counter reports healthy activity — which is exactly the failure the
//! deadband and the confirmation count were added to prevent, and exactly the
//! failure a hand-written test is least likely to catch, because it needs a
//! SEQUENCE of minutes to appear at all.
//!
//! # The honest limit
//!
//! A passing run means no counterexample was found among the cases tried. The
//! input space is infinite; these widen the search rather than closing it.

use proptest::prelude::*;

use tickvault_app::depth200_atm::{
    ATM_SWITCH_CONFIRM_OBSERVATIONS, ChainMinute, DEPTH_200_ATM_UNDERLYINGS, Depth200AtmConfig,
    Depth200AtmTracker, NoSwitch, StrikePair, SwitchReason,
};

/// Ladder start, in paise: 24,000.00 to 24,100.00 rupees.
const LADDER_BASE_LO: i64 = 2_400_000;
const LADDER_BASE_HI: i64 = 2_410_000;

/// A ladder of `count` strikes, 50 rupees apart, from `base` paise.
///
/// Contract ids are a FUNCTION of the strike, so one id is one contract. Ids
/// drawn independently would build a ladder where the same contract sits at
/// two strikes — a shape no exchange produces, and one that fails almost any
/// property for a reason that says nothing about the code.
fn ladder(base_paise: i64, count: usize, id_epoch: i64) -> Vec<StrikePair> {
    (0..count)
        .map(|k| {
            let strike = base_paise + (k as i64) * 5_000;
            StrikePair {
                strike_paise: strike,
                ce_security_id: strike * 4 + id_epoch * 2 + 1,
                pe_security_id: strike * 4 + id_epoch * 2 + 2,
            }
        })
        .collect()
}

/// Spot in rupees, anywhere across a NIFTY-shaped ladder and a little beyond
/// each end, so the nearest strike is sometimes the first or the last.
fn spot() -> impl Strategy<Value = f64> {
    24_000.0_f64..25_600.0
}

fn config() -> impl Strategy<Value = Depth200AtmConfig> {
    (
        prop_oneof![
            Just(0.0_f64),
            Just(0.25),
            Just(0.5),
            Just(1.0),
            // Out of range on both sides: the doc says these are clamped, and
            // a claim of clamping that nothing exercises is a claim.
            Just(-3.0),
            Just(9.0),
        ],
        prop_oneof![Just(0_u32), Just(1), Just(2), Just(5)],
    )
        .prop_map(
            |(switch_margin_fraction, confirm_observations)| Depth200AtmConfig {
                switch_margin_fraction,
                confirm_observations,
            },
        )
}

proptest! {
    /// A STABLE CHAIN NEVER FLAPS.
    ///
    /// Feed the identical minute over and over: the tracker may adopt once,
    /// and then must never switch again. A machine that switches twice on
    /// unchanged input is one that re-subscribes a live socket forever.
    #[test]
    fn an_unchanging_minute_switches_at_most_once(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        s in spot(),
        cfg in config(),
    ) {
        let pairs = ladder(base, count, 0);
        let mut tracker = Depth200AtmTracker::new(cfg);
        let mut switches = 0_usize;
        for _ in 0..40 {
            let minute = ChainMinute { underlying: "NIFTY", spot: s, pairs: &pairs };
            if tracker.observe(&minute).is_ok() {
                switches += 1;
            }
        }
        prop_assert!(switches <= 1, "{switches} switches on an unchanging chain");
    }

    /// TWO STRIKES CANNOT TRADE THE SOCKET BACK AND FORTH.
    ///
    /// The index sits EXACTLY between two strikes and wobbles — the ordinary
    /// afternoon shape, not an exotic one. The nearest strike therefore
    /// changes every single minute, and nothing but the hysteresis stands
    /// between that and a re-subscribe every minute for the rest of the
    /// session, reporting healthy swap counts the whole way.
    ///
    /// **The first version of this property was vacuous and I am recording
    /// that rather than quietly fixing it.** It wobbled around a strike rather
    /// than around the midpoint between two, so the nearest strike never
    /// changed and no hysteresis was ever exercised. It passed under every
    /// configuration, including ones that switch on the first observation —
    /// which is precisely the shape it was written to catch. A test that
    /// cannot fail is not evidence.
    ///
    /// It runs on the SHIPPED configuration deliberately. Under an arbitrary
    /// config it is not a property at all: an operator who sets the
    /// confirmation count to one has asked for a switch on the first
    /// observation, and alternating input then alternates the socket, which is
    /// the setting behaving as written rather than a defect.
    #[test]
    fn a_spot_sitting_between_two_strikes_does_not_swap_the_socket_every_minute(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 4_usize..20,
        jitter in 0.5_f64..24.0,
    ) {
        let pairs = ladder(base, count, 0);
        // Exactly halfway between strike k and strike k+1 — 50 rupees apart,
        // so the midpoint is 25 rupees from each.
        let midpoint = (base as f64) / 100.0 + 50.0 * ((count / 2) as f64) + 25.0;
        let mut tracker = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let mut switches = 0_usize;
        for i in 0..40 {
            let s = if i % 2 == 0 { midpoint - jitter } else { midpoint + jitter };
            let minute = ChainMinute { underlying: "NIFTY", spot: s, pairs: &pairs };
            if tracker.observe(&minute).is_ok() {
                switches += 1;
            }
        }
        // One adoption, and at most a settle. Anything more is the socket
        // being traded between two strikes.
        prop_assert!(
            switches <= 2,
            "{switches} switches over 40 minutes on a {jitter}-rupee wobble"
        );
    }

    /// THE DEADBAND HOLDS A SUSTAINED SMALL MOVE.
    ///
    /// The alternation property above is passed by the confirmation count
    /// alone — a challenger that changes every minute never confirms, whatever
    /// the margin is. So the margin needs its own case, and it is a DIFFERENT
    /// shape: a steady move, in one direction, to a price where the next
    /// strike genuinely leads but only just.
    ///
    /// Strikes are 50 rupees apart and the shipped margin is a quarter of the
    /// spacing, so a neighbour that leads by 12.5 rupees or less must not take
    /// the socket however long it leads for. Without this, a book sitting a
    /// little off-centre all afternoon re-subscribes once and then reads as
    /// at-the-money when it is not.
    #[test]
    fn a_sustained_move_inside_the_deadband_never_takes_the_socket(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 4_usize..20,
        // Past halfway (so the neighbour leads) but inside the margin.
        delta in 26.0_f64..31.0,
    ) {
        let pairs = ladder(base, count, 0);
        let on_strike = (base as f64) / 100.0 + 50.0 * ((count / 2) as f64);
        let mut tracker = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let adopt = ChainMinute { underlying: "NIFTY", spot: on_strike, pairs: &pairs };
        let Ok(first) = tracker.observe(&adopt) else { return Ok(()); };
        for _ in 0..40 {
            let minute =
                ChainMinute { underlying: "NIFTY", spot: on_strike + delta, pairs: &pairs };
            let got = tracker.observe(&minute);
            prop_assert!(got.is_err(), "switched on a {delta}-rupee lead: {got:?}");
        }
        prop_assert_eq!(tracker.current(0), Some(first.to));
    }

    /// AND A REAL MOVE STILL GETS THROUGH.
    ///
    /// The deadband must hold noise without holding the market. A neighbour
    /// leading by well over the margin, steadily, has to take the socket —
    /// after the confirmation count, not before it. A tracker that never
    /// switches is as wrong as one that switches constantly, and it fails
    /// silently in the other direction: the socket reads a strike the index
    /// left hours ago.
    #[test]
    fn a_sustained_move_past_the_deadband_does_take_the_socket(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 4_usize..20,
        delta in 34.0_f64..49.0,
    ) {
        let pairs = ladder(base, count, 0);
        let on_strike = (base as f64) / 100.0 + 50.0 * ((count / 2) as f64);
        let mut tracker = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let adopt = ChainMinute { underlying: "NIFTY", spot: on_strike, pairs: &pairs };
        let Ok(first) = tracker.observe(&adopt) else { return Ok(()); };
        let mut switched = None;
        for _ in 0..10 {
            let minute =
                ChainMinute { underlying: "NIFTY", spot: on_strike + delta, pairs: &pairs };
            if let Ok(s) = tracker.observe(&minute) {
                switched = Some(s);
                break;
            }
        }
        let s = switched.ok_or_else(|| {
            TestCaseError::fail(format!("never switched on a sustained {delta}-rupee move"))
        })?;
        prop_assert_ne!(s.to.strike_paise, first.to.strike_paise);
    }

    /// A SWITCH NEVER INVENTS A CONTRACT.
    ///
    /// Subscribing an id the chain did not list is a well-formed request that
    /// returns silence forever and looks exactly like a quiet book.
    #[test]
    fn a_switch_only_ever_names_a_pair_from_the_chain(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        spots in prop::collection::vec(spot(), 1..25),
        cfg in config(),
    ) {
        let pairs = ladder(base, count, 0);
        let mut tracker = Depth200AtmTracker::new(cfg);
        for s in spots {
            let minute = ChainMinute { underlying: "NIFTY", spot: s, pairs: &pairs };
            if let Ok(switch) = tracker.observe(&minute) {
                prop_assert!(
                    pairs.contains(&switch.to),
                    "invented {:?}",
                    switch.to
                );
            }
        }
    }

    /// REPORTED STATE MATCHES ACTUAL STATE.
    ///
    /// After a switch, what the tracker says it holds must be what it just
    /// reported switching to. A machine whose report and whose memory disagree
    /// sends one subscribe and remembers another, and every later comparison
    /// is made against the wrong contract.
    #[test]
    fn after_a_switch_the_tracker_holds_exactly_what_it_reported(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        spots in prop::collection::vec(spot(), 1..25),
        cfg in config(),
    ) {
        let pairs = ladder(base, count, 0);
        let mut tracker = Depth200AtmTracker::new(cfg);
        for s in spots {
            let minute = ChainMinute { underlying: "NIFTY", spot: s, pairs: &pairs };
            if let Ok(switch) = tracker.observe(&minute) {
                prop_assert_eq!(tracker.current(switch.underlying_index), Some(switch.to));
                prop_assert_ne!(Some(switch.to), switch.from, "switched to what it held");
            }
        }
    }

    /// THE MAP IS BOUNDED BY THE OPERATOR'S LIST, NOT BY VENDOR INPUT.
    ///
    /// An unbounded per-entity map is the growth class this repository has
    /// recorded five separate times. Here the bound is the two named
    /// underlyings, and nothing a chain says may add a third.
    #[test]
    fn no_vendor_input_can_grow_the_tracked_set(
        names in prop::collection::vec("[A-Z]{3,10}", 1..30),
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        s in spot(),
    ) {
        let pairs = ladder(base, 8, 0);
        let mut tracker = Depth200AtmTracker::new(Depth200AtmConfig::default());
        for name in &names {
            let minute = ChainMinute { underlying: name, spot: s, pairs: &pairs };
            let _ = tracker.observe(&minute);
        }
        prop_assert!(tracker.tracked_len() <= DEPTH_200_ATM_UNDERLYINGS.len());
    }

    /// A VANISHED STRIKE IS ADOPTED IMMEDIATELY, past both guards.
    ///
    /// Holding a subscription to a contract the chain no longer lists is
    /// strictly worse than switching — it is a socket reading a book that does
    /// not exist, and it reports healthy. So this must bypass the deadband and
    /// the confirmation count, whatever they are set to.
    #[test]
    fn a_vanished_subscribed_strike_switches_on_the_very_next_minute(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 3_usize..20,
        cfg in config(),
    ) {
        let pairs = ladder(base, count, 0);
        let centre = (base as f64) / 100.0 + 50.0 * ((count / 2) as f64);
        let mut tracker = Depth200AtmTracker::new(cfg);
        let first = ChainMinute { underlying: "NIFTY", spot: centre, pairs: &pairs };
        let Ok(adopted) = tracker.observe(&first) else { return Ok(()); };
        // Same chain minus the strike we hold.
        let without: Vec<StrikePair> = pairs
            .iter()
            .copied()
            .filter(|p| p.strike_paise != adopted.to.strike_paise)
            .collect();
        if without.is_empty() {
            return Ok(());
        }
        let next = ChainMinute { underlying: "NIFTY", spot: centre, pairs: &without };
        let got = tracker.observe(&next);
        prop_assert!(
            matches!(
                got,
                Ok(s) if s.reason == SwitchReason::SubscribedStrikeVanished
            ),
            "held a vanished strike: {got:?}"
        );
    }

    /// A RE-ISSUED CONTRACT ID MOVES THE SUBSCRIPTION.
    ///
    /// Dhan documents derivative ids as unstable across days. The subscription
    /// key is the id, not the strike — so a chain whose strike is unchanged
    /// and whose ids are new must switch, or the socket sits on a dead id
    /// while every price-level check agrees.
    #[test]
    fn a_new_contract_id_at_the_same_strike_is_never_missed(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        cfg in config(),
    ) {
        let pairs = ladder(base, count, 0);
        let centre = (base as f64) / 100.0 + 50.0 * ((count / 2) as f64);
        let mut tracker = Depth200AtmTracker::new(cfg);
        let first = ChainMinute { underlying: "NIFTY", spot: centre, pairs: &pairs };
        if tracker.observe(&first).is_err() {
            return Ok(());
        }
        // Same strikes, tomorrow's ids.
        let reissued = ladder(base, count, 1);
        let next = ChainMinute { underlying: "NIFTY", spot: centre, pairs: &reissued };
        let got = tracker.observe(&next);
        prop_assert!(
            matches!(got, Ok(s) if s.reason == SwitchReason::ContractIdChanged),
            "stayed on a stale id: {got:?}"
        );
    }

    /// DETERMINISTIC. The same sequence of minutes gives the same sequence of
    /// decisions.
    ///
    /// Anything order- or hash-dependent leaking into this machine would swap
    /// sockets on identical data, and the swap counters would look like
    /// ordinary activity.
    #[test]
    fn the_same_sequence_of_minutes_decides_the_same_way_twice(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        spots in prop::collection::vec(spot(), 1..25),
        cfg in config(),
    ) {
        let pairs = ladder(base, count, 0);
        let run = || {
            let mut tracker = Depth200AtmTracker::new(cfg);
            spots
                .iter()
                .map(|s| {
                    let minute = ChainMinute { underlying: "NIFTY", spot: *s, pairs: &pairs };
                    format!("{:?}", tracker.observe(&minute))
                })
                .collect::<Vec<_>>()
        };
        prop_assert_eq!(run(), run());
    }

    /// UNUSABLE INPUT IS REFUSED, NAMED, AND CHANGES NOTHING.
    ///
    /// A spot that is not a price cannot centre a window. The tracker must say
    /// which of the two it is — absent price, or absent pairs — and must not
    /// disturb a live subscription while it says so.
    #[test]
    fn a_minute_with_no_usable_price_refuses_without_disturbing_the_subscription(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        bad in prop_oneof![Just(f64::NAN), Just(0.0_f64), Just(-1.0), Just(f64::INFINITY)],
        cfg in config(),
    ) {
        let pairs = ladder(base, count, 0);
        let centre = (base as f64) / 100.0 + 50.0 * ((count / 2) as f64);
        let mut tracker = Depth200AtmTracker::new(cfg);
        let good = ChainMinute { underlying: "NIFTY", spot: centre, pairs: &pairs };
        let _ = tracker.observe(&good);
        let held = tracker.current(0);
        let minute = ChainMinute { underlying: "NIFTY", spot: bad, pairs: &pairs };
        let got = tracker.observe(&minute);
        // Infinity is a finite-check failure the same as NaN.
        prop_assert_eq!(got, Err(NoSwitch::UnusableSpot));
        prop_assert_eq!(tracker.current(0), held, "an unusable price moved the socket");
    }

    /// AN EMPTY CHAIN NEVER MOVES A LIVE SUBSCRIPTION.
    ///
    /// A minute that carried no strike at all is a vendor gap, not a signal to
    /// abandon what we hold.
    #[test]
    fn an_empty_chain_minute_holds_what_is_subscribed(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        cfg in config(),
    ) {
        let pairs = ladder(base, count, 0);
        let centre = (base as f64) / 100.0 + 50.0 * ((count / 2) as f64);
        let mut tracker = Depth200AtmTracker::new(cfg);
        let good = ChainMinute { underlying: "NIFTY", spot: centre, pairs: &pairs };
        let _ = tracker.observe(&good);
        let held = tracker.current(0);
        let empty: [StrikePair; 0] = [];
        let minute = ChainMinute { underlying: "NIFTY", spot: centre, pairs: &empty };
        prop_assert_eq!(tracker.observe(&minute), Err(NoSwitch::NoUsablePairs));
        prop_assert_eq!(tracker.current(0), held);
    }

    /// THE CONFIRMATION COUNT IS REAL. One anomalous print cannot move a live
    /// subscription when confirmation is required.
    ///
    /// A single bad spot — a vendor glitch, a stale copy — arriving between
    /// two good minutes must leave the socket where it was.
    #[test]
    fn one_anomalous_minute_cannot_move_the_subscription(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 6_usize..20,
    ) {
        let pairs = ladder(base, count, 0);
        let cfg = Depth200AtmConfig {
            switch_margin_fraction: 0.25,
            confirm_observations: ATM_SWITCH_CONFIRM_OBSERVATIONS.max(2),
        };
        let centre = (base as f64) / 100.0 + 50.0 * ((count / 2) as f64);
        let mut tracker = Depth200AtmTracker::new(cfg);
        let steady = ChainMinute { underlying: "NIFTY", spot: centre, pairs: &pairs };
        let _ = tracker.observe(&steady);
        let held = tracker.current(0);
        // One wild print, far away, then straight back.
        let wild = ChainMinute { underlying: "NIFTY", spot: centre + 400.0, pairs: &pairs };
        let _ = tracker.observe(&wild);
        let _ = tracker.observe(&steady);
        prop_assert_eq!(tracker.current(0), held, "a single anomaly moved the socket");
    }

    /// Never panics. Saturating paise arithmetic, a median over a generated
    /// ladder, and float comparisons against config values that are
    /// deliberately out of range.
    #[test]
    fn it_never_panics(
        base in i64::MIN / 4..i64::MAX / 4,
        count in 0_usize..30,
        s in prop_oneof![Just(f64::NAN), Just(0.0_f64), Just(f64::MAX), spot()],
        cfg in config(),
        name in "[A-Za-z]{1,12}",
    ) {
        let pairs = ladder(base, count, 0);
        let mut tracker = Depth200AtmTracker::new(cfg);
        let minute = ChainMinute { underlying: &name, spot: s, pairs: &pairs };
        let _ = tracker.observe(&minute);
    }
}
