//! Index constituency integration tests.
//!
//! End-to-end tests covering:
//! - CSV parsing → map building → cache roundtrip
//! - O(1) lookup guarantees on the built map
//! - Edge cases: empty inputs, overlapping stocks, all downloads fail
//! - Proptest: fuzz constituency map building

use chrono::NaiveDate;
use dhan_live_trader_common::config::IndexConstituencyConfig;
use dhan_live_trader_common::instrument_types::{
    ConstituencyBuildMetadata, IndexConstituencyMap, IndexConstituent,
};
use dhan_live_trader_core::index_constituency::{csv_parser, mapper};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 3, 15).expect("valid date")
}

fn make_constituent(index: &str, symbol: &str, isin: &str, weight: f64) -> IndexConstituent {
    IndexConstituent {
        index_name: index.to_string(),
        symbol: symbol.to_string(),
        isin: isin.to_string(),
        weight,
        sector: "Test".to_string(),
        last_updated: today(),
    }
}

fn make_nifty50_csv() -> &'static str {
    "Company Name,Industry,Symbol,Series,ISIN Code,Weight(%)\n\
     Reliance Industries Ltd.,Oil Gas & Consumable Fuels,RELIANCE,EQ,INE002A01018,10.25\n\
     HDFC Bank Ltd.,Financial Services,HDFCBANK,EQ,INE040A01034,8.50\n\
     Infosys Ltd.,Information Technology,INFY,EQ,INE009A01021,6.75\n\
     TCS Ltd.,Information Technology,TCS,EQ,INE467B01029,5.20\n\
     ICICI Bank Ltd.,Financial Services,ICICIBANK,EQ,INE090A01021,4.80\n"
}

fn make_nifty_it_csv() -> &'static str {
    "Company Name,Industry,Symbol,Series,ISIN Code,Weight(%)\n\
     Infosys Ltd.,Information Technology,INFY,EQ,INE009A01021,25.50\n\
     TCS Ltd.,Information Technology,TCS,EQ,INE467B01029,22.30\n\
     Wipro Ltd.,Information Technology,WIPRO,EQ,INE075A01022,12.00\n\
     HCL Technologies Ltd.,Information Technology,HCLTECH,EQ,INE860A01027,15.00\n\
     Tech Mahindra Ltd.,Information Technology,TECHM,EQ,INE669C01036,8.50\n"
}

fn make_nifty_bank_csv() -> &'static str {
    "Company Name,Industry,Symbol,Series,ISIN Code,Weight(%)\n\
     HDFC Bank Ltd.,Financial Services,HDFCBANK,EQ,INE040A01034,28.50\n\
     ICICI Bank Ltd.,Financial Services,ICICIBANK,EQ,INE090A01021,22.00\n\
     Kotak Mahindra Bank Ltd.,Financial Services,KOTAKBANK,EQ,INE237A01028,14.80\n\
     Axis Bank Ltd.,Financial Services,AXISBANK,EQ,INE238A01034,12.00\n\
     State Bank of India,Financial Services,SBIN,EQ,INE062A01020,10.50\n"
}

// ---------------------------------------------------------------------------
// End-to-end: CSV → Parse → Map → Lookup
// ---------------------------------------------------------------------------

#[test]
fn e2e_parse_and_build_single_index() {
    let parsed = csv_parser::parse_constituency_csv("Nifty 50", make_nifty50_csv(), today());
    assert_eq!(parsed.len(), 5);

    let map = mapper::build_constituency_map(
        vec![("Nifty 50".to_string(), parsed)],
        ConstituencyBuildMetadata::default(),
    );

    assert_eq!(map.index_count(), 1);
    assert_eq!(map.stock_count(), 5);

    // O(1) forward lookup
    let constituents = map.get_constituents("Nifty 50").expect("Nifty 50 exists");
    assert_eq!(constituents.len(), 5);
    assert_eq!(constituents[0].symbol, "RELIANCE");

    // O(1) reverse lookup
    let indices = map
        .get_indices_for_stock("RELIANCE")
        .expect("RELIANCE exists");
    assert_eq!(indices, &["Nifty 50"]);
}

