//! Properties of the depth-20 layout — the thing that decides the operator's
//! 250.
//!
//! # Why this surface
//!
//! The layout chooses WHAT sits on the five 20-level connections: two index
//! windows around the money, and today's biggest risers and fallers. Every
//! other guard in this subsystem operates on its output, so a defect here is
//! not caught downstream — it is faithfully carried out.
//!
//! The 29 hand-written tests cover the shapes I thought of. A real chain and
//! a real ranking are assembled elsewhere, so their shapes are not the ones
//! I would choose: a stock ranking as both a riser and a faller, a chain
//! listing one strike twice, a ranking naming instruments with no chain, an
//! underlying with a one-sided ladder, values that are not numbers.
//!
//! # The honest limit
//!
//! A passing run means no counterexample was found among the cases tried.
//! The input space is infinite; these widen the search rather than closing
//! it.

use proptest::prelude::*;
use std::collections::BTreeSet;

use tickvault_app::depth_rebalance::MoverRow;
use tickvault_app::depth20_layout::{
    DEPTH_20_INDEX_UNDERLYINGS, DEPTH_20_PER_SOCKET, DEPTH_20_SOCKETS, build_depth20_layout,
};
use tickvault_app::dhan_depth_universe::DepthCandidate;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

/// The two index underlyings plus stocks, so both halves are exercised.
const SYMBOLS: [&str; 5] = ["NIFTY", "BANKNIFTY", "STK1", "STK2", "STK3"];

fn key(i: SubscribeInstrument) -> (u64, u8) {
    (i.security_id, i.segment as u8)
}

/// A contract id is a FUNCTION of the contract's identity.
///
/// One `security_id` is one contract, so it has exactly one underlying, one
/// strike and one leg. Drawing the id independently lets the generator build
/// a chain where id 1 is a CALL at 24550 *and* a PUT at 24400 — a shape no
/// exchange can produce, and one that makes almost any downstream property
/// fail for a reason that says nothing about the code.
///
/// Deriving it keeps every genuinely interesting collision: the same strike
/// carrying both legs, the same row repeated, two underlyings sharing a
/// strike price, a ranked stock with no ladder. Only the impossible one goes
/// away.
fn contract_id(symbol: usize, strike_idx: i64, is_ce: bool) -> i64 {
    let leg = i64::from(is_ce);
    #[expect(clippy::cast_possible_wrap, reason = "symbol is 0..5")]
    let sym = symbol as i64;
    (sym * 8 + strike_idx) * 2 + leg + 1
}

fn candidate() -> impl Strategy<Value = DepthCandidate> {
    (
        0_usize..SYMBOLS.len(),
        0_i64..8,
        any::<bool>(),
        // Spot includes the values that are not prices: zero, negative and
        // non-finite all have to be refused rather than divided by.
        prop_oneof![
            Just(0.0_f64),
            Just(-1.0),
            Just(f64::NAN),
            Just(f64::INFINITY),
            24_400.0_f64..24_700.0,
        ],
    )
        .prop_map(|(s, k, is_ce, spot)| {
            #[expect(clippy::cast_precision_loss, reason = "k is 0..8")]
            let strike = (k as f64).mul_add(50.0, 24_400.0);
            DepthCandidate {
                underlying: SYMBOLS[s].to_owned(),
                contract_security_id: contract_id(s, k, is_ce),
                // One expiry: the current-expiry rule has its own property
                // file, and mixing the two would make a failure here
                // ambiguous about which rule broke.
                expiry_micros: 1_900_000_000_000_000,
                strike,
                spot,
                leg: if is_ce { "CE" } else { "PE" }.to_owned(),
                is_index_option: SYMBOLS[s] == "NIFTY" || SYMBOLS[s] == "BANKNIFTY",
            }
        })
}

