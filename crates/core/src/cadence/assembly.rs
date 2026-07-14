//! Per-cycle per-broker-lane data store, the data-complete predicate,
//! provenance stamping, cross-source fill and the INLINE moneyness
//! resolution (design §5 + §6).
//!
//! Moneyness math is consumed EXACTLY from `tickvault_common::moneyness`
//! (#1540) — `price_to_paise_guarded` → `strike_step_paise` →
//! `atm_strike_paise` ONCE per (lane, underlying, cycle) the instant the
//! spot lands, then `classify_moneyness_paise` per row over the
//! `pipeline::chain_snapshot` registry snapshot. **This module implements
//! NO classification math.**

use tickvault_common::feed::Feed;
use tickvault_common::moneyness::{
    Moneyness, atm_strike_paise, price_to_paise_guarded, strike_step_paise,
};

use super::executor::SpotTarget;
use super::schedule::CADENCE_CROSS_FILL_FRESHNESS_FLOOR_MS;
use crate::pipeline::chain_snapshot::{ChainUnderlying, load_chain_snapshot};

/// Where a lane's per-underlying spot came from — the design §5
/// resolution order (each step past the first stamps the decision
/// degraded).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpotProvenance {
    /// The lane's own spot fetch.
    OwnFetch,
    /// The OTHER broker's same-cycle spot (cross-source fill).
    CrossSource,
    /// The lane's own chain-embedded underlying price (Dhan
    /// `data.last_price` / Groww `underlying_ltp`).
    ChainEmbedded,
}

impl SpotProvenance {
    /// Stable label for logs/counters.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OwnFetch => "own_fetch",
            Self::CrossSource => "cross_source",
            Self::ChainEmbedded => "chain_embedded",
        }
    }
}

/// Where a lane's per-underlying CHAIN came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainProvenance {
    /// The lane's own chain fetch.
    OwnFetch,
    /// The OTHER broker's same-cycle chain (cross-source fill).
    CrossSource,
}

/// One resolved spot cell (paise + ATM computed ONCE at record time).
#[derive(Clone, Copy, Debug)]
pub struct SpotCell {
    /// Raw fetched price, rupees.
    pub price: f64,
    /// Guarded integer paise (0 = guard failed — invalid/absurd price).
    pub spot_paise: i64,
    /// Grid-rounded ATM strike, paise (0 = unresolvable: bad spot or no
    /// step for the symbol — every row then classifies Unknown,
    /// SURFACED, never dropped).
    pub atm_paise: i64,
    /// Resolution provenance.
    pub provenance: SpotProvenance,
    /// Receipt instant, IST ms-of-day.
    pub fetched_at_ms: i64,
    /// The minute the price belongs to (MINUTE-OPEN, IST secs-of-day).
    pub minute_ist: u32,
}

/// One resolved chain cell.
#[derive(Clone, Copy, Debug)]
pub struct ChainCell {
    /// Resolution provenance.
    pub provenance: ChainProvenance,
    /// Receipt instant, IST ms-of-day.
    pub fetched_at_ms: i64,
    /// The minute the chain belongs to (MINUTE-OPEN, IST secs-of-day).
    pub minute_ist: u32,
    /// The chain response's own embedded underlying spot, rupees
    /// (rung 3 of the spot resolution order; `None` = vendor-absent).
    pub embedded_spot: Option<f64>,
}

/// The per-(lane, cycle) assembly. Fixed-size arrays indexed by
/// [`ChainUnderlying::index`] — cold-path per-cycle state, rebuilt each
/// minute (the per-cycle allocation-free reset is a `Default` overwrite).
#[derive(Debug)]
pub struct LaneAssembly {
    /// The lane (broker) this assembly belongs to.
    pub feed: Feed,
    /// The decided minute (MINUTE-OPEN, IST secs-of-day).
    pub cycle_minute_ist: u32,
    /// The cycle's minute-close boundary T, IST ms-of-day.
    pub boundary_ms: i64,
    /// Per-underlying chains.
    chains: [Option<ChainCell>; ChainUnderlying::COUNT],
    /// Per-underlying spots (NIFTY/BANKNIFTY/SENSEX).
    spots: [Option<SpotCell>; ChainUnderlying::COUNT],
    /// The advisory VIX spot (never blocks the predicate).
    vix: Option<SpotCell>,
}

