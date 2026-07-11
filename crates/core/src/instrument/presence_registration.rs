//! Dhan-side presence-registry registration — scoreboard PR-D.
//!
//! Builds the [`PresenceRegistration`] set from a BUILT [`DailyUniverse`]
//! (the same seam as `record_dhan_selection_from_universe`, so BOTH boot
//! paths — the cold orchestrator Step 3d AND the §29 warm-snapshot path —
//! register identically; the FUTIDX-02 warm-path lesson, round 2
//! 2026-07-08). Pure derivation + a thin recording wrapper.
//!
//! Cross-feed pairing keys (shared canonical space with the Groww watch
//! build — see `feed_presence` module doc):
//! - Index → `PairingKey::Index(canonicalize_index_symbol(symbol))`
//! - Stock (F&O underlying / NTM constituent) → `PairingKey::Isin` when
//!   the CSV row carries one; ISIN-less rows register as feed-local
//!   singletons (reported at drain, never dropped — Rule 11).
//! - IndexFuture → `PairingKey::Future { canonical underlying, expiry }`
//!   (contract identity — native ids never pair across feeds).
//!
//! Segment derivation mirrors `build_subscription_plan_from_daily_universe`
//! exactly (role-authoritative; FUTIDX from the CSV row) so the registered
//! `(security_id, segment_code)` key is byte-identical to what the tick
//! processor's hot fold sees.

use tickvault_common::feed::Feed;
use tickvault_common::types::ExchangeSegment;

use crate::pipeline::feed_presence::{self, PairingKey, PresenceRegistration};

use super::daily_universe::{DailyUniverse, InstrumentRole};
use super::index_extractor::canonicalize_index_symbol;

/// Days from CE (0001-01-01 = day 1) of the Unix epoch 1970-01-01 —
/// pinned by `test_ist_day_from_date_epoch_anchor`.
const EPOCH_DAYS_FROM_CE: i32 = 719_163;

/// IST day number (days since 1970-01-01) for an IST-naive date — the SAME
/// day-number convention the 15:45 scoreboard task uses
/// (`ist_secs.div_euclid(86_400)`), so registration days and drain targets
/// always agree.
#[must_use]
pub fn ist_day_from_date(d: chrono::NaiveDate) -> u64 {
    use chrono::Datelike;
    u64::try_from(d.num_days_from_ce().saturating_sub(EPOCH_DAYS_FROM_CE)).unwrap_or(0)
}

/// Pure derivation: one [`PresenceRegistration`] per subscription target.
/// Rows with an unparsable SID or an unknown FUTIDX segment are skipped
/// (the planner already counts + pages those — this derivation must never
/// register a key the hot fold can't see).
///
/// O(1) EXEMPT: cold-path boot derivation, once per universe build.
#[must_use]
pub fn dhan_presence_registrations(universe: &DailyUniverse) -> Vec<PresenceRegistration> {
    let mut out = Vec::with_capacity(universe.subscription_targets.len());
    for target in &universe.subscription_targets {
        let Ok(security_id) = target.csv_row.security_id.parse::<u64>() else {
            continue;
        };
        let (segment, pairing) = match target.role {
            InstrumentRole::Index => (
                ExchangeSegment::IdxI,
                Some(PairingKey::Index(canonicalize_index_symbol(
                    &target.csv_row.symbol_name,
                ))),
            ),
            InstrumentRole::FnoUnderlying | InstrumentRole::IndexConstituent => {
                let isin = target.csv_row.isin.trim();
                (
                    ExchangeSegment::NseEquity,
                    (!isin.is_empty()).then(|| PairingKey::Isin(isin.to_string())),
                )
            }
            InstrumentRole::IndexFuture => {
                let seg = match target.csv_row.segment.as_str() {
                    "NSE_FNO" => ExchangeSegment::NseFno,
                    "BSE_FNO" => ExchangeSegment::BseFno,
                    // Fail-closed skip — the planner pages this drop class.
                    _ => continue,
                };
                (
                    seg,
                    Some(PairingKey::Future {
                        underlying: canonicalize_index_symbol(&target.csv_row.underlying_symbol),
                        expiry: target.csv_row.expiry_date.trim().to_string(),
                    }),
                )
            }
        };
        out.push(PresenceRegistration {
            security_id,
            segment_code: segment.binary_code(),
            segment_label: segment.as_str(),
            symbol: target.csv_row.symbol_name.clone(),
            pairing,
        });
    }
    out
}

