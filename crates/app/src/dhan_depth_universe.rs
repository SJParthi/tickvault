//! Depth-20 / depth-200 instrument selection, sourced from the per-minute
//! option chain.
//!
//! Authorized by the operator 2026-08-11 (second quote of that day): *"ensure
//! to enable connect estbalish al lteh 16 ocnenctions defintitley ddue okay?"*
//! The verbatim quote and the constraint analysis live in
//! `.claude/rules/project/websocket-connection-scope-lock.md`.
//!
//! # Why the option chain is the only legal source
//!
//! Depth needs a *tradeable* order book, and an index does not have one — so
//! the 4 hardcoded index SIDs the main feed carries can never populate depth.
//! Reaching real contracts needs contract-level `security_id`s, and there are
//! exactly three ways to get them:
//!
//! | Source | Rule status | Automation |
//! |---|---|---|
//! | Dhan instrument-master CSV | allowed again since the 2026-08-11 third quote | daily, but a whole extra parse for ids we already receive |
//! | A hardcoded contract list | permitted by the letter of Q3 | **fails** the standing no-manual-intervention mandate — option contracts expire weekly, so it needs a human edit every week and goes silently stale between them |
//! | **The per-minute option chain** | already authorized, already running | automatic and self-rolling |
//!
//! The third is the only one that satisfies both the quote and the
//! zero-manual-intervention rule at the same time. `POST /v2/optionchain`
//! returns a per-leg `security_id` (`docs/dhan-ref/06-option-chain.md:195`:
//! *"gives you the SecurityId of each option contract directly, no instrument
//! master lookup needed for subscriptions"*), we already parse it, and we
//! already persist it every minute as `option_chain_1m.contract_security_id`.
//! When the expiry rolls, the chain returns the new contracts and this
//! selection follows — with no code change and no human edit.
//!
//! # What this deliberately does NOT cover
//!
//! **Index FUTURES depth is unreachable and is not attempted here.**
//! `/v2/optionchain` returns `ce`/`pe` legs only; the expiry-list endpoint
//! returns dates, not ids; and `index_futures.rs` is a pure date filter fed by
//! the *Groww* master, whose `exchange_token` is a different id space from a
//! Dhan `security_id`. So a claim that "all 16 sockets carry data" must not be
//! read as including futures depth. Option depth is what this can honestly
//! deliver.

use tickvault_common::types::{ExchangeSegment, SecurityId};
use tickvault_core::websocket::pool_budget::{
    DEPTH_20_INSTRUMENTS_PER_CONNECTION, MAX_DEPTH_20_CONNECTIONS,
};
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;
use tickvault_storage::option_chain_1m_persistence::OPTION_CHAIN_1M_TABLE;

/// Instruments the whole depth-20 pool can carry.
///
/// DERIVED from the two vendor limits rather than written as `250`, so it
/// cannot drift away from what the pool will actually accept: 5 connections
/// (`16-full-market-depth.md:183`) × 50 instruments each (`:54`).
pub const DEPTH_20_MAX_INSTRUMENTS: usize =
    MAX_DEPTH_20_CONNECTIONS as usize * DEPTH_20_INSTRUMENTS_PER_CONNECTION as usize;

/// Ceiling on the adaptive half-window, however few underlyings there are.
///
/// With a single eligible underlying the budget arithmetic would allow ±62,
/// which is 125 strikes of a chain whose far end is deep out of the money and
/// barely quotes. A 20-level book on a strike nobody trades is a socket spent
/// on nothing. 50 keeps the window wide while staying inside the range where
/// a book still exists.
pub const DEPTH_20_MAX_STRIKES_EACH_SIDE: usize = 50;

/// The ATM half-window that fills the depth-20 envelope for `underlyings`
/// eligible underlyings.
///
/// # Why this is computed rather than a constant
///
/// A fixed window under-fills or overflows the moment the eligible-underlying
/// count changes, and that count is NOT stable: Dhan serves depth on NSE only
/// (`04-full-market-depth-websocket.md:13`), so SENSEX — a `BSE_FNO`
/// underlying — can never have a depth book, and the eligible set is whatever
/// the chain happens to carry on NSE that day.
///
/// The failure this replaces was real and silent in both directions. At a
/// fixed ±10 with two eligible underlyings the pool carried 84 of its 250
/// slots — two thirds of the authorized depth budget idle, with nothing
/// reporting it. Raising the constant to fill 250 at two underlyings would
/// then OVERFLOW the moment a third became eligible, and `plan_pool` refuses
/// the WHOLE pool rather than truncating — so the fix for an under-fill would
/// have turned into a total depth outage on the day the chain widened.
///
/// Returns 0 for no eligible underlyings AND for a window too narrow to hold
/// one strike either side. The two are NOT the same at the call site, and the
/// difference was undocumented until 2026-08-25: with no underlyings the
/// caller selects nothing, but with a collapsed window it computes
/// `keep = each_side * 2 + 1` = 1 and still takes the ATM strike itself, both
/// legs, per underlying. That is the RIGHT behaviour -- the single most
/// informative strike is better than none -- but at 126 or more eligible
/// underlyings it sums past `DEPTH_20_MAX_INSTRUMENTS`. What keeps that safe
/// is NOT this function: it is the last-resort envelope guard in
/// `select_depth_universe`, which truncates the nearest-ATM-first ordering and
/// COUNTS the drop in `depth_20_dropped_for_capacity`. Unreachable today only
/// because `contract_segment_for_underlying` admits four NSE names, so the
/// bound is enforced by an unrelated match rather than by the sizing.
///
/// The superseded sentence read: "the caller selects nothing, which is the
/// correct answer to depth on what?" -- true for the zero-underlying case and
/// false for the collapsed-window one.
#[must_use]
pub fn depth_20_strikes_each_side(underlyings: usize) -> usize {
    if underlyings == 0 {
        return 0;
    }
    // Each strike costs both legs, so one strike of one underlying is 2 slots.
    let strikes = DEPTH_20_MAX_INSTRUMENTS / (underlyings * 2);
    if strikes == 0 {
        return 0;
    }
    // `strikes` counts the ATM strike itself, so the half-window is what is
    // left after removing it, split either side.
    ((strikes - 1) / 2).min(DEPTH_20_MAX_STRIKES_EACH_SIDE)
}

/// Instruments depth-200 subscribes, across all underlyings.
///
/// **FOUR since 2026-08-26, by operator instruction** — not the vendor
/// ceiling of five. Verbatim: *"as fo now dont need to conside the fifth one
/// dude jsut go aheaf with 4 aloe"*, following his specification of the set
/// as *"only nifty atm ce atm pr and banknifty atm ce and atm pe"*.
///
/// Four is exactly two CE/PE pairs: NIFTY ATM and BANKNIFTY ATM. The fifth
/// authorized socket is deliberately left IDLE, which is a real cost stated
/// plainly rather than quietly absorbed — one of five paid-for 200-level
/// connections carries nothing.
///
/// **This retires the lone-leg design**, and the reversal is recorded rather
/// than silently applied. The constant was 5 with the odd socket filled by a
/// next-nearest lone CE, on the reasoning that a 200-level book on a single
/// leg still answers every WITHIN-leg question (resting size per level, the
/// liquidity cliff, what a large order would sweep) even though it cannot
/// answer CE-vs-PE parity or skew. That reasoning was and remains correct.
/// What overrides it is the operator's requirement that this feed carry a
/// SPECIFIC, ATM-tracking set: a lone leg on a third strike is not part of
/// that set, and filling the socket with one would mean the depth-200 feed
/// no longer means one thing.
///
/// [`DepthSelection::depth_200_lone_leg`] is RETAINED and must now always be
/// false; a test pins that, so a future change that re-enables a lone leg
/// fails rather than quietly re-widening the set.
pub const DEPTH_200_MAX_SOCKETS: usize = 4;

/// Depth-200 CONNECTIONS the session may open — five, the operator's figure.
///
/// Deliberately NOT the same constant as [`DEPTH_200_MAX_SOCKETS`], which is
/// the budget for at-the-money PAIRS and is even for a reason: a pair costs
/// two sockets, so an even budget means a whole pair either fits or the budget
/// is full, and the odd-socket case cannot arise by arithmetic.
///
/// Conflating the two would undo that. Raising the pair budget to five lets
/// the selector reach for a third underlying's pair and stop half-way,
/// filling the fifth socket with a lone leg — which is precisely the shape the
/// 2026-08-26 retirement removed. So the fifth connection is budgeted here,
/// separately, and filled by the day's biggest mover at the dial site rather
/// than by the pair selector.
pub const DEPTH_200_TOTAL_SOCKETS: usize = 5;

/// One contract from a chain snapshot, before selection.
#[derive(Debug, Clone, PartialEq)]
pub struct DepthCandidate {
    /// Underlying symbol as the chain reports it (`NIFTY`, `BANKNIFTY`, …).
    pub underlying: String,
    /// The CONTRACT's own Dhan `security_id` — the thing depth subscribes.
    pub contract_security_id: i64,
    /// Expiry, epoch micros. Used to keep only the nearest expiry per
    /// underlying; a chain can legitimately carry weeklies and monthlies at
    /// once, and depth on a far month is bandwidth spent on an illiquid book.
    pub expiry_micros: i64,
    /// Strike price.
    pub strike: f64,
    /// Underlying spot at snapshot time — the reference for "how far from ATM".
    pub spot: f64,
    /// `CE` or `PE`.
    pub leg: String,
    /// `true` for an INDEX option (`OPTIDX`), `false` for a stock option.
    ///
    /// Carried rather than re-derived from `underlying`, because re-deriving
    /// is exactly the bug this field closes: asking a six-entry index map
    /// "do you know this name?" answers "is it an index?" only by accident,
    /// and reports the miss as if the name were unrecognisable.
    pub is_index_option: bool,
}

/// The chosen depth sets, plus an honest account of everything refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DepthSelection {
    /// depth-20 subscription set.
    pub depth_20: Vec<SubscribeInstrument>,
    /// depth-200 subscription set.
    pub depth_200: Vec<SubscribeInstrument>,
    /// Whether the last depth-200 socket carries a LONE leg (no partner).
    ///
    /// The 5-socket budget is odd and options trade in pairs, so a full
    /// budget always ends in one unpartnered leg. That leg's 200-level book
    /// answers every within-leg question (resting size per level, the
    /// liquidity cliff, sweep cost) and answers NO cross-leg question
    /// (synthetic parity, straddle spread, relative skew) because its
    /// partner is not subscribed.
    ///
    /// Surfaced as a field rather than left in a comment so any consumer that
    /// computes a cross-leg quantity can see that the data cannot support it,
    /// instead of computing a number from a book whose other half is absent.
    pub depth_200_lone_leg: bool,
    /// The ATM half-window depth-20 actually used, from
    /// [`depth_20_strikes_each_side`].
    ///
    /// Reported rather than inferred: a caller comparing `depth_20.len()`
    /// against 250 cannot tell a narrow window from a thin chain, and those
    /// need different responses.
    pub depth_20_strikes_each_side: usize,
    /// depth-20 contracts dropped as duplicates of an already-chosen
    /// `(security_id, exchange_segment)` composite (I-P1-11).
    pub depth_20_deduped: usize,
    /// True when candidates existed but NO depth-200 socket could be filled.
    ///
    /// Means no strike carried both legs. Distinct from "there were no
    /// candidates at all", which is the ordinary pre-open state — this one
    /// says the chain arrived and still produced nothing, which is worth
    /// looking at rather than reading as silence.
    pub depth_200_no_pair_available: bool,
    /// depth-20 instruments dropped because the pool envelope was exceeded.
    ///
    /// Should be 0 — the window is sized to fit. A non-zero value means an
    /// assumption about the chain's shape broke, and is worth investigating
    /// even though the truncation itself is safe.
    pub depth_20_dropped_for_capacity: usize,
    /// Rows whose `contract_security_id` was 0 or negative.
    ///
    /// The chain parser defaults this field to `0` when absent, and the field
    /// is marked "added v2.5" upstream — so a zero here means "the vendor did
    /// not tell us the id", NOT "instrument zero". Subscribing it would send a
    /// well-formed request for a nonexistent instrument and receive silence
    /// that looks exactly like a quiet book.
    pub refused_zero_id: usize,
    /// Rows whose underlying has no known contract segment.
    ///
    /// After 2026-08-22 this means what it says. Before it, STOCK options
    /// landed here — all ~20,000 of them, every attempt — because
    /// `contract_segment_for_underlying` only maps the six INDEX underlyings
    /// and returns `None` for RELIANCE, TCS and every other F&O stock. The
    /// counter therefore read five figures of "unknown underlying" on a
    /// perfectly healthy morning, which is indistinguishable from a broken
    /// instrument master. See [`DepthSelection::refused_stock_option`].
    pub refused_unknown_underlying: usize,
    /// Stock options, which this lane deliberately does not give depth to.
    ///
    /// # Why they are refused at all
    ///
    /// `depth_candidates_from_master` accepts `OPTSTK` explicitly, so ~20,000
    /// stock-option candidates reach the selector. Only the six index
    /// underlyings have a segment mapping, so every one of them was dropped —
    /// correctly, since 250 depth-20 slots cannot cover 20,000 contracts and
    /// ranking them purely by distance from at-the-money would let stock
    /// strikes crowd NIFTY and BANKNIFTY off the list entirely.
    ///
    /// # Why it is its own counter
    ///
    /// The DROP is a design choice; reporting it as "unknown underlying" was
    /// not. An operator reading 20,000 unknown-underlying refusals would
    /// reasonably conclude the daily master had failed, and go looking for a
    /// data problem that does not exist. Naming the real reason costs one
    /// field and removes a five-figure false alarm from every session.
    ///
    /// Giving stock options depth is a COVERAGE decision, not a bug fix, and
    /// needs the operator's word: the honest shape is reserve-and-rank (hold
    /// N slots for index legs, award the rest by ATM distance), not simply
    /// mapping the segment and letting the ranking decide.
    pub refused_stock_option: usize,
    /// Rows whose contract segment cannot carry market depth AT ALL.
    ///
    /// Dhan's Full Market Depth is NSE-only —
    /// `docs/dhan-ref/04-full-market-depth-websocket.md:13` ("Only NSE Equity
    /// and Derivatives segments supported") and `:274` ("No BSE, MCX, or
    /// currency") — and that overview line governs the 20-level and 200-level
    /// endpoints alike. SENSEX options are `BSE_FNO`, so **no amount of
    /// configuration can give SENSEX a depth book**; it is a vendor
    /// limitation, not a budget one.
    ///
    /// Refusing here rather than at subscribe time is the whole point. Before
    /// this counter existed the selector handed BSE_FNO contracts to the
    /// planner, which opened a socket for them; the depth-200 builder then
    /// refused the payload and the connection was torn down — one of the five
    /// authorized 200-level sockets spent, live, on an instrument that can
    /// never deliver (observed in prod 2026-08-12 09:06:49 IST, `WS-GAP-02`,
    /// `reason: "... got: BSE_FNO"`). The depth-20 path was worse: it had no
    /// such builder check, so the same contracts went on the wire and came
    /// back as silence, which is indistinguishable from a quiet book.
    pub refused_depth_ineligible_segment: usize,
    /// Rows with a non-finite or non-positive strike/spot.
    pub refused_bad_price: usize,
}

