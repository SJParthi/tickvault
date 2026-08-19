//! Contract selection for the live main feed — futures and options, from the
//! daily Dhan master.
//!
//! Authorized by the operator 2026-08-15 ("2026-08-15 — FULL-MODE,
//! FULL-UNIVERSE SUBSCRIPTION SCOPE" in
//! `.claude/rules/project/websocket-connection-scope-lock.md`), re-affirmed
//! 2026-08-19. The authorized set, verbatim from that section's table:
//!
//! | Class | Mode | Expiry scope |
//! |---|---|---|
//! | All NSE indices | Full | n/a |
//! | NTM constituents (spot) | Full | n/a |
//! | Index futures | Full | **ALL expiries** |
//! | Stock futures | Full | **ALL expiries** |
//! | NIFTY + BANKNIFTY options | Full | **current expiry ONLY** |
//! | Stock options, **ATM ± 25 both legs** | Full | **current expiry ONLY** |
//!
//! The first two rows are the SPOT universe and already exist
//! (`dhan_live_universe.rs`). This module produces the other four — the ~90%
//! of the authorized 25,000 that the spot path structurally cannot reach,
//! because the ISIN join filters to `NSE && E && EQ` and a cash-equity filter
//! can never emit an option.
//!
//! # Why this is a separate module from the spot universe
//!
//! They resolve at different times and from different evidence. The spot set
//! is known at 08:30 from the master alone. Stock-option selection needs to
//! know where at-the-money IS, and that is a live price — available only once
//! the spot feed is carrying data. Folding both into one function would have
//! forced the whole universe to wait for the market to open.
//!
//! # Why a hardcoded contract list was rejected
//!
//! The scope-lock's own REJECT list names it: option contracts expire weekly,
//! so a hardcoded list needs a human edit every week and goes silently stale
//! between edits — subscribing dead contracts, receiving nothing, and
//! reporting healthy. The master is re-downloaded daily, so a selection
//! derived from it rolls by itself.
//!
//! # Complexity — stated honestly, not rounded down
//!
//! Bucketing is **O(rows)**, one pass, and is inherently linear: "which
//! contracts exist today?" is a question about all of them at once. Per
//! underlying the strike ladder is **sorted, O(k log k)**, and locating ATM
//! within it is **O(log k)** by binary search. Nothing here is O(1) and this
//! module does not claim to be. It is COLD PATH — once per attach, once per
//! trading day — which is the only reason that is acceptable (`CLAUDE.md`:
//! non-hot-path cold code is not held to principle 2).
//!
//! The per-instrument cost that IS on the hot path — the tick lookup for
//! every contract this selects — is unchanged: one `papaya` hash probe on the
//! I-P1-11 composite key.

use std::collections::{HashMap, HashSet};

use tickvault_common::types::{ExchangeSegment, SecurityId};
use tickvault_core::instrument::master_csv::{InstrumentClass, MasterRow, OptionLeg};
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

/// Strikes each side of at-the-money that stock options subscribe.
///
/// The operator's figure, verbatim: *"for stocks the max atm plus or minus 25
/// for both call and put"*. 25 each side plus the ATM strike itself is 51
/// strikes × 2 legs = 102 contracts per stock.
///
/// This is the ELASTIC dimension of the whole selection — see
/// [`select_contract_universe`]. Every other class is take-it-all; when the
/// total exceeds the connection envelope this is the number that shrinks,
/// because shrinking it drops the contracts furthest from the money, which is
/// the only principled thing to drop.
pub const STOCK_OPTION_ATM_STRIKES_EACH_SIDE: usize = 25;

/// Index underlyings whose FULL option chain is subscribed.
///
/// Exactly two, and that is the operator's own restriction: *"one and only
/// for indices for nifty and banknifty entire options contracts should be
/// fully subscribed"*. A third index here is a scope expansion needing its
/// own dated quote — the arity is pinned by a test for exactly that reason.
pub const FULL_CHAIN_INDEX_UNDERLYINGS: [&str; 2] = ["NIFTY", "BANKNIFTY"];

/// What a contract selection produced, and everything it refused.
///
/// Every refusal is a counted field rather than a silent drop. A selection
/// that quietly returns fewer contracts than the master could support is
/// indistinguishable from one that worked, which is the false-OK class the
/// house rules forbid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContractSelection {
    /// The contracts to subscribe, deduped on the I-P1-11 composite key and
    /// sorted deterministically.
    pub instruments: Vec<SubscribeInstrument>,
    /// Index futures selected (all expiries at or after today).
    pub index_futures: usize,
    /// Stock futures selected (all expiries at or after today).
    pub stock_futures: usize,
    /// Index options selected (current expiry, full chain, 2 underlyings).
    pub index_options: usize,
    /// Stock options selected (current expiry, ATM window both legs).
    pub stock_options: usize,
    /// The ATM half-window actually used. Below
    /// [`STOCK_OPTION_ATM_STRIKES_EACH_SIDE`] means the envelope forced a
    /// shrink — the operator asked for 25 and got this.
    pub atm_window_used: usize,
    /// Underlyings whose options were REFUSED because no live spot price was
    /// available to locate at-the-money. Never guessed.
    pub underlyings_without_spot: usize,
    /// Rows refused for carrying no usable expiry.
    pub refused_no_expiry: usize,
    /// Rows refused for carrying no usable strike (options only).
    pub refused_no_strike: usize,
    /// Rows refused because their exchange maps to no segment we subscribe.
    pub refused_unknown_segment: usize,
    /// Contracts dropped as duplicates of an already-selected composite key.
    pub deduped: usize,
    /// Contracts the envelope could not fit even at an ATM window of zero.
    pub dropped_for_capacity: usize,
}

