//! §36 (2026-07-08) / §36.7 (2026-07-10) — all-monthly-expiries index-futures
//! selection, shared by BOTH feeds (Dhan daily-universe build + Groww
//! watch-set build).
//!
//! Operator verbatim quotes (recorded in
//! `daily-universe-scope-expansion-2026-05-27.md` §36/§36.7): 2026-07-08 —
//! "for both dhan and groww we need to add futures and those also should be
//! subscribed along with this, especially only for nifty banknifty and
//! sensex nifty midcap."; 2026-07-10 — "instead of only one current month
//! futures contracts just take all the futures of these indices — I mean
//! take all available applicable months futures."
//!
//! ## The contract (LOCKED — rule file first)
//!
//! - Exactly FOUR underlyings, pinned by [`INDEX_FUTURES_UNDERLYINGS`]:
//!   NIFTY / BANKNIFTY / MIDCPNIFTY (NSE_FNO) + SENSEX (BSE_FNO).
//! - ALL monthly expiries `>= today` ([`select_index_future_expiries`],
//!   §36.7) — whatever the vendor master lists, bounded per underlying by
//!   [`MAX_MONTHLY_EXPIRIES_PER_UNDERLYING`]; the NEAREST expiry is the
//!   first of each set ([`select_index_future_expiry`] delegates).
//! - Index futures NEVER roll — the T-0 expiring month stays selected
//!   through the 15:30 close ALONGSIDE the later months (the "FUTIDX →
//!   NEVER roll" lock; the planner-side twin pins died with
//!   `subscription_planner.rs` in PR-C3, 2026-07-14 — the selector-side
//!   boundary tests below remain the lock). The next trading day's
//!   build advances automatically because `expiry < today` fails the
//!   `>= today` filter. Deliberately NO `TradingCalendar` parameter — the
//!   calendar arm exists only to trigger the STOCK T-0 roll, which is banned
//!   for index futures; omitting the parameter makes accidental roll
//!   activation unrepresentable.
//! - DEGRADE, never a build failure: a zero-candidate / serial-flood miss
//!   drops the whole underlying; a per-month flood/ambiguity miss drops ONLY
//!   that month (§36.7 — the trading-relevant front month survives a corrupt
//!   far-month row). The caller pages `FUTIDX-01` per miss and continues
//!   with the resolved subset.
//! - Cross-feed parity: both feeds record their selection here; the moment
//!   BOTH are present, the per-underlying expiry SETS are compared — any
//!   comparable-month divergence pages `FUTIDX-02`; a pure far-suffix depth
//!   difference (vendor publication lag) is an `info!` + counter, never a
//!   page (§36.7).
//!
//! COLD PATH — every function here runs once per feed per boot/activation.
//!
//! PR-C1 (2026-07-13): the module-level `daily_universe_fetcher` gate was
//! REMOVED per the daily-universe 2026-07-13 banner §(d) — the §36.7 GROWW
//! futures leg STANDS after the Dhan live-WS retirement, and this shared
//! selector must not depend on a build feature (a future feature removal
//! would silently drop the Groww futures — a scope violation). Ratchet:
//! `tests::test_futidx_selector_is_not_feature_gated`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use chrono::NaiveDate;
use tickvault_common::error_code::ErrorCode;
use tracing::{error, info};

/// §36 (2026-07-08): one authorized index-futures underlying.
///
/// SecurityIds/tokens are NEVER hardcoded (instrument-master rule 3) —
/// matching is by canonicalized `UNDERLYING_SYMBOL`. Do NOT match by
/// `UNDERLYING_SECURITY_ID`: Dhan index-F&O underlying SIDs are
/// DERIVATIVES-domain (NIFTY=26000, BANKNIFTY=26009, SENSEX=1 — the
/// 2026-05-29 ~17K-row-miss precedent, `fno_underlying_extractor.rs`) and
/// MIDCPNIFTY's is UNKNOWN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexFutureUnderlying {
    /// Canonical underlying symbol after `canonicalize_index_symbol`.
    pub canonical: &'static str,
    /// Exchange the future is listed on (`"NSE"` | `"BSE"`).
    pub exch_id: &'static str,
    /// The Dhan canonical segment its FUTIDX rows carry
    /// (`"NSE_FNO"` | `"BSE_FNO"`).
    pub dhan_segment: &'static str,
}

/// §36: the ONLY four authorized index-futures underlyings. Adding a 5th
/// requires a fresh dated operator quote + rule-file edit FIRST (ratcheted by
/// `daily_universe_scope_guard.rs::futidx_scope_pinned_to_4_underlyings_all_monthly_expiries`).
pub const INDEX_FUTURES_UNDERLYINGS: [IndexFutureUnderlying; 4] = [
    IndexFutureUnderlying {
        canonical: "NIFTY",
        exch_id: "NSE",
        dhan_segment: "NSE_FNO",
    },
    IndexFutureUnderlying {
        canonical: "BANKNIFTY",
        exch_id: "NSE",
        dhan_segment: "NSE_FNO",
    },
    IndexFutureUnderlying {
        canonical: "MIDCPNIFTY",
        exch_id: "NSE",
        dhan_segment: "NSE_FNO",
    },
    IndexFutureUnderlying {
        canonical: "SENSEX",
        exch_id: "BSE",
        dhan_segment: "BSE_FNO",
    },
];

