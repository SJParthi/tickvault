//! Index constituency API endpoints.
//!
//! - `GET /api/index-constituency` — summary of all indices with stock counts
//! - `GET /api/index-constituency/{index_name}` — constituents of a specific index
//! - `GET /api/stock-indices/{symbol}` — all indices containing a stock

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;

use dhan_live_trader_common::instrument_types::IndexConstituent;

use crate::state::SharedAppState;

/// Summary of all indices in the constituency map.
#[derive(Debug, Serialize)]
pub struct ConstituencySummaryResponse {
    /// Whether constituency data is available.
    pub available: bool,
    /// Total number of indices.
    pub index_count: usize,
    /// Total number of unique stocks across all indices.
    pub stock_count: usize,
    /// List of index names with their constituent count.
    pub indices: Vec<IndexSummaryEntry>,
}

/// A single index in the summary.
#[derive(Debug, Serialize)]
pub struct IndexSummaryEntry {
    /// Index name (e.g., "Nifty 50").
    pub name: String,
    /// Number of constituent stocks.
    pub constituent_count: usize,
}

/// Response for a specific index's constituents.
#[derive(Debug, Serialize)]
pub struct IndexConstituentsResponse {
    /// Whether the index was found.
    pub found: bool,
    /// Index name.
    pub index_name: String,
    /// Constituent stocks.
    pub constituents: Vec<ConstituentEntry>,
}

/// A single constituent stock in an index.
#[derive(Debug, Serialize)]
pub struct ConstituentEntry {
    pub symbol: String,
    pub isin: String,
    pub weight: f64,
    pub sector: String,
}

impl From<&IndexConstituent> for ConstituentEntry {
    fn from(c: &IndexConstituent) -> Self {
        Self {
            symbol: c.symbol.clone(),
            isin: c.isin.clone(),
            weight: c.weight,
            sector: c.sector.clone(),
        }
    }
}

/// Response for stock → indices reverse lookup.
#[derive(Debug, Serialize)]
pub struct StockIndicesResponse {
    /// Whether the stock was found in any index.
    pub found: bool,
    /// Stock symbol.
    pub symbol: String,
    /// Indices containing this stock (sorted alphabetically).
    pub indices: Vec<String>,
}

/// `GET /api/index-constituency` — summary of all indices.
pub async fn get_constituency_summary(
    State(state): State<SharedAppState>,
) -> Json<ConstituencySummaryResponse> {
    let handle = state.constituency_map();

    let response = match handle.read() {
        Ok(guard) => match guard.as_ref() {
            Some(map) => {
                let mut indices: Vec<IndexSummaryEntry> = map
                    .all_index_names()
                    .into_iter()
                    .map(|name| {
                        let count = map.get_constituents(name).map(|c| c.len()).unwrap_or(0);
                        IndexSummaryEntry {
                            name: name.to_string(),
                            constituent_count: count,
                        }
                    })
                    .collect();
                indices.sort_by(|a, b| a.name.cmp(&b.name));

                ConstituencySummaryResponse {
                    available: true,
                    index_count: map.index_count(),
                    stock_count: map.stock_count(),
                    indices,
                }
            }
            None => unavailable_summary(),
        },
        Err(_) => unavailable_summary(),
    };

    Json(response)
}

/// `GET /api/index-constituency/{index_name}` — constituents of a specific index.
pub async fn get_index_constituents(
    State(state): State<SharedAppState>,
    Path(index_name): Path<String>,
) -> Json<IndexConstituentsResponse> {
    let handle = state.constituency_map();

    let response = match handle.read() {
        Ok(guard) => match guard.as_ref() {
            Some(map) => match map.get_constituents(&index_name) {
                Some(constituents) => IndexConstituentsResponse {
                    found: true,
                    index_name,
                    constituents: constituents.iter().map(ConstituentEntry::from).collect(),
                },
                None => IndexConstituentsResponse {
                    found: false,
                    index_name,
                    constituents: Vec::new(),
                },
            },
            None => IndexConstituentsResponse {
                found: false,
                index_name,
                constituents: Vec::new(),
            },
        },
        Err(_) => IndexConstituentsResponse {
            found: false,
            index_name,
            constituents: Vec::new(),
        },
    };

    Json(response)
}