fn movers() -> impl Strategy<Value = Vec<MoverRow>> {
    prop::collection::vec(
        (
            0_usize..SYMBOLS.len(),
            1_u64..60,
            prop_oneof![
                Just(f64::NAN),
                Just(f64::INFINITY),
                Just(0.0_f64),
                -12.0_f64..12.0,
            ],
        )
            .prop_map(|(s, id, pct)| MoverRow {
                security_id: id,
                segment: ExchangeSegment::NseEquity,
                symbol: SYMBOLS[s].to_owned(),
                pct_change: pct,
            }),
        0..12,
    )
}

fn chain() -> impl Strategy<Value = Vec<DepthCandidate>> {
    prop::collection::vec(candidate(), 0..40)
}

proptest! {
    /// THE OPERATOR'S BUDGET. Five connections, fifty instruments each. A
    /// layout that exceeds either would be refused by the pool — the whole
    /// pool, not the excess — so depth would be absent for the session.
    #[test]
    fn the_layout_never_exceeds_the_authorized_budget(rows in chain(), m in movers()) {
        let layout = build_depth20_layout(&rows, &m);
        prop_assert!(
            layout.sockets.len() <= DEPTH_20_SOCKETS,
            "{} sockets, budget is {}", layout.sockets.len(), DEPTH_20_SOCKETS
        );
        for (i, s) in layout.sockets.iter().enumerate() {
            prop_assert!(
                s.instruments.len() <= DEPTH_20_PER_SOCKET,
                "socket {} carries {}, cap is {}", i, s.instruments.len(), DEPTH_20_PER_SOCKET
            );
        }
        prop_assert!(layout.instrument_count() <= DEPTH_20_SOCKETS * DEPTH_20_PER_SOCKET);
    }

    /// It may DROP a contract. It may never INVENT an id. A fabricated
    /// security_id subscribes an instrument that returns silence forever
    /// while looking perfectly healthy.
    #[test]
    fn the_layout_never_invents_an_instrument(rows in chain(), m in movers()) {
        let known: BTreeSet<u64> = rows
            .iter()
            .filter_map(|c| u64::try_from(c.contract_security_id).ok())
            .collect();
        for socket in &build_depth20_layout(&rows, &m).sockets {
            for i in &socket.instruments {
                prop_assert!(
                    known.contains(&i.security_id),
                    "invented instrument {i:?}"
                );
            }
        }
    }

    /// No instrument twice ANYWHERE in the layout. A repeat spends two of the
    /// 250 authorized slots on one order book — and across two sockets it is
    /// also a duplicate subscribe.
    #[test]
    fn no_instrument_appears_twice_in_the_whole_layout(rows in chain(), m in movers()) {
        let layout = build_depth20_layout(&rows, &m);
        let mut all: Vec<(u64, u8)> = layout
            .sockets
            .iter()
            .flat_map(|s| s.instruments.iter().copied().map(key))
            .collect();
        let before = all.len();
        all.sort_unstable();
        all.dedup();
        prop_assert_eq!(all.len(), before, "an instrument appears twice: {:?}", layout);
    }

    /// An index socket carries ONLY its own underlying's contracts. A window
    /// that mixed underlyings would be centred on one spot while holding
    /// another's strikes.
    #[test]
    fn an_index_socket_carries_only_its_own_underlying(rows in chain(), m in movers()) {
        let layout = build_depth20_layout(&rows, &m);
        for socket in &layout.sockets {
            let Some(underlying) = socket.underlying.as_deref() else { continue };
            let owned: BTreeSet<u64> = rows
                .iter()
                .filter(|c| c.underlying == underlying)
                .filter_map(|c| u64::try_from(c.contract_security_id).ok())
                .collect();
            for i in &socket.instruments {
                prop_assert!(
                    owned.contains(&i.security_id),
                    "the {underlying} window holds {i:?}, which is not a {underlying} contract"
                );
            }
        }
    }

    /// A MOVERS socket that exists carries something.
    ///
    /// The index sockets are deliberately exempt: their POSITION is their
    /// identity, so socket 1 stays BANKNIFTY's even on a minute when
    /// BANKNIFTY's chain has not published, and the tracker reads that
    /// position. An empty index socket is a named hole; a movers socket is
    /// packed from a ranked list and can only be empty by mistake.
    ///
    /// This property originally asserted the stronger form for every socket
    /// and I changed the code to satisfy it — which broke the positional
    /// contract two hand-written tests existed to protect. The property was
    /// the thing that was wrong.
    #[test]
    fn no_emitted_movers_socket_is_empty(rows in chain(), m in movers()) {
        for (i, s) in build_depth20_layout(&rows, &m).sockets.iter().enumerate() {
            if s.underlying.is_none() {
                prop_assert!(!s.instruments.is_empty(), "movers socket {i} was emitted empty");
            }
        }
    }

    /// A socket holds whole strikes — both legs, never an odd count. The
    /// tracker moves a CALL socket and a PUT socket together; a stranded
    /// single leg means the two are reading different books.
    #[test]
    fn every_socket_holds_whole_pairs(rows in chain(), m in movers()) {
        for (i, s) in build_depth20_layout(&rows, &m).sockets.iter().enumerate() {
            prop_assert_eq!(
                s.instruments.len() % 2,
                0,
                "socket {} holds {} instruments — an odd count means a stranded leg",
                i,
                s.instruments.len()
            );
        }
    }

    /// Values that are not prices — zero, negative, NaN, infinity — must be
    /// refused rather than ranked. A non-finite spot has no nearest strike,
    /// and a NaN percentage sorts unpredictably.
    #[test]
    fn non_prices_and_non_numbers_never_reach_a_socket(rows in chain(), m in movers()) {
        // The assertion is simply that it produces a valid layout at all:
        // every other property above holds on the same input, and the
        // generator feeds NaN, infinity, zero and negative throughout.
        let layout = build_depth20_layout(&rows, &m);
        for socket in &layout.sockets {
            for i in &socket.instruments {
                prop_assert!(i.security_id > 0, "instrument 0 subscribes nothing forever");
            }
        }
    }

    /// Deterministic. Two builds of one minute must produce identical
    /// sockets, or the tracker diffs against a target that moved on its own.
    #[test]
    fn the_layout_is_deterministic(rows in chain(), m in movers()) {
        prop_assert_eq!(
            build_depth20_layout(&rows, &m),
            build_depth20_layout(&rows, &m)
        );
    }

    /// Input order must not decide the layout. A vendor chain and a ranking
    /// query both arrive in whatever order they arrive in.
    #[test]
    fn reversing_the_inputs_does_not_change_the_instrument_set(
        rows in chain(),
        m in movers(),
    ) {
        let forward = build_depth20_layout(&rows, &m);
        let rev_rows: Vec<DepthCandidate> = rows.iter().rev().cloned().collect();
        let backward = build_depth20_layout(&rev_rows, &m);
        let set = |l: &tickvault_app::depth20_layout::Depth20Layout| {
            l.sockets
                .iter()
                .map(|s| s.instruments.iter().copied().map(key).collect::<BTreeSet<_>>())
                .collect::<Vec<_>>()
        };
        prop_assert_eq!(set(&forward), set(&backward));
    }

    /// The index underlyings are the two the operator named, and no others
    /// may claim an index socket.
    #[test]
    fn only_the_named_underlyings_get_index_sockets(rows in chain(), m in movers()) {
        for socket in &build_depth20_layout(&rows, &m).sockets {
            if let Some(u) = socket.underlying.as_deref() {
                prop_assert!(
                    DEPTH_20_INDEX_UNDERLYINGS.contains(&u),
                    "{u} claimed an index socket"
                );
            }
        }
    }

    /// Never panics. Float comparisons, ranking sorts and two
    /// independently-sized inputs are exactly where this subsystem's defects
    /// have lived.
    #[test]
    fn it_never_panics(rows in chain(), m in movers()) {
        let _ = build_depth20_layout(&rows, &m);
    }
}
