//! §36 (2026-07-08) — nearest-expiry index-futures selection, shared by BOTH
//! feeds (Dhan daily-universe build + Groww watch-set build).
//!
//! Operator verbatim quote (2026-07-08, recorded in
//! `daily-universe-scope-expansion-2026-05-27.md` §36): "for both dhan and
//! groww we need to add futures and those also should be subscribed along
//! with this, especially only for nifty banknifty and sensex nifty midcap."
//!
//! ## The contract (LOCKED — rule file first)
//!
//! - Exactly FOUR underlyings, pinned by [`INDEX_FUTURES_UNDERLYINGS`]:
//!   NIFTY / BANKNIFTY / MIDCPNIFTY (NSE_FNO) + SENSEX (BSE_FNO).
//! - NEAREST expiry only: first expiry `>= today` ([`select_index_future_expiry`]).
//! - Index futures NEVER roll — the T-0 expiring contract stays selected
//!   through the 15:30 close (`subscription_planner.rs` "FUTIDX → NEVER roll"
//!   lock + `test_index_expiry_never_rolls_via_planner`). The next trading
//!   day's build advances automatically because `expiry < today` fails the
//!   `>= today` filter. Deliberately NO `TradingCalendar` parameter — the
//!   calendar arm exists only to trigger the STOCK T-0 roll, which is banned
//!   for index futures; omitting the parameter makes accidental roll
//!   activation unrepresentable.
//! - Per-underlying DEGRADE, never a build failure: a miss is returned to the
//!   caller, which pages `FUTIDX-01` and continues with the resolved subset.
//! - Cross-feed parity: both feeds record their selection here; the moment
//!   BOTH are present, any expiry divergence pages `FUTIDX-02`.
//!
//! COLD PATH — every function here runs once per feed per boot/activation.

#![cfg(feature = "daily_universe_fetcher")]

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use chrono::NaiveDate;
use tickvault_common::error_code::ErrorCode;
use tracing::{error, info};

use super::csv_parser::CsvRow;
use super::index_extractor::canonicalize_index_symbol;

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
/// `daily_universe_scope_guard.rs::futidx_scope_pinned_to_4_underlyings_nearest_expiry`).
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

/// Hard cap on selected index-future targets — one nearest-expiry contract
/// per authorized underlying, never more.
pub const MAX_INDEX_FUTURE_TARGETS: usize = 4;

/// Bound on the distinct `underlying_symbol` evidence values carried in a
/// [`IndexFutureSelection`] (the FUTIDX-01 alias-drift payload).
const MAX_UNDERLYING_SYMBOLS_EVIDENCE: usize = 20;

/// THE shared boundary rule — both feeds call exactly this.
///
/// Nearest = first expiry `>= today`. NEVER rolls (index futures trade to
/// expiry — contrast `select_stock_expiry_with_rollover`'s stock-only T-0
/// roll). NO calendar parameter, deliberately: accidental roll activation is
/// unrepresentable. `expiry_dates_sorted_asc` MUST be sorted ascending.
#[must_use]
pub fn select_index_future_expiry(
    expiry_dates_sorted_asc: &[NaiveDate],
    today_ist: NaiveDate,
) -> Option<NaiveDate> {
    expiry_dates_sorted_asc
        .iter()
        .copied()
        .find(|d| *d >= today_ist)
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
}

/// One degraded underlying (FUTIDX-01 unit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexFutureMiss {
    pub canonical: &'static str,
    pub reason: IndexFutureMissReason,
}

/// Output of [`select_index_future_contracts`] — ≤4 chosen rows + the misses
/// the caller pages, plus bounded alias-drift evidence.
#[derive(Debug, Clone, Default)]
pub struct IndexFutureSelection {
    /// The chosen FUTIDX rows (≤ [`MAX_INDEX_FUTURE_TARGETS`]).
    pub chosen: Vec<CsvRow>,
    /// Per-underlying degrades — the caller emits `FUTIDX-01` per entry.
    pub misses: Vec<IndexFutureMiss>,
    /// Distinct `underlying_symbol` literals seen among FUTIDX rows (bounded
    /// to [`MAX_UNDERLYING_SYMBOLS_EVIDENCE`]) — so an `INDEX_SYMBOL_ALIASES`
    /// drift (esp. MIDCPNIFTY) can be extended with evidence, never guessed.
    pub fut_underlying_symbols_seen: Vec<String>,
}

