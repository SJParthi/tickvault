//! Per-row option **moneyness** classification (ITM / ATM / OTM / UNKNOWN)
//! — the single home for EVERY strike/spot arithmetic in the moneyness
//! feature (operator directive 2026-07-14, relayed via the coordinator
//! session: moneyness is CRITICAL and MANDATORY on every option-chain and
//! option-contract row — O(1) time + O(1) space + zero allocation, computed
//! in RAM AND stored precomputed in the DB; the DB column is AUDIT-ONLY,
//! the RAM snapshot is the decision source of truth).
//!
//! ## Why the math lives HERE and nowhere else
//! The `option_chain_1m` DEDUP key carries a float `strike` column that is
//! safe ONLY while strikes are PARSE-ONLY, never computed (the ratchet
//! `crates/app/tests/option_chain_1m_wiring_guard.rs::ratchet_chain1m_strike_is_parse_only_never_computed`
//! scans the boot + persistence files for arithmetic needles). The boot
//! legs therefore only CALL the fns below — they never subtract, multiply,
//! or round a strike themselves, and NO derived strike is ever persisted
//! (the classification is a label; `strike` stays the vendor's parsed
//! value bit-for-bit).
//!
//! ## The decision table (normative — operator-specified, judge-approved)
//!
//! | # | Guard state | leg | strike vs atm | strike vs spot | Class |
//! |---|---|---|---|---|---|
//! | 1 | leg not "CE"/"PE" | — | — | — | UNKNOWN |
//! | 2 | spot NaN/±inf/≤0/>1e7 rupees/rounds-to-0-paise | any | — | — | UNKNOWN |
//! | 3 | strike NaN/±inf/≤0/>1e7 rupees/rounds-to-0-paise | any | — | — | UNKNOWN |
//! | 4 | step ≤0 / odd / unknown underlying | any | — | — | UNKNOWN |
//! | 5 | ok | CE or PE | == atm | any | ATM |
//! | 6 | ok | CE or PE | ≠ atm | == spot (paise-exact) | ATM (degenerate — off-grid only) |
//! | 7 | ok | CE | ≠ atm | < spot | ITM |
//! | 8 | ok | CE | ≠ atm | > spot | OTM |
//! | 9 | ok | PE | ≠ atm | > spot | ITM |
//! | 10 | ok | PE | ≠ atm | < spot | OTM |
//!
//! Row 5 note: ATM precedence over the inequality is the operator's
//! definition — the 24550 CE at spot 24536.40 has strike > spot but IS the
//! ATM strike (the 2026-04-21 live capture,
//! `docs/dhan-support/2026-04-21-ticket-5519522-python-also-fails.md`), so
//! it labels ATM, not OTM. Row 6: a strike paise-exactly AT the money is
//! financially neither in nor out of the money — ITM/OTM would be a
//! direction lie and UNKNOWN would hide a numerically certain fact; the
//! arm is reachable only for an off-grid strike (an on-grid strike equal
//! to spot IS the grid-rounding fixpoint).
//!
//! ## ATM = GRID ROUNDING, never a strike-list scan (operator mandate)
//! `atm_paise = ((spot_paise + step_paise/2) / step_paise) * step_paise` —
//! round-half-UP (a paise-exact midway spot rounds to the HIGHER strike).
//! Proof in [`atm_strike_paise`]. The step influences ONLY the ATM label;
//! the ITM/OTM direction is a pure strike-vs-spot inequality and is immune
//! to any step error — the worst wrong-step failure is a misplaced/absent
//! ATM label, made loud by the call sites' step-drift + atm-absent
//! counters, never a swapped direction.
//!
//! ## Integer paise everywhere (house convention)
//! All price comparison is integer paise — never f64 `==`/epsilon (mirrors
//! `crates/app/src/tf_consistency_boot.rs::to_paise` + the §37 house
//! precedent). The guarded conversion makes the saturating f64→i64 cast
//! provably in-range before it runs.
//!
//! # Performance
//! Every fn here is O(1) time / O(1) space with ZERO heap allocation —
//! DHAT-ratcheted (`crates/core/tests/dhat_moneyness.rs`) and
//! Criterion-budgeted (`moneyness = 50` ns in
//! `quality/benchmark-budgets.toml`).

/// The moneyness class of one option row. Wire labels are the SCREAMING
/// forms `"ITM"`/`"ATM"`/`"OTM"`/`"UNKNOWN"` — stamped verbatim as the
/// `moneyness` SYMBOL column on `option_chain_1m` +
/// `option_contract_1m_rest` and held in the RAM chain snapshot
/// (`tickvault_core::pipeline::chain_snapshot`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Moneyness {
    /// In the money (CE: strike < spot; PE: strike > spot).
    Itm,
    /// At the money (strike == grid-rounded ATM, or paise-exactly == spot).
    Atm,
    /// Out of the money (CE: strike > spot; PE: strike < spot).
    Otm,
    /// Unclassifiable — invalid spot/strike/leg/step. Fail-soft: a row is
    /// NEVER dropped for being unclassifiable, it is stamped UNKNOWN.
    Unknown,
}