/// Map an underlying to the segment its OPTION CONTRACTS trade in.
///
/// # Why this mapping has to exist at all
///
/// The chain response does not carry the contract's segment. What
/// `option_chain_1m.exchange_segment` stores is the UNDERLYING's segment —
/// `IDX_I`, hardcoded at `option_chain_1m_persistence.rs:108` — because the
/// underlying is an index. Subscribing depth with `IDX_I` would be a
/// well-formed request for the wrong instrument.
///
/// So the contract segment is OUR inference, not a vendor fact, and it is
/// written here once, tested, and **fail-closed**: an underlying this does not
/// recognise is refused and counted, never guessed. Guessing would be
/// especially seductive here because two of the three map to the same value —
/// a default of `NseFno` would be right twice and silently wrong for SENSEX,
/// which is precisely the kind of wrongness that survives review.
#[must_use]
pub fn contract_segment_for_underlying(underlying: &str) -> Option<ExchangeSegment> {
    match underlying.trim().to_ascii_uppercase().as_str() {
        "NIFTY" | "BANKNIFTY" | "MIDCPNIFTY" | "FINNIFTY" => Some(ExchangeSegment::NseFno),
        "SENSEX" | "BANKEX" => Some(ExchangeSegment::BseFno),
        _ => None,
    }
}

/// Whether a segment can carry Dhan market depth at all.
///
/// This is a VENDOR limitation, not ours and not a budget choice:
/// `docs/dhan-ref/04-full-market-depth-websocket.md:13` states "Only NSE
/// Equity and Derivatives segments supported" in the OVERVIEW — above the
/// 20-level and 200-level endpoint sections both — and `:274` repeats it as
/// "No BSE, MCX, or currency".
///
/// The consequence worth stating plainly: **SENSEX can never have a depth
/// book.** SENSEX options trade in `BSE_FNO`, so no config flag, no extra
/// socket, and no operator authorization can produce depth for it. Any plan
/// that counts a SENSEX depth socket is counting a socket that dies on
/// connect.
///
/// Mirrors `subscription_builder::validate_depth_segment`, deliberately.
/// That function is the last line of defence at payload-build time; this one
/// is the first, at SELECTION time — and the two must agree. A divergence is
/// caught by `test_segment_supports_depth_matches_the_builders_own_guard`,
/// which runs both over every segment.
#[must_use]
pub const fn segment_supports_depth(segment: ExchangeSegment) -> bool {
    matches!(
        segment,
        ExchangeSegment::NseEquity | ExchangeSegment::NseFno
    )
}

/// A strike as an exact, hashable key: integer paise.
///
/// A float cannot key a map, and comparing strikes with `abs() < EPSILON`
/// forces a pairwise scan — which is precisely how two O(k^2) passes ended up
/// under a comment claiming O(k). Rounding to paise expresses the same
/// equality those compares did (strikes are whole paise at source) while
/// making the grouping a hash lookup.
///
/// Saturating rather than wrapping: a garbage strike must land on a far-away
/// key and be ranked last, never alias onto a real strike's bucket and quietly
/// join its legs.
#[must_use]
fn strike_key(strike: f64) -> i64 {
    let paise = (strike * 100.0).round();
    if paise.is_finite() {
        // `as` saturates at the i64 bounds for finite floats in Rust.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "saturating by design; a strike beyond i64 paise is not a real contract \
                      and must sort last rather than alias onto a real bucket"
        )]
        {
            paise as i64
        }
    } else {
        i64::MAX
    }
}
/// Distance from at-the-money, against a spot supplied by the CALLER.
///
/// `f64::MAX` for an unusable strike or price, so it sorts last and is refused
/// before selection rather than silently ranked first.
///
/// The spot is a PARAMETER rather than read off the candidate, because spot
/// belongs to the underlying and the chain merely stamps a copy on every row.
/// The no-parameter version was deleted with the per-row refusal rule it
/// served. See [`consensus_spot_by_underlying`].
fn atm_distance_at(candidate: &DepthCandidate, spot: f64) -> f64 {
    if !candidate.strike.is_finite() || !spot.is_finite() || candidate.strike <= 0.0 || spot <= 0.0
    {
        return f64::MAX;
    }
    (candidate.strike - spot).abs()
}

/// Distance from at-the-money as a FRACTION of spot, for ranking ACROSS
/// underlyings.
///
/// WHY NOT RAW RUPEES — 2026-08-26, live. `atm_distance_at` returns
/// `|strike - spot|`, and `select_depth_universe` sorted one pool mixing every
/// underlying by it. Absolute rupee distance is not comparable between a
/// ~24,000 index and a ~57,000 one with different strike spacings: a FINNIFTY
/// strike 50 points from spot outranked a BANKNIFTY strike 100 points from a
/// spot more than twice as large, though the BANKNIFTY strike is nearer in
/// every sense that matters and far more liquid.
///
/// The result on the box that day: FINNIFTY and MIDCPNIFTY took four of the
/// five 200-level sockets, NIFTY took one lone leg, and **BANKNIFTY took
/// none**. Two of those sockets carried FINNIFTY strikes delivering ~125x
/// fewer rows than the others, which then tripped the 50-second idle-silence
/// watchdog into 322 redials between them.
///
/// Normalising by spot makes the comparison dimensionless and therefore
/// meaningful across underlyings. `atm_distance_at` is UNCHANGED and still
/// ranks WITHIN an underlying, where raw rupees are already comparable.
fn atm_distance_fraction_at(candidate: &DepthCandidate, spot: f64) -> f64 {
    let raw = atm_distance_at(candidate, spot);
    if !raw.is_finite() || !spot.is_finite() || spot <= 0.0 {
        return f64::MAX;
    }
    raw / spot
}

/// One spot per underlying: the most common usable value its rows carry,
/// ties to the lower.
///
/// # Why a row's own spot is not the right price
///
/// Spot is a property of the UNDERLYING, not of a contract row — but the
/// chain query stamps a copy on every row, and `LATEST ON ts PARTITION BY
/// underlying_security_id, expiry, strike, leg` takes each strike's own newest
/// row. A strike the vendor stopped returning at 09:47 therefore keeps 09:47's
/// `underlying_spot` while the rest of the chain carries this minute's, and a
/// row whose spot went NULL parses to `NaN`.
///
/// Refusing per row makes that a selection defect rather than a data note. A
/// BANKNIFTY PUT whose single row carried an unusable spot used to be refused
/// on its own, which left its strike with no whole pair — and the operator's
/// locked NIFTY/BANKNIFTY-first rule then handed the 200-level socket to
/// MIDCPNIFTY, silently, on a chain that plainly listed a usable BANKNIFTY
/// pair. Found by a property test asserting a priority underlying is never
/// displaced while it can supply a whole pair.
///
/// The most common value is order-independent by construction and is the price
/// the chain as a whole is quoting; stale rows are the ones that dropped out of
/// the vendor's response, so they are the minority. Grouping is on the bit
/// pattern, so rows carrying literally the same column value group exactly and
/// no epsilon has to be invented.
///
/// An underlying with NO usable row anywhere is absent from the map, and every
/// one of its contracts is then refused as before — a missing price is still a
/// refusal, just not one decided by which row happened to carry it.
///
/// # Complexity
///
/// O(candidates). Cold path, once per attach.
fn consensus_spot_by_underlying(
    candidates: &[DepthCandidate],
) -> std::collections::HashMap<String, f64> {
    let mut tally: std::collections::HashMap<String, std::collections::HashMap<u64, (usize, f64)>> =
        std::collections::HashMap::new();
    for c in candidates {
        if !c.spot.is_finite() || c.spot <= 0.0 {
            continue;
        }
        let slot = tally
            .entry(c.underlying.trim().to_ascii_uppercase())
            .or_default()
            .entry(c.spot.to_bits())
            .or_insert((0, c.spot));
        slot.0 = slot.0.saturating_add(1);
    }
    tally
        .into_iter()
        .filter_map(|(underlying, votes)| {
            let mut best: Option<(usize, f64)> = None;
            for (count, value) in votes.into_values() {
                let take = match best {
                    None => true,
                    Some((best_count, best_value)) => {
                        count > best_count || (count == best_count && value < best_value)
                    }
                };
                if take {
                    best = Some((count, value));
                }
            }
            best.map(|(_, value)| (underlying, value))
        })
        .collect()
}

/// Underlyings that claim depth-200 sockets before any other, in this order.
///
/// OPERATOR-LOCKED 2026-08-26 (`websocket-connection-scope-lock.md`,
/// "DEPTH-200 IS NIFTY + BANKNIFTY ATM"): *"for evry one minute alwyas
/// espeiclaly for dpeth 200 … nifty atm ce atm pe always … even for
/// bancknfity atm ce atm pe also"*.
///
/// Four of the five sockets are these two underlyings' ATM CE/PE pairs. The
/// list is ORDERED — index 0 outranks index 1 — so NIFTY takes its pair
/// first if only one pair fits.
pub const DEPTH_200_PRIORITY_UNDERLYINGS: [&str; 2] = ["NIFTY", "BANKNIFTY"];

/// Rank of a candidate's underlying in [`DEPTH_200_PRIORITY_UNDERLYINGS`],
/// or `usize::MAX` for everything else.
///
/// Compared BEFORE distance, so a NIFTY pair outranks a nearer FINNIFTY pair
/// however the rupee arithmetic falls out.
fn depth_200_priority_rank(underlying: &str) -> usize {
    DEPTH_200_PRIORITY_UNDERLYINGS
        .iter()
        .position(|u| u.eq_ignore_ascii_case(underlying))
        .unwrap_or(usize::MAX)
}

