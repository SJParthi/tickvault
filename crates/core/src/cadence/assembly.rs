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
use super::schedule::{CADENCE_CROSS_FILL_FRESHNESS_FLOOR_MS, CycleSlots};
use crate::pipeline::chain_snapshot::{
    ChainMoneynessSnapshot, ChainUnderlying, load_chain_snapshot,
};

/// Max snapshot age (whole seconds vs the caller's IST-epoch-nanos "now")
/// a decide-time registry read may trust (design §6 guard fields). A
/// same-cycle snapshot is ≤ ~75s old at the latest Dhan cutoff (minute
/// open + 60s boundary + 15s cutoff); 120s adds slack while rejecting a
/// yesterday-same-minute stale slot (age ≈ 86,400s) outright.
pub const CADENCE_CHAIN_SNAPSHOT_MAX_AGE_SECS: i64 = 120;

/// Seconds per IST day (minute-of-day reduction of the registry's
/// IST-epoch-nanos minute stamp).
const SECS_PER_DAY: i64 = 86_400;

/// Nanoseconds per second.
const NANOS_PER_SEC: i64 = 1_000_000_000;

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
    /// The feed whose executor FETCHED (and published) this chain — the
    /// registry slot the decide-time fold must read. Equals the lane's
    /// own feed for an own fetch; the LENDER's feed for a cross-filled
    /// cell (design §3(e): the borrowed chain's rows live under the
    /// lender's registry slot, never the borrower's).
    pub source_feed: Feed,
    /// TRUE when the fetching executor confirmed it published the
    /// classified snapshot to the `chain_snapshot` registry for this
    /// (source_feed, underlying) — the decide-time fold refuses an
    /// unconfirmed publish (a stale prior-minute registry read is the
    /// exact silent-corruption class the design §6 guard fields exist to
    /// prevent).
    pub published_to_registry: bool,
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
    /// stamped `DecidedDegraded`, never hidden — cross-fill and
    /// chain-embedded fallbacks, design §5).
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
    /// same-cycle assembly, freshness-checked per [`cross_fill_fresh`]
    /// against the caller-supplied `freshness_floor_abs_ms` (the base
    /// floor from [`cross_fill_freshness_floor_ms`] — simplified
    /// 2026-07-16 with the pre-close schedule's retirement). Returns
    /// `(spots_filled, chains_filled)`.
    pub fn cross_fill_from(
        &mut self,
        other: &LaneAssembly,
        freshness_floor_abs_ms: i64,
        now_ms: i64,
        cutoff_abs_ms: i64,
    ) -> (u32, u32) {
        let mut spots_filled = 0;
        let mut chains_filled = 0;
        for u in ChainUnderlying::ALL {
            let i = u.index();
            // M4 (audit 2026-07-20, Dim B): a donor spot whose paise guard
            // FAILED (`spot_paise == 0` — invalid/absurd price) is refused;
            // borrowing it would mint a CrossSource cell that anchors every
            // strike Unknown-at-best (or wrong via the raw f64). The cell
            // stays empty — the honest skip/degrade path.
            if let (None, Some(foreign)) = (self.spots[i], other.spots[i])
                && foreign.spot_paise > 0
                && cross_fill_fresh(
                    foreign.minute_ist,
                    self.cycle_minute_ist,
                    foreign.fetched_at_ms,
                    freshness_floor_abs_ms,
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
                    freshness_floor_abs_ms,
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
/// cycle's minute AND it was fetched at/after `freshness_floor_abs_ms`
/// AND the borrow happens at/before the borrowing lane's cutoff. The
/// floor is the absolute instant from
/// [`cross_fill_freshness_floor_ms`]: the plain base T − 5000ms window
/// (2026-07-16 — every fire on both lanes is POST-close, so same-cycle
/// completions trivially pass; the floor belts against cross-CYCLE
/// staleness). Pure.
#[must_use]
pub fn cross_fill_fresh(
    foreign_minute_ist: u32,
    cycle_minute_ist: u32,
    foreign_fetched_at_ms: i64,
    freshness_floor_abs_ms: i64,
    now_ms: i64,
    borrowing_cutoff_abs_ms: i64,
) -> bool {
    foreign_minute_ist == cycle_minute_ist
        && foreign_fetched_at_ms >= freshness_floor_abs_ms
        && now_ms <= borrowing_cutoff_abs_ms
}

/// The advisory spot price-coherence band, basis points (H3/H2-partial/M14,
/// audit 2026-07-20): 50 bps = 0.5% — the OPTION-CHAIN-04 LTP-disagreement
/// tolerance precedent (`option-chain-cross-verify-error-codes.md` §1).
/// Two same-minute prices for one underlying differing beyond this band
/// signal vendor staleness, a wrong-instrument body, or cross-broker
/// divergence — ADVISORY (stage + counter), never decision-blocking.
pub const CADENCE_SPOT_DIVERGENCE_MAX_BPS: i64 = 50;

/// TRUE when two paise prices for the SAME (underlying, minute) diverge
/// beyond [`CADENCE_SPOT_DIVERGENCE_MAX_BPS`] relative to the SMALLER
/// operand (the conservative denominator — symmetric in its arguments).
/// Any non-positive operand returns `false`: an unresolved/guard-failed
/// price carries no verdict (its own Unknown/skip paths already surface
/// it). Integer-only saturating math — O(1), no float compare. Pure.
#[must_use]
pub fn spots_diverge_paise(a_paise: i64, b_paise: i64) -> bool {
    if a_paise <= 0 || b_paise <= 0 {
        return false;
    }
    let diff = (a_paise - b_paise).abs();
    let denom = a_paise.min(b_paise);
    // diff/denom > BPS/10_000  ⇔  diff * 10_000 > denom * BPS (denom > 0).
    diff.saturating_mul(10_000) > denom.saturating_mul(CADENCE_SPOT_DIVERGENCE_MAX_BPS)
}

/// The cross-fill freshness floor (absolute IST ms-of-day) for the
/// cycle described by `slots`: the plain base T − 5000ms
/// ([`CADENCE_CROSS_FILL_FRESHNESS_FLOOR_MS`]). SIMPLIFIED 2026-07-16
/// (coordinator addendum item 3): retiring the pre-close schedule + the
/// anchor-shift ladder removed the rung-shifted PRE-fires the
/// lender-aware widening (CADENCE-XFILL-RUNG-1, 2026-07-15) was built
/// around — every fire on both lanes is POST-close now, so any
/// same-cycle completion trivially satisfies the base floor and the
/// floor is a belt against cross-CYCLE staleness only. Pure.
#[must_use]
pub fn cross_fill_freshness_floor_ms(slots: &CycleSlots) -> i64 {
    slots
        .boundary_ms
        .saturating_sub(CADENCE_CROSS_FILL_FRESHNESS_FLOOR_MS)
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
    let guard = load_chain_snapshot(feed, underlying);
    let snap: &ChainMoneynessSnapshot = &guard;
    fold_snapshot_rows(snap, spot_paise, atm_paise)
}

/// The raw row fold shared by the guarded and unguarded entry points.
fn fold_snapshot_rows(
    snap: &ChainMoneynessSnapshot,
    spot_paise: i64,
    atm_paise: i64,
) -> MoneynessFold {
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

/// The decide-time registry guard (design §6 consumer contract): a
/// snapshot is usable for the cycle ONLY when it is not the boot
/// sentinel, its minute-of-day equals the decided cycle minute, and its
/// age against the caller's IST-epoch-nanos "now" sits inside
/// `[0, CADENCE_CHAIN_SNAPSHOT_MAX_AGE_SECS]` (a negative age = a
/// publisher clock ahead of ours — fail-closed; a yesterday-same-minute
/// slot fails the age bound). Pure.
#[must_use]
pub fn chain_snapshot_fresh_for_cycle(
    snap: &ChainMoneynessSnapshot,
    cycle_minute_ist: u32,
    now_ist_nanos: i64,
) -> bool {
    if snap.is_empty_sentinel() {
        return false;
    }
    let minute_of_day_secs = (snap.minute_ts_ist_nanos / NANOS_PER_SEC).rem_euclid(SECS_PER_DAY);
    if minute_of_day_secs != i64::from(cycle_minute_ist) {
        return false;
    }
    (0..=CADENCE_CHAIN_SNAPSHOT_MAX_AGE_SECS).contains(&snap.age_secs(now_ist_nanos))
}

/// The CHAIN-ROW moneyness anchor (R5, 2026-07-16): the chain's OWN
/// embedded underlying spot FIRST (same-response coherence — the
/// embedded `last_price` is same-instant with the rows it anchors),
/// the lane's resolved spot cell as the FALLBACK, Unknown-anchor
/// `(0, 0)` last. The OwnFetch spot serves the SPOT SERIES, not chain
/// moneyness. Either returned operand may be 0 (guard failed /
/// unresolvable step), in which case every row classifies Unknown —
/// SURFACED, never dropped (the total-classifier contract).
///
/// # Performance
/// O(1), zero allocation (one guarded paise conversion + one grid
/// round per call; cold decide-time path).
#[must_use]
pub fn chain_moneyness_anchor(
    underlying: ChainUnderlying,
    chain: Option<&ChainCell>,
    spot: Option<&SpotCell>,
) -> (i64, i64) {
    if let Some(embedded) = chain.and_then(|c| c.embedded_spot)
        && let Some(spot_paise) = price_to_paise_guarded(embedded)
        && spot_paise > 0
    {
        let atm_paise = strike_step_paise(underlying.as_str())
            .and_then(|step| atm_strike_paise(spot_paise, step))
            .unwrap_or(0);
        return (spot_paise, atm_paise);
    }
    // Fallback: the lane's resolved spot cell (own fetch / cross-fill /
    // chain-embedded fill) — paise + ATM were computed once at record
    // time; absent cell = the Unknown anchor.
    spot.map_or((0, 0), |s| (s.spot_paise, s.atm_paise))
}

/// GUARDED decide-time fold over the resolved chain cell (design §5/§6):
/// reads the registry slot of the cell's SOURCE feed (the lender's slot
/// for a cross-filled chain — the borrowed rows never live under the
/// borrower's slot), refuses an unconfirmed executor publish, and
/// refuses a stale / wrong-minute / sentinel snapshot via
/// [`chain_snapshot_fresh_for_cycle`]. Any refusal returns the default
/// fold (0 rows ⇒ `all_unknown` ⇒ SURFACED as unusable, never a silent
/// stale-row classification).
///
/// # Performance
/// O(rows) over the lock-free snapshot guard, zero allocation
/// (DHAT-ratcheted alongside the raw fold).
#[must_use]
pub fn fold_chain_cell_moneyness(
    cell: &ChainCell,
    underlying: ChainUnderlying,
    cycle_minute_ist: u32,
    now_ist_nanos: i64,
    spot_paise: i64,
    atm_paise: i64,
) -> MoneynessFold {
    if !cell.published_to_registry {
        return MoneynessFold::default();
    }
    let guard = load_chain_snapshot(cell.source_feed, underlying);
    let snap: &ChainMoneynessSnapshot = &guard;
    if !chain_snapshot_fresh_for_cycle(snap, cycle_minute_ist, now_ist_nanos) {
        return MoneynessFold::default();
    }
    fold_snapshot_rows(snap, spot_paise, atm_paise)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T_MS: i64 = 36_000_000; // 10:00:00 IST as ms-of-day
    const MINUTE: u32 = 35_940; // 09:59:00 — the decided minute

    fn asm(feed: Feed) -> LaneAssembly {
        LaneAssembly::new(feed, MINUTE, T_MS)
    }

    fn own_chain(feed: Feed, minute: u32, fetched_at: i64, embedded: Option<f64>) -> ChainCell {
        ChainCell {
            provenance: ChainProvenance::OwnFetch,
            source_feed: feed,
            published_to_registry: true,
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
            a.record_chain(*u, own_chain(Feed::Groww, MINUTE, T_MS + 100, None));
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

    /// The rung-0 base floor: T − 5000ms absolute.
    const BASE_FLOOR_MS: i64 = T_MS - CADENCE_CROSS_FILL_FRESHNESS_FLOOR_MS;

    #[test]
    fn test_cross_source_freshness_window_spans_the_base_floor() {
        // Data fetched at the floor edge (T−5000) IS fresh for the
        // borrowing lane's same cycle — the window deliberately keeps
        // the 5s slack even though every 2026-07-16 fire is post-close.
        assert!(cross_fill_fresh(
            MINUTE,
            MINUTE,
            T_MS - 5_000,
            BASE_FLOOR_MS,
            T_MS + 900,
            T_MS + 6_000
        ));
        // Groww's :00 spot (all fires are post-close) likewise.
        assert!(cross_fill_fresh(
            MINUTE,
            MINUTE,
            T_MS + 400,
            BASE_FLOOR_MS,
            T_MS + 4_500,
            T_MS + 15_000
        ));
        // One ms OLDER than the floor → stale, refused.
        assert!(!cross_fill_fresh(
            MINUTE,
            MINUTE,
            T_MS - 5_001,
            BASE_FLOOR_MS,
            T_MS + 900,
            T_MS + 6_000
        ));
        // Wrong minute → refused regardless of freshness.
        assert!(!cross_fill_fresh(
            MINUTE - 60,
            MINUTE,
            T_MS + 400,
            BASE_FLOOR_MS,
            T_MS + 900,
            T_MS + 6_000
        ));
        // Past the borrowing lane's cutoff → refused.
        assert!(!cross_fill_fresh(
            MINUTE,
            MINUTE,
            T_MS + 400,
            BASE_FLOOR_MS,
            T_MS + 6_001,
            T_MS + 6_000
        ));
    }

    #[test]
    fn test_cross_fill_freshness_floor_ms_is_the_plain_base_floor() {
        // 2026-07-16 (coordinator addendum item 3): the lender-aware
        // widening (CADENCE-XFILL-RUNG-1) retired with the pre-close
        // schedule — the floor is the PLAIN base T − 5000 for every
        // shape/tier permutation, and every POST-close completion
        // (fetched at/after its own scheduled fire ≥ T+0) trivially
        // passes it.
        use super::super::schedule::build_cycle_slots;
        use tickvault_common::config::CadenceConfig;
        let cfg = CadenceConfig::default();
        let boundary_secs = 10 * 3600; // 10:00:00 IST — matches T_MS.

        for shape in 0..=1u8 {
            for step in 0..=3u8 {
                let slots = build_cycle_slots(boundary_secs, shape, step, 0, &cfg);
                assert_eq!(
                    cross_fill_freshness_floor_ms(&slots),
                    BASE_FLOOR_MS,
                    "shape {shape} step {step}: the floor is the plain base"
                );
                // Every scheduled fire instant is post-close, so a
                // completion at its own slot always clears the floor.
                for slot in slots
                    .dhan_chain_slots_ms
                    .iter()
                    .chain(slots.dhan_spot_slots_ms.iter())
                {
                    assert!(*slot >= BASE_FLOOR_MS);
                }
            }
        }
        // A same-minute completion at T+900 is FRESH under the base
        // floor; a PREVIOUS-cycle leftover (fetched a minute earlier)
        // is refused — the cross-CYCLE staleness belt the floor exists
        // for.
        assert!(cross_fill_fresh(
            MINUTE,
            MINUTE,
            T_MS + 900,
            BASE_FLOOR_MS,
            T_MS + 1_000,
            T_MS + 6_000
        ));
        assert!(!cross_fill_fresh(
            MINUTE,
            MINUTE,
            T_MS - 59_000,
            BASE_FLOOR_MS,
            T_MS + 1_000,
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
            own_chain(Feed::Groww, MINUTE, T_MS + 300, Some(81_250.0)),
        );
        let (spots, chains) =
            borrower.cross_fill_from(&lender, BASE_FLOOR_MS, T_MS + 1_000, T_MS + 15_000);
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
        // A cross-filled chain KEEPS the lender's registry identity — the
        // decide-time fold must read the LENDER's slot, never the
        // borrower's (design §3(e)).
        assert!(
            borrower
                .chain(ChainUnderlying::Sensex)
                .is_some_and(|c| c.source_feed == Feed::Groww && c.published_to_registry)
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
        a.record_chain(
            ChainUnderlying::Nifty,
            own_chain(Feed::Dhan, MINUTE, T_MS, None),
        );
        assert!(a.chain(ChainUnderlying::Nifty).is_some());
    }

    #[test]
    fn test_cadence_assembly_record_chain_first_write_wins_is_data_complete_vix_missing() {
        let mut a = asm(Feed::Dhan);
        a.record_chain(
            ChainUnderlying::Nifty,
            own_chain(Feed::Dhan, MINUTE, T_MS - 5_000, None),
        );
        // A late duplicate (e.g. a post-latch cross-fill race) never
        // overwrites — first write wins, the duplicate is audit-only.
        a.record_chain(
            ChainUnderlying::Nifty,
            ChainCell {
                provenance: ChainProvenance::CrossSource,
                source_feed: Feed::Groww,
                published_to_registry: true,
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
            own_chain(Feed::Groww, MINUTE - 60, T_MS - 59_000, None),
        );
        assert_eq!(
            borrower.cross_fill_from(&lender, BASE_FLOOR_MS, T_MS + 1_000, T_MS + 15_000),
            (0, 0)
        );
        assert!(borrower.spot(ChainUnderlying::Nifty).is_none());
        assert!(borrower.chain(ChainUnderlying::Nifty).is_none());
    }

    #[test]
    fn cross_fill_refuses_nonpositive_donor_spot() {
        // M4 (audit 2026-07-20, Dim B): a donor spot whose paise guard
        // failed (spot_paise == 0) must NOT cross-fill — the borrower's
        // cell stays empty (honest skip), while a VALID donor still fills.
        let mut borrower = asm(Feed::Dhan);
        let mut lender = asm(Feed::Groww);
        // Guard-failed donor: non-finite/absurd price → spot_paise == 0.
        lender.record_spot(
            SpotTarget::Nifty,
            -1.0,
            SpotProvenance::OwnFetch,
            T_MS + 300,
            MINUTE,
        );
        assert!(
            lender
                .spot(ChainUnderlying::Nifty)
                .is_some_and(|s| s.spot_paise == 0),
            "precondition: the guard must have failed the donor price"
        );
        // Valid donor on a second underlying.
        lender.record_spot(
            SpotTarget::BankNifty,
            51_000.0,
            SpotProvenance::OwnFetch,
            T_MS + 300,
            MINUTE,
        );
        let (spots, chains) =
            borrower.cross_fill_from(&lender, BASE_FLOOR_MS, T_MS + 1_000, T_MS + 15_000);
        assert_eq!((spots, chains), (1, 0), "only the valid donor fills");
        assert!(
            borrower.spot(ChainUnderlying::Nifty).is_none(),
            "the paise-0 donor was refused"
        );
        assert!(
            borrower
                .spot(ChainUnderlying::Banknifty)
                .is_some_and(|s| s.provenance == SpotProvenance::CrossSource)
        );
    }

    #[test]
    fn spots_diverge_paise_band_boundaries() {
        // H3/H2-partial/M14 (audit 2026-07-20): 50 bps band, strictly-
        // greater fires, relative to the SMALLER operand, symmetric.
        let base = 2_450_000_i64; // NIFTY 24,500.00 in paise
        // Exactly at the band (0.5% of base = 12_250 paise): NOT diverged.
        assert!(!spots_diverge_paise(base, base + 12_250));
        assert!(!spots_diverge_paise(base + 12_250, base));
        // One paise beyond the band: diverged (both argument orders).
        assert!(spots_diverge_paise(base, base + 12_251));
        assert!(spots_diverge_paise(base + 12_251, base));
        // Equal prices: never diverged.
        assert!(!spots_diverge_paise(base, base));
        // A 2:1 split-class divergence: loudly diverged.
        assert!(spots_diverge_paise(base, base / 2));
        // Non-positive operands carry NO verdict.
        assert!(!spots_diverge_paise(0, base));
        assert!(!spots_diverge_paise(base, 0));
        assert!(!spots_diverge_paise(-1, base));
    }

    #[test]
    fn test_cadence_assembly_fill_spots_from_chain_embedded_zero_when_vendor_absent() {
        // Chains present but with NO embedded spot (the genuinely-optional
        // Groww absence) → rung 3 fills nothing, absence tracked upstream.
        let mut a = asm(Feed::Groww);
        for u in ChainUnderlying::ALL {
            a.record_chain(*u, own_chain(Feed::Groww, MINUTE, T_MS + 100, None));
        }
        assert_eq!(a.fill_spots_from_chain_embedded(T_MS + 900), 0);
        assert!(!a.is_data_complete());
    }

    /// IST-epoch nanos helper for the guard vectors: `day_base` days past
    /// epoch + `secs_of_day` (pure arithmetic — no chrono in the guard).
    const fn ist_nanos(days: i64, secs_of_day: i64) -> i64 {
        (days * SECS_PER_DAY + secs_of_day) * NANOS_PER_SEC
    }

    #[test]
    fn test_cadence_chain_snapshot_fresh_for_cycle_guards_sentinel_minute_and_age() {
        let mk = |minute_ts_ist_nanos: i64| ChainMoneynessSnapshot {
            minute_ts_ist_nanos,
            ..ChainMoneynessSnapshot::empty_sentinel(Feed::Dhan, ChainUnderlying::Nifty)
        };
        let day = 20_000_i64; // an arbitrary IST calendar day
        let minute_open = i64::from(MINUTE);
        // "now" = boundary + 900ms → age ≈ 60s: fresh.
        let now = ist_nanos(day, minute_open + 60);
        // The boot sentinel is NEVER fresh.
        assert!(!chain_snapshot_fresh_for_cycle(&mk(0), MINUTE, now));
        // Same-day matching minute inside the age bound: fresh.
        assert!(chain_snapshot_fresh_for_cycle(
            &mk(ist_nanos(day, minute_open)),
            MINUTE,
            now
        ));
        // The PREVIOUS minute's snapshot (the stale-registry class):
        // refused on the minute-of-day mismatch.
        assert!(!chain_snapshot_fresh_for_cycle(
            &mk(ist_nanos(day, minute_open - 60)),
            MINUTE,
            now
        ));
        // YESTERDAY's same-minute snapshot: minute-of-day matches but the
        // age bound refuses it.
        assert!(!chain_snapshot_fresh_for_cycle(
            &mk(ist_nanos(day - 1, minute_open)),
            MINUTE,
            now
        ));
        // A publisher clock AHEAD of ours (negative age): fail-closed.
        assert!(!chain_snapshot_fresh_for_cycle(
            &mk(ist_nanos(day, minute_open + 120)),
            MINUTE,
            now
        ));
        // Exactly AT the age bound is admitted (inclusive).
        assert!(chain_snapshot_fresh_for_cycle(
            &mk(ist_nanos(day, minute_open)),
            MINUTE,
            ist_nanos(day, minute_open + CADENCE_CHAIN_SNAPSHOT_MAX_AGE_SECS)
        ));
        // One second PAST the bound is refused.
        assert!(!chain_snapshot_fresh_for_cycle(
            &mk(ist_nanos(day, minute_open)),
            MINUTE,
            ist_nanos(day, minute_open + CADENCE_CHAIN_SNAPSHOT_MAX_AGE_SECS + 1)
        ));
    }

    #[test]
    fn test_cadence_fold_chain_cell_moneyness_refuses_unconfirmed_publish() {
        // published_to_registry = false ⇒ the fold NEVER touches the
        // registry — default fold (0 rows ⇒ all_unknown, SURFACED).
        let cell = ChainCell {
            provenance: ChainProvenance::OwnFetch,
            source_feed: Feed::Dhan,
            published_to_registry: false,
            fetched_at_ms: T_MS + 400,
            minute_ist: MINUTE,
            embedded_spot: None,
        };
        let day = 20_000_i64;
        let now = ist_nanos(day, i64::from(MINUTE) + 61);
        let fold = fold_chain_cell_moneyness(
            &cell,
            ChainUnderlying::Nifty,
            MINUTE,
            now,
            2_450_000,
            2_450_000,
        );
        assert_eq!(fold, MoneynessFold::default());
        assert!(fold.all_unknown(), "an unconfirmed publish is unusable");
    }

    #[test]
    fn test_chain_moneyness_anchor_prefers_embedded_spot_then_own_fetch_then_unknown() {
        // R5 (2026-07-16): chain rows anchor on the chain's OWN embedded
        // underlying spot FIRST — the OwnFetch spot cell serves the spot
        // series, not chain moneyness.
        let mut a = asm(Feed::Dhan);
        // OwnFetch spot at 24_500 vs a chain-embedded spot at 25_000 —
        // the anchor must be the EMBEDDED value (25_000.00 → 2_500_000
        // paise; NIFTY step 50_00 paise ⇒ ATM 2_500_000).
        a.record_spot(
            SpotTarget::Nifty,
            24_500.0,
            SpotProvenance::OwnFetch,
            T_MS + 200,
            MINUTE,
        );
        a.record_chain(
            ChainUnderlying::Nifty,
            own_chain(Feed::Dhan, MINUTE, T_MS + 100, Some(25_000.0)),
        );
        let (spot, atm) = chain_moneyness_anchor(
            ChainUnderlying::Nifty,
            a.chain(ChainUnderlying::Nifty),
            a.spot(ChainUnderlying::Nifty),
        );
        assert_eq!(spot, 2_500_000, "embedded spot wins over the OwnFetch cell");
        assert_eq!(atm, 2_500_000, "ATM derives from the EMBEDDED anchor");

        // Fallback: no embedded spot ⇒ the resolved spot cell's
        // record-time (paise, ATM).
        let mut b = asm(Feed::Dhan);
        b.record_spot(
            SpotTarget::Nifty,
            24_500.0,
            SpotProvenance::OwnFetch,
            T_MS + 200,
            MINUTE,
        );
        b.record_chain(
            ChainUnderlying::Nifty,
            own_chain(Feed::Dhan, MINUTE, T_MS + 100, None),
        );
        let (spot, atm) = chain_moneyness_anchor(
            ChainUnderlying::Nifty,
            b.chain(ChainUnderlying::Nifty),
            b.spot(ChainUnderlying::Nifty),
        );
        assert_eq!(spot, 2_450_000, "OwnFetch fallback when no embedded spot");
        assert_eq!(atm, 2_450_000);

        // An INVALID embedded spot (NaN — guard fails) also falls back,
        // never a poisoned anchor.
        let (spot, _) = chain_moneyness_anchor(
            ChainUnderlying::Nifty,
            Some(&own_chain(Feed::Dhan, MINUTE, T_MS + 100, Some(f64::NAN))),
            b.spot(ChainUnderlying::Nifty),
        );
        assert_eq!(spot, 2_450_000, "NaN embedded spot falls back to the cell");

        // Unknown LAST: no chain, no spot ⇒ the (0, 0) anchor — every
        // row then classifies Unknown (total, surfaced, never dropped).
        assert_eq!(
            chain_moneyness_anchor(ChainUnderlying::Nifty, None, None),
            (0, 0)
        );
    }
}
