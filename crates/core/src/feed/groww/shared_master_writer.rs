//! Groww shared-master writer (PR-A, operator lock `groww-second-feed-scope-2026-06-19.md`).
//!
//! Persists the daily Groww instrument set ([`GrowwWatchSet`]) into the SAME shared
//! QuestDB master tables Dhan uses — `instrument_lifecycle` and `index_constituency`
//! — every row tagged `feed='groww'` (operator decision 2026-06-19: "same tables +
//! feed column", NO `groww_*` master table). PR #1229 already put `feed` in the DEDUP
//! key + DDL + row struct of both tables and stamps `feed='dhan'` on the Dhan side; this
//! module fills the `feed='groww'` rows.
//!
//! ## What this is (and is NOT)
//! This is COLD-PATH master/metadata persistence, fire-and-forget + degrade-safe. It is a
//! forensic/queryable record of "which Groww instruments did we track today", NOT the
//! `ticks` path (live-feed-purity.md is untouched — no synthesized ticks anywhere) and NOT
//! on the hot/order/recovery path. A QuestDB outage only loses the best-effort `feed='groww'`
//! rows; the next idempotent boot re-runs (DEDUP UPSERT).
//!
//! ## Uniqueness (I-P1-11 + feed-in-key)
//! The DEDUP key is the composite `(security_id, exchange_segment, feed)` (lifecycle) /
//! `(ts, index_name, security_id, exchange_segment, feed)` (constituency). A Dhan row and a
//! Groww row for the same instrument id are DISTINCT rows because `feed` differs — neither
//! overwrites the other.
//!
//! ## O(1) / allocation
//! [`groww_segment_label`] returns a zero-alloc `&'static str`. The row builders run once
//! per daily watch entry (cold path, O(n) over the ~779-instrument daily set — flagged O(n)
//! honestly; this is NOT a per-tick path) and allocate owned `String`s only for the borrowed
//! row structs, exactly as the Dhan reconciler does.

use tracing::warn;

use tickvault_common::config::QuestDbConfig;

use super::instruments::{GrowwWatchSet, WatchEntry, WatchKind};

// `Feed` / `ErrorCode` / the `error!`+`info!` macros are only used by the feature-gated
// builders + persist impl (the shared master tables exist only under `daily_universe_fetcher`).
#[cfg(feature = "daily_universe_fetcher")]
use tickvault_common::error_code::ErrorCode;
#[cfg(feature = "daily_universe_fetcher")]
use tickvault_common::feed::Feed;
#[cfg(feature = "daily_universe_fetcher")]
use tracing::{error, info};

/// Returns the canonical shared-table `exchange_segment` label for a Groww watch entry as a
/// ZERO-ALLOC `&'static str`.
///
/// Mapping (Groww `exchange` ∈ {`NSE`,`BSE`}, `segment` ∈ {`CASH`,`FNO`}):
/// - [`WatchKind::IndexValue`] → `"IDX_I"` (all index values live in the index segment,
///   matching the Dhan convention — a BSE SENSEX index value is still `IDX_I`).
/// - [`WatchKind::Ltp`] CASH → `"NSE_EQ"` (NSE) / `"BSE_EQ"` (BSE).
/// - [`WatchKind::Ltp`] FNO  → `"NSE_FNO"` (NSE) / `"BSE_FNO"` (BSE).
///
/// The watch set today is only IndexValue + Ltp-CASH (`instruments.rs`); the FNO arms are
/// forward-cover for a future F&O watch set. An unrecognized `exchange` cannot occur from the
/// resolvers (they emit only `"NSE"`/`"BSE"`); it is mapped to `"NSE_EQ"` with a one-line
/// `warn!` rather than panicking, so a future code change surfaces visibly (audit Rule 11)
/// without taking down the cold-path writer.
#[must_use]
pub fn groww_segment_label(entry: &WatchEntry) -> &'static str {
    match entry.kind {
        WatchKind::IndexValue => "IDX_I",
        WatchKind::Ltp => {
            let is_fno = entry.segment.eq_ignore_ascii_case("FNO");
            match (entry.exchange.as_str(), is_fno) {
                ("NSE", false) => "NSE_EQ",
                ("BSE", false) => "BSE_EQ",
                ("NSE", true) => "NSE_FNO",
                ("BSE", true) => "BSE_FNO",
                (other, _) => {
                    warn!(
                        exchange = %other,
                        segment = %entry.segment,
                        "groww shared-master: unexpected exchange for an Ltp entry; \
                         defaulting segment label to NSE_EQ (no panic, surfaced)"
                    );
                    "NSE_EQ"
                }
            }
        }
    }
}

