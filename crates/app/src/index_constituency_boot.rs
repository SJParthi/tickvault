//! `index_constituency` boot population — NTM Sub-PR "Map-B" (operator
//! 2026-06-06, "full per-index mapping … guarantee and assurance").
//!
//! Populates the Map-A `index_constituency` table with the FULL §31-item-2
//! mapping: for ALL ~46 tracked indices, every constituent stock resolved to its
//! Dhan `security_id` (ISIN-primary), one row per `(index, stock)`.
//!
//! ## Why this is a SEPARATE, decoupled step (operator's "don't change existing
//! logic")
//!
//! The live subscription path (NTM-union-only fetch + fold) is UNTOUCHED. This
//! module runs independently AFTER the universe build: it re-fetches the full
//! constituency map (all 46 slugs) + the Dhan master, resolves per-index, and
//! writes the mapping table. It is **map-only** — it NEVER feeds the WebSocket
//! subscription. Spawned fire-and-forget so it never delays boot or the live feed.
//!
//! ## Degrade-safe (never blocks, never panics)
//!
//! Any failure (downloader init, niftyindices fetch, Dhan CSV fetch/parse,
//! resolve) logs `warn!` and returns — the live trading path is unaffected. The
//! mapping is a best-effort observability surface, refreshed next boot.
//!
//! **Feature-gated** under `daily_universe_fetcher` per rule §21.

#![cfg(feature = "daily_universe_fetcher")]

use std::collections::HashMap;

use chrono::NaiveDate;
use tracing::{info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::INDEX_CONSTITUENCY_SLUGS;
use tickvault_common::instrument_types::IndexConstituencyMap;
use tickvault_core::instrument::constituent_resolver::{ResolvedConstituent, resolve_constituents};
use tickvault_core::instrument::csv_downloader::CsvDownloader;
use tickvault_core::instrument::csv_parser::parse_detailed_csv;
use tickvault_core::instrument::index_constituency::build_constituency_map;
use tickvault_core::instrument::index_constituency::downloader::ConstituencyDownloader;
use tickvault_storage::index_constituency_persistence::{
    INDEX_CONSTITUENCY_FEED_DHAN, IndexConstituencyRow, append_index_constituency_rows,
    ensure_index_constituency_table, migrate_index_constituency_truncate_once,
};

/// One fully-resolved (index, stock) mapping row — OWNED so the pure builder is
/// unit-testable without borrows; the writer converts to the borrowing
/// [`IndexConstituencyRow`] for the ILP flush.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedConstituencyRow {
    pub index_name: String,
    pub security_id: i64,
    pub exchange_segment: String,
    pub symbol_name: String,
    pub isin: String,
    pub via_isin: bool,
}

/// PURE: turn the full constituency map + the flat resolved set into per-index
/// rows. For each index, each constituent symbol is looked up (O(1)) in the
/// resolved set; unresolved constituents are SKIPPED (counted by the caller via
/// the returned-vs-input delta — they are the dangling symbols the resolver
/// already reported). A resolved `security_id` that does not parse as `i64` is
/// skipped (defensive; Dhan SIDs are numeric).
///
/// O(C) where C = total (index, constituent) pairs — cold path, runs once per
/// boot. No network, no QuestDB → fully unit-tested.
#[must_use]
pub fn build_index_constituency_rows(
    full_map: &IndexConstituencyMap,
    resolved: &[ResolvedConstituent],
) -> Vec<OwnedConstituencyRow> {
    // O(1) symbol → resolved lookup (resolved is deduped by symbol upstream).
    let by_symbol: HashMap<&str, &ResolvedConstituent> =
        resolved.iter().map(|r| (r.symbol.as_str(), r)).collect();

    let mut out: Vec<OwnedConstituencyRow> = Vec::new();
    for (index_name, constituents) in &full_map.index_to_constituents {
        for c in constituents {
            let Some(r) = by_symbol.get(c.symbol.as_str()) else {
                continue; // dangling — resolver already counted it
            };
            let Ok(security_id) = r.security_id.trim().parse::<i64>() else {
                continue; // non-numeric SID (defensive)
            };
            out.push(OwnedConstituencyRow {
                index_name: index_name.clone(),
                security_id,
                exchange_segment: r.exchange_segment.clone(),
                symbol_name: r.symbol.clone(),
                isin: r.isin.clone(),
                via_isin: r.via_isin,
            });
        }
    }
    out
}

