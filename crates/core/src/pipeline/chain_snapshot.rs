//! In-RAM per-(feed, underlying) option-chain **moneyness snapshot** — the
//! DECISION SOURCE OF TRUTH for moneyness (operator directive 2026-07-14,
//! relayed via the coordinator session: moneyness lives in RAM; the
//! `moneyness` DB column on `option_chain_1m` / `option_contract_1m_rest`
//! is a write-only AUDIT MIRROR — no decision path ever reads the DB;
//! RAM-first per aws-budget.md rules 9/12 + banned-pattern Cat 10).
//!
//! ## Shape
//! A fixed 6-slot registry (`Feed::COUNT` 2 × [`ChainUnderlying::COUNT`] 3)
//! of `arc_swap::ArcSwap<ChainMoneynessSnapshot>`, house `OnceLock` pattern
//! (the retired `feed_presence::GLOBAL` precedent — registry deleted
//! 2026-07-18, stage-4 dead-producer sweep). Each
//! chain leg's per-minute fire PUBLISHES one whole snapshot (atomic
//! pointer swap — a reader can never see minute N's header with minute
//! N+1's rows); readers `load()` lock-free with ZERO allocation (the
//! arc-swap Guard fast path — the `TokenHandle` precedent, benched by
//! `token_handle/load` ≤ 50 ns).
//!
//! ## Write vs read (the honest envelope)
//! - WRITE ([`publish_chain_snapshot`]): cold path, once per (feed,
//!   underlying) per minute, inside the already-scheduled chain fire — it
//!   wraps the caller-built rows `Vec` in one `Arc` (the per-minute Vec is
//!   built by the CALLER in `crates/app`, the same cold-path allocation
//!   class as the parsed chain itself). The write MAY allocate — stated
//!   plainly, mirrors the seal-writer "write may allocate, read may not"
//!   split.
//! - READ ([`load_chain_snapshot`]): lock-free, O(1), zero-alloc — this
//!   module sits under `crates/core/src/pipeline/`, so the Cat-2 hot-path
//!   banned-pattern scan mechanically enforces the allocation-free body;
//!   DHAT (`crates/core/tests/dhat_moneyness.rs`) + Criterion
//!   (`moneyness/snapshot_read`) prove it.
//!
//! ## Staleness (§38.8 decision-freshness gate)
//! The snapshot is per-minute; a mid-outage read sees the LAST GOOD minute
//! — by design, never hidden. `groww-second-feed-scope-2026-06-19.md`
//! §38.8 (binding): "Any future strategy consumer … MUST fail closed on
//! staleness: a row whose retrieval was older than a configured freshness
//! threshold ⇒ NO trade for that minute." The mechanical hooks are
//! [`ChainMoneynessSnapshot::age_secs`] + [`ChainMoneynessSnapshot::is_empty_sentinel`]
//! + the `spot_missing` flag (a FRESH minute whose vendor omitted the spot
//! advertises "moneyness unusable this minute" instead of aging silently).
//! NO strategy consumer exists (§28 boundary untouched) — this API is the
//! read contract only.
//!
//! ## Memory envelope (bounded)
//! `SnapshotRow` = 24 B; hard row bound = `MAX_STRIKES_PER_CHAIN` (400) ×
//! 2 legs = 800 rows ≈ 19.2 KiB/snapshot worst case (observed live chains:
//! 99–189 strikes). 6 slots ≈ ~115 KiB steady state, ×2 generations while
//! a reader Guard pins the previous Arc ≈ ~230 KiB absolute worst — 0.003%
//! of the r8g.large headroom. No eviction, no config knob needed.

use std::sync::Arc;
use std::sync::OnceLock;

use arc_swap::ArcSwap;

use tickvault_common::feed::Feed;
use tickvault_common::moneyness::{Moneyness, OptionLeg};