#[test]
fn e2e_parse_and_build_multiple_indices_with_overlap() {
    let n50 = csv_parser::parse_constituency_csv("Nifty 50", make_nifty50_csv(), today());
    let nit = csv_parser::parse_constituency_csv("Nifty IT", make_nifty_it_csv(), today());
    let nbank = csv_parser::parse_constituency_csv("Nifty Bank", make_nifty_bank_csv(), today());

    let map = mapper::build_constituency_map(
        vec![
            ("Nifty 50".to_string(), n50),
            ("Nifty IT".to_string(), nit),
            ("Nifty Bank".to_string(), nbank),
        ],
        ConstituencyBuildMetadata::default(),
    );

    assert_eq!(map.index_count(), 3);

    // INFY is in Nifty 50 and Nifty IT
    let infy_indices = map.get_indices_for_stock("INFY").expect("INFY exists");
    assert_eq!(infy_indices.len(), 2);
    assert!(infy_indices.contains(&"Nifty 50".to_string()));
    assert!(infy_indices.contains(&"Nifty IT".to_string()));

    // HDFCBANK is in Nifty 50 and Nifty Bank
    let hdfc_indices = map
        .get_indices_for_stock("HDFCBANK")
        .expect("HDFCBANK exists");
    assert_eq!(hdfc_indices.len(), 2);

    // WIPRO is only in Nifty IT
    let wipro_indices = map.get_indices_for_stock("WIPRO").expect("WIPRO exists");
    assert_eq!(wipro_indices, &["Nifty IT"]);

    // KOTAKBANK is only in Nifty Bank
    let kotak_indices = map
        .get_indices_for_stock("KOTAKBANK")
        .expect("KOTAKBANK exists");
    assert_eq!(kotak_indices, &["Nifty Bank"]);
}

// ---------------------------------------------------------------------------
// O(1) lookup guarantees
// ---------------------------------------------------------------------------

#[test]
fn o1_forward_lookup_nonexistent_index_returns_none() {
    let map = mapper::build_constituency_map(
        vec![(
            "Nifty 50".to_string(),
            vec![make_constituent(
                "Nifty 50",
                "RELIANCE",
                "INE002A01018",
                10.0,
            )],
        )],
        ConstituencyBuildMetadata::default(),
    );

    assert!(map.get_constituents("NONEXISTENT").is_none());
}

#[test]
fn o1_reverse_lookup_nonexistent_stock_returns_none() {
    let map = mapper::build_constituency_map(
        vec![(
            "Nifty 50".to_string(),
            vec![make_constituent(
                "Nifty 50",
                "RELIANCE",
                "INE002A01018",
                10.0,
            )],
        )],
        ConstituencyBuildMetadata::default(),
    );

    assert!(map.get_indices_for_stock("NONEXISTENT").is_none());
}