impl Moneyness {
    /// The single-source list of every class (the `Feed::ALL` pattern) —
    /// build every iteration from this, never a hand-written literal.
    pub const ALL: &'static [Moneyness] = &[
        Moneyness::Itm,
        Moneyness::Atm,
        Moneyness::Otm,
        Moneyness::Unknown,
    ];

    /// The number of classes — derived from [`Moneyness::ALL`].
    pub const COUNT: usize = Self::ALL.len();

    /// Dense 0-based index (per-class arrays). Exhaustive match — a new
    /// variant is a compile error at every site that forgot it.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Itm => 0,
            Self::Atm => 1,
            Self::Otm => 2,
            Self::Unknown => 3,
        }
    }

    /// The stable wire-format label (`"ITM"`/`"ATM"`/`"OTM"`/`"UNKNOWN"`).
    /// `const fn` so it can seed `const` SYMBOL-label declarations in the
    /// storage writers with zero allocation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Itm => "ITM",
            Self::Atm => "ATM",
            Self::Otm => "OTM",
            Self::Unknown => "UNKNOWN",
        }
    }

    /// Parse a wire label (case-sensitive — machine-facing). Built from
    /// [`Moneyness::ALL`] so a new variant is automatically parseable.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|m| m.as_str() == name)
    }
}

/// One option leg (call / put). Wire labels mirror the row `leg` SYMBOL
/// constants (`OPTION_CHAIN_1M_LEG_CE`/`_PE` = `"CE"`/`"PE"`) so the RAM
/// snapshot and the DB rows speak the same strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum OptionLeg {
    /// Call option.
    Ce,
    /// Put option.
    Pe,
}

impl OptionLeg {
    /// The single-source list of both legs.
    pub const ALL: &'static [OptionLeg] = &[OptionLeg::Ce, OptionLeg::Pe];

    /// The stable wire-format label (`"CE"`/`"PE"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ce => "CE",
            Self::Pe => "PE",
        }
    }

    /// Parse a leg label (case-sensitive; anything not exactly `"CE"` or
    /// `"PE"` is `None` — the classifier maps that to UNKNOWN).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|l| l.as_str() == name)
    }
}

/// Price-plausibility ceiling in RUPEES (exclusive at the guard): mirrors
/// `MAX_PLAUSIBLE_STRIKE` in `crates/app/src/option_chain_1m_boot.rs` — no
/// NSE index spot or strike approaches 10M rupees. With this bound the
/// paise value is ≤ 1e9, so every intermediate in [`atm_strike_paise`]
/// (max `spot_paise + step_paise/2` ≈ 1e9 + 5e3) sits 9 orders of
/// magnitude below `i64::MAX` — no checked arithmetic needed (pinned by
/// the extreme/overflow boundary test below).
pub const MAX_PLAUSIBLE_PRICE_RUPEES: f64 = 10_000_000.0;

/// Strike-grid steps in PAISE, keyed by the cross-feed `underlying_symbol`
/// (the identical plain literals both feeds stamp — `underlying_security_id`
/// does NOT join Dhan↔Groww: 13/25/51 vs FNV bit-62 ids).
///
/// Evidence classes (2026-07-14):
/// - NIFTY Rs.50 — Verified-in-repo:
///   `docs/dhan-support/2026-04-21-ticket-5519522-python-also-fails.md:69,138`
///   ("strike gap 50", live ATM 24550 @ spot 24536.40) +
///   `docs/groww-ref/14-option-chain.md:54,84` (consecutive keys
///   "23400", "23450").
/// - BANKNIFTY Rs.100 — operator directive 2026-07-14 (repo evidence
///   consistent-only: telegram-style-rules.md:246 48,100→48,200 — not
///   adjacency-proving).
/// - SENSEX Rs.100 — operator directive 2026-07-14 (SOLE authority; zero
///   repo evidence — Verified-absence). The call sites' runtime
///   observed-step cross-check (`tv_moneyness_step_drift_total`) is the
///   live drift tripwire for all three.
pub const STRIKE_STEP_PAISE: [(&str, i64); 3] =
    [("NIFTY", 5_000), ("BANKNIFTY", 10_000), ("SENSEX", 10_000)];

/// The directive strike step (paise) for a chain underlying — a bounded
/// linear scan of the 3-element const table (O(1), zero alloc). Unknown
/// symbol → `None` → the classification degrades to UNKNOWN, never a
/// guessed grid.
///
/// # Performance
/// O(1) (bounded 3-element scan), zero allocation.
#[inline]
#[must_use]
pub fn strike_step_paise(underlying_symbol: &str) -> Option<i64> {
    STRIKE_STEP_PAISE
        .iter()
        .find(|(sym, _)| *sym == underlying_symbol)
        .map(|&(_, step)| step)
}