impl LaneAssembly {
    /// A fresh, empty assembly for one (lane, cycle).
    #[must_use]
    pub fn new(feed: Feed, cycle_minute_ist: u32, boundary_ms: i64) -> Self {
        Self {
            feed,
            cycle_minute_ist,
            boundary_ms,
            chains: [None; ChainUnderlying::COUNT],
            spots: [None; ChainUnderlying::COUNT],
            vix: None,
        }
    }

    /// Record a resolved chain for `underlying` (first write wins per the
    /// provenance order — an own fetch is recorded before any cross-fill
    /// attempt can run, and a late duplicate is audit-only).
    pub fn record_chain(&mut self, underlying: ChainUnderlying, cell: ChainCell) {
        let slot = &mut self.chains[underlying.index()];
        if slot.is_none() {
            *slot = Some(cell);
        }
    }

    /// Record a spot for `target`, resolving paise + ATM ONCE at record
    /// time via `tickvault_common::moneyness` (zero local math). First
    /// write wins. VIX records into the advisory cell (no ATM — no chain,
    /// no step).
    pub fn record_spot(
        &mut self,
        target: SpotTarget,
        price: f64,
        provenance: SpotProvenance,
        fetched_at_ms: i64,
        minute_ist: u32,
    ) {
        let spot_paise = price_to_paise_guarded(price).unwrap_or(0);
        let atm_paise = match target.chain_underlying() {
            Some(u) if spot_paise > 0 => strike_step_paise(u.as_str())
                .and_then(|step| atm_strike_paise(spot_paise, step))
                .unwrap_or(0),
            _ => 0,
        };
        let cell = SpotCell {
            price,
            spot_paise,
            atm_paise,
            provenance,
            fetched_at_ms,
            minute_ist,
        };
        match target.chain_underlying() {
            Some(u) => {
                let slot = &mut self.spots[u.index()];
                if slot.is_none() {
                    *slot = Some(cell);
                }
            }
            None => {
                if self.vix.is_none() {
                    self.vix = Some(cell);
                }
            }
        }
    }

    /// The resolved chain for `underlying`, if any.
    #[must_use]
    pub fn chain(&self, underlying: ChainUnderlying) -> Option<&ChainCell> {
        self.chains[underlying.index()].as_ref()
    }

    /// The resolved spot for `underlying`, if any.
    #[must_use]
    pub fn spot(&self, underlying: ChainUnderlying) -> Option<&SpotCell> {
        self.spots[underlying.index()].as_ref()
    }

    /// The advisory VIX spot, if any.
    #[must_use]
    pub fn vix_spot(&self) -> Option<&SpotCell> {
        self.vix.as_ref()
    }

    /// The data-complete predicate (design §5, LOCKED; Assumed for
    /// operator confirm): all 3 chains + all 3 underlying spots. **VIX is
    /// ADVISORY** — fetched, published, stamped `vix_missing` when
    /// absent, NEVER blocks or skips a decision.
    #[must_use]
    pub fn is_data_complete(&self) -> bool {
        self.chains.iter().all(Option::is_some) && self.spots.iter().all(Option::is_some)
    }

    /// TRUE when the advisory VIX spot is absent (stamped on the
    /// decision, never blocking).
    #[must_use]
    pub fn vix_missing(&self) -> bool {
        self.vix.is_none()
    }

    /// TRUE when ANY resolved cell is non-own-provenance (the decision is
    /// stamped `DecidedDegraded`, never hidden — incl. the deliberate
    /// pre-close-chain ↔ post-close-spot mix, design §5).
    #[must_use]
    pub fn any_degraded_provenance(&self) -> bool {
        self.chains
            .iter()
            .flatten()
            .any(|c| c.provenance != ChainProvenance::OwnFetch)
            || self
                .spots
                .iter()
                .flatten()
                .any(|s| s.provenance != SpotProvenance::OwnFetch)
    }

    /// Third rung of the spot resolution order: resolve any still-missing
    /// underlying spot from the LANE'S OWN chain-embedded price. Returns
    /// how many spots were filled (each stamped
    /// [`SpotProvenance::ChainEmbedded`]).
    pub fn fill_spots_from_chain_embedded(&mut self, now_ms: i64) -> u32 {
        let mut filled = 0;
        for u in ChainUnderlying::ALL {
            if self.spots[u.index()].is_some() {
                continue;
            }
            let Some(chain) = self.chains[u.index()] else {
                continue;
            };
            let Some(embedded) = chain.embedded_spot else {
                continue;
            };
            let target = match u {
                ChainUnderlying::Nifty => SpotTarget::Nifty,
                ChainUnderlying::Banknifty => SpotTarget::BankNifty,
                ChainUnderlying::Sensex => SpotTarget::Sensex,
            };
            self.record_spot(
                target,
                embedded,
                SpotProvenance::ChainEmbedded,
                now_ms,
                chain.minute_ist,
            );
            filled += 1;
        }
        filled
    }

