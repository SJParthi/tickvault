//! The depth-20 layout the operator specified — 250 of 250 slots, by design
//! rather than by arithmetic.
//!
//! # What this replaces, and why the old shape was not merely smaller
//!
//! [`crate::dhan_depth_universe::depth_20_strikes_each_side`] sizes an
//! ADAPTIVE window: it divides the 250-slot envelope by however many
//! underlyings the chain happens to carry that day and takes that many strikes
//! either side of at-the-money, capped at 50. With two eligible index
//! underlyings that is ±50 each — 404 instruments — which the envelope guard
//! then truncates back to 250, nearest-at-the-money first.
//!
//! The result fills the budget and is still the wrong set. It spends every
//! slot on two indices, at strikes running fifty steps out where a 20-level
//! book is a handful of resting orders, and it carries no stock at all. The
//! operator's design spends the same 250 slots on a tighter index window and
//! the day's actual movers:
//!
//! | Socket | Carries | Slots |
//! |---|---|---|
//! | 1 | NIFTY at-the-money ±12, call and put | 50 |
//! | 2 | BANKNIFTY at-the-money ±12, call and put | 50 |
//! | 3–5 | 37 gainers + 37 losers + a 75th stock, at-the-money call and put each | 150 |
//!
//! Operator, 2026-08-26: *"this oen dude okay? B — NIFTY ATM ±12, CE + PE
//! both — 50"* for the index half, and *"widen to top 37 gainers/losers each"*
//! plus *"what about the remaining two dude we need to fill this also"* for
//! the movers half.
//!
//! # Why ±12 and not ±12.5
//!
//! 25 strikes at two legs is exactly 50, one whole connection. ±13 is 27
//! strikes and 54 instruments, which spills onto a second connection and
//! leaves BANKNIFTY short. The number is arithmetic, not preference.
//!
//! # This module is pure
//!
//! It takes a candidate slice and a ranking and returns the instruments. It
//! reads no database and opens no socket.

use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

use crate::depth_rebalance::{MoverRow, atm_pair_for};
use crate::dhan_depth_universe::{DepthCandidate, contract_segment_for_underlying};
use crate::movers::{MOVER_STOCKS_TOTAL, MoverSelection, rank_movers};

/// The two index underlyings that hold a whole depth-20 socket each.
///
/// The same pair the depth-200 sockets carry, and for the same reason: they
/// are the only two whose books are deep enough at every strike in a ±12
/// window to be worth a 20-level subscription.
pub const DEPTH_20_INDEX_UNDERLYINGS: [&str; 2] = ["NIFTY", "BANKNIFTY"];

/// Strikes either side of at-the-money on an index socket.
///
/// 12 gives 25 strikes, which at two legs is exactly one 50-instrument
/// connection. See the module header for why 13 does not work.
pub const DEPTH_20_INDEX_STRIKES_EACH_SIDE: usize = 12;

/// Instruments one depth-20 connection carries — Dhan's own per-connection
/// limit for the 20-level feed.
pub const DEPTH_20_PER_SOCKET: usize = 50;

/// Sockets given to the index windows.
pub const DEPTH_20_INDEX_SOCKETS: usize = DEPTH_20_INDEX_UNDERLYINGS.len();

/// Sockets given to the movers.
pub const DEPTH_20_MOVER_SOCKETS: usize = 3;

/// Every depth-20 socket.
pub const DEPTH_20_SOCKETS: usize = DEPTH_20_INDEX_SOCKETS + DEPTH_20_MOVER_SOCKETS;

/// One socket's worth of instruments, and which underlying it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct Depth20Socket {
    /// The underlying whose window this is, or `None` for a movers socket —
    /// a movers socket carries several stocks and belongs to no single one.
    pub underlying: Option<String>,
    /// What to subscribe, at most [`DEPTH_20_PER_SOCKET`].
    pub instruments: Vec<SubscribeInstrument>,
}

/// The whole depth-20 layout, plus what it could not fill.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Depth20Layout {
    /// Sockets in dial order: the index windows first, then the movers.
    pub sockets: Vec<Depth20Socket>,
    /// Index underlyings with no usable chain in the candidate slice.
    pub index_underlyings_unresolved: Vec<String>,
    /// Ranked stocks whose at-the-money pair could not be resolved.
    pub movers_unresolved: usize,
    /// The ranking this layout was built from, so the caller can report the
    /// same numbers it acted on rather than recomputing them.
    pub ranking: MoverSelection,
}

impl Depth20Layout {
    /// Total instruments across every socket.
    #[must_use]
    pub fn instrument_count(&self) -> usize {
        self.sockets.iter().map(|s| s.instruments.len()).sum()
    }

    /// Every instrument, in dial order — the flat form the pool planner takes.
    #[must_use]
    pub fn flattened(&self) -> Vec<SubscribeInstrument> {
        self.sockets
            .iter()
            .flat_map(|s| s.instruments.iter().copied())
            .collect()
    }
}

