//! Sub-PR #7 of 2026-05-27 daily-universe expansion — build the unified
//! daily universe by combining the F&O underlyings (Sub-PR #5) +
//! indices (Sub-PR #6) into a single `DailyUniverse` collection +
//! enforce the `[MIN_DAILY_UNIVERSE_SIZE, MAX_DAILY_UNIVERSE_SIZE]`
//! envelope per §2 + §22 of the rule file.
//!
//! **Feature-gated.** This module compiles only when the
//! `daily_universe_fetcher` cargo feature is enabled (per §21 of the
//! authoritative rule file). Under the current `Indices4Only` default
//! scope this module is dead code.
//!
//! ## Contract (§2 + §15 + §22 of the rule file)
//!
//! Input: outputs of Sub-PR #5 + Sub-PR #6 (typed extractions).
//!
//! Output (on success): `DailyUniverse` containing:
//! - `subscription_targets` — every instrument the WebSocket pool will
//!   subscribe to (~250 today: ~30 indices + ~218 F&O underlyings + 1
//!   BSE SENSEX, plus headroom). Each target carries its role
//!   (`Index` vs `FnoUnderlying`) so downstream dispatchers can
//!   classify.
//! - `total_count` — used by Sub-PR #11's boot-time-of-day guard for
//!   re-validation + by the `tv_universe_size` CloudWatch gauge
//!   (Sub-PR #12).
//!
//! ## Envelope enforcement (§2 + §22)
//!
//! `MIN_DAILY_UNIVERSE_SIZE = 100`, `MAX_DAILY_UNIVERSE_SIZE = 1200`
//! (constants from Sub-PR #2 in `tickvault-common`). Computed universe
//! sizes outside this envelope return `Err(UniverseSizeOutOfBounds)` —
//! fail-closed per §1 of the rule file. The orchestrator surfaces this
//! as a CRITICAL Telegram event + HALT boot.

#![cfg(feature = "daily_universe_fetcher")]

use thiserror::Error;
use tickvault_common::constants::{MAX_DAILY_UNIVERSE_SIZE, MIN_DAILY_UNIVERSE_SIZE};

use super::csv_parser::CsvRow;
use super::fno_underlying_extractor::FnoUnderlyingExtraction;
use super::index_extractor::IndexExtraction;

/// Role of an instrument in the daily universe.
///
/// Downstream consumers (subscription dispatcher in Sub-PR #10,
/// indicator pipeline, etc.) use this to classify how the instrument
/// should be subscribed + persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstrumentRole {
    /// `IDX_I` `INDEX` row — NSE indices + 1 BSE SENSEX (Sub-PR #6).
    Index,
    /// `NSE_EQ` `EQUITY` row referenced by a FUTSTK/OPTSTK as its
    /// `UNDERLYING_SECURITY_ID`. This is the PRIMARY tag for an equity that
    /// is an F&O underlying — whether or not it is ALSO an NTM constituent
    /// (the dual membership lives in [`SubscriptionTarget`]'s two flags).
    FnoUnderlying,
    /// `NSE_EQ` `EQUITY` row that is a NIFTY Total Market constituent but is
    /// NOT an F&O underlying (operator lock §31, 2026-06-06). Equities that
    /// are BOTH keep [`InstrumentRole::FnoUnderlying`] and set
    /// `is_index_constituent` — so these three primary classes stay mutually
    /// exclusive while the flags carry the lossless "and/or" membership.
    IndexConstituent,
    /// §36 (2026-07-08): one of the 4 nearest-expiry FUTIDX contracts
    /// (NIFTY/BANKNIFTY/MIDCPNIFTY = NSE_FNO, SENSEX = BSE_FNO) promoted
    /// into `subscription_targets` at build time. The planner derives the
    /// segment from `csv_row.segment` (NOT from the role — SENSEX is
    /// BSE_FNO). Selected once per trading date; NEVER rolls intraday.
    IndexFuture,
}

impl InstrumentRole {
    /// Stable wire-format label used by observability / persistence
    /// paths. The same string lands in the `instrument_lifecycle`
    /// table's `role` column (Sub-PR #9) and in CloudWatch metric
    /// dimensions (Sub-PR #12).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::FnoUnderlying => "fno_underlying",
            Self::IndexConstituent => "index_constituent",
            Self::IndexFuture => "index_future",
        }
    }
}

/// One instrument the WebSocket pool will subscribe to.
///
/// `role` is the mutually-exclusive PRIMARY classification; the two booleans
/// are the lossless membership set per §31.1(6) ("`fno_underlying` **and/or**
/// `index_constituent`"). A stock that is BOTH an F&O underlying AND an NTM
/// constituent carries `role == FnoUnderlying` with BOTH flags `true`. Invariants
/// (asserted by tests): `Index` ⇒ both flags false; `FnoUnderlying` ⇒
/// `is_fno_underlying`; `IndexConstituent` ⇒ `!is_fno_underlying && is_index_constituent`.
/// "F&O only" is the O(1) filter `is_fno_underlying`.
#[derive(Debug, Clone)]
pub struct SubscriptionTarget {
    pub role: InstrumentRole,
    /// `true` iff this equity is an F&O underlying (§31 item 5 O(1) filter).
    pub is_fno_underlying: bool,
    /// `true` iff this equity is a NIFTY Total Market constituent.
    pub is_index_constituent: bool,
    pub csv_row: CsvRow,
}