    /// Second rung: cross-fill still-missing cells from the OTHER lane's
    /// same-cycle assembly, freshness-checked per [`cross_fill_fresh`].
    /// Returns `(spots_filled, chains_filled)`.
    pub fn cross_fill_from(
        &mut self,
        other: &LaneAssembly,
        now_ms: i64,
        cutoff_abs_ms: i64,
    ) -> (u32, u32) {
        let mut spots_filled = 0;
        let mut chains_filled = 0;
        for u in ChainUnderlying::ALL {
            let i = u.index();
            if let (None, Some(foreign)) = (self.spots[i], other.spots[i])
                && cross_fill_fresh(
                    foreign.minute_ist,
                    self.cycle_minute_ist,
                    foreign.fetched_at_ms,
                    self.boundary_ms,
                    now_ms,
                    cutoff_abs_ms,
                )
            {
                self.spots[i] = Some(SpotCell {
                    provenance: SpotProvenance::CrossSource,
                    ..foreign
                });
                spots_filled += 1;
            }
            if let (None, Some(foreign)) = (self.chains[i], other.chains[i])
                && cross_fill_fresh(
                    foreign.minute_ist,
                    self.cycle_minute_ist,
                    foreign.fetched_at_ms,
                    self.boundary_ms,
                    now_ms,
                    cutoff_abs_ms,
                )
            {
                self.chains[i] = Some(ChainCell {
                    provenance: ChainProvenance::CrossSource,
                    ..foreign
                });
                chains_filled += 1;
            }
        }
        (spots_filled, chains_filled)
    }
}

/// The cross-fill freshness window, BOTH directions (design §5, LOCKED):
/// a foreign snapshot is valid iff its `minute_ts` equals the borrowing
/// cycle's minute AND it was fetched at/after T − 5000ms AND the borrow
/// happens at/before the borrowing lane's cutoff. The window deliberately
/// spans Dhan's PRE-close :55/:58 chains and Groww's POST-close :00
/// burst; any pre-close-chain ↔ post-close-spot mix is stamped
/// `DecidedDegraded`, never hidden. Pure.
#[must_use]
pub fn cross_fill_fresh(
    foreign_minute_ist: u32,
    cycle_minute_ist: u32,
    foreign_fetched_at_ms: i64,
    boundary_ms: i64,
    now_ms: i64,
    borrowing_cutoff_abs_ms: i64,
) -> bool {
    foreign_minute_ist == cycle_minute_ist
        && foreign_fetched_at_ms
            >= boundary_ms.saturating_sub(CADENCE_CROSS_FILL_FRESHNESS_FLOOR_MS)
        && now_ms <= borrowing_cutoff_abs_ms
}

/// One underlying's moneyness classification counts over the registry
/// snapshot rows (the DHAT-pinned zero-alloc decide read).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MoneynessFold {
    /// Rows classified in-the-money.
    pub itm: u32,
    /// Rows classified at-the-money.
    pub atm: u32,
    /// Rows classified out-of-the-money.
    pub otm: u32,
    /// Rows classified Unknown (invalid spot/ATM/strike — SURFACED).
    pub unknown: u32,
    /// Total rows folded.
    pub rows: u32,
}

impl MoneynessFold {
    /// TRUE when the fold saw rows and EVERY row classified Unknown
    /// (invalid spot ⇒ unusable moneyness for the minute, design §6) —
    /// OR when the anchor itself was unresolvable with no rows to show
    /// it (spot/ATM invalid + empty snapshot).
    #[must_use]
    pub fn all_unknown(&self) -> bool {
        self.rows == 0 || self.unknown == self.rows
    }
}