/// Guarded rupees→paise conversion: `None` unless the value is a finite,
/// positive, plausible price whose 2dp-rounded paise value is ≥ 1.
/// Mirrors the house `to_paise` (`crates/app/src/tf_consistency_boot.rs`:
/// `(v * 100.0).round() as i64`) with the guards folded in so the
/// saturating float→int cast is provably in-range:
/// - NaN / ±inf → `None` (`is_finite`),
/// - ≤ 0 → `None` (a Dhan silently-absent spot defaults to 0.0),
/// - > [`MAX_PLAUSIBLE_PRICE_RUPEES`] → `None` (blocks the saturating
///   cast from ever producing garbage),
/// - 0 < v < 0.005 → `None` (rounds to 0 paise — numerically no price).
///
/// # Performance
/// O(1), zero allocation.
#[inline]
#[must_use]
pub fn price_to_paise_guarded(v: f64) -> Option<i64> {
    if !v.is_finite() || v <= 0.0 || v > MAX_PLAUSIBLE_PRICE_RUPEES {
        return None;
    }
    // APPROVED: cast is in-range by the guard above (paise ≤ 1e9 + rounding)
    #[allow(clippy::cast_possible_truncation)]
    let paise = (v * 100.0).round() as i64;
    (paise >= 1).then_some(paise)
}

/// Grid-rounded ATM strike in paise: round-half-UP of the spot to the
/// strike grid — the operator-mandated formula
/// `((spot_paise + step_paise/2) / step_paise) * step_paise`, NEVER a
/// strike-list scan.
///
/// **Round-half-up proof (positive operands):** write
/// `spot = q*step + r`, `0 ≤ r < step`, `h = step/2` (exact — step is
/// even). `(spot + h) / step` truncates toward zero, which for positives
/// is floor: the result is `q` iff `r + h < step` iff `r < step/2`, and
/// `q + 1` iff `r ≥ step/2`. The exact midpoint `r == step/2` therefore
/// rounds UP — precisely round-half-up; the distance property
/// `|atm − spot| ≤ step/2` holds with the upper strike chosen at exactly
/// `step/2`. Worked live check: NIFTY spot 24536.40, step ₹50 →
/// `(2_453_640 + 2_500) / 5_000 = 491` → `491 × 5_000 = 2_455_000` =
/// 24550.00 — matches the captured live ATM ("strike gap 50",
/// 2026-04-21 dhan-support doc).
///
/// Returns `None` when `spot_paise < 1`, `step_paise ≤ 0`, the step is
/// odd (defensive: real steps are ₹50/₹100 = 5_000/10_000 paise, both
/// even; an odd step would make "half" ambiguous and indicates a
/// corrupted table), or the grid-rounded result would be 0 — a spot
/// strictly below half a step (`spot_paise < step_paise/2`, e.g. spot
/// ₹1.00 on a ₹50 grid — the 2026-07-14 proptest counterexample) has no
/// positive grid strike to label ATM; fail closed to UNKNOWN, never a
/// bogus `Some(0)`.
///
/// # Performance
/// O(1) pure integer arithmetic, zero allocation. With the
/// [`price_to_paise_guarded`] bound (`spot_paise ≤ ~1e9`) every
/// intermediate is ≪ `i64::MAX` — overflow-free by construction.
#[inline]
#[must_use]
pub fn atm_strike_paise(spot_paise: i64, step_paise: i64) -> Option<i64> {
    if spot_paise < 1 || step_paise <= 0 || step_paise % 2 != 0 {
        return None;
    }
    let atm = ((spot_paise + step_paise / 2) / step_paise) * step_paise;
    // A spot below half a step grid-rounds to 0 — not a tradable strike
    // (grid strikes are positive multiples of the step). Fail closed:
    // unresolvable ATM → callers classify UNKNOWN (never a bogus Some(0)).
    if atm < 1 { None } else { Some(atm) }
}

/// Per-row moneyness classification against a PRECOMPUTED ATM — the
/// two-step API's row half: the caller resolves `spot_paise` +
/// `atm_paise` ONCE per (underlying, minute) via
/// [`price_to_paise_guarded`] + [`atm_strike_paise`], then classifies
/// every row with this fn. Pure, total, panic-free.
///
/// `leg` must be exactly `"CE"` or `"PE"` (the row SYMBOL constants);
/// anything else → UNKNOWN. Any operand < 1 paise (invalid/missing spot,
/// invalid strike, unresolvable ATM) → UNKNOWN.
///
/// # Performance
/// O(1): one 2-arm label parse + three integer compares. Zero allocation
/// (DHAT-ratcheted; Criterion `moneyness/classify` ≤ 50 ns).
#[inline]
#[must_use]
pub fn classify_moneyness_paise(
    leg: &str,
    strike_paise: i64,
    spot_paise: i64,
    atm_paise: i64,
) -> Moneyness {
    let Some(leg) = OptionLeg::parse(leg) else {
        return Moneyness::Unknown;
    };
    if strike_paise < 1 || spot_paise < 1 || atm_paise < 1 {
        return Moneyness::Unknown;
    }
    // ATM precedence (decision table rows 5 + 6): the grid-rounded ATM
    // strike labels ATM even when strike > spot / strike < spot; a
    // paise-exact strike==spot (off-grid degenerate) is also ATM.
    if strike_paise == atm_paise || strike_paise == spot_paise {
        return Moneyness::Atm;
    }
    match (leg, strike_paise < spot_paise) {
        // CE below the spot is IN the money (rows 7/8).
        (OptionLeg::Ce, true) => Moneyness::Itm,
        (OptionLeg::Ce, false) => Moneyness::Otm,
        // PE below the spot is OUT of the money (rows 9/10).
        (OptionLeg::Pe, true) => Moneyness::Otm,
        (OptionLeg::Pe, false) => Moneyness::Itm,
    }
}

