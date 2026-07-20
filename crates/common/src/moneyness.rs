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
//! to any wrong-but-VALID (positive, even) step — the worst such failure
//! is a misplaced/absent ATM label, made loud by the call sites'
//! step-drift + atm-absent counters, never a swapped direction. A
//! missing/odd/zero step is NOT in that envelope: it fail-closes the
//! whole minute to UNKNOWN (no direction is emitted at all).
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
    // checked_add makes the fn structurally TOTAL for any i64 pair —
    // unreachable via the guarded conversion (spot ≤ 1e9 paise) + the const
    // step table, but a future caller with a raw i64 can never overflow-
    // panic here; overflow fail-closes to None like every other guard.
    let half_up = spot_paise.checked_add(step_paise / 2)?;
    let atm = (half_up / step_paise) * step_paise;
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

/// Outcome of the signed moneyness STEP-INDEX computation
/// ([`moneyness_step_index`]) — a 3-way verdict so the caller can route
/// the misaligned case LOUDLY (counter + edge-latched warn) instead of a
/// silent NULL that would be indistinguishable from ordinary
/// invalid-input UNKNOWNs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepIndexOutcome {
    /// The strike sits exactly on the ATM-anchored grid — the signed
    /// step count (ITM negative / OTM positive / ATM 0; the classic
    /// chain notation "ITM-1/ITM-2 … OTM+1/OTM+2").
    Aligned(i64),
    /// The strike is NOT a whole number of grid steps from the ATM
    /// anchor — the call site must count + warn (never silent rounding,
    /// never a fabricated index) and fall back to the bare class label.
    NotAligned,
    /// Guard-invalid inputs (unparsable leg / strike or ATM < 1 paise /
    /// step ≤ 0 / overflow) — the ordinary UNKNOWN-class quiet path.
    Invalid,
}