/// Nanoseconds per second (age math).
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// The chain-leg underlyings — the SAME pinned 3-underlying set as
/// `CHAIN_1M_UNDERLYINGS` / `GROWW_CHAIN_1M_UNDERLYINGS` (INDIA VIX is
/// compile-time excluded from the chain legs — SPOT-ONLY per the
/// 2026-07-13 operator scope). Keyed by the cross-feed `underlying_symbol`
/// literals (the only key that joins Dhan↔Groww — the numeric ids live in
/// disjoint spaces: 13/25/51 vs FNV bit-62).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ChainUnderlying {
    /// NIFTY (Dhan IDX_I 13 / Groww `NSE-NIFTY`).
    Nifty = 0,
    /// BANKNIFTY (Dhan IDX_I 25 / Groww `NSE-BANKNIFTY`).
    Banknifty = 1,
    /// SENSEX (Dhan IDX_I 51 / Groww `BSE-SENSEX`).
    Sensex = 2,
}

impl ChainUnderlying {
    /// The single-source list (the `Feed::ALL` pattern).
    pub const ALL: &'static [ChainUnderlying] = &[
        ChainUnderlying::Nifty,
        ChainUnderlying::Banknifty,
        ChainUnderlying::Sensex,
    ];

    /// The number of chain underlyings — derived from [`ChainUnderlying::ALL`].
    pub const COUNT: usize = Self::ALL.len();

    /// Dense 0-based slot index (exhaustive match — a 4th underlying is a
    /// compile error at every site that forgot it).
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Nifty => 0,
            Self::Banknifty => 1,
            Self::Sensex => 2,
        }
    }

    /// The cross-feed plain symbol (`"NIFTY"`/`"BANKNIFTY"`/`"SENSEX"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nifty => "NIFTY",
            Self::Banknifty => "BANKNIFTY",
            Self::Sensex => "SENSEX",
        }
    }

    /// Resolve a chain underlying from the cross-feed plain symbol
    /// (case-sensitive; anything else — incl. INDIA VIX — is `None`).
    #[must_use]
    pub fn from_symbol(symbol: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|u| u.as_str() == symbol)
    }
}

/// One classified chain row held in RAM (24 bytes: i64 + i64 + u8 + u8,
/// padded). A consumer can re-derive any strike's class in O(1) via
/// `tickvault_common::moneyness::classify_moneyness_paise` against the
/// snapshot's anchor triple — the rows are the precomputed answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotRow {
    /// The vendor-parsed strike, integer paise.
    pub strike_paise: i64,
    /// The leg's last traded price, integer paise (0 = absent/invalid).
    pub ltp_paise: i64,
    /// CE / PE.
    pub leg: OptionLeg,
    /// The write-time classification (the same value stamped on the DB
    /// audit row).
    pub moneyness: Moneyness,
}

/// One whole per-(feed, underlying) minute snapshot — published atomically
/// (a reader never sees a torn header/rows pair).
#[derive(Clone, Debug)]
pub struct ChainMoneynessSnapshot {
    /// The producing feed.
    pub feed: Feed,
    /// The chain underlying.
    pub underlying: ChainUnderlying,
    /// The fired minute's MINUTE-OPEN, IST nanoseconds (0 = the empty
    /// boot sentinel — see [`Self::is_empty_sentinel`]). The staleness
    /// anchor for the §38.8 decision-freshness gate.
    pub minute_ts_ist_nanos: i64,
    /// Retrieval wall-clock instant, IST nanoseconds.
    pub fetched_at_ist_nanos: i64,
    /// The chain response's own underlying spot, raw f64 (display/audit
    /// parity with the DB `underlying_spot` column; 0.0 = vendor-absent).
    pub underlying_spot: f64,
    /// The guarded spot in integer paise (0 = missing/invalid — see
    /// `spot_missing`).
    pub underlying_spot_paise: i64,
    /// The grid-rounded ATM strike in paise (0 = unresolvable this
    /// minute: missing spot or unknown step).
    pub atm_strike_paise: i64,
    /// The chain's expiry — IST midnight nanoseconds of the expiry day.
    pub expiry_ist_nanos: i64,
    /// TRUE when the vendor omitted / zeroed the underlying spot this
    /// minute (Dhan silent 0.0 default OR Groww `underlying_ltp_missing`)
    /// — every row is then [`Moneyness::Unknown`] and a consumer MUST
    /// treat moneyness as unusable for the minute (fail-closed, §38.8).
    pub spot_missing: bool,
    /// Every classified leg row of the minute, in the parse order the
    /// chain fire produced (built by the CALLER on the cold path).
    pub rows: Vec<SnapshotRow>,
}