/// §36.7 (2026-07-10): envelope bound on DISTINCT monthly future expiries per
/// underlying — a legitimate master lists ~3 serials (near/next/far); beyond
/// 6 is a corrupt/flooded file and the underlying degrades fail-closed with
/// [`IndexFutureMissReason::MonthlySerialFlood`] (INSTR-FETCH-04 discipline —
/// corrupt data is never truncated-and-trusted). Shared by BOTH feeds.
pub const MAX_MONTHLY_EXPIRIES_PER_UNDERLYING: usize = 6;

/// Envelope bound on total selected index-future targets (§36.7): 4
/// underlyings × [`MAX_MONTHLY_EXPIRIES_PER_UNDERLYING`]. NOT an expected
/// count — the expected count is vendor-controlled (typically ~12).
pub const MAX_INDEX_FUTURE_TARGETS: usize =
    INDEX_FUTURES_UNDERLYINGS.len() * MAX_MONTHLY_EXPIRIES_PER_UNDERLYING;

/// Bound on the distinct `underlying_symbol` evidence values carried in a
/// index-future selection's miss evidence (the FUTIDX-01 alias-drift
/// payload; the Dhan-leg `IndexFutureSelection` carrier was DELETED
/// 2026-07-18, dead-code batch 2). `pub` so
/// the Groww-side evidence collector shares the SAME bound (no divergence).
pub const MAX_UNDERLYING_SYMBOLS_EVIDENCE: usize = 20;

/// §36.7: ALL monthly expiries `>= today`, ascending — THE shared boundary
/// rule; both feeds call exactly this.
///
/// The expiring month stays through its final session (`>= today` keeps
/// T-0) and falls out the next morning; NEVER rolls (index futures trade to
/// expiry — contrast the RETIRED stock selector's T-0 roll, deleted with
/// `subscription_planner.rs` in PR-C3 2026-07-14). NO calendar parameter, deliberately: accidental roll activation is
/// unrepresentable. `expiry_dates_sorted_asc` MUST be sorted ascending.
#[must_use]
pub fn select_index_future_expiries(
    expiry_dates_sorted_asc: &[NaiveDate],
    today_ist: NaiveDate,
) -> Vec<NaiveDate> {
    expiry_dates_sorted_asc
        .iter()
        .copied()
        .filter(|d| *d >= today_ist)
        .collect()
}

/// Nearest = first of [`select_index_future_expiries`]. Production consumer:
/// the §36.7 D7 far-month alarm-gate exclusion (`far_month_future_sids` in
/// `crates/app/src/main.rs`) identifies each underlying's nearest month
/// through THIS fn (AM-r1 F6, 2026-07-10 — wired, not just documented).
/// Delegates so the `>= today` rule has exactly ONE implementation.
#[must_use]
pub fn select_index_future_expiry(
    expiry_dates_sorted_asc: &[NaiveDate],
    today_ist: NaiveDate,
) -> Option<NaiveDate> {
    select_index_future_expiries(expiry_dates_sorted_asc, today_ist)
        .into_iter()
        .next()
}

/// Why an authorized underlying resolved NO contract (FUTIDX-01 payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexFutureMissReason {
    /// No FUTIDX rows for this underlying on its expected exchange/segment.
    NoFutRows,
    /// Every parsed expiry was `< today` (stale-master data anomaly).
    AllExpiriesPast,
    /// ≥2 rows share the chosen (underlying, expiry) — fail-closed, never
    /// guess a SID/token (mirrors the Groww ISIN-ambiguity precedent).
    AmbiguousDuplicateExpiry,
    /// Every candidate row's expiry was unparsable (skipped + counted).
    BadExpiryFormat,
    /// Every candidate row's native id (Groww `exchange_token`) was
    /// non-numeric (skipped + counted). Groww-side only — the Dhan
    /// `security_id` is kept as a string and never numerically gated.
    /// Distinct from [`Self::BadExpiryFormat`] so the operator triages the
    /// id-space arm, not the expiry arm (hostile-review round 1, 2026-07-08).
    BadNativeToken,
    /// More than [`FUTIDX_SAME_EXPIRY_CANDIDATE_CAP`] rows matched the chosen
    /// (underlying, expiry) — a corrupt/flooded vendor master (hostile-review
    /// round 3, 2026-07-08: INSTR-FETCH-04-style envelope discipline; the
    /// dedup pass is O(n) but a flood this size is untrustworthy data, so the
    /// month degrades fail-closed instead of being guessed from a corrupt
    /// set). §36.7 (2026-07-10): per-(underlying, expiry) — drops ONLY that
    /// month; other months of the underlying still subscribe.
    SameExpiryCandidateFlood,
    /// §36.7 (2026-07-10): more than [`MAX_MONTHLY_EXPIRIES_PER_UNDERLYING`]
    /// DISTINCT future expiries for one underlying — a corrupt/flooded
    /// master (a legitimate one lists ~3 monthly serials). The WHOLE
    /// underlying degrades fail-closed — corrupt data is never
    /// truncated-and-trusted (INSTR-FETCH-04 discipline).
    MonthlySerialFlood,
}