/// Unified daily universe — every instrument the live system tracks.
#[derive(Debug, Clone)]
pub struct DailyUniverse {
    /// The 331-SID set the WebSocket subscribes to (indices + F&O underlying
    /// spots). Bounded by `[MIN, MAX]_DAILY_UNIVERSE_SIZE`. The subscription
    /// dispatcher reads ONLY this — the 2-WebSocket lock is enforced here.
    pub subscription_targets: Vec<SubscriptionTarget>,
    /// Applicable F&O CONTRACT rows (FUTSTK/OPTSTK for resolved underlyings +
    /// FUTIDX/OPTIDX for tracked indices; currency/commodity excluded). These
    /// are persisted to the `instrument_lifecycle` master ONLY — they are
    /// NEVER subscribed (operator lock 2026-05-29 Quote 5, §5 of
    /// daily-universe-scope-expansion-2026-05-27.md). Not bounded by the
    /// subscription envelope (legitimately ~219K rows).
    pub fno_contracts: Vec<CsvRow>,
}

impl DailyUniverse {
    /// Count of SUBSCRIPTION instruments (indices + F&O underlyings). This is
    /// what the `[100,400]` envelope guard + the WS dispatcher use — it
    /// deliberately EXCLUDES `fno_contracts` (master-only). Sub-PR #11's
    /// boot-time-of-day guard + Sub-PR #12's CloudWatch gauge.
    #[must_use]
    pub fn total_count(&self) -> usize {
        self.subscription_targets.len()
    }

    /// Count of ALL instruments written to the `instrument_lifecycle` master:
    /// subscription targets + applicable F&O contracts. Used for the
    /// reconcile / audit observability (NOT the subscription envelope).
    #[must_use]
    pub fn lifecycle_count(&self) -> usize {
        self.subscription_targets.len() + self.fno_contracts.len()
    }

    /// Count of instruments by role — operator-facing observability.
    #[must_use]
    pub fn count_by_role(&self, role: InstrumentRole) -> usize {
        self.subscription_targets
            .iter()
            .filter(|t| t.role == role)
            .count()
    }

    /// O(1)-per-target count of F&O underlyings in the subscription — the
    /// operator's "F&O separately extractable" guarantee (§31 item 5). Counts
    /// the `is_fno_underlying` flag, so it INCLUDES the both-case (an F&O
    /// underlying that is also an NTM constituent), unlike `count_by_role`.
    #[must_use]
    pub fn fno_underlying_count(&self) -> usize {
        self.subscription_targets
            .iter()
            .filter(|t| t.is_fno_underlying)
            .count()
    }

    /// O(1)-per-target count of NIFTY Total Market constituents in the
    /// subscription — counts the `is_index_constituent` flag, INCLUDING the
    /// both-case (an NTM stock that is also an F&O underlying).
    #[must_use]
    pub fn index_constituent_count(&self) -> usize {
        self.subscription_targets
            .iter()
            .filter(|t| t.is_index_constituent)
            .count()
    }

    /// Build the always-on (no market-hours filter) exemption set —
    /// operator lock 2026-06-01 §30. Scans the subscription targets for
    /// GIFT Nifty (`GIFTNIFTY`, an NSE-IX index that trades ~21 h/day)
    /// and returns its `(security_id, exchange_segment_code)`. The result
    /// is installed once at boot via
    /// `tickvault_common::always_on::init_always_on_segments` and read by
    /// the tick processor + candle aggregator so GIFT ticks/candles are
    /// NOT dropped outside 09:15–15:30 IST.
    ///
    /// Returns an EMPTY set when GIFT Nifty is absent (cold day / Dhan
    /// dropped it) — the caller's loud allowlist-miss warn already covers
    /// the visibility; the exemption simply does nothing in that case.
    ///
    /// O(1) EXEMPT: cold-path, runs once per boot over ~250 targets.
    #[must_use]
    pub fn always_on_segments(&self) -> std::collections::HashSet<(u64, u8)> {
        use super::index_extractor::{GIFT_NIFTY_SYMBOL, normalize_index_symbol};
        let mut set = std::collections::HashSet::new();
        for t in &self.subscription_targets {
            if t.role != InstrumentRole::Index {
                continue;
            }
            if normalize_index_symbol(&t.csv_row.symbol_name) != GIFT_NIFTY_SYMBOL {
                continue;
            }
            // GIFT Nifty row found — resolve sid + numeric segment code.
            // Rule 11 (no silent false-OK): if a found GIFT row can't resolve,
            // the exemption is silently empty → GIFT ticks wrongly dropped
            // outside 09:15–15:30. Surface it loudly instead.
            let Ok(sid) = t.csv_row.security_id.trim().parse::<u64>() else {
                tracing::warn!(
                    security_id = %t.csv_row.security_id,
                    "always-on: GIFT Nifty row found but security_id did not parse as u64 — \
                     market-hours exemption will be EMPTY for GIFT this session"
                );
                continue;
            };
            let Some(code) = tickvault_common::segment::segment_str_to_code(&t.csv_row.segment)
            else {
                tracing::warn!(
                    sid,
                    segment = %t.csv_row.segment,
                    "always-on: GIFT Nifty segment unrecognised — exemption EMPTY for GIFT"
                );
                continue;
            };
            set.insert((sid, code));
        }
        set
    }
}

