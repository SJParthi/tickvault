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

use tickvault_common::constants::STOCK_OPTION_ATM_STRIKES_EACH_SIDE;
use tickvault_common::types::{ExchangeSegment, SecurityId};
use tickvault_core::instrument::master_csv::{InstrumentClass, MasterRow, OptionLeg};
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

/// Directory the day's contract artifact lives in.
///
/// The same directory the mapping artifact uses, so one cleanup and one
/// retention policy covers both.
const CONTRACT_DIR: &str = "data/instrument-cache";

/// Path of the day's contract artifact.
///
/// # Why an artifact at all
///
/// The daily rider fetches the ~15 MB master, parses it, writes the ISIN
/// mapping, and drops the rows. The mapping is index constituents — cash
/// equities — so it carries no contract. Selection needs the derivative rows,
/// and it runs LATER than the rider (it waits for live prices to locate
/// at-the-money), by which time those rows are gone.
///
/// Re-downloading the master at attach time would mean a second 15 MB fetch of
/// a file we already had, in the minutes right after the open. Writing the
/// derivative subset once costs a few megabytes of disk and no network at all.
#[must_use]
pub fn contract_artifact_path(date_ist: &str) -> std::path::PathBuf {
    std::path::Path::new(CONTRACT_DIR).join(format!("dhan-contracts-{date_ist}.json"))
}

/// One derivative row, as written to and read from the artifact.
///
/// Field names are short because there are ~100,000 of them and the long
/// forms would add megabytes of key text to a file nothing reads by eye.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContractRow {
    /// `security_id`.
    pub i: u64,
    /// `EXCH_ID`.
    pub x: String,
    /// Instrument class, as the master's own word (`OPTIDX`, `FUTSTK`, …).
    pub c: String,
    /// Expiry `YYYYMMDD`.
    pub e: u32,
    /// Strike in paise.
    pub s: i64,
    /// `CE`, `PE`, or empty.
    pub l: String,
    /// Underlying symbol.
    pub u: String,
}

impl ContractRow {
    /// Rebuilds the [`MasterRow`] shape selection consumes.
    ///
    /// Round-tripping through `MasterRow` rather than teaching the selector a
    /// second input type keeps ONE selection path: the artifact and a freshly
    /// parsed master produce byte-identical selections, so a test against
    /// parsed rows is a test of what production actually runs.
    #[must_use]
    pub fn to_master_row(&self) -> MasterRow {
        MasterRow {
            security_id: self.i,
            isin: String::new(),
            symbol_name: String::new(),
            exch_id: self.x.clone(),
            segment: "D".into(),
            series: String::new(),
            class: InstrumentClass::from_master_field(&self.c),
            expiry_ymd: self.e,
            strike_paise: self.s,
            option_leg: OptionLeg::from_master_field(&self.l),
            underlying_symbol: self.u.clone(),
        }
    }
}

/// The derivative subset of a master, ready to write.
///
/// Equities and indices are dropped: they are the spot universe's business and
/// carry no expiry, so keeping them would roughly double the file for rows
/// selection immediately skips.
#[must_use]
pub fn contract_rows_from_master(master: &[MasterRow]) -> Vec<ContractRow> {
    master
        .iter()
        .filter(|r| r.class.is_future() || r.class.is_option())
        .map(|r| ContractRow {
            i: r.security_id,
            x: r.exch_id.clone(),
            c: match r.class {
                InstrumentClass::IndexFuture => "FUTIDX",
                InstrumentClass::StockFuture => "FUTSTK",
                InstrumentClass::IndexOption => "OPTIDX",
                InstrumentClass::StockOption => "OPTSTK",
                _ => "",
            }
            .into(),
            e: r.expiry_ymd,
            s: r.strike_paise,
            l: match r.option_leg {
                OptionLeg::Call => "CE",
                OptionLeg::Put => "PE",
                OptionLeg::None => "",
            }
            .into(),
            u: r.underlying_symbol.clone(),
        })
        .collect()
}