/// Converts a validated `YYYY-MM-DD` IST trading date into IST-midnight epoch NANOSECONDS in
/// the same convention the rest of the system uses for designated timestamps (IST epoch nanos,
/// no UTC offset added — `data-integrity.md`). Returns `0` for an unparseable date (the caller
/// already validates `watch_date`, so this is defense-in-depth, never a panic).
///
/// Feature-gated: only the (gated) persist path computes a designated `ts`.
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
fn watch_date_to_ist_midnight_nanos(watch_date: &str) -> i64 {
    let Ok(date) = chrono::NaiveDate::parse_from_str(watch_date, "%Y-%m-%d") else {
        return 0;
    };
    // Days since the Unix epoch → IST-midnight nanos (the value represents IST wall-clock
    // midnight in IST-epoch-nanos space, matching the lifecycle/constituency boot convention).
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or(date);
    let days = (date - epoch).num_days();
    days.saturating_mul(i64::from(tickvault_common::constants::SECONDS_PER_DAY))
        .saturating_mul(1_000_000_000)
}

/// Pure gate: should the master-table APPEND be SKIPPED for this run?
///
/// Returns `true` exactly when `dry_run` is set — the §27 isolation rule: in a
/// dry run the rows are still BUILT (so the would-write counts are observable),
/// but NO ILP append runs, so a Day-1 dry-run never leaks into the live tables.
/// Pure, zero-alloc, no I/O — extracted from [`persist_groww_instruments`] so the
/// dry-run decision is unit-testable without touching QuestDB.
///
/// Module-internal (`fn`, not `pub`): the only caller is [`persist_groww_instruments`]
/// in this module; the in-module `#[cfg(test)]` tests call it directly. Keeping it
/// private (no external surface) is the correct shape for a module-local pure helper.
#[must_use]
fn should_skip_master_append(dry_run: bool) -> bool {
    dry_run
}

/// Pure classifier for a master-table persist FAILURE.
///
/// Maps the failing stage (`"lifecycle"` / `"constituency"`, or any other label
/// defensively) to the `(stage, ErrorCode)` pair the degrade-safe failure arm
/// emits: the stage label echoes back verbatim for the `tv_groww_master_persist_errors_total{stage}`
/// counter, and the code is ALWAYS [`ErrorCode::GrowwMaster01PersistFailed`]
/// (wire `GROWW-MASTER-01`). TOTAL — never panics, has no else-less arm. The
/// caller only logs+counts+returns with this; it NEVER aborts the feed, an
/// order, or a tick. Extracted so the degrade contract is unit-testable.
///
/// Module-internal (`fn`, not `pub`): the only callers are the two failure arms
/// of [`persist_groww_instruments`] in this module; the in-module `#[cfg(test)]`
/// tests call it directly. Keeping it private is the correct shape for a
/// module-local pure helper.
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
fn classify_persist_failure(stage: &'static str) -> (&'static str, ErrorCode) {
    (stage, ErrorCode::GrowwMaster01PersistFailed)
}

/// Builds the `instrument_lifecycle` rows for the Groww watch set, all tagged `feed='groww'`.
///
/// Spot/index-only sentinels per the row contract (no derivatives in the watch set):
/// `lot_size=0`, `tick_size=0.0`, `expiry=0`, `strike=0.0`, `option_type=""`,
/// `underlying_security_id=0`. `instrument_type` is `INDEX` for an index value else `EQUITY`.
/// `symbol_name`/`display_name` come from the retained provenance (index → `index_name`;
/// stock → `symbol_name`), with the `exchange_token` as a last-resort non-empty fallback so a
/// row is never wholly anonymous.
///
/// Borrows from `set`; the returned rows borrow `&'static` labels + the entry strings, so the
/// caller keeps `set` alive for the append.
///
/// Feature-gated behind `daily_universe_fetcher` — the storage `InstrumentLifecycleRow` type
/// (and the whole `instrument_lifecycle` table machinery) only exists under that feature.
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
pub fn build_groww_lifecycle_rows<'a>(
    set: &'a GrowwWatchSet,
    last_update_ts_nanos: i64,
    trading_date_ist_nanos: i64,
    dry_run: bool,
) -> Vec<tickvault_storage::instrument_lifecycle_persistence::InstrumentLifecycleRow<'a>> {
    use tickvault_storage::instrument_lifecycle_persistence::{
        InstrumentLifecycleRow, LifecycleState,
    };

    // Iterate the FULL pre-cap universe (`master_entries`), NOT the capped live
    // `entries` — the master table records every resolved instrument regardless of
    // the (smaller) live-subscribe cap, exactly as the Dhan side persists its full
    // `DailyUniverse` independent of subscription.
    set.master_entries
        .iter()
        .map(|e| {
            let is_index = e.kind == WatchKind::IndexValue;
            // Prefer the retained provenance; fall back to the subscribe token so the row is
            // never anonymous. `index_name` for indices, `symbol_name` for stocks.
            let symbol_name: &str = if is_index {
                e.index_name.as_deref().unwrap_or(&e.exchange_token)
            } else {
                e.symbol_name.as_deref().unwrap_or(&e.exchange_token)
            };
            InstrumentLifecycleRow {
                last_update_ts_nanos,
                security_id: e.security_id,
                exchange_segment: groww_segment_label(e),
                // Groww exchange (`NSE`/`BSE`) — exchange_id is an optional SYMBOL.
                exchange_id: e.exchange.as_str(),
                instrument_type: if is_index { "INDEX" } else { "EQUITY" },
                symbol_name,
                display_name: symbol_name,
                underlying_security_id: 0,
                underlying_symbol: "",
                lot_size: 0,
                tick_size: 0.0,
                expiry_date_nanos: 0,
                strike_price: 0.0,
                option_type: "",
                lifecycle_state: LifecycleState::Active,
                lifecycle_state_locked: false,
                first_seen_date_nanos: trading_date_ist_nanos,
                last_seen_date_nanos: trading_date_ist_nanos,
                last_active_date_nanos: trading_date_ist_nanos,
                expired_date_nanos: 0,
                prev_symbol_chain: "",
                source_csv_sha256: "",
                dry_run,
                feed: Feed::Groww.as_str(),
            }
        })
        .collect()
}