/// `GET /api/stock-indices/{symbol}` — all indices containing a stock.
pub async fn get_stock_indices(
    State(state): State<SharedAppState>,
    Path(symbol): Path<String>,
) -> Json<StockIndicesResponse> {
    let handle = state.constituency_map();

    let response = match handle.read() {
        Ok(guard) => match guard.as_ref() {
            Some(map) => match map.get_indices_for_stock(&symbol) {
                Some(indices) => StockIndicesResponse {
                    found: true,
                    symbol,
                    indices: indices.to_vec(),
                },
                None => StockIndicesResponse {
                    found: false,
                    symbol,
                    indices: Vec::new(),
                },
            },
            None => StockIndicesResponse {
                found: false,
                symbol,
                indices: Vec::new(),
            },
        },
        Err(_) => StockIndicesResponse {
            found: false,
            symbol,
            indices: Vec::new(),
        },
    };

    Json(response)
}

fn unavailable_summary() -> ConstituencySummaryResponse {
    ConstituencySummaryResponse {
        available: false,
        index_count: 0,
        stock_count: 0,
        indices: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SharedConstituencyMap;
    use dhan_live_trader_common::instrument_types::{
        ConstituencyBuildMetadata, IndexConstituencyMap,
    };
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};

    fn make_test_map() -> IndexConstituencyMap {
        let mut index_to_constituents = HashMap::new();
        index_to_constituents.insert(
            "Nifty 50".to_string(),
            vec![
                IndexConstituent {
                    index_name: "Nifty 50".to_string(),
                    symbol: "RELIANCE".to_string(),
                    isin: "INE002A01018".to_string(),
                    weight: 10.25,
                    sector: "Oil Gas".to_string(),
                    last_updated: chrono::NaiveDate::from_ymd_opt(2026, 3, 15).unwrap(),
                },
                IndexConstituent {
                    index_name: "Nifty 50".to_string(),
                    symbol: "HDFCBANK".to_string(),
                    isin: "INE040A01034".to_string(),
                    weight: 8.50,
                    sector: "Financial".to_string(),
                    last_updated: chrono::NaiveDate::from_ymd_opt(2026, 3, 15).unwrap(),
                },
            ],
        );
        index_to_constituents.insert(
            "Nifty Bank".to_string(),
            vec![IndexConstituent {
                index_name: "Nifty Bank".to_string(),
                symbol: "HDFCBANK".to_string(),
                isin: "INE040A01034".to_string(),
                weight: 28.50,
                sector: "Financial".to_string(),
                last_updated: chrono::NaiveDate::from_ymd_opt(2026, 3, 15).unwrap(),
            }],
        );

        let mut stock_to_indices = HashMap::new();
        stock_to_indices.insert("RELIANCE".to_string(), vec!["Nifty 50".to_string()]);
        stock_to_indices.insert(
            "HDFCBANK".to_string(),
            vec!["Nifty 50".to_string(), "Nifty Bank".to_string()],
        );

        IndexConstituencyMap {
            index_to_constituents,
            stock_to_indices,
            build_metadata: ConstituencyBuildMetadata::default(),
        }
    }

    fn shared_map_with_data() -> SharedConstituencyMap {
        Arc::new(RwLock::new(Some(make_test_map())))
    }

    fn shared_map_empty() -> SharedConstituencyMap {
        Arc::new(RwLock::new(None))
    }

    fn test_state(constituency: SharedConstituencyMap) -> SharedAppState {
        SharedAppState::new(
            dhan_live_trader_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
                pg_port: 1,
                ilp_port: 1,
            },
            dhan_live_trader_common::config::DhanConfig {
                websocket_url: "wss://test".to_string(),
                order_update_websocket_url: "wss://test".to_string(),
                rest_api_base_url: "https://test".to_string(),
                auth_base_url: "https://test".to_string(),
                instrument_csv_url: "https://test".to_string(),
                instrument_csv_fallback_url: "https://test".to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
            },
            dhan_live_trader_common::config::InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/dlt-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            constituency,
            std::sync::Arc::new(crate::state::SystemHealthStatus::new()),
        )
    }

    // --- Summary endpoint ---

    #[tokio::test]
    async fn test_summary_with_data() {
        let state = test_state(shared_map_with_data());
        let Json(resp) = get_constituency_summary(State(state)).await;

        assert!(resp.available);
        assert_eq!(resp.index_count, 2);
        assert_eq!(resp.stock_count, 2);
        assert_eq!(resp.indices.len(), 2);

        // Sorted alphabetically
        assert_eq!(resp.indices[0].name, "Nifty 50");
        assert_eq!(resp.indices[0].constituent_count, 2);
        assert_eq!(resp.indices[1].name, "Nifty Bank");
        assert_eq!(resp.indices[1].constituent_count, 1);
    }

    #[tokio::test]
    async fn test_summary_unavailable() {
        let state = test_state(shared_map_empty());
        let Json(resp) = get_constituency_summary(State(state)).await;

        assert!(!resp.available);
        assert_eq!(resp.index_count, 0);
        assert_eq!(resp.indices.len(), 0);
    }

    // --- Index constituents endpoint ---

    #[tokio::test]
    async fn test_get_constituents_found() {
        let state = test_state(shared_map_with_data());
        let Json(resp) = get_index_constituents(State(state), Path("Nifty 50".to_string())).await;

        assert!(resp.found);
        assert_eq!(resp.index_name, "Nifty 50");
        assert_eq!(resp.constituents.len(), 2);
        assert_eq!(resp.constituents[0].symbol, "RELIANCE");
        assert!((resp.constituents[0].weight - 10.25).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_get_constituents_not_found() {
        let state = test_state(shared_map_with_data());
        let Json(resp) =
            get_index_constituents(State(state), Path("Nonexistent".to_string())).await;

        assert!(!resp.found);
        assert!(resp.constituents.is_empty());
    }

    #[tokio::test]
    async fn test_get_constituents_no_map() {
        let state = test_state(shared_map_empty());
        let Json(resp) = get_index_constituents(State(state), Path("Nifty 50".to_string())).await;

        assert!(!resp.found);
        assert!(resp.constituents.is_empty());
    }

    // --- Stock indices endpoint ---

    #[tokio::test]
    async fn test_get_stock_indices_found() {
        let state = test_state(shared_map_with_data());
        let Json(resp) = get_stock_indices(State(state), Path("HDFCBANK".to_string())).await;

        assert!(resp.found);
        assert_eq!(resp.symbol, "HDFCBANK");
        assert_eq!(resp.indices.len(), 2);
    }

    #[tokio::test]
    async fn test_get_stock_indices_not_found() {
        let state = test_state(shared_map_with_data());
        let Json(resp) = get_stock_indices(State(state), Path("UNKNOWN".to_string())).await;

        assert!(!resp.found);
        assert!(resp.indices.is_empty());
    }

    #[tokio::test]
    async fn test_get_stock_indices_no_map() {
        let state = test_state(shared_map_empty());
        let Json(resp) = get_stock_indices(State(state), Path("RELIANCE".to_string())).await;

        assert!(!resp.found);
        assert!(resp.indices.is_empty());
    }

    // --- Serialization ---

    #[test]
    fn test_summary_response_serialization() {
        let resp = ConstituencySummaryResponse {
            available: true,
            index_count: 2,
            stock_count: 100,
            indices: vec![IndexSummaryEntry {
                name: "Nifty 50".to_string(),
                constituent_count: 50,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"available\":true"));
        assert!(json.contains("\"index_count\":2"));
        assert!(json.contains("\"Nifty 50\""));
    }

    #[test]
    fn test_constituents_response_serialization() {
        let resp = IndexConstituentsResponse {
            found: true,
            index_name: "Nifty IT".to_string(),
            constituents: vec![ConstituentEntry {
                symbol: "INFY".to_string(),
                isin: "INE009A01021".to_string(),
                weight: 25.5,
                sector: "IT".to_string(),
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"found\":true"));
        assert!(json.contains("\"INFY\""));
    }

    #[test]
    fn test_stock_indices_response_serialization() {
        let resp = StockIndicesResponse {
            found: true,
            symbol: "RELIANCE".to_string(),
            indices: vec!["Nifty 50".to_string(), "Nifty 100".to_string()],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"RELIANCE\""));
        assert!(json.contains("\"Nifty 50\""));
    }

    #[test]
    fn test_constituent_entry_from_index_constituent() {
        let ic = IndexConstituent {
            index_name: "Nifty 50".to_string(),
            symbol: "TCS".to_string(),
            isin: "INE467B01029".to_string(),
            weight: 5.20,
            sector: "IT".to_string(),
            last_updated: chrono::NaiveDate::from_ymd_opt(2026, 3, 15).unwrap(),
        };
        let entry = ConstituentEntry::from(&ic);
        assert_eq!(entry.symbol, "TCS");
        assert_eq!(entry.isin, "INE467B01029");
        assert!((entry.weight - 5.20).abs() < f64::EPSILON);
    }

    // -------------------------------------------------------------------
    // Poisoned RwLock: all three handlers must not panic
    // -------------------------------------------------------------------

    fn poisoned_constituency_map() -> SharedConstituencyMap {
        let map: SharedConstituencyMap = Arc::new(RwLock::new(None));
        let map_clone = map.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = map_clone.write().unwrap();
            panic!("intentional poison");
        }));
        assert!(result.is_err(), "should have panicked");
        assert!(map.read().is_err(), "lock should be poisoned");
        map
    }

    #[tokio::test]
    async fn test_summary_poisoned_rwlock_returns_unavailable() {
        let state = test_state(poisoned_constituency_map());
        let Json(resp) = get_constituency_summary(State(state)).await;
        assert!(!resp.available);
        assert_eq!(resp.index_count, 0);
    }

    #[tokio::test]
    async fn test_get_constituents_poisoned_rwlock_returns_not_found() {
        let state = test_state(poisoned_constituency_map());
        let Json(resp) = get_index_constituents(State(state), Path("Nifty 50".to_string())).await;
        assert!(!resp.found);
        assert!(resp.constituents.is_empty());
    }

    #[tokio::test]
    async fn test_get_stock_indices_poisoned_rwlock_returns_not_found() {
        let state = test_state(poisoned_constituency_map());
        let Json(resp) = get_stock_indices(State(state), Path("RELIANCE".to_string())).await;
        assert!(!resp.found);
        assert!(resp.indices.is_empty());
    }

    // -------------------------------------------------------------------
    // Debug impl coverage
    // -------------------------------------------------------------------

    #[test]
    fn test_constituency_summary_debug_impl() {
        let resp = ConstituencySummaryResponse {
            available: true,
            index_count: 5,
            stock_count: 200,
            indices: vec![],
        };
        let debug = format!("{resp:?}");
        assert!(debug.contains("ConstituencySummaryResponse"));
    }

    #[test]
    fn test_index_constituents_response_debug_impl() {
        let resp = IndexConstituentsResponse {
            found: true,
            index_name: "Nifty 50".to_string(),
            constituents: vec![],
        };
        let debug = format!("{resp:?}");
        assert!(debug.contains("IndexConstituentsResponse"));
    }

    #[test]
    fn test_stock_indices_response_debug_impl() {
        let resp = StockIndicesResponse {
            found: false,
            symbol: "TEST".to_string(),
            indices: vec![],
        };
        let debug = format!("{resp:?}");
        assert!(debug.contains("StockIndicesResponse"));
    }

    #[test]
    fn test_index_summary_entry_debug_impl() {
        let entry = IndexSummaryEntry {
            name: "Nifty 50".to_string(),
            constituent_count: 50,
        };
        let debug = format!("{entry:?}");
        assert!(debug.contains("IndexSummaryEntry"));
    }

    #[test]
    fn test_constituent_entry_debug_impl() {
        let entry = ConstituentEntry {
            symbol: "RELIANCE".to_string(),
            isin: "INE002A01018".to_string(),
            weight: 10.25,
            sector: "Oil Gas".to_string(),
        };
        let debug = format!("{entry:?}");
        assert!(debug.contains("ConstituentEntry"));
        assert!(debug.contains("RELIANCE"));
    }
}