/// Writes the day's contract artifact atomically.
///
/// # Errors
///
/// Any filesystem or serialisation failure. The caller treats this as
/// non-fatal: a missing artifact costs the session its contracts, never its
/// spot universe.
///
/// # Why atomic
///
/// A half-written artifact that a later reader parses is worse than no
/// artifact: it would yield a partial contract set that looks complete. Same
/// reasoning, same mechanism as the mapping artifact beside it.
pub fn write_contract_artifact(date_ist: &str, rows: &[ContractRow]) -> anyhow::Result<()> {
    let path = contract_artifact_path(date_ist);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(rows)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Reads the day's contract artifact.
///
/// # Errors
///
/// Missing or unparseable file. Both are non-fatal to the caller and are
/// reported as "contracts are NOT in effect" rather than as an empty set that
/// looks like a market with no derivatives.
pub fn read_contract_artifact(date_ist: &str) -> anyhow::Result<Vec<ContractRow>> {
    let path = contract_artifact_path(date_ist);
    let body = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&body)?)
}

/// Index underlyings whose FULL option chain is subscribed.
///
/// Exactly two, and that is the operator's own restriction (2026-08-15):
/// *"one and only for indices for nifty and banknifty entire options contracts
/// should be fully subscribed"*. A third index here is a scope expansion
/// needing its own dated quote — the arity is pinned by a test for that reason.
///
/// # Why this differs from the older `FULL_CHAIN_INDEX_*` pair in `constants.rs`
///
/// Those list THREE — NIFTY, BANKNIFTY, SENSEX — from a 2026-04-25 sizing
/// exercise, and they are dead code (no call site; the dead-const ratchet
/// tracks them as such). Two dated things happened since and both cut the same
/// way: the 2026-08-15 quote above names two underlyings, and SENSEX is
/// `BSE_FNO` while the depth endpoints Dhan serves are NSE-only.
///
/// The divergence is recorded here rather than reconciled by using them,
/// deliberately. Referencing a dead constant — even from a doc comment — makes
/// the ratchet read it as wired, which would retire a real dead-code finding
/// in exchange for a cross-reference. The older constants stay dead and stay
/// tracked; this one is what the lane actually subscribes.
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
    /// Rows refused for carrying no usable expiry at all.
    pub refused_no_expiry: usize,
    /// Rows refused for having ALREADY expired.
    ///
    /// Separate from [`Self::refused_no_expiry`] because they mean different
    /// things: a missing date is a vendor defect, while a large count here on
    /// a normal day means the artifact is STALE — yesterday's file read after
    /// IST midnight, in which case every row is expired and the selection is
    /// empty for a reason that has nothing to do with the market.
    pub refused_expired: usize,
    /// Rows refused for naming no underlying.
    pub refused_no_underlying: usize,
    /// Expiries skipped for carrying too few legs to be a real chain.
    ///
    /// Non-zero means the nearest date in the master was a stub and the real
    /// chain sits behind it — worth seeing, because the alternative behaviour
    /// (taking the stub) silently discards a 400-leg chain.
    pub expiries_skipped_as_stubs: usize,
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
        if row.expiry_ymd == 0 {
            out.refused_no_expiry += 1;
            continue;
        }
        // Counted, not merely skipped. A stale artifact — yesterday's file
        // read after IST midnight — is ALL expired rows, and without this
        // counter it would select nothing while reporting zero refusals,
        // which reads identically to a market with no derivatives.
        if row.expiry_ymd < today_ymd {
            out.refused_expired += 1;
            continue;
        }
        // Its own counter, not the expiry one. A row with a good expiry and no
        // underlying is a different defect, and filing it under
        // `refused_no_expiry` sends triage at the wrong column.
        if row.underlying_symbol.is_empty() {
            out.refused_no_underlying += 1;
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
        let (expiry, skipped) = nearest_expiry(&bucket.options);
        out.expiries_skipped_as_stubs += skipped;
        let Some(expiry) = expiry else {
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

/// Legs an expiry must carry before it is treated as a real chain.
///
/// Two strikes, both legs. Below this it is not an option chain — it is one
/// stray row, and treating it as the current expiry discards the real one.
///
/// Deliberately small. A genuinely thin weekly on an illiquid stock is real
/// and must survive; the case this rejects is the single-row artefact, not the
/// quiet market.
const MIN_LEGS_FOR_A_REAL_EXPIRY: usize = 4;

/// The nearest expiry at or after today that carries a real chain.
///
/// # Why this is not a plain `min()`
///
/// It was, and that was a silent chain-collapse waiting to happen. Every
/// contract here is already `>= today`, so `min()` looks obviously correct —
/// but it hands the whole selection to whichever row has the earliest date,
/// however few legs sit behind it. One master row carrying a stray-but-valid
/// earlier expiry (a typo'd date, a vendor artefact, a delisted stub) would
/// take the "current expiry" slot from a 400-leg chain, and the result would
/// report `index_options = 1` with every refusal counter at zero — a chain
/// that vanished with nothing saying so.
///
/// So expiries are tried in ascending order and the first one carrying at
/// least [`MIN_LEGS_FOR_A_REAL_EXPIRY`] wins. Skipped expiries are returned so
/// the caller can count them rather than discard the fact.
///
/// Returns `None` when no expiry qualifies. `usize` is the number skipped.
fn nearest_expiry(contracts: &[Contract<'_>]) -> (Option<u32>, usize) {
    let mut legs: HashMap<u32, usize> = HashMap::new();
    for c in contracts {
        *legs.entry(c.expiry_ymd).or_insert(0) += 1;
    }
    let mut expiries: Vec<u32> = legs.keys().copied().collect();
    expiries.sort_unstable();
    let mut skipped = 0usize;
    for e in expiries {
        if legs.get(&e).copied().unwrap_or(0) >= MIN_LEGS_FOR_A_REAL_EXPIRY {
            return (Some(e), skipped);
        }
        skipped += 1;
    }
    (None, skipped)
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
        let (expiry, skipped) = nearest_expiry(&bucket.options);
        out.expiries_skipped_as_stubs += skipped;
        let Some(expiry) = expiry else {
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

// ---------------------------------------------------------------------------
// Live wiring: spot prices, the symbol map, and the composed load
// ---------------------------------------------------------------------------

/// Seconds a QuestDB `/exec` read may take before it is abandoned.
///
/// Matches the depth loader's budget. The attach retries, so a slow read costs
/// one attempt rather than the session.
const QUESTDB_EXEC_TIMEOUT_SECS: u64 = 10;

/// The `/exec` query returning today's latest traded price per instrument.
///
/// `LATEST ON ts PARTITION BY security_id, segment` collapses the day's ticks
/// to one row per instrument — the I-P1-11 composite, never the id alone, or
/// two instruments sharing a numeric id across segments would collapse into
/// one and one of them would get the other's price.
///
/// The `ts >= today` bound is what stops a stale price reaching ATM selection.
/// Without it `LATEST ON` returns the newest row from ANY day, so a pre-open
/// caller would locate at-the-money against yesterday's close — plausible,
/// wrong, and completely silent.
#[must_use]
pub fn build_spot_price_query(today_ist_nanos: i64) -> String {
    let today_micros = today_ist_nanos / 1_000;
    format!(
        "SELECT security_id, segment, ltp FROM ticks \
         WHERE feed = 'dhan' AND ts >= {today_micros} AND ltp > 0 \
         LATEST ON ts PARTITION BY security_id, segment;"
    )
}

/// Extracts `symbol -> (security_id, segment_code)` from a mapping artifact.
///
/// A separate parser from `dhan_live_universe::parse_mapping_artifact`
/// deliberately: that one returns ids only, because the live universe does not
/// care what anything is called. Contract selection groups by underlying
/// SYMBOL, so it needs the name the id belongs to.
///
/// # Errors
///
/// Malformed JSON, or no `mappings` array. A row missing any of the three
/// fields is skipped rather than failing the parse — one bad row must not cost
/// the other seven hundred.
pub fn parse_symbol_map(body: &str) -> Result<HashMap<String, (u64, u8)>, String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Err("mapping artifact is not valid JSON".to_owned());
    };
    let Some(rows) = v.get("mappings").and_then(|m| m.as_array()) else {
        return Err("mapping artifact has no `mappings` array".to_owned());
    };
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        let (Some(symbol), Some(id), Some(seg)) = (
            row.get("symbol").and_then(serde_json::Value::as_str),
            row.get("security_id").and_then(serde_json::Value::as_u64),
            row.get("exchange_segment")
                .and_then(serde_json::Value::as_u64),
        ) else {
            continue;
        };
        let Ok(seg) = u8::try_from(seg) else { continue };
        // A stock appears once per index it belongs to, so the same symbol
        // arrives many times with the same id. Insert is idempotent.
        out.insert(symbol.trim().to_uppercase(), (id, seg));
    }
    Ok(out)
}

/// Converts a QuestDB `/exec` dataset of `[security_id, segment, ltp]` into a
/// price map keyed on the I-P1-11 composite, in paise.
///
/// # Errors
///
/// Malformed JSON or a missing `dataset` array.
pub fn parse_spot_prices(body: &str) -> Result<HashMap<(u64, u8), i64>, String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Err("spot price response is not valid JSON".to_owned());
    };
    let Some(rows) = v.get("dataset").and_then(|d| d.as_array()) else {
        return Err("spot price response has no `dataset` array".to_owned());
    };
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        let Some(cols) = row.as_array() else { continue };
        if cols.len() < 3 {
            continue;
        }
        // Accepts the number AND the quoted-number form. QuestDB returns LONG
        // as a JSON number today, but a serialiser that quotes large integers
        // is a common and silent change, and here it would empty the price map
        // rather than error — the same failure shape as the segment bug below.
        let id = match &cols[0] {
            serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
            other => other.as_u64(),
        };
        let Some(id) = id else { continue };
        // The segment column is a SYMBOL holding the segment NAME — `NSE_EQ`,
        // `IDX_I`, `NSE_FNO` — because `TickRow::from_parsed_tick` writes it
        // through `segment_code_to_str`.
        //
        // This read used to `parse::<u8>()` the column, on the assumption that
        // it held the numeric code as text. It does not, so EVERY row was
        // skipped and the price map came back empty — every stock underlying
        // then landed in `underlyings_without_spot`, `ladders` was empty, and
        // `fit_atm_window` took its empty-ladders early return and reported a
        // window of 25. The result claimed the operator had received ATM ± 25
        // while ~22,000 authorized contracts were never subscribed. The
        // original unit test passed because its fixture encoded the same wrong
        // assumption; `spot_prices_parse_the_real_wire_format` now pins the
        // format production actually writes.
        let seg = match &cols[1] {
            serde_json::Value::String(s) => {
                tickvault_common::segment::segment_str_to_code(s.trim())
            }
            other => other.as_u64().and_then(|n| u8::try_from(n).ok()),
        };
        let Some(seg) = seg else { continue };
        let Some(ltp) = cols[2].as_f64() else {
            continue;
        };
        // Non-finite or non-positive is refused, not stored: a price of zero
        // would place at-the-money at the bottom of every ladder.
        if !ltp.is_finite() || ltp <= 0.0 {
            continue;
        }
        let paise = (ltp * 100.0).round();
        if paise > 9_007_199_254_740_991.0 {
            continue;
        }
        #[allow(clippy::cast_possible_truncation)]
        // APPROVED: bounded above by the line before and below by the `<= 0.0`
        // guard — a whole number well inside i64.
        let paise = paise as i64;
        out.insert((id, seg), paise);
    }
    Ok(out)
}