/// Builds the `index_constituency` rows for the Groww STOCK entries, all tagged `feed='groww'`.
///
/// Only stock (Ltp) entries with a retained `symbol_name` are constituents; index values are
/// not their own constituents. `index_name` is `"NIFTY Total Market"` (the NTM membership all
/// resolved Groww stocks belong to — the `GrowwWatchSet` stock set IS the NTM-resolved set per
/// `instruments.rs`). `via_isin` is `true` (the Groww resolver joins by ISIN). Feature-gated
/// behind `daily_universe_fetcher` (the storage row type lives there).
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
pub fn build_groww_constituency_rows<'a>(
    set: &'a GrowwWatchSet,
    trading_date_ist_nanos: i64,
    dry_run: bool,
) -> Vec<tickvault_storage::index_constituency_persistence::IndexConstituencyRow<'a>> {
    use tickvault_storage::index_constituency_persistence::IndexConstituencyRow;

    /// The NTM membership every resolved Groww stock belongs to (§31.1 — the Groww watch
    /// stock set is the NIFTY-Total-Market-resolved set).
    ///
    /// PINNED ASSUMPTION (test `test_groww_constituency_index_name_is_ntm_pinned`): today
    /// the Groww watch STOCK set is EXACTLY the NIFTY-Total-Market-resolved set
    /// (`instruments.rs`), so a single `index_name` is trivially correct for every
    /// constituent row. If the Groww watch set ever resolves stocks from MORE than one
    /// index, this hardcode becomes wrong and the per-entry `index_name` must be threaded
    /// through `WatchEntry` instead — tracked as a follow-up, NOT done here (no risky
    /// refactor while NTM is the only membership). The test pins the documented assumption.
    const GROWW_NTM_INDEX_NAME: &str = "NIFTY Total Market";

    // Iterate the FULL pre-cap universe (`master_entries`), NOT the capped live
    // `entries` — every resolved NTM stock is a constituent regardless of the
    // live-subscribe cap.
    set.master_entries
        .iter()
        .filter(|e| e.kind == WatchKind::Ltp && e.symbol_name.is_some())
        .map(|e| IndexConstituencyRow {
            trading_date_ist_nanos,
            index_name: GROWW_NTM_INDEX_NAME,
            security_id: e.security_id,
            exchange_segment: groww_segment_label(e),
            symbol_name: e.symbol_name.as_deref().unwrap_or(""),
            isin: e.isin.as_deref().unwrap_or(""),
            via_isin: true,
            source: "groww",
            dry_run,
            feed: Feed::Groww.as_str(),
        })
        .collect()
}

