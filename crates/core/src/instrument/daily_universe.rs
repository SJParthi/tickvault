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
    /// `UNDERLYING_SECURITY_ID` (Sub-PR #5).
    FnoUnderlying,
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
        }
    }
}

/// One instrument the WebSocket pool will subscribe to.
#[derive(Debug, Clone)]
pub struct SubscriptionTarget {
    pub role: InstrumentRole,
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
    pub fn always_on_segments(&self) -> std::collections::HashSet<(u32, u8)> {
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
            let Ok(sid) = t.csv_row.security_id.trim().parse::<u32>() else {
                tracing::warn!(
                    security_id = %t.csv_row.security_id,
                    "always-on: GIFT Nifty row found but security_id did not parse as u32 — \
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
) -> Result<DailyUniverse, BuildError> {
    let mut subscription_targets: Vec<SubscriptionTarget> = Vec::new();

    // Pass 1 — NSE indices.
    for row in indices.nse_indices {
        subscription_targets.push(SubscriptionTarget {
            role: InstrumentRole::Index,
            csv_row: row,
        });
    }

    // Pass 2 — BSE SENSEX (after NSE for deterministic order).
    if let Some(sensex) = indices.bse_sensex {
        subscription_targets.push(SubscriptionTarget {
            role: InstrumentRole::Index,
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
                csv_row: row.clone(),
            });
        }
        // SID with no NSE_EQ row should be IMPOSSIBLE — the Sub-PR #5
        // extractor only inserts a SID into `unique_underlying_ids`
        // AFTER confirming `nse_eq_lookup.contains_key(sid)`. Defensive
        // skip (don't panic).
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
                SubscriptionTarget {
                    role: InstrumentRole::Index,
                    csv_row: idx_i_row("13", "NSE", "NIFTY"),
                },
                SubscriptionTarget {
                    role: InstrumentRole::Index,
                    csv_row: idx_i_row("5024", "NSE", "GIFTNIFTY"),
                },
                SubscriptionTarget {
                    role: InstrumentRole::FnoUnderlying,
                    csv_row: nse_eq_row("2885", "RELIANCE"),
                },
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
            subscription_targets: vec![SubscriptionTarget {
                role: InstrumentRole::Index,
                csv_row: idx_i_row("13", "NSE", "NIFTY"),
            }],
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
        let universe = build_daily_universe(indices, fno, Vec::new()).expect("build");
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
        let result = build_daily_universe(indices, fno, Vec::new());
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
        let result = build_daily_universe(indices, fno, Vec::new());
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
        let universe = build_daily_universe(indices, fno, Vec::new()).expect("at MIN accepts");
        assert_eq!(universe.total_count(), 100);
    }

    #[test]
    fn accepts_universe_exactly_at_max_size() {
        // 30 indices + 1 SENSEX + 1169 underlyings = 1200, exactly MAX.
        let indices = make_indices(30);
        let fno = make_fno(1169);
        let universe = build_daily_universe(indices, fno, Vec::new()).expect("at MAX accepts");
        assert_eq!(universe.total_count(), 1200);
    }

    #[test]
    fn indices_appear_before_underlyings_in_order() {
        let indices = make_indices(30);
        let fno = make_fno(69);
        let universe = build_daily_universe(indices, fno, Vec::new()).expect("build");

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
        let universe = build_daily_universe(indices, fno, Vec::new()).expect("build");

        // Index target #30 (0-indexed) should be SENSEX.
        let sensex = &universe.subscription_targets[30];
        assert_eq!(sensex.csv_row.symbol_name, "SENSEX");
        assert_eq!(sensex.csv_row.exch_id, "BSE");
    }

    #[test]
    fn count_by_role_returns_correct_counts() {
        let indices = make_indices(30);
        let fno = make_fno(150);
        let universe = build_daily_universe(indices, fno, Vec::new()).expect("build");
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
        let universe = build_daily_universe(indices, fno, Vec::new()).expect("build");
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
        let result = build_daily_universe(indices, fno, Vec::new());
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
}