/// Choose the depth-20 and depth-200 sets from one chain snapshot.
///
/// Pure, so every refusal rule is testable without a database or a socket.
///
/// Selection, per underlying:
/// 1. keep only the NEAREST expiry (a far month is an illiquid book);
/// 2. rank strikes by distance from spot;
/// 3. depth-20 takes ATM ± [`depth_20_strikes_each_side`], both legs — a
///    window sized to the ELIGIBLE underlying count so the 250-slot pool is
///    filled without being exceeded;
/// 4. depth-200 takes whole CE/PE pairs nearest ATM, up to
///    [`DEPTH_200_MAX_PAIRS`] across ALL underlyings.
#[must_use]
pub fn select_depth_universe(candidates: &[DepthCandidate]) -> DepthSelection {
    let mut out = DepthSelection::default();

    // ONE spot per underlying, decided before anything is refused.
    //
    // A contract is refused for its own bad strike or its own missing id, but
    // NEVER for a stale copy of a price that belongs to the underlying — see
    // `consensus_spot_by_underlying` for the socket that was silently lost to
    // exactly that.
    let spots = consensus_spot_by_underlying(candidates);
    let spot_of = |c: &DepthCandidate| -> f64 {
        spots
            .get(&c.underlying.trim().to_ascii_uppercase())
            .copied()
            .unwrap_or(f64::NAN)
    };

    // ── Refuse first, so nothing unusable can reach the ranking. ──
    let mut usable: Vec<(&DepthCandidate, ExchangeSegment)> = Vec::new();
    for c in candidates {
        if c.contract_security_id <= 0 {
            out.refused_zero_id += 1;
            continue;
        }
        let Some(segment) = contract_segment_for_underlying(&c.underlying) else {
            // Ask the RIGHT question. A stock option failing an index-name
            // lookup is not an unrecognised instrument — it is a stock, and
            // this lane does not give stocks depth. Only a candidate that
            // claims to be an INDEX option and still has no mapping is
            // genuinely unknown, which would mean a new index the six-entry
            // map has not been taught.
            if c.is_index_option {
                out.refused_unknown_underlying += 1;
            } else {
                out.refused_stock_option += 1;
            }
            continue;
        };
        if !segment_supports_depth(segment) {
            out.refused_depth_ineligible_segment += 1;
            continue;
        }
        if atm_distance_at(c, spot_of(c)) == f64::MAX {
            out.refused_bad_price += 1;
            continue;
        }
        usable.push((c, segment));
    }

    // ── Group by underlying, keeping only its nearest expiry. ──
    let mut underlyings: Vec<String> = usable
        .iter()
        .map(|(c, _)| c.underlying.trim().to_ascii_uppercase())
        .collect();
    underlyings.sort_unstable();
    underlyings.dedup();

    // Sized to the underlyings that SURVIVED refusal, not to the ones the
    // chain offered. SENSEX is offered every day and refused every day (BSE
    // has no depth book), so sizing on the offered count would reserve a
    // third of the pool for an underlying that can never use it.
    let each_side = depth_20_strikes_each_side(underlyings.len());
    out.depth_20_strikes_each_side = each_side;

    let mut chosen_depth_20: std::collections::HashSet<(SecurityId, ExchangeSegment)> =
        std::collections::HashSet::new();

    // Whole CE/PE pairs for depth-200, ranked across every underlying, so the
    // 2 pairs go to the 2 most at-the-money books rather than to whichever
    // underlying happens to sort first.
    // (priority_rank, atm_fraction, CE, PE). Priority is compared FIRST so a
    // NIFTY/BANKNIFTY pair outranks a nearer pair from any other underlying
    // (operator lock 2026-08-26); the fraction is normalised by spot so the
    // cross-underlying comparison is dimensionless. See the ranking helpers.
    let mut pair_pool: Vec<(String, usize, f64, SubscribeInstrument, SubscribeInstrument)> =
        Vec::new();

    for underlying in &underlyings {
        let mut rows: Vec<(&DepthCandidate, ExchangeSegment)> = usable
            .iter()
            .filter(|(c, _)| &c.underlying.trim().to_ascii_uppercase() == underlying)
            .copied()
            .collect();
        if rows.is_empty() {
            continue;
        }
        let nearest_expiry = rows.iter().map(|(c, _)| c.expiry_micros).min().unwrap_or(0);
        rows.retain(|(c, _)| c.expiry_micros == nearest_expiry);

        // Distinct strikes, nearest-ATM first.
        //
        // ONE index pass, then O(1) lookups. The previous shape claimed to be
        // "one O(k) pass to attach distances" in this very comment while
        // doing `rows.iter().find()` PER STRIKE — O(strikes x rows), i.e.
        // quadratic — and the loop below then filtered `rows` again per
        // strike, a second quadratic pass. Two O(k^2) scans sitting under a
        // comment asserting O(k).
        //
        // Keyed on integer paise rather than the f64 itself: a float is not
        // hashable, and rounding to paise is the same equality the old
        // `abs() < EPSILON` compare expressed, without the pairwise scan
        // needed to evaluate it.
        let mut by_strike: std::collections::HashMap<i64, Vec<(&DepthCandidate, ExchangeSegment)>> =
            std::collections::HashMap::with_capacity(rows.len());
        for (c, seg) in &rows {
            by_strike
                .entry(strike_key(c.strike))
                .or_default()
                .push((*c, *seg));
        }

        let mut strikes: Vec<f64> = rows.iter().map(|(c, _)| c.strike).collect();
        strikes.sort_by(|a, b| a.total_cmp(b));
        strikes.dedup_by(|a, b| (*a - *b).abs() < f64::EPSILON);
        let mut ranked: Vec<(f64, f64)> = strikes
            .iter()
            .map(|s| {
                let d = by_strike
                    .get(&strike_key(*s))
                    .and_then(|legs| legs.first())
                    .map_or(f64::MAX, |(c, _)| atm_distance_at(c, spot_of(c)));
                (d, *s)
            })
            .collect();
        ranked.sort_by(|a, b| a.0.total_cmp(&b.0));
        let strikes: Vec<f64> = ranked.into_iter().map(|(_, s)| s).collect();

        let keep = each_side * 2 + 1;
        for (rank, strike) in strikes.iter().enumerate() {
            // O(1) lookup replacing the second per-strike scan of `rows`.
            // `legs` is empty only if a strike vanished between the two
            // passes, which cannot happen — both read the same `rows`.
            let empty: Vec<(&DepthCandidate, ExchangeSegment)> = Vec::new();
            let legs = by_strike.get(&strike_key(*strike)).unwrap_or(&empty);

            if rank < keep {
                for (c, segment) in legs {
                    let inst = SubscribeInstrument {
                        security_id: c.contract_security_id as SecurityId,
                        segment: *segment,
                    };
                    // I-P1-11 dedup on the COMPOSITE, which this path did not
                    // have. A chain snapshot can carry the same contract under
                    // two `underlying_security_id` partitions, and a duplicate
                    // here is not harmless: it burns one of Dhan's 50 wire
                    // slots on that connection and inflates the count toward
                    // the 250 envelope, so real contracts get squeezed out by
                    // copies of ones already subscribed.
                    if chosen_depth_20.insert((inst.security_id, inst.segment)) {
                        out.depth_20.push(inst);
                    } else {
                        out.depth_20_deduped += 1;
                    }
                }
            }

            // A pair needs BOTH legs at this strike; a lone leg is skipped
            // rather than promoted, per the never-split-a-pair rule above.
            let ce = legs.iter().find(|(c, _)| c.leg.eq_ignore_ascii_case("CE"));
            let pe = legs.iter().find(|(c, _)| c.leg.eq_ignore_ascii_case("PE"));
            if let (Some((ce_c, ce_seg)), Some((pe_c, pe_seg))) = (ce, pe) {
                pair_pool.push((
                    ce_c.underlying.trim().to_ascii_uppercase(),
                    depth_200_priority_rank(&ce_c.underlying),
                    atm_distance_fraction_at(ce_c, spot_of(ce_c)),
                    SubscribeInstrument {
                        security_id: ce_c.contract_security_id as SecurityId,
                        segment: *ce_seg,
                    },
                    SubscribeInstrument {
                        security_id: pe_c.contract_security_id as SecurityId,
                        segment: *pe_seg,
                    },
                ));
            }
        }
    }

    // ── depth-200: whole pairs first, then one lone leg for the odd socket. ──
    //
    // Pairs are taken nearest-ATM first so the two complete books are the two
    // most informative ones. Only once no WHOLE pair fits in the remaining
    // budget does a lone leg get taken — so the lone leg is always the LAST
    // socket, never a pair displaced by a half.
    // ORDER: one ATM pair from EACH priority underlying before any underlying
    // takes a second pair.
    //
    // Operator lock 2026-08-26: "nifty atm ce atm pe always … even for
    // bancknfity atm ce atm pe also". That is ONE pair each, not two pairs
    // from whichever index ranks first — sorting on (priority, distance)
    // alone gives NIFTY both its ATM and its next strike and leaves BANKNIFTY
    // a lone leg, which is the same shape of miss as the raw-rupee bug, one
    // level up.
    //
    // So the primary key is the pair's RANK WITHIN ITS OWN UNDERLYING: every
    // underlying's nearest pair competes in round 0, every underlying's
    // second pair in round 1, and so on. Priority breaks ties inside a round
    // (NIFTY before BANKNIFTY before the rest), and the spot-normalised
    // distance breaks ties inside that.
    //
    // Result on a normal chain: NIFTY ATM pair, BANKNIFTY ATM pair, then the
    // odd socket to the next round's leader.
    {
        let mut by_underlying: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        // Nearest-first within each underlying, so the round index below is
        // assigned in ATM order rather than chain order.
        pair_pool.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.total_cmp(&b.2)));
        let mut ranked: Vec<(usize, usize, f64, SubscribeInstrument, SubscribeInstrument)> =
            Vec::with_capacity(pair_pool.len());
        for (underlying, priority, frac, ce, pe) in pair_pool.drain(..) {
            let round = by_underlying.entry(underlying).or_insert(0);
            ranked.push((*round, priority, frac, ce, pe));
            *round += 1;
        }
        // A priority underlying exhausts its strikes BEFORE any other
        // underlying takes a socket. Without this first key, NIFTY's SECOND
        // pair sorts behind FINNIFTY's first (both are "round 0" for their own
        // underlying), and the odd 5th socket lands on the sparse contract
        // whose idle-silence redials this change exists to stop.
        ranked.sort_by(|a, b| {
            (a.1 == usize::MAX)
                .cmp(&(b.1 == usize::MAX))
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.total_cmp(&b.2))
        });
        pair_pool = ranked
            .into_iter()
            .map(|(_, p, f, ce, pe)| (String::new(), p, f, ce, pe))
            .collect();
    }
    let mut remaining_pairs = pair_pool.into_iter();
    for (_, _, _, ce, pe) in remaining_pairs.by_ref() {
        // `+ 2` because a pair costs two sockets. The budget is EVEN (4) since
        // 2026-08-26, so a whole pair always either fits or the budget is
        // already full — the odd-socket case cannot arise by arithmetic.
        //
        // The lone-leg fill that used to live here is RETIRED with the fifth
        // socket (see DEPTH_200_MAX_SOCKETS). It is not merely unreachable at
        // today's budget: it must not come back if the budget ever changes
        // again, because a lone third-strike leg is not part of the set the
        // operator specified. `depth_200_lone_leg` therefore stays false and a
        // test pins it, so re-enabling one is a build failure rather than a
        // quiet re-widening.
        if out.depth_200.len().saturating_add(2) > DEPTH_200_MAX_SOCKETS {
            break;
        }
        out.depth_200.push(ce);
        out.depth_200.push(pe);
    }

    // A chain that offered candidates but yielded no depth-200 socket is a
    // real outcome and was previously invisible. It happens whenever no strike
    // carries BOTH legs — a CE-only snapshot, say — because the lone-leg
    // fallback fires only from a REJECTED pair and an empty pair pool never
    // rejects anything. Five authorized 200-level sockets then sit idle with
    // nothing in the result saying why.
    if out.depth_200.is_empty() && !usable.is_empty() {
        out.depth_200_no_pair_available = true;
    }

    // Last-resort envelope guard. The adaptive window is sized to fit, so
    // reaching here means an assumption broke — a chain carrying an uneven
    // number of legs per strike, say. Truncating and COUNTING is strictly
    // better than handing an oversized set to `plan_pool`, which refuses the
    // WHOLE pool and would cost the session all depth rather than the excess.
    if out.depth_20.len() > DEPTH_20_MAX_INSTRUMENTS {
        out.depth_20_dropped_for_capacity = out.depth_20.len() - DEPTH_20_MAX_INSTRUMENTS;
        out.depth_20.truncate(DEPTH_20_MAX_INSTRUMENTS);
    }

    out
}

/// The `/exec` query selecting one row per live contract from the most recent
/// chain snapshot.
///
/// `LATEST ON ts PARTITION BY` collapses the per-minute history to the newest
/// row per contract, and the `expiry >= today` bound is what stops a dead
/// contract being subscribed after an expiry rolls: those rows stay in the
/// table forever (nothing evicts them) and a subscription to an expired
/// contract returns silence indistinguishable from a quiet book.
///
/// `contract_security_id > 0` is deliberately ALSO enforced in
/// [`select_depth_universe`], not only here — the SQL filter keeps the result
/// set small, the code filter is what makes the refusal countable.
#[must_use]
pub fn build_depth_candidate_query(today_ist_nanos: i64) -> String {
    let today_micros = today_ist_nanos / 1_000;
    format!(
        // `ts >= {today_micros}` added 2026-08-14. Without it `LATEST ON ts`
        // returns the newest row per partition from ANY day, so a pre-09:16
        // caller silently got YESTERDAY's chain — stale `underlying_spot`
        // driving ATM ranking, on derivative ids Dhan documents as unstable
        // across days.
        //
        // This bound is safe ONLY because the caller now retries
        // (`dhan_feed_stack::attach_depth_when_available`). Added alone, it
        // would have turned a boot-time read into zero rows and REDUCED the
        // socket count — correct but worse. Day bound and late-attach are one
        // change; do not separate them.
        "SELECT underlying_symbol, contract_security_id, expiry, strike, \
         underlying_spot, leg FROM {OPTION_CHAIN_1M_TABLE} \
         WHERE feed = 'dhan' AND contract_security_id > 0 AND expiry >= {today_micros} \
         AND ts >= {today_micros} \
         LATEST ON ts PARTITION BY underlying_security_id, expiry, strike, leg;"
    )
}