/// Fold the CURRENT registry snapshot for (feed, underlying) through
/// `classify_moneyness_paise` against a precomputed (spot, ATM) anchor
/// pair — the event-driven inline finalize (design §6).
///
/// # Performance
/// O(rows) over the lock-free snapshot guard, ZERO allocation
/// (DHAT-ratcheted by `crates/core/tests/dhat_cadence_decide.rs`); the
/// per-row classify is the #1540 O(1) integer compare.
#[must_use]
pub fn fold_chain_moneyness(
    feed: Feed,
    underlying: ChainUnderlying,
    spot_paise: i64,
    atm_paise: i64,
) -> MoneynessFold {
    let snap = load_chain_snapshot(feed, underlying);
    let mut fold = MoneynessFold::default();
    for row in &snap.rows {
        // classify_moneyness_paise is total: any operand < 1 (incl. an
        // unresolvable anchor passed through as 0) → Unknown, surfaced.
        let m = tickvault_common::moneyness::classify_moneyness_paise(
            row.leg.as_str(),
            row.strike_paise,
            spot_paise,
            atm_paise,
        );
        match m {
            Moneyness::Itm => fold.itm += 1,
            Moneyness::Atm => fold.atm += 1,
            Moneyness::Otm => fold.otm += 1,
            Moneyness::Unknown => fold.unknown += 1,
        }
        fold.rows += 1;
    }
    fold
}

#[cfg(test)]
mod tests {
    use super::*;

    const T_MS: i64 = 36_000_000; // 10:00:00 IST as ms-of-day
    const MINUTE: u32 = 35_940; // 09:59:00 — the decided minute

    fn asm(feed: Feed) -> LaneAssembly {
        LaneAssembly::new(feed, MINUTE, T_MS)
    }

    fn own_chain(minute: u32, fetched_at: i64, embedded: Option<f64>) -> ChainCell {
        ChainCell {
            provenance: ChainProvenance::OwnFetch,
            fetched_at_ms: fetched_at,
            minute_ist: minute,
            embedded_spot: embedded,
        }
    }

    #[test]
    fn test_cadence_assembly_predicate_3_chains_3_spots_vix_advisory() {
        let mut a = asm(Feed::Groww);
        assert!(!a.is_data_complete());
        for u in ChainUnderlying::ALL {
            a.record_chain(*u, own_chain(MINUTE, T_MS + 100, None));
        }
        assert!(!a.is_data_complete(), "chains alone are not complete");
        a.record_spot(
            SpotTarget::Nifty,
            24_500.0,
            SpotProvenance::OwnFetch,
            T_MS + 200,
            MINUTE,
        );
        a.record_spot(
            SpotTarget::BankNifty,
            51_000.0,
            SpotProvenance::OwnFetch,
            T_MS + 200,
            MINUTE,
        );
        assert!(!a.is_data_complete(), "2 of 3 spots is not complete");
        a.record_spot(
            SpotTarget::Sensex,
            81_000.0,
            SpotProvenance::OwnFetch,
            T_MS + 200,
            MINUTE,
        );
        // Complete WITHOUT the VIX — VIX is advisory, never blocking…
        assert!(a.is_data_complete());
        assert!(a.vix_missing(), "…and its absence is stamped");
        a.record_spot(
            SpotTarget::IndiaVix,
            13.5,
            SpotProvenance::OwnFetch,
            T_MS + 250,
            MINUTE,
        );
        assert!(!a.vix_missing());
        assert!(a.is_data_complete());
        // Own-provenance everywhere → not degraded.
        assert!(!a.any_degraded_provenance());
        // record_spot resolved paise + ATM once, via #1540 moneyness.
        let nifty = a.spot(ChainUnderlying::Nifty).copied();
        let cell = nifty.into_iter().next();
        assert!(cell.is_some_and(|c| c.spot_paise == 2_450_000 && c.atm_paise == 2_450_000));
        // VIX has no chain/step → no ATM (advisory only).
        assert!(a.vix_spot().is_some_and(|v| v.atm_paise == 0));
    }