/// Guarded f64 convenience wrapper — converts strike + spot to paise
/// (guarded), grid-rounds the ATM, and classifies. Any guard failure →
/// UNKNOWN, never a panic, never a saturating-cast garbage value.
///
/// # Performance
/// O(1), zero allocation.
#[inline]
#[must_use]
pub fn classify_moneyness(
    leg: &str,
    strike_rupees: f64,
    spot_rupees: f64,
    step_paise: i64,
) -> Moneyness {
    let (Some(strike_paise), Some(spot_paise)) = (
        price_to_paise_guarded(strike_rupees),
        price_to_paise_guarded(spot_rupees),
    ) else {
        return Moneyness::Unknown;
    };
    let Some(atm_paise) = atm_strike_paise(spot_paise, step_paise) else {
        return Moneyness::Unknown;
    };
    classify_moneyness_paise(leg, strike_paise, spot_paise, atm_paise)
}

/// Symbol-resolving convenience: looks up the underlying's directive step
/// ([`strike_step_paise`]) and classifies. Unknown underlying → UNKNOWN.
/// Used by the per-contract leg (≤30 rows/minute); the chain legs use the
/// two-step API (ATM once per chain, [`classify_moneyness_paise`] per row).
///
/// # Performance
/// O(1), zero allocation.
#[inline]
#[must_use]
pub fn classify_moneyness_for(
    underlying_symbol: &str,
    leg: &str,
    strike_rupees: f64,
    spot_rupees: f64,
) -> Moneyness {
    match strike_step_paise(underlying_symbol) {
        Some(step_paise) => classify_moneyness(leg, strike_rupees, spot_rupees, step_paise),
        None => Moneyness::Unknown,
    }
}