/// Dhan-side selector. Pure, O(N) single pass over `fno_contracts`, cold
/// path, once per boot. NEVER panics. NEVER returns > 4 rows.
///
/// Algorithm:
/// 1. Keep rows where `instrument == "FUTIDX"` AND
///    `canonicalize_index_symbol(underlying_symbol)` matches an
///    [`INDEX_FUTURES_UNDERLYINGS`] entry AND `segment == entry.dhan_segment`
///    (SENSEX only from BSE_FNO; the NSE three only from NSE_FNO —
///    cross-exchange listings skipped).
/// 2. Per underlying: parse `expiry_date` (`YYYY-MM-DD` from
///    `SM_EXPIRY_DATE`; unparsable → skip + `BadExpiryFormat` accounting),
///    sort asc, pick via [`select_index_future_expiry`].
/// 3. ≥2 rows at the chosen (underlying, expiry) → fail-closed miss
///    (`AmbiguousDuplicateExpiry`).
#[must_use]
pub fn select_index_future_contracts(
    fno_contracts: &[CsvRow],
    today_ist: NaiveDate,
) -> IndexFutureSelection {
    let mut selection = IndexFutureSelection::default();

    // Bounded alias-drift evidence: every distinct underlying_symbol among
    // FUTIDX rows (any exchange), so a MIDCPNIFTY literal drift is visible.
    for row in fno_contracts {
        if row.instrument != "FUTIDX" {
            continue;
        }
        if selection.fut_underlying_symbols_seen.len() >= MAX_UNDERLYING_SYMBOLS_EVIDENCE {
            break;
        }
        if !selection
            .fut_underlying_symbols_seen
            .iter()
            .any(|s| s == &row.underlying_symbol)
        {
            selection
                .fut_underlying_symbols_seen
                .push(row.underlying_symbol.clone());
        }
    }

    for entry in &INDEX_FUTURES_UNDERLYINGS {
        // Candidate rows for THIS underlying on ITS segment only.
        let mut candidates: Vec<(NaiveDate, &CsvRow)> = Vec::new();
        let mut saw_fut_row = false;
        let mut saw_bad_expiry = false;
        for row in fno_contracts {
            if row.instrument != "FUTIDX" || row.segment != entry.dhan_segment {
                continue;
            }
            if canonicalize_index_symbol(&row.underlying_symbol) != entry.canonical {
                continue;
            }
            saw_fut_row = true;
            match NaiveDate::parse_from_str(row.expiry_date.trim(), "%Y-%m-%d") {
                Ok(d) => candidates.push((d, row)),
                Err(_) => saw_bad_expiry = true,
            }
        }
        if candidates.is_empty() {
            let reason = if saw_bad_expiry {
                IndexFutureMissReason::BadExpiryFormat
            } else if saw_fut_row {
                // Unreachable today (a parsed row always lands in
                // candidates); defensive classification.
                IndexFutureMissReason::NoFutRows
            } else {
                IndexFutureMissReason::NoFutRows
            };
            selection.misses.push(IndexFutureMiss {
                canonical: entry.canonical,
                reason,
            });
            continue;
        }
        candidates.sort_by_key(|(d, _)| *d);
        let dates: Vec<NaiveDate> = candidates.iter().map(|(d, _)| *d).collect();
        let Some(chosen_expiry) = select_index_future_expiry(&dates, today_ist) else {
            selection.misses.push(IndexFutureMiss {
                canonical: entry.canonical,
                reason: IndexFutureMissReason::AllExpiriesPast,
            });
            continue;
        };
        let at_chosen: Vec<&CsvRow> = candidates
            .iter()
            .filter(|(d, _)| *d == chosen_expiry)
            .map(|(_, r)| *r)
            .collect();
        if at_chosen.len() > 1 {
            selection.misses.push(IndexFutureMiss {
                canonical: entry.canonical,
                reason: IndexFutureMissReason::AmbiguousDuplicateExpiry,
            });
            continue;
        }
        selection.chosen.push(at_chosen[0].clone());
    }
    debug_assert!(selection.chosen.len() <= MAX_INDEX_FUTURE_TARGETS);
    selection
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

/// One cross-feed divergence (FUTIDX-02 unit). `None` = missing on that feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityMismatch {
    pub canonical: &'static str,
    pub dhan_expiry: Option<NaiveDate>,
    pub groww_expiry: Option<NaiveDate>,
}