impl ChainMoneynessSnapshot {
    /// The pre-first-publish boot sentinel for a slot: minute ts 0, no
    /// rows, `spot_missing` — fail-closed by construction (a reader sees
    /// an "infinitely stale" empty snapshot, never a panic or an Option).
    #[must_use]
    pub fn empty_sentinel(feed: Feed, underlying: ChainUnderlying) -> Self {
        Self {
            feed,
            underlying,
            minute_ts_ist_nanos: 0,
            fetched_at_ist_nanos: 0,
            underlying_spot: 0.0,
            underlying_spot_paise: 0,
            atm_strike_paise: 0,
            expiry_ist_nanos: 0,
            spot_missing: true,
            rows: Vec::with_capacity(0),
        }
    }

    /// TRUE for the boot sentinel (no chain fire has published yet).
    #[must_use]
    pub const fn is_empty_sentinel(&self) -> bool {
        self.minute_ts_ist_nanos == 0
    }

    /// Snapshot age in whole seconds against the caller's IST-nanos "now"
    /// (saturating — a poisoned/backwards clock yields a negative age
    /// rather than a panic/overflow; the boot sentinel yields an enormous
    /// age). The §38.8 staleness input: a consumer MUST fail closed when
    /// this exceeds its freshness threshold.
    ///
    /// # Performance
    /// O(1), zero allocation.
    #[inline]
    #[must_use]
    pub const fn age_secs(&self, now_ist_nanos: i64) -> i64 {
        now_ist_nanos.saturating_sub(self.minute_ts_ist_nanos) / NANOS_PER_SEC
    }
}

/// Fixed slot count: 2 feeds × 3 underlyings.
const SLOT_COUNT: usize = Feed::COUNT * ChainUnderlying::COUNT;

/// Process-global registry (house `OnceLock` pattern — mirrors
/// the retired `feed_presence::GLOBAL`). Lazily seeded with empty
/// sentinels on first touch — no init-order failure class.
static SLOTS: OnceLock<[ArcSwap<ChainMoneynessSnapshot>; SLOT_COUNT]> = OnceLock::new();

/// Dense slot index for a (feed, underlying) pair — two integer ops.
#[must_use]
const fn slot_index(feed: Feed, underlying: ChainUnderlying) -> usize {
    feed.index() * ChainUnderlying::COUNT + underlying.index()
}

/// The lazily-initialized slot array (every slot starts as the empty
/// sentinel for its (feed, underlying) pair).
fn slots() -> &'static [ArcSwap<ChainMoneynessSnapshot>; SLOT_COUNT] {
    SLOTS.get_or_init(|| {
        std::array::from_fn(|i| {
            // Reverse the slot_index mapping: i = feed_idx * COUNT + u_idx.
            let feed = Feed::ALL[i / ChainUnderlying::COUNT];
            let underlying = ChainUnderlying::ALL[i % ChainUnderlying::COUNT];
            ArcSwap::from_pointee(ChainMoneynessSnapshot::empty_sentinel(feed, underlying))
        })
    })
}

/// Publish one whole minute snapshot (COLD path — once per (feed,
/// underlying) per minute from the chain leg's scheduled fire). The write
/// MAY allocate (one `Arc::new` here; the rows `Vec` was built by the
/// caller) — the documented honest envelope; the READ path stays
/// zero-alloc. Last-write-wins per slot (each slot has exactly one writer
/// task — the leg's own supervised loop).
pub fn publish_chain_snapshot(snapshot: ChainMoneynessSnapshot) {
    slots()[slot_index(snapshot.feed, snapshot.underlying)].store(Arc::new(snapshot));
}