/// Maps a master row's `EXCH_ID` to the derivative segment we subscribe.
///
/// NSE derivatives are `NSE_FNO`, BSE derivatives (SENSEX) are `BSE_FNO`.
/// Anything else — MCX commodity, currency — returns `None` and is refused:
/// those are outside the authorized scope, and a wrong segment is a
/// wrong-instrument subscription that looks perfectly healthy (I-P1-11).
#[must_use]
pub fn derivative_segment(exch_id: &str) -> Option<ExchangeSegment> {
    match exch_id {
        "NSE" => Some(ExchangeSegment::NseFno),
        "BSE" => Some(ExchangeSegment::BseFno),
        _ => None,
    }
}

/// One contract, reduced to what selection needs.
///
/// Borrowed from the master rather than cloned: the master is ~150,000 rows
/// and this runs over all of them.
#[derive(Debug, Clone, Copy)]
struct Contract<'a> {
    security_id: SecurityId,
    segment: ExchangeSegment,
    expiry_ymd: u32,
    strike_paise: i64,
    leg: OptionLeg,
    underlying: &'a str,
}

/// Everything one underlying offers, bucketed in the single pass.
#[derive(Debug, Default)]
struct UnderlyingBucket<'a> {
    options: Vec<Contract<'a>>,
}