/// Signed moneyness depth as a STRIKE-STEP INDEX — the numeric companion
/// to the ITM/ATM/OTM class in the classic option-chain notation the
/// operator specified 2026-07-20 (verbatim: *"it should be ITM -1 or
/// ITM -2 or something like that"*): **ITM rows step −1, −2, …; OTM rows
/// step +1, +2, …; the grid-ATM anchor strike steps 0.** The 2026-07-17
/// SIGN convention (ITM negative / OTM positive) is KEPT — it matches
/// the "ITM-minus / OTM-plus" notation; what the 2026-07-20 ruling
/// changed is the UNITS (grid steps, not rupees) and the DELIVERY (the
/// step is folded into the `moneyness` label itself via
/// [`moneyness_step_label`]; the `moneyness_depth` column is REMOVED).
///
/// Leg-normalized, so the sign reads the same for both legs:
/// - CE: `index = (strike − atm) / step` (CE strikes BELOW the ATM
///   anchor are ITM → negative ✓);
/// - PE: `index = (atm − strike) / step` (PE strikes ABOVE the ATM
///   anchor are ITM → negative ✓).
///
/// The anchor is the SAME grid-rounded ATM the classifier uses
/// ([`atm_strike_paise`] — round-half-up of the spot to the directive
/// grid [`strike_step_paise`]), so for every ON-GRID strike the index is
/// consistent with [`classify_moneyness_paise`]: the grid-ATM strike is
/// labeled ATM and indexes 0, and any on-grid strike above the anchor
/// is strictly above the spot (proof: `strike ≥ atm + step` and
/// `atm ≥ spot − step/2` ⇒ `strike ≥ spot + step/2 > spot`; mirror
/// below), so `Itm ⇒ index < 0` and `Otm ⇒ index > 0`
/// (consistency-pinned in the tests below). The ATM±10 chain plan puts
/// nominal rows in `[−10, +10]`; deeper vendor rows index honestly
/// beyond ±10 — never clamped, never relabeled.
///
/// ## Decision table
///
/// | # | Guard state | leg | Result |
/// |---|---|---|---|
/// | 1 | leg not "CE"/"PE" | — | `Invalid` |
/// | 2 | `strike_paise < 1` or `atm_paise < 1` or `step_paise ≤ 0` | any | `Invalid` |
/// | 3 | `(strike − atm)` not a multiple of `step` | any | `NotAligned` (loud at the call site — NEVER silent rounding) |
/// | 4 | ok | CE | `Aligned((strike_paise − atm_paise) / step_paise)` |
/// | 5 | ok | PE | `Aligned((atm_paise − strike_paise) / step_paise)` |
///
/// Overflow is IMPOSSIBLE past row 2 (no checked/panicking arithmetic
/// needed): both operands are ≥ 1, so the difference lies in
/// `[1 − i64::MAX, i64::MAX − 1]` — pinned by the i64-extreme tests
/// below (the same totality argument as [`atm_strike_paise`]'s bound).
///
/// The `< 1` guards mirror [`classify_moneyness_paise`]'s operand
/// guards: callers feed [`price_to_paise_guarded`] +
/// [`atm_strike_paise`] outputs (0 = invalid/missing/unresolvable), so a
/// moneyness=UNKNOWN row gets `Invalid` by construction (missing vendor
/// spot, unknown-underlying step, unresolvable ATM). Real NSE strikes
/// are whole grid multiples — `NotAligned` is a vendor-data-anomaly
/// signal, the same family as the observed-step drift cross-check.
///
/// # Performance
/// O(1): one 2-arm label parse + one subtraction + one integer rem/div
/// pair. Zero allocation.
#[inline]
#[must_use]
pub fn moneyness_step_index(
    leg: &str,
    strike_paise: i64,
    atm_paise: i64,
    step_paise: i64,
) -> StepIndexOutcome {
    let Some(leg) = OptionLeg::parse(leg) else {
        return StepIndexOutcome::Invalid;
    };
    if strike_paise < 1 || atm_paise < 1 || step_paise <= 0 {
        return StepIndexOutcome::Invalid;
    }
    // Leg-normalized signed distance from the grid anchor. Total by the
    // ≥1 guards above: both operands sit in [1, i64::MAX], so the
    // difference is bounded by ±(i64::MAX − 1) — overflow is
    // mathematically impossible (extreme-pinned in the tests below), so
    // plain subtraction is provably panic-free even with
    // overflow-checks on.
    let diff = match leg {
        OptionLeg::Ce => strike_paise - atm_paise,
        OptionLeg::Pe => atm_paise - strike_paise,
    };
    if diff % step_paise != 0 {
        return StepIndexOutcome::NotAligned;
    }
    StepIndexOutcome::Aligned(diff / step_paise)
}

/// Byte capacity of [`MoneynessStepLabel`] — "OTM+" (4) + the 20 digits
/// of `u64::MAX` = 24; every producible label fits with zero truncation.
pub const MONEYNESS_STEP_LABEL_CAP: usize = 24;

/// A stack-allocated combined moneyness label — the `moneyness` SYMBOL
/// column value in the 2026-07-20 combined-label law: `"ITM-1"`,
/// `"ITM-2"`, …, `"ATM"`, `"OTM+1"`, `"OTM+2"`, …, with the bare class
/// (`"ITM"`/`"OTM"`) as the honest fallback when the step is
/// unresolvable (misaligned strike / invalid grid inputs) and
/// `"UNKNOWN"` for unclassifiable rows. Zero heap allocation (fixed
/// 24-byte buffer, `Copy`); build via [`moneyness_step_label`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MoneynessStepLabel {
    buf: [u8; MONEYNESS_STEP_LABEL_CAP],
    len: u8,
}

