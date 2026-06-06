//! NTM Sub-PR #3 of 2026-05-27 daily-universe expansion — rebuild of the
//! niftyindices.com index-constituency downloader + parser + cache.
//!
//! **Feature-gated** behind `daily_universe_fetcher` (per §21 of the rule
//! file). The predecessor module was deleted under the 4-IDX_I scope; the
//! `INDEX_CONSTITUENCY_*` constants + `IndexConstituent` / `IndexConstituencyMap`
//! types survived and are reused here. Boot wiring + the Dhan-master ISIN
//! join land in later sub-PRs (#4 / #10) per the §31.1 mapping contract —
//! this module ships ONLY: fetch (HTTP) → parse (CSV) → assemble
//! (`IndexConstituencyMap`) → cache (JSON), exactly the same unwired,
//! feature-gated shape as its sibling `csv_downloader`.
//!
//! ## Three layers
//!
//! - [`downloader`] — hardened `reqwest` client (redirect-none, body cap,
//!   timeouts, Content-Type allowlist, browser UA).
//! - [`parser`] — robust constituent-CSV → `Vec<IndexConstituent>`.
//! - [`cache`] — JSON round-trip with a path-traversal-guarded date key.
//!
//! `build_constituency_map` composes the downloader + parser over a slug
//! list; `assemble_map` is the PURE, network-free core (fully unit-tested).

#![cfg(feature = "daily_universe_fetcher")]

pub mod cache;
pub mod downloader;
pub mod parser;

use chrono::{NaiveDate, Utc};
use tickvault_common::instrument_types::{
    ConstituencyBuildMetadata, IndexConstituencyMap, IndexConstituent,
};

use self::downloader::ConstituencyDownloader;
use self::parser::{ParsedConstituents, parse_constituents};

/// Assemble an [`IndexConstituencyMap`] from per-index parsed rows.
///
/// PURE + network-free. Builds the forward map (index → constituents),
/// the reverse map (stock → indices, deduped), and the build metadata.
/// `indices_failed` is the count of slugs that failed to download/parse
/// upstream (passed in so the metadata reflects the full attempt set).
#[must_use]
pub fn assemble_map(
    parsed_per_index: Vec<ParsedConstituents>,
    indices_failed: usize,
    download_duration: std::time::Duration,
) -> IndexConstituencyMap {
    let mut map = IndexConstituencyMap::default();
    let mut total_mappings = 0usize;
    let mut total_missing_isin = 0usize;

    for parsed in parsed_per_index {
        total_missing_isin += parsed.missing_isin;
        if parsed.constituents.is_empty() {
            continue;
        }
        let index_name = parsed.constituents[0].index_name.clone();
        for c in &parsed.constituents {
            total_mappings += 1;
            let indices = map.stock_to_indices.entry(c.symbol.clone()).or_default();
            if !indices.iter().any(|n| n == &c.index_name) {
                indices.push(c.index_name.clone());
            }
        }
        let constituents: Vec<IndexConstituent> = parsed.constituents;
        map.index_to_constituents.insert(index_name, constituents);
    }

    let ist = tickvault_common::trading_calendar::ist_offset();
    map.build_metadata = ConstituencyBuildMetadata {
        download_duration,
        indices_downloaded: map.index_to_constituents.len(),
        indices_failed,
        unique_stocks: map.stock_to_indices.len(),
        total_mappings,
        missing_isin: total_missing_isin,
        build_timestamp: Utc::now().with_timezone(&ist),
    };
    map
}

/// Download + parse every `(display_name, slug)` in `slugs` and assemble
/// the constituency map. Per-slug failures are logged + skipped (best
/// effort — niftyindices serves some slugs as 404/HTML); the metadata
/// records how many failed.
///
/// `today` stamps `last_updated` on every constituent.
pub async fn build_constituency_map(
    downloader: &ConstituencyDownloader,
    slugs: &[(&str, &str)],
    today: NaiveDate,
) -> IndexConstituencyMap {
    let start = std::time::Instant::now();
    let mut parsed_per_index = Vec::with_capacity(slugs.len());
    let mut indices_failed = 0usize;

    for (display_name, slug) in slugs {
        match downloader.fetch_slug(slug).await {
            Ok(bytes) => match parse_constituents(&bytes, display_name, today) {
                Ok(parsed) => {
                    tracing::info!(
                        index = display_name,
                        constituents = parsed.constituents.len(),
                        missing_isin = parsed.missing_isin,
                        "constituency parsed"
                    );
                    parsed_per_index.push(parsed);
                }
                Err(err) => {
                    indices_failed += 1;
                    tracing::warn!(index = display_name, %err, "constituency parse skipped");
                }
            },
            Err(err) => {
                indices_failed += 1;
                tracing::warn!(index = display_name, %err, "constituency download skipped");
            }
        }
    }

    assemble_map(parsed_per_index, indices_failed, start.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 6).unwrap()
    }

    fn parsed(index: &str, symbols: &[&str]) -> ParsedConstituents {
        let constituents = symbols
            .iter()
            .map(|s| IndexConstituent {
                index_name: index.to_string(),
                symbol: s.to_string(),
                isin: format!("INE{s}"),
                weight: 0.0,
                sector: "Test".to_string(),
                last_updated: today(),
            })
            .collect();
        ParsedConstituents {
            constituents,
            skipped_blank_rows: 0,
            missing_isin: 0,
            skipped_non_eq: 0,
        }
    }

    #[test]
    fn assembles_forward_and_reverse_maps() {
        let map = assemble_map(
            vec![
                parsed("Nifty Total Market", &["RELIANCE", "TCS", "INFY"]),
                parsed("Nifty 50", &["RELIANCE", "TCS"]),
            ],
            0,
            std::time::Duration::from_millis(5),
        );
        // Forward
        assert_eq!(map.index_count(), 2);
        assert_eq!(map.get_constituents("Nifty Total Market").unwrap().len(), 3);
        // Reverse: RELIANCE in both indices, INFY in one.
        assert_eq!(map.stock_count(), 3);
        let reliance_indices = map.get_indices_for_stock("RELIANCE").unwrap();
        assert_eq!(reliance_indices.len(), 2);
        assert_eq!(map.get_indices_for_stock("INFY").unwrap().len(), 1);
    }

    #[test]
    fn reverse_map_dedups_same_index() {
        // Same symbol twice in one index must NOT duplicate the index name.
        let map = assemble_map(
            vec![parsed("Nifty 50", &["RELIANCE", "RELIANCE"])],
            0,
            std::time::Duration::ZERO,
        );
        assert_eq!(map.get_indices_for_stock("RELIANCE").unwrap().len(), 1);
    }

    #[test]
    fn metadata_counts_downloaded_failed_and_mappings() {
        let map = assemble_map(
            vec![parsed("Nifty 50", &["A", "B"])],
            3,
            std::time::Duration::from_millis(10),
        );
        assert_eq!(map.build_metadata.indices_downloaded, 1);
        assert_eq!(map.build_metadata.indices_failed, 3);
        assert_eq!(map.build_metadata.unique_stocks, 2);
        assert_eq!(map.build_metadata.total_mappings, 2);
    }

    #[test]
    fn empty_input_yields_empty_map() {
        let map = assemble_map(Vec::new(), 49, std::time::Duration::ZERO);
        assert_eq!(map.index_count(), 0);
        assert_eq!(map.stock_count(), 0);
        assert_eq!(map.build_metadata.indices_failed, 49);
    }
}