/// Lock-free, O(1), ZERO-allocation read of the current snapshot — the
/// future decision surface (§28 boundary: no strategy consumer ships;
/// this is the read contract only). Returns the arc-swap `Guard` — deref
/// to `&ChainMoneynessSnapshot`; no Arc refcount clone on the fast path.
/// Total: before the first publish the slot holds the empty sentinel
/// (`is_empty_sentinel()`; §38.8 fail-closed staleness).
///
/// # Performance
/// O(1) — two integer ops + one arc-swap load (the `token_handle/load`
/// class, ≤ 50 ns budget; DHAT-ratcheted zero-alloc).
#[inline]
#[must_use]
pub fn load_chain_snapshot(
    feed: Feed,
    underlying: ChainUnderlying,
) -> arc_swap::Guard<Arc<ChainMoneynessSnapshot>> {
    slots()[slot_index(feed, underlying)].load()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot(
        feed: Feed,
        underlying: ChainUnderlying,
        minute_ts: i64,
    ) -> ChainMoneynessSnapshot {
        ChainMoneynessSnapshot {
            feed,
            underlying,
            minute_ts_ist_nanos: minute_ts,
            fetched_at_ist_nanos: minute_ts + 1_042_000_000,
            underlying_spot: 24_536.40,
            underlying_spot_paise: 2_453_640,
            atm_strike_paise: 2_455_000,
            expiry_ist_nanos: 1_770_508_800_000_000_000,
            spot_missing: false,
            rows: vec![
                SnapshotRow {
                    strike_paise: 2_450_000,
                    ltp_paise: 13_400,
                    leg: OptionLeg::Ce,
                    moneyness: Moneyness::Itm,
                },
                SnapshotRow {
                    strike_paise: 2_455_000,
                    ltp_paise: 9_800,
                    leg: OptionLeg::Ce,
                    moneyness: Moneyness::Atm,
                },
                SnapshotRow {
                    strike_paise: 2_460_000,
                    ltp_paise: 7_100,
                    leg: OptionLeg::Pe,
                    moneyness: Moneyness::Itm,
                },
            ],
        }
    }

    #[test]
    fn test_publish_then_load_roundtrip() {
        let snap = sample_snapshot(
            Feed::Dhan,
            ChainUnderlying::Nifty,
            1_770_000_900_000_000_000,
        );
        publish_chain_snapshot(snap.clone());
        let loaded = load_chain_snapshot(Feed::Dhan, ChainUnderlying::Nifty);
        assert!(!loaded.is_empty_sentinel());
        assert_eq!(loaded.minute_ts_ist_nanos, snap.minute_ts_ist_nanos);
        assert_eq!(loaded.atm_strike_paise, 2_455_000);
        assert_eq!(loaded.underlying_spot_paise, 2_453_640);
        assert_eq!(loaded.rows.len(), 3);
        assert_eq!(loaded.rows[1].moneyness, Moneyness::Atm);
        assert!(!loaded.spot_missing);
        // A later publish replaces the whole snapshot atomically.
        let newer = sample_snapshot(
            Feed::Dhan,
            ChainUnderlying::Nifty,
            1_770_000_960_000_000_000,
        );
        publish_chain_snapshot(newer);
        let reloaded = load_chain_snapshot(Feed::Dhan, ChainUnderlying::Nifty);
        assert_eq!(reloaded.minute_ts_ist_nanos, 1_770_000_960_000_000_000);
    }

    #[test]
    fn test_slot_isolation_across_feeds_and_underlyings() {
        // slot_index is a bijection over the 6 (feed, underlying) pairs.
        let mut seen = [false; SLOT_COUNT];
        for &feed in Feed::ALL {
            for &u in ChainUnderlying::ALL {
                let idx = slot_index(feed, u);
                assert!(idx < SLOT_COUNT, "slot index in range");
                assert!(!seen[idx], "slot index must be unique per pair");
                seen[idx] = true;
            }
        }
        assert!(seen.iter().all(|&s| s), "every slot reachable");

        // Publishing one (feed, underlying) never disturbs another slot.
        let groww_bn = sample_snapshot(
            Feed::Groww,
            ChainUnderlying::Banknifty,
            1_770_000_900_000_000_000,
        );
        publish_chain_snapshot(groww_bn);
        let loaded = load_chain_snapshot(Feed::Groww, ChainUnderlying::Banknifty);
        assert_eq!(loaded.minute_ts_ist_nanos, 1_770_000_900_000_000_000);
        // The Dhan BANKNIFTY slot and the Groww SENSEX slot are untouched
        // (still whatever they held — for SENSEX in this test binary the
        // boot sentinel).
        let sensex = load_chain_snapshot(Feed::Groww, ChainUnderlying::Sensex);
        assert!(sensex.is_empty_sentinel());
        assert_eq!(sensex.feed, Feed::Groww);
        assert_eq!(sensex.underlying, ChainUnderlying::Sensex);
    }

    #[test]
    fn test_empty_sentinel_before_first_publish() {
        // The Dhan SENSEX slot is never published in this test binary —
        // it must read as the fail-closed boot sentinel.
        let s = load_chain_snapshot(Feed::Dhan, ChainUnderlying::Sensex);
        assert!(s.is_empty_sentinel());
        assert!(s.spot_missing, "sentinel advertises unusable moneyness");
        assert!(s.rows.is_empty());
        assert_eq!(s.atm_strike_paise, 0);
        // An enormous age — any freshness threshold fails closed.
        let now = 1_770_000_900_000_000_000_i64;
        assert!(s.age_secs(now) >= now / NANOS_PER_SEC);
    }

    #[test]
    fn test_age_secs_boundaries() {
        let snap = sample_snapshot(
            Feed::Dhan,
            ChainUnderlying::Nifty,
            1_770_000_900_000_000_000,
        );
        // Same instant → 0.
        assert_eq!(snap.age_secs(1_770_000_900_000_000_000), 0);
        // One minute later → 60.
        assert_eq!(snap.age_secs(1_770_000_960_000_000_000), 60);
        // Sub-second later → still 0 (whole seconds).
        assert_eq!(snap.age_secs(1_770_000_900_999_999_999), 0);
        // Backwards clock (poisoned) → negative, never a panic.
        assert_eq!(snap.age_secs(1_770_000_840_000_000_000), -60);
        // Saturating at the extremes — no overflow panic.
        let _ = snap.age_secs(i64::MAX);
        let _ = snap.age_secs(i64::MIN);
    }

    #[test]
    fn test_from_symbol_exhaustive_and_reject() {
        for &u in ChainUnderlying::ALL {
            assert_eq!(ChainUnderlying::from_symbol(u.as_str()), Some(u));
        }
        assert_eq!(
            ChainUnderlying::from_symbol("NIFTY"),
            Some(ChainUnderlying::Nifty)
        );
        assert_eq!(
            ChainUnderlying::from_symbol("BANKNIFTY"),
            Some(ChainUnderlying::Banknifty)
        );
        assert_eq!(
            ChainUnderlying::from_symbol("SENSEX"),
            Some(ChainUnderlying::Sensex)
        );
        // INDIA VIX is SPOT-ONLY — never a chain underlying; case matters.
        assert_eq!(ChainUnderlying::from_symbol("INDIA VIX"), None);
        assert_eq!(ChainUnderlying::from_symbol("nifty"), None);
        assert_eq!(ChainUnderlying::from_symbol(""), None);
        assert_eq!(ChainUnderlying::COUNT, ChainUnderlying::ALL.len());
    }
}