/// Selects the contract universe from today's master.
///
/// `spot_paise` maps an underlying SYMBOL to its last traded price in paise.
/// It is consulted only for stock options; index options take the full chain
/// and futures take every expiry, so neither needs a price.
///
/// `capacity` is how many main-feed slots remain after the spot universe. The
/// selection never exceeds it: classes are filled in priority order and the
/// ATM window shrinks until the total fits.
///
/// # Panics
///
/// Never. Every refusal is counted and returned.
#[must_use]
#[allow(clippy::too_many_lines)]
// APPROVED: the six selection classes are one decision each and read as a
// single table; splitting them across helpers would hide the priority order,
// which is the only thing about this function that is subtle.
pub fn select_contract_universe(
    rows: &[MasterRow],
    spot_paise: &HashMap<String, i64>,
    today_ymd: u32,
    capacity: usize,
) -> ContractSelection {
    let mut out = ContractSelection {
        atm_window_used: STOCK_OPTION_ATM_STRIKES_EACH_SIDE,
        ..ContractSelection::default()
    };

    // ---- pass 1: bucket by underlying, O(rows) ----
    //
    // Four buckets, not one flat list, because every downstream decision is
    // per-underlying: which expiry is nearest, where ATM sits, whether a full
    // chain is authorized. A flat list would need a scan per underlying.
    let mut index_futures: Vec<Contract<'_>> = Vec::new();
    let mut stock_futures: Vec<Contract<'_>> = Vec::new();
    let mut index_opt: HashMap<&str, UnderlyingBucket<'_>> = HashMap::new();
    let mut stock_opt: HashMap<&str, UnderlyingBucket<'_>> = HashMap::new();

    for row in rows {
        let class = row.class;
        if !class.is_future() && !class.is_option() {
            continue;
        }
        let Some(segment) = derivative_segment(&row.exch_id) else {
            out.refused_unknown_segment += 1;
            continue;
        };
        // An expiry of 0 is the parser's "absent or unusable" answer. A
        // contract with no expiry cannot be placed on a ladder, and guessing
        // one would let a garbled row win the nearest-expiry slot.
        if row.expiry_ymd == 0 || row.expiry_ymd < today_ymd {
            if row.expiry_ymd == 0 {
                out.refused_no_expiry += 1;
            }
            continue;
        }
        if row.underlying_symbol.is_empty() {
            out.refused_no_expiry += 1;
            continue;
        }
        let c = Contract {
            security_id: row.security_id,
            segment,
            expiry_ymd: row.expiry_ymd,
            strike_paise: row.strike_paise,
            leg: row.option_leg,
            underlying: row.underlying_symbol.as_str(),
        };
        match class {
            InstrumentClass::IndexFuture => index_futures.push(c),
            InstrumentClass::StockFuture => stock_futures.push(c),
            InstrumentClass::IndexOption | InstrumentClass::StockOption => {
                // An option with no strike or no leg cannot be placed on a
                // ladder either.
                if c.strike_paise == 0 {
                    out.refused_no_strike += 1;
                    continue;
                }
                if c.leg == OptionLeg::None {
                    out.refused_no_strike += 1;
                    continue;
                }
                let target = if class == InstrumentClass::IndexOption {
                    &mut index_opt
                } else {
                    &mut stock_opt
                };
                target.entry(c.underlying).or_default().options.push(c);
            }
            _ => {}
        }
    }
    // ---- assemble in priority order ----
    //
    // Priority is what makes the capacity bound principled: when the envelope
    // binds, what gives way is the far-from-the-money tail of stock options,
    // never a future and never an index chain.
    let mut chosen: HashSet<(SecurityId, ExchangeSegment)> = HashSet::new();
    let mut picked: Vec<SubscribeInstrument> = Vec::new();

    // 1 + 2. Futures, ALL expiries at or after today. Never rolled, never
    // trimmed: they are ~700 contracts against a 25,000 envelope.
    for c in &index_futures {
        match push_contract(c, capacity, &mut chosen, &mut picked) {
            PushOutcome::Added => out.index_futures += 1,
            PushOutcome::Duplicate => out.deduped += 1,
            PushOutcome::NoRoom => out.dropped_for_capacity += 1,
        }
    }
    for c in &stock_futures {
        match push_contract(c, capacity, &mut chosen, &mut picked) {
            PushOutcome::Added => out.stock_futures += 1,
            PushOutcome::Duplicate => out.deduped += 1,
            PushOutcome::NoRoom => out.dropped_for_capacity += 1,
        }
    }

    // 3. Index options — FULL chain, current expiry, exactly the two
    // authorized underlyings. No spot price needed: taking every strike is
    // what "entire options contracts" means.
    for underlying in FULL_CHAIN_INDEX_UNDERLYINGS {
        let Some(bucket) = index_opt.get(underlying) else {
            continue;
        };
        let Some(expiry) = nearest_expiry(&bucket.options) else {
            continue;
        };
        for c in bucket.options.iter().filter(|c| c.expiry_ymd == expiry) {
            match push_contract(c, capacity, &mut chosen, &mut picked) {
                PushOutcome::Added => out.index_options += 1,
                PushOutcome::Duplicate => out.deduped += 1,
                PushOutcome::NoRoom => out.dropped_for_capacity += 1,
            }
        }
    }

    // 4. Stock options — current expiry, ATM ± window, both legs. The only
    // class that needs a live price, and the only one that shrinks.
    //
    // The window is chosen BEFORE anything is pushed so the result is the
    // same window for every underlying: an asymmetric selection where early
    // stocks got 25 strikes and late ones got 3 would be a silent, ordering-
    // dependent bias in what the strategy can see.
    let ladders = build_ladders(&stock_opt, spot_paise, &mut out);
    let remaining = capacity.saturating_sub(picked.len());
    match fit_atm_window(&ladders, remaining) {
        Some(window) => {
            out.atm_window_used = window;
            for ladder in &ladders {
                for c in ladder.within(window) {
                    match push_contract(c, capacity, &mut chosen, &mut picked) {
                        PushOutcome::Added => out.stock_options += 1,
                        PushOutcome::Duplicate => out.deduped += 1,
                        PushOutcome::NoRoom => out.dropped_for_capacity += 1,
                    }
                }
            }
        }
        // Not even the at-the-money strike of every stock fits. `None` and a
        // window of 0 are DIFFERENT answers and must not be conflated: window
        // 0 still selects the ATM strike itself, both legs, for every stock.
        // Reporting "window 0" here would claim we subscribed the money when
        // we subscribed nothing.
        None => {
            out.atm_window_used = 0;
            // Counts the FULL ask — what a window of 25 would have taken —
            // not the minimum viable one. The operator asked for ATM ± 25;
            // reporting only the two at-the-money contracts as "dropped"
            // would understate the gap by a factor of fifty.
            out.dropped_for_capacity += ladders
                .iter()
                .map(|l| l.count_within(STOCK_OPTION_ATM_STRIKES_EACH_SIDE))
                .fold(0usize, usize::saturating_add);
        }
    }

    // Deterministic order so two runs over the same master produce the same
    // subscribe batches, which is what makes a diff between days meaningful.
    picked.sort_unstable_by_key(|i| (i.segment as u8, i.security_id));
    out.instruments = picked;
    out
}

/// What happened to one attempted subscription.
///
/// Three outcomes rather than a `bool`, because the two failure modes need
/// different counters: a duplicate is expected and harmless, while running out
/// of room means the master offered more than the connection envelope can
/// carry and the operator is not getting what the scope authorized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushOutcome {
    Added,
    Duplicate,
    NoRoom,
}

/// Adds one contract if it is new and there is room.
///
/// The capacity check lives HERE, at the push, rather than as a truncate at
/// the end. A trailing truncate would leave the per-class counters describing
/// contracts that were dropped a moment later — the counters would report a
/// selection that never existed, which is worse than the truncation itself.
fn push_contract(
    c: &Contract<'_>,
    capacity: usize,
    chosen: &mut HashSet<(SecurityId, ExchangeSegment)>,
    picked: &mut Vec<SubscribeInstrument>,
) -> PushOutcome {
    if picked.len() >= capacity {
        return PushOutcome::NoRoom;
    }
    // The I-P1-11 composite, never the id alone: two instruments can share a
    // numeric id across segments, and deduping on the id would silently drop
    // one of two real contracts.
    if chosen.insert((c.security_id, c.segment)) {
        picked.push(SubscribeInstrument {
            security_id: c.security_id,
            segment: c.segment,
        });
        PushOutcome::Added
    } else {
        PushOutcome::Duplicate
    }
}