/// Pure cross-feed comparator: for every underlying present on EITHER feed,
/// flag differing expiries and one-sided presence. Empty result = parity OK.
#[must_use]
pub fn compare_index_future_selections(
    dhan: &[FeedFutureSelection],
    groww: &[FeedFutureSelection],
) -> Vec<ParityMismatch> {
    let mut mismatches = Vec::new();
    for entry in &INDEX_FUTURES_UNDERLYINGS {
        let d = dhan
            .iter()
            .find(|s| s.canonical == entry.canonical)
            .map(|s| s.expiry);
        let g = groww
            .iter()
            .find(|s| s.canonical == entry.canonical)
            .map(|s| s.expiry);
        match (d, g) {
            // Absent on BOTH feeds → not a parity question (both degraded;
            // each already paged FUTIDX-01 per-feed).
            (None, None) => {}
            (Some(de), Some(ge)) if de == ge => {}
            _ => mismatches.push(ParityMismatch {
                canonical: entry.canonical,
                dhan_expiry: d,
                groww_expiry: g,
            }),
        }
    }
    mismatches
}

/// Backing store for the per-boot cross-feed parity recorder.
static SELECTIONS: OnceLock<Mutex<HashMap<&'static str, Vec<FeedFutureSelection>>>> =
    OnceLock::new();