    #[test]
    fn test_cross_source_freshness_window_pre_close_chain_post_close_spot() {
        // Dhan's :55 pre-close chain (fetched T−5000) IS fresh for the
        // borrowing lane's same cycle — the window deliberately spans it.
        assert!(cross_fill_fresh(
            MINUTE,
            MINUTE,
            T_MS - 5_000,
            T_MS,
            T_MS + 900,
            T_MS + 6_000
        ));
        // Groww's :00 post-close spot likewise.
        assert!(cross_fill_fresh(
            MINUTE,
            MINUTE,
            T_MS + 400,
            T_MS,
            T_MS + 4_500,
            T_MS + 15_000
        ));
        // One ms OLDER than T−5000 → stale, refused.
        assert!(!cross_fill_fresh(
            MINUTE,
            MINUTE,
            T_MS - 5_001,
            T_MS,
            T_MS + 900,
            T_MS + 6_000
        ));
        // Wrong minute → refused regardless of freshness.
        assert!(!cross_fill_fresh(
            MINUTE - 60,
            MINUTE,
            T_MS + 400,
            T_MS,
            T_MS + 900,
            T_MS + 6_000
        ));
        // Past the borrowing lane's cutoff → refused.
        assert!(!cross_fill_fresh(
            MINUTE,
            MINUTE,
            T_MS + 400,
            T_MS,
            T_MS + 6_001,
            T_MS + 6_000
        ));
    }

    #[test]
    fn test_spot_provenance_order_own_crossfill_chain_embedded() {
        // Rung 1: own fetch wins and is never overwritten.
        let mut own = asm(Feed::Dhan);
        own.record_spot(
            SpotTarget::Nifty,
            24_500.0,
            SpotProvenance::OwnFetch,
            T_MS + 100,
            MINUTE,
        );
        own.record_spot(
            SpotTarget::Nifty,
            99_999.0,
            SpotProvenance::CrossSource,
            T_MS + 200,
            MINUTE,
        );
        assert!(
            own.spot(ChainUnderlying::Nifty)
                .is_some_and(|s| s.provenance == SpotProvenance::OwnFetch
                    && (s.price - 24_500.0).abs() < f64::EPSILON)
        );

        // Rung 2: cross-fill from the other lane's same-cycle data.
        let mut borrower = asm(Feed::Dhan);
        let mut lender = asm(Feed::Groww);
        lender.record_spot(
            SpotTarget::BankNifty,
            51_000.0,
            SpotProvenance::OwnFetch,
            T_MS + 300,
            MINUTE,
        );
        lender.record_chain(
            ChainUnderlying::Sensex,
            own_chain(MINUTE, T_MS + 300, Some(81_250.0)),
        );
        let (spots, chains) = borrower.cross_fill_from(&lender, T_MS + 1_000, T_MS + 15_000);
        assert_eq!((spots, chains), (1, 1));
        assert!(
            borrower
                .spot(ChainUnderlying::Banknifty)
                .is_some_and(|s| s.provenance == SpotProvenance::CrossSource)
        );
        assert!(
            borrower
                .chain(ChainUnderlying::Sensex)
                .is_some_and(|c| c.provenance == ChainProvenance::CrossSource)
        );
        assert!(borrower.any_degraded_provenance());

        // Rung 3: chain-embedded fallback fills ONLY still-missing spots.
        let filled = borrower.fill_spots_from_chain_embedded(T_MS + 1_100);
        assert_eq!(filled, 1, "only SENSEX lacked a spot and had a chain");
        assert!(
            borrower
                .spot(ChainUnderlying::Sensex)
                .is_some_and(
                    |s| s.provenance == SpotProvenance::ChainEmbedded && s.spot_paise == 8_125_000
                )
        );
        // BANKNIFTY keeps its cross-source cell (embedded never
        // overwrites), NIFTY stays missing (no chain to embed from).
        assert!(borrower.spot(ChainUnderlying::Nifty).is_none());
    }

    #[test]
    fn test_cadence_record_spot_invalid_price_yields_zero_anchor() {
        // NaN / zero / negative spot → guarded paise 0, ATM 0 — every
        // downstream row classifies Unknown, SURFACED, never dropped.
        for bad in [f64::NAN, 0.0, -12.5, f64::INFINITY] {
            let mut a = asm(Feed::Dhan);
            a.record_spot(
                SpotTarget::Nifty,
                bad,
                SpotProvenance::OwnFetch,
                T_MS,
                MINUTE,
            );
            let cell = a.spot(ChainUnderlying::Nifty).copied();
            assert!(cell.is_some_and(|c| c.spot_paise == 0 && c.atm_paise == 0));
        }
    }