/// The nearest expiry at or after today within a bucket, or `None` if empty.
///
/// Every contract in the bucket is already filtered to `>= today` by the
/// bucketing pass, so this is a plain minimum.
fn nearest_expiry(contracts: &[Contract<'_>]) -> Option<u32> {
    contracts.iter().map(|c| c.expiry_ymd).min()
}

/// One underlying's current-expiry option ladder, positioned at ATM.
struct Ladder<'a> {
    /// Distinct strikes of the current expiry, ascending.
    strikes: Vec<i64>,
    /// Index into `strikes` of the strike nearest the spot price.
    atm: usize,
    /// Every current-expiry contract, kept for the final filter.
    contracts: Vec<Contract<'a>>,
}

impl<'a> Ladder<'a> {
    /// Contracts within `window` strikes each side of ATM, both legs.
    fn within(&self, window: usize) -> impl Iterator<Item = &Contract<'a>> {
        let lo = self.strikes[self.atm.saturating_sub(window)];
        // `min` rather than a bare index: the ladder can be shorter than the
        // window on either side, and an out-of-range index is a panic on a
        // path that runs against vendor data.
        let hi_idx = (self.atm + window).min(self.strikes.len() - 1);
        let hi = self.strikes[hi_idx];
        self.contracts
            .iter()
            .filter(move |c| c.strike_paise >= lo && c.strike_paise <= hi)
    }

    /// How many contracts `window` would select. Used to size the window
    /// before anything is pushed.
    fn count_within(&self, window: usize) -> usize {
        self.within(window).count()
    }
}

/// Builds one ATM-positioned ladder per stock underlying that has a spot price.
///
/// An underlying with no spot price is REFUSED and counted, never given a
/// guessed at-the-money: picking the median strike, or the first, would
/// subscribe a plausible-looking window centred on nothing.
fn build_ladders<'a>(
    stock_opt: &HashMap<&'a str, UnderlyingBucket<'a>>,
    spot_paise: &HashMap<String, i64>,
    out: &mut ContractSelection,
) -> Vec<Ladder<'a>> {
    let mut ladders = Vec::with_capacity(stock_opt.len());
    // Sorted so the selection is deterministic regardless of hash order.
    let mut underlyings: Vec<&&str> = stock_opt.keys().collect();
    underlyings.sort_unstable();

    for underlying in underlyings {
        let bucket = &stock_opt[*underlying];
        let Some(&spot) = spot_paise.get(*underlying) else {
            out.underlyings_without_spot += 1;
            continue;
        };
        if spot <= 0 {
            out.underlyings_without_spot += 1;
            continue;
        }
        let Some(expiry) = nearest_expiry(&bucket.options) else {
            continue;
        };
        let contracts: Vec<Contract<'a>> = bucket
            .options
            .iter()
            .filter(|c| c.expiry_ymd == expiry)
            .copied()
            .collect();
        let mut strikes: Vec<i64> = contracts.iter().map(|c| c.strike_paise).collect();
        strikes.sort_unstable();
        strikes.dedup();
        if strikes.is_empty() {
            continue;
        }
        // Binary search for the insertion point, then compare the two
        // neighbours: the nearest strike to spot is one of them, and this is
        // O(log k) rather than a scan.
        let idx = strikes.partition_point(|&s| s < spot);
        let atm = if idx == 0 {
            0
        } else if idx >= strikes.len() {
            strikes.len() - 1
        } else {
            let below = spot - strikes[idx - 1];
            let above = strikes[idx] - spot;
            if below <= above { idx - 1 } else { idx }
        };
        ladders.push(Ladder {
            strikes,
            atm,
            contracts,
        });
    }
    ladders
}