/// Registers the Dhan universe into the process-global presence registry
/// for one IST day. Thin wrapper over the pure derivation — called next to
/// `record_dhan_selection_from_universe` at BOTH boot seams (cold
/// orchestrator Step 3d + warm-snapshot path). No-op (logged debug) when
/// the registry is uninitialized (presence fold disabled).
// TEST-EXEMPT: thin wrapper over dhan_presence_registrations (tested) + feed_presence::register_instruments (tested); call sites pinned by test_dhan_presence_registration_sites_wired.
pub fn register_dhan_presence_from_universe(universe: &DailyUniverse, ist_day: u64) {
    let regs = dhan_presence_registrations(universe);
    match feed_presence::register_instruments(Feed::Dhan, ist_day, &regs) {
        Some(summary) => {
            if summary.overflow_dropped > 0 {
                tracing::error!(
                    code = tickvault_common::error_code::ErrorCode::Scoreboard01AggregationDegraded
                        .code_str(),
                    stage = "presence_register_overflow",
                    overflow_dropped = summary.overflow_dropped,
                    "SCOREBOARD-01: presence slot table overflowed — coverage \
                     rows for the dropped instruments are missing today \
                     (raise MAX_PRESENCE_SLOTS)"
                );
            }
            tracing::info!(
                feed = "dhan",
                registered = summary.registered,
                paired = summary.paired,
                ist_day,
                "feed_presence: Dhan universe registered for per-instrument \
                 coverage tracking"
            );
        }
        None => {
            tracing::debug!(
                "feed_presence: registry uninitialized (presence fold \
                 disabled) — Dhan registration skipped"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::csv_parser::CsvRow;
    use crate::instrument::daily_universe::SubscriptionTarget;

    fn row(sid: &str, segment: &str, symbol: &str, isin: &str) -> CsvRow {
        CsvRow {
            security_id: sid.to_string(),
            exch_id: "NSE".to_string(),
            segment: segment.to_string(),
            instrument: "EQUITY".to_string(),
            symbol_name: symbol.to_string(),
            underlying_security_id: String::new(),
            underlying_symbol: String::new(),
            display_name: symbol.to_string(),
            lot_size: 0,
            tick_size: 0.05,
            expiry_date: String::new(),
            strike_price: 0.0,
            option_type: String::new(),
            isin: isin.to_string(),
        }
    }

    fn target(role: InstrumentRole, csv_row: CsvRow) -> SubscriptionTarget {
        SubscriptionTarget {
            role,
            is_fno_underlying: matches!(role, InstrumentRole::FnoUnderlying),
            is_index_constituent: matches!(role, InstrumentRole::IndexConstituent),
            csv_row,
        }
    }

    #[test]
    fn test_dhan_presence_registrations_cover_all_roles_with_correct_keys() {
        let mut fut_row = row("428001", "BSE_FNO", "SENSEX-Jul2026-FUT", "");
        fut_row.underlying_symbol = "SENSEX".to_string();
        fut_row.expiry_date = "2026-07-30".to_string();
        let universe = DailyUniverse {
            subscription_targets: vec![
                target(InstrumentRole::Index, row("13", "I", "NIFTY", "")),
                target(
                    InstrumentRole::FnoUnderlying,
                    row("2885", "E", "RELIANCE", "INE002A01018"),
                ),
                target(
                    InstrumentRole::IndexConstituent,
                    row("777", "E", "NOISINSTOCK", ""),
                ),
                target(InstrumentRole::IndexFuture, fut_row),
            ],
            fno_contracts: vec![],
        };
        let regs = dhan_presence_registrations(&universe);
        assert_eq!(regs.len(), 4);
        // Index: IDX_I + canonical index pairing.
        assert_eq!(regs[0].segment_code, 0);
        assert_eq!(regs[0].segment_label, "IDX_I");
        assert_eq!(
            regs[0].pairing,
            Some(PairingKey::Index("NIFTY".to_string()))
        );
        // Stock with ISIN: NSE_EQ + ISIN pairing.
        assert_eq!(regs[1].segment_code, 1);
        assert_eq!(
            regs[1].pairing,
            Some(PairingKey::Isin("INE002A01018".to_string()))
        );
        // ISIN-less stock: registered as a feed-local singleton — never
        // dropped (Rule 11).
        assert_eq!(regs[2].pairing, None);
        assert_eq!(regs[2].segment_label, "NSE_EQ");
        // FUTIDX: segment from the CSV row (SENSEX = BSE_FNO, code 8) +
        // contract-identity pairing.
        assert_eq!(regs[3].segment_code, 8);
        assert_eq!(
            regs[3].pairing,
            Some(PairingKey::Future {
                underlying: "SENSEX".to_string(),
                expiry: "2026-07-30".to_string(),
            })
        );
    }

    #[test]
    fn test_dhan_presence_registrations_skip_unparsable_sid_and_bad_segment() {
        let mut bad_fut = row("999", "NSE_CURRENCY", "BAD-FUT", "");
        bad_fut.underlying_symbol = "NIFTY".to_string();
        bad_fut.expiry_date = "2026-07-30".to_string();
        let universe = DailyUniverse {
            subscription_targets: vec![
                target(InstrumentRole::Index, row("not-a-sid", "I", "NIFTY", "")),
                target(InstrumentRole::IndexFuture, bad_fut),
            ],
            fno_contracts: vec![],
        };
        assert!(dhan_presence_registrations(&universe).is_empty());
    }

    #[test]
    fn test_ist_day_from_date_epoch_anchor() {
        let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1);
        assert_eq!(epoch.map(ist_day_from_date), Some(0));
        // 2026-07-10 = 20_644 days after the epoch (cross-checked against
        // the scoreboard task's div_euclid(86_400) convention).
        let day = chrono::NaiveDate::from_ymd_opt(2026, 7, 10);
        assert_eq!(day.map(ist_day_from_date), Some(20_644));
    }

    /// Wiring ratchet: BOTH Dhan boot seams (the cold orchestrator Step 3d
    /// + the main.rs warm-snapshot path) must register the universe into
    /// the presence registry — next to their `record_dhan_selection_from_
    /// universe` calls (the FUTIDX-02 warm-path lesson: a seam wired on
    /// only one path silently loses the other for the whole trading day).
    #[test]
    fn test_dhan_presence_registration_sites_wired() {
        let orchestrator = include_str!("daily_universe_orchestrator.rs");
        assert!(
            orchestrator.contains("register_dhan_presence_from_universe("),
            "daily_universe_orchestrator.rs must register Dhan presence at Step 3d"
        );
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        assert!(
            main_rs.contains("register_dhan_presence_from_universe("),
            "main.rs warm-snapshot path must register Dhan presence"
        );
    }

    /// Wiring ratchet: the two Dhan persist arms in tick_processor.rs must
    /// fold presence (mirror of the feed_lag producer-site ratchet).
    #[test]
    fn test_dhan_presence_fold_sites_wired_into_tick_processor() {
        let src = include_str!("../pipeline/tick_processor.rs");
        let needle = "feed_presence::record_presence(";
        let count = src.matches(needle).count();
        assert_eq!(
            count, 2,
            "tick_processor.rs must fold presence at BOTH Dhan persist arms \
             (Ticker/Quote + Full) — found {count} call(s)"
        );
    }
}