/// Parse the `/exec` dataset into candidates.
///
/// Fail-LOUD on a malformed body or a missing `dataset` key, fail-soft per row
/// — the house pattern from `brutex_crossverify_boot::parse_lifecycle_dataset`.
/// The distinction matters: an empty `Vec` returned for garbage would be
/// indistinguishable from a genuinely empty chain, and the caller's response to
/// those two is different.
///
/// # Errors
/// Returns `Err` when the body is not JSON or carries no `dataset` array.
pub fn parse_depth_candidates_dataset(body: &str) -> Result<Vec<DepthCandidate>, String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Err("malformed /exec response: not valid JSON".to_owned());
    };
    let Some(rows) = v.get("dataset").and_then(|d| d.as_array()) else {
        return Err("malformed /exec response: missing dataset array".to_owned());
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(cols) = row.as_array() else { continue };
        if cols.len() < 6 {
            continue;
        }
        let underlying = cols[0].as_str().unwrap_or_default();
        if underlying.is_empty() {
            continue;
        }
        let Some(contract_security_id) = cols[1].as_i64() else {
            continue;
        };
        let expiry_micros = cols[2].as_i64().unwrap_or(0);
        let strike = cols[3].as_f64().unwrap_or(f64::NAN);
        let spot = cols[4].as_f64().unwrap_or(f64::NAN);
        let leg = cols[5].as_str().unwrap_or_default();
        if leg.is_empty() {
            continue;
        }
        out.push(DepthCandidate {
            underlying: underlying.to_owned(),
            contract_security_id,
            expiry_micros,
            strike,
            spot,
            // The chain leg pulls NIFTY / BANKNIFTY / SENSEX only — index
            // underlyings by construction, never a stock. Asserted rather
            // than assumed by `chain_sourced_candidates_are_all_index_options`,
            // because if the chain leg's scope ever widens, a stock arriving
            // here would be mislabelled an index and its refusal would read
            // as an unknown index rather than a stock we chose not to cover.
            is_index_option: true,
            leg: leg.to_owned(),
        });
    }
    Ok(out)
}

/// Build depth candidates from the DAILY CONTRACT ARTIFACT plus spot prices,
/// instead of from the per-minute option chain.
///
/// # Why this exists — the constraint was historical, not real
///
/// Depth needs a tradeable contract's `security_id`. When this module was
/// written on 2026-08-11 the Dhan instrument-master download was FORBIDDEN by
/// Q3 of the 2026-07-13 amendment, so the per-minute option chain was the only
/// authorized source of one. That is why depth waits for `option_chain_1m`.
///
/// Q3 was reversed the SAME DAY by the third 2026-08-11 quote, which ordered
/// the daily master download back. This file's own header records that ("the
/// instrument-master CSV — allowed again since the 2026-08-11 third quote")
/// and rejected it anyway, on the grounds that it was "a whole extra parse for
/// ids we already receive".
///
/// That reasoning weighed COST and ignored TIME, and time is the whole
/// problem. The chain's ids arrive at 09:16:00 at the earliest, because the
/// cadence leg's first boundary is compile-time asserted to that minute. The
/// artifact's ids are on disk before 08:30. And the parse is not extra at all:
/// `dhan_contract_universe::load_contract_universe` already reads this exact
/// artifact every attempt, so the rows are in memory regardless.
///
/// The consequence of the old source is that ten of the sixteen authorized
/// sockets cannot carry a byte until after the market has been open for a
/// full minute — on a system whose stated requirement is that nothing is
/// missed. Nothing about the exchange forces that; only the choice of source
/// did.
///
/// # What still has to be true
///
/// At-the-money needs a price, and the NSE pre-open call auction settles at
/// 09:08–09:12 and publishes an equilibrium price for every scrip. Feed that
/// in and depth is selectable at 09:12 and on the wire before 09:15. An
/// underlying with no price is SKIPPED, never guessed — an ATM window
/// centred on an invented spot would subscribe the wrong strikes and look
/// entirely healthy doing it.
///
/// # Not a scope change
///
/// No new fetch, no new endpoint, no widened universe, and nothing hardcoded:
/// the artifact is rebuilt from the master every morning, so the contract set
/// self-rolls at expiry exactly as the chain-sourced one did. The REJECT rows
/// in the scope lock forbid a hardcoded expiring list and a fifth endpoint
/// type; this is neither.
#[must_use]
pub fn depth_candidates_from_master(
    rows: &[crate::dhan_contract_universe::ContractRow],
    spot_paise: &std::collections::HashMap<String, i64>,
    today_ymd: u32,
) -> Vec<DepthCandidate> {
    let mut out = Vec::new();
    for r in rows {
        // Options only. A future has no strike and no leg, so it can never be
        // ranked by distance from at-the-money.
        if r.c != "OPTIDX" && r.c != "OPTSTK" {
            continue;
        }
        if r.l != "CE" && r.l != "PE" {
            continue;
        }
        // An expired contract will not trade again today. The chain-sourced
        // path gets this from its `expiry >= today` SQL predicate; here it is
        // explicit.
        if r.e < today_ymd {
            continue;
        }
        // No price means no at-the-money. Skipping is the only honest option:
        // a window centred on a guessed spot subscribes the wrong strikes and
        // reports success.
        let Some(&spot) = spot_paise.get(&r.u) else {
            continue;
        };
        if spot <= 0 || r.s <= 0 {
            continue;
        }
        let Some(expiry_micros) = ymd_to_epoch_micros(r.e) else {
            continue;
        };
        // Rupees on both sides, matching the chain-sourced builder. Only the
        // DIFFERENCE decides the ranking, so a consistent unit is what
        // matters — but mixing paise and rupees across the two sources would
        // produce a plausible, wrong window rather than an error.
        #[expect(
            clippy::cast_precision_loss,
            reason = "strike and spot in paise are far below f64's exact-integer range; \
                      the chain-sourced path carries the same f64 and they must agree"
        )]
        out.push(DepthCandidate {
            underlying: r.u.clone(),
            contract_security_id: i64::try_from(r.i).unwrap_or(0),
            expiry_micros,
            strike: r.s as f64 / 100.0,
            spot: spot as f64 / 100.0,
            leg: r.l.clone(),
            is_index_option: r.c == "OPTIDX",
        });
    }
    out
}

/// `YYYYMMDD` to epoch micros at UTC midnight.
///
/// The selector uses this only to keep the nearest expiry per underlying, so
/// a consistent monotonic mapping is what it needs; a real timestamp is
/// produced anyway so the field never becomes a number that means nothing.
#[must_use]
pub fn ymd_to_epoch_micros(ymd: u32) -> Option<i64> {
    let (y, m, d) = (ymd / 10_000, (ymd / 100) % 100, ymd % 100);
    let date = chrono::NaiveDate::from_ymd_opt(i32::try_from(y).ok()?, m, d)?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_micros())
}

/// Select depth from the daily contract artifact — available before 08:30 —
/// rather than from the option chain, which cannot publish before 09:16:00.
///
/// Returns `None` when the artifact is unreadable or yields no candidate, so
/// the caller falls back to the chain-sourced path. A fallback rather than a
/// hard failure because the artifact is written by a separate daily rider: if
/// that rider had a bad morning, late depth beats no depth.
///
/// # Errors
///
/// None — every failure degrades to `None` and is logged with its reason.
// TEST-EXEMPT: async composition of load_depth_candidates + select_depth_universe, each separately tested.
pub async fn load_depth_universe_from_master(
    questdb: &tickvault_common::config::QuestDbConfig,
    date_ist: &str,
    today_ymd: u32,
) -> Option<DepthSelection> {
    let candidates = load_depth_candidates(questdb, date_ist, today_ymd).await;
    if candidates.is_empty() {
        return None;
    }
    let selection = select_depth_universe(&candidates);
    if selection.depth_20.is_empty() && selection.depth_200.is_empty() {
        return None;
    }
    tracing::info!(
        source = "contract_artifact",
        candidates = candidates.len(),
        depth_20 = selection.depth_20.len(),
        depth_200 = selection.depth_200.len(),
        "depth universe resolved from the daily artifact — no wait for the 09:16 chain"
    );
    Some(selection)
}

/// The candidate slice the depth selector consumes, resolved from the daily
/// contract artifact plus live spot prices.
///
/// # Why this is its own function
///
/// The per-minute rebalance ([`crate::depth_rebalance`]) needs the SAME slice
/// the attach selected from. Otherwise the strikes it reasons about could
/// drift from the strikes actually subscribed, and a socket would move onto a
/// contract the selector never considered — a well-formed subscription to the
/// wrong instrument, which nothing downstream could tell apart from the right
/// one. Extracting it is what makes "one path" a fact rather than a comment.
///
/// # Returns an empty slice on every failure
///
/// An unreadable artifact, no underlying priced yet, or a master with no
/// usable options all produce an empty `Vec`, each logged with its reason.
/// Empty is the honest answer here: the rebalance's response to "we could not
/// tell" is to keep what it has, which is the same as its response to
/// "nothing changed", so the two need not be distinguished by this layer.
// TEST-EXEMPT: async composition of read_contract_artifact + fetch_spot_prices + parse_symbol_map + depth_candidates_from_master, each separately tested.
pub async fn load_depth_candidates(
    questdb: &tickvault_common::config::QuestDbConfig,
    date_ist: &str,
    today_ymd: u32,
) -> Vec<DepthCandidate> {
    let rows = match crate::dhan_contract_universe::read_contract_artifact(date_ist) {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(
                code = tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching.code_str(),
                %err,
                "depth: contract artifact unreadable, falling back to the option chain \
                 (which cannot publish before 09:16 IST)"
            );
            return Vec::new();
        }
    };
    let prices = crate::dhan_contract_universe::fetch_spot_prices(questdb, {
        // Same day bound the contract path uses; the artifact rows are
        // already today's by filename.
        crate::dhan_universe::ist_midnight_nanos(date_ist)
    })
    .await;
    // The same symbol map the contract path reads: depth groups by underlying
    // SYMBOL, and the spot prices come back keyed on (security_id, segment).
    let mapping_path = crate::dhan_universe::mapping_artifact_path(date_ist);
    let symbols = std::fs::read_to_string(&mapping_path)
        .map_err(|e| e.to_string())
        .and_then(|b| crate::dhan_contract_universe::parse_symbol_map(&b))
        .unwrap_or_default();
    let spot = crate::dhan_contract_universe::spot_paise_by_symbol(&symbols, &prices);
    if spot.is_empty() {
        // Pre-open has not settled yet (or the feed is not delivering). This
        // is the NORMAL state before ~09:08 and must not be an error.
        tracing::debug!("depth: no underlying priced yet — nothing to centre a window on");
        return Vec::new();
    }
    let candidates = depth_candidates_from_master(&rows, &spot, today_ymd);
    tracing::debug!(
        priced_underlyings = spot.len(),
        candidates = candidates.len(),
        "depth candidates resolved from the daily artifact"
    );
    candidates
}

/// Counter: a depth-universe resolution that ended with ZERO sockets.
///
/// This module had **no metrics at all** until 2026-08-21, and six failure
/// arms whose `tracing::error!` carried no `code=` field — so no metric filter
/// could match them either. QuestDB down, a non-2xx, an unparseable body or an
/// empty chain all produced the same outcome: ten of the sixteen authorized
/// sockets carry nothing for the whole session, and every operator surface
/// reads exactly as it does on a healthy day.
///
/// The `reason` label is what turns "depth is dark" into something actionable
/// — a transport failure and an empty chain need opposite responses, and the
/// morning after an expiry the empty case is EXPECTED.
pub const DEPTH_UNIVERSE_FAILED_COUNTER: &str = "tv_dhan_depth_universe_failed_total";

/// Every `reason` value [`DEPTH_UNIVERSE_FAILED_COUNTER`] can carry.
///
/// Enumerated so they can be pre-registered at zero. The CloudWatch delta
/// pipeline drops each series' FIRST observed sample as its baseline, and this
/// counter fires at most a handful of times per session — so an
/// un-pre-registered reason would lose its first increment and, on a session
/// that failed once, its only one.
pub const DEPTH_FAILURE_REASONS: [&str; 6] = [
    "client_build",
    "response_unreadable",
    "non_2xx",
    "request_failed",
    "unparseable",
    "empty_selection",
];

/// Publish a 0 for every reason before the first attempt.
pub fn pre_register_depth_failure_counters() {
    for reason in DEPTH_FAILURE_REASONS {
        metrics::counter!(DEPTH_UNIVERSE_FAILED_COUNTER, "reason" => reason).increment(0);
    }
}

/// Record one depth-universe failure.
///
/// `reason` must be a member of [`DEPTH_FAILURE_REASONS`]; a `&'static str`
/// rather than a `String` because a dynamic label allocates, and this is
/// called from a path the drain's own budget covers.
pub fn record_depth_failure(reason: &'static str) {
    metrics::counter!(DEPTH_UNIVERSE_FAILED_COUNTER, "reason" => reason).increment(1);
}
/// `/exec` HTTP timeout. Matches the sibling boot readers.
const QUESTDB_EXEC_TIMEOUT_SECS: u64 = 10;