/// Observed finest grid step over a SORTED slice of distinct strike paise:
/// the minimum positive adjacent difference. `None` when fewer than 2
/// distinct values. Advisory ONLY — the call sites compare it against the
/// directive const step (`tv_moneyness_step_drift_total`) and NEVER let it
/// change a classification. Duplicates (zero diffs) are skipped, never
/// returned. Known spurious-fire mode (documented, accepted): a single
/// off-grid stray strike shrinks the observed step — itself a correct
/// "vendor data is weird this minute" signal.
///
/// # Performance
/// O(n) over the caller's once-per-chain-fire sorted slice (cold path);
/// zero allocation.
#[must_use]
pub fn observed_finest_step_paise(sorted_strike_paise: &[i64]) -> Option<i64> {
    let mut finest: Option<i64> = None;
    for pair in sorted_strike_paise.windows(2) {
        let diff = pair[1].saturating_sub(pair[0]);
        if diff > 0 && finest.is_none_or(|f| diff < f) {
            finest = Some(diff);
        }
    }
    finest
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    // -- Wire format (the feed.rs pattern) --------------------------------

    #[test]
    fn test_moneyness_as_str_and_parse_round_trip_for_every_variant() {
        for &m in Moneyness::ALL {
            assert_eq!(Moneyness::parse(m.as_str()), Some(m));
        }
        assert_eq!(Moneyness::parse("itm"), None, "parse is case-sensitive");
        assert_eq!(Moneyness::parse("At the money"), None);
        assert_eq!(Moneyness::parse(""), None);
        // Pin the exact wire labels — the SYMBOL column depends on them.
        assert_eq!(Moneyness::Itm.as_str(), "ITM");
        assert_eq!(Moneyness::Atm.as_str(), "ATM");
        assert_eq!(Moneyness::Otm.as_str(), "OTM");
        assert_eq!(Moneyness::Unknown.as_str(), "UNKNOWN");
    }

    #[test]
    fn test_moneyness_labels_unique() {
        let labels: Vec<&str> = Moneyness::ALL.iter().map(|m| m.as_str()).collect();
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "labels must be unique");
        assert_eq!(Moneyness::COUNT, Moneyness::ALL.len());
        for (i, &m) in Moneyness::ALL.iter().enumerate() {
            assert_eq!(m.index(), i, "index must match ALL order");
        }
    }

    #[test]
    fn test_option_leg_as_str_and_parse_round_trip() {
        for &l in OptionLeg::ALL {
            assert_eq!(OptionLeg::parse(l.as_str()), Some(l));
        }
        assert_eq!(OptionLeg::Ce.as_str(), "CE");
        assert_eq!(OptionLeg::Pe.as_str(), "PE");
        assert_eq!(OptionLeg::parse("ce"), None, "case-sensitive");
        assert_eq!(OptionLeg::parse("Ce"), None);
        assert_eq!(OptionLeg::parse("XX"), None);
        assert_eq!(OptionLeg::parse(""), None);
    }

    // -- The step table ----------------------------------------------------

    #[test]
    fn test_strike_step_paise_table_and_unknown_boundary() {
        assert_eq!(strike_step_paise("NIFTY"), Some(5_000));
        assert_eq!(strike_step_paise("BANKNIFTY"), Some(10_000));
        assert_eq!(strike_step_paise("SENSEX"), Some(10_000));
        // Unknown / excluded underlyings resolve to None (→ UNKNOWN):
        assert_eq!(strike_step_paise("FINNIFTY"), None);
        assert_eq!(strike_step_paise("INDIA VIX"), None);
        assert_eq!(strike_step_paise("nifty"), None, "case-sensitive");
        assert_eq!(strike_step_paise(""), None);
        // Every table step is positive and even (the atm formula's
        // preconditions hold for every configured underlying).
        for (sym, step) in STRIKE_STEP_PAISE {
            assert!(
                step > 0 && step % 2 == 0,
                "{sym} step must be even+positive"
            );
        }
    }

    // -- Paise conversion (financial boundary suite) -----------------------

    #[test]
    fn test_price_to_paise_guarded_boundary() {
        // The tf_consistency to_paise pins, replicated:
        assert_eq!(price_to_paise_guarded(25_647.50), Some(2_564_750));
        assert_eq!(price_to_paise_guarded(0.005), Some(1));
        // Representation wobble: 24536.40 is not exactly representable.
        assert_eq!(price_to_paise_guarded(24_536.40), Some(2_453_640));
        // Sub-paise positive value rounds to 0 paise → None.
        assert_eq!(price_to_paise_guarded(0.004), None);
        // Zero / negative → None.
        assert_eq!(price_to_paise_guarded(0.0), None);
        assert_eq!(price_to_paise_guarded(-25_000.0), None);
        // NaN / ±inf → None.
        assert_eq!(price_to_paise_guarded(f64::NAN), None);
        assert_eq!(price_to_paise_guarded(f64::INFINITY), None);
        assert_eq!(price_to_paise_guarded(f64::NEG_INFINITY), None);
        // Plausibility ceiling: exactly at the max passes, above fails —
        // the saturating cast is unreachable for f64::MAX / 1e17.
        assert_eq!(
            price_to_paise_guarded(MAX_PLAUSIBLE_PRICE_RUPEES),
            Some(1_000_000_000)
        );
        assert_eq!(
            price_to_paise_guarded(MAX_PLAUSIBLE_PRICE_RUPEES + 1.0),
            None
        );
        assert_eq!(price_to_paise_guarded(1e17), None);
        assert_eq!(price_to_paise_guarded(f64::MAX), None);
    }

    // -- Grid-rounded ATM (half-up proof pins) ------------------------------

    #[test]
    fn test_atm_strike_paise_midpoint_tie_half_up_boundary() {
        for step in [5_000_i64, 10_000] {
            let q = 491_i64;
            let lower = q * step;
            // r == 0 → identity (spot exactly ON a grid strike).
            assert_eq!(atm_strike_paise(lower, step), Some(lower));
            // r == step/2 (exact midpoint) → UP to the higher strike.
            assert_eq!(atm_strike_paise(lower + step / 2, step), Some(lower + step));
            // r == step/2 - 1 (one paise below midway) → down.
            assert_eq!(atm_strike_paise(lower + step / 2 - 1, step), Some(lower));
            // r == step - 1 (just below the next strike) → up.
            assert_eq!(atm_strike_paise(lower + step - 1, step), Some(lower + step));
        }
        // The 2026-04-21 live capture: NIFTY spot 24536.40, step Rs.50 →
        // ATM 24550 (the captured live-run value).
        assert_eq!(atm_strike_paise(2_453_640, 5_000), Some(2_455_000));
    }

    #[test]
    fn test_atm_strike_paise_zero_negative_odd_step_is_none() {
        assert_eq!(atm_strike_paise(0, 5_000), None, "spot < 1 paise");
        assert_eq!(atm_strike_paise(-1, 5_000), None);
        assert_eq!(atm_strike_paise(2_453_640, 0), None, "zero step");
        assert_eq!(atm_strike_paise(2_453_640, -5_000), None, "negative step");
        assert_eq!(atm_strike_paise(2_453_640, 4_999), None, "odd step");
        // Sub-half-step spot (the 2026-07-14 proptest counterexample:
        // spot ₹1.00 on a ₹50 grid) grid-rounds to 0 — not a strike; None.
        assert_eq!(atm_strike_paise(100, 5_000), None, "sub-half-step spot");
        assert_eq!(
            atm_strike_paise(2_499, 5_000),
            None,
            "one paise below half-step"
        );
        // Exact half-step is the boundary: rounds UP to the first strike.
        assert_eq!(
            atm_strike_paise(2_500, 5_000),
            Some(5_000),
            "exact half-step rounds UP to the first positive strike"
        );
    }

    // -- The CE/PE direction ratchet (real capture numbers) ----------------

    #[test]
    fn test_classify_moneyness_ce_pe_direction_table() {
        // (underlying, leg, strike, spot, expected) — the catastrophic-if-
        // swapped arms pinned with REAL market numbers.
        // NIFTY spot 24536.40 = the 2026-04-21 dhan-support live capture
        // (ATM strike resolved: 24550.0, "strike gap 50").
        let cases: &[(&str, &str, f64, f64, Moneyness)] = &[
            ("NIFTY", "CE", 24_450.0, 24_536.40, Moneyness::Itm),
            ("NIFTY", "CE", 24_500.0, 24_536.40, Moneyness::Itm),
            ("NIFTY", "CE", 24_550.0, 24_536.40, Moneyness::Atm), // the captured live ATM
            ("NIFTY", "CE", 24_600.0, 24_536.40, Moneyness::Otm),
            ("NIFTY", "PE", 24_500.0, 24_536.40, Moneyness::Otm),
            ("NIFTY", "PE", 24_550.0, 24_536.40, Moneyness::Atm),
            ("NIFTY", "PE", 24_600.0, 24_536.40, Moneyness::Itm),
            ("NIFTY", "PE", 24_700.0, 24_536.40, Moneyness::Itm),
            // BANKNIFTY spot 48143.25, step 100 → ATM 48100 (nearest:
            // 43.25 below vs 56.75 above).
            ("BANKNIFTY", "CE", 48_000.0, 48_143.25, Moneyness::Itm),
            ("BANKNIFTY", "CE", 48_100.0, 48_143.25, Moneyness::Atm),
            ("BANKNIFTY", "CE", 48_200.0, 48_143.25, Moneyness::Otm),
            ("BANKNIFTY", "PE", 48_000.0, 48_143.25, Moneyness::Otm),
            ("BANKNIFTY", "PE", 48_100.0, 48_143.25, Moneyness::Atm),
            ("BANKNIFTY", "PE", 48_200.0, 48_143.25, Moneyness::Itm),
            // SENSEX spot 81050.00 = the exact midpoint of the Rs.100 grid
            // → half-up: 81100 is ATM, deterministically.
            ("SENSEX", "CE", 81_100.0, 81_050.00, Moneyness::Atm),
            ("SENSEX", "CE", 81_000.0, 81_050.00, Moneyness::Itm),
            ("SENSEX", "PE", 81_000.0, 81_050.00, Moneyness::Otm),
            ("SENSEX", "PE", 81_100.0, 81_050.00, Moneyness::Atm),
        ];
        for &(sym, leg, strike, spot, expected) in cases {
            assert_eq!(
                classify_moneyness_for(sym, leg, strike, spot),
                expected,
                "{sym} {leg} strike={strike} spot={spot} must classify {expected:?}"
            );
        }
    }

    // -- UNKNOWN fail-soft boundary suite -----------------------------------

    #[test]
    fn test_classify_moneyness_zero_spot_is_unknown() {
        // The Dhan silently-absent spot (val_f64 defaults last_price to
        // 0.0 with NO vendor flag) — the single most-reached UNKNOWN arm.
        assert_eq!(
            classify_moneyness("CE", 24_550.0, 0.0, 5_000),
            Moneyness::Unknown
        );
        assert_eq!(
            classify_moneyness("PE", 24_550.0, 0.0, 5_000),
            Moneyness::Unknown
        );
        // Sub-paise "positive" spot (rounds to 0 paise) is equally no price.
        assert_eq!(
            classify_moneyness("CE", 24_550.0, 0.004, 5_000),
            Moneyness::Unknown
        );
    }

    #[test]
    fn test_classify_moneyness_negative_spot_and_strike_is_unknown() {
        assert_eq!(
            classify_moneyness("CE", 24_550.0, -24_536.40, 5_000),
            Moneyness::Unknown
        );
        assert_eq!(
            classify_moneyness("PE", -24_550.0, 24_536.40, 5_000),
            Moneyness::Unknown
        );
        assert_eq!(
            classify_moneyness("CE", -1.0, -1.0, 5_000),
            Moneyness::Unknown
        );
    }

    #[test]
    fn test_classify_moneyness_nan_inf_extreme_price_overflow_guard() {
        // Hostile f64 inputs: UNKNOWN, no panic, the saturating cast is
        // unreachable (the plausibility guard fires first).
        for hostile in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e17, f64::MAX] {
            assert_eq!(
                classify_moneyness("CE", hostile, 24_536.40, 5_000),
                Moneyness::Unknown,
                "hostile strike {hostile} must be UNKNOWN"
            );
            assert_eq!(
                classify_moneyness("PE", 24_550.0, hostile, 5_000),
                Moneyness::Unknown,
                "hostile spot {hostile} must be UNKNOWN"
            );
        }
    }

    #[test]
    fn test_classify_moneyness_strike_zero_is_unknown() {
        assert_eq!(
            classify_moneyness("CE", 0.0, 24_536.40, 5_000),
            Moneyness::Unknown
        );
        assert_eq!(
            classify_moneyness_paise("CE", 0, 2_453_640, 2_455_000),
            Moneyness::Unknown
        );
    }

    #[test]
    fn test_classify_moneyness_unknown_leg_is_unknown() {
        for bad_leg in ["", "XX", "ce", "Ce", "CALL", "PUT"] {
            assert_eq!(
                classify_moneyness(bad_leg, 24_550.0, 24_536.40, 5_000),
                Moneyness::Unknown,
                "leg {bad_leg:?} must be UNKNOWN"
            );
        }
    }

    #[test]
    fn test_classify_moneyness_for_unknown_underlying_is_unknown() {
        assert_eq!(
            classify_moneyness_for("FINNIFTY", "CE", 24_550.0, 24_536.40),
            Moneyness::Unknown
        );
        assert_eq!(
            classify_moneyness_for("INDIA VIX", "PE", 12.0, 12.5),
            Moneyness::Unknown
        );
    }

    #[test]
    fn test_classify_moneyness_corrupt_step_zero_negative_is_unknown() {
        assert_eq!(
            classify_moneyness("CE", 24_550.0, 24_536.40, 0),
            Moneyness::Unknown
        );
        assert_eq!(
            classify_moneyness("CE", 24_550.0, 24_536.40, -5_000),
            Moneyness::Unknown
        );
        assert_eq!(
            classify_moneyness("CE", 24_550.0, 24_536.40, 4_999),
            Moneyness::Unknown,
            "odd step is a corrupted table"
        );
    }

    // -- The exact-money arms ------------------------------------------------

    #[test]
    fn test_classify_moneyness_spot_exactly_on_strike_boundary_is_atm() {
        // On-grid spot: r = 0 → atm = spot = strike (row 5), both legs.
        assert_eq!(
            classify_moneyness("CE", 24_550.0, 24_550.0, 5_000),
            Moneyness::Atm
        );
        assert_eq!(
            classify_moneyness("PE", 24_550.0, 24_550.0, 5_000),
            Moneyness::Atm
        );
        // Its neighbors keep the plain directions.
        assert_eq!(
            classify_moneyness("CE", 24_500.0, 24_550.0, 5_000),
            Moneyness::Itm
        );
        assert_eq!(
            classify_moneyness("CE", 24_600.0, 24_550.0, 5_000),
            Moneyness::Otm
        );
    }

    #[test]
    fn test_classify_moneyness_off_grid_strike_equal_spot_is_atm_edge() {
        // The 25700.5-class hostile off-grid strike (the repo's own
        // hostile-input fixture) paise-exactly AT the spot: row 6 — ATM
        // (numerically at-the-money; ITM/OTM would be a direction lie).
        assert_eq!(
            classify_moneyness("CE", 25_700.5, 25_700.50, 5_000),
            Moneyness::Atm
        );
        // Off-grid but unequal: the plain inequality applies.
        assert_eq!(
            classify_moneyness("CE", 25_700.5, 25_600.0, 5_000),
            Moneyness::Otm
        );
        assert_eq!(
            classify_moneyness("PE", 25_700.5, 25_600.0, 5_000),
            Moneyness::Itm
        );
    }

    // -- Observed-step cross-check helper ------------------------------------

    #[test]
    fn test_observed_finest_step_paise_min_adjacent_diff_and_edge_boundary() {
        // Uniform Rs.50 grid → 5_000.
        assert_eq!(
            observed_finest_step_paise(&[2_450_000, 2_455_000, 2_460_000]),
            Some(5_000)
        );
        // Mixed 50/100 grid → the finest (5_000).
        assert_eq!(
            observed_finest_step_paise(&[2_440_000, 2_450_000, 2_455_000]),
            Some(5_000)
        );
        // Fewer than 2 distinct values → None.
        assert_eq!(observed_finest_step_paise(&[]), None);
        assert_eq!(observed_finest_step_paise(&[2_455_000]), None);
        // Duplicates (zero diffs) are skipped, never returned as 0.
        assert_eq!(
            observed_finest_step_paise(&[2_455_000, 2_455_000, 2_460_000]),
            Some(5_000)
        );
        assert_eq!(observed_finest_step_paise(&[2_455_000, 2_455_000]), None);
        // The documented spurious-drift case: one off-grid stray strike
        // (25700.5) shrinks the observed step to 50 paise.
        assert_eq!(
            observed_finest_step_paise(&[2_570_000, 2_570_050, 2_575_000]),
            Some(50)
        );
    }

    // -- Property suite --------------------------------------------------------

    /// Test-only 2dp rounding through the same paise convention.
    fn round_2dp(x: f64) -> f64 {
        (x * 100.0).round() / 100.0
    }

    fn step_strategy() -> impl Strategy<Value = i64> {
        prop_oneof![Just(5_000_i64), Just(10_000_i64)]
    }

    proptest! {
        /// P1 — single-ATM: on an on-grid uniform chain covering the spot,
        /// EXACTLY ONE strike classifies ATM, for both its CE and PE.
        #[test]
        fn proptest_classify_moneyness_invariants_single_atm_on_grid(
            spot in 1.0_f64..100_000.0,
            step_paise in step_strategy(),
        ) {
            let spot = round_2dp(spot);
            let Some(spot_paise) = price_to_paise_guarded(spot) else {
                return Ok(()); // sub-paise spot — guarded out, nothing to assert
            };
            let Some(atm) = atm_strike_paise(spot_paise, step_paise) else {
                return Ok(());
            };
            let mut atm_count = 0_usize;
            for k in -10_i64..=10 {
                let strike_paise = atm + k * step_paise;
                if strike_paise < 1 {
                    continue;
                }
                #[allow(clippy::cast_precision_loss)] // APPROVED: test-only paise→rupees display conversion, ≤1e9
                let strike = strike_paise as f64 / 100.0;
                let ce = classify_moneyness("CE", strike, spot, step_paise);
                let pe = classify_moneyness("PE", strike, spot, step_paise);
                if ce == Moneyness::Atm {
                    prop_assert_eq!(pe, Moneyness::Atm, "ATM must hold for both legs");
                    atm_count += 1;
                }
            }
            prop_assert_eq!(atm_count, 1, "exactly one ATM strike per on-grid chain");
        }

        /// P2 + P3 — never-both-ITM and leg-flip: for any (strike, spot)
        /// with non-UNKNOWN classes, {CE, PE} is one of (ITM,OTM),
        /// (OTM,ITM), (ATM,ATM); flipping the leg maps ITM↔OTM and fixes
        /// ATM.
        #[test]
        fn proptest_classify_moneyness_invariants_leg_flip_never_both_itm(
            strike in 1.0_f64..100_000.0,
            spot in 1.0_f64..100_000.0,
            step_paise in step_strategy(),
        ) {
            let (strike, spot) = (round_2dp(strike), round_2dp(spot));
            let ce = classify_moneyness("CE", strike, spot, step_paise);
            let pe = classify_moneyness("PE", strike, spot, step_paise);
            match (ce, pe) {
                (Moneyness::Itm, Moneyness::Otm)
                | (Moneyness::Otm, Moneyness::Itm)
                | (Moneyness::Atm, Moneyness::Atm)
                | (Moneyness::Unknown, Moneyness::Unknown) => {}
                other => prop_assert!(false, "illegal leg pair {other:?} for strike={strike} spot={spot}"),
            }
        }

        /// P4 — totality / never-panics over hostile f64 (incl. NaN/inf)
        /// and arbitrary leg strings: always returns a variant.
        #[test]
        fn proptest_classify_moneyness_invariants_total_never_panics(
            strike in proptest::num::f64::ANY,
            spot in proptest::num::f64::ANY,
            step_paise in proptest::num::i64::ANY,
            leg in ".*",
        ) {
            let m = classify_moneyness(&leg, strike, spot, step_paise);
            prop_assert!(Moneyness::ALL.contains(&m));
        }

        /// P5 — paise round-trip invariance: classifying the 2dp-rounded
        /// inputs gives the same class as the raw inputs.
        #[test]
        fn proptest_classify_moneyness_invariants_paise_round_trip(
            strike in 1.0_f64..100_000.0,
            spot in 1.0_f64..100_000.0,
            step_paise in step_strategy(),
        ) {
            prop_assert_eq!(
                classify_moneyness("CE", strike, spot, step_paise),
                classify_moneyness("CE", round_2dp(strike), round_2dp(spot), step_paise)
            );
        }

        /// P6 — ATM distance: when the ATM computes, |atm − spot| ≤ step/2
        /// (equality only on the half-up tie, which rounds to the UPPER
        /// strike: atm > spot at the tie).
        #[test]
        fn proptest_classify_moneyness_invariants_atm_distance(
            spot in 1.0_f64..100_000.0,
            step_paise in step_strategy(),
        ) {
            let spot = round_2dp(spot);
            let Some(spot_paise) = price_to_paise_guarded(spot) else {
                return Ok(());
            };
            let Some(atm) = atm_strike_paise(spot_paise, step_paise) else {
                return Ok(());
            };
            let dist = (atm - spot_paise).abs();
            prop_assert!(dist <= step_paise / 2, "|atm-spot|={dist} > step/2");
            if dist == step_paise / 2 {
                prop_assert!(atm > spot_paise, "the half-up tie must round UP");
            }
        }
    }
}
