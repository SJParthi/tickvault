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

use self::downloader::{ConstituencyDownloadError, ConstituencyDownloader};
use self::parser::{ParsedConstituents, parse_constituents};

use std::time::Duration;
use tickvault_common::constants::{
    INDEX_CONSTITUENCY_RETRY_MAX_DELAY_SECS, INDEX_CONSTITUENCY_RETRY_MAX_TIMES,
    INDEX_CONSTITUENCY_RETRY_MIN_DELAY_SECS,
};

/// Backoff (seconds) before the retry that follows a failed `attempt`
/// (1-based). Exponential from `INDEX_CONSTITUENCY_RETRY_MIN_DELAY_SECS`,
/// doubling each attempt, capped at `INDEX_CONSTITUENCY_RETRY_MAX_DELAY_SECS`.
/// PURE — no I/O; unit-tested below.
#[must_use]
pub fn constituency_retry_delay_secs(attempt: usize) -> u64 {
    let min = INDEX_CONSTITUENCY_RETRY_MIN_DELAY_SECS;
    let max = INDEX_CONSTITUENCY_RETRY_MAX_DELAY_SECS.max(min);
    if attempt <= 1 {
        return min;
    }
    // doubling: min * 2^(attempt-1), saturating + capped at max.
    let shift = u32::try_from(attempt - 1).unwrap_or(u32::MAX).min(20);
    min.saturating_mul(1u64 << shift).min(max)
}

/// Fetch one slug with up to `INDEX_CONSTITUENCY_RETRY_MAX_TIMES` attempts and
/// exponential backoff between them (operator 2026-06-08: "retry at least five
/// times"). A transient niftyindices timeout / 5xx / reset on one attempt no
/// longer drops the whole list — only an exhausted ladder returns `Err`.
///
/// COLD PATH — runs once per slug at boot.
// TEST-EXEMPT: network I/O orchestration; the pure backoff schedule is unit-tested
// via `constituency_retry_delay_secs`, and `ConstituencyDownloader::fetch_slug`
// is covered in the downloader's own tests.
async fn fetch_slug_with_retry(
    downloader: &ConstituencyDownloader,
    display_name: &str,
    slug: &str,
) -> Result<Vec<u8>, ConstituencyDownloadError> {
    let max = INDEX_CONSTITUENCY_RETRY_MAX_TIMES.max(1);
    let mut attempt = 1usize;
    loop {
        match downloader.fetch_slug(slug).await {
            Ok(bytes) => return Ok(bytes),
            Err(err) => {
                if attempt >= max {
                    return Err(err);
                }
                let delay = constituency_retry_delay_secs(attempt);
                tracing::warn!(
                    index = display_name,
                    attempt,
                    max,
                    delay_secs = delay,
                    %err,
                    "constituency download attempt failed; retrying"
                );
                tokio::time::sleep(Duration::from_secs(delay)).await;
                attempt += 1;
            }
        }
    }
}

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
        match fetch_slug_with_retry(downloader, display_name, slug).await {
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

    #[test]
    fn retry_delay_first_attempt_is_min() {
        assert_eq!(
            constituency_retry_delay_secs(1),
            INDEX_CONSTITUENCY_RETRY_MIN_DELAY_SECS
        );
    }

    #[test]
    fn retry_delay_grows_exponentially_then_caps() {
        // min=1, max=10 → 1, 2, 4, 8, 10(capped), 10...
        assert_eq!(constituency_retry_delay_secs(1), 1);
        assert_eq!(constituency_retry_delay_secs(2), 2);
        assert_eq!(constituency_retry_delay_secs(3), 4);
        assert_eq!(constituency_retry_delay_secs(4), 8);
        assert_eq!(
            constituency_retry_delay_secs(5),
            INDEX_CONSTITUENCY_RETRY_MAX_DELAY_SECS
        );
    }

    #[test]
    fn retry_delay_never_exceeds_max_even_for_huge_attempt() {
        assert!(constituency_retry_delay_secs(1000) <= INDEX_CONSTITUENCY_RETRY_MAX_DELAY_SECS);
        // and no overflow panic on a pathological attempt count
        assert!(
            constituency_retry_delay_secs(usize::MAX) <= INDEX_CONSTITUENCY_RETRY_MAX_DELAY_SECS
        );
    }

    #[test]
    fn retry_max_times_is_at_least_five() {
        // operator 2026-06-08: "retry at least five times"
        assert!(INDEX_CONSTITUENCY_RETRY_MAX_TIMES >= 5);
    }

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