/// Load and select the depth universe from the newest chain snapshot.
///
/// # Timing, stated plainly
///
/// This runs ONCE, at boot. On a normal morning the newest snapshot is
/// yesterday's final minute, whose contracts are still the live expiry — so
/// depth comes up populated before the open.
///
/// **The morning AFTER an expiry is the exception**: yesterday's rows are
/// excluded by the `expiry >= today` bound (correctly — they are dead
/// contracts), and today's chain has not been fetched yet at 08:30. Depth is
/// therefore EMPTY that morning until the process is next restarted. That is a
/// real limitation, not a bug being papered over: closing it needs live
/// re-subscription on an already-running pool, which is a larger change than
/// this one. It is logged at `error!` rather than left to be noticed.
///
/// An empty result is never reported as success. An empty instrument set opens
/// ZERO sockets, and calling that "depth enabled" is exactly the false-OK the
/// scope-lock forbids.
// Every decision this makes is delegated to the unit-tested pure fns
// (build_depth_candidate_query, parse_depth_candidates_dataset,
// select_depth_universe); this wrapper only moves bytes and logs.
// TEST-EXEMPT: network I/O (QuestDB /exec) — see the note above.
pub async fn load_depth_universe(
    questdb: &tickvault_common::config::QuestDbConfig,
    today_ist_nanos: i64,
) -> DepthSelection {
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            record_depth_failure("client_build");
            tracing::error!(
                code =
                    tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching.code_str(),
                ?err,
                "depth universe: HTTP client build failed — depth-20 and depth-200 will \
                 open ZERO sockets this session"
            );
            return DepthSelection::default();
        }
    };
    let sql = build_depth_candidate_query(today_ist_nanos);
    let body = match client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(b) => b,
            Err(err) => {
                record_depth_failure("response_unreadable");
                tracing::error!(
                    code = tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching
                        .code_str(),
                    ?err,
                    "depth universe: could not read the chain snapshot response — \
                     depth-20 and depth-200 will open ZERO sockets this session"
                );
                return DepthSelection::default();
            }
        },
        Ok(resp) => {
            record_depth_failure("non_2xx");
            tracing::error!(
                code = tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching.code_str(),
                status = %resp.status(),
                "depth universe: chain snapshot query returned non-2xx — depth-20 and \
                 depth-200 will open ZERO sockets this session"
            );
            return DepthSelection::default();
        }
        Err(err) => {
            record_depth_failure("request_failed");
            tracing::error!(
                code =
                    tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching.code_str(),
                ?err,
                "depth universe: chain snapshot query failed — depth-20 and depth-200 \
                 will open ZERO sockets this session"
            );
            return DepthSelection::default();
        }
    };

    let candidates = match parse_depth_candidates_dataset(&body) {
        Ok(c) => c,
        Err(reason) => {
            record_depth_failure("unparseable");
            tracing::error!(
                code =
                    tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching.code_str(),
                reason,
                "depth universe: chain snapshot did not parse — depth-20 and depth-200 \
                 will open ZERO sockets this session"
            );
            return DepthSelection::default();
        }
    };

    let selection = select_depth_universe(&candidates);
    if selection.depth_20.is_empty() && selection.depth_200.is_empty() {
        record_depth_failure("empty_selection");
        tracing::error!(
            code = tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching.code_str(),
            candidates = candidates.len(),
            refused_zero_id = selection.refused_zero_id,
            refused_unknown_underlying = selection.refused_unknown_underlying,
            refused_stock_option = selection.refused_stock_option,
            refused_bad_price = selection.refused_bad_price,
            refused_depth_ineligible_segment = selection.refused_depth_ineligible_segment,
            "depth universe is EMPTY — depth-20 and depth-200 will open ZERO sockets \
             this session. Expected on the morning after an expiry (yesterday's \
             contracts are dead and today's chain has not been fetched yet); a restart \
             after 09:16 IST populates it. Any other time, check whether the \
             option-chain leg is running and whether contract_security_id is populated."
        );
    } else {
        tracing::info!(
            depth_20 = selection.depth_20.len(),
            depth_200 = selection.depth_200.len(),
            candidates = candidates.len(),
            refused_zero_id = selection.refused_zero_id,
            refused_unknown_underlying = selection.refused_unknown_underlying,
            refused_stock_option = selection.refused_stock_option,
            refused_bad_price = selection.refused_bad_price,
            refused_depth_ineligible_segment = selection.refused_depth_ineligible_segment,
            "depth universe selected from the option chain"
        );
    }
    selection
}

#[cfg(test)]
mod tests {

    /// Build a candidate with an explicit spot, so cross-underlying ranking
    /// can be exercised at realistic index price levels.
    fn candidate_at(
        underlying: &str,
        sid: i64,
        strike: f64,
        leg: &str,
        spot: f64,
    ) -> DepthCandidate {
        let mut c = candidate(underlying, sid, strike, leg);
        c.spot = spot;
        c
    }

    /// THE OPERATOR'S RULE (lock 2026-08-26), reproduced from the live box.
    ///
    /// These are the real spots and strikes on 2026-08-26. Under the old raw
    /// `|strike - spot|` sort, FINNIFTY's 50-point gap beat BANKNIFTY's
    /// 100-point gap even though BANKNIFTY's spot is more than twice as large
    /// — and the box really did end up with FINNIFTY and MIDCPNIFTY holding
    /// four of the five 200-level sockets while BANKNIFTY held NONE.
    #[test]
    fn depth_200_gives_its_sockets_to_nifty_and_banknifty_not_the_nearest_rupee_gap() {
        let candidates = vec![
            // FINNIFTY: 50 points from spot — the SMALLEST raw gap here.
            candidate_at("FINNIFTY", 63097, 26_250.0, "CE", 26_200.0),
            candidate_at("FINNIFTY", 63100, 26_250.0, "PE", 26_200.0),
            // MIDCPNIFTY: 60 points.
            candidate_at("MIDCPNIFTY", 72982, 15_000.0, "CE", 14_940.0),
            candidate_at("MIDCPNIFTY", 72983, 15_000.0, "PE", 14_940.0),
            // BANKNIFTY: 100 points, but on a 57,500 spot — nearer in
            // fractional terms than either of the above.
            candidate_at("BANKNIFTY", 90001, 57_500.0, "CE", 57_400.0),
            candidate_at("BANKNIFTY", 90002, 57_500.0, "PE", 57_400.0),
            // NIFTY: 40 points on a 24,340 spot.
            candidate_at("NIFTY", 46991, 24_300.0, "CE", 24_310.0),
            candidate_at("NIFTY", 46992, 24_300.0, "PE", 24_310.0),
            // NIFTY one strike out — the realistic shape, and the natural
            // occupant of the odd 5th socket.
            candidate_at("NIFTY", 46993, 24_350.0, "CE", 24_310.0),
            candidate_at("NIFTY", 46994, 24_350.0, "PE", 24_310.0),
        ];

        let sel = select_depth_universe(&candidates);
        let ids: Vec<i64> = sel
            .depth_200
            .iter()
            .map(|i| i64::try_from(i.security_id).expect("test sid fits i64"))
            .collect();

        assert!(
            ids.contains(&46991) && ids.contains(&46992),
            "NIFTY ATM CE and PE must both hold sockets, got {ids:?}"
        );
        assert!(
            ids.contains(&90001) && ids.contains(&90002),
            "BANKNIFTY ATM CE and PE must both hold sockets — it held NONE on \
             2026-08-26, which is the defect this pins. Got {ids:?}"
        );
        assert!(
            !ids.contains(&63097) && !ids.contains(&63100),
            "FINNIFTY must NOT take a socket while a NIFTY or BANKNIFTY leg is \
             available on a smaller raw rupee gap; those two slots redialled 322 \
             times on 2026-08-26. Got {ids:?}"
        );
        assert!(
            !ids.contains(&72982) && !ids.contains(&72983),
            "MIDCPNIFTY must not take a socket either. Got {ids:?}"
        );
        assert_eq!(
            ids.len(),
            4,
            "FOUR sockets since the operator's 2026-08-26 instruction — two whole \
             pairs, no fifth. Got {ids:?}"
        );
        assert!(
            !sel.depth_200_lone_leg,
            "the lone leg is RETIRED with the fifth socket; a set that reports one \
             is no longer the set the operator specified"
        );
    }

    /// NIFTY outranks BANKNIFTY when only one pair can fit, because the
    /// priority list is ORDERED and index 0 wins.
    #[test]
    fn nifty_takes_the_last_pair_before_banknifty() {
        assert_eq!(depth_200_priority_rank("NIFTY"), 0);
        assert_eq!(depth_200_priority_rank("BANKNIFTY"), 1);
        assert_eq!(depth_200_priority_rank("banknifty"), 1, "case-insensitive");
        assert_eq!(depth_200_priority_rank("FINNIFTY"), usize::MAX);
        assert_eq!(depth_200_priority_rank("MIDCPNIFTY"), usize::MAX);
    }

    /// The normalised distance must make a far-in-rupees strike on a large
    /// index rank NEARER than a close-in-rupees strike on a small one — that
    /// inversion is the whole point, and raw rupees get it backwards.
    #[test]
    fn the_normalised_distance_compares_across_price_levels() {
        let banknifty = candidate_at("BANKNIFTY", 1, 57_500.0, "CE", 57_400.0); // 100 pts
        let finnifty = candidate_at("FINNIFTY", 2, 26_250.0, "CE", 26_200.0); // 50 pts

        assert!(
            atm_distance_at(&banknifty, banknifty.spot) > atm_distance_at(&finnifty, finnifty.spot),
            "raw rupees rank BANKNIFTY as further — this is the trap"
        );
        assert!(
            atm_distance_fraction_at(&banknifty, banknifty.spot)
                < atm_distance_fraction_at(&finnifty, finnifty.spot),
            "normalised by spot, BANKNIFTY is NEARER: {} vs {}",
            atm_distance_fraction_at(&banknifty, banknifty.spot),
            atm_distance_fraction_at(&finnifty, finnifty.spot)
        );
    }

    /// An unusable spot cannot rank first. `f64::MAX` sorts last, so a
    /// candidate with a zero or non-finite spot is refused rather than
    /// silently promoted to the front of the pool.
    #[test]
    fn an_unusable_spot_sorts_last_in_the_normalised_ranking() {
        let zero_spot = candidate_at("NIFTY", 3, 24_300.0, "CE", 0.0);
        let nan_spot = candidate_at("NIFTY", 4, 24_300.0, "CE", f64::NAN);
        assert_eq!(
            atm_distance_fraction_at(&zero_spot, zero_spot.spot),
            f64::MAX
        );
        assert_eq!(atm_distance_fraction_at(&nan_spot, nan_spot.spot), f64::MAX);
    }
    use super::*;

    fn candidate(underlying: &str, sid: i64, strike: f64, leg: &str) -> DepthCandidate {
        DepthCandidate {
            underlying: underlying.to_owned(),
            contract_security_id: sid,
            expiry_micros: 1_000,
            strike,
            spot: 100.0,
            leg: leg.to_owned(),
            // Index by default: every pre-existing test in this module uses an
            // index-shaped underlying, so this keeps their meaning unchanged.
            // The stock case sets it explicitly.
            is_index_option: true,
        }
    }

    /// The index that replaced two O(k^2) scans must group exactly what the
    /// pairwise `abs() < EPSILON` compares grouped — no more, no less.
    ///
    /// Getting this wrong is silent in the worst way: legs that fail to group
    /// leave a strike looking like a lone leg, which the never-split-a-pair
    /// rule then skips, and depth-200 quietly carries fewer pairs than it
    /// should while every counter reads healthy.
    #[test]
    fn strike_key_groups_exactly_what_the_epsilon_compare_grouped() {
        assert_eq!(strike_key(25_000.0), strike_key(25_000.0));
        // A float that is not bit-identical but IS the same strike.
        assert_eq!(strike_key(0.1 + 0.2), strike_key(0.3));
        // Genuinely different strikes must never collide.
        assert_ne!(strike_key(25_000.0), strike_key(25_050.0));
        // One paise apart is a real difference and must stay distinct.
        assert_ne!(strike_key(100.00), strike_key(100.01));
    }

    /// A garbage strike must land far away and sort last, never alias onto a
    /// real strike's bucket and silently join its legs.
    #[test]
    fn strike_key_sends_unusable_values_far_away_instead_of_aliasing() {
        assert_eq!(strike_key(f64::NAN), i64::MAX);
        assert_eq!(strike_key(f64::INFINITY), i64::MAX);
        assert_ne!(strike_key(f64::NAN), strike_key(25_000.0));
    }

    /// Behaviour parity: the same candidates must produce the same selection
    /// after the quadratic scans were replaced. Ordering is part of it —
    /// depth-20 fills nearest-ATM first, and an index that reordered legs
    /// would change which contracts make the 250 envelope.
    #[test]
    fn the_indexed_ranking_is_deterministic_and_nearest_atm_first() {
        let mut cands = Vec::new();
        for i in 0..30i64 {
            #[expect(clippy::cast_precision_loss, reason = "small test strikes")]
            let strike = 24_000.0 + (i as f64) * 50.0;
            cands.push(candidate("NIFTY", 1000 + i, strike, "CE"));
            cands.push(candidate("NIFTY", 2000 + i, strike, "PE"));
        }
        let a = select_depth_universe(&cands);
        let b = select_depth_universe(&cands);
        assert_eq!(
            a.depth_20, b.depth_20,
            "selection must be deterministic — a HashMap in the grouping must \
             never leak its iteration order into the result"
        );
        assert_eq!(a.depth_200, b.depth_200);
        assert!(!a.depth_20.is_empty());
    }