/// Hard sanity cap on same-(underlying, expiry) FUTIDX candidate rows —
/// a legitimate master carries EXACTLY ONE row per (underlying, expiry)
/// (plus at most a handful of vendor-glitch duplicates); anything beyond
/// this is a corrupt/flooded file and the underlying degrades fail-closed
/// with [`IndexFutureMissReason::SameExpiryCandidateFlood`]. Shared by the
/// Dhan selector and the Groww extractor (hostile-review round 3,
/// 2026-07-08).
pub const FUTIDX_SAME_EXPIRY_CANDIDATE_CAP: usize = 16;

/// One degraded (underlying[, month]) — the FUTIDX-01 unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexFutureMiss {
    pub canonical: &'static str,
    pub reason: IndexFutureMissReason,
    /// §36.7: `Some(expiry)` = ONLY that month degraded (flood/ambiguity at
    /// that expiry); `None` = the whole underlying (zero candidates /
    /// all-past / serial flood).
    pub expiry: Option<NaiveDate>,
}

/// One feed's chosen contract for an underlying — the cross-feed parity unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedFutureSelection {
    pub canonical: &'static str,
    pub expiry: NaiveDate,
    /// Dhan `SECURITY_ID` or Groww `exchange_token` — DIFFERENT id spaces;
    /// parity compares by (canonical, expiry), never by id.
    pub native_id: String,
    /// `"NSE_FNO"` | `"BSE_FNO"` (Dhan) or `"FNO"` (Groww).
    pub segment: String,
}

/// §36.7: how a cross-feed expiry-SET mismatch is classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParityMismatchKind {
    /// Comparable-month divergence (nearest differs, a hole, or a whole
    /// underlying one-sided) — pages FUTIDX-02 (High).
    Divergence,
    /// Pure far-suffix depth difference (every one-sided month strictly
    /// beyond the shorter feed's max) — info + counter, never a page:
    /// vendors legitimately publish far serials at different times.
    DepthOnly,
}

/// One cross-feed divergence (FUTIDX-02 / depth-note unit) — §36.7 expiry-SET
/// semantics: the months present on only one feed, sorted ascending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityMismatch {
    pub canonical: &'static str,
    /// Months in Dhan's set but not Groww's (sorted asc).
    pub dhan_only: Vec<NaiveDate>,
    /// Months in Groww's set but not Dhan's (sorted asc).
    pub groww_only: Vec<NaiveDate>,
    pub kind: ParityMismatchKind,
}

/// Pure cross-feed comparator (§36.7): for every underlying present on
/// EITHER feed, compare the SORTED-DEDUP expiry SET of ALL selections with
/// that canonical (fixes the pre-2026-07-10 `.find()` first-entry bug that
/// would have compared one arbitrary month per feed). Empty result = parity
/// OK. A one-sided set that is a strict FAR-SUFFIX of the other feed's
/// non-empty set classifies [`ParityMismatchKind::DepthOnly`]; everything
/// else (nearest divergence, a hole, a whole underlying one-sided) is
/// [`ParityMismatchKind::Divergence`].
#[must_use]
pub fn compare_index_future_selections(
    dhan: &[FeedFutureSelection],
    groww: &[FeedFutureSelection],
) -> Vec<ParityMismatch> {
    use std::collections::BTreeSet;
    let mut mismatches = Vec::new();
    for entry in &INDEX_FUTURES_UNDERLYINGS {
        let d_set: BTreeSet<NaiveDate> = dhan
            .iter()
            .filter(|s| s.canonical == entry.canonical)
            .map(|s| s.expiry)
            .collect();
        let g_set: BTreeSet<NaiveDate> = groww
            .iter()
            .filter(|s| s.canonical == entry.canonical)
            .map(|s| s.expiry)
            .collect();
        // Absent on BOTH feeds → not a parity question (both degraded; each
        // already paged FUTIDX-01 per-feed). Equal sets → parity OK.
        if d_set == g_set {
            continue;
        }
        let dhan_only: Vec<NaiveDate> = d_set.difference(&g_set).copied().collect();
        let groww_only: Vec<NaiveDate> = g_set.difference(&d_set).copied().collect();
        // DepthOnly iff exactly one side has extra months, the OTHER feed's
        // set is non-empty (a whole-underlying one-sided presence keeps the
        // original Divergence semantics), and every one-sided month lies
        // strictly beyond the shorter feed's max (a strict far-suffix —
        // sorted BTreeSet difference, so `first()` is the minimum).
        let kind = match (dhan_only.is_empty(), groww_only.is_empty()) {
            (false, true) if !g_set.is_empty() => {
                let min_one_sided = dhan_only.first().copied();
                let other_max = g_set.iter().next_back().copied();
                if min_one_sided > other_max {
                    ParityMismatchKind::DepthOnly
                } else {
                    ParityMismatchKind::Divergence
                }
            }
            (true, false) if !d_set.is_empty() => {
                let min_one_sided = groww_only.first().copied();
                let other_max = d_set.iter().next_back().copied();
                if min_one_sided > other_max {
                    ParityMismatchKind::DepthOnly
                } else {
                    ParityMismatchKind::Divergence
                }
            }
            _ => ParityMismatchKind::Divergence,
        };
        mismatches.push(ParityMismatch {
            canonical: entry.canonical,
            dhan_only,
            groww_only,
            kind,
        });
    }
    mismatches
}