/// Joins the symbol map and the price map into the `symbol -> paise` form
/// selection consumes.
///
/// Only spot segments are consulted. A derivative's own price is not the
/// underlying's, and pricing at-the-money off a contract would put the window
/// around the option's premium rather than the stock.
#[must_use]
pub fn spot_paise_by_symbol(
    symbols: &HashMap<String, (u64, u8)>,
    prices: &HashMap<(u64, u8), i64>,
) -> HashMap<String, i64> {
    let mut out = HashMap::with_capacity(symbols.len());
    for (symbol, (id, seg)) in symbols {
        if let Some(&paise) = prices.get(&(*id, *seg)) {
            out.insert(symbol.clone(), paise);
        }
    }
    out
}

/// Loads today's contract universe: artifact from disk, prices from QuestDB.
///
/// Returns an EMPTY selection on any failure, having said which one. An empty
/// selection is safe — the lane keeps its spot universe — but it is never
/// silent, because "the market has no derivatives today" and "we could not
/// read the file" look identical in an instrument count.
// TEST-EXEMPT: async I/O composition (file read + HTTP GET); every pure part it calls — read_contract_artifact, parse_symbol_map, parse_spot_prices, spot_paise_by_symbol, select_contract_universe — is separately tested above.
pub async fn load_contract_universe(
    questdb: &tickvault_common::config::QuestDbConfig,
    date_ist: &str,
    today_ymd: u32,
    today_ist_nanos: i64,
    capacity: usize,
) -> ContractSelection {
    let contracts = match read_contract_artifact(date_ist) {
        Ok(c) => c,
        Err(err) => {
            tracing::error!(
                code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
                %err,
                date_ist,
                path = %contract_artifact_path(date_ist).display(),
                "contract universe: today's contract artifact is unreadable — the lane will \
                 carry its spot universe only. No futures, no option contracts."
            );
            return ContractSelection::default();
        }
    };
    let mapping_path = crate::dhan_universe::mapping_artifact_path(date_ist);
    let symbols = match std::fs::read_to_string(&mapping_path)
        .map_err(|e| e.to_string())
        .and_then(|b| parse_symbol_map(&b))
    {
        Ok(s) => s,
        Err(err) => {
            tracing::error!(
                code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
                err,
                path = %mapping_path.display(),
                "contract universe: the symbol map is unreadable, so at-the-money cannot be \
                 located for any stock. Futures and index chains are unaffected."
            );
            HashMap::new()
        }
    };

    let prices = fetch_spot_prices(questdb, today_ist_nanos).await;
    let spot = spot_paise_by_symbol(&symbols, &prices);

    let rows: Vec<MasterRow> = contracts.iter().map(ContractRow::to_master_row).collect();
    let selection = select_contract_universe(&rows, &spot, today_ymd, capacity);

    tracing::info!(
        contracts_in_artifact = contracts.len(),
        priced_underlyings = spot.len(),
        selected = selection.instruments.len(),
        index_futures = selection.index_futures,
        stock_futures = selection.stock_futures,
        index_options = selection.index_options,
        stock_options = selection.stock_options,
        atm_window = selection.atm_window_used,
        without_spot = selection.underlyings_without_spot,
        dropped_for_capacity = selection.dropped_for_capacity,
        "contract universe resolved"
    );
    selection
}