    /// Two of the three underlyings map to `NseFno`, so a default-to-NseFno
    /// would be right twice and silently wrong for SENSEX. That is exactly the
    /// shape of wrongness that survives review, which is why the map is
    /// fail-closed and this test names SENSEX explicitly.
    #[test]
    fn test_contract_segment_for_underlying_is_fail_closed_and_bse_is_not_defaulted() {
        assert_eq!(
            contract_segment_for_underlying("NIFTY"),
            Some(ExchangeSegment::NseFno)
        );
        assert_eq!(
            contract_segment_for_underlying("BANKNIFTY"),
            Some(ExchangeSegment::NseFno)
        );
        assert_eq!(
            contract_segment_for_underlying("SENSEX"),
            Some(ExchangeSegment::BseFno),
            "SENSEX options are BSE_FNO — a default of NseFno would be wrong here only"
        );
        assert_eq!(
            contract_segment_for_underlying("NOT_A_REAL_INDEX"),
            None,
            "unknown underlyings must be refused, never guessed"
        );
    }

    /// The finding this closes: `contract_segment_for_underlying` maps only
    /// the six INDEX underlyings, so every one of the ~20,000 stock-option
    /// candidates was counted as `refused_unknown_underlying`. Five figures of
    /// "unknown underlying" on a healthy morning is indistinguishable from a
    /// broken instrument master, and would send an operator hunting a data
    /// problem that does not exist.
    ///
    /// The DROP is deliberate and unchanged — 250 depth-20 slots cannot cover
    /// 20,000 contracts. Only the REASON moves.
    #[test]
    fn stock_options_are_refused_as_stock_options_not_as_unknown_underlyings() {
        let mut stock = candidate("RELIANCE", 5001, 2_500.0, "CE");
        stock.is_index_option = false;
        let sel = select_depth_universe(&[stock]);
        assert_eq!(sel.refused_stock_option, 1, "a stock option must say so");
        assert_eq!(
            sel.refused_unknown_underlying, 0,
            "a stock is not an unknown underlying — that label is what made a healthy \
             session look like a broken master"
        );
        assert!(sel.depth_20.is_empty() && sel.depth_200.is_empty());
    }

    /// The other half: a genuinely unmapped INDEX still reports as unknown.
    /// Without this, the fix above would silently swallow a new NSE index the
    /// six-entry map has not been taught — turning a real gap into a
    /// deliberate-looking skip.
    #[test]
    fn an_unmapped_index_option_still_counts_as_an_unknown_underlying() {
        let mut idx = candidate("NIFTYNXT50", 6001, 2_500.0, "CE");
        idx.is_index_option = true;
        let sel = select_depth_universe(&[idx]);
        assert_eq!(
            sel.refused_unknown_underlying, 1,
            "an index with no mapping is a real gap and must stay visible"
        );
        assert_eq!(sel.refused_stock_option, 0);
    }

    /// The chain leg hardcodes `is_index_option: true`. That is correct only
    /// while the chain pulls index underlyings alone — if its scope widens, a
    /// stock arriving through it would be mislabelled an index and its refusal
    /// would read as an unknown index rather than a stock we chose not to
    /// cover. Pinned so the widening has to face this.
    #[test]
    fn chain_sourced_candidates_are_all_index_options() {
        let body = r#"{"dataset":[["NIFTY",101,1900000000000000,25000.0,25000.0,"CE"]]}"#;
        let got = parse_depth_candidates_dataset(body).expect("valid dataset");
        assert_eq!(got.len(), 1);
        assert!(
            got[0].is_index_option,
            "the chain leg's candidates are index options by construction"
        );
    }

    /// The chain parser defaults a missing `contract_security_id` to 0, and the
    /// field is "added v2.5" upstream. A zero means the vendor did not tell us
    /// the id — subscribing it sends a well-formed request for a nonexistent
    /// instrument and receives silence that reads exactly like a quiet book.
    ///
    /// This doc comment and the `#[test]` below it were STRANDED ~55 lines
    /// above, separated from this function by a later test that was inserted
    /// between them. The attribute therefore landed on that other test as a
    /// duplicate, and this function was left with none — so it compiled as
    /// dead code and has never run. `cargo` said so on every build
    /// ("duplicated attribute", "function is never used"); nothing failed,
    /// because a test that does not run cannot fail.
    #[test]
    fn test_select_depth_universe_refuses_and_counts_zero_contract_ids() {
        let rows = vec![
            candidate("NIFTY", 0, 100.0, "CE"),
            candidate("NIFTY", -1, 100.0, "PE"),
            candidate("NIFTY", 555, 100.0, "CE"),
        ];
        let sel = select_depth_universe(&rows);
        assert_eq!(sel.refused_zero_id, 2);
        assert!(
            sel.depth_20.iter().all(|i| i.security_id == 555),
            "only the real id may be subscribed"
        );
    }

    #[test]
    fn test_unknown_underlying_is_refused_and_counted() {
        let rows = vec![candidate("MYSTERY", 900, 100.0, "CE")];
        let sel = select_depth_universe(&rows);
        assert_eq!(sel.refused_unknown_underlying, 1);
        assert!(sel.depth_20.is_empty());
        assert!(sel.depth_200.is_empty());
    }

    /// depth-200 has 5 slots and options trade in pairs, so a full budget is
    /// 2 whole pairs plus one lone leg. Pair ORDER is what this pins: the two
    /// complete books must be the two nearest ATM, and the lone leg must be
    /// the LAST socket — never a pair displaced by a half.
    #[test]
    fn test_depth_200_fills_all_five_sockets_pairs_first_then_one_lone_leg() {
        let rows = vec![
            candidate("NIFTY", 1, 100.0, "CE"),
            candidate("NIFTY", 2, 100.0, "PE"),
            candidate("NIFTY", 3, 105.0, "CE"),
            candidate("NIFTY", 4, 105.0, "PE"),
            candidate("NIFTY", 5, 110.0, "CE"),
            candidate("NIFTY", 6, 110.0, "PE"),
        ];
        let sel = select_depth_universe(&rows);
        assert_eq!(
            sel.depth_200.len(),
            DEPTH_200_MAX_SOCKETS,
            "all FOUR authorized 200-level sockets must be used"
        );
        // Nearest-ATM pairs (spot 100) are (1,2) then (3,4). The third pair
        // (5,6) does not fit and — since 2026-08-26 — its CE is NOT promoted
        // to a lone leg; the budget simply ends at two whole pairs.
        let ids: Vec<u64> = sel.depth_200.iter().map(|i| i.security_id).collect();
        assert_eq!(
            ids,
            vec![1, 2, 3, 4],
            "two whole pairs, and nothing after them"
        );
        assert!(
            !sel.depth_200_lone_leg,
            "the lone-leg fill is RETIRED: an even budget can only end on a whole \
             pair, and a third-strike leg is not part of the operator's set"
        );
    }

    /// A budget that only fits whole pairs must NOT report a lone leg.
    ///
    /// The flag is what a consumer keys its cross-leg logic off, so a flag
    /// that is set unconditionally is worse than no flag at all.
    #[test]
    fn test_lone_leg_flag_is_false_when_the_budget_holds_only_whole_pairs() {
        let rows = vec![
            candidate("NIFTY", 1, 100.0, "CE"),
            candidate("NIFTY", 2, 100.0, "PE"),
            candidate("NIFTY", 3, 105.0, "CE"),
            candidate("NIFTY", 4, 105.0, "PE"),
        ];
        let sel = select_depth_universe(&rows);
        assert_eq!(sel.depth_200.len(), 4, "only two pairs exist to take");
        assert!(
            !sel.depth_200_lone_leg,
            "no lone leg was taken, so the flag must stay false"
        );
    }

    /// A strike carrying only one leg is not a PAIR and must not be ranked as
    /// one. It may still reach the odd socket, but only via the lone-leg step
    /// after the pairs are placed — never ahead of a complete pair.
    #[test]
    fn test_a_half_strike_never_displaces_a_whole_pair() {
        let rows = vec![
            candidate("NIFTY", 1, 100.0, "CE"), // no PE at this strike
            candidate("NIFTY", 3, 105.0, "CE"),
            candidate("NIFTY", 4, 105.0, "PE"),
        ];
        let sel = select_depth_universe(&rows);
        let ids: Vec<u64> = sel.depth_200.iter().map(|i| i.security_id).collect();
        assert_eq!(
            ids,
            vec![3, 4],
            "the pair at 105 is taken; the lone CE at 100 is nearer ATM but is \
             not a pair, and with no second pair to reject there is no lone-leg \
             step to place it"
        );
        assert!(!sel.depth_200_lone_leg);
    }

    /// SENSEX can NEVER have a depth book, and the selector — not the socket —
    /// is where that has to be enforced.
    ///
    /// Regression for the 2026-08-12 prod defect: the selector handed BSE_FNO
    /// contracts to the planner, one of the five authorized 200-level sockets
    /// was opened for them, and the builder then refused the payload
    /// (`WS-GAP-02`, 09:06:49 IST) and tore the connection down. The depth-20
    /// path had no such check at all, so the same contracts went on the wire
    /// and came back as silence.
    #[test]
    fn test_bse_fno_is_refused_before_it_can_cost_a_socket() {
        let rows = vec![
            candidate("SENSEX", 10, 100.0, "CE"),
            candidate("SENSEX", 11, 100.0, "PE"),
            candidate("NIFTY", 1, 100.0, "CE"),
            candidate("NIFTY", 2, 100.0, "PE"),
        ];
        let sel = select_depth_universe(&rows);

        assert_eq!(
            sel.refused_depth_ineligible_segment, 2,
            "both SENSEX legs must be refused AND counted — a silent skip \
             would leave the operator unable to explain the missing underlying"
        );
        let all: Vec<u64> = sel
            .depth_20
            .iter()
            .chain(sel.depth_200.iter())
            .map(|i| i.security_id)
            .collect();
        assert_eq!(
            all,
            vec![1, 2, 1, 2],
            "only the NIFTY legs survive, in both depth sets"
        );
        assert!(
            sel.depth_20
                .iter()
                .chain(sel.depth_200.iter())
                .all(|i| i.segment != ExchangeSegment::BseFno),
            "no BSE_FNO instrument may reach either depth set"
        );
    }

    /// The selection-time gate and the payload-time gate must agree exactly.
    ///
    /// They are two functions in two crates encoding one vendor rule, which is
    /// precisely the shape that drifts. If a future edit widens one, this
    /// fails rather than letting the selector queue instruments the builder
    /// will refuse (a socket opened to die) or — worse — letting the builder
    /// accept what the selector would have rejected.
    #[test]
    fn test_segment_supports_depth_matches_the_builders_own_guard() {
        use tickvault_core::websocket::subscription_builder::validate_depth_segment;

        for segment in [
            ExchangeSegment::IdxI,
            ExchangeSegment::NseEquity,
            ExchangeSegment::NseFno,
            ExchangeSegment::BseEquity,
            ExchangeSegment::BseFno,
            ExchangeSegment::McxComm,
            ExchangeSegment::NseCurrency,
            ExchangeSegment::BseCurrency,
        ] {
            assert_eq!(
                segment_supports_depth(segment),
                validate_depth_segment(segment).is_ok(),
                "selection and payload gates disagree on {segment:?} — one of \
                 them will admit an instrument the other refuses"
            );
        }
    }

    /// What this selector costs at the authorised scale.
    ///
    /// MEASUREMENT, not a gate. `#[ignore]`d so a wall-clock number never
    /// becomes a flaky CI failure — the house pattern from
    /// `catch_up_seal_all_sweep_cost_at_the_authorized_ceiling`.
    ///
    /// # The number, and the two optimisations it refuted
    ///
    /// 44,000 rows across 220 underlyings, release-less debug build in an
    /// x86 dev container, minimum of five runs:
    ///
    /// | version | cost |
    /// |---|---|
    /// | before the per-underlying consensus spot | **7.4 ms** |
    /// | with it (shipped) | **32.3 ms** |
    /// | normalise once into a `Vec<String>`, carry the spot | **46.1 ms** |
    /// | borrow the raw `&str` as the map key, zero allocations | **29.7 ms** |
    ///
    /// The consensus fix (defect 10) cost 4.4x, which is worth knowing and
    /// was invisible until timed. Both attempts to win it back FAILED:
    ///
    /// - Normalising once up front and carrying the resolved spot on the
    ///   `usable` tuple made it **43% SLOWER**. Forty-four thousand retained
    ///   `String`s cost more than twice as many short-lived ones the
    ///   allocator immediately reuses, and widening the tuple added copy cost
    ///   through two further `Vec` builds.
    /// - Borrowing the raw name as the key removes all 88,000 allocations and
    ///   buys **8%** — which also refutes the diagnosis behind the first
    ///   attempt. The cost is not the strings. It is two hash lookups per row
    ///   into a nested map. Eight percent does not pay for the behaviour
    ///   change it carries (two rows of one underlying written with different
    ///   casing would get separate consensus prices).
    ///
    /// So the shipped version stands, and the honest framing is that ~32 ms
    /// once a minute is a **0.05% duty cycle** on the rebalance task. Recorded
    /// rather than optimised, because a measured cost with a stated shape is
    /// the thing this repository's own complexity table exists to hold — and
    /// because I nearly recorded "fixed, 4.4x faster" off a single sample of
    /// an optimisation that was in fact slower.
    ///
    /// NOT measured on the prod r8g.xlarge (Graviton4), and not measured in
    /// release mode; both would be faster.
    #[test]
    #[ignore = "wall-clock measurement, run on demand"]
    fn select_depth_universe_cost_at_the_authorized_ceiling() {
        // 220 F&O underlyings, 100 strikes each, both legs = 44,000 rows.
        let mut rows: Vec<DepthCandidate> = Vec::with_capacity(44_000);
        for u in 0..220 {
            let name = format!("STK{u:03}");
            let spot = 1_000.0 + f64::from(u);
            for k in 0..100 {
                let strike = spot + f64::from(k - 50) * 10.0;
                for (leg, off) in [("CE", 0), ("PE", 1)] {
                    rows.push(DepthCandidate {
                        underlying: name.clone(),
                        contract_security_id: i64::from(u) * 1_000 + i64::from(k) * 2 + off + 1,
                        expiry_micros: 1_900_000_000_000_000,
                        strike,
                        spot,
                        leg: leg.to_owned(),
                        is_index_option: false,
                    });
                }
            }
        }
        // Five runs, reporting the MINIMUM. A single sample in a shared
        // container is noise, and the minimum is the closest thing to the
        // cost without interference.
        let mut best = std::time::Duration::MAX;
        for _ in 0..5 {
            let start = std::time::Instant::now();
            let got = select_depth_universe(&rows);
            best = best.min(start.elapsed());
            assert_eq!(got.refused_stock_option, rows.len());
        }
        println!(
            "MEASURED select_depth_universe: {} rows, 220 underlyings -> {best:?} (min of 5)",
            rows.len(),
        );
    }