/// Persist the Groww instrument set into the SHARED `instrument_lifecycle` +
/// `index_constituency` master tables, tagged `feed='groww'`.
///
/// COLD-PATH, fire-and-forget, degrade-safe: any failure logs `error!(code=GROWW-MASTER-01)`,
/// increments `tv_groww_master_persist_errors_total{stage}`, and RETURNS — it NEVER aborts
/// Groww activation, the live feed, or any order/tick path. When `dry_run` is true the rows are
/// built and the would-write counts logged, but NO append runs (rule §27 isolation).
///
/// `watch_date` is a validated `YYYY-MM-DD` IST date (the caller already validates it).
///
/// Feature-gated impl: both shared master tables (`instrument_lifecycle`,
/// `index_constituency`) and their row types / append fns live behind the storage
/// `daily_universe_fetcher` feature. The non-gated stub below keeps the call site compiling
/// when the feature is off (in which case the master tables themselves do not exist).
#[cfg(feature = "daily_universe_fetcher")]
// TEST-EXEMPT: cold-path ILP I/O orchestration; not unit-testable without live QuestDB. The pure builders (groww_segment_label / build_groww_lifecycle_rows / build_groww_constituency_rows) + the should_skip_master_append / classify_persist_failure gates are unit-tested below; the bulk append fns it reuses are unit-tested + boot-integration-exercised in the storage crate.
pub async fn persist_groww_instruments(
    questdb: &QuestDbConfig,
    set: &GrowwWatchSet,
    watch_date: &str,
    dry_run: bool,
) {
    let now_ist_nanos = chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
    let trading_date_ist_nanos = watch_date_to_ist_midnight_nanos(watch_date);

    let lifecycle_rows =
        build_groww_lifecycle_rows(set, now_ist_nanos, trading_date_ist_nanos, dry_run);
    let constituency_rows = build_groww_constituency_rows(set, trading_date_ist_nanos, dry_run);

    if should_skip_master_append(dry_run) {
        info!(
            lifecycle_rows = lifecycle_rows.len(),
            constituency_rows = constituency_rows.len(),
            watch_date = %watch_date,
            "[feeds] Groww shared-master DRY-RUN — rows built, NOT written (isolation)"
        );
        return;
    }

    // ── instrument_lifecycle ──
    tickvault_storage::instrument_lifecycle_persistence::ensure_instrument_lifecycle_table(questdb)
        .await;
    match tickvault_storage::instrument_lifecycle_persistence::append_instrument_lifecycle_rows(
        questdb,
        &lifecycle_rows,
    )
    .await
    {
        Ok(()) => {
            info!(
                lifecycle_rows = lifecycle_rows.len(),
                "[feeds] Groww instrument_lifecycle rows persisted (feed=groww)"
            );
        }
        Err(err) => {
            // Degrade-safe: log + count + RETURN (continue to the next table). The
            // classifier keeps the stage label + GROWW-MASTER-01 code in one place.
            let (stage, code) = classify_persist_failure("lifecycle");
            metrics::counter!("tv_groww_master_persist_errors_total", "stage" => stage)
                .increment(1);
            error!(
                code = code.code_str(),
                stage,
                rows = lifecycle_rows.len(),
                ?err,
                "[feeds] Groww instrument_lifecycle persist failed — best-effort cold-path \
                 master write; feed + ticks unaffected, next boot re-runs"
            );
        }
    }

    // ── index_constituency ──
    tickvault_storage::index_constituency_persistence::ensure_index_constituency_table(questdb)
        .await;
    match tickvault_storage::index_constituency_persistence::append_index_constituency_rows(
        questdb,
        &constituency_rows,
    )
    .await
    {
        Ok(()) => {
            info!(
                constituency_rows = constituency_rows.len(),
                "[feeds] Groww index_constituency rows persisted (feed=groww)"
            );
        }
        Err(err) => {
            // Degrade-safe: log + count + RETURN. Same classifier, never aborts.
            let (stage, code) = classify_persist_failure("constituency");
            metrics::counter!("tv_groww_master_persist_errors_total", "stage" => stage)
                .increment(1);
            error!(
                code = code.code_str(),
                stage,
                rows = constituency_rows.len(),
                ?err,
                "[feeds] Groww index_constituency persist failed — best-effort cold-path \
                 master write; feed + ticks unaffected, next boot re-runs"
            );
        }
    }
}