impl MoneynessStepLabel {
    /// Internal constructor from a known-short ASCII literal.
    fn from_ascii(s: &str) -> Self {
        let mut buf = [0u8; MONEYNESS_STEP_LABEL_CAP];
        let bytes = s.as_bytes();
        let n = bytes.len().min(MONEYNESS_STEP_LABEL_CAP);
        buf[..n].copy_from_slice(&bytes[..n]);
        // APPROVED: n ≤ 24 by the min() above — the cast is lossless.
        #[allow(clippy::cast_possible_truncation)]
        Self { buf, len: n as u8 }
    }

    /// The label as a `&str` (the ILP SYMBOL write value).
    #[must_use]
    pub fn as_str(&self) -> &str {
        // The buffer is only ever filled from ASCII literals + ASCII
        // digits, so the slice is always valid UTF-8; the fallback is
        // pure defense (never reachable).
        core::str::from_utf8(&self.buf[..usize::from(self.len)]).unwrap_or("UNKNOWN")
    }
}

impl core::fmt::Debug for MoneynessStepLabel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "MoneynessStepLabel({:?})", self.as_str())
    }
}

impl core::fmt::Display for MoneynessStepLabel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Build the combined `moneyness` SYMBOL label from the class + the step
/// verdict (operator ruling 2026-07-20 — the step count lives IN the
/// label; verbatim: *"under moneyness column itself make it as itm-1 or
/// otm+1"*, upper-cased to match the table's existing SCREAMING SYMBOL
/// convention):
///
/// | class | step | label |
/// |---|---|---|
/// | UNKNOWN | any | `"UNKNOWN"` |
/// | ATM | any | `"ATM"` (the anchor strike indexes 0; the off-grid strike==spot degenerate stays bare) |
/// | ITM | `Aligned(n)`, `n ≠ 0` | `"ITM-{|n|}"` |
/// | OTM | `Aligned(n)`, `n ≠ 0` | `"OTM+{|n|}"` |
/// | ITM / OTM | `NotAligned` / `Invalid` / `Aligned(0)` | bare `"ITM"` / `"OTM"` (direction known from the spot comparison; step honestly absent — the NotAligned case is made loud by the call site) |
///
/// The sign GLYPH comes from the CLASS (ITM always renders `-`, OTM
/// always `+` — the operator's notation) and the magnitude from the
/// index; for on-grid strikes the two always agree (the consistency law
/// pinned in the tests: `Itm ⇒ index < 0`, `Otm ⇒ index > 0`).
/// `Aligned(0)` under an ITM/OTM class is structurally impossible
/// (index 0 ⇔ strike == atm ⇔ class ATM by row-5 precedence) — the
/// defensive arm falls back to the bare class, never a lying "ITM-0".
///
/// # Performance
/// O(1), zero heap allocation (stack buffer + manual digit write).
#[must_use]
pub fn moneyness_step_label(class: Moneyness, step: StepIndexOutcome) -> MoneynessStepLabel {
    match class {
        Moneyness::Unknown => MoneynessStepLabel::from_ascii("UNKNOWN"),
        Moneyness::Atm => MoneynessStepLabel::from_ascii("ATM"),
        Moneyness::Itm | Moneyness::Otm => {
            let (bare, glyph) = match class {
                Moneyness::Itm => ("ITM", b'-'),
                _ => ("OTM", b'+'),
            };
            let StepIndexOutcome::Aligned(idx) = step else {
                return MoneynessStepLabel::from_ascii(bare);
            };
            let magnitude = idx.unsigned_abs();
            if magnitude == 0 {
                return MoneynessStepLabel::from_ascii(bare);
            }
            let mut buf = [0u8; MONEYNESS_STEP_LABEL_CAP];
            buf[..3].copy_from_slice(bare.as_bytes());
            buf[3] = glyph;
            // Manual base-10 digit write (backwards into a scratch, then
            // copied) — u64::MAX is 20 digits, 4 + 20 = 24 = CAP.
            let mut scratch = [0u8; 20];
            let mut m = magnitude;
            let mut digits = 0usize;
            while m > 0 {
                // APPROVED: m % 10 < 10 — the cast is lossless.
                #[allow(clippy::cast_possible_truncation)]
                {
                    scratch[digits] = b'0' + (m % 10) as u8;
                }
                m /= 10;
                digits += 1;
            }
            for (i, d) in (0..digits).rev().enumerate() {
                buf[4 + i] = scratch[d];
            }
            // APPROVED: 4 + digits ≤ 24 — the cast is lossless.
            #[allow(clippy::cast_possible_truncation)]
            MoneynessStepLabel {
                buf,
                len: (4 + digits) as u8,
            }
        }
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
        // Structural totality: a raw i64::MAX spot (unreachable via the
        // guarded conversion — defense-in-depth for future raw callers)
        // fail-closes to None via checked_add instead of overflowing.
        assert_eq!(
            atm_strike_paise(i64::MAX, 5_000),
            None,
            "checked_add overflow must fail closed, never panic"
        );
        assert_eq!(
            atm_strike_paise(i64::MAX - 2_499, 5_000),
            None,
            "one paise past the checked_add boundary still fails closed"
        );
        assert!(
            atm_strike_paise(i64::MAX - 2_500, 5_000).is_some(),
            "the exact checked_add boundary (spot + step/2 == i64::MAX) \
             still computes — the guard rejects only genuine overflow"
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

    #[test]
    fn test_classify_moneyness_dual_atm_off_grid_spot_and_grid_atm_coexist() {
        // Spec pin (hostile-review round 1): when the spot itself is an
        // OFF-GRID value, decision-table rows 5 AND 6 can BOTH be live in
        // one chain — the grid-rounded ATM strike (row 5) and a degenerate
        // off-grid strike paise-exactly == spot (row 6) each classify ATM.
        // The P1 single-ATM proptest invariant is scoped to ON-GRID
        // uniform chains and is not contradicted.
        let spot_paise = 2_453_640_i64; // 24536.40 — the 2026-04-21 capture
        let step_paise = 5_000_i64;
        let atm = atm_strike_paise(spot_paise, step_paise).expect("ATM must resolve");
        assert_eq!(atm, 2_455_000, "grid ATM = 24550.00 (round-half-up)");
        for leg in ["CE", "PE"] {
            assert_eq!(
                classify_moneyness_paise(leg, spot_paise, spot_paise, atm),
                Moneyness::Atm,
                "{leg}: off-grid strike paise-exactly == spot is ATM (row 6)"
            );
            assert_eq!(
                classify_moneyness_paise(leg, atm, spot_paise, atm),
                Moneyness::Atm,
                "{leg}: the grid-rounded ATM strike is ATM (row 5)"
            );
        }
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

    // -- Step-index + combined label (operator ruling 2026-07-20) ------------

    #[test]
    fn test_moneyness_step_index_ce_pe_sign_convention() {
        // NIFTY Rs.50 grid (5_000 paise), ATM anchor 25_000.00.
        let atm = 2_500_000_i64;
        let step = 5_000_i64;
        // 4-quadrant pin (the coordinator-relayed law examples):
        // CE 24,950 = one step BELOW the anchor → ITM-1 territory → −1.
        assert_eq!(
            moneyness_step_index("CE", 2_495_000, atm, step),
            StepIndexOutcome::Aligned(-1)
        );
        // CE 25,050 → OTM+1 → +1.
        assert_eq!(
            moneyness_step_index("CE", 2_505_000, atm, step),
            StepIndexOutcome::Aligned(1)
        );
        // PE 25,050 → ITM-1 → −1.
        assert_eq!(
            moneyness_step_index("PE", 2_505_000, atm, step),
            StepIndexOutcome::Aligned(-1)
        );
        // PE 24,950 → OTM+1 → +1.
        assert_eq!(
            moneyness_step_index("PE", 2_495_000, atm, step),
            StepIndexOutcome::Aligned(1)
        );
        // The anchor strike itself → 0, both legs.
        assert_eq!(
            moneyness_step_index("CE", atm, atm, step),
            StepIndexOutcome::Aligned(0)
        );
        assert_eq!(
            moneyness_step_index("PE", atm, atm, step),
            StepIndexOutcome::Aligned(0)
        );
        // Two steps out, both directions.
        assert_eq!(
            moneyness_step_index("CE", 2_490_000, atm, step),
            StepIndexOutcome::Aligned(-2)
        );
        assert_eq!(
            moneyness_step_index("PE", 2_510_000, atm, step),
            StepIndexOutcome::Aligned(-2)
        );
        // The ATM±10 plan edges index exactly ±10.
        assert_eq!(
            moneyness_step_index("CE", atm - 10 * step, atm, step),
            StepIndexOutcome::Aligned(-10)
        );
        assert_eq!(
            moneyness_step_index("CE", atm + 10 * step, atm, step),
            StepIndexOutcome::Aligned(10)
        );
        // The 2026-07-20 operator screenshot re-derived on its real grid:
        // BANKNIFTY spot 57,800.90, Rs.100 step (10_000 paise) → grid ATM
        // 57,800.00; strike 81,000.00 sits 232 steps above — PE = ITM-232,
        // CE = OTM+232 (honestly OUTSIDE the ATM±10 plan window, never
        // clamped).
        let bn_atm = atm_strike_paise(5_780_090, 10_000).expect("grid ATM");
        assert_eq!(bn_atm, 5_780_000);
        assert_eq!(
            moneyness_step_index("PE", 8_100_000, bn_atm, 10_000),
            StepIndexOutcome::Aligned(-232)
        );
        assert_eq!(
            moneyness_step_index("CE", 8_100_000, bn_atm, 10_000),
            StepIndexOutcome::Aligned(232)
        );
    }

    #[test]
    fn test_moneyness_step_index_guards_misalignment_and_extremes() {
        let atm = 2_500_000_i64;
        let step = 5_000_i64;
        // Unknown / miscased legs → Invalid (mirrors the classifier).
        for bad_leg in ["", "XX", "ce", "Ce", "CALL", "PUT"] {
            assert_eq!(
                moneyness_step_index(bad_leg, 2_495_000, atm, step),
                StepIndexOutcome::Invalid,
                "leg {bad_leg:?} must be Invalid"
            );
        }
        // Zero/negative operands + step → Invalid (the guarded-conversion
        // sentinel — a moneyness=UNKNOWN row carries the quiet path).
        assert_eq!(
            moneyness_step_index("CE", 0, atm, step),
            StepIndexOutcome::Invalid
        );
        assert_eq!(
            moneyness_step_index("CE", 2_495_000, 0, step),
            StepIndexOutcome::Invalid
        );
        assert_eq!(
            moneyness_step_index("CE", 2_495_000, atm, 0),
            StepIndexOutcome::Invalid
        );
        assert_eq!(
            moneyness_step_index("PE", -1, atm, step),
            StepIndexOutcome::Invalid
        );
        assert_eq!(
            moneyness_step_index("PE", 2_495_000, atm, -5_000),
            StepIndexOutcome::Invalid
        );
        // An off-grid strike is NotAligned — NEVER silently rounded (the
        // loud error path at the call sites): half a step off…
        assert_eq!(
            moneyness_step_index("CE", 2_502_500, atm, step),
            StepIndexOutcome::NotAligned
        );
        // …and one paise off.
        assert_eq!(
            moneyness_step_index("PE", 2_495_001, atm, step),
            StepIndexOutcome::NotAligned
        );
        // Structural totality at the i64 extremes: with both operands ≥ 1
        // the subtraction is bounded by ±(i64::MAX − 1); step 1 keeps the
        // extremes grid-aligned, so the widest legal pairs still compute.
        assert_eq!(
            moneyness_step_index("CE", i64::MAX, 1, 1),
            StepIndexOutcome::Aligned(i64::MAX - 1)
        );
        assert_eq!(
            moneyness_step_index("PE", i64::MAX, 1, 1),
            StepIndexOutcome::Aligned(1 - i64::MAX)
        );
        assert_eq!(
            moneyness_step_index("CE", 1, i64::MAX, 1),
            StepIndexOutcome::Aligned(1 - i64::MAX)
        );
        assert_eq!(
            moneyness_step_index("PE", 1, i64::MAX, 1),
            StepIndexOutcome::Aligned(i64::MAX - 1)
        );
    }

    #[test]
    fn test_moneyness_step_label_law() {
        // The operator's literal notation: ITM-1 / ITM-2 / OTM+1 / OTM+2,
        // ATM, UNKNOWN — upper-case, glyph from the class.
        assert_eq!(
            moneyness_step_label(Moneyness::Itm, StepIndexOutcome::Aligned(-1)).as_str(),
            "ITM-1"
        );
        assert_eq!(
            moneyness_step_label(Moneyness::Itm, StepIndexOutcome::Aligned(-2)).as_str(),
            "ITM-2"
        );
        assert_eq!(
            moneyness_step_label(Moneyness::Otm, StepIndexOutcome::Aligned(1)).as_str(),
            "OTM+1"
        );
        assert_eq!(
            moneyness_step_label(Moneyness::Otm, StepIndexOutcome::Aligned(2)).as_str(),
            "OTM+2"
        );
        // Plan edges.
        assert_eq!(
            moneyness_step_label(Moneyness::Itm, StepIndexOutcome::Aligned(-10)).as_str(),
            "ITM-10"
        );
        assert_eq!(
            moneyness_step_label(Moneyness::Otm, StepIndexOutcome::Aligned(10)).as_str(),
            "OTM+10"
        );
        // Beyond the plan (the screenshot's 232-step strike) — honest,
        // never clamped.
        assert_eq!(
            moneyness_step_label(Moneyness::Itm, StepIndexOutcome::Aligned(-232)).as_str(),
            "ITM-232"
        );
        assert_eq!(
            moneyness_step_label(Moneyness::Otm, StepIndexOutcome::Aligned(232)).as_str(),
            "OTM+232"
        );
        // ATM + UNKNOWN stay bare regardless of the step verdict.
        assert_eq!(
            moneyness_step_label(Moneyness::Atm, StepIndexOutcome::Aligned(0)).as_str(),
            "ATM"
        );
        assert_eq!(
            moneyness_step_label(Moneyness::Atm, StepIndexOutcome::NotAligned).as_str(),
            "ATM"
        );
        assert_eq!(
            moneyness_step_label(Moneyness::Unknown, StepIndexOutcome::Invalid).as_str(),
            "UNKNOWN"
        );
        // Misaligned / invalid ITM/OTM fall back to the bare class —
        // direction honest, step honestly absent, never "ITM-0".
        assert_eq!(
            moneyness_step_label(Moneyness::Itm, StepIndexOutcome::NotAligned).as_str(),
            "ITM"
        );
        assert_eq!(
            moneyness_step_label(Moneyness::Otm, StepIndexOutcome::Invalid).as_str(),
            "OTM"
        );
        assert_eq!(
            moneyness_step_label(Moneyness::Itm, StepIndexOutcome::Aligned(0)).as_str(),
            "ITM"
        );
        // The glyph comes from the CLASS (defensive — magnitude only from
        // the index).
        assert_eq!(
            moneyness_step_label(Moneyness::Itm, StepIndexOutcome::Aligned(3)).as_str(),
            "ITM-3"
        );
        // Extreme magnitude fits the 24-byte buffer (20 digits + "OTM+").
        assert_eq!(
            moneyness_step_label(Moneyness::Otm, StepIndexOutcome::Aligned(i64::MAX)).as_str(),
            format!("OTM+{}", i64::MAX).as_str()
        );
        assert_eq!(
            moneyness_step_label(Moneyness::Itm, StepIndexOutcome::Aligned(i64::MIN)).as_str(),
            format!("ITM-{}", i64::MIN.unsigned_abs()).as_str()
        );
    }

    /// The label type's trait surface: `Display` renders the bare label,
    /// `Debug` wraps it, equality/copy compare by content (the buffers
    /// are zero-padded past `len`, so derived `PartialEq` is exact).
    #[test]
    fn test_moneyness_step_label_display_debug_and_eq() {
        let l = moneyness_step_label(Moneyness::Otm, StepIndexOutcome::Aligned(7));
        assert_eq!(format!("{l}"), "OTM+7");
        assert_eq!(format!("{l:?}"), "MoneynessStepLabel(\"OTM+7\")");
        let same = moneyness_step_label(Moneyness::Otm, StepIndexOutcome::Aligned(7));
        assert_eq!(l, same, "content-equal labels compare equal");
        let copied = l;
        assert_eq!(copied.as_str(), "OTM+7", "Copy semantics preserved");
        assert_ne!(
            l,
            moneyness_step_label(Moneyness::Itm, StepIndexOutcome::Aligned(-7)),
            "different labels compare unequal"
        );
    }

    #[test]
    fn test_moneyness_step_sign_is_consistent_with_classification() {
        // For a spread of valid on-grid (leg, strike, spot, atm) inputs:
        // classify==Itm ⇒ step index < 0, classify==Otm ⇒ index > 0, and
        // the grid-ATM strike (class Atm) ⇒ index 0 — the label glyphs
        // (ITM-, OTM+) can therefore never lie for aligned strikes.
        let spots = [2_453_640_i64, 4_814_325, 8_105_000, 5_000, 2_455_000];
        let step = 5_000_i64;
        for &spot_paise in &spots {
            let Some(atm_paise) = atm_strike_paise(spot_paise, step) else {
                continue;
            };
            for k in -10_i64..=10 {
                let strike_paise = atm_paise + k * step;
                if strike_paise < 1 {
                    continue;
                }
                for leg in ["CE", "PE"] {
                    let class = classify_moneyness_paise(leg, strike_paise, spot_paise, atm_paise);
                    let index = moneyness_step_index(leg, strike_paise, atm_paise, step);
                    let StepIndexOutcome::Aligned(idx) = index else {
                        panic!("on-grid strike must align: {leg} {strike_paise} vs {atm_paise}");
                    };
                    match class {
                        Moneyness::Itm => assert!(
                            idx < 0,
                            "{leg} ITM strike={strike_paise} atm={atm_paise} must index<0, got {idx}"
                        ),
                        Moneyness::Otm => assert!(
                            idx > 0,
                            "{leg} OTM strike={strike_paise} atm={atm_paise} must index>0, got {idx}"
                        ),
                        Moneyness::Atm => assert_eq!(
                            idx, 0,
                            "the grid-ATM strike must index 0 ({leg} {strike_paise})"
                        ),
                        Moneyness::Unknown => {
                            unreachable!("valid guarded inputs can never classify UNKNOWN")
                        }
                    }
                    // The combined label round-trips the law.
                    let label = moneyness_step_label(class, index);
                    match class {
                        Moneyness::Itm => assert!(
                            label.as_str().starts_with("ITM-"),
                            "ITM label must carry the minus glyph: {label}"
                        ),
                        Moneyness::Otm => assert!(
                            label.as_str().starts_with("OTM+"),
                            "OTM label must carry the plus glyph: {label}"
                        ),
                        Moneyness::Atm => assert_eq!(label.as_str(), "ATM"),
                        Moneyness::Unknown => unreachable!(),
                    }
                }
            }
        }
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