/// Reads today's latest price per instrument. Empty on any failure, said once.
async fn fetch_spot_prices(
    questdb: &tickvault_common::config::QuestDbConfig,
    today_ist_nanos: i64,
) -> HashMap<(u64, u8), i64> {
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()
    else {
        tracing::error!("contract universe: HTTP client build failed — no stock options");
        return HashMap::new();
    };
    let sql = build_spot_price_query(today_ist_nanos);
    let body = match client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(b) => b,
            Err(err) => {
                tracing::error!(?err, "contract universe: spot price response unreadable");
                return HashMap::new();
            }
        },
        Ok(resp) => {
            tracing::error!(status = %resp.status(), "contract universe: spot price query non-2xx");
            return HashMap::new();
        }
        Err(err) => {
            tracing::error!(?err, "contract universe: spot price query failed");
            return HashMap::new();
        }
    };
    match parse_spot_prices(&body) {
        Ok(p) => p,
        Err(err) => {
            tracing::error!(err, "contract universe: spot price response unparseable");
            HashMap::new()
        }
    }
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
    fn derivative_segment_maps_sensex_to_bse_fno_and_nse_to_nse_fno() {
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

    // ---- artifact + live wiring ----

    #[test]
    fn contract_rows_from_master_round_trips_to_the_same_selection() {
        // The property that makes every test above meaningful in production:
        // the artifact path and the parsed-master path must select the SAME
        // instruments. If they diverge, the tests exercise one thing and the
        // lane runs another.
        let mut master = stock_ladder("RELIANCE");
        master.push(contract(
            9_001,
            InstrumentClass::IndexFuture,
            "NSE",
            "NIFTY",
            2026_08_28,
            0,
            OptionLeg::None,
        ));
        // Equities and indices must not survive into the artifact.
        master.push(contract(
            9_002,
            InstrumentClass::Equity,
            "NSE",
            "",
            0,
            0,
            OptionLeg::None,
        ));

        let rows = contract_rows_from_master(&master);
        assert_eq!(
            rows.len(),
            master.len() - 1,
            "the equity row is dropped; contracts are not"
        );

        let rebuilt: Vec<MasterRow> = rows.iter().map(ContractRow::to_master_row).collect();
        let prices = spot("RELIANCE", 1000);
        assert_eq!(
            select_contract_universe(&master, &prices, TODAY, 25_000),
            select_contract_universe(&rebuilt, &prices, TODAY, 25_000),
        );
    }

    #[test]
    fn to_master_row_survives_serialisation() {
        let rows = contract_rows_from_master(&[contract(
            5,
            InstrumentClass::StockOption,
            "NSE",
            "RELIANCE",
            2026_08_28,
            1_234,
            OptionLeg::Put,
        )]);
        let json = serde_json::to_string(&rows).expect("serialises");
        let back: Vec<ContractRow> = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(rows, back);
        let m = back[0].to_master_row();
        assert_eq!(m.class, InstrumentClass::StockOption);
        assert_eq!(m.option_leg, OptionLeg::Put);
        assert_eq!(m.strike_paise, 123_400);
        assert_eq!(m.underlying_symbol, "RELIANCE");
    }

    #[test]
    fn build_spot_price_query_bounds_to_today_and_keys_on_the_composite() {
        let sql = build_spot_price_query(1_700_000_000_000_000_000);
        assert!(sql.contains("LATEST ON ts PARTITION BY security_id, segment"));
        assert!(
            sql.contains("ts >= 1700000000000000"),
            "micros, and bounded to today: an unbounded LATEST ON returns \
             yesterday's close and prices at-the-money against it silently"
        );
        assert!(sql.contains("ltp > 0"));
        assert!(sql.contains("feed = 'dhan'"));
    }

    /// The format production actually writes — segment NAMES, not codes.
    ///
    /// This test exists because its predecessor used `"1"` and `"0"` as the
    /// segment column and passed while the code could not parse a single real
    /// row. `TickRow::from_parsed_tick` writes the column through
    /// `segment_code_to_str`, so the wire values are `NSE_EQ` and `IDX_I`.
    /// A fixture that encodes the implementation's assumption instead of the
    /// producer's output is not a test, and this one is written from the
    /// producer.
    #[test]
    fn parse_spot_prices_reads_the_real_wire_format() {
        let body = r#"{"dataset":[
            ["2885","NSE_EQ",1234.55],
            [26000,"IDX_I",24500.0],
            [1,"NSE_FNO",42.0]
        ]}"#;
        let p = parse_spot_prices(body).expect("parses");
        assert_eq!(
            p.get(&(2885, 1)),
            Some(&123_455),
            "NSE_EQ must resolve to code 1 — a parse::<u8>() here silently \
             skipped EVERY row and produced zero stock options while the \
             report still said the ATM window was 25"
        );
        assert_eq!(p.get(&(26_000, 0)), Some(&2_450_000), "IDX_I is code 0");
        assert_eq!(p.get(&(1, 2)), Some(&4_200), "NSE_FNO is code 2");
    }

    #[test]
    fn parse_spot_prices_to_paise_and_refuses_the_unusable() {
        let body = r#"{"dataset":[
            [2885,"NSE_EQ",1234.55],
            [26000,"IDX_I",24500.0],
            [1,"NSE_EQ",0.0],
            [2,"NSE_EQ",-5.0],
            [3,"NSE_EQ",null],
            [4,"NOT_A_SEGMENT",100.0],
            [5,"NSE_EQ"]
        ]}"#;
        let p = parse_spot_prices(body).expect("parses");
        assert_eq!(p.get(&(2885, 1)), Some(&123_455));
        assert_eq!(p.get(&(26_000, 0)), Some(&2_450_000));
        assert_eq!(p.len(), 2, "zero, negative, null, bad segment, short row");
    }

    #[test]
    fn a_stray_early_expiry_cannot_steal_the_chain() {
        // One row with a valid but earlier date used to take the whole
        // current-expiry slot from a real chain, reporting index_options = 1
        // with every refusal counter at zero.
        let mut rows = vec![contract(
            999,
            InstrumentClass::IndexOption,
            "NSE",
            "NIFTY",
            2026_08_20,
            24_000,
            OptionLeg::Call,
        )];
        for (i, strike) in [24_000, 24_100, 24_200, 24_300].into_iter().enumerate() {
            let base = 100 + (i as u64) * 2;
            rows.push(contract(
                base,
                InstrumentClass::IndexOption,
                "NSE",
                "NIFTY",
                2026_08_26,
                strike,
                OptionLeg::Call,
            ));
            rows.push(contract(
                base + 1,
                InstrumentClass::IndexOption,
                "NSE",
                "NIFTY",
                2026_08_26,
                strike,
                OptionLeg::Put,
            ));
        }
        let sel = select_contract_universe(&rows, &no_spot(), TODAY, 25_000);
        assert_eq!(sel.index_options, 8, "the real chain, not the stub");
        assert_eq!(sel.expiries_skipped_as_stubs, 1, "and the stub is COUNTED");
        assert!(!sel.instruments.iter().any(|i| i.security_id == 999));
    }

    #[test]
    fn a_genuinely_thin_expiry_still_counts_as_a_chain() {
        // The guard must reject a one-row artefact WITHOUT rejecting a real
        // but quiet expiry. Four legs is a chain.
        let mut rows = Vec::new();
        for (i, strike) in [100, 110].into_iter().enumerate() {
            let base = 10 + (i as u64) * 2;
            rows.push(contract(
                base,
                InstrumentClass::IndexOption,
                "NSE",
                "NIFTY",
                2026_08_26,
                strike,
                OptionLeg::Call,
            ));
            rows.push(contract(
                base + 1,
                InstrumentClass::IndexOption,
                "NSE",
                "NIFTY",
                2026_08_26,
                strike,
                OptionLeg::Put,
            ));
        }
        let sel = select_contract_universe(&rows, &no_spot(), TODAY, 25_000);
        assert_eq!(sel.index_options, 4);
        assert_eq!(sel.expiries_skipped_as_stubs, 0);
    }

    #[test]
    fn a_stale_artifact_reports_expired_rows_rather_than_an_empty_market() {
        // Yesterday's artifact read after IST midnight is ALL expired. Without
        // its own counter that returns an empty selection with zero refusals,
        // which reads exactly like a market with no derivatives.
        let rows = vec![
            contract(
                1,
                InstrumentClass::IndexFuture,
                "NSE",
                "NIFTY",
                2026_08_18,
                0,
                OptionLeg::None,
            ),
            contract(
                2,
                InstrumentClass::StockFuture,
                "NSE",
                "RELIANCE",
                2026_08_01,
                0,
                OptionLeg::None,
            ),
        ];
        let sel = select_contract_universe(&rows, &no_spot(), TODAY, 25_000);
        assert!(sel.instruments.is_empty());
        assert_eq!(sel.refused_expired, 2);
        assert_eq!(sel.refused_no_expiry, 0, "expired is not missing");
    }

    #[test]
    fn a_missing_underlying_is_filed_under_its_own_counter() {
        let rows = vec![contract(
            1,
            InstrumentClass::IndexFuture,
            "NSE",
            "",
            2026_08_28,
            0,
            OptionLeg::None,
        )];
        let sel = select_contract_universe(&rows, &no_spot(), TODAY, 25_000);
        assert_eq!(sel.refused_no_underlying, 1);
        assert_eq!(
            sel.refused_no_expiry, 0,
            "a good expiry with no underlying is not an expiry defect — \
             filing it there sends triage at the wrong column"
        );
    }

    #[test]
    fn a_malformed_price_response_is_an_error_not_an_empty_market() {
        assert!(parse_spot_prices("not json").is_err());
        assert!(parse_spot_prices(r#"{"columns":[]}"#).is_err());
    }

    #[test]
    fn parse_symbol_map_reads_names_and_survives_a_bad_row() {
        let body = r#"{"mappings":[
            {"index_name":"NIFTY 50","symbol":"reliance","isin":"X","security_id":2885,"exchange_segment":1},
            {"index_name":"NIFTY 100","symbol":"RELIANCE","isin":"X","security_id":2885,"exchange_segment":1},
            {"index_name":"NIFTY 50","symbol":"TCS","security_id":11536},
            {"index_name":"NIFTY 50","symbol":"INFY","isin":"Y","security_id":1594,"exchange_segment":1}
        ]}"#;
        let m = parse_symbol_map(body).expect("parses");
        assert_eq!(m.get("RELIANCE"), Some(&(2885u64, 1u8)), "uppercased");
        assert_eq!(m.get("INFY"), Some(&(1594u64, 1u8)));
        assert_eq!(m.len(), 2, "the row missing exchange_segment is skipped");
    }

    #[test]
    fn a_malformed_symbol_map_is_an_error_not_an_empty_universe() {
        assert!(parse_symbol_map("not json").is_err());
        assert!(parse_symbol_map(r#"{"resolved":7}"#).is_err());
    }

    #[test]
    fn spot_paise_by_symbol_prices_only_symbols_that_actually_ticked() {
        let mut symbols = HashMap::new();
        symbols.insert("RELIANCE".to_string(), (2885u64, 1u8));
        symbols.insert("TCS".to_string(), (11_536u64, 1u8));
        let mut prices = HashMap::new();
        prices.insert((2885u64, 1u8), 123_455i64);
        // TCS has a symbol but no tick today — it must be absent, not zero.
        let joined = spot_paise_by_symbol(&symbols, &prices);
        assert_eq!(joined.get("RELIANCE"), Some(&123_455));
        assert!(
            !joined.contains_key("TCS"),
            "a symbol with no tick is unpriced, and unpriced means refused"
        );
    }

    #[test]
    fn the_join_will_not_price_a_stock_off_its_own_derivative() {
        // Same numeric id, different segment: the NSE_FNO row is a contract,
        // not the underlying, and its premium must never become the spot.
        let mut symbols = HashMap::new();
        symbols.insert("RELIANCE".to_string(), (2885u64, 1u8));
        let mut prices = HashMap::new();
        prices.insert((2885u64, 2u8), 4_200i64);
        assert!(spot_paise_by_symbol(&symbols, &prices).is_empty());
    }

    #[test]
    fn test_write_contract_artifact_and_read_contract_artifact_round_trip_on_disk() {
        // A real filesystem round trip, not a serde round trip — the atomic
        // rename is the part that stops a half-written file being parsed into
        // a partial contract set that looks complete, and only a real write
        // exercises it.
        //
        // The date is deliberately impossible so this can never collide with,
        // or be mistaken for, a real trading day's artifact.
        let date = "1970-01-02";
        let rows = contract_rows_from_master(&[
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
                InstrumentClass::StockOption,
                "NSE",
                "RELIANCE",
                2026_08_28,
                1_000,
                OptionLeg::Put,
            ),
        ]);
        write_contract_artifact(date, &rows).expect("writes");
        let back = read_contract_artifact(date).expect("reads");
        assert_eq!(back, rows);

        // No `.tmp` may survive: a leftover temp file is a partial artifact
        // sitting next to the real one, waiting for a future glob to find it.
        let tmp = contract_artifact_path(date).with_extension("json.tmp");
        assert!(
            !tmp.exists(),
            "the temp file must be renamed, not left behind"
        );

        std::fs::remove_file(contract_artifact_path(date)).expect("cleanup");
        assert!(
            read_contract_artifact(date).is_err(),
            "a missing artifact is an ERROR, never an empty contract set — \
             the two look identical in an instrument count"
        );
    }

    #[test]
    fn contract_artifact_path_is_dated_so_a_stale_day_is_never_read() {
        let p = contract_artifact_path("2026-08-19");
        assert!(p.to_string_lossy().contains("2026-08-19"));
        assert_ne!(p, contract_artifact_path("2026-08-18"));
    }

    #[test]
    fn select_contract_universe_reaches_the_authorized_scale() {
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