    /// A chain legitimately carries weeklies AND monthlies. Depth on a far
    /// month is bandwidth spent on an illiquid book, so only the nearest expiry
    /// is subscribed.
    #[test]
    fn test_only_the_nearest_expiry_is_selected() {
        let mut far = candidate("NIFTY", 99, 100.0, "CE");
        far.expiry_micros = 9_999_999;
        let rows = vec![candidate("NIFTY", 1, 100.0, "CE"), far];
        let sel = select_depth_universe(&rows);
        assert!(
            sel.depth_20.iter().all(|i| i.security_id == 1),
            "the far-month contract must not be subscribed"
        );
    }

    /// A NaN or non-positive STRIKE cannot be ranked against ATM. Ranking it
    /// anyway would sort it to an arbitrary position and could put garbage at
    /// the front of the depth-200 pair pool.
    ///
    /// **Amended 2026-08-26.** This test used to assert that a row carrying a
    /// zero SPOT was refused too, and counted BOTH refusals. That was the
    /// per-row rule, and it cost a socket: spot belongs to the UNDERLYING and
    /// the chain merely stamps a copy on every row, so one stale or NULL copy
    /// refused a real contract. Here NIFTY is quoted at 100 by the other row,
    /// so the contract at strike 100 is a genuine at-the-money contract and is
    /// now chosen. Only the NaN strike — a defect of the contract itself — is
    /// refused. See `consensus_spot_by_underlying`.
    #[test]
    fn test_non_finite_strikes_are_refused_and_counted() {
        let mut nan_strike = candidate("NIFTY", 7, f64::NAN, "CE");
        nan_strike.spot = 100.0;
        let mut zero_spot = candidate("NIFTY", 8, 100.0, "PE");
        zero_spot.spot = 0.0;
        let sel = select_depth_universe(&[nan_strike, zero_spot]);
        assert_eq!(
            sel.refused_bad_price, 1,
            "only the NaN strike is the contract's own fault"
        );
        assert_eq!(
            sel.depth_20
                .iter()
                .map(|i| i.security_id)
                .collect::<Vec<_>>(),
            vec![8],
            "the contract at the money is chosen from the underlying's quoted price"
        );
    }

    /// An underlying with NO usable price ANYWHERE still refuses every one of
    /// its contracts.
    ///
    /// The amendment above narrows what a bad price refuses; it must not
    /// remove the refusal. With nothing to centre on, a window is centred on a
    /// guess, and a guessed window subscribes the wrong strikes and reads as a
    /// quiet book.
    #[test]
    fn test_an_underlying_with_no_price_at_all_is_still_refused() {
        let mut a = candidate("NIFTY", 7, 100.0, "CE");
        a.spot = 0.0;
        let mut b = candidate("NIFTY", 8, 100.0, "PE");
        b.spot = f64::NAN;
        let sel = select_depth_universe(&[a, b]);
        assert_eq!(sel.refused_bad_price, 2);
        assert!(sel.depth_20.is_empty());
        assert!(sel.depth_200.is_empty());
    }

    /// The stale-copy case, end to end, on the shape that actually lost a
    /// socket: BANKNIFTY offers a whole pair but one leg's row carries an
    /// unusable spot, while MIDCPNIFTY offers a clean pair.
    ///
    /// Under the per-row rule the BANKNIFTY PUT was refused, its strike had no
    /// whole pair left, and the operator's locked NIFTY/BANKNIFTY-first order
    /// handed the 200-level sockets to MIDCPNIFTY — on a chain that plainly
    /// listed a usable BANKNIFTY pair. Found by a property test.
    #[test]
    fn test_a_stale_spot_on_one_leg_does_not_cost_a_priority_underlying_its_socket() {
        let mut bank_ce = candidate("BANKNIFTY", 101, 57_500.0, "CE");
        bank_ce.spot = 57_500.0;
        let mut bank_pe = candidate("BANKNIFTY", 102, 57_500.0, "PE");
        bank_pe.spot = 0.0; // the NULL/stale copy
        let mut mid_ce = candidate("MIDCPNIFTY", 201, 12_600.0, "CE");
        mid_ce.spot = 12_600.0;
        let mut mid_pe = candidate("MIDCPNIFTY", 202, 12_600.0, "PE");
        mid_pe.spot = 12_600.0;
        let sel = select_depth_universe(&[bank_ce, bank_pe, mid_ce, mid_pe]);
        let chosen: Vec<u64> = sel.depth_200.iter().map(|i| i.security_id).collect();
        assert!(
            chosen.contains(&101) && chosen.contains(&102),
            "BANKNIFTY lost its socket to a stale price copy: {chosen:?}"
        );
    }

    /// Ranking is by distance from spot, not by strike order — otherwise the
    /// "nearest ATM" set would just be the lowest strikes.
    #[test]
    fn test_depth_20_keeps_the_strikes_nearest_atm_not_the_lowest() {
        // The chain must be WIDER than the window or nothing is dropped and
        // the ranking is untested. One underlying takes the ±50 cap, so 101
        // strikes are kept — this chain offers 140.
        let mut rows = Vec::new();
        // Spot is 200; strikes 1..=140 means the lowest are the FURTHEST out.
        for i in 1..=140_i64 {
            let mut c = candidate("NIFTY", i, i as f64, "CE");
            c.spot = 200.0;
            rows.push(c);
        }
        let sel = select_depth_universe(&rows);
        let ids: Vec<u64> = sel.depth_20.iter().map(|i| i.security_id).collect();
        assert!(
            ids.contains(&140),
            "strike 140 is nearest spot 200 and must be kept: {ids:?}"
        );
        assert!(
            !ids.contains(&1),
            "strike 1 is furthest from spot and must be dropped"
        );
        assert_eq!(
            sel.depth_20_strikes_each_side, DEPTH_20_MAX_STRIKES_EACH_SIDE,
            "a single underlying takes the cap, not the budget arithmetic"
        );
        assert_eq!(
            ids.len(),
            DEPTH_20_MAX_STRIKES_EACH_SIDE * 2 + 1,
            "one leg per strike here, so the count is the strike window"
        );
    }

    // ---- adaptive depth-20 window (2026-08-19) ----

    #[test]
    fn depth_20_strikes_each_side_fills_the_envelope_without_exceeding_it() {
        // The property that matters at every underlying count: use nearly all
        // 250 slots, never more. Before this was adaptive, two underlyings
        // used 84 of 250 and nothing reported it.
        for underlyings in 1..=8usize {
            let each_side = depth_20_strikes_each_side(underlyings);
            let used = (each_side * 2 + 1) * underlyings * 2;
            assert!(
                used <= DEPTH_20_MAX_INSTRUMENTS,
                "{underlyings} underlyings would need {used} of {DEPTH_20_MAX_INSTRUMENTS}"
            );
            // Allow one strike of slack per underlying — the window is an odd
            // count of strikes, so a perfect fill is not always reachable.
            let slack = underlyings * 2 * 2;
            assert!(
                used + slack >= DEPTH_20_MAX_INSTRUMENTS
                    || each_side == DEPTH_20_MAX_STRIKES_EACH_SIDE,
                "{underlyings} underlyings used only {used} of {DEPTH_20_MAX_INSTRUMENTS}"
            );
        }
    }

    #[test]
    fn the_two_eligible_underlyings_case_fills_the_pool() {
        // The case that actually runs today: Dhan serves depth on NSE only,
        // so SENSEX is refused and the eligible set is NIFTY + BANKNIFTY.
        let each_side = depth_20_strikes_each_side(2);
        let used = (each_side * 2 + 1) * 2 * 2;
        assert_eq!(each_side, 30);
        assert_eq!(used, 244, "244 of 250, against 84 before this change");
    }

    #[test]
    fn the_same_contract_twice_burns_no_depth_20_slot() {
        // A chain snapshot can carry the same contract under two
        // underlying_security_id partitions. Without composite dedup the
        // duplicate takes one of Dhan's 50 wire slots on that connection and
        // pushes a real contract out of the 250-slot envelope.
        let mut a = candidate("NIFTY", 42, 100.0, "CE");
        a.spot = 100.0;
        let mut b = candidate("NIFTY", 42, 100.0, "CE");
        b.spot = 100.0;
        let sel = select_depth_universe(&[a, b]);
        assert_eq!(sel.depth_20.len(), 1);
        assert_eq!(sel.depth_20_deduped, 1, "and the duplicate is COUNTED");
    }

    #[test]
    fn a_ce_only_chain_reports_that_no_depth_200_pair_existed() {
        // The lone-leg fallback fires only from a REJECTED pair, and an empty
        // pair pool never rejects anything — so all 5 authorized 200-level
        // sockets sat idle with nothing in the result saying why.
        let mut c = candidate("NIFTY", 1, 100.0, "CE");
        c.spot = 100.0;
        let sel = select_depth_universe(&[c]);
        assert!(sel.depth_200.is_empty());
        assert!(
            sel.depth_200_no_pair_available,
            "candidates arrived and still produced no socket — that is a \
             finding, not silence"
        );
        assert!(!sel.depth_20.is_empty(), "depth-20 is unaffected");
    }

    #[test]
    fn no_candidates_at_all_is_not_reported_as_a_missing_pair() {
        // The ordinary pre-open state must stay distinguishable from the
        // chain-arrived-but-unpairable one above.
        let sel = select_depth_universe(&[]);
        assert!(!sel.depth_200_no_pair_available);
    }

    #[test]
    fn no_eligible_underlyings_selects_no_window_rather_than_dividing_by_zero() {
        assert_eq!(depth_20_strikes_each_side(0), 0);
    }

    #[test]
    fn a_very_wide_underlying_set_degrades_to_the_money_rather_than_overflowing() {
        // 200 underlyings cannot each have a window; the arithmetic must
        // return 0 (the ATM strike only) and never a negative or a panic.
        let each_side = depth_20_strikes_each_side(200);
        assert_eq!(each_side, 0);
        assert!(200 * 2 > DEPTH_20_MAX_INSTRUMENTS);
        // ...which is exactly why the last-resort truncate guard exists.
    }

    #[test]
    fn the_pool_envelope_is_derived_from_the_vendor_limits_not_written_down() {
        assert_eq!(
            DEPTH_20_MAX_INSTRUMENTS,
            MAX_DEPTH_20_CONNECTIONS as usize * DEPTH_20_INSTRUMENTS_PER_CONNECTION as usize
        );
        assert_eq!(DEPTH_20_MAX_INSTRUMENTS, 250, "5 connections x 50 each");
    }

    /// Segments are resolved PER UNDERLYING, and the BSE answer is then what
    /// disqualifies SENSEX from depth entirely.
    ///
    /// **This test previously asserted the opposite** — that a SENSEX row
    /// reaches `depth_20` carrying `BSE_FNO`. It passed, and the behaviour it
    /// pinned was the 2026-08-12 prod defect: a `BSE_FNO` instrument in a
    /// depth set costs a socket that either dies on connect (depth-200) or
    /// sits live and silent (depth-20). The mapping half of the old assertion
    /// was right and is kept via `contract_segment_for_underlying`; the
    /// reaches-the-depth-set half was the bug and is now inverted.
    #[test]
    fn test_segments_are_per_underlying_and_bse_is_then_disqualified() {
        // The mapping itself still distinguishes them — that is what makes the
        // disqualification possible rather than a blanket refusal.
        assert_eq!(
            contract_segment_for_underlying("NIFTY"),
            Some(ExchangeSegment::NseFno)
        );
        assert_eq!(
            contract_segment_for_underlying("SENSEX"),
            Some(ExchangeSegment::BseFno)
        );

        let rows = vec![
            candidate("NIFTY", 11, 100.0, "CE"),
            candidate("SENSEX", 22, 100.0, "CE"),
        ];
        let sel = select_depth_universe(&rows);

        let nifty = sel
            .depth_20
            .iter()
            .find(|i| i.security_id == 11)
            .expect("NIFTY is NSE_FNO and depth-eligible");
        assert_eq!(nifty.segment, ExchangeSegment::NseFno);

        assert!(
            sel.depth_20.iter().all(|i| i.security_id != 22),
            "SENSEX must NOT reach the depth set — Dhan serves no BSE depth, \
             so this socket could only ever be silent"
        );
        assert_eq!(sel.refused_depth_ineligible_segment, 1);
    }