/// No-op fallback when `daily_universe_fetcher` is OFF — the shared master tables (and their
/// storage modules) don't exist in that build, so there is nothing to persist. Keeps the
/// `groww_activation` call site compiling identically regardless of feature.
#[cfg(not(feature = "daily_universe_fetcher"))]
// TEST-EXEMPT: empty no-op stub (feature OFF — no master surface to write); exercised by test_persist_is_noop_when_feature_off.
pub async fn persist_groww_instruments(
    _questdb: &QuestDbConfig,
    _set: &GrowwWatchSet,
    _watch_date: &str,
    _dry_run: bool,
) {
    // The `instrument_lifecycle` / `index_constituency` master tables are gated behind
    // `daily_universe_fetcher`; without it there is no master surface to write.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::groww::instruments::{GrowwWatchSet, WatchEntry, WatchKind};

    // Used only by the feature-gated row-builder tests below.
    #[cfg(feature = "daily_universe_fetcher")]
    fn stock(token: &str, isin: &str, symbol: &str) -> WatchEntry {
        WatchEntry {
            exchange: "NSE".to_string(),
            segment: "CASH".to_string(),
            exchange_token: token.to_string(),
            kind: WatchKind::Ltp,
            security_id: token.parse::<i64>().unwrap_or(0),
            isin: Some(isin.to_string()),
            symbol_name: Some(symbol.to_string()),
            index_name: None,
        }
    }

    fn index(name: &str, exchange: &str, sid: i64) -> WatchEntry {
        WatchEntry {
            exchange: exchange.to_string(),
            segment: "CASH".to_string(),
            exchange_token: name.to_string(),
            kind: WatchKind::IndexValue,
            security_id: sid,
            isin: None,
            symbol_name: None,
            index_name: Some(format!("{exchange}-{name}")),
        }
    }

    fn ltp(exchange: &str, segment: &str) -> WatchEntry {
        WatchEntry {
            exchange: exchange.to_string(),
            segment: segment.to_string(),
            exchange_token: "1".to_string(),
            kind: WatchKind::Ltp,
            security_id: 1,
            isin: Some("INE000000001".to_string()),
            symbol_name: Some("X".to_string()),
            index_name: None,
        }
    }

    fn set_of(entries: Vec<WatchEntry>) -> GrowwWatchSet {
        let indices = entries
            .iter()
            .filter(|e| e.kind == WatchKind::IndexValue)
            .count();
        let resolved_stocks = entries.iter().filter(|e| e.kind == WatchKind::Ltp).count();
        // Uncapped case: the live-subscribe set and the master set are identical.
        let master_entries = entries.clone();
        GrowwWatchSet {
            entries,
            master_entries,
            resolved_stocks,
            unresolved_stocks: Vec::new(),
            indices,
        }
    }

    /// Builds a set where the live-subscribe `entries` is CAPPED to `entries`, but
    /// the master `master_entries` is the FULL pre-cap superset — the decoupling the
    /// row builders must honor. Used by the "uses master_entries not capped" tests.
    fn capped_set(capped: Vec<WatchEntry>, full: Vec<WatchEntry>) -> GrowwWatchSet {
        let indices = full
            .iter()
            .filter(|e| e.kind == WatchKind::IndexValue)
            .count();
        let resolved_stocks = full.iter().filter(|e| e.kind == WatchKind::Ltp).count();
        GrowwWatchSet {
            entries: capped,
            master_entries: full,
            resolved_stocks,
            unresolved_stocks: Vec::new(),
            indices,
        }
    }

    // ── groww_segment_label: every (kind, exchange, segment) permutation ──

    #[test]
    fn test_groww_segment_label_idx_i() {
        assert_eq!(groww_segment_label(&index("NIFTY", "NSE", 123)), "IDX_I");
        // A BSE SENSEX index value is still IDX_I (index segment).
        assert_eq!(groww_segment_label(&index("SENSEX", "BSE", 456)), "IDX_I");
    }

    #[test]
    fn test_groww_segment_label_nse_eq() {
        assert_eq!(groww_segment_label(&ltp("NSE", "CASH")), "NSE_EQ");
    }

    #[test]
    fn test_groww_segment_label_bse_eq() {
        assert_eq!(groww_segment_label(&ltp("BSE", "CASH")), "BSE_EQ");
    }

    #[test]
    fn test_groww_segment_label_nse_fno() {
        assert_eq!(groww_segment_label(&ltp("NSE", "FNO")), "NSE_FNO");
        // Case-insensitive segment match.
        assert_eq!(groww_segment_label(&ltp("NSE", "fno")), "NSE_FNO");
    }

    #[test]
    fn test_groww_segment_label_bse_fno() {
        assert_eq!(groww_segment_label(&ltp("BSE", "FNO")), "BSE_FNO");
    }

    #[test]
    fn test_groww_segment_label_unknown_exchange_falls_back_to_nse_eq_no_panic() {
        // Cannot occur from the resolvers, but must never panic — defensive fallback.
        assert_eq!(groww_segment_label(&ltp("MCX", "CASH")), "NSE_EQ");
    }

    // ── build_groww_lifecycle_rows (feature-gated: storage row type) ──

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_lifecycle_rows_feed_is_groww() {
        let set = set_of(vec![
            stock("2885", "INE002A01018", "RELIANCE"),
            index("NIFTY", "NSE", 999),
        ]);
        let rows = build_groww_lifecycle_rows(&set, 111, 222, false);
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(r.feed, "groww");
        }
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_lifecycle_rows_index_is_index_idx_i() {
        let set = set_of(vec![index("NIFTY", "NSE", 999)]);
        let rows = build_groww_lifecycle_rows(&set, 111, 222, false);
        assert_eq!(rows[0].instrument_type, "INDEX");
        assert_eq!(rows[0].exchange_segment, "IDX_I");
        assert_eq!(rows[0].symbol_name, "NSE-NIFTY");
        assert_eq!(rows[0].security_id, 999);
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_lifecycle_rows_stock_is_equity_nse_eq() {
        let set = set_of(vec![stock("2885", "INE002A01018", "RELIANCE")]);
        let rows = build_groww_lifecycle_rows(&set, 111, 222, false);
        assert_eq!(rows[0].instrument_type, "EQUITY");
        assert_eq!(rows[0].exchange_segment, "NSE_EQ");
        assert_eq!(rows[0].symbol_name, "RELIANCE");
        assert_eq!(rows[0].security_id, 2885);
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_lifecycle_rows_spot_sentinels() {
        let set = set_of(vec![stock("2885", "INE002A01018", "RELIANCE")]);
        let rows = build_groww_lifecycle_rows(&set, 111, 222, false);
        let r = &rows[0];
        assert_eq!(r.lot_size, 0);
        assert_eq!(r.tick_size, 0.0);
        assert_eq!(r.expiry_date_nanos, 0);
        assert_eq!(r.strike_price, 0.0);
        assert_eq!(r.option_type, "");
        assert_eq!(r.underlying_security_id, 0);
        assert_eq!(r.underlying_symbol, "");
        assert_eq!(r.last_update_ts_nanos, 111);
        assert_eq!(r.first_seen_date_nanos, 222);
        assert!(r.lifecycle_state.is_active());
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_no_row_has_empty_feed() {
        // A feed-less / empty-feed row must be impossible: the builder always stamps a
        // non-empty feed, even when isin/symbol provenance is absent.
        let mut anon = stock("7", "X", "Y");
        anon.isin = None;
        anon.symbol_name = None;
        let set = set_of(vec![anon, index("NIFTY", "NSE", 1)]);
        let rows = build_groww_lifecycle_rows(&set, 1, 2, false);
        for r in &rows {
            assert!(!r.feed.is_empty(), "feed must never be empty");
            assert_eq!(r.feed, "groww");
            // Falls back to the subscribe token so the row is never anonymous.
            assert!(!r.symbol_name.is_empty());
        }
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_regression_dhan_and_groww_same_id_distinct_under_composite_key() {
        // Regression: 2026-06-28 — feed-in-key. A Dhan row and a Groww row for the SAME
        // (security_id, exchange_segment) must be DISTINCT under the composite DEDUP key
        // (security_id, exchange_segment, feed). Pure in-memory proof of the key tuple — no
        // live QuestDB. Both rows share id+segment; only `feed` differs, so the key tuples
        // are distinct and a HashSet keeps both.
        use std::collections::HashSet;
        let set = set_of(vec![stock("2885", "INE002A01018", "RELIANCE")]);
        let groww = &build_groww_lifecycle_rows(&set, 1, 2, false)[0];

        // The Dhan-side key tuple for the same instrument id+segment, feed='dhan'.
        let dhan_key = (groww.security_id, groww.exchange_segment, "dhan");
        let groww_key = (groww.security_id, groww.exchange_segment, groww.feed);

        assert_eq!(dhan_key.0, groww_key.0, "same security_id");
        assert_eq!(dhan_key.1, groww_key.1, "same exchange_segment");
        assert_ne!(dhan_key.2, groww_key.2, "feed differs");

        let mut keys: HashSet<(i64, &str, &str)> = HashSet::new();
        assert!(keys.insert(dhan_key), "dhan row inserts");
        assert!(
            keys.insert(groww_key),
            "groww row inserts as a DISTINCT row (feed differs)"
        );
        assert_eq!(
            keys.len(),
            2,
            "both feeds' rows coexist under the composite key"
        );
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_watch_date_to_ist_midnight_nanos() {
        // 1970-01-02 = 1 day after epoch = 86400 * 1e9 IST-midnight nanos.
        assert_eq!(
            watch_date_to_ist_midnight_nanos("1970-01-02"),
            86_400_i64 * 1_000_000_000
        );
        // Epoch day itself = 0.
        assert_eq!(watch_date_to_ist_midnight_nanos("1970-01-01"), 0);
        // Unparseable → 0 (defense-in-depth, no panic).
        assert_eq!(watch_date_to_ist_midnight_nanos("not-a-date"), 0);
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_constituency_rows_feed_is_groww() {
        let set = set_of(vec![
            stock("2885", "INE002A01018", "RELIANCE"),
            // An index entry is NOT its own constituent — must be filtered out.
            index("NIFTY", "NSE", 999),
        ]);
        let rows = build_groww_constituency_rows(&set, 222, false);
        assert_eq!(rows.len(), 1, "only the stock is a constituent row");
        let r = &rows[0];
        assert_eq!(r.feed, "groww");
        assert_eq!(r.symbol_name, "RELIANCE");
        assert_eq!(r.isin, "INE002A01018");
        assert_eq!(r.security_id, 2885);
        assert_eq!(r.exchange_segment, "NSE_EQ");
        assert!(r.via_isin);
        assert_eq!(r.source, "groww");
        assert_eq!(r.index_name, "NIFTY Total Market");
    }

    // ── full-universe master: builders iterate master_entries, not capped entries ──

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_lifecycle_rows_uses_master_entries_not_capped() {
        // Regression: the master must record the FULL pre-cap universe. Live
        // `entries` is capped to 2, but `master_entries` holds the full 5 → the
        // lifecycle builder MUST emit 5 rows, not 2.
        let full = vec![
            index("NIFTY", "NSE", 999),
            stock("2885", "INE002A01018", "RELIANCE"),
            stock("1594", "INE009A01021", "INFY"),
            stock("3045", "INE040A01034", "HDFCBANK"),
            stock("4963", "INE467B01029", "TCS"),
        ];
        let capped = full[..2].to_vec();
        let set = capped_set(capped, full);
        assert_eq!(set.entries.len(), 2, "live subscribe set is capped");
        let rows = build_groww_lifecycle_rows(&set, 111, 222, false);
        assert_eq!(
            rows.len(),
            5,
            "lifecycle rows must cover the full master universe, not the capped entries"
        );
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_constituency_rows_uses_master_entries_not_capped() {
        // Same decoupling for constituency: full master has 4 stocks (+1 index);
        // live `entries` capped to 1 → constituency builder MUST emit 4 rows (the
        // index is filtered out), not be limited by the cap.
        let full = vec![
            index("NIFTY", "NSE", 999),
            stock("2885", "INE002A01018", "RELIANCE"),
            stock("1594", "INE009A01021", "INFY"),
            stock("3045", "INE040A01034", "HDFCBANK"),
            stock("4963", "INE467B01029", "TCS"),
        ];
        let capped = full[..1].to_vec();
        let set = capped_set(capped, full);
        let rows = build_groww_constituency_rows(&set, 222, false);
        assert_eq!(
            rows.len(),
            4,
            "constituency rows must cover every full-master stock (4), not the capped entries"
        );
    }

    // ── F1: dry-run builds rows but skips the append (§27 isolation) ──

    #[test]
    fn test_persist_dry_run_skips_append() {
        // The pure gate: dry_run=true → SKIP append; dry_run=false → do append.
        assert!(
            should_skip_master_append(true),
            "dry_run must skip the master append (isolation)"
        );
        assert!(
            !should_skip_master_append(false),
            "live run must NOT skip the append"
        );
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_persist_dry_run_builds_rows_but_skips_append() {
        // Even when the append is skipped, the rows are still BUILT (so the
        // would-write counts are observable) — "build-but-don't-write".
        let set = set_of(vec![
            stock("2885", "INE002A01018", "RELIANCE"),
            index("NIFTY", "NSE", 999),
        ]);
        let lifecycle = build_groww_lifecycle_rows(&set, 111, 222, true);
        let constituency = build_groww_constituency_rows(&set, 222, true);
        assert!(should_skip_master_append(true), "dry-run skips the write");
        assert!(
            !lifecycle.is_empty(),
            "dry-run STILL builds lifecycle rows (build-but-don't-write)"
        );
        assert!(
            !constituency.is_empty(),
            "dry-run STILL builds constituency rows"
        );
        // The dry_run flag is stamped on every built row so the isolation column
        // (`dry_run`) is correct if these rows were ever inspected.
        assert!(lifecycle.iter().all(|r| r.dry_run));
        assert!(constituency.iter().all(|r| r.dry_run));
    }

    // ── F2: persist-failure classifies GROWW-MASTER-01, degrade-safe (no abort) ──

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_persist_failure_classifies_groww_master_01_without_abort() {
        // Pure classifier is TOTAL — every stage maps to GROWW-MASTER-01, echoing
        // the stage label back for the per-stage counter. It returns a value (it
        // never panics / aborts), proving the failure arm only logs+counts+returns.
        for stage in ["lifecycle", "constituency", "unexpected_stage"] {
            let (echoed, code) = classify_persist_failure(stage);
            assert_eq!(echoed, stage, "stage label echoes back for the counter");
            assert_eq!(
                code.code_str(),
                "GROWW-MASTER-01",
                "every persist failure is GROWW-MASTER-01"
            );
        }
        // Source-scan: the persist orchestration only log+count+returns on failure —
        // never aborts the feed. Scope the scan to the PRODUCTION fn body (between its
        // signature and the `#[cfg(not(...))]` stub) so this test's own assertion
        // strings — which necessarily NAME the banned tokens — are not self-matched.
        let file = include_str!("shared_master_writer.rs");
        let start = file
            .find("pub async fn persist_groww_instruments(")
            .expect("persist fn present");
        let end = file[start..]
            .find("#[cfg(not(feature = \"daily_universe_fetcher\"))]")
            .map(|o| start + o)
            .expect("non-gated stub follows the gated impl");
        let persist_body = &file[start..end];
        for banned in [
            "panic!(",
            "process::exit",
            "unreachable!(",
            "todo!(",
            ".abort(",
        ] {
            assert!(
                !persist_body.contains(banned),
                "degrade-safe persist_groww_instruments must never use {banned} — \
                 failure arms log+count+return"
            );
        }
        // Both failure arms must route through the classifier + bump the counter.
        assert!(
            persist_body.matches("classify_persist_failure(").count() >= 2,
            "both failure arms must route through classify_persist_failure"
        );
        assert!(
            persist_body.contains("tv_groww_master_persist_errors_total"),
            "failure arms must increment the per-stage counter"
        );
    }

    // ── F5: brownfield — a legacy feed=NULL/"" row never collides with dhan/groww ──

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_null_feed_row_distinct_from_groww_under_key() {
        // Regression: 2026-06-28 — feed-in-key brownfield. A backfilled/legacy row
        // with feed="" (NULL before self-heal stamps 'dhan') must be DISTINCT from
        // both the dhan and groww rows for the same (security_id, exchange_segment).
        use std::collections::HashSet;
        let set = set_of(vec![stock("2885", "INE002A01018", "RELIANCE")]);
        let groww = &build_groww_lifecycle_rows(&set, 1, 2, false)[0];

        let legacy_key = (groww.security_id, groww.exchange_segment, "");
        let dhan_key = (groww.security_id, groww.exchange_segment, "dhan");
        let groww_key = (groww.security_id, groww.exchange_segment, groww.feed);

        // Same id + segment across all three; only `feed` differs.
        assert_eq!(legacy_key.0, groww_key.0);
        assert_eq!(legacy_key.1, groww_key.1);
        assert_ne!(legacy_key.2, dhan_key.2);
        assert_ne!(legacy_key.2, groww_key.2);

        let mut keys: HashSet<(i64, &str, &str)> = HashSet::new();
        assert!(keys.insert(legacy_key), "legacy NULL-feed row inserts");
        assert!(keys.insert(dhan_key), "dhan row inserts distinctly");
        assert!(keys.insert(groww_key), "groww row inserts distinctly");
        assert_eq!(
            keys.len(),
            3,
            "legacy(NULL)/dhan/groww are 3 DISTINCT rows under the composite key"
        );
    }

    // ── F6: IST-midnight convention matches the Dhan reconciler date→nanos ──

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_groww_ist_midnight_matches_dhan_lifecycle_convention() {
        // The Dhan daily-universe orchestrator (crates/app/src/main.rs) computes the
        // designated trading-date nanos as:
        //     now_ist.date_naive().and_hms_opt(0,0,0).and_utc().timestamp_nanos_opt()
        // i.e. IST-midnight epoch nanos with NO UTC offset added (data-integrity.md).
        // The Groww writer's `watch_date_to_ist_midnight_nanos` must produce the
        // IDENTICAL value for the same date, else dhan + groww rows for the same date
        // would carry different `ts` and silently split. Both reduce to
        // days_since_epoch * 86400 * 1e9, so they are exactly equal for every
        // parseable date — proven here against the Dhan formula directly.
        fn dhan_convention(date: chrono::NaiveDate) -> i64 {
            date.and_hms_opt(0, 0, 0)
                .and_then(|m| m.and_utc().timestamp_nanos_opt())
                .unwrap_or(0)
        }
        for ymd in [
            "1970-01-01",
            "1970-01-02",
            "2025-12-25",
            "2026-06-28",
            "2024-02-29", // leap day
        ] {
            let date = chrono::NaiveDate::parse_from_str(ymd, "%Y-%m-%d").unwrap();
            assert_eq!(
                watch_date_to_ist_midnight_nanos(ymd),
                dhan_convention(date),
                "Groww IST-midnight nanos must equal the Dhan reconciler convention for {ymd}"
            );
        }
        // Unparseable → 0 (defense-in-depth), never a panic.
        assert_eq!(watch_date_to_ist_midnight_nanos("not-a-date"), 0);
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_groww_constituency_index_name_is_ntm_pinned() {
        // Pins the documented single-membership assumption: today the Groww watch
        // stock set is exactly the NIFTY-Total-Market-resolved set, so every
        // constituency row carries `index_name = "NIFTY Total Market"`. If the watch
        // set ever spans multiple indices, this test fails — forcing the per-entry
        // index_name follow-up noted in the builder's doc comment.
        let set = set_of(vec![
            stock("2885", "INE002A01018", "RELIANCE"),
            stock("1594", "INE009A01021", "INFY"),
        ]);
        let rows = build_groww_constituency_rows(&set, 222, false);
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter().all(|r| r.index_name == "NIFTY Total Market"),
            "all Groww constituents are pinned to the single NTM membership"
        );
    }

    // ── F3: feature OFF — persist_groww_instruments is a compiled no-op ──

    #[cfg(not(feature = "daily_universe_fetcher"))]
    #[tokio::test]
    async fn test_persist_is_noop_when_feature_off() {
        // With `daily_universe_fetcher` OFF the shared master tables don't exist, so
        // the stub must compile + return WITHOUT touching QuestDB. We call it with an
        // empty set + a bogus questdb config; it must return cleanly (no connect, no
        // panic). Proves the call site compiles identically regardless of feature.
        use tickvault_common::config::QuestDbConfig;
        let set = set_of(vec![]);
        let questdb = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        // Returns () without any network I/O — the stub body is empty.
        persist_groww_instruments(&questdb, &set, "2026-06-28", false).await;
    }
}