    #[test]
    fn test_cadence_fold_chain_moneyness_all_unknown_flag_and_accessors() {
        // fold_chain_moneyness against the (test-local) registry slot is
        // exercised end-to-end by the boundary tests in
        // crates/core/tests/cadence_zero_429_replay.rs (the registry is
        // process-global — unit tests here avoid cross-test interference)
        // — this test pins the pure MoneynessFold::all_unknown contract +
        // the chain/spot/vix_spot accessors.
        assert!(MoneynessFold::default().all_unknown(), "0 rows = unusable");
        let mixed = MoneynessFold {
            itm: 1,
            atm: 1,
            otm: 1,
            unknown: 0,
            rows: 3,
        };
        assert!(!mixed.all_unknown());
        let all_u = MoneynessFold {
            itm: 0,
            atm: 0,
            otm: 0,
            unknown: 5,
            rows: 5,
        };
        assert!(all_u.all_unknown());

        let mut a = asm(Feed::Dhan);
        assert!(a.chain(ChainUnderlying::Nifty).is_none());
        assert!(a.spot(ChainUnderlying::Nifty).is_none());
        assert!(a.vix_spot().is_none());
        a.record_chain(ChainUnderlying::Nifty, own_chain(MINUTE, T_MS, None));
        assert!(a.chain(ChainUnderlying::Nifty).is_some());
    }

    #[test]
    fn test_cadence_assembly_record_chain_first_write_wins_is_data_complete_vix_missing() {
        let mut a = asm(Feed::Dhan);
        a.record_chain(
            ChainUnderlying::Nifty,
            own_chain(MINUTE, T_MS - 5_000, None),
        );
        // A late duplicate (e.g. a post-latch cross-fill race) never
        // overwrites — first write wins, the duplicate is audit-only.
        a.record_chain(
            ChainUnderlying::Nifty,
            ChainCell {
                provenance: ChainProvenance::CrossSource,
                fetched_at_ms: T_MS + 900,
                minute_ist: MINUTE,
                embedded_spot: Some(1.0),
            },
        );
        assert!(
            a.chain(ChainUnderlying::Nifty)
                .is_some_and(|c| c.provenance == ChainProvenance::OwnFetch)
        );
        assert!(!a.is_data_complete(), "1 chain + 0 spots is incomplete");
        assert!(a.vix_missing(), "no VIX recorded yet");
    }

    #[test]
    fn test_cadence_assembly_vix_spot_never_gates_any_degraded_provenance() {
        // A VIX-only assembly: vix_spot is readable, the predicate stays
        // false (VIX advisory), and OwnFetch VIX never stamps degraded.
        let mut a = asm(Feed::Groww);
        a.record_spot(
            SpotTarget::IndiaVix,
            14.25,
            SpotProvenance::OwnFetch,
            T_MS + 100,
            MINUTE,
        );
        assert!(a.vix_spot().is_some_and(|v| v.atm_paise == 0));
        assert!(!a.is_data_complete());
        assert!(!a.any_degraded_provenance());
    }

    #[test]
    fn test_cadence_assembly_cross_fill_from_refuses_when_cross_fill_fresh_false() {
        // The lender's cells are from the WRONG minute — cross_fill_from
        // must fill NOTHING (cross_fill_fresh refuses on minute mismatch).
        let mut borrower = asm(Feed::Dhan);
        let mut lender = LaneAssembly::new(Feed::Groww, MINUTE - 60, T_MS - 60_000);
        lender.record_spot(
            SpotTarget::Nifty,
            24_500.0,
            SpotProvenance::OwnFetch,
            T_MS - 59_000,
            MINUTE - 60,
        );
        lender.record_chain(
            ChainUnderlying::Nifty,
            own_chain(MINUTE - 60, T_MS - 59_000, None),
        );
        assert_eq!(
            borrower.cross_fill_from(&lender, T_MS + 1_000, T_MS + 15_000),
            (0, 0)
        );
        assert!(borrower.spot(ChainUnderlying::Nifty).is_none());
        assert!(borrower.chain(ChainUnderlying::Nifty).is_none());
    }

    #[test]
    fn test_cadence_assembly_fill_spots_from_chain_embedded_zero_when_vendor_absent() {
        // Chains present but with NO embedded spot (the genuinely-optional
        // Groww absence) → rung 3 fills nothing, absence tracked upstream.
        let mut a = asm(Feed::Groww);
        for u in ChainUnderlying::ALL {
            a.record_chain(*u, own_chain(MINUTE, T_MS + 100, None));
        }
        assert_eq!(a.fill_spots_from_chain_embedded(T_MS + 900), 0);
        assert!(!a.is_data_complete());
    }
}