/// Cold-path helper: render a sorted date list for the FUTIDX-02 / depth-note
/// payloads (`"2026-08-27,2026-09-24"`; empty list → `"-"`).
fn join_dates(dates: &[NaiveDate]) -> String {
    if dates.is_empty() {
        return "-".to_string();
    }
    dates
        .iter()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Backing store for the per-boot cross-feed parity recorder. Each feed's
/// entry carries the IST TRADING DATE it was recorded for — the comparator
/// refuses to compare selections from DIFFERENT trading dates (a Groww
/// re-activation on day D+1 must never page FUTIDX-02 against a stale day-D
/// Dhan selection across an expiry boundary; hostile-review round 1,
/// 2026-07-08).
type DatedSelections = HashMap<&'static str, (NaiveDate, Vec<FeedFutureSelection>)>;

static SELECTIONS: OnceLock<Mutex<DatedSelections>> = OnceLock::new();

/// Outcome of one recorder insertion (testable, no I/O).
#[derive(Debug, Clone, PartialEq, Eq)]
enum RecordOutcome {
    /// Only one feed has recorded so far — no verdict possible.
    SingleFeed,
    /// Both feeds present but for DIFFERENT trading dates — comparison
    /// refused; the STALE (older-dated) feed's entry was evicted so a fresh
    /// same-date record can pair later.
    CrossDateSkipped { evicted_feed: &'static str },
    /// Both feeds present for the SAME trading date — comparator verdict
    /// (empty = parity OK).
    Verdict(Vec<ParityMismatch>),
}

/// Testable core of [`record_index_future_selection`]: insert one feed's
/// dated selection; compare ONLY when the other feed's entry exists for the
/// SAME trading date.
fn record_selection_in(
    map: &Mutex<DatedSelections>,
    feed: &'static str,
    trading_date_ist: NaiveDate,
    sel: Vec<FeedFutureSelection>,
) -> RecordOutcome {
    let mut guard = match map.lock() {
        Ok(g) => g,
        // Poison only signals a prior panic; the data is still valid.
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.insert(feed, (trading_date_ist, sel));
    let (Some((dhan_date, dhan)), Some((groww_date, groww))) =
        (guard.get("dhan"), guard.get("groww"))
    else {
        return RecordOutcome::SingleFeed;
    };
    if dhan_date != groww_date {
        // Cross-date: never a parity question. Evict the OLDER entry so a
        // later same-date record from that feed can pair cleanly.
        let evicted_feed = if dhan_date < groww_date {
            "dhan"
        } else {
            "groww"
        };
        guard.remove(evicted_feed);
        return RecordOutcome::CrossDateSkipped { evicted_feed };
    }
    let verdict = compare_index_future_selections(dhan, groww);
    RecordOutcome::Verdict(verdict)
}

// PR-C3 (2026-07-14, operator retirement directive 2026-07-13 — scope-lock
// amendment §B): the two Dhan-side `DailyUniverse` helpers
// (`dhan_selections_from_universe` + `record_dhan_selection_from_universe`,
// item-gated since C1 with "retires in C3" notes) are DELETED with the Dhan
// instrument chain. The FUTIDX-02 comparator below goes structurally DORMANT
// (single-feed runs never fire it — futidx-4-error-codes.md §2 banner); it is
// RETAINED as the ready-made cross-feed parity seam for GDF feed #3.

/// Record one feed's index-future selection for one IST trading date (cold
/// path, once per feed per boot/activation). Order-independent: the
/// comparator fires the moment BOTH feeds have recorded FOR THE SAME
/// TRADING DATE; single-feed runs and cross-date pairs never fire it.
/// Parity OK → one `info!` verdict line; mismatch → one
/// `error!(code = "FUTIDX-02")` per underlying +
/// `tv_index_futures_parity_mismatch_total`.
pub fn record_index_future_selection(
    feed: &'static str,
    trading_date_ist: NaiveDate,
    sel: Vec<FeedFutureSelection>,
) {
    let map = SELECTIONS.get_or_init(|| Mutex::new(HashMap::new()));
    let mismatches = match record_selection_in(map, feed, trading_date_ist, sel) {
        RecordOutcome::SingleFeed => return,
        RecordOutcome::CrossDateSkipped { evicted_feed } => {
            info!(
                feed,
                evicted_feed,
                trading_date_ist = %trading_date_ist,
                "index-futures parity: cross-date selections — comparison refused, stale \
                 feed entry evicted (a fresh same-date record will re-pair)"
            );
            return;
        }
        RecordOutcome::Verdict(m) => m,
    };
    if mismatches.is_empty() {
        info!(
            feed,
            "index-futures parity OK — both feeds selected identical per-underlying expiry sets"
        );
        return;
    }
    for m in &mismatches {
        match m.kind {
            ParityMismatchKind::Divergence => {
                error!(
                    code = ErrorCode::Futidx02CrossFeedExpiryMismatch.code_str(),
                    underlying = m.canonical,
                    dhan_only = %join_dates(&m.dhan_only),
                    groww_only = %join_dates(&m.groww_only),
                    "index-futures cross-feed expiry-set mismatch — one vendor master is \
                     stale/divergent; both feeds STAY LIVE, cross-feed rows for this \
                     underlying are not comparable today"
                );
                metrics::counter!("tv_index_futures_parity_mismatch_total").increment(1);
            }
            ParityMismatchKind::DepthOnly => {
                // §36.7: far-serial publication lag is a legitimate,
                // self-healing vendor state — never a page. The message
                // deliberately carries no FUTIDX-02 code.
                info!(
                    underlying = m.canonical,
                    dhan_only = %join_dates(&m.dhan_only),
                    groww_only = %join_dates(&m.groww_only),
                    "index-futures parity depth note: far-serial publication lag — one vendor \
                     lists extra far months beyond the other's max; comparable months agree"
                );
                metrics::counter!(
                    "tv_index_futures_parity_depth_mismatch_total",
                    "underlying" => m.canonical
                )
                .increment(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PR-C1 (2026-07-13) ratchet — daily-universe 2026-07-13 banner §(d):
    /// this shared selector module must NEVER regain a MODULE-LEVEL
    /// `daily_universe_fetcher` gate (a future removal of the feature would
    /// silently drop the Groww §36.7 futures — a scope violation), and the
    /// Groww extraction call chain in `feed/groww/instruments.rs` must stay
    /// UNCONDITIONAL. (The two item-gated Dhan-side `DailyUniverse`
    /// helpers were deleted in PR-C3, 2026-07-14, with the feature itself —
    /// this ratchet now pins a feature that must never RETURN.)
    #[test]
    fn test_futidx_selector_is_not_feature_gated() {
        // (a) No inner module-level gate in THIS file (the `#![cfg(...)]`
        //     form that gated the whole module pre-C1).
        let own_src = include_str!("index_futures.rs");
        assert!(
            !own_src.contains(concat!("#![cfg(feature = \"daily_universe_fetcher\")", "]")),
            "index_futures.rs regained a module-level daily_universe_fetcher gate — \
             the Groww §36.7 futures mandate must not depend on a build feature"
        );
        // (b) The mod.rs declaration is ungated: the `pub mod index_futures;`
        //     line must NOT be immediately preceded by a cfg attribute.
        let mod_src = include_str!("mod.rs");
        let decl = mod_src
            .find("pub mod index_futures;")
            .expect("mod.rs declares index_futures"); // APPROVED: test
        let preceding_line = mod_src[..decl].trim_end().rsplit('\n').next().unwrap_or(""); // APPROVED: test
        assert!(
            !preceding_line.contains("cfg(feature"),
            "mod.rs re-gated `pub mod index_futures;` behind a feature \
             (preceding line: {preceding_line:?})"
        );
        // (c) The Groww instruments file carries NO feature gate AT ALL —
        //     the strongest simple pin (2026-07-13 hostile-review L1: the
        //     previous 6-line look-back above `extract_index_future_entries`
        //     could be evaded by an attribute placed ABOVE the doc block;
        //     whole-file zero-occurrence cannot). The fn-existence check
        //     keeps the pin non-vacuous. This assertion's own literal lives
        //     in THIS file, so it can never satisfy itself in the scanned
        //     Groww source.
        let groww_src = include_str!("../feed/groww/instruments.rs");
        assert!(
            groww_src.contains("pub fn extract_index_future_entries("),
            "groww instruments lost extract_index_future_entries — the §36.7 \
             extraction site moved; re-point this ratchet"
        );
        assert!(
            !groww_src.contains("cfg(feature"),
            "feed/groww/instruments.rs regained a feature gate — the Groww \
             futures extraction (and the whole Groww instruments module) must \
             stay unconditional (daily-universe 2026-07-13 banner §(d))"
        );
        // (d) The dead empty-futures fallback stayed dead.
        assert!(
            !groww_src.contains("futures require the shared selector (feature-gated)"),
            "the not(feature) empty-futures fallback returned to instruments.rs"
        );
    }

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("test date")
    }
    #[test]
    fn test_index_futures_underlyings_pinned_exactly_four() {
        assert_eq!(INDEX_FUTURES_UNDERLYINGS.len(), 4, "§36: exactly 4");
        assert_eq!(MAX_MONTHLY_EXPIRIES_PER_UNDERLYING, 6, "§36.7 envelope");
        assert_eq!(
            MAX_INDEX_FUTURE_TARGETS, 24,
            "§36.7: 4 underlyings × 6-serial envelope — a bound, not an expected count"
        );
        let canonicals: Vec<&str> = INDEX_FUTURES_UNDERLYINGS
            .iter()
            .map(|u| u.canonical)
            .collect();
        assert_eq!(
            canonicals,
            vec!["NIFTY", "BANKNIFTY", "MIDCPNIFTY", "SENSEX"]
        );
        let nse = INDEX_FUTURES_UNDERLYINGS
            .iter()
            .filter(|u| u.exch_id == "NSE" && u.dhan_segment == "NSE_FNO")
            .count();
        let bse = INDEX_FUTURES_UNDERLYINGS
            .iter()
            .filter(|u| u.exch_id == "BSE" && u.dhan_segment == "BSE_FNO")
            .count();
        assert_eq!((nse, bse), (3, 1), "NSE ×3 + BSE SENSEX ×1");
    }

    #[test]
    fn test_select_index_future_expiry_picks_first_at_or_after_today() {
        let dates = [d("2026-07-30"), d("2026-08-27")];
        assert_eq!(
            select_index_future_expiry(&dates, d("2026-07-08")),
            Some(d("2026-07-30")),
            "earlier future expiry wins"
        );
    }

    #[test]
    fn test_select_index_future_expiries_returns_all_at_or_after_today() {
        // §36.7: ALL monthly expiries >= today, ascending; past ones dropped.
        let dates = [
            d("2026-06-10"),
            d("2026-07-15"),
            d("2026-08-12"),
            d("2026-09-10"),
        ];
        assert_eq!(
            select_index_future_expiries(&dates, d("2026-07-10")),
            vec![d("2026-07-15"), d("2026-08-12"), d("2026-09-10")],
            "all future months kept, ascending; the past month dropped"
        );
    }

    #[test]
    fn test_select_index_future_expiry_delegates_to_plural_first() {
        // The `>= today` rule has exactly ONE implementation — the singular
        // is the plural's first element on every fixture shape.
        for (dates, today) in [
            (vec![d("2026-07-30"), d("2026-08-27")], d("2026-07-08")),
            (vec![d("2026-07-30"), d("2026-08-27")], d("2026-07-30")),
            (vec![d("2026-07-30"), d("2026-08-27")], d("2026-07-31")),
            (vec![d("2026-06-25")], d("2026-07-08")),
            (vec![], d("2026-07-08")),
        ] {
            assert_eq!(
                select_index_future_expiry(&dates, today),
                select_index_future_expiries(&dates, today).first().copied(),
                "singular == plural.first() for dates={dates:?} today={today}"
            );
        }
    }

    #[test]
    fn test_select_index_future_expiries_keeps_expiring_month_and_later_on_t_zero() {
        // §36.7 NEVER-roll pin: on T-0 the expiring month stays ALONGSIDE
        // the later months through the close.
        let dates = [d("2026-07-30"), d("2026-08-27")];
        assert_eq!(
            select_index_future_expiries(&dates, d("2026-07-30")),
            vec![d("2026-07-30"), d("2026-08-27")],
            "T-0 keeps the expiring month AND the later months"
        );
    }

    #[test]
    fn test_select_index_future_expiries_drops_expired_month_next_morning() {
        let dates = [d("2026-07-29"), d("2026-08-27")];
        assert_eq!(
            select_index_future_expiries(&dates, d("2026-07-30")),
            vec![d("2026-08-27")],
            "the expired month falls out of >= today the next morning"
        );
    }

    #[test]
    fn test_select_index_future_expiry_keeps_expiring_contract_on_t_zero() {
        // NEVER-roll pin: today == expiry day → the EXPIRING contract stays.
        let dates = [d("2026-07-30"), d("2026-08-27")];
        assert_eq!(
            select_index_future_expiry(&dates, d("2026-07-30")),
            Some(d("2026-07-30")),
            "T-0 keeps the expiring contract through the close"
        );
        // T-1 also keeps nearest (no early roll).
        assert_eq!(
            select_index_future_expiry(&dates, d("2026-07-29")),
            Some(d("2026-07-30"))
        );
    }

    #[test]
    fn test_select_index_future_expiry_advances_day_after_expiry() {
        let dates = [d("2026-07-30"), d("2026-08-27")];
        assert_eq!(
            select_index_future_expiry(&dates, d("2026-07-31")),
            Some(d("2026-08-27")),
            "T+1 advances automatically (expired contract fails >= today)"
        );
    }

    #[test]
    fn test_select_index_future_expiry_none_when_all_past() {
        let dates = [d("2026-06-25"), d("2026-06-26")];
        assert_eq!(select_index_future_expiry(&dates, d("2026-07-08")), None);
    }

    // PR-C3 (2026-07-14): `test_index_future_selection_never_rolls_unlike_stocks`
    // — the comparative pin against the STOCK T-0 roll selector — retired with
    // its import (`subscription_planner::select_stock_expiry_with_rollover`,
    // deleted with the Dhan subscription planner per the scope-lock amendment
    // §B item 2). The index-side half of the divergence stays pinned:
    #[test]
    fn test_index_future_selection_never_rolls_on_t_zero() {
        let expiry = d("2026-07-30"); // a Thursday
        let next = d("2026-08-27");
        let dates = [expiry, next];
        let index = select_index_future_expiry(&dates, expiry);
        assert_eq!(index, Some(expiry), "index futures NEVER roll on T-0");
    }

    // ---- cross-feed parity ----

    fn feed_sel(canonical: &'static str, expiry: &str, id: &str) -> FeedFutureSelection {
        FeedFutureSelection {
            canonical,
            expiry: d(expiry),
            native_id: id.to_string(),
            segment: "NSE_FNO".to_string(),
        }
    }

    #[test]
    fn test_compare_index_future_selections_ok_on_identical_pairs() {
        let dhan = vec![
            feed_sel("NIFTY", "2026-07-30", "35001"),
            feed_sel("BANKNIFTY", "2026-07-30", "35002"),
        ];
        // Same (canonical, expiry) pairs; DIFFERENT native ids (id spaces
        // differ by design — parity maps by contract, never by id).
        let groww = vec![
            feed_sel("NIFTY", "2026-07-30", "99001"),
            feed_sel("BANKNIFTY", "2026-07-30", "99002"),
        ];
        assert!(compare_index_future_selections(&dhan, &groww).is_empty());
    }

    #[test]
    fn test_cross_feed_parity_multiple_months_identical_ok() {
        // §36.7: identical 3-month sets across all 4 canonicals → empty.
        // Fixes the pre-2026-07-10 `.find()` first-entry comparator bug —
        // the multi-month Vec must compare as a SET, not by first entry.
        let months = ["2026-07-30", "2026-08-27", "2026-09-24"];
        let mut dhan = Vec::new();
        let mut groww = Vec::new();
        for (u, base) in [
            ("NIFTY", 35000),
            ("BANKNIFTY", 35100),
            ("MIDCPNIFTY", 35200),
            ("SENSEX", 45000),
        ] {
            for (i, m) in months.iter().enumerate() {
                dhan.push(feed_sel(u, m, &format!("{}", base + i)));
                groww.push(feed_sel(u, m, &format!("{}", base + 900 + i)));
            }
        }
        // Order-independence of the set compare: reverse one side.
        groww.reverse();
        assert!(compare_index_future_selections(&dhan, &groww).is_empty());
    }

    #[test]
    fn test_cross_feed_parity_flags_expiry_mismatch() {
        // Nearest-month divergence → Divergence (pages FUTIDX-02).
        let dhan = vec![feed_sel("NIFTY", "2026-07-30", "35001")];
        let groww = vec![feed_sel("NIFTY", "2026-08-27", "99001")];
        let mismatches = compare_index_future_selections(&dhan, &groww);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].canonical, "NIFTY");
        assert_eq!(mismatches[0].dhan_only, vec![d("2026-07-30")]);
        assert_eq!(mismatches[0].groww_only, vec![d("2026-08-27")]);
        assert_eq!(mismatches[0].kind, ParityMismatchKind::Divergence);
    }

    #[test]
    fn test_cross_feed_parity_flags_one_sided_underlying() {
        // A whole underlying present on only one feed stays Divergence —
        // the other feed's set is EMPTY, so this is never depth-only.
        let dhan = vec![feed_sel("SENSEX", "2026-07-31", "45001")];
        let groww: Vec<FeedFutureSelection> = Vec::new();
        let mismatches = compare_index_future_selections(&dhan, &groww);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].canonical, "SENSEX");
        assert_eq!(mismatches[0].dhan_only, vec![d("2026-07-31")]);
        assert!(mismatches[0].groww_only.is_empty());
        assert_eq!(mismatches[0].kind, ParityMismatchKind::Divergence);
    }

    #[test]
    fn test_cross_feed_parity_far_suffix_is_depth_only() {
        // §36.7: dhan {Jul,Aug,Sep} vs groww {Jul,Aug} — the one-sided Sep
        // lies strictly beyond groww's max → DepthOnly (info, never a page).
        let dhan = vec![
            feed_sel("NIFTY", "2026-07-30", "35001"),
            feed_sel("NIFTY", "2026-08-27", "35002"),
            feed_sel("NIFTY", "2026-09-24", "35003"),
        ];
        let groww = vec![
            feed_sel("NIFTY", "2026-07-30", "99001"),
            feed_sel("NIFTY", "2026-08-27", "99002"),
        ];
        let mismatches = compare_index_future_selections(&dhan, &groww);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].canonical, "NIFTY");
        assert_eq!(mismatches[0].dhan_only, vec![d("2026-09-24")]);
        assert!(mismatches[0].groww_only.is_empty());
        assert_eq!(mismatches[0].kind, ParityMismatchKind::DepthOnly);
    }

    #[test]
    fn test_cross_feed_parity_hole_is_divergence() {
        // §36.7: dhan {Jul,Sep} vs groww {Jul,Aug,Sep} — the one-sided Aug
        // sits BELOW dhan's max (a HOLE) → Divergence (pages FUTIDX-02).
        let dhan = vec![
            feed_sel("NIFTY", "2026-07-30", "35001"),
            feed_sel("NIFTY", "2026-09-24", "35003"),
        ];
        let groww = vec![
            feed_sel("NIFTY", "2026-07-30", "99001"),
            feed_sel("NIFTY", "2026-08-27", "99002"),
            feed_sel("NIFTY", "2026-09-24", "99003"),
        ];
        let mismatches = compare_index_future_selections(&dhan, &groww);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0].dhan_only.is_empty());
        assert_eq!(mismatches[0].groww_only, vec![d("2026-08-27")]);
        assert_eq!(mismatches[0].kind, ParityMismatchKind::Divergence);
    }

    #[test]
    fn test_join_dates_renders_sorted_list_and_dash_for_empty() {
        assert_eq!(join_dates(&[]), "-");
        assert_eq!(
            join_dates(&[d("2026-08-27"), d("2026-09-24")]),
            "2026-08-27,2026-09-24"
        );
    }

    #[test]
    fn test_record_index_future_selection_fires_verdict_only_when_both_feeds() {
        // Exercises the recorder core + the pub wrapper
        // (record_index_future_selection) end-to-end on the global store.
        let local = Mutex::new(HashMap::new());
        let day = d("2026-07-08");
        let dhan = vec![feed_sel("NIFTY", "2026-07-30", "35001")];
        assert_eq!(
            record_selection_in(&local, "dhan", day, dhan.clone()),
            RecordOutcome::SingleFeed,
            "single feed → no verdict"
        );
        let groww = vec![feed_sel("NIFTY", "2026-07-30", "99001")];
        let RecordOutcome::Verdict(verdict) =
            record_selection_in(&local, "groww", day, groww.clone())
        else {
            panic!("both feeds same date → verdict");
        };
        assert!(verdict.is_empty(), "identical pairs → parity OK");

        // The pub wrapper runs the same core against the process-global
        // store. HONEST NOTE (hostile-review round 1, 2026-07-08): other
        // tests ALSO write the global via the production entry points
        // (`build_universe_from_bytes` records "dhan",
        // `build_groww_watch_from_csvs` records "groww"), so global-store
        // emissions here are test-log noise only — no assertion in this
        // suite reads the global verdict, and none may (order-flaky).
        record_index_future_selection("dhan", day, dhan);
        record_index_future_selection("groww", day, groww);
    }

    #[test]
    fn test_record_selection_cross_date_refuses_compare_and_evicts_stale() {
        // Day-D Dhan vs day-D+1 Groww (the expiry-boundary re-activation
        // case) → NO verdict, the OLDER (dhan) entry evicted.
        let local = Mutex::new(HashMap::new());
        let dhan = vec![feed_sel("NIFTY", "2026-07-30", "35001")];
        assert_eq!(
            record_selection_in(&local, "dhan", d("2026-07-08"), dhan),
            RecordOutcome::SingleFeed
        );
        // Next-day Groww selects the NEXT contract — a real vendor master
        // would NOT be divergent; pre-fix this paged a false FUTIDX-02.
        let groww = vec![feed_sel("NIFTY", "2026-08-27", "99001")];
        assert_eq!(
            record_selection_in(&local, "groww", d("2026-07-09"), groww.clone()),
            RecordOutcome::CrossDateSkipped {
                evicted_feed: "dhan"
            },
            "cross-date → refused + stale dhan evicted"
        );
        // A fresh SAME-date Dhan record now pairs cleanly.
        let dhan2 = vec![feed_sel("NIFTY", "2026-08-27", "35002")];
        let RecordOutcome::Verdict(verdict) =
            record_selection_in(&local, "dhan", d("2026-07-09"), dhan2)
        else {
            panic!("same-date re-pair → verdict");
        };
        assert!(verdict.is_empty(), "re-paired same-date selections match");
    }

    #[test]
    fn test_record_selection_mismatch_verdict_through_recorder() {
        // Drives the MISMATCH arm through the recorder core (hostile-review
        // round 1 coverage gap): same date, differing expiries → non-empty
        // verdict naming the underlying.
        let local = Mutex::new(HashMap::new());
        let day = d("2026-07-08");
        let dhan = vec![feed_sel("NIFTY", "2026-07-30", "35001")];
        assert_eq!(
            record_selection_in(&local, "dhan", day, dhan),
            RecordOutcome::SingleFeed
        );
        let groww = vec![feed_sel("NIFTY", "2026-08-27", "99001")];
        let RecordOutcome::Verdict(verdict) = record_selection_in(&local, "groww", day, groww)
        else {
            panic!("both feeds same date → verdict");
        };
        assert_eq!(verdict.len(), 1);
        assert_eq!(verdict[0].canonical, "NIFTY");
        assert_eq!(verdict[0].dhan_only, vec![d("2026-07-30")]);
        assert_eq!(verdict[0].groww_only, vec![d("2026-08-27")]);
        assert_eq!(verdict[0].kind, ParityMismatchKind::Divergence);

        // And the same mismatch THROUGH the pub recorder (executes the
        // error!(FUTIDX-02) emit loop — log/counter noise is expected and
        // harmless in tests; no assertion reads the global store).
        record_index_future_selection(
            "dhan",
            day,
            vec![feed_sel("BANKNIFTY", "2026-07-30", "35002")],
        );
        record_index_future_selection(
            "groww",
            day,
            vec![feed_sel("BANKNIFTY", "2026-08-27", "99002")],
        );
    }
}