/// Errors that can occur during universe assembly.
#[derive(Debug, Error)]
pub enum BuildError {
    /// Computed universe size outside `[MIN_DAILY_UNIVERSE_SIZE,
    /// MAX_DAILY_UNIVERSE_SIZE]`. Per §2 + §22 — boot HALTS.
    ///
    /// Fail-closed: a too-small universe (e.g. <100) means an upstream
    /// extractor returned partial data; a too-large universe (>1200)
    /// means an upstream regression let in extra rows (e.g. BSE
    /// non-SENSEX, or commodity F&O). Either way, refuse to proceed.
    #[error("computed universe size {actual} outside envelope [{min}, {max}]")]
    UniverseSizeOutOfBounds {
        actual: usize,
        min: usize,
        max: usize,
    },
}

/// Combine the F&O underlying + index extractions into a single
/// `DailyUniverse`.
///
/// Order within `subscription_targets`:
/// 1. Indices first (NSE indices → BSE SENSEX last), then
/// 2. F&O underlyings (insertion order from the extractor).
///
/// This ordering is deterministic per input — needed for the §9 audit
/// trail (the operator can SELECT the universe at any point in time
/// and reason about subscription dispatch order).
///
/// # Errors
///
/// See [`BuildError::UniverseSizeOutOfBounds`].
pub fn build_daily_universe(
    indices: IndexExtraction,
    fno: FnoUnderlyingExtraction,
    fno_contracts: Vec<CsvRow>,
    ntm_constituents: Vec<CsvRow>,
    index_futures: Vec<CsvRow>,
) -> Result<DailyUniverse, BuildError> {
    let mut subscription_targets: Vec<SubscriptionTarget> = Vec::new();

    // Pass 1 — NSE indices.
    for row in indices.nse_indices {
        subscription_targets.push(SubscriptionTarget {
            role: InstrumentRole::Index,
            is_fno_underlying: false,
            is_index_constituent: false,
            csv_row: row,
        });
    }

    // Pass 2 — BSE SENSEX (after NSE for deterministic order).
    if let Some(sensex) = indices.bse_sensex {
        subscription_targets.push(SubscriptionTarget {
            role: InstrumentRole::Index,
            is_fno_underlying: false,
            is_index_constituent: false,
            csv_row: sensex,
        });
    }

    // Pass 3 — F&O underlyings (resolved via the NSE_EQ lookup the
    // Sub-PR #5 extractor already built; insertion order is the
    // HashMap iteration order, which is deterministic per Rust's
    // documented `HashMap` behaviour with a fixed seed).
    // Resolve every unique underlying SID to its full NSE_EQ row.
    for sid in &fno.unique_underlying_ids {
        if let Some(row) = fno.nse_eq_lookup.get(sid) {
            subscription_targets.push(SubscriptionTarget {
                role: InstrumentRole::FnoUnderlying,
                is_fno_underlying: true,
                is_index_constituent: false,
                csv_row: row.clone(),
            });
        }
        // SID with no NSE_EQ row should be IMPOSSIBLE — the Sub-PR #5
        // extractor only inserts a SID into `unique_underlying_ids`
        // AFTER confirming `nse_eq_lookup.contains_key(sid)`. Defensive
        // skip (don't panic).
    }

    // Pass 4 — NTM constituents (operator lock §31, 2026-06-06).
    //
    // `ntm_constituents` are already-resolved NSE_EQ EQUITY rows (the SID→row
    // bridge from `constituent_resolver` is Sub-PR #10's job). Union them in,
    // deduped by `(security_id, exchange_segment)` (I-P1-11):
    //  * a constituent whose `(sid, NSE_EQ)` matches an existing target (an F&O
    //    underlying pushed in Pass 3) → flag that target `is_index_constituent`
    //    (the lossless "both" case, NO duplicate row);
    //  * otherwise → push a new `IndexConstituent` target.
    //
    // O(N) cold-path build of the index over the equity targets pushed so far;
    // O(1) lookup per constituent. NOT a hot path.
    if !ntm_constituents.is_empty() {
        use std::collections::HashMap;
        let mut by_key: HashMap<(String, String), usize> = HashMap::new();
        for (idx, t) in subscription_targets.iter().enumerate() {
            // Only equity targets can collide with a constituent; index values
            // are IDX_I and keyed separately, so they never match an NSE_EQ key.
            by_key.insert(
                (t.csv_row.security_id.clone(), t.csv_row.segment.clone()),
                idx,
            );
        }
        for row in ntm_constituents {
            // §31.1 contract: constituents are pre-resolved NSE_EQ EQUITY rows.
            // Fail-closed against a future Sub-PR #10 wiring bug — a non-NSE_EQ
            // row could otherwise collide on the string key or be mis-routed as
            // an equity by the subscription planner. Skip + warn, never subscribe.
            if row.segment != "NSE_EQ" {
                tracing::warn!(
                    security_id = %row.security_id,
                    segment = %row.segment,
                    "NTM fold: dropping non-NSE_EQ constituent row (contract violation)"
                );
                continue;
            }
            let key = (row.security_id.clone(), row.segment.clone());
            if let Some(&idx) = by_key.get(&key) {
                // Existing target (F&O underlying OR an already-folded
                // constituent) — set the constituent flag, no new row.
                subscription_targets[idx].is_index_constituent = true;
            } else {
                let new_idx = subscription_targets.len();
                subscription_targets.push(SubscriptionTarget {
                    role: InstrumentRole::IndexConstituent,
                    is_fno_underlying: false,
                    is_index_constituent: true,
                    csv_row: row,
                });
                by_key.insert(key, new_idx);
            }
        }
    }

    // Pass 5 — §36 (2026-07-08): promote the ≤4 nearest-expiry FUTIDX rows
    // into `subscription_targets`. The chosen rows ALSO stay in
    // `fno_contracts` (master path untouched); dedup here is on the
    // composite `(security_id, segment)` key (I-P1-11) against everything
    // pushed so far AND within the futures set itself. Runs BEFORE the
    // envelope check (+4 is trivially inside `[100, 1200]`). Empty input
    // (spot-only rollback pin) ⇒ byte-identical prior behavior.
    if !index_futures.is_empty() {
        use std::collections::HashSet;
        let mut seen: HashSet<(String, String)> = subscription_targets
            .iter()
            .map(|t| (t.csv_row.security_id.clone(), t.csv_row.segment.clone()))
            .collect();
        for row in index_futures {
            let key = (row.security_id.clone(), row.segment.clone());
            if !seen.insert(key) {
                continue;
            }
            subscription_targets.push(SubscriptionTarget {
                role: InstrumentRole::IndexFuture,
                is_fno_underlying: false,
                is_index_constituent: false,
                csv_row: row,
            });
        }
    }

    let total = subscription_targets.len();
    if !(MIN_DAILY_UNIVERSE_SIZE..=MAX_DAILY_UNIVERSE_SIZE).contains(&total) {
        return Err(BuildError::UniverseSizeOutOfBounds {
            actual: total,
            min: MIN_DAILY_UNIVERSE_SIZE,
            max: MAX_DAILY_UNIVERSE_SIZE,
        });
    }

    Ok(DailyUniverse {
        subscription_targets,
        fno_contracts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn nse_eq_row(security_id: &str, symbol: &str) -> CsvRow {
        CsvRow {
            security_id: security_id.to_string(),
            exch_id: "NSE".to_string(),
            segment: "NSE_EQ".to_string(),
            instrument: "EQUITY".to_string(),
            symbol_name: symbol.to_string(),
            underlying_security_id: String::new(),
            ..Default::default()
        }
    }

    fn target_index(row: CsvRow) -> SubscriptionTarget {
        SubscriptionTarget {
            role: InstrumentRole::Index,
            is_fno_underlying: false,
            is_index_constituent: false,
            csv_row: row,
        }
    }

    fn target_fno(row: CsvRow) -> SubscriptionTarget {
        SubscriptionTarget {
            role: InstrumentRole::FnoUnderlying,
            is_fno_underlying: true,
            is_index_constituent: false,
            csv_row: row,
        }
    }

    fn idx_i_row(security_id: &str, exch_id: &str, symbol: &str) -> CsvRow {
        CsvRow {
            security_id: security_id.to_string(),
            exch_id: exch_id.to_string(),
            segment: "IDX_I".to_string(),
            instrument: "INDEX".to_string(),
            symbol_name: symbol.to_string(),
            underlying_security_id: String::new(),
            ..Default::default()
        }
    }

    fn make_fno(count: usize) -> FnoUnderlyingExtraction {
        let mut unique_underlying_ids = HashSet::new();
        let mut nse_eq_lookup = HashMap::new();
        for i in 0..count {
            let sid = format!("U{i:04}");
            unique_underlying_ids.insert(sid.clone());
            nse_eq_lookup.insert(sid.clone(), nse_eq_row(&sid, &format!("STK{i}")));
        }
        FnoUnderlyingExtraction {
            unique_underlying_ids,
            nse_eq_lookup,
            dangling_count: 0,
            total_derivative_count: count,
        }
    }

    fn make_indices(nse_count: usize) -> IndexExtraction {
        let nse_indices: Vec<CsvRow> = (0..nse_count)
            .map(|i| idx_i_row(&format!("I{i}"), "NSE", &format!("IDX{i}")))
            .collect();
        IndexExtraction {
            nse_indices,
            bse_sensex: Some(idx_i_row("51", "BSE", "SENSEX")),
            allowlist_misses: Vec::new(),
        }
    }

    #[test]
    fn always_on_segments_resolves_gift_nifty() {
        // Operator lock 2026-06-01 §30: GIFT Nifty (sid 5024, IDX_I=0) is
        // the only always-on instrument.
        let universe = DailyUniverse {
            subscription_targets: vec![
                target_index(idx_i_row("13", "NSE", "NIFTY")),
                target_index(idx_i_row("5024", "NSE", "GIFTNIFTY")),
                target_fno(nse_eq_row("2885", "RELIANCE")),
            ],
            fno_contracts: Vec::new(),
        };
        let set = universe.always_on_segments();
        assert_eq!(set.len(), 1, "only GIFT Nifty is always-on");
        assert!(set.contains(&(5024, 0)), "GIFT Nifty (IDX_I=0) exempt");
        assert!(!set.contains(&(13, 0)), "NIFTY is NOT exempt");
    }

    #[test]
    fn always_on_segments_empty_without_gift_nifty() {
        let universe = DailyUniverse {
            subscription_targets: vec![target_index(idx_i_row("13", "NSE", "NIFTY"))],
            fno_contracts: Vec::new(),
        };
        assert!(
            universe.always_on_segments().is_empty(),
            "no GIFT Nifty → empty exemption set"
        );
    }

    #[test]
    fn builds_universe_with_indices_and_underlyings() {
        // 30 indices + 1 SENSEX + 219 underlyings = 250 total.
        let indices = make_indices(30);
        let fno = make_fno(219);
        let universe =
            build_daily_universe(indices, fno, Vec::new(), Vec::new(), Vec::new()).expect("build");
        assert_eq!(universe.total_count(), 250);
        assert_eq!(universe.count_by_role(InstrumentRole::Index), 31);
        assert_eq!(universe.count_by_role(InstrumentRole::FnoUnderlying), 219);
    }

    #[test]
    fn lifecycle_count_adds_contracts_total_count_excludes_them() {
        // 30 indices + 1 SENSEX + 100 underlyings = 131 subscription targets.
        let universe = build_daily_universe(
            make_indices(30),
            make_fno(100),
            vec![nse_eq_row("49081", "NIFTY26JUN24000CE")],
            Vec::new(),
            Vec::new(),
        )
        .expect("build");
        // total_count = SUBSCRIPTION only (the [100,400] envelope basis).
        assert_eq!(universe.total_count(), 131);
        // lifecycle_count = subscription + the master-only contract.
        assert_eq!(universe.lifecycle_count(), 132);
    }

    #[test]
    fn rejects_universe_below_min_size() {
        // 30 indices + 1 SENSEX + 50 underlyings = 81, below MIN=100.
        let indices = make_indices(30);
        let fno = make_fno(50);
        let result = build_daily_universe(indices, fno, Vec::new(), Vec::new(), Vec::new());
        assert!(matches!(
            result,
            Err(BuildError::UniverseSizeOutOfBounds {
                actual: 81,
                min: 100,
                max: 1200
            })
        ));
    }

    #[test]
    fn rejects_universe_above_max_size() {
        // 30 indices + 1 SENSEX + 1170 underlyings = 1201, above MAX=1200.
        let indices = make_indices(30);
        let fno = make_fno(1170);
        let result = build_daily_universe(indices, fno, Vec::new(), Vec::new(), Vec::new());
        assert!(matches!(
            result,
            Err(BuildError::UniverseSizeOutOfBounds {
                actual: 1201,
                min: 100,
                max: 1200
            })
        ));
    }

    #[test]
    fn accepts_universe_exactly_at_min_size() {
        // 30 indices + 1 SENSEX + 69 underlyings = 100, exactly MIN.
        let indices = make_indices(30);
        let fno = make_fno(69);
        let universe = build_daily_universe(indices, fno, Vec::new(), Vec::new(), Vec::new())
            .expect("at MIN accepts");
        assert_eq!(universe.total_count(), 100);
    }

    #[test]
    fn accepts_universe_exactly_at_max_size() {
        // 30 indices + 1 SENSEX + 1169 underlyings = 1200, exactly MAX.
        let indices = make_indices(30);
        let fno = make_fno(1169);
        let universe = build_daily_universe(indices, fno, Vec::new(), Vec::new(), Vec::new())
            .expect("at MAX accepts");
        assert_eq!(universe.total_count(), 1200);
    }

    #[test]
    fn indices_appear_before_underlyings_in_order() {
        let indices = make_indices(30);
        let fno = make_fno(69);
        let universe =
            build_daily_universe(indices, fno, Vec::new(), Vec::new(), Vec::new()).expect("build");

        // First 30 are NSE indices, then BSE SENSEX, then underlyings.
        for (i, t) in universe.subscription_targets.iter().take(31).enumerate() {
            assert_eq!(t.role, InstrumentRole::Index, "target[{i}] should be Index");
        }
        for (i, t) in universe.subscription_targets.iter().skip(31).enumerate() {
            assert_eq!(
                t.role,
                InstrumentRole::FnoUnderlying,
                "target[{}] should be FnoUnderlying",
                i + 31
            );
        }
    }

    #[test]
    fn bse_sensex_appears_after_nse_indices() {
        let indices = make_indices(30);
        let fno = make_fno(69);
        let universe =
            build_daily_universe(indices, fno, Vec::new(), Vec::new(), Vec::new()).expect("build");

        // Index target #30 (0-indexed) should be SENSEX.
        let sensex = &universe.subscription_targets[30];
        assert_eq!(sensex.csv_row.symbol_name, "SENSEX");
        assert_eq!(sensex.csv_row.exch_id, "BSE");
    }

    #[test]
    fn count_by_role_returns_correct_counts() {
        let indices = make_indices(30);
        let fno = make_fno(150);
        let universe =
            build_daily_universe(indices, fno, Vec::new(), Vec::new(), Vec::new()).expect("build");
        assert_eq!(universe.count_by_role(InstrumentRole::Index), 31);
        assert_eq!(universe.count_by_role(InstrumentRole::FnoUnderlying), 150);
        // Sum equals total.
        assert_eq!(
            universe.count_by_role(InstrumentRole::Index)
                + universe.count_by_role(InstrumentRole::FnoUnderlying),
            universe.total_count()
        );
    }

    #[test]
    fn instrument_role_as_str_is_stable_wire_format() {
        assert_eq!(InstrumentRole::Index.as_str(), "index");
        assert_eq!(InstrumentRole::FnoUnderlying.as_str(), "fno_underlying");
        assert_eq!(
            InstrumentRole::IndexConstituent.as_str(),
            "index_constituent"
        );
        assert_eq!(InstrumentRole::IndexFuture.as_str(), "index_future");
    }

    #[test]
    fn dangling_sid_skipped_defensively_does_not_count_in_universe() {
        // Synthetic: unique_underlying_ids has a SID that nse_eq_lookup
        // does NOT have. This SHOULD be impossible by construction —
        // the extractor only inserts into the set AFTER confirming the
        // lookup contains the SID — but defend against future
        // regressions by skipping rather than panicking.
        let mut unique_underlying_ids = HashSet::new();
        unique_underlying_ids.insert("GHOST_SID".to_string());
        let nse_eq_lookup: HashMap<String, CsvRow> = HashMap::new(); // empty

        let fno = FnoUnderlyingExtraction {
            unique_underlying_ids,
            nse_eq_lookup,
            dangling_count: 0,
            total_derivative_count: 1,
        };
        let indices = make_indices(100); // 100 + 1 = 101 indices alone
        let universe =
            build_daily_universe(indices, fno, Vec::new(), Vec::new(), Vec::new()).expect("build");
        // Ghost SID is silently skipped → 0 fno_underlying entries.
        assert_eq!(universe.count_by_role(InstrumentRole::FnoUnderlying), 0);
        assert_eq!(universe.count_by_role(InstrumentRole::Index), 101);
    }

    #[test]
    fn empty_universe_rejected_as_below_min() {
        // 0 indices + 0 fno = 0 < 100.
        let indices = IndexExtraction {
            nse_indices: Vec::new(),
            bse_sensex: None,
            allowlist_misses: Vec::new(),
        };
        let fno = FnoUnderlyingExtraction {
            unique_underlying_ids: HashSet::new(),
            nse_eq_lookup: HashMap::new(),
            dangling_count: 0,
            total_derivative_count: 0,
        };
        let result = build_daily_universe(indices, fno, Vec::new(), Vec::new(), Vec::new());
        assert!(matches!(
            result,
            Err(BuildError::UniverseSizeOutOfBounds { actual: 0, .. })
        ));
    }

    #[test]
    fn envelope_constants_match_rule_file_section_2() {
        // Defensive: pin the constants used here against the same
        // constants pinned in Sub-PR #2's source-scan ratchet.
        assert_eq!(MIN_DAILY_UNIVERSE_SIZE, 100);
        assert_eq!(MAX_DAILY_UNIVERSE_SIZE, 1200);
    }

    // ----- Sub-PR #5: NTM constituent fold (operator lock §31) -----

    #[test]
    fn ntm_fold_empty_is_unchanged() {
        // Empty ntm_constituents (today, pre-#10) → identical to current.
        let a = build_daily_universe(
            make_indices(30),
            make_fno(150),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .expect("build");
        assert_eq!(a.total_count(), 181);
        assert_eq!(a.index_constituent_count(), 0);
        assert_eq!(a.fno_underlying_count(), 150);
    }

    #[test]
    fn ntm_fold_both_case_sets_both_flags_no_dup() {
        // Constituent SID "U0001" collides with an F&O underlying pushed in
        // Pass 3 → the existing target gets is_index_constituent, NO new row.
        let before = build_daily_universe(
            make_indices(30),
            make_fno(150),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .expect("before")
        .total_count();
        let universe = build_daily_universe(
            make_indices(30),
            make_fno(150),
            Vec::new(),
            vec![nse_eq_row("U0001", "STK1")],
            Vec::new(),
        )
        .expect("build");
        // No duplicate row — count unchanged.
        assert_eq!(universe.total_count(), before);
        // The matched target is BOTH.
        let both = universe
            .subscription_targets
            .iter()
            .find(|t| t.csv_row.security_id == "U0001")
            .expect("U0001 present");
        assert_eq!(both.role, InstrumentRole::FnoUnderlying);
        assert!(
            both.is_fno_underlying && both.is_index_constituent,
            "both flags"
        );
        assert_eq!(universe.fno_underlying_count(), 150);
        assert_eq!(universe.index_constituent_count(), 1);
    }

    #[test]
    fn ntm_fold_pure_constituent_adds_role() {
        // A constituent SID with no F&O underlying match → new IndexConstituent.
        let universe = build_daily_universe(
            make_indices(30),
            make_fno(150),
            Vec::new(),
            vec![nse_eq_row("999999", "PURECONST")],
            Vec::new(),
        )
        .expect("build");
        assert_eq!(universe.total_count(), 182); // 181 + 1 new
        let pc = universe
            .subscription_targets
            .iter()
            .find(|t| t.csv_row.security_id == "999999")
            .expect("present");
        assert_eq!(pc.role, InstrumentRole::IndexConstituent);
        assert!(!pc.is_fno_underlying && pc.is_index_constituent);
        assert_eq!(universe.index_constituent_count(), 1);
        assert_eq!(universe.fno_underlying_count(), 150);
    }

    #[test]
    fn ntm_fold_dedups_repeat_rows() {
        // Same constituent row twice in the input → folded once (I-P1-11).
        let universe = build_daily_universe(
            make_indices(30),
            make_fno(150),
            Vec::new(),
            vec![
                nse_eq_row("999999", "PURECONST"),
                nse_eq_row("999999", "PURECONST"),
            ],
            Vec::new(),
        )
        .expect("build");
        assert_eq!(universe.total_count(), 182, "repeat folded once, not twice");
        assert_eq!(universe.index_constituent_count(), 1);
    }

    #[test]
    fn ntm_fold_does_not_touch_index_values_across_segments() {
        // I-P1-11: a constituent NSE_EQ SID equal to an index IDX_I SID must
        // NOT flag the index value (keyed on (sid, NSE_EQ)).
        // make_indices uses IDX_I sids "I0".."I29" + "51"; use NSE_EQ "51".
        let universe = build_daily_universe(
            make_indices(30),
            make_fno(150),
            Vec::new(),
            vec![nse_eq_row("51", "FIFTYONE_EQ")],
            Vec::new(),
        )
        .expect("build");
        // SENSEX index (IDX_I, sid 51) must remain a pure Index.
        let sensex = universe
            .subscription_targets
            .iter()
            .find(|t| t.csv_row.segment == "IDX_I" && t.csv_row.security_id == "51")
            .expect("SENSEX present");
        assert!(!sensex.is_index_constituent, "index value untouched");
        // The NSE_EQ "51" is a new constituent row.
        assert_eq!(universe.index_constituent_count(), 1);
        assert_eq!(universe.total_count(), 182);
    }

    #[test]
    fn fno_underlying_count_includes_both_case() {
        // Flag-based count includes an F&O underlying that is ALSO a constituent.
        let universe = build_daily_universe(
            make_indices(30),
            make_fno(150),
            Vec::new(),
            vec![nse_eq_row("U0001", "STK1")], // both-case
            Vec::new(),
        )
        .expect("build");
        assert_eq!(universe.fno_underlying_count(), 150);
    }

    #[test]
    fn index_constituent_count_includes_both_case() {
        // Flag-based count includes the both-case + a pure constituent.
        let universe = build_daily_universe(
            make_indices(30),
            make_fno(150),
            Vec::new(),
            vec![
                nse_eq_row("U0001", "STK1"),       // both-case
                nse_eq_row("999999", "PURECONST"), // pure constituent
            ],
            Vec::new(),
        )
        .expect("build");
        assert_eq!(universe.index_constituent_count(), 2);
    }

    #[test]
    fn ntm_fold_drops_non_nse_eq_constituent_row_fail_closed() {
        // Defensive (hostile-review LOW): a non-NSE_EQ row (a future #10 wiring
        // bug) is dropped, never subscribed.
        let bad = CsvRow {
            security_id: "12345".to_string(),
            exch_id: "NSE".to_string(),
            segment: "NSE_FNO".to_string(), // NOT NSE_EQ
            instrument: "OPTSTK".to_string(),
            symbol_name: "BADROW".to_string(),
            ..Default::default()
        };
        let universe = build_daily_universe(
            make_indices(30),
            make_fno(150),
            Vec::new(),
            vec![bad],
            Vec::new(),
        )
        .expect("build");
        assert_eq!(universe.index_constituent_count(), 0, "non-NSE_EQ dropped");
        assert_eq!(universe.total_count(), 181, "no row added");
    }

    // ----- §36 (2026-07-08): FUTIDX-4 Pass 5 fold -----

    fn futidx_row(underlying: &str, segment: &str, sid: &str, expiry: &str) -> CsvRow {
        CsvRow {
            security_id: sid.to_string(),
            exch_id: if segment == "BSE_FNO" { "BSE" } else { "NSE" }.to_string(),
            segment: segment.to_string(),
            instrument: "FUTIDX".to_string(),
            symbol_name: format!("{underlying}-FUT"),
            underlying_security_id: "26000".to_string(),
            underlying_symbol: underlying.to_string(),
            expiry_date: expiry.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn test_build_daily_universe_pass5_promotes_futidx_targets() {
        let futures = vec![
            futidx_row("NIFTY", "NSE_FNO", "35001", "2026-07-30"),
            futidx_row("BANKNIFTY", "NSE_FNO", "35002", "2026-07-30"),
            futidx_row("MIDCPNIFTY", "NSE_FNO", "35003", "2026-07-28"),
            futidx_row("SENSEX", "BSE_FNO", "45001", "2026-07-31"),
        ];
        let universe = build_daily_universe(
            make_indices(30),
            make_fno(150),
            Vec::new(),
            Vec::new(),
            futures,
        )
        .expect("build");
        // Envelope counts the futures: 31 indices + 150 underlyings + 4.
        assert_eq!(universe.total_count(), 185);
        assert_eq!(universe.count_by_role(InstrumentRole::IndexFuture), 4);
        let fut = universe
            .subscription_targets
            .iter()
            .find(|t| t.csv_row.security_id == "45001")
            .expect("SENSEX future promoted");
        assert_eq!(fut.role, InstrumentRole::IndexFuture);
        assert_eq!(fut.csv_row.segment, "BSE_FNO");
        assert!(!fut.is_fno_underlying && !fut.is_index_constituent);
    }

    #[test]
    fn test_count_by_role_includes_index_future() {
        let futures = vec![futidx_row("NIFTY", "NSE_FNO", "35001", "2026-07-30")];
        let universe = build_daily_universe(
            make_indices(30),
            make_fno(150),
            Vec::new(),
            Vec::new(),
            futures,
        )
        .expect("build");
        assert_eq!(universe.count_by_role(InstrumentRole::IndexFuture), 1);
        // Existing role counts unaffected.
        assert_eq!(universe.count_by_role(InstrumentRole::Index), 31);
        assert_eq!(universe.count_by_role(InstrumentRole::FnoUnderlying), 150);
    }

    #[test]
    fn test_daily_universe_spot_only_when_no_future_rows() {
        // The rollback pin: an empty `index_futures` param ⇒ byte-identical
        // prior (spot-only) behavior — same targets, same order, same counts.
        let a = build_daily_universe(
            make_indices(30),
            make_fno(150),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .expect("build a");
        assert_eq!(a.count_by_role(InstrumentRole::IndexFuture), 0);
        assert_eq!(a.total_count(), 181);
        assert!(
            a.subscription_targets
                .iter()
                .all(|t| t.role != InstrumentRole::IndexFuture)
        );
    }

    #[test]
    fn test_pass5_dedup_futidx_on_composite_key() {
        // A duplicate futures row (same sid + segment) folds ONCE (I-P1-11);
        // an NSE_FNO SID numerically equal to an NSE_EQ spot SID is a
        // DIFFERENT composite key and both survive.
        let futures = vec![
            futidx_row("NIFTY", "NSE_FNO", "35001", "2026-07-30"),
            futidx_row("NIFTY", "NSE_FNO", "35001", "2026-07-30"), // dup
            futidx_row("BANKNIFTY", "NSE_FNO", "U0001", "2026-07-30"), // collides with spot U0001
        ];
        let universe = build_daily_universe(
            make_indices(30),
            make_fno(150),
            Vec::new(),
            Vec::new(),
            futures,
        )
        .expect("build");
        assert_eq!(universe.count_by_role(InstrumentRole::IndexFuture), 2);
        // Both the NSE_EQ spot and the NSE_FNO future with sid "U0001" survive.
        let u0001: Vec<_> = universe
            .subscription_targets
            .iter()
            .filter(|t| t.csv_row.security_id == "U0001")
            .collect();
        assert_eq!(u0001.len(), 2, "cross-segment sid collision: both kept");
    }
}