/// Fetch the full per-index mapping + persist it to `index_constituency`.
///
/// Degrade-safe: returns `()` and logs `warn!` on any failure. Spawn this
/// fire-and-forget from boot — it NEVER blocks the live feed or trading path.
// TEST-EXEMPT: network + QuestDB I/O orchestration — the pure mapping logic is unit-tested via `build_index_constituency_rows`; the resolver/parser/downloader are tested in their own crates.
pub async fn persist_index_constituency_mapping(
    questdb: QuestDbConfig,
    today: NaiveDate,
    trading_date_ist_nanos: i64,
    dry_run: bool,
) {
    // 1. Full constituency map (all ~46 tracked indices) — map-only, best-effort.
    let downloader = match ConstituencyDownloader::new() {
        Ok(d) => d,
        Err(err) => {
            warn!(
                ?err,
                "index_constituency map: downloader init failed — skipping mapping"
            );
            return;
        }
    };
    let full_map = build_constituency_map(&downloader, INDEX_CONSTITUENCY_SLUGS, today).await;
    if full_map.index_to_constituents.is_empty() {
        warn!(
            indices_failed = full_map.build_metadata.indices_failed,
            "index_constituency map: no indices fetched — skipping mapping this boot"
        );
        return;
    }

    // 2. Dhan master (re-fetched independently so the live subscription path is
    //    untouched). Cold path, best-effort.
    let csv_downloader = match CsvDownloader::new() {
        Ok(d) => d,
        Err(err) => {
            warn!(
                ?err,
                "index_constituency map: Dhan CSV downloader init failed"
            );
            return;
        }
    };
    let bytes = match csv_downloader.fetch_csv().await {
        Ok(b) => b,
        Err(err) => {
            warn!(?err, "index_constituency map: Dhan CSV fetch failed");
            return;
        }
    };
    let rows = match parse_detailed_csv(&bytes) {
        Ok(r) => r,
        Err(err) => {
            warn!(?err, "index_constituency map: Dhan CSV parse failed");
            return;
        }
    };

    // 3. Resolve every constituent across all indices → security_id (ISIN-primary).
    let outcome = match resolve_constituents(&full_map, &rows) {
        Ok(o) => o,
        Err(err) => {
            warn!(?err, "index_constituency map: constituent resolve failed");
            return;
        }
    };

    // 4. Per-index rows (pure) → ILP rows.
    let owned = build_index_constituency_rows(&full_map, &outcome.resolved);
    let ilp_rows: Vec<IndexConstituencyRow<'_>> = owned
        .iter()
        .map(|r| IndexConstituencyRow {
            trading_date_ist_nanos,
            index_name: &r.index_name,
            security_id: r.security_id,
            exchange_segment: &r.exchange_segment,
            symbol_name: &r.symbol_name,
            isin: &r.isin,
            via_isin: r.via_isin,
            source: "niftyindices",
            dry_run,
            // operator override 2026-06-28: feed in-key (Dhan-resolved SIDs today).
            feed: INDEX_CONSTITUENCY_FEED_DHAN,
        })
        .collect();

    // 5. Persist (idempotent UPSERT). Ensure the table first (self-heal), then
    //    run the one-time, marker-gated ts-pin migration BEFORE the write so the
    //    order is truncate → write: it clears the legacy day-floored rows once
    //    (the `ts` is now pinned to epoch 0, so new writes UPSERT in place but
    //    can't overwrite the old per-day rows). Degrade-safe — never blocks.
    ensure_index_constituency_table(&questdb).await;
    migrate_index_constituency_truncate_once(&questdb).await;
    match append_index_constituency_rows(&questdb, &ilp_rows).await {
        Ok(()) => {
            info!(
                indices = full_map.index_to_constituents.len(),
                rows = ilp_rows.len(),
                unresolved = outcome.unresolved_symbols.len(),
                "index_constituency mapping persisted (map-only; subscription untouched)"
            );
        }
        Err(err) => {
            warn!(
                ?err,
                rows = ilp_rows.len(),
                "index_constituency map: persist failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::instrument_types::{
        ConstituencyBuildMetadata, IndexConstituencyMap, IndexConstituent,
    };

    fn constituent(symbol: &str, isin: &str) -> IndexConstituent {
        IndexConstituent {
            index_name: String::new(),
            symbol: symbol.to_string(),
            isin: isin.to_string(),
            weight: 0.0,
            sector: String::new(),
            last_updated: NaiveDate::from_ymd_opt(2026, 6, 6).unwrap(),
        }
    }

    fn resolved(symbol: &str, isin: &str, sid: &str, via_isin: bool) -> ResolvedConstituent {
        ResolvedConstituent {
            symbol: symbol.to_string(),
            isin: isin.to_string(),
            security_id: sid.to_string(),
            exchange_segment: "NSE_EQ".to_string(),
            via_isin,
        }
    }

    fn map_with(indices: &[(&str, &[(&str, &str)])]) -> IndexConstituencyMap {
        let mut index_to_constituents = HashMap::new();
        let mut stock_to_indices: HashMap<String, Vec<String>> = HashMap::new();
        for (idx, members) in indices {
            let cs: Vec<IndexConstituent> =
                members.iter().map(|(s, i)| constituent(s, i)).collect();
            for (s, _) in *members {
                stock_to_indices
                    .entry((*s).to_string())
                    .or_default()
                    .push((*idx).to_string());
            }
            index_to_constituents.insert((*idx).to_string(), cs);
        }
        IndexConstituencyMap {
            index_to_constituents,
            stock_to_indices,
            build_metadata: ConstituencyBuildMetadata::default(),
        }
    }

    #[test]
    fn build_index_constituency_rows_maps_each_index_membership() {
        // RELIANCE in 2 indices → 2 rows; INFY in 1 → 1 row. 3 rows total.
        let map = map_with(&[
            (
                "Nifty 50",
                &[("RELIANCE", "INE002A01018"), ("INFY", "INE009A01021")],
            ),
            ("Nifty Bank", &[("RELIANCE", "INE002A01018")]),
        ]);
        let res = vec![
            resolved("RELIANCE", "INE002A01018", "2885", true),
            resolved("INFY", "INE009A01021", "1594", true),
        ];
        let rows = build_index_constituency_rows(&map, &res);
        assert_eq!(rows.len(), 3, "RELIANCE×2 + INFY×1");
        let reliance: Vec<_> = rows
            .iter()
            .filter(|r| r.symbol_name == "RELIANCE")
            .collect();
        assert_eq!(reliance.len(), 2, "RELIANCE in both indices");
        assert!(reliance.iter().all(|r| r.security_id == 2885));
        let idx_names: Vec<&str> = reliance.iter().map(|r| r.index_name.as_str()).collect();
        assert!(idx_names.contains(&"Nifty 50") && idx_names.contains(&"Nifty Bank"));
    }

    #[test]
    fn build_rows_skips_unresolved_constituent() {
        // ZEEL has no resolved entry (dangling) → no row for it.
        let map = map_with(&[(
            "Nifty 50",
            &[("RELIANCE", "INE002A01018"), ("ZEEL", "INE256A01028")],
        )]);
        let res = vec![resolved("RELIANCE", "INE002A01018", "2885", true)];
        let rows = build_index_constituency_rows(&map, &res);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol_name, "RELIANCE");
    }

    #[test]
    fn build_rows_skips_non_numeric_security_id() {
        let map = map_with(&[("Nifty 50", &[("RELIANCE", "INE002A01018")])]);
        let res = vec![resolved("RELIANCE", "INE002A01018", "NOTANUMBER", true)];
        let rows = build_index_constituency_rows(&map, &res);
        assert!(rows.is_empty(), "non-numeric SID skipped defensively");
    }

    #[test]
    fn build_rows_preserves_via_isin_flag() {
        let map = map_with(&[("Nifty 50", &[("RELIANCE", "INE002A01018")])]);
        let res = vec![resolved("RELIANCE", "INE002A01018", "2885", false)];
        let rows = build_index_constituency_rows(&map, &res);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].via_isin, "symbol-fallback flag preserved");
    }

    #[test]
    fn build_rows_empty_map_is_empty() {
        let map = map_with(&[]);
        let rows = build_index_constituency_rows(&map, &[]);
        assert!(rows.is_empty());
    }
}
