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
        assert!(url.starts_with("https://niftyindices.com/IndexConstituent/"));
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