/// One index underlying's ±`each_side` window, both legs, ascending by strike.
///
/// Returns fewer than the full window when the chain is short on one side,
/// which is normal near the edges of a freshly-listed expiry. It never pads:
/// a strike that does not exist cannot be subscribed, and inventing one
/// subscribes an instrument id that returns silence forever.
#[must_use]
pub fn index_window(
    candidates: &[DepthCandidate],
    underlying: &str,
    each_side: usize,
) -> Vec<SubscribeInstrument> {
    let Some(segment) = contract_segment_for_underlying(underlying) else {
        // Fail-closed on an underlying we cannot name a segment for. Guessing
        // subscribes a well-formed request for the wrong instrument, which
        // comes back as silence — indistinguishable from a quiet book.
        return Vec::new();
    };
    let minutes = crate::depth_rebalance::chain_minutes_from_candidates(candidates);
    let Some(minute) = minutes.iter().find(|m| m.underlying == underlying) else {
        return Vec::new();
    };
    let Some(atm) = atm_pair_for(candidates, underlying) else {
        return Vec::new();
    };
    // The pairs are already sorted ascending, so the at-the-money strike's
    // position is where the window centres.
    let Some(centre) = minute
        .pairs
        .iter()
        .position(|p| p.strike_paise == atm.strike_paise)
    else {
        return Vec::new();
    };
    let lo = centre.saturating_sub(each_side);
    let hi = centre
        .saturating_add(each_side)
        .min(minute.pairs.len().saturating_sub(1));
    let mut out = Vec::with_capacity((hi - lo + 1) * 2);
    for pair in &minute.pairs[lo..=hi] {
        for id in [pair.ce_security_id, pair.pe_security_id] {
            if let Ok(security_id) = u64::try_from(id)
                && security_id > 0
            {
                out.push(SubscribeInstrument {
                    security_id,
                    segment,
                });
            }
        }
    }
    out
}