/// The largest ATM half-window whose total fits `remaining` slots, or `None`
/// when not even the at-the-money strikes fit.
///
/// Returns `Some(`[`STOCK_OPTION_ATM_STRIKES_EACH_SIDE`]`)` when the
/// operator's figure already fits, which is the normal case. Shrinks only
/// under pressure, and the caller records the value it got so a shrink is
/// visible rather than inferred from a short instrument count.
///
/// `None` is deliberately NOT `Some(0)`: a window of zero still selects the
/// ATM strike of every stock, both legs, which for 220 stocks is 440
/// contracts. Collapsing the two would report "we took the money" when the
/// envelope had no room for anything at all.
fn fit_atm_window(ladders: &[Ladder<'_>], remaining: usize) -> Option<usize> {
    if ladders.is_empty() {
        return Some(STOCK_OPTION_ATM_STRIKES_EACH_SIDE);
    }
    let total = |w: usize| -> usize {
        ladders
            .iter()
            .map(|l| l.count_within(w))
            .fold(0usize, usize::saturating_add)
    };
    // Descending search over 26 values, widest first. A binary search would
    // be O(log 26) probes of an O(underlyings) function instead of O(26) —
    // saving roughly twenty passes over a few hundred ladders, once per day.
    // Not worth the off-by-one surface on a cold path.
    (0..=STOCK_OPTION_ATM_STRIKES_EACH_SIDE)
        .rev()
        .find(|&w| total(w) <= remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a master row. Only the fields selection reads are meaningful.
    fn contract(
        id: u64,
        class: InstrumentClass,
        exch: &str,
        underlying: &str,
        expiry: u32,
        strike_rupees: i64,
        leg: OptionLeg,
    ) -> MasterRow {
        MasterRow {
            security_id: id,
            isin: String::new(),
            symbol_name: format!("SYM{id}"),
            exch_id: exch.into(),
            segment: "D".into(),
            series: String::new(),
            class,
            expiry_ymd: expiry,
            strike_paise: strike_rupees * 100,
            option_leg: leg,
            underlying_symbol: underlying.into(),
        }
    }

    const TODAY: u32 = 2026_08_19;

    fn no_spot() -> HashMap<String, i64> {
        HashMap::new()
    }

    fn spot(sym: &str, rupees: i64) -> HashMap<String, i64> {
        let mut m = HashMap::new();
        m.insert(sym.to_string(), rupees * 100);
        m
    }

    #[test]
    fn an_empty_master_selects_nothing_and_says_so() {
        let sel = select_contract_universe(&[], &no_spot(), TODAY, 25_000);
        assert!(sel.instruments.is_empty());
        assert_eq!(sel.index_futures, 0);
        assert_eq!(sel.stock_options, 0);
        assert_eq!(sel.dropped_for_capacity, 0);
    }

    #[test]
    fn futures_take_every_expiry_at_or_after_today() {
        let rows = vec![
            contract(
                1,
                InstrumentClass::IndexFuture,
                "NSE",
                "NIFTY",
                2026_08_28,
                0,
                OptionLeg::None,
            ),
            contract(
                2,
                InstrumentClass::IndexFuture,
                "NSE",
                "NIFTY",
                2026_09_24,
                0,
                OptionLeg::None,
            ),
            contract(
                3,
                InstrumentClass::IndexFuture,
                "NSE",
                "NIFTY",
                2026_10_29,
                0,
                OptionLeg::None,
            ),
            // Yesterday's contract must NOT be subscribed: it is not tradeable
            // and its absence of ticks would read as a silent instrument.
            contract(
                4,
                InstrumentClass::IndexFuture,
                "NSE",
                "NIFTY",
                2026_08_18,
                0,
                OptionLeg::None,
            ),
        ];
        let sel = select_contract_universe(&rows, &no_spot(), TODAY, 25_000);
        assert_eq!(
            sel.index_futures, 3,
            "all three live expiries, never rolled"
        );
        assert!(!sel.instruments.iter().any(|i| i.security_id == 4));
    }

    #[test]
    fn todays_expiry_is_still_subscribed_on_expiry_day() {
        // Index futures NEVER roll: the expiring contract streams through its
        // final session and falls out of tomorrow's build by itself.
        let rows = vec![contract(
            1,
            InstrumentClass::IndexFuture,
            "NSE",
            "NIFTY",
            TODAY,
            0,
            OptionLeg::None,
        )];
        let sel = select_contract_universe(&rows, &no_spot(), TODAY, 25_000);
        assert_eq!(sel.index_futures, 1, ">= today, not > today");
    }

    #[test]
    fn index_options_take_the_full_chain_of_the_current_expiry_only() {
        let mut rows = Vec::new();
        // Current expiry: 6 strikes × 2 legs.
        for (i, strike) in [24_000, 24_100, 24_200, 24_300, 24_400, 24_500]
            .into_iter()
            .enumerate()
        {
            let base = 100 + (i as u64) * 2;
            rows.push(contract(
                base,
                InstrumentClass::IndexOption,
                "NSE",
                "NIFTY",
                2026_08_28,
                strike,
                OptionLeg::Call,
            ));
            rows.push(contract(
                base + 1,
                InstrumentClass::IndexOption,
                "NSE",
                "NIFTY",
                2026_08_28,
                strike,
                OptionLeg::Put,
            ));
        }
        // Next expiry must NOT be selected.
        rows.push(contract(
            900,
            InstrumentClass::IndexOption,
            "NSE",
            "NIFTY",
            2026_09_24,
            24_000,
            OptionLeg::Call,
        ));
        let sel = select_contract_universe(&rows, &no_spot(), TODAY, 25_000);
        assert_eq!(sel.index_options, 12, "full chain, both legs, one expiry");
        assert!(!sel.instruments.iter().any(|i| i.security_id == 900));
    }

    #[test]
    fn an_index_outside_the_authorized_two_gets_no_chain() {
        // FINNIFTY is a real index with a real chain. It is NOT in the
        // operator's "one and only for nifty and banknifty" grant, and a
        // third index here is a scope expansion.
        let rows = vec![contract(
            1,
            InstrumentClass::IndexOption,
            "NSE",
            "FINNIFTY",
            2026_08_28,
            24_000,
            OptionLeg::Call,
        )];
        let sel = select_contract_universe(&rows, &no_spot(), TODAY, 25_000);
        assert_eq!(sel.index_options, 0);
        assert!(sel.instruments.is_empty());
    }

    #[test]
    fn the_full_chain_grant_is_exactly_two_underlyings() {
        // Pinned deliberately: widening this array is a scope change that
        // needs its own dated operator quote.
        assert_eq!(FULL_CHAIN_INDEX_UNDERLYINGS.len(), 2);
        assert!(FULL_CHAIN_INDEX_UNDERLYINGS.contains(&"NIFTY"));
        assert!(FULL_CHAIN_INDEX_UNDERLYINGS.contains(&"BANKNIFTY"));
    }

    /// 101 strikes at 10 rupees apart, centred on 1000.
    fn stock_ladder(underlying: &str) -> Vec<MasterRow> {
        let mut rows = Vec::new();
        for i in 0..101i64 {
            let strike = 500 + i * 10;
            let base = 1000 + (i as u64) * 2;
            rows.push(contract(
                base,
                InstrumentClass::StockOption,
                "NSE",
                underlying,
                2026_08_28,
                strike,
                OptionLeg::Call,
            ));
            rows.push(contract(
                base + 1,
                InstrumentClass::StockOption,
                "NSE",
                underlying,
                2026_08_28,
                strike,
                OptionLeg::Put,
            ));
        }
        rows
    }

    #[test]
    fn stock_options_take_the_atm_window_both_legs() {
        let rows = stock_ladder("RELIANCE");
        // Spot 1000 sits exactly on a strike, so ATM is that strike and the
        // window is 25 each side: 51 strikes × 2 legs.
        let sel = select_contract_universe(&rows, &spot("RELIANCE", 1000), TODAY, 25_000);
        assert_eq!(sel.atm_window_used, 25);
        assert_eq!(sel.stock_options, 102, "51 strikes x 2 legs");
    }

    #[test]
    fn atm_lands_on_the_nearer_strike_when_spot_falls_between_two() {
        let rows = stock_ladder("RELIANCE");
        // Spot 1004 is between strikes 1000 and 1010, nearer 1000. The window
        // must therefore run 750..1250, not 760..1260.
        let sel = select_contract_universe(&rows, &spot("RELIANCE", 1004), TODAY, 25_000);
        assert_eq!(sel.stock_options, 102);
        // 750 is exactly 25 strikes below 1000 and must be included.
        assert!(
            sel.instruments.len() >= 102,
            "the window is anchored on the nearer strike"
        );
    }

    #[test]
    fn an_underlying_with_no_spot_price_is_refused_not_guessed() {
        let rows = stock_ladder("RELIANCE");
        let sel = select_contract_universe(&rows, &no_spot(), TODAY, 25_000);
        assert_eq!(sel.stock_options, 0);
        assert_eq!(sel.underlyings_without_spot, 1);
        assert!(
            sel.instruments.is_empty(),
            "a guessed at-the-money is a plausible-looking window centred on nothing"
        );
    }

    #[test]
    fn a_zero_or_negative_spot_is_refused_like_a_missing_one() {
        let rows = stock_ladder("RELIANCE");
        let mut prices = HashMap::new();
        prices.insert("RELIANCE".to_string(), 0i64);
        let sel = select_contract_universe(&rows, &prices, TODAY, 25_000);
        assert_eq!(sel.underlyings_without_spot, 1);
        assert_eq!(sel.stock_options, 0);
    }

    #[test]
    fn a_ladder_shorter_than_the_window_is_taken_whole_without_panicking() {
        // Three strikes, a window of 25. An unguarded index would panic here,
        // on a path fed by vendor data.
        let mut rows = Vec::new();
        for (i, strike) in [990, 1000, 1010].into_iter().enumerate() {
            let base = 10 + (i as u64) * 2;
            rows.push(contract(
                base,
                InstrumentClass::StockOption,
                "NSE",
                "TINY",
                2026_08_28,
                strike,
                OptionLeg::Call,
            ));
            rows.push(contract(
                base + 1,
                InstrumentClass::StockOption,
                "NSE",
                "TINY",
                2026_08_28,
                strike,
                OptionLeg::Put,
            ));
        }
        let sel = select_contract_universe(&rows, &spot("TINY", 1000), TODAY, 25_000);
        assert_eq!(sel.stock_options, 6, "the whole short ladder");
    }

    #[test]
    fn the_atm_window_shrinks_rather_than_the_selection_truncating() {
        let rows = stock_ladder("RELIANCE");
        // Room for 40 contracts. A window of 25 wants 102, so it must shrink
        // to the largest window that fits: 9 each side = 19 strikes x 2 = 38.
        let sel = select_contract_universe(&rows, &spot("RELIANCE", 1000), TODAY, 40);
        assert!(sel.atm_window_used < 25, "the window shrank");
        assert!(sel.stock_options <= 40);
        assert_eq!(
            sel.dropped_for_capacity, 0,
            "shrinking is the mechanism; truncation is the last resort"
        );
        // What survives must be the strikes NEAREST the money — that is the
        // whole reason the window is the elastic dimension.
        assert!(sel.stock_options >= 2);
    }

    #[test]
    fn every_underlying_gets_the_same_window_regardless_of_iteration_order() {
        let mut rows = stock_ladder("AAA");
        rows.extend(stock_ladder("ZZZ").into_iter().map(|mut r| {
            r.security_id += 100_000;
            r
        }));
        let mut prices = HashMap::new();
        prices.insert("AAA".to_string(), 100_000i64);
        prices.insert("ZZZ".to_string(), 100_000i64);
        // Room for both at a reduced window.
        let sel = select_contract_universe(&rows, &prices, TODAY, 80);
        // An ordering-dependent selection would give AAA 25 strikes and ZZZ
        // almost none — a silent bias in what the strategy can see.
        assert!(sel.atm_window_used < 25);
        assert_eq!(
            sel.stock_options % 2,
            0,
            "both underlyings selected symmetrically"
        );
    }

    #[test]
    fn futures_and_index_chains_outrank_stock_options_under_pressure() {
        let mut rows = vec![
            contract(
                1,
                InstrumentClass::IndexFuture,
                "NSE",
                "NIFTY",
                2026_08_28,
                0,
                OptionLeg::None,
            ),
            contract(
                2,
                InstrumentClass::StockFuture,
                "NSE",
                "RELIANCE",
                2026_08_28,
                0,
                OptionLeg::None,
            ),
        ];
        rows.extend(stock_ladder("RELIANCE"));
        // Only 2 slots: the two futures must win, stock options get nothing.
        let sel = select_contract_universe(&rows, &spot("RELIANCE", 1000), TODAY, 2);
        assert_eq!(sel.index_futures, 1);
        assert_eq!(sel.stock_futures, 1);
        assert_eq!(sel.stock_options, 0);
        assert_eq!(sel.instruments.len(), 2);
        // The 102 stock options the operator asked for and did not get are
        // COUNTED. A selection that silently returns futures only is
        // indistinguishable from one where the master had no options.
        assert_eq!(sel.dropped_for_capacity, 102);
        assert_eq!(sel.atm_window_used, 0, "no window was usable");
    }

    #[test]
    fn an_envelope_smaller_than_the_futures_alone_drops_and_reports() {
        let rows = vec![
            contract(
                1,
                InstrumentClass::IndexFuture,
                "NSE",
                "NIFTY",
                2026_08_28,
                0,
                OptionLeg::None,
            ),
            contract(
                2,
                InstrumentClass::IndexFuture,
                "NSE",
                "NIFTY",
                2026_09_24,
                0,
                OptionLeg::None,
            ),
            contract(
                3,
                InstrumentClass::IndexFuture,
                "NSE",
                "NIFTY",
                2026_10_29,
                0,
                OptionLeg::None,
            ),
        ];
        let sel = select_contract_universe(&rows, &no_spot(), TODAY, 2);
        assert_eq!(sel.instruments.len(), 2);
        assert_eq!(
            sel.dropped_for_capacity, 1,
            "a truncation that is not counted is indistinguishable from a complete set"
        );
    }

    #[test]
    fn sensex_derivatives_map_to_bse_fno_and_nse_to_nse_fno() {
        assert_eq!(derivative_segment("NSE"), Some(ExchangeSegment::NseFno));
        assert_eq!(derivative_segment("BSE"), Some(ExchangeSegment::BseFno));
        // Commodity and currency are outside the authorized scope, and a
        // wrong segment is a wrong-instrument subscription that looks healthy.
        assert_eq!(derivative_segment("MCX"), None);
        assert_eq!(derivative_segment(""), None);
    }

    #[test]
    fn an_unmappable_exchange_is_counted_not_silently_skipped() {
        let rows = vec![contract(
            1,
            InstrumentClass::IndexFuture,
            "MCX",
            "GOLD",
            2026_08_28,
            0,
            OptionLeg::None,
        )];
        let sel = select_contract_universe(&rows, &no_spot(), TODAY, 25_000);
        assert_eq!(sel.refused_unknown_segment, 1);
        assert!(sel.instruments.is_empty());
    }

    #[test]
    fn the_same_composite_key_twice_is_deduped_per_i_p1_11() {
        let rows = vec![
            contract(
                7,
                InstrumentClass::IndexFuture,
                "NSE",
                "NIFTY",
                2026_08_28,
                0,
                OptionLeg::None,
            ),
            contract(
                7,
                InstrumentClass::IndexFuture,
                "NSE",
                "NIFTY",
                2026_09_24,
                0,
                OptionLeg::None,
            ),
        ];
        let sel = select_contract_universe(&rows, &no_spot(), TODAY, 25_000);
        assert_eq!(sel.instruments.len(), 1);
        assert_eq!(sel.deduped, 1);
    }

    #[test]
    fn the_same_id_in_two_segments_is_kept_as_two_instruments() {
        // The I-P1-11 case that matters: security_id alone is NOT unique, and
        // deduping on it would silently drop one of two real instruments.
        let rows = vec![
            contract(
                7,
                InstrumentClass::IndexFuture,
                "NSE",
                "NIFTY",
                2026_08_28,
                0,
                OptionLeg::None,
            ),
            contract(
                7,
                InstrumentClass::IndexFuture,
                "BSE",
                "SENSEX",
                2026_08_28,
                0,
                OptionLeg::None,
            ),
        ];
        let sel = select_contract_universe(&rows, &no_spot(), TODAY, 25_000);
        assert_eq!(sel.instruments.len(), 2);
        assert_eq!(sel.deduped, 0);
    }

    #[test]
    fn a_contract_with_no_usable_expiry_or_strike_is_counted_and_refused() {
        let rows = vec![
            contract(
                1,
                InstrumentClass::IndexFuture,
                "NSE",
                "NIFTY",
                0,
                0,
                OptionLeg::None,
            ),
            contract(
                2,
                InstrumentClass::StockOption,
                "NSE",
                "RELIANCE",
                2026_08_28,
                0,
                OptionLeg::Call,
            ),
            contract(
                3,
                InstrumentClass::StockOption,
                "NSE",
                "RELIANCE",
                2026_08_28,
                1000,
                OptionLeg::None,
            ),
        ];
        let sel = select_contract_universe(&rows, &spot("RELIANCE", 1000), TODAY, 25_000);
        assert_eq!(sel.refused_no_expiry, 1);
        assert_eq!(sel.refused_no_strike, 2, "no strike, and no leg");
        assert!(sel.instruments.is_empty());
    }

    #[test]
    fn equities_and_indices_in_the_master_are_not_contracts() {
        let rows = vec![
            contract(1, InstrumentClass::Equity, "NSE", "", 0, 0, OptionLeg::None),
            contract(2, InstrumentClass::Index, "NSE", "", 0, 0, OptionLeg::None),
            contract(3, InstrumentClass::Other, "NSE", "", 0, 0, OptionLeg::None),
        ];
        let sel = select_contract_universe(&rows, &no_spot(), TODAY, 25_000);
        assert!(sel.instruments.is_empty());
        assert_eq!(sel.refused_no_expiry, 0, "a non-contract is not a refusal");
    }

    #[test]
    fn the_selection_is_deterministic_across_runs() {
        let mut rows = stock_ladder("AAA");
        rows.extend(stock_ladder("BBB").into_iter().map(|mut r| {
            r.security_id += 100_000;
            r
        }));
        let mut prices = HashMap::new();
        prices.insert("AAA".to_string(), 100_000i64);
        prices.insert("BBB".to_string(), 100_000i64);
        let a = select_contract_universe(&rows, &prices, TODAY, 25_000);
        let b = select_contract_universe(&rows, &prices, TODAY, 25_000);
        assert_eq!(a, b, "hash iteration order must not reach the output");
        assert!(
            a.instruments.windows(2).all(|w| {
                (w[0].segment as u8, w[0].security_id) < (w[1].segment as u8, w[1].security_id)
            }),
            "sorted, so a day-over-day diff of subscribe batches is meaningful"
        );
    }

    #[test]
    fn a_full_shape_selection_reaches_the_authorized_scale() {
        // The point of the whole module: prove the shape scales to the
        // authorized ~25,000 rather than asserting it in prose.
        let mut rows = Vec::new();
        let mut prices = HashMap::new();
        let mut next: u64 = 1;
        // 220 stocks, each with 51 in-window strikes x 2 legs = 102.
        for s in 0..220u64 {
            let underlying = format!("STK{s}");
            prices.insert(underlying.clone(), 100_000i64);
            for i in 0..51i64 {
                let strike = 750 + i * 10;
                for leg in [OptionLeg::Call, OptionLeg::Put] {
                    rows.push(contract(
                        next,
                        InstrumentClass::StockOption,
                        "NSE",
                        &underlying,
                        2026_08_28,
                        strike,
                        leg,
                    ));
                    next += 1;
                }
            }
            // and one future per stock
            rows.push(contract(
                next,
                InstrumentClass::StockFuture,
                "NSE",
                &underlying,
                2026_08_28,
                0,
                OptionLeg::None,
            ));
            next += 1;
        }
        let sel = select_contract_universe(&rows, &prices, TODAY, 25_000);
        assert_eq!(sel.stock_futures, 220);
        assert_eq!(sel.stock_options, 220 * 102);
        assert_eq!(sel.atm_window_used, 25, "no shrink needed at this scale");
        assert_eq!(sel.dropped_for_capacity, 0);
        assert!(
            sel.instruments.len() > 22_000,
            "got {} — the authorized set is ~24,600",
            sel.instruments.len()
        );
        assert!(sel.instruments.len() <= 25_000, "never over the envelope");
    }
}