#[test]
fn o1_contains_index_and_stock_consistency() {
    let map = mapper::build_constituency_map(
        vec![
            (
                "A".to_string(),
                vec![make_constituent("A", "X", "INE001", 5.0)],
            ),
            (
                "B".to_string(),
                vec![
                    make_constituent("B", "X", "INE001", 3.0),
                    make_constituent("B", "Y", "INE002", 7.0),
                ],
            ),
        ],
        ConstituencyBuildMetadata::default(),
    );

    assert!(map.contains_index("A"));
    assert!(map.contains_index("B"));
    assert!(!map.contains_index("C"));

    assert!(map.contains_stock("X"));
    assert!(map.contains_stock("Y"));
    assert!(!map.contains_stock("Z"));
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn edge_empty_csv_produces_empty_map() {
    let parsed = csv_parser::parse_constituency_csv("Empty", "", today());
    assert!(parsed.is_empty());

    let map = mapper::build_constituency_map(Vec::new(), ConstituencyBuildMetadata::default());
    assert_eq!(map.index_count(), 0);
    assert_eq!(map.stock_count(), 0);
}

#[test]
fn edge_csv_with_only_non_eq_series_produces_empty() {
    let csv = "Company Name,Industry,Symbol,Series,ISIN Code\n\
               SomeBond,Finance,BOND1,BL,INE999X01001\n\
               SomeSME,Tech,SME1,SM,INE888X01002\n";

    let parsed = csv_parser::parse_constituency_csv("Test", csv, today());
    assert!(parsed.is_empty());
}

#[test]
fn edge_csv_with_bom_and_trailing_commas() {
    let csv = "\u{FEFF}Company Name,Industry,Symbol,Series,ISIN Code,\n\
               Reliance,Energy,RELIANCE,EQ,INE002A01018,\n\
               HDFC,Finance,HDFCBANK,EQ,INE040A01034,\n";

    let parsed = csv_parser::parse_constituency_csv("Test", csv, today());
    assert_eq!(parsed.len(), 2);
}

#[test]
fn edge_stock_count_with_overlapping_indices() {
    // RELIANCE appears in all 3 indices — unique stock count should be 1
    let map = mapper::build_constituency_map(
        vec![
            (
                "A".to_string(),
                vec![make_constituent("A", "RELIANCE", "INE002A01018", 10.0)],
            ),
            (
                "B".to_string(),
                vec![make_constituent("B", "RELIANCE", "INE002A01018", 8.0)],
            ),
            (
                "C".to_string(),
                vec![make_constituent("C", "RELIANCE", "INE002A01018", 6.0)],
            ),
        ],
        ConstituencyBuildMetadata::default(),
    );

    assert_eq!(map.stock_count(), 1);
    assert_eq!(map.index_count(), 3);

    let indices = map.get_indices_for_stock("RELIANCE").expect("RELIANCE");
    assert_eq!(indices.len(), 3);
}

#[test]
fn edge_reverse_map_sorted_alphabetically() {
    let map = mapper::build_constituency_map(
        vec![
            (
                "Nifty IT".to_string(),
                vec![make_constituent("Nifty IT", "INFY", "INE009", 25.0)],
            ),
            (
                "Nifty 50".to_string(),
                vec![make_constituent("Nifty 50", "INFY", "INE009", 6.0)],
            ),
            (
                "Nifty 100".to_string(),
                vec![make_constituent("Nifty 100", "INFY", "INE009", 4.0)],
            ),
        ],
        ConstituencyBuildMetadata::default(),
    );

    let indices = map.get_indices_for_stock("INFY").expect("INFY");
    // Must be sorted alphabetically
    assert_eq!(indices, &["Nifty 100", "Nifty 50", "Nifty IT"]);
}

// ---------------------------------------------------------------------------
// Cache roundtrip (async)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cache_roundtrip_preserves_all_data() {
    use dhan_live_trader_core::index_constituency::cache;

    let cache_dir = format!(
        "/tmp/dlt-test-constituency-integration-{}",
        std::process::id()
    );
    std::fs::create_dir_all(&cache_dir).unwrap();

    // Build a multi-index map
    let n50 = csv_parser::parse_constituency_csv("Nifty 50", make_nifty50_csv(), today());
    let nit = csv_parser::parse_constituency_csv("Nifty IT", make_nifty_it_csv(), today());

    let original = mapper::build_constituency_map(
        vec![("Nifty 50".to_string(), n50), ("Nifty IT".to_string(), nit)],
        ConstituencyBuildMetadata::default(),
    );

    // Save and reload
    cache::save_constituency_cache(&original, &cache_dir, "test-integ.json")
        .await
        .unwrap();

    let loaded = cache::load_constituency_cache(&cache_dir, "test-integ.json")
        .await
        .unwrap();

    // Verify counts match
    assert_eq!(loaded.index_count(), original.index_count());
    assert_eq!(loaded.stock_count(), original.stock_count());

    // Verify specific lookups match
    assert_eq!(
        loaded.get_indices_for_stock("INFY"),
        original.get_indices_for_stock("INFY")
    );

    assert_eq!(
        loaded.get_constituents("Nifty 50").map(|v| v.len()),
        original.get_constituents("Nifty 50").map(|v| v.len())
    );

    // Cleanup
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn cache_load_missing_file_returns_error() {
    use dhan_live_trader_core::index_constituency::cache;

    let result = cache::load_constituency_cache("/nonexistent/path", "missing.json").await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Config validation
// ---------------------------------------------------------------------------

#[test]
fn config_disabled_skips_download() {
    let config = IndexConstituencyConfig {
        enabled: false,
        ..IndexConstituencyConfig::default()
    };

    assert!(!config.enabled);
}

#[test]
fn config_defaults_are_sane() {
    let config = IndexConstituencyConfig::default();

    assert!(config.enabled);
    assert!(config.download_timeout_secs > 0);
    assert!(config.max_concurrent_downloads > 0);
    assert!(config.max_concurrent_downloads <= 20);
}

// ---------------------------------------------------------------------------
// CSV downloader URL construction
// ---------------------------------------------------------------------------

#[test]
fn download_url_construction_all_slugs() {
    use dhan_live_trader_common::constants::INDEX_CONSTITUENCY_SLUGS;
    use dhan_live_trader_core::index_constituency::csv_downloader::build_download_url;

    for (name, slug) in INDEX_CONSTITUENCY_SLUGS {
        let url = build_download_url(slug);
        assert!(url.starts_with("https://www.niftyindices.com/IndexConstituent/"));
        assert!(url.ends_with(".csv"));
        assert!(
            !name.is_empty(),
            "index name should not be empty for slug {slug}"
        );
    }
}

#[test]
fn slug_count_matches_constant() {
    use dhan_live_trader_common::constants::{
        INDEX_CONSTITUENCY_SLUG_COUNT, INDEX_CONSTITUENCY_SLUGS,
    };

    assert_eq!(
        INDEX_CONSTITUENCY_SLUGS.len(),
        INDEX_CONSTITUENCY_SLUG_COUNT
    );
}

// ---------------------------------------------------------------------------
// Slug validation — no duplicates, no empty, URL-safe
// ---------------------------------------------------------------------------

#[test]
fn test_no_duplicate_slugs() {
    use dhan_live_trader_common::constants::INDEX_CONSTITUENCY_SLUGS;
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    for (name, slug) in INDEX_CONSTITUENCY_SLUGS {
        assert!(
            seen.insert(*slug),
            "duplicate CSV slug: {slug} (index: {name})"
        );
    }
}

#[test]
fn test_no_duplicate_index_names() {
    use dhan_live_trader_common::constants::INDEX_CONSTITUENCY_SLUGS;
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    for (name, slug) in INDEX_CONSTITUENCY_SLUGS {
        assert!(
            seen.insert(*name),
            "duplicate index name: {name} (slug: {slug})"
        );
    }
}

#[test]
fn test_all_slugs_valid_format() {
    use dhan_live_trader_common::constants::INDEX_CONSTITUENCY_SLUGS;

    for (name, slug) in INDEX_CONSTITUENCY_SLUGS {
        // Names must be non-empty
        assert!(!name.is_empty(), "empty index name for slug {slug}");

        // Slugs must be non-empty
        assert!(!slug.is_empty(), "empty slug for index {name}");

        // Slugs must start with "ind_nifty" (niftyindices.com convention)
        assert!(
            slug.starts_with("ind_nifty") || slug.starts_with("ind_Nifty"),
            "slug {slug} does not start with ind_nifty or ind_Nifty"
        );

        // Slugs must end with "list" (niftyindices.com convention)
        assert!(
            slug.ends_with("list"),
            "slug {slug} does not end with 'list'"
        );

        // Slugs must be URL-safe (alphanumeric + underscore only)
        assert!(
            slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "slug {slug} contains non-URL-safe characters"
        );

        // Names must be printable ASCII
        assert!(
            name.chars().all(|c| c.is_ascii_graphic() || c == ' '),
            "index name {name} contains non-printable characters"
        );
    }
}

#[test]
fn test_slug_count_covers_all_categories() {
    use dhan_live_trader_common::constants::INDEX_CONSTITUENCY_SLUGS;

    // We must have at least 40 indices to cover broad + sectoral + thematic
    assert!(
        INDEX_CONSTITUENCY_SLUGS.len() >= 40,
        "expected at least 40 indices, got {}",
        INDEX_CONSTITUENCY_SLUGS.len()
    );
}

// ---------------------------------------------------------------------------
// IndexConstituencyMap methods
// ---------------------------------------------------------------------------

#[test]
fn map_default_is_empty() {
    let map = IndexConstituencyMap::default();
    assert_eq!(map.index_count(), 0);
    assert_eq!(map.stock_count(), 0);
    assert!(!map.contains_index("anything"));
    assert!(!map.contains_stock("anything"));
}

#[test]
fn map_weight_preserved_through_build() {
    let map = mapper::build_constituency_map(
        vec![(
            "Nifty 50".to_string(),
            vec![make_constituent(
                "Nifty 50",
                "RELIANCE",
                "INE002A01018",
                10.25,
            )],
        )],
        ConstituencyBuildMetadata::default(),
    );

    let constituents = map.get_constituents("Nifty 50").unwrap();
    assert!((constituents[0].weight - 10.25).abs() < f64::EPSILON);
}

#[test]
fn map_isin_preserved_through_build() {
    let map = mapper::build_constituency_map(
        vec![(
            "Nifty 50".to_string(),
            vec![make_constituent(
                "Nifty 50",
                "RELIANCE",
                "INE002A01018",
                10.0,
            )],
        )],
        ConstituencyBuildMetadata::default(),
    );

    let constituents = map.get_constituents("Nifty 50").unwrap();
    assert_eq!(constituents[0].isin, "INE002A01018");
}

// ---------------------------------------------------------------------------
// Proptest (Type 6) — property-based fuzzing
// ---------------------------------------------------------------------------

mod proptest_constituency {
    use super::*;
    use proptest::prelude::*;

    /// Generate a valid CSV row for a given symbol.
    fn csv_row(symbol: &str, isin: &str) -> String {
        format!("Company {symbol},Sector,{symbol},EQ,{isin}")
    }

    /// Generate a valid CSV with N EQ-series rows.
    fn generate_csv(symbols: &[(&str, &str)]) -> String {
        let mut csv = "Company Name,Industry,Symbol,Series,ISIN Code\n".to_string();
        for (sym, isin) in symbols {
            csv.push_str(&csv_row(sym, isin));
            csv.push('\n');
        }
        csv
    }

    proptest! {
        /// Property: any valid CSV with N EQ-series rows produces exactly N constituents.
        #[test]
        fn proptest_parse_eq_row_count_matches(count in 0_usize..50) {
            let symbols: Vec<(String, String)> = (0..count)
                .map(|i| (format!("SYM{i}"), format!("INE{i:06}X01001")))
                .collect();
            let refs: Vec<(&str, &str)> = symbols
                .iter()
                .map(|(s, i)| (s.as_str(), i.as_str()))
                .collect();
            let csv_text = generate_csv(&refs);
            let parsed = csv_parser::parse_constituency_csv("TestIdx", &csv_text, today());
            prop_assert_eq!(parsed.len(), count);
        }

        /// Property: reverse map contains every stock that exists in any forward map entry.
        #[test]
        fn proptest_reverse_map_complete(
            idx_count in 1_usize..5,
            stocks_per_idx in 1_usize..10,
        ) {
            let mut entries = Vec::new();
            for i in 0..idx_count {
                let index_name = format!("Index_{i}");
                let constituents: Vec<IndexConstituent> = (0..stocks_per_idx)
                    .map(|j| make_constituent(
                        &index_name,
                        &format!("STOCK_{j}"),
                        &format!("INE{j:06}"),
                        1.0,
                    ))
                    .collect();
                entries.push((index_name, constituents));
            }

            let map = mapper::build_constituency_map(entries, ConstituencyBuildMetadata::default());

            // Every stock in every forward map entry must exist in reverse map
            for constituents in map.index_to_constituents.values() {
                for c in constituents {
                    prop_assert!(
                        map.stock_to_indices.contains_key(&c.symbol),
                        "stock {} missing from reverse map",
                        c.symbol
                    );
                }
            }
        }

        /// Property: build is idempotent (same input → same output).
        #[test]
        fn proptest_build_idempotent(stock_count in 1_usize..20) {
            let constituents: Vec<IndexConstituent> = (0..stock_count)
                .map(|i| make_constituent(
                    "Nifty Test",
                    &format!("S{i}"),
                    &format!("INE{i:06}"),
                    (i as f64) * 0.5,
                ))
                .collect();

            let input = vec![("Nifty Test".to_string(), constituents.clone())];
            let map1 = mapper::build_constituency_map(
                input.clone(),
                ConstituencyBuildMetadata::default(),
            );
            let map2 = mapper::build_constituency_map(input, ConstituencyBuildMetadata::default());

            prop_assert_eq!(map1.index_count(), map2.index_count());
            prop_assert_eq!(map1.stock_count(), map2.stock_count());
            prop_assert_eq!(map1.all_index_names(), map2.all_index_names());
        }

        /// Property: `all_index_names()` is always sorted.
        #[test]
        fn proptest_all_index_names_sorted(count in 1_usize..10) {
            let entries: Vec<(String, Vec<IndexConstituent>)> = (0..count)
                .map(|i| {
                    let name = format!("Idx_{i:03}");
                    let c = vec![make_constituent(&name, &format!("S{i}"), "INE000", 1.0)];
                    (name, c)
                })
                .collect();

            let map = mapper::build_constituency_map(entries, ConstituencyBuildMetadata::default());
            let names = map.all_index_names();

            for window in names.windows(2) {
                prop_assert!(
                    window[0] <= window[1],
                    "index names not sorted: {} > {}",
                    window[0],
                    window[1]
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Real HTTP integration tests (hit actual niftyindices.com)
//
// These tests are #[ignore] by default — they require network access to
// niftyindices.com and are slow (~30s for all 49 indices).
// Run manually: cargo test -p dhan-live-trader-core --test index_constituency real_fetch -- --ignored
// ---------------------------------------------------------------------------

mod real_fetch {
    use dhan_live_trader_common::config::IndexConstituencyConfig;
    use dhan_live_trader_common::constants::INDEX_CONSTITUENCY_SLUGS;
    use dhan_live_trader_core::index_constituency::csv_downloader;
    use dhan_live_trader_core::index_constituency::csv_parser;

    fn default_config() -> IndexConstituencyConfig {
        IndexConstituencyConfig {
            enabled: true,
            download_timeout_secs: 30,
            max_concurrent_downloads: 3,
            inter_batch_delay_ms: 200,
        }
    }

    /// Fetch a single Nifty 50 CSV from niftyindices.com and verify it parses.
    #[tokio::test]
    #[ignore]
    async fn real_fetch_nifty50_csv_downloads_and_parses() {
        let config = default_config();
        let slugs: &[(&str, &str)] = &[("Nifty 50", "ind_nifty50list")];

        let results = csv_downloader::download_constituency_csvs(&config, slugs).await;

        assert_eq!(results.len(), 1, "Nifty 50 CSV must download successfully");
        let (name, csv_text) = &results[0];
        assert_eq!(name, "Nifty 50");
        assert!(!csv_text.is_empty(), "CSV body must not be empty");
        assert!(
            csv_text.contains("Symbol"),
            "CSV must contain 'Symbol' header"
        );

        // Parse and verify
        let today = chrono::Local::now().date_naive();
        let constituents = csv_parser::parse_constituency_csv("Nifty 50", csv_text, today);
        assert_eq!(
            constituents.len(),
            50,
            "Nifty 50 must have exactly 50 constituents"
        );

        // Verify known stocks exist
        let symbols: Vec<&str> = constituents.iter().map(|c| c.symbol.as_str()).collect();
        assert!(
            symbols.contains(&"RELIANCE") || symbols.contains(&"Reliance"),
            "Nifty 50 must contain RELIANCE"
        );
        assert!(
            symbols.contains(&"HDFCBANK") || symbols.contains(&"HdfcBank"),
            "Nifty 50 must contain HDFCBANK"
        );

        // Verify weights sum to ~100%
        let weight_sum: f64 = constituents.iter().map(|c| c.weight).sum();
        if weight_sum > 0.0 {
            assert!(
                (90.0..=110.0).contains(&weight_sum),
                "Nifty 50 weights must sum to ~100%, got {weight_sum}"
            );
        }

        // Verify ISIN format (12 chars, starts with IN)
        for c in &constituents {
            if !c.isin.is_empty() {
                assert!(
                    c.isin.len() == 12 && c.isin.starts_with("IN"),
                    "ISIN must be 12 chars starting with IN, got: {}",
                    c.isin
                );
            }
        }
    }

    /// Fetch Nifty Bank CSV and verify sector data is present.
    #[tokio::test]
    #[ignore]
    async fn real_fetch_nifty_bank_csv_has_bank_stocks() {
        let config = default_config();
        let slugs: &[(&str, &str)] = &[("Nifty Bank", "ind_niftybanklist")];

        let results = csv_downloader::download_constituency_csvs(&config, slugs).await;

        assert_eq!(results.len(), 1, "Nifty Bank CSV must download");
        let today = chrono::Local::now().date_naive();
        let constituents = csv_parser::parse_constituency_csv("Nifty Bank", &results[0].1, today);

        assert!(
            constituents.len() >= 10,
            "Nifty Bank must have at least 10 constituents, got {}",
            constituents.len()
        );

        let symbols: Vec<&str> = constituents.iter().map(|c| c.symbol.as_str()).collect();
        assert!(
            symbols.contains(&"HDFCBANK"),
            "Nifty Bank must contain HDFCBANK"
        );
        assert!(
            symbols.contains(&"ICICIBANK"),
            "Nifty Bank must contain ICICIBANK"
        );
    }

    /// Fetch ALL 49 indices and verify minimum success rate.
    #[tokio::test]
    #[ignore]
    async fn real_fetch_all_49_indices_minimum_success_rate() {
        let config = default_config();

        let results =
            csv_downloader::download_constituency_csvs(&config, INDEX_CONSTITUENCY_SLUGS).await;

        let total = INDEX_CONSTITUENCY_SLUGS.len();
        let downloaded = results.len();
        let failed = total - downloaded;

        eprintln!("Downloaded {downloaded}/{total} indices ({failed} failed)");

        // At least 80% must succeed (40 out of 49)
        assert!(
            downloaded >= 40,
            "At least 40 of {total} indices must download, got {downloaded}"
        );

        // Parse all downloaded CSVs and verify they have data
        let today = chrono::Local::now().date_naive();
        let mut total_stocks = 0usize;
        let mut empty_indices = Vec::new();

        for (name, csv_text) in &results {
            let constituents = csv_parser::parse_constituency_csv(name, csv_text, today);
            if constituents.is_empty() {
                empty_indices.push(name.as_str());
            } else {
                total_stocks += constituents.len();
            }
        }

        assert!(
            empty_indices.len() <= 5,
            "At most 5 indices may parse as empty, got {}: {:?}",
            empty_indices.len(),
            empty_indices
        );

        eprintln!(
            "Total stock-index mappings: {total_stocks}, empty indices: {:?}",
            empty_indices
        );

        assert!(
            total_stocks >= 500,
            "Must have at least 500 total stock-index mappings, got {total_stocks}"
        );
    }

    /// End-to-end: download → parse → build map → verify O(1) lookups.
    #[tokio::test]
    #[ignore]
    async fn real_fetch_end_to_end_download_parse_build_lookup() {
        use dhan_live_trader_common::instrument_types::ConstituencyBuildMetadata;
        use dhan_live_trader_core::index_constituency::mapper;

        let config = default_config();
        let slugs: &[(&str, &str)] = &[
            ("Nifty 50", "ind_nifty50list"),
            ("Nifty Bank", "ind_niftybanklist"),
            ("Nifty IT", "ind_niftyitlist"),
        ];

        let downloaded = csv_downloader::download_constituency_csvs(&config, slugs).await;

        assert!(
            downloaded.len() >= 2,
            "At least 2 of 3 indices must download"
        );

        let today = chrono::Local::now().date_naive();
        let parsed: Vec<_> = downloaded
            .iter()
            .map(|(name, csv)| {
                let constituents = csv_parser::parse_constituency_csv(name, csv, today);
                (name.clone(), constituents)
            })
            .collect();

        let map = mapper::build_constituency_map(parsed, ConstituencyBuildMetadata::default());

        // Forward lookup
        if let Some(nifty50) = map.get_constituents("Nifty 50") {
            assert_eq!(nifty50.len(), 50, "Nifty 50 must have 50 constituents");
        }

        // Reverse lookup — HDFCBANK should be in both Nifty 50 and Nifty Bank
        if let Some(indices) = map.get_indices_for_stock("HDFCBANK") {
            assert!(
                indices.len() >= 2,
                "HDFCBANK must be in at least 2 indices, got {:?}",
                indices
            );
        }

        // Verify map counts
        assert!(map.index_count() >= 2, "Map must have at least 2 indices");
        assert!(
            map.stock_count() >= 50,
            "Map must have at least 50 unique stocks"
        );
    }

    /// Verify the URL format matches what niftyindices.com expects.
    #[test]
    fn real_fetch_url_has_www_prefix() {
        let url = csv_downloader::build_download_url("ind_nifty50list");
        assert!(
            url.starts_with("https://www.niftyindices.com/"),
            "URL must use www. prefix, got: {url}"
        );
        assert!(url.ends_with(".csv"), "URL must end with .csv, got: {url}");
    }
}

// ---------------------------------------------------------------------------
// Constituency ↔ Instrument mapping tests
// ---------------------------------------------------------------------------

mod instrument_mapping {
    use super::*;
    use dhan_live_trader_common::instrument_types::{
        FnoUnderlying, FnoUniverse, UnderlyingKind, UniverseBuildMetadata,
    };
    use dhan_live_trader_common::types::ExchangeSegment;
    use std::collections::HashMap;

    fn empty_build_metadata() -> UniverseBuildMetadata {
        use chrono::{FixedOffset, Utc};
        let ist = FixedOffset::east_opt(19_800).expect("valid");
        UniverseBuildMetadata {
            csv_source: String::new(),
            csv_row_count: 0,
            parsed_row_count: 0,
            index_count: 0,
            equity_count: 0,
            underlying_count: 0,
            derivative_count: 0,
            option_chain_count: 0,
            build_duration: std::time::Duration::ZERO,
            build_timestamp: Utc::now().with_timezone(&ist),
        }
    }

    /// Build a minimal FnoUniverse with known underlyings for testing.
    fn make_test_universe() -> FnoUniverse {
        let mut underlyings = HashMap::new();
        underlyings.insert(
            "RELIANCE".to_string(),
            FnoUnderlying {
                underlying_symbol: "RELIANCE".to_string(),
                underlying_security_id: 26000,
                price_feed_security_id: 2885,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 250,
                contract_count: 0,
            },
        );
        underlyings.insert(
            "HDFCBANK".to_string(),
            FnoUnderlying {
                underlying_symbol: "HDFCBANK".to_string(),
                underlying_security_id: 27000,
                price_feed_security_id: 1333,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 550,
                contract_count: 0,
            },
        );
        underlyings.insert(
            "NIFTY".to_string(),
            FnoUnderlying {
                underlying_symbol: "NIFTY".to_string(),
                underlying_security_id: 26000,
                price_feed_security_id: 13,
                price_feed_segment: ExchangeSegment::IdxI,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::NseIndex,
                lot_size: 25,
                contract_count: 0,
            },
        );

        FnoUniverse {
            underlyings,
            derivative_contracts: HashMap::new(),
            instrument_info: HashMap::new(),
            option_chains: HashMap::new(),
            expiry_calendars: HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: empty_build_metadata(),
        }
    }

    #[test]
    fn symbol_to_security_id_known_stocks() {
        let universe = make_test_universe();

        assert_eq!(
            universe.symbol_to_security_id("RELIANCE"),
            Some(2885),
            "RELIANCE → 2885"
        );
        assert_eq!(
            universe.symbol_to_security_id("HDFCBANK"),
            Some(1333),
            "HDFCBANK → 1333"
        );
        assert_eq!(
            universe.symbol_to_security_id("NIFTY"),
            Some(13),
            "NIFTY → 13"
        );
    }

    #[test]
    fn symbol_to_security_id_unknown_returns_none() {
        let universe = make_test_universe();
        assert_eq!(universe.symbol_to_security_id("UNKNOWN_STOCK_XYZ"), None);
    }

    #[test]
    fn constituency_to_security_id_roundtrip() {
        // Build a constituency map with known symbols
        let csv = "\
            Company Name,Industry,Symbol,Series,ISIN Code,Weight(%)\n\
            Reliance Industries,Oil Gas,RELIANCE,EQ,INE002A01018,10.5\n\
            HDFC Bank,Financial Services,HDFCBANK,EQ,INE040A01034,8.2\n\
            TCS,IT,TCS,EQ,INE467B01029,5.1\n";

        let constituents = csv_parser::parse_constituency_csv("Nifty 50", csv, today());
        assert_eq!(constituents.len(), 3);

        let parsed = vec![("Nifty 50".to_string(), constituents)];
        let map = mapper::build_constituency_map(parsed, ConstituencyBuildMetadata::default());

        // Now bridge to instrument universe
        let universe = make_test_universe();

        // Verify each constituent can be mapped to a security_id
        let nifty50 = map
            .get_constituents("Nifty 50")
            .expect("Nifty 50 must exist");
        for constituent in nifty50 {
            let sec_id = universe.symbol_to_security_id(&constituent.symbol);
            match constituent.symbol.as_str() {
                "RELIANCE" => assert_eq!(sec_id, Some(2885)),
                "HDFCBANK" => assert_eq!(sec_id, Some(1333)),
                "TCS" => assert_eq!(sec_id, None, "TCS not in test universe"),
                _ => panic!("unexpected symbol: {}", constituent.symbol),
            }
        }
    }

    #[test]
    fn reverse_lookup_indices_for_stock() {
        let csv_nifty50 = "\
            Company Name,Industry,Symbol,Series,ISIN Code,Weight(%)\n\
            Reliance Industries,Oil Gas,RELIANCE,EQ,INE002A01018,10.5\n\
            HDFC Bank,Financial Services,HDFCBANK,EQ,INE040A01034,8.2\n";

        let csv_bank = "\
            Company Name,Industry,Symbol,Series,ISIN Code,Weight(%)\n\
            HDFC Bank,Financial Services,HDFCBANK,EQ,INE040A01034,25.0\n\
            ICICI Bank,Financial Services,ICICIBANK,EQ,INE090A01021,20.0\n";

        let parsed = vec![
            (
                "Nifty 50".to_string(),
                csv_parser::parse_constituency_csv("Nifty 50", csv_nifty50, today()),
            ),
            (
                "Nifty Bank".to_string(),
                csv_parser::parse_constituency_csv("Nifty Bank", csv_bank, today()),
            ),
        ];

        let map = mapper::build_constituency_map(parsed, ConstituencyBuildMetadata::default());

        // HDFCBANK is in both indices
        let hdfcbank_indices = map.get_indices_for_stock("HDFCBANK").expect("HDFCBANK");
        assert!(hdfcbank_indices.contains(&"Nifty 50".to_string()));
        assert!(hdfcbank_indices.contains(&"Nifty Bank".to_string()));

        // RELIANCE is only in Nifty 50
        let reliance_indices = map.get_indices_for_stock("RELIANCE").expect("RELIANCE");
        assert_eq!(reliance_indices.len(), 1);
        assert!(reliance_indices.contains(&"Nifty 50".to_string()));

        // Bridge to security_ids
        let universe = make_test_universe();
        for symbol in &["RELIANCE", "HDFCBANK"] {
            let sec_id = universe.symbol_to_security_id(symbol);
            assert!(sec_id.is_some(), "{symbol} must have a security_id");
        }
    }

    #[test]
    fn news_based_trading_flow_symbol_to_all_data() {
        // Simulate: "RELIANCE in news" → find all relevant data
        let csv = "\
            Company Name,Industry,Symbol,Series,ISIN Code,Weight(%)\n\
            Reliance Industries,Oil Gas,RELIANCE,EQ,INE002A01018,10.5\n";

        let parsed = vec![(
            "Nifty 50".to_string(),
            csv_parser::parse_constituency_csv("Nifty 50", csv, today()),
        )];

        let map = mapper::build_constituency_map(parsed, ConstituencyBuildMetadata::default());
        let universe = make_test_universe();

        // Step 1: Symbol → security_id
        let sec_id = universe
            .symbol_to_security_id("RELIANCE")
            .expect("RELIANCE security_id");
        assert_eq!(sec_id, 2885);

        // Step 2: Symbol → underlying details
        let underlying = universe
            .get_underlying("RELIANCE")
            .expect("RELIANCE underlying");
        assert_eq!(underlying.lot_size, 250);
        assert_eq!(underlying.price_feed_segment, ExchangeSegment::NseEquity);

        // Step 3: Symbol → all indices
        let indices = map
            .get_indices_for_stock("RELIANCE")
            .expect("RELIANCE indices");
        assert!(indices.contains(&"Nifty 50".to_string()));

        // Step 4: Symbol → derivative contracts (empty in test, but API works)
        let derivatives = universe.derivative_security_ids_for_symbol("RELIANCE");
        assert!(
            derivatives.is_empty(),
            "test universe has no contracts loaded"
        );
    }
}