/// Builds the operator's depth-20 layout.
///
/// # Why the movers are packed rather than one socket per group
///
/// The gainers, the losers and the 75th are one set of 75 stocks, not three
/// groups of 25. Packing them 50 to a socket in ranked order means the three
/// sockets fill exactly, and a stock that drops out of the ranking is replaced
/// by the next one in line rather than leaving a hole on whichever socket it
/// happened to sit on.
///
/// # Complexity
///
/// O(candidates) per index underlying and per mover. Cold path, once a minute.
#[must_use]
pub fn build_depth20_layout(candidates: &[DepthCandidate], movers: &[MoverRow]) -> Depth20Layout {
    let mut out = Depth20Layout::default();

    for underlying in DEPTH_20_INDEX_UNDERLYINGS {
        let mut instruments =
            index_window(candidates, underlying, DEPTH_20_INDEX_STRIKES_EACH_SIDE);
        if instruments.is_empty() {
            out.index_underlyings_unresolved.push(underlying.to_owned());
        }
        // Never more than one connection's worth. A window wider than the
        // socket would spill onto the next one and push the second index
        // underlying off the end entirely.
        instruments.truncate(DEPTH_20_PER_SOCKET);
        // An index socket is emitted EVEN WHEN EMPTY, deliberately.
        //
        // Its position is its identity: socket 0 is NIFTY and socket 1 is
        // BANKNIFTY, and the rebalance and the per-minute tracker both read
        // that. Declining to emit an unresolved underlying's socket would
        // slide a movers socket into index position, so the tracker would
        // compare a stock ladder against BANKNIFTY's held set and swap the
        // whole socket — a far worse outcome than a socket that holds
        // nothing for a minute. `a_missing_index_chain_is_named_not_silently_dropped`
        // pins this.
        //
        // Suppressing it was tried, in the belief that an empty member of a
        // public list is always a defect. It is not one here, and the pin
        // above is what said so.
        out.sockets.push(Depth20Socket {
            underlying: Some(underlying.to_owned()),
            instruments,
        });
    }

    let moves: Vec<crate::movers::StockMove> = movers.iter().map(MoverRow::to_move).collect();
    out.ranking = rank_movers(&moves);

    // Ranked order, strongest first from each side, then the 75th. Symbols
    // come from the rows because the ranking carries only the numeric
    // identity — the contract rows key on the underlying's SYMBOL.
    let mut symbol_of = std::collections::HashMap::new();
    for row in movers {
        symbol_of.insert((row.security_id, row.segment), row.symbol.as_str());
    }
    let ranked: Vec<&crate::movers::StockMove> = out
        .ranking
        .gainers
        .iter()
        .chain(out.ranking.losers.iter())
        .chain(out.ranking.tiebreak.iter())
        .collect();

    // No instrument may appear twice in the WHOLE layout.
    //
    // Two slots spent on one order book is the mild reading. The sharp one
    // is that two sockets carrying the same instrument means two connections
    // subscribing it, and on Dhan's per-connection subscription state the
    // second is a duplicate.
    //
    // Reachable when an index underlying also appears in the ranking: its
    // at-the-money pair would then be taken by both its own index window and
    // a movers socket. The movers query filters to NSE_EQ and an index is
    // IDX_I, so that should not happen today — but "should not happen today"
    // is a property of the QUERY, not of this function, and a later widening
    // of that filter would land here silently. Found by a property test.
    let mut seen: std::collections::HashSet<(u64, tickvault_common::types::ExchangeSegment)> = out
        .sockets
        .iter()
        .flat_map(|s| s.instruments.iter().map(|i| (i.security_id, i.segment)))
        .collect();

    let mut mover_instruments = Vec::with_capacity(MOVER_STOCKS_TOTAL * 2);
    for stock in ranked {
        let Some(symbol) = symbol_of.get(&stock.key()) else {
            out.movers_unresolved = out.movers_unresolved.saturating_add(1);
            continue;
        };
        let Some(pair) = atm_pair_for(candidates, symbol) else {
            // A ranked stock whose ladder is not in the slice. Counted rather
            // than silently skipped: at any real scale a rising count here
            // means the contract artifact and the candle frames disagree about
            // which stocks exist.
            out.movers_unresolved = out.movers_unresolved.saturating_add(1);
            continue;
        };
        // Both legs or neither.
        //
        // A pair is the unit a depth socket subscribes: the tracker moves a
        // CALL and a PUT together, so admitting one leg strands the other on
        // a different strike and the two sockets stop reading the same book.
        // Building the pair first and admitting it whole is what keeps every
        // socket's instrument count even.
        let legs: Vec<SubscribeInstrument> = [pair.ce_security_id, pair.pe_security_id]
            .into_iter()
            .filter_map(|id| u64::try_from(id).ok())
            .filter(|id| *id > 0)
            .map(|security_id| SubscribeInstrument {
                security_id,
                segment: crate::depth_rebalance::STOCK_OPTION_SEGMENT,
            })
            .collect();
        let [ce, pe] = legs.as_slice() else {
            out.movers_unresolved = out.movers_unresolved.saturating_add(1);
            continue;
        };
        // Skip the PAIR when either leg is already on a socket. Skipping the
        // leg alone would leave the other stranded — which is how the first
        // version of this deduplication produced odd-sized sockets.
        if !seen.insert((ce.security_id, ce.segment)) {
            continue;
        }
        if !seen.insert((pe.security_id, pe.segment)) {
            // Undo the CE claim: the pair is refused, so it never took a
            // slot and must not block a later pair that legitimately holds
            // that instrument.
            seen.remove(&(ce.security_id, ce.segment));
            continue;
        }
        mover_instruments.push(*ce);
        mover_instruments.push(*pe);
    }
    // Truncate on a PAIR boundary — the cap is an even number of legs, so
    // this cannot split a pair, and an odd truncation would strand one.
    mover_instruments.truncate(DEPTH_20_MOVER_SOCKETS * DEPTH_20_PER_SOCKET);

    for chunk in mover_instruments.chunks(DEPTH_20_PER_SOCKET) {
        out.sockets.push(Depth20Socket {
            underlying: None,
            instruments: chunk.to_vec(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::types::ExchangeSegment;

    fn candidate(u: &str, strike: f64, leg: &str, id: i64, spot: f64) -> DepthCandidate {
        DepthCandidate {
            underlying: u.to_owned(),
            contract_security_id: id,
            expiry_micros: 1_900_000_000_000_000,
            strike,
            spot,
            leg: leg.to_owned(),
            is_index_option: DEPTH_20_INDEX_UNDERLYINGS.contains(&u),
        }
    }

    fn mover(id: u64, symbol: &str, pct: f64) -> MoverRow {
        MoverRow {
            security_id: id,
            segment: ExchangeSegment::NseEquity,
            symbol: symbol.to_owned(),
            pct_change: pct,
        }
    }

    /// A chain of `strikes` steps of `step` rupees centred on `spot`.
    fn chain(u: &str, spot: f64, step: f64, strikes: i64, id_base: i64) -> Vec<DepthCandidate> {
        let mut out = Vec::new();
        let half = strikes / 2;
        for k in -half..=half {
            #[expect(clippy::cast_precision_loss, reason = "k is tiny")]
            let strike = (spot / step).round() * step + (k as f64) * step;
            let ce = id_base + k * 2;
            let pe = id_base + k * 2 + 1;
            out.push(candidate(u, strike, "CE", ce, spot));
            out.push(candidate(u, strike, "PE", pe, spot));
        }
        out
    }

    /// A stock with exactly one at-the-money pair.
    fn stock(symbol: &str, id_base: i64) -> Vec<DepthCandidate> {
        let mut c1 = candidate(symbol, 100.0, "CE", id_base, 99.0);
        let mut c2 = candidate(symbol, 100.0, "PE", id_base + 1, 99.0);
        c1.is_index_option = false;
        c2.is_index_option = false;
        vec![c1, c2]
    }

    // ---- the arithmetic that makes the layout fit ----

    #[test]
    fn the_index_window_is_exactly_one_socket() {
        // 25 strikes at two legs is 50. This is the arithmetic the whole
        // layout rests on; if it drifts, one index underlying silently loses
        // its socket to the other.
        assert_eq!(
            (DEPTH_20_INDEX_STRIKES_EACH_SIDE * 2 + 1) * 2,
            DEPTH_20_PER_SOCKET
        );
    }

    #[test]
    fn the_five_sockets_account_for_every_authorized_slot() {
        assert_eq!(DEPTH_20_SOCKETS, 5);
        assert_eq!(
            DEPTH_20_SOCKETS * DEPTH_20_PER_SOCKET,
            crate::dhan_depth_universe::DEPTH_20_MAX_INSTRUMENTS
        );
    }

    #[test]
    fn the_movers_fill_their_three_sockets_exactly() {
        // 75 stocks at two legs is 150, which is three sockets of 50.
        assert_eq!(
            MOVER_STOCKS_TOTAL * 2,
            DEPTH_20_MOVER_SOCKETS * DEPTH_20_PER_SOCKET
        );
    }

    // ---- the index window ----

    #[test]
    fn a_full_chain_gives_a_full_window_both_legs() {
        let c = chain("NIFTY", 24_500.0, 50.0, 60, 1_000);
        let got = index_window(&c, "NIFTY", DEPTH_20_INDEX_STRIKES_EACH_SIDE);
        assert_eq!(got.len(), DEPTH_20_PER_SOCKET);
        assert!(got.iter().all(|i| i.segment == ExchangeSegment::NseFno));
    }

    #[test]
    fn a_short_chain_gives_a_short_window_and_never_pads() {
        // Inventing a strike that does not exist subscribes an id that
        // returns silence forever — indistinguishable from a quiet book.
        let c = chain("NIFTY", 24_500.0, 50.0, 4, 1_000);
        let got = index_window(&c, "NIFTY", DEPTH_20_INDEX_STRIKES_EACH_SIDE);
        assert_eq!(got.len(), 10, "5 strikes x 2 legs");
    }

    #[test]
    fn the_window_is_centred_on_at_the_money_not_on_the_chain() {
        // Spot near the bottom of a chain: the window must clamp, not wrap.
        let mut c = chain("NIFTY", 24_500.0, 50.0, 60, 1_000);
        for row in &mut c {
            row.spot = 23_400.0;
        }
        let got = index_window(&c, "NIFTY", 2);
        assert!(got.len() <= 10, "clamped, got {}", got.len());
        assert!(!got.is_empty());
    }

    #[test]
    fn an_underlying_with_no_nameable_segment_is_refused_not_guessed() {
        let mut c = chain("MYSTERY", 100.0, 5.0, 10, 1_000);
        for row in &mut c {
            row.is_index_option = true;
        }
        assert!(index_window(&c, "MYSTERY", 12).is_empty());
    }

    #[test]
    fn an_underlying_absent_from_the_slice_yields_nothing() {
        let c = chain("NIFTY", 24_500.0, 50.0, 10, 1_000);
        assert!(index_window(&c, "BANKNIFTY", 12).is_empty());
    }

    #[test]
    fn a_zero_contract_id_never_reaches_the_window() {
        let c = vec![
            candidate("NIFTY", 24_500.0, "CE", 0, 24_500.0),
            candidate("NIFTY", 24_500.0, "PE", 0, 24_500.0),
        ];
        assert!(index_window(&c, "NIFTY", 12).is_empty());
    }

    #[test]
    fn a_negative_contract_id_never_reaches_the_window() {
        let c = vec![
            candidate("NIFTY", 24_500.0, "CE", -5, 24_500.0),
            candidate("NIFTY", 24_500.0, "PE", -6, 24_500.0),
        ];
        assert!(index_window(&c, "NIFTY", 12).is_empty());
    }

    #[test]
    fn a_zero_width_window_still_takes_the_at_the_money_strike() {
        // The single most informative strike beats none.
        let c = chain("NIFTY", 24_500.0, 50.0, 20, 1_000);
        let got = index_window(&c, "NIFTY", 0);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn an_empty_slice_yields_an_empty_window() {
        assert!(index_window(&[], "NIFTY", 12).is_empty());
    }

    // ---- the whole layout ----

    #[test]
    fn a_full_market_fills_every_socket() {
        let mut c = chain("NIFTY", 24_500.0, 50.0, 60, 10_000);
        c.extend(chain("BANKNIFTY", 54_000.0, 100.0, 60, 20_000));
        let mut movers = Vec::new();
        for i in 0..MOVER_STOCKS_TOTAL {
            let symbol = format!("S{i}");
            #[expect(clippy::cast_precision_loss, reason = "i is tiny")]
            let pct = if i % 2 == 0 {
                10.0 - (i as f64) * 0.1
            } else {
                -10.0 + (i as f64) * 0.1
            };
            let id = 100_000 + u64::try_from(i).expect("small");
            movers.push(mover(id, &symbol, pct));
            c.extend(stock(
                &symbol,
                500_000 + i64::try_from(i).expect("small") * 2,
            ));
        }
        let got = build_depth20_layout(&c, &movers);
        assert_eq!(got.sockets.len(), DEPTH_20_SOCKETS, "{got:?}");
        assert_eq!(got.instrument_count(), DEPTH_20_MAX_SLOTS_CHECK);
        assert!(got.index_underlyings_unresolved.is_empty());
        assert_eq!(got.movers_unresolved, 0);
    }

    const DEPTH_20_MAX_SLOTS_CHECK: usize = 250;

    #[test]
    fn a_flat_market_still_gives_both_index_sockets() {
        // No stock has moved, so the movers sockets are empty — but NIFTY and
        // BANKNIFTY are still worth watching, and refusing the whole layout
        // because the ranking is empty would be the wrong trade.
        let mut c = chain("NIFTY", 24_500.0, 50.0, 60, 10_000);
        c.extend(chain("BANKNIFTY", 54_000.0, 100.0, 60, 20_000));
        let got = build_depth20_layout(&c, &[]);
        assert_eq!(got.sockets.len(), DEPTH_20_INDEX_SOCKETS);
        assert_eq!(got.instrument_count(), DEPTH_20_PER_SOCKET * 2);
    }

    #[test]
    fn a_missing_index_chain_is_named_not_silently_dropped() {
        let c = chain("NIFTY", 24_500.0, 50.0, 60, 10_000);
        let got = build_depth20_layout(&c, &[]);
        assert_eq!(got.index_underlyings_unresolved, vec!["BANKNIFTY"]);
        // The socket still exists, empty, so dial order never shifts and
        // BANKNIFTY's socket cannot silently become a movers socket.
        assert_eq!(got.sockets.len(), DEPTH_20_INDEX_SOCKETS);
        assert!(got.sockets[1].instruments.is_empty());
    }

    #[test]
    fn dial_order_puts_the_indices_first_always() {
        // A movers socket landing at index 0 would put stocks where the
        // rebalance expects NIFTY.
        let mut c = chain("NIFTY", 24_500.0, 50.0, 60, 10_000);
        c.extend(chain("BANKNIFTY", 54_000.0, 100.0, 60, 20_000));
        c.extend(stock("ALPHA", 900_000));
        let got = build_depth20_layout(&c, &[mover(1, "ALPHA", 9.0)]);
        assert_eq!(got.sockets[0].underlying.as_deref(), Some("NIFTY"));
        assert_eq!(got.sockets[1].underlying.as_deref(), Some("BANKNIFTY"));
        assert_eq!(got.sockets[2].underlying, None);
    }

    #[test]
    fn a_ranked_stock_with_no_ladder_is_counted_not_skipped_silently() {
        // At any real scale a rising count here means the contract artifact
        // and the candle frames disagree about which stocks exist.
        let mut c = chain("NIFTY", 24_500.0, 50.0, 60, 10_000);
        c.extend(chain("BANKNIFTY", 54_000.0, 100.0, 60, 20_000));
        let got = build_depth20_layout(&c, &[mover(1, "GHOST", 9.0)]);
        assert_eq!(got.movers_unresolved, 1);
    }

    #[test]
    fn no_socket_ever_exceeds_its_connection_limit() {
        // The invariant that keeps plan_pool from refusing the WHOLE pool: an
        // oversized set costs the session all depth, not just the excess.
        let mut c = chain("NIFTY", 24_500.0, 50.0, 400, 10_000);
        c.extend(chain("BANKNIFTY", 54_000.0, 100.0, 400, 900_000));
        let mut movers = Vec::new();
        for i in 0..200_usize {
            let symbol = format!("S{i}");
            #[expect(clippy::cast_precision_loss, reason = "i is tiny")]
            let pct = 20.0 - (i as f64) * 0.05;
            movers.push(mover(
                2_000_000 + u64::try_from(i).expect("small"),
                &symbol,
                pct,
            ));
            c.extend(stock(
                &symbol,
                3_000_000 + i64::try_from(i).expect("small") * 2,
            ));
        }
        let got = build_depth20_layout(&c, &movers);
        for socket in &got.sockets {
            assert!(
                socket.instruments.len() <= DEPTH_20_PER_SOCKET,
                "socket over the per-connection limit: {}",
                socket.instruments.len()
            );
        }
        assert!(
            got.sockets.len() <= DEPTH_20_SOCKETS,
            "{}",
            got.sockets.len()
        );
        assert!(got.instrument_count() <= DEPTH_20_MAX_SLOTS_CHECK);
    }

    #[test]
    fn the_same_instrument_never_appears_twice_in_the_layout() {
        // A duplicate inside one subscribe batch is what Dhan answers with an
        // 804, which is Fatal and drops the connection.
        let mut c = chain("NIFTY", 24_500.0, 50.0, 60, 10_000);
        c.extend(chain("BANKNIFTY", 54_000.0, 100.0, 60, 20_000));
        for i in 0..20_usize {
            let symbol = format!("S{i}");
            c.extend(stock(
                &symbol,
                500_000 + i64::try_from(i).expect("small") * 2,
            ));
        }
        let movers: Vec<MoverRow> = (0..20_usize)
            .map(|i| {
                #[expect(clippy::cast_precision_loss, reason = "i is tiny")]
                let pct = 5.0 - (i as f64) * 0.1;
                mover(
                    100_000 + u64::try_from(i).expect("small"),
                    &format!("S{i}"),
                    pct,
                )
            })
            .collect();
        let got = build_depth20_layout(&c, &movers);
        let flat = got.flattened();
        let mut seen = std::collections::HashSet::new();
        for i in &flat {
            assert!(
                seen.insert((i.security_id, i.segment)),
                "duplicate instrument {i:?} — a repeated id in one batch is an 804"
            );
        }
    }

    #[test]
    fn a_completely_empty_market_yields_two_empty_index_sockets_and_no_panic() {
        let got = build_depth20_layout(&[], &[]);
        assert_eq!(got.sockets.len(), DEPTH_20_INDEX_SOCKETS);
        assert_eq!(got.instrument_count(), 0);
        assert_eq!(got.index_underlyings_unresolved.len(), 2);
    }

    #[test]
    fn non_finite_and_flat_moves_never_reach_a_socket() {
        let mut c = chain("NIFTY", 24_500.0, 50.0, 60, 10_000);
        c.extend(chain("BANKNIFTY", 54_000.0, 100.0, 60, 20_000));
        c.extend(stock("ALPHA", 900_000));
        let movers = vec![
            mover(1, "ALPHA", f64::NAN),
            mover(2, "ALPHA", 0.0),
            mover(3, "ALPHA", f64::INFINITY),
        ];
        let got = build_depth20_layout(&c, &movers);
        assert_eq!(got.sockets.len(), DEPTH_20_INDEX_SOCKETS);
    }

    #[test]
    fn the_flattened_form_preserves_dial_order() {
        let mut c = chain("NIFTY", 24_500.0, 50.0, 60, 10_000);
        c.extend(chain("BANKNIFTY", 54_000.0, 100.0, 60, 20_000));
        let got = build_depth20_layout(&c, &[]);
        let flat = got.flattened();
        assert_eq!(flat.len(), got.instrument_count());
        assert_eq!(flat[0], got.sockets[0].instruments[0]);
        assert_eq!(flat[DEPTH_20_PER_SOCKET], got.sockets[1].instruments[0]);
    }

    #[test]
    fn a_stock_option_never_carries_an_index_segment() {
        let mut c = chain("NIFTY", 24_500.0, 50.0, 60, 10_000);
        c.extend(chain("BANKNIFTY", 54_000.0, 100.0, 60, 20_000));
        c.extend(stock("ALPHA", 900_000));
        let got = build_depth20_layout(&c, &[mover(1, "ALPHA", 9.0)]);
        for i in &got.sockets[2].instruments {
            assert_eq!(i.segment, crate::depth_rebalance::STOCK_OPTION_SEGMENT);
            assert!(crate::dhan_depth_universe::segment_supports_depth(
                i.segment
            ));
        }
    }

    #[test]
    fn every_index_instrument_sits_in_a_segment_that_can_carry_depth() {
        // SENSEX trades in BSE_FNO and Dhan serves depth on NSE only, so a
        // BSE underlying reaching here would dial a socket that dies on
        // connect.
        let mut c = chain("NIFTY", 24_500.0, 50.0, 60, 10_000);
        c.extend(chain("BANKNIFTY", 54_000.0, 100.0, 60, 20_000));
        let got = build_depth20_layout(&c, &[]);
        for socket in &got.sockets {
            for i in &socket.instruments {
                assert!(
                    crate::dhan_depth_universe::segment_supports_depth(i.segment),
                    "{i:?} is in a segment Dhan does not serve depth on"
                );
            }
        }
    }
}

#[cfg(test)]
mod truncate_reach_tests {
    use super::*;

    fn candidate(u: &str, strike: f64, leg: &str, id: i64, spot: f64) -> DepthCandidate {
        DepthCandidate {
            underlying: u.to_owned(),
            contract_security_id: id,
            expiry_micros: 1_900_000_000_000_000,
            strike,
            spot,
            leg: leg.to_owned(),
            is_index_option: true,
        }
    }

    fn wide_chain() -> Vec<DepthCandidate> {
        let mut out = Vec::new();
        for k in -100_i64..=100 {
            #[expect(clippy::cast_precision_loss, reason = "k is tiny")]
            let strike = 24_500.0 + (k as f64) * 50.0;
            out.push(candidate("NIFTY", strike, "CE", 10_000 + k * 2, 24_500.0));
            out.push(candidate("NIFTY", strike, "PE", 10_001 + k * 2, 24_500.0));
        }
        out
    }

    #[test]
    fn the_per_socket_truncate_is_unreachable_today_and_that_is_the_point() {
        // HONEST NOTE, written after a bite-proof: removing the truncate in
        // `build_depth20_layout` fails NO test, because ±12 can only ever
        // produce 50 instruments. The truncate is therefore not a live guard —
        // it is a guard against a FUTURE change to
        // DEPTH_20_INDEX_STRIKES_EACH_SIDE, and claiming otherwise would be
        // the false-OK this repo keeps finding.
        //
        // What this test proves is that the hazard is real: a wider window
        // DOES exceed one connection, so raising the constant without the
        // truncate would push the second index underlying off the end of the
        // pool entirely.
        let wide = index_window(&wide_chain(), "NIFTY", 40);
        assert!(
            wide.len() > DEPTH_20_PER_SOCKET,
            "a ±40 window must exceed one socket, else the truncate guards nothing: {}",
            wide.len()
        );
        // And the constant in force today does not.
        let live = index_window(&wide_chain(), "NIFTY", DEPTH_20_INDEX_STRIKES_EACH_SIDE);
        assert_eq!(live.len(), DEPTH_20_PER_SOCKET);
    }

    #[test]
    fn a_window_wider_than_the_chain_takes_the_whole_chain_and_stops() {
        let c = wide_chain();
        let got = index_window(&c, "NIFTY", 10_000);
        assert_eq!(got.len(), 201 * 2, "every strike, both legs, no more");
    }

    #[test]
    fn an_each_side_at_the_usize_ceiling_does_not_overflow() {
        // saturating_add is why. A plain `centre + each_side` panics in debug
        // and wraps in release, and a wrapped high bound reads as a low one —
        // the window would silently collapse to nothing.
        let got = index_window(&wide_chain(), "NIFTY", usize::MAX);
        assert_eq!(got.len(), 201 * 2);
    }
}

#[cfg(test)]
mod cap_chain_tests {
    use super::*;
    use tickvault_common::types::ExchangeSegment;

    /// WHERE THE 250-SLOT BOUND ACTUALLY COMES FROM.
    ///
    /// Written after two bite-proofs that both FAILED TO BITE: removing either
    /// `truncate` in `build_depth20_layout` breaks no test, because neither is
    /// reachable. Recording that honestly matters more than the guards do —
    /// a test suite that appears to cover a bound it never exercises is the
    /// false-OK class this repo keeps rediscovering.
    ///
    /// The bound is enforced UPSTREAM, in this chain:
    ///
    /// | Step | Cap | Enforced by |
    /// |---|---|---|
    /// | ranked stocks | 75 | `rank_movers`, which takes at most 37 a side plus one |
    /// | mover instruments | 150 | 75 stocks x 2 legs |
    /// | mover sockets | 3 | 150 / 50 per connection |
    /// | index instruments | 50 each | `DEPTH_20_INDEX_STRIKES_EACH_SIDE` = 12 gives 25 strikes x 2 |
    /// | total | 250 | 2 x 50 + 3 x 50 |
    ///
    /// The two `truncate` calls are redundant belt-and-braces against a future
    /// change to those constants. They are kept, and they are NOT claimed to
    /// be doing the work today.
    #[test]
    fn the_ranking_is_what_bounds_the_movers_not_the_truncate() {
        let moves: Vec<crate::movers::StockMove> = (0..500_u64)
            .map(|i| {
                #[expect(clippy::cast_precision_loss, reason = "i is tiny")]
                let pct = 30.0 - (i as f64) * 0.01;
                crate::movers::StockMove {
                    security_id: i + 1,
                    segment: ExchangeSegment::NseEquity,
                    pct_change: pct,
                }
            })
            .collect();
        let ranking = rank_movers(&moves);
        let selected =
            ranking.gainers.len() + ranking.losers.len() + usize::from(ranking.tiebreak.is_some());
        assert!(
            selected <= MOVER_STOCKS_TOTAL,
            "500 movers in, {selected} out — the ranking is the real cap"
        );
        assert!(
            selected * 2 <= DEPTH_20_MOVER_SOCKETS * DEPTH_20_PER_SOCKET,
            "the ranking's own cap must already fit the three sockets"
        );
    }

    #[test]
    fn the_constants_multiply_out_to_exactly_the_authorized_envelope() {
        // If any one of these drifts, the layout either strands slots or
        // overflows the pool — and plan_pool refuses the WHOLE pool rather
        // than truncating, so an overflow costs the session all depth.
        let index_slots = DEPTH_20_INDEX_SOCKETS * DEPTH_20_PER_SOCKET;
        let mover_slots = DEPTH_20_MOVER_SOCKETS * DEPTH_20_PER_SOCKET;
        assert_eq!(index_slots, 100);
        assert_eq!(mover_slots, 150);
        assert_eq!(
            index_slots + mover_slots,
            crate::dhan_depth_universe::DEPTH_20_MAX_INSTRUMENTS
        );
        assert_eq!(MOVER_STOCKS_TOTAL * 2, mover_slots);
        assert_eq!(
            (DEPTH_20_INDEX_STRIKES_EACH_SIDE * 2 + 1) * 2,
            DEPTH_20_PER_SOCKET
        );
    }
}

#[cfg(test)]
mod nine_thirteen_readiness_tests {
    use super::*;
    use crate::depth_rebalance::MoverRow;
    use tickvault_common::types::ExchangeSegment;

    const EXPIRY: i64 = 1_900_000_000_000_000;

    fn opt(
        underlying: &str,
        strike: f64,
        leg: &str,
        id: i64,
        spot: f64,
        index: bool,
    ) -> DepthCandidate {
        DepthCandidate {
            underlying: underlying.to_owned(),
            contract_security_id: id,
            expiry_micros: EXPIRY,
            strike,
            spot,
            leg: leg.to_owned(),
            is_index_option: index,
        }
    }

    /// A pre-open chain: both index windows plus enough stock ladders to
    /// fill the three movers sockets.
    fn preopen_candidates() -> Vec<DepthCandidate> {
        let mut c = Vec::new();
        let mut id = 1_000_i64;
        // The two index windows, wide enough for +/-12 either side.
        for (u, spot) in [("NIFTY", 24_500.0), ("BANKNIFTY", 54_000.0)] {
            for k in -14_i64..=14 {
                #[expect(clippy::cast_precision_loss, reason = "k is tiny")]
                let strike = (k as f64).mul_add(50.0, spot);
                c.push(opt(u, strike, "CE", id, spot, true));
                id += 1;
                c.push(opt(u, strike, "PE", id, spot, true));
                id += 1;
            }
        }
        // 80 stocks, one at-the-money pair each — more than the 75 the
        // movers sockets can take, so the cap is exercised.
        for s in 0..80_i64 {
            let sym = format!("STK{s:03}");
            c.push(opt(&sym, 1_000.0, "CE", id, 1_000.0, false));
            id += 1;
            c.push(opt(&sym, 1_000.0, "PE", id, 1_000.0, false));
            id += 1;
        }
        c
    }

    /// The ranking as the PRE-OPEN source produces it — from ticks, before
    /// any candle has sealed.
    fn preopen_movers() -> Vec<MoverRow> {
        (0..80_i64)
            .map(|s| MoverRow {
                security_id: 90_000 + u64::try_from(s).unwrap_or(0),
                segment: ExchangeSegment::NseEquity,
                symbol: format!("STK{s:03}"),
                // Half up, half down, so both gainers and losers resolve.
                #[expect(clippy::cast_precision_loss, reason = "s is tiny")]
                pct_change: if s % 2 == 0 {
                    5.0 - (s as f64) * 0.01
                } else {
                    -5.0 + (s as f64) * 0.01
                },
            })
            .collect()
    }

    /// THE 09:13 PROOF. Given only what exists before the first candle
    /// seals — a chain with spot prices, and a ranking sourced from ticks —
    /// every one of the five depth-20 sockets fills.
    ///
    /// Before the pre-open ranking existed this same input with an EMPTY
    /// mover list left three of the five sockets carrying nothing, which is
    /// the second assertion below.
    #[test]
    fn all_five_sockets_fill_from_preopen_inputs_alone() {
        let layout = build_depth20_layout(&preopen_candidates(), &preopen_movers());
        assert_eq!(layout.sockets.len(), DEPTH_20_SOCKETS, "{layout:?}");
        for (i, s) in layout.sockets.iter().enumerate() {
            assert!(
                !s.instruments.is_empty(),
                "socket {i} is empty at 09:13 — it would carry no depth until the first \
                 candle seals around 09:16: {layout:?}"
            );
        }
        assert_eq!(
            layout.instrument_count(),
            DEPTH_20_SOCKETS * DEPTH_20_PER_SOCKET,
            "the operator's 250 were not all filled: {layout:?}"
        );
    }

    /// Non-vacuity, and the finding is worse than "three sockets carry
    /// nothing": with no ranking the layout does not EMIT the movers sockets
    /// at all, so three of the five depth-20 connections are never dialed.
    ///
    /// This is exactly what 09:13 looked like before the pre-open ranking
    /// existed — 100 of the operator's 250 instruments, on two connections
    /// out of five — and it is why that change is not cosmetic.
    #[test]
    fn without_a_ranking_three_of_the_five_connections_are_never_dialed() {
        let layout = build_depth20_layout(&preopen_candidates(), &[]);
        assert_eq!(
            layout.sockets.len(),
            DEPTH_20_INDEX_SOCKETS,
            "only the index windows should survive an empty ranking: {layout:?}"
        );
        assert_eq!(
            layout.instrument_count(),
            DEPTH_20_INDEX_SOCKETS * DEPTH_20_PER_SOCKET,
            "100 of 250 — the movers half is entirely absent"
        );
        // The index windows are unaffected: they read spot, not a ranking.
        for s in &layout.sockets {
            assert_eq!(s.instruments.len(), DEPTH_20_PER_SOCKET);
        }
    }

    /// A ranking that resolves only a few stocks still fills what it can,
    /// rather than refusing the whole set — the shape a thin pre-open takes.
    #[test]
    fn a_partial_ranking_fills_partially_and_says_how_much() {
        let few: Vec<MoverRow> = preopen_movers().into_iter().take(6).collect();
        let layout = build_depth20_layout(&preopen_candidates(), &few);
        assert!(layout.instrument_count() > DEPTH_20_SOCKETS_INDEX_INSTRUMENTS);
        assert!(
            layout.instrument_count() < DEPTH_20_SOCKETS * DEPTH_20_PER_SOCKET,
            "a six-stock ranking should not fill all 150 movers slots: {layout:?}"
        );
    }

    /// A ranking naming stocks that have no chain resolves to nothing and is
    /// COUNTED, never silently dropped.
    #[test]
    fn a_ranking_of_stocks_with_no_chain_is_counted_not_hidden() {
        let ghosts: Vec<MoverRow> = (0..10_i64)
            .map(|s| MoverRow {
                security_id: 70_000 + u64::try_from(s).unwrap_or(0),
                segment: ExchangeSegment::NseEquity,
                symbol: format!("GHOST{s}"),
                pct_change: 9.0,
            })
            .collect();
        let layout = build_depth20_layout(&preopen_candidates(), &ghosts);
        assert!(layout.movers_unresolved > 0, "{layout:?}");
    }

    const DEPTH_20_SOCKETS_INDEX_INSTRUMENTS: usize = DEPTH_20_INDEX_SOCKETS * DEPTH_20_PER_SOCKET;
}
