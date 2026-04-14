//! Bidirectional map builder for index constituency data.
//!
//! Pure function — no I/O. Builds both forward (index → stocks) and
//! reverse (stock → indices) maps in a single pass.

use std::collections::HashMap;

use tracing::warn;

use tickvault_common::instrument_types::{
    ConstituencyBuildMetadata, IndexConstituencyMap, IndexConstituent,
};

/// Build the bidirectional constituency map from parsed CSV results.
///
/// - Forward map: index name → Vec of constituents
/// - Reverse map: stock symbol → Vec of index names (sorted)
/// - Deduplicates: same stock in same index → keep first, warn
pub fn build_constituency_map(
    parsed: Vec<(String, Vec<IndexConstituent>)>,
    metadata: ConstituencyBuildMetadata,
) -> IndexConstituencyMap {
    let mut index_to_constituents: HashMap<String, Vec<IndexConstituent>> = HashMap::new();
    let mut stock_to_indices: HashMap<String, Vec<String>> = HashMap::new();

    for (index_name, constituents) in parsed {
        let mut seen_symbols: HashMap<String, bool> = HashMap::new();
        let mut deduped = Vec::with_capacity(constituents.len());

        for constituent in constituents {
            if seen_symbols.contains_key(&constituent.symbol) {
                warn!(
                    index = %index_name,
                    symbol = %constituent.symbol,
                    "duplicate stock in same index — keeping first occurrence"
                );
                continue;
            }
            seen_symbols.insert(constituent.symbol.clone(), true);

            // Build reverse map entry
            stock_to_indices
                .entry(constituent.symbol.clone())
                .or_default()
                .push(index_name.clone());

            deduped.push(constituent);
        }

        index_to_constituents.insert(index_name, deduped);
    }

    // Sort reverse map values for deterministic output
    for indices in stock_to_indices.values_mut() {
        indices.sort_unstable();
        indices.dedup();
    }

    IndexConstituencyMap {
        index_to_constituents,
        stock_to_indices,
        build_metadata: metadata,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()
    }

    fn make_constituent(index: &str, symbol: &str) -> IndexConstituent {
        IndexConstituent {
            index_name: index.to_string(),
            symbol: symbol.to_string(),
            isin: format!("INE{symbol}01018"),
            weight: 5.0,
            sector: "Test".to_string(),
            last_updated: today(),
        }
    }

    #[test]
    fn test_build_empty_input() {
        let map = build_constituency_map(Vec::new(), ConstituencyBuildMetadata::default());
        assert_eq!(map.index_count(), 0);
        assert_eq!(map.stock_count(), 0);
    }

    #[test]
    fn test_build_single_index_single_stock() {
        let parsed = vec![(
            "Nifty 50".to_string(),
            vec![make_constituent("Nifty 50", "RELIANCE")],
        )];

        let map = build_constituency_map(parsed, ConstituencyBuildMetadata::default());
        assert_eq!(map.index_count(), 1);
        assert_eq!(map.stock_count(), 1);
        assert!(map.contains_index("Nifty 50"));
        assert!(map.contains_stock("RELIANCE"));
    }

    #[test]
    fn test_build_multiple_indices_shared_stock() {
        let parsed = vec![
            (
                "Nifty 50".to_string(),
                vec![make_constituent("Nifty 50", "RELIANCE")],
            ),
            (
                "Nifty 100".to_string(),
                vec![make_constituent("Nifty 100", "RELIANCE")],
            ),
            (
                "Nifty Energy".to_string(),
                vec![make_constituent("Nifty Energy", "RELIANCE")],
            ),
        ];

        let map = build_constituency_map(parsed, ConstituencyBuildMetadata::default());
        assert_eq!(map.index_count(), 3);
        assert_eq!(map.stock_count(), 1); // RELIANCE in all 3

        let indices = map.get_indices_for_stock("RELIANCE").unwrap();
        assert_eq!(indices.len(), 3);
    }

    #[test]
    fn test_reverse_map_correctness() {
        let parsed = vec![
            (
                "Nifty 50".to_string(),
                vec![
                    make_constituent("Nifty 50", "RELIANCE"),
                    make_constituent("Nifty 50", "INFY"),
                ],
            ),
            (
                "Nifty IT".to_string(),
                vec![make_constituent("Nifty IT", "INFY")],
            ),
        ];

        let map = build_constituency_map(parsed, ConstituencyBuildMetadata::default());

        // RELIANCE only in Nifty 50
        let rel_indices = map.get_indices_for_stock("RELIANCE").unwrap();
        assert_eq!(rel_indices, &["Nifty 50"]);

        // INFY in both Nifty 50 and Nifty IT
        let infy_indices = map.get_indices_for_stock("INFY").unwrap();
        assert_eq!(infy_indices.len(), 2);
        assert!(infy_indices.contains(&"Nifty 50".to_string()));
        assert!(infy_indices.contains(&"Nifty IT".to_string()));
    }

    #[test]
    fn test_reverse_map_values_sorted() {
        let parsed = vec![
            (
                "Nifty IT".to_string(),
                vec![make_constituent("Nifty IT", "INFY")],
            ),
            (
                "Nifty 50".to_string(),
                vec![make_constituent("Nifty 50", "INFY")],
            ),
            (
                "Nifty 100".to_string(),
                vec![make_constituent("Nifty 100", "INFY")],
            ),
        ];

        let map = build_constituency_map(parsed, ConstituencyBuildMetadata::default());
        let indices = map.get_indices_for_stock("INFY").unwrap();

        // Should be sorted alphabetically
        assert_eq!(indices, &["Nifty 100", "Nifty 50", "Nifty IT"]);
    }

    #[test]
    fn test_duplicate_stock_in_same_index_deduped() {
        let parsed = vec![(
            "Nifty 50".to_string(),
            vec![
                make_constituent("Nifty 50", "RELIANCE"),
                make_constituent("Nifty 50", "RELIANCE"), // duplicate
            ],
        )];

        let map = build_constituency_map(parsed, ConstituencyBuildMetadata::default());
        let constituents = map.get_constituents("Nifty 50").unwrap();
        assert_eq!(constituents.len(), 1); // deduped
    }

    #[test]
    fn test_stock_in_multiple_indices() {
        let parsed = vec![
            ("A".to_string(), vec![make_constituent("A", "STOCK1")]),
            (
                "B".to_string(),
                vec![
                    make_constituent("B", "STOCK1"),
                    make_constituent("B", "STOCK2"),
                ],
            ),
        ];

        let map = build_constituency_map(parsed, ConstituencyBuildMetadata::default());
        assert_eq!(map.stock_count(), 2);

        let stock1_indices = map.get_indices_for_stock("STOCK1").unwrap();
        assert_eq!(stock1_indices.len(), 2);

        let stock2_indices = map.get_indices_for_stock("STOCK2").unwrap();
        assert_eq!(stock2_indices.len(), 1);
    }
}