/// Testable core of [`record_index_future_selection`]: insert one feed's
/// selection; when the OTHER feed's entry exists, return the comparator
/// verdict (`Some(mismatches)` — possibly empty = parity OK). `None` =
/// single-feed so far, no verdict yet.
fn record_selection_in(
    map: &Mutex<HashMap<&'static str, Vec<FeedFutureSelection>>>,
    feed: &'static str,
    sel: Vec<FeedFutureSelection>,
) -> Option<Vec<ParityMismatch>> {
    let mut guard = match map.lock() {
        Ok(g) => g,
        // Poison only signals a prior panic; the data is still valid.
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.insert(feed, sel);
    let (Some(dhan), Some(groww)) = (guard.get("dhan"), guard.get("groww")) else {
        return None;
    };
    Some(compare_index_future_selections(dhan, groww))
}

/// Record one feed's index-future selection (cold path, once per feed per
/// boot/activation). Order-independent: the comparator fires the moment BOTH
/// feeds have recorded; single-feed runs never fire it. Parity OK → one
/// `info!` verdict line; mismatch → one `error!(code = "FUTIDX-02")` per
/// underlying + `tv_index_futures_parity_mismatch_total`.
pub fn record_index_future_selection(feed: &'static str, sel: Vec<FeedFutureSelection>) {
    let map = SELECTIONS.get_or_init(|| Mutex::new(HashMap::new()));
    let Some(mismatches) = record_selection_in(map, feed, sel) else {
        return;
    };
    if mismatches.is_empty() {
        info!(
            feed,
            "index-futures parity OK — both feeds selected identical (underlying, expiry) pairs"
        );
        return;
    }
    for m in &mismatches {
        error!(
            code = ErrorCode::Futidx02CrossFeedExpiryMismatch.code_str(),
            underlying = m.canonical,
            dhan_expiry = ?m.dhan_expiry,
            groww_expiry = ?m.groww_expiry,
            "index-futures cross-feed expiry mismatch — one vendor master is stale/divergent; \
             both feeds STAY LIVE, cross-feed rows for this underlying are not comparable today"
        );
        metrics::counter!("tv_index_futures_parity_mismatch_total").increment(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("test date")
    }

    fn futidx_row(underlying: &str, segment: &str, sid: &str, expiry: &str) -> CsvRow {
        let exch_id = if segment == "BSE_FNO" { "BSE" } else { "NSE" };
        CsvRow {
            security_id: sid.to_string(),
            exch_id: exch_id.to_string(),
            segment: segment.to_string(),
            instrument: "FUTIDX".to_string(),
            symbol_name: format!("{underlying}-FUT-{expiry}"),
            underlying_security_id: "26000".to_string(),
            underlying_symbol: underlying.to_string(),
            expiry_date: expiry.to_string(),
            ..CsvRow::default()
        }
    }

    /// The full 4-underlying fixture at one expiry each.
    fn four_rows(expiry: &str) -> Vec<CsvRow> {
        vec![
            futidx_row("NIFTY", "NSE_FNO", "35001", expiry),
            futidx_row("BANKNIFTY", "NSE_FNO", "35002", expiry),
            futidx_row("MIDCPNIFTY", "NSE_FNO", "35003", expiry),
            futidx_row("SENSEX", "BSE_FNO", "45001", expiry),
        ]
    }

    #[test]
    fn test_index_futures_underlyings_pinned_exactly_four() {
        assert_eq!(INDEX_FUTURES_UNDERLYINGS.len(), 4, "§36: exactly 4");
        assert_eq!(MAX_INDEX_FUTURE_TARGETS, 4);
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

    #[test]
    fn test_index_future_selection_never_rolls_unlike_stocks() {
        // Same T-0 fixture through the STOCK selector (with calendar → ROLLS)
        // vs the index selector (→ KEEPS): pins the deliberate divergence.
        use crate::instrument::subscription_planner::select_stock_expiry_with_rollover;
        use tickvault_common::config::TradingConfig;
        use tickvault_common::trading_calendar::TradingCalendar;

        let expiry = d("2026-07-30"); // a Thursday
        let next = d("2026-08-27");
        let dates = [expiry, next];
        let cfg = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        };
        let calendar = TradingCalendar::from_config(&cfg).expect("calendar must build");

        let stock = select_stock_expiry_with_rollover(&dates, expiry, Some(&calendar));
        assert_eq!(stock, Some(next), "stocks ROLL on T-0");

        let index = select_index_future_expiry(&dates, expiry);
        assert_eq!(index, Some(expiry), "index futures NEVER roll on T-0");
    }

    #[test]
    fn test_select_index_future_contracts_picks_nearest_per_underlying() {
        let mut rows = four_rows("2026-08-27");
        rows.extend(four_rows("2026-07-30")); // nearer expiry, listed second
        let sel = select_index_future_contracts(&rows, d("2026-07-08"));
        assert_eq!(sel.chosen.len(), 4);
        assert!(sel.misses.is_empty());
        for row in &sel.chosen {
            assert_eq!(row.expiry_date, "2026-07-30", "nearest expiry chosen");
            assert_eq!(row.instrument, "FUTIDX");
        }
    }

    #[test]
    fn test_select_index_future_contracts_excludes_optidx_futstk_optstk() {
        let mut rows = Vec::new();
        for (instr, sid) in [
            ("OPTIDX", "60001"),
            ("FUTSTK", "60002"),
            ("OPTSTK", "60003"),
        ] {
            let mut r = futidx_row("NIFTY", "NSE_FNO", sid, "2026-07-30");
            r.instrument = instr.to_string();
            rows.push(r);
        }
        let sel = select_index_future_contracts(&rows, d("2026-07-08"));
        assert!(sel.chosen.is_empty(), "only FUTIDX rows are selectable");
        assert_eq!(sel.misses.len(), 4, "all 4 underlyings degrade");
    }

    #[test]
    fn test_select_index_future_contracts_excludes_fifth_underlying() {
        let mut rows = four_rows("2026-07-30");
        rows.push(futidx_row("FINNIFTY", "NSE_FNO", "70001", "2026-07-30"));
        rows.push(futidx_row("BANKEX", "BSE_FNO", "70002", "2026-07-30"));
        let sel = select_index_future_contracts(&rows, d("2026-07-08"));
        assert_eq!(sel.chosen.len(), 4, "FINNIFTY/BANKEX never chosen");
        assert!(
            sel.chosen
                .iter()
                .all(|r| r.underlying_symbol != "FINNIFTY" && r.underlying_symbol != "BANKEX")
        );
    }

    #[test]
    fn test_select_index_future_contracts_fails_closed_on_duplicate_expiry() {
        let mut rows = four_rows("2026-07-30");
        // A second NIFTY row at the SAME (underlying, expiry), different SID.
        rows.push(futidx_row("NIFTY", "NSE_FNO", "35099", "2026-07-30"));
        let sel = select_index_future_contracts(&rows, d("2026-07-08"));
        assert_eq!(sel.chosen.len(), 3, "NIFTY dropped fail-closed");
        assert!(sel.chosen.iter().all(|r| r.underlying_symbol != "NIFTY"));
        assert_eq!(
            sel.misses,
            vec![IndexFutureMiss {
                canonical: "NIFTY",
                reason: IndexFutureMissReason::AmbiguousDuplicateExpiry,
            }]
        );
    }

    #[test]
    fn test_select_index_future_contracts_sensex_from_bse_fno_only() {
        // A (bogus) NSE_FNO SENSEX row must be rejected; the BSE_FNO row wins.
        let rows = vec![
            futidx_row("SENSEX", "NSE_FNO", "45099", "2026-07-24"),
            futidx_row("SENSEX", "BSE_FNO", "45001", "2026-07-31"),
        ];
        let sel = select_index_future_contracts(&rows, d("2026-07-08"));
        let sensex: Vec<&CsvRow> = sel
            .chosen
            .iter()
            .filter(|r| r.underlying_symbol == "SENSEX")
            .collect();
        assert_eq!(sensex.len(), 1);
        assert_eq!(sensex[0].segment, "BSE_FNO");
        assert_eq!(sensex[0].security_id, "45001");
    }

    #[test]
    fn test_select_index_future_contracts_canonicalizes_midcpnifty_alias() {
        // The Dhan master may carry the "NIFTY MIDCAP SELECT" literal —
        // INDEX_SYMBOL_ALIASES canonicalizes it to MIDCPNIFTY.
        let rows = vec![futidx_row(
            "NIFTY MIDCAP SELECT",
            "NSE_FNO",
            "35003",
            "2026-07-30",
        )];
        let sel = select_index_future_contracts(&rows, d("2026-07-08"));
        assert_eq!(sel.chosen.len(), 1, "alias resolves to MIDCPNIFTY");
        assert_eq!(sel.chosen[0].security_id, "35003");
    }

    #[test]
    fn test_select_index_future_contracts_missing_underlying_degrades() {
        // Only 3 of 4 underlyings have FUT rows → 3 chosen + 1 miss, no panic.
        let rows: Vec<CsvRow> = four_rows("2026-07-30")
            .into_iter()
            .filter(|r| r.underlying_symbol != "MIDCPNIFTY")
            .collect();
        let sel = select_index_future_contracts(&rows, d("2026-07-08"));
        assert_eq!(sel.chosen.len(), 3);
        assert_eq!(
            sel.misses,
            vec![IndexFutureMiss {
                canonical: "MIDCPNIFTY",
                reason: IndexFutureMissReason::NoFutRows,
            }]
        );
    }

    #[test]
    fn test_select_index_future_contracts_all_expiries_past_degrades() {
        let rows = vec![futidx_row("NIFTY", "NSE_FNO", "35001", "2026-06-25")];
        let sel = select_index_future_contracts(&rows, d("2026-07-08"));
        assert!(sel.chosen.is_empty());
        assert!(sel.misses.contains(&IndexFutureMiss {
            canonical: "NIFTY",
            reason: IndexFutureMissReason::AllExpiriesPast,
        }));
    }

    #[test]
    fn test_select_index_future_contracts_skips_unparsable_expiry_counted() {
        // One garbage-date NIFTY row + one good one → good one chosen.
        let rows = vec![
            futidx_row("NIFTY", "NSE_FNO", "35001", "garbage-date"),
            futidx_row("NIFTY", "NSE_FNO", "35004", "2026-07-30"),
        ];
        let sel = select_index_future_contracts(&rows, d("2026-07-08"));
        assert_eq!(sel.chosen.len(), 1);
        assert_eq!(sel.chosen[0].security_id, "35004");

        // ALL rows unparsable → BadExpiryFormat miss.
        let rows = vec![futidx_row("NIFTY", "NSE_FNO", "35001", "not-a-date")];
        let sel = select_index_future_contracts(&rows, d("2026-07-08"));
        assert!(sel.chosen.is_empty());
        assert!(sel.misses.contains(&IndexFutureMiss {
            canonical: "NIFTY",
            reason: IndexFutureMissReason::BadExpiryFormat,
        }));
    }

    #[test]
    fn test_selection_carries_bounded_underlying_symbol_evidence() {
        let rows = four_rows("2026-07-30");
        let sel = select_index_future_contracts(&rows, d("2026-07-08"));
        assert_eq!(sel.fut_underlying_symbols_seen.len(), 4);
        assert!(
            sel.fut_underlying_symbols_seen
                .contains(&"NIFTY".to_string())
        );
    }

    proptest::proptest! {
        /// For random contract soups: result ≤ 4, every chosen row is FUTIDX +
        /// allowlisted + expiry >= today, and the selector never panics.
        #[test]
        fn arbitrary_contract_sets_never_select_beyond_allowlist(
            rows in proptest::collection::vec(
                (
                    proptest::sample::select(vec![
                        "NIFTY", "BANKNIFTY", "MIDCPNIFTY", "SENSEX", "FINNIFTY",
                        "BANKEX", "RELIANCE", "NIFTY MIDCAP SELECT", "",
                    ]),
                    proptest::sample::select(vec![
                        "FUTIDX", "OPTIDX", "FUTSTK", "OPTSTK", "EQUITY",
                    ]),
                    proptest::sample::select(vec![
                        "NSE_FNO", "BSE_FNO", "NSE_EQ", "MCX_COMM",
                    ]),
                    proptest::sample::select(vec![
                        "2026-06-25", "2026-07-08", "2026-07-30", "2026-08-27",
                        "garbage", "",
                    ]),
                    0u32..99_999,
                ),
                0..40,
            )
        ) {
            let today = d("2026-07-08");
            let csv_rows: Vec<CsvRow> = rows
                .iter()
                .map(|(ul, instr, seg, exp, sid)| {
                    let mut r = futidx_row(ul, seg, &sid.to_string(), exp);
                    r.instrument = (*instr).to_string();
                    r
                })
                .collect();
            let sel = select_index_future_contracts(&csv_rows, today);
            proptest::prop_assert!(sel.chosen.len() <= MAX_INDEX_FUTURE_TARGETS);
            for row in &sel.chosen {
                proptest::prop_assert_eq!(&row.instrument, "FUTIDX");
                let canonical = canonicalize_index_symbol(&row.underlying_symbol);
                proptest::prop_assert!(
                    INDEX_FUTURES_UNDERLYINGS
                        .iter()
                        .any(|u| u.canonical == canonical && u.dhan_segment == row.segment)
                );
                let exp = NaiveDate::parse_from_str(&row.expiry_date, "%Y-%m-%d")
                    .expect("chosen row has a parsable expiry");
                proptest::prop_assert!(exp >= today);
            }
        }
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
    fn test_cross_feed_parity_flags_expiry_mismatch() {
        let dhan = vec![feed_sel("NIFTY", "2026-07-30", "35001")];
        let groww = vec![feed_sel("NIFTY", "2026-08-27", "99001")];
        let mismatches = compare_index_future_selections(&dhan, &groww);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].canonical, "NIFTY");
        assert_eq!(mismatches[0].dhan_expiry, Some(d("2026-07-30")));
        assert_eq!(mismatches[0].groww_expiry, Some(d("2026-08-27")));
    }

    #[test]
    fn test_cross_feed_parity_flags_one_sided_underlying() {
        let dhan = vec![feed_sel("SENSEX", "2026-07-31", "45001")];
        let groww: Vec<FeedFutureSelection> = Vec::new();
        let mismatches = compare_index_future_selections(&dhan, &groww);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].canonical, "SENSEX");
        assert_eq!(mismatches[0].dhan_expiry, Some(d("2026-07-31")));
        assert_eq!(mismatches[0].groww_expiry, None);
    }

    #[test]
    fn test_record_index_future_selection_fires_verdict_only_when_both_feeds() {
        // Exercises the recorder core + the pub wrapper
        // (record_index_future_selection) end-to-end on the global store.
        let local = Mutex::new(HashMap::new());
        let dhan = vec![feed_sel("NIFTY", "2026-07-30", "35001")];
        assert!(
            record_selection_in(&local, "dhan", dhan.clone()).is_none(),
            "single feed → no verdict"
        );
        let groww = vec![feed_sel("NIFTY", "2026-07-30", "99001")];
        let verdict = record_selection_in(&local, "groww", groww.clone())
            .expect("both feeds present → verdict");
        assert!(verdict.is_empty(), "identical pairs → parity OK");

        // The pub wrapper runs the same core against the process-global
        // store; this is the ONLY test touching the global (no interference).
        record_index_future_selection("dhan", dhan);
        record_index_future_selection("groww", groww);
    }
}