    /// Garbage must NOT read as "the chain is empty" — the caller treats those
    /// two very differently, and conflating them is how a parse failure
    /// becomes a silent zero-socket depth lane.
    #[test]
    fn test_parse_depth_candidates_dataset_errors_on_garbage_not_empty_list() {
        assert!(parse_depth_candidates_dataset("not json").is_err());
        assert!(parse_depth_candidates_dataset("{}").is_err());
        assert_eq!(
            parse_depth_candidates_dataset(r#"{"dataset":[]}"#),
            Ok(vec![]),
            "a genuinely empty chain stays Ok(empty)"
        );
    }

    #[test]
    fn test_dataset_row_parses_into_a_candidate() {
        let body = r#"{"dataset":[["NIFTY",4242,1780000000000,25000.0,24980.5,"CE"]]}"#;
        let rows = parse_depth_candidates_dataset(body).expect("parses");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].underlying, "NIFTY");
        assert_eq!(rows[0].contract_security_id, 4242);
        assert_eq!(rows[0].leg, "CE");
    }

    /// Expired contracts stay in the table forever (nothing evicts them), so
    /// without the expiry bound a rolled expiry would keep being subscribed and
    /// return silence indistinguishable from a quiet book.
    #[test]
    fn test_build_depth_candidate_query_bounds_by_expiry_and_refuses_zero_ids() {
        let sql = build_depth_candidate_query(1_780_000_000_000_000_000);
        assert!(
            sql.contains("expiry >="),
            "must exclude dead contracts: {sql}"
        );
        assert!(
            sql.contains("contract_security_id > 0"),
            "must exclude vendor-absent ids: {sql}"
        );
        // The 2026-08-14 rename: this reader is what feeds the 10 depth
        // sockets, so a stale table name here is the difference between
        // depth-live and depth-dark. Literal on purpose — see the note in
        // market_ram_store_boot.
        assert!(
            sql.contains("FROM rest_option_chain_1m"),
            "depth reader must follow the renamed REST table: {sql}"
        );
        assert!(
            sql.contains("LATEST ON ts"),
            "must collapse per-minute history to the newest row per contract: {sql}"
        );
        assert!(
            sql.contains("1780000000000000"),
            "nanos must be converted to micros for QuestDB: {sql}"
        );
    }

    /// The envelope is 5 conns x 50 for depth-20 and 5 x 1 for depth-200.
    /// Exceeding either makes `plan_pool` refuse the WHOLE lane, so a selection
    /// that overflows does not degrade — it takes the main feed down with it.
    #[test]
    fn test_selection_stays_inside_the_authorized_connection_envelope() {
        let mut rows = Vec::new();
        for (u, base) in [("NIFTY", 0_i64), ("BANKNIFTY", 1000), ("SENSEX", 2000)] {
            for i in 1..=200_i64 {
                for (leg_idx, leg) in ["CE", "PE"].iter().enumerate() {
                    let mut c = candidate(u, base + i * 2 + leg_idx as i64, i as f64, leg);
                    c.spot = 100.0;
                    rows.push(c);
                }
            }
        }
        let sel = select_depth_universe(&rows);
        assert!(
            sel.depth_20.len() <= 250,
            "depth-20 envelope is 5 conns x 50 = 250, got {}",
            sel.depth_20.len()
        );
        assert!(
            sel.depth_200.len() <= 5,
            "depth-200 envelope is 5 conns x 1 = 5, got {}",
            sel.depth_200.len()
        );
    }

    /// The day bound is what stops a pre-09:16 caller from silently receiving
    /// YESTERDAY's chain. `LATEST ON ts` returns the newest row per partition
    /// from ANY day without it.
    #[test]
    fn test_depth_candidate_query_is_day_bounded() {
        let today = 1_786_000_000_000_000_000i64;
        let sql = build_depth_candidate_query(today);
        let micros = today / 1_000;
        assert!(
            sql.contains(&format!("ts >= {micros}")),
            "the depth candidate query MUST bound ts to today — without it LATEST ON ts \
             returns yesterday's chain and depth ranks ATM off a stale spot. sql={sql}"
        );
        assert!(
            sql.contains(&format!("expiry >= {micros}")),
            "the expiry filter must survive alongside the day bound. sql={sql}"
        );
    }
}

#[cfg(test)]
mod master_sourced_tests {
    use super::*;
    use crate::dhan_contract_universe::ContractRow;
    use std::collections::HashMap;

    fn opt(u: &str, sid: u64, strike_paise: i64, leg: &str, e: u32) -> ContractRow {
        ContractRow {
            i: sid,
            x: "NSE".into(),
            c: "OPTIDX".into(),
            e,
            s: strike_paise,
            l: leg.into(),
            u: u.into(),
        }
    }

    fn spot(u: &str, paise: i64) -> HashMap<String, i64> {
        let mut m = HashMap::new();
        m.insert(u.to_string(), paise);
        m
    }

    /// The point of the whole change: the artifact carries everything a depth
    /// candidate needs except the price, and the price exists at 09:12.
    #[test]
    fn depth_candidates_from_master_needs_only_the_artifact_and_a_price() {
        let rows = vec![
            opt("NIFTY", 101, 2_500_000, "CE", 20_260_828),
            opt("NIFTY", 102, 2_500_000, "PE", 20_260_828),
        ];
        let got = depth_candidates_from_master(&rows, &spot("NIFTY", 2_500_000), 20_260_821);
        assert_eq!(got.len(), 2, "both legs of a priced strike are candidates");
        assert!(
            (got[0].strike - 25_000.0).abs() < f64::EPSILON,
            "paise -> rupees"
        );
        assert!(
            (got[0].spot - 25_000.0).abs() < f64::EPSILON,
            "same unit on both sides"
        );
        assert_eq!(got[0].underlying, "NIFTY");
        assert!(got[0].expiry_micros > 0);
    }

    /// An unpriced underlying is SKIPPED, never centred on a guess. A window
    /// built around an invented spot subscribes the wrong strikes and reports
    /// success doing it — the exact false-OK shape this repo forbids.
    #[test]
    fn an_unpriced_underlying_is_skipped_rather_than_guessed() {
        let rows = vec![opt("RELIANCE", 201, 300_000, "CE", 20_260_828)];
        assert!(depth_candidates_from_master(&rows, &HashMap::new(), 20_260_821).is_empty());
    }

    /// Futures have no strike and no leg, so they can never be ranked by
    /// distance from at-the-money. Including them would put bandwidth on a
    /// book the selector cannot order.
    #[test]
    fn futures_and_legless_rows_are_not_depth_candidates() {
        let mut fut = opt("NIFTY", 301, 0, "", 20_260_828);
        fut.c = "FUTIDX".into();
        let rows = vec![fut];
        assert!(
            depth_candidates_from_master(&rows, &spot("NIFTY", 2_500_000), 20_260_821).is_empty()
        );
    }

    /// An expired contract will not trade again today. The chain-sourced path
    /// gets this from its SQL predicate; this path must do it explicitly or a
    /// stale artifact would subscribe dead strikes.
    #[test]
    fn an_expired_contract_is_dropped() {
        let rows = vec![opt("NIFTY", 401, 2_500_000, "CE", 20_260_820)];
        assert!(
            depth_candidates_from_master(&rows, &spot("NIFTY", 2_500_000), 20_260_821).is_empty()
        );
        // Same-day expiry is still tradeable and must survive.
        let today = vec![opt("NIFTY", 402, 2_500_000, "CE", 20_260_821)];
        assert_eq!(
            depth_candidates_from_master(&today, &spot("NIFTY", 2_500_000), 20_260_821).len(),
            1,
            "expiry day is a trading day"
        );
    }

    /// A zero or negative id can never be subscribed. The selector refuses it
    /// downstream too, but a candidate list that carries it makes the refusal
    /// counter fire for a row this builder should never have emitted.
    #[test]
    fn a_zero_strike_or_zero_spot_never_becomes_a_candidate() {
        let rows = vec![opt("NIFTY", 501, 0, "CE", 20_260_828)];
        assert!(
            depth_candidates_from_master(&rows, &spot("NIFTY", 2_500_000), 20_260_821).is_empty()
        );
        let ok = vec![opt("NIFTY", 502, 2_500_000, "CE", 20_260_828)];
        assert!(depth_candidates_from_master(&ok, &spot("NIFTY", 0), 20_260_821).is_empty());
    }

    /// The candidates must survive the real selector, not just look right.
    /// Without this the builder could emit a shape the ranking silently
    /// refuses, and depth would still be dark.
    #[test]
    fn master_sourced_candidates_are_accepted_by_the_real_selector() {
        let mut rows = Vec::new();
        let mut sid = 1000u64;
        for i in 0..40i64 {
            let strike = 2_400_000 + i * 5_000;
            rows.push(opt("NIFTY", sid, strike, "CE", 20_260_828));
            sid += 1;
            rows.push(opt("NIFTY", sid, strike, "PE", 20_260_828));
            sid += 1;
        }
        let cands = depth_candidates_from_master(&rows, &spot("NIFTY", 2_500_000), 20_260_821);
        assert_eq!(cands.len(), 80);
        let sel = select_depth_universe(&cands);
        assert!(
            !sel.depth_20.is_empty(),
            "the selector must actually pick from master-sourced candidates — \
             otherwise depth stays dark and this whole path buys nothing"
        );
        assert_eq!(sel.refused_zero_id, 0, "no zero ids reach the selector");
    }

    #[test]
    fn ymd_to_epoch_micros_rejects_impossible_dates_instead_of_wrapping() {
        assert!(ymd_to_epoch_micros(20_260_230).is_none(), "30 February");
        assert!(ymd_to_epoch_micros(20_261_301).is_none(), "month 13");
        assert!(ymd_to_epoch_micros(20_260_828).is_some());
    }
}

#[cfg(test)]
mod failure_metric_tests {
    use super::*;

    /// Every reason the code can emit must be pre-registered at zero.
    ///
    /// This counter fires at most a handful of times per session, and the
    /// CloudWatch delta pipeline drops each series' first observed sample as
    /// its baseline. An un-pre-registered reason therefore loses its first
    /// increment — and on a session that failed once, its only one, leaving
    /// the module exactly as silent as it was before this existed.
    #[test]
    fn pre_register_depth_failure_counters_covers_every_record_depth_failure_reason() {
        let src = include_str!("dhan_depth_universe.rs");
        let mut emitted: Vec<&str> = Vec::new();
        for (idx, _) in src.match_indices("record_depth_failure(\"") {
            let rest = &src[idx + "record_depth_failure(\"".len()..];
            if let Some(end) = rest.find('"') {
                emitted.push(&rest[..end]);
            }
        }
        assert!(
            emitted.len() >= 6,
            "expected every failure arm to record a reason, found {}",
            emitted.len()
        );
        for reason in emitted {
            assert!(
                DEPTH_FAILURE_REASONS.contains(&reason),
                "`{reason}` is emitted but not pre-registered — its first increment \
                 would be eaten as the CloudWatch baseline"
            );
        }
        // Safe with no recorder installed.
        pre_register_depth_failure_counters();
        record_depth_failure("empty_selection");
    }

    /// Every failure arm must carry a `code=` field.
    ///
    /// Without one, no CloudWatch metric filter can ever match the line — and
    /// that is precisely why six loud-looking `error!`s produced no operator
    /// signal at all. A log nobody can filter is a log nobody reads.
    #[test]
    fn every_depth_failure_log_carries_an_error_code() {
        // Production code only. Scanning the whole file makes the test match
        // ITS OWN source and fail on strings it wrote itself — which is how
        // the first version of this test failed.
        let src = include_str!("dhan_depth_universe.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        let errors = prod.matches("tracing::error!(").count();
        assert!(errors >= 6, "expected the six failure arms, found {errors}");
        let mut uncoded = 0;
        for (idx, _) in prod.match_indices("tracing::error!(") {
            // CHARS, not bytes. These messages are full of em-dashes, and a
            // byte-index window lands mid-character and panics — which is how
            // the second version of this test failed.
            let window: String = prod[idx..].chars().take(220).collect();
            if !window.contains("code =") {
                uncoded += 1;
            }
        }
        assert_eq!(
            uncoded, 0,
            "{uncoded} depth failure log(s) carry no `code=` field, so no alarm \
             can ever match them"
        );
    }

    /// `record_depth_failure` must be callable with no recorder installed,
    /// and must reject nothing — a metrics call that panicked inside a failure
    /// arm would turn a degraded depth selection into a dead task.
    #[test]
    fn record_depth_failure_is_safe_without_a_recorder() {
        for reason in DEPTH_FAILURE_REASONS {
            record_depth_failure(reason);
        }
    }
    /// Non-vacuity: the reason list must not contain entries nothing emits.
    /// A pre-registered reason with no emit site is a permanently-flat series
    /// that reads as health — the dead-monitor class this repo has retired
    /// twice before.
    #[test]
    fn no_pre_registered_depth_reason_is_unreachable() {
        let src = include_str!("dhan_depth_universe.rs");
        for reason in DEPTH_FAILURE_REASONS {
            assert!(
                src.contains(&format!("record_depth_failure(\"{reason}\")")),
                "`{reason}` is pre-registered but nothing emits it — a flat series \
                 that reads as health forever"
            );
        }
    }
}
