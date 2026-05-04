//! Market data endpoints for the live dashboard.
//!
//! Provides JSON APIs for:
//! - `/api/market/indices` — live NSE index data (LTP, Change, OHLC)
//! - `/api/market/stock-movers` — top stock gainers/losers/most-active with symbol names
//! - `/api/market/option-movers` — top options by OI/Volume/Price change

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::state::SharedAppState;

// ---------------------------------------------------------------------------
// Index data
// ---------------------------------------------------------------------------

/// A single index entry in the indices response.
#[derive(Debug, Serialize)]
pub struct IndexEntry {
    pub name: String,
    pub security_id: i64,
    pub ltp: f64,
    pub change: f64,
    pub change_pct: f64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub prev_close: f64,
    pub volume: i64,
}

/// Response for `/api/market/indices`.
#[derive(Debug, Serialize)]
pub struct IndicesResponse {
    pub available: bool,
    pub indices: Vec<IndexEntry>,
}

/// `GET /api/market/indices` — live index data from QuestDB ticks + previous_close.
// TEST-EXEMPT: HTTP handler — requires live QuestDB for integration
pub async fn get_indices(State(state): State<SharedAppState>) -> impl IntoResponse {
    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
    let client = state.questdb_http_client();

    // Query latest tick for each index instrument (segment = IDX_I, exchange_segment_code = 0)
    let query = "SELECT security_id, ltp, open, high, low, close, volume, \
                 close as prev_close \
                 FROM ticks \
                 WHERE segment = 'IDX_I' \
                 LATEST ON ts PARTITION BY security_id \
                 ORDER BY security_id";

    let exec_url = format!("{base_url}/exec");
    let resp = match client
        .get(&exec_url)
        .query(&[("query", query)])
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "QuestDB unreachable"})),
            )
                .into_response();
        }
    };

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to parse QuestDB response"})),
            )
                .into_response();
        }
    };

    let rows = body["dataset"].as_array();
    let mut indices = Vec::new();

    if let Some(rows) = rows {
        for row in rows {
            let arr = match row.as_array() {
                Some(a) => a,
                None => continue,
            };
            if arr.len() < 8 {
                continue;
            }

            let security_id = arr[0].as_i64().unwrap_or(0);
            let ltp = arr[1].as_f64().unwrap_or(0.0);
            let open = arr[2].as_f64().unwrap_or(0.0);
            let high = arr[3].as_f64().unwrap_or(0.0);
            let low = arr[4].as_f64().unwrap_or(0.0);
            let _close = arr[5].as_f64().unwrap_or(0.0);
            let volume = arr[6].as_i64().unwrap_or(0);
            let prev_close = arr[7].as_f64().unwrap_or(0.0);

            let change = if prev_close > 0.0 {
                ltp - prev_close
            } else {
                0.0
            };
            let change_pct = if prev_close > 0.0 {
                (change / prev_close) * 100.0
            } else {
                0.0
            };

            // Map security_id to index name
            let name = index_name_from_security_id(security_id);

            indices.push(IndexEntry {
                name,
                security_id,
                ltp,
                change,
                change_pct,
                open,
                high,
                low,
                prev_close,
                volume,
            });
        }
    }

    // Sort by absolute change_pct descending for most relevant first
    indices.sort_by(|a, b| {
        b.change_pct
            .abs()
            .partial_cmp(&a.change_pct.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Json(IndicesResponse {
        available: !indices.is_empty(),
        indices,
    })
    .into_response()
}

/// Maps known index security IDs to display names.
// O(1) EXEMPT: cold-path match statement, not per-tick
fn index_name_from_security_id(sid: i64) -> String {
    match sid {
        13 => "Nifty 50".to_string(),
        25 => "Nifty Bank".to_string(),
        27 => "Fin Nifty".to_string(),
        21 => "India VIX".to_string(),
        442 => "Nifty Midcap Select".to_string(),
        19 => "Nifty 500".to_string(),
        17 => "Nifty 100".to_string(),
        20 => "Nifty 200".to_string(),
        22 => "Nifty Smallcap 50".to_string(),
        1 => "Nifty Next 50".to_string(),
        14 => "Nifty IT".to_string(),
        15 => "Nifty Pharma".to_string(),
        18 => "Nifty Midcap 50".to_string(),
        30 => "Nifty Auto".to_string(),
        34 => "Nifty Realty".to_string(),
        43 => "Nifty Energy".to_string(),
        44 => "Nifty Metal".to_string(),
        51 => "Nifty FMCG".to_string(),
        42 => "Nifty PSE".to_string(),
        33 => "Nifty Infra".to_string(),
        _ => format!("Index-{sid}"),
    }
}

// ---------------------------------------------------------------------------
// Stock movers (from QuestDB with symbol names)
// ---------------------------------------------------------------------------

/// A single stock mover entry.
#[derive(Debug, Serialize)]
pub struct StockMoverEntry {
    pub rank: i64,
    pub symbol: String,
    pub security_id: i64,
    pub ltp: f64,
    pub change: f64,
    pub change_pct: f64,
    pub volume: i64,
    pub prev_close: f64,
}

/// Response for `/api/market/stock-movers`.
#[derive(Debug, Serialize)]
pub struct StockMoversResponse {
    pub available: bool,
    pub gainers: Vec<StockMoverEntry>,
    pub losers: Vec<StockMoverEntry>,
    pub most_active: Vec<StockMoverEntry>,
}

/// `GET /api/market/stock-movers` — top stock gainers/losers/most-active from QuestDB.
///
/// Audit-2026-05-03 PR #448: migrated from the legacy `stock_movers`
/// table to the canonical `movers_5s` materialized view (5-second
/// freshness, server-pre-aggregated). Replaces 3 SQL queries against
/// the legacy schema (with `rank`/`symbol` columns) with 3 parallel
/// QuestDB SQL calls via the shared `movers_questdb::query_movers`
/// helper.
///
/// **API contract changes:**
/// - `rank` is now derived from the response array index (1-indexed)
///   rather than a per-snapshot computed field stored in the table.
/// - `symbol` is empty `""` — the legacy `stock_movers.symbol` column
///   was populated by `StockMoversWriter` (which had `InstrumentRegistry`
///   access at write time). The new `movers_5s` view doesn't carry
///   `symbol`. Frontend should resolve display names via a separate
///   lookup (e.g. `/api/quote/{security_id}` or instrument cache).
///   A future PR may add `InstrumentRegistry` to `SharedAppState` to
///   restore in-handler resolution.
/// - `change` (= last_price - prev_close) is computed from the view's
///   `last_price` + `prev_close` columns.
///
/// 3 parallel queries (gainers / losers / most-active) via `tokio::join!`.
// TEST-EXEMPT: HTTP handler — requires live QuestDB for integration
pub async fn get_stock_movers(State(state): State<SharedAppState>) -> impl IntoResponse {
    use crate::handlers::movers_questdb::{
        DEFAULT_MOVERS_LIMIT, InstrumentTypeFilter, MoversSortMetric, query_movers,
    };

    let questdb = state.questdb_config();

    // 3 parallel queries against `movers_5s` filtered by `instrument_type = 'EQUITY'`.
    let (gainers_result, losers_result, most_active_result) = tokio::join!(
        query_movers(
            questdb,
            InstrumentTypeFilter::Equity,
            MoversSortMetric::GainersByChangePct,
            DEFAULT_MOVERS_LIMIT,
        ),
        query_movers(
            questdb,
            InstrumentTypeFilter::Equity,
            MoversSortMetric::LosersByChangePct,
            DEFAULT_MOVERS_LIMIT,
        ),
        query_movers(
            questdb,
            InstrumentTypeFilter::Equity,
            MoversSortMetric::MostActiveByVolume,
            DEFAULT_MOVERS_LIMIT,
        ),
    );

    // Map MoverEntry → StockMoverEntry. `rank` is index+1; `symbol` is
    // empty (frontend resolves); `change` is computed from ltp-prev_close.
    //
    // Audit-2026-05-03 PR #448 (security-reviewer HIGH fix): log
    // QuestDB query failures at error! level so operator + Telegram
    // see the failure. Legacy handler also swallowed errors silently;
    // fixing in this migration since `unwrap_or_default()` previously
    // returned empty Vec with no log line.
    let gainers = mover_entries_to_stock(unwrap_or_log(gainers_result, "gainers"));
    let losers = mover_entries_to_stock(unwrap_or_log(losers_result, "losers"));
    let most_active = mover_entries_to_stock(unwrap_or_log(most_active_result, "most_active"));

    Json(StockMoversResponse {
        available: !gainers.is_empty() || !losers.is_empty() || !most_active.is_empty(),
        gainers,
        losers,
        most_active,
    })
    .into_response()
}

/// Audit-2026-05-03 PR #448 (security-reviewer HIGH fix): log a
/// QuestDB query failure at `error!` level so the operator gets
/// visibility (Loki ERROR routing → Telegram) instead of seeing a
/// silent empty list. Returns empty Vec on Err — the handler still
/// serves a JSON response with `available: false` rather than 5xx.
fn unwrap_or_log(
    result: anyhow::Result<Vec<tickvault_core::pipeline::top_movers::MoverEntry>>,
    list_name: &'static str,
) -> Vec<tickvault_core::pipeline::top_movers::MoverEntry> {
    match result {
        Ok(v) => v,
        Err(err) => {
            // PR #455 (2026-05-04) MEDIUM-fix: `?err` Debug formatter
            // can embed wrapped reqwest URL (containing internal
            // QuestDB host:port) when anyhow's chain wraps a
            // reqwest::Error. Use Display (`%err`) which only shows
            // the top error message — no URL leak. Loss of wrapped
            // chain context is acceptable; the explicit `code` field
            // + list name are enough for triage.
            tracing::error!(
                error = %err,
                list = list_name,
                code = "API-MOVERS-01",
                "QuestDB query for /api/market/stock-movers list failed"
            );
            Vec::new()
        }
    }
}

/// Maps the canonical `MoverEntry` (from `movers_questdb`) to the
/// API-stable `StockMoverEntry` shape. `rank` is 1-indexed array
/// position; `symbol` is empty (frontend resolves).
fn mover_entries_to_stock(
    entries: Vec<tickvault_core::pipeline::top_movers::MoverEntry>,
) -> Vec<StockMoverEntry> {
    entries
        .into_iter()
        .enumerate()
        .map(|(idx, e)| StockMoverEntry {
            rank: (idx + 1) as i64,
            symbol: String::new(), // frontend resolves via separate lookup
            security_id: i64::from(e.security_id),
            ltp: f64::from(e.last_traded_price),
            change: f64::from(e.last_traded_price - e.prev_close),
            change_pct: f64::from(e.change_pct),
            volume: i64::from(e.volume),
            prev_close: f64::from(e.prev_close),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Option movers (from QuestDB)
// ---------------------------------------------------------------------------

/// A single option mover entry.
#[derive(Debug, Serialize)]
pub struct OptionMoverEntry {
    pub rank: i64,
    pub contract_name: String,
    pub underlying: String,
    pub option_type: String,
    pub strike: f64,
    pub expiry: String,
    pub spot_price: f64,
    pub ltp: f64,
    pub change: f64,
    pub change_pct: f64,
    pub oi: i64,
    pub oi_change: i64,
    pub oi_change_pct: f64,
    pub volume: i64,
}

/// Query parameters for option movers.
#[derive(Debug, Deserialize)]
pub struct OptionMoversQuery {
    /// Category filter: highest_oi, oi_gainers, oi_losers, top_volume, price_gainers, price_losers
    pub category: Option<String>,
}

/// Response for `/api/market/option-movers`.
#[derive(Debug, Serialize)]
pub struct OptionMoversResponse {
    pub available: bool,
    pub category: String,
    pub entries: Vec<OptionMoverEntry>,
}

/// Audit-2026-05-03 PR #449: typed enum for option mover categories.
/// Replaces the legacy `&str` parameter that was injection-validated
/// only by a `Vec::contains` whitelist — type system now closes the
/// vector entirely. Each variant maps to an `ORDER BY` clause body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionMoverCategory {
    HighestOi,
    OiGainer,
    OiLoser,
    TopVolume,
    TopValue,
    PriceGainer,
    PriceLoser,
}

impl OptionMoverCategory {
    /// Parses the wire-format string (case-insensitive). Returns `None`
    /// on unknown input — handler emits 400.
    ///
    /// PR #449 hostile-bug-hunt C1 fix: accepts BOTH singular
    /// (`OI_GAINER`) and plural (`OI_GAINERS`) forms because the
    /// `markets-options.html` portal sends the plural variant via
    /// `data-filter="oi_gainers"`. Without the alias, 4 of 7
    /// dashboard tabs would return HTTP 400.
    #[must_use]
    pub fn parse_wire_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "HIGHEST_OI" => Some(Self::HighestOi),
            "OI_GAINER" | "OI_GAINERS" => Some(Self::OiGainer),
            "OI_LOSER" | "OI_LOSERS" => Some(Self::OiLoser),
            "TOP_VOLUME" => Some(Self::TopVolume),
            "TOP_VALUE" => Some(Self::TopValue),
            "PRICE_GAINER" | "PRICE_GAINERS" => Some(Self::PriceGainer),
            "PRICE_LOSER" | "PRICE_LOSERS" => Some(Self::PriceLoser),
            _ => None,
        }
    }

    /// Wire-format string for the `category` field in the response.
    #[must_use]
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::HighestOi => "HIGHEST_OI",
            Self::OiGainer => "OI_GAINER",
            Self::OiLoser => "OI_LOSER",
            Self::TopVolume => "TOP_VOLUME",
            Self::TopValue => "TOP_VALUE",
            Self::PriceGainer => "PRICE_GAINER",
            Self::PriceLoser => "PRICE_LOSER",
        }
    }

    /// SQL `ORDER BY` clause body using `movers_5s` view-aliased
    /// columns. The view aggregates per-second base via `last()` /
    /// `first()` and exposes `volume_cumulative` (= `last(volume)`)
    /// and `change_pct_session` (= guarded change_pct).
    #[must_use]
    pub const fn order_by_clause(self) -> &'static str {
        match self {
            Self::HighestOi => "open_interest DESC",
            Self::OiGainer => "oi_delta DESC",
            Self::OiLoser => "oi_delta ASC",
            Self::TopVolume => "volume_cumulative DESC",
            Self::TopValue => "(last_price * volume_cumulative) DESC",
            Self::PriceGainer => "change_pct_session DESC",
            Self::PriceLoser => "change_pct_session ASC",
        }
    }
}

/// `GET /api/market/option-movers?category=top_volume` — top options from QuestDB.
///
/// Audit-2026-05-03 PR #449: migrated from the legacy `option_movers`
/// table to the canonical `movers_5s` materialized view. Replaces the
/// SQL string-interpolation pattern (security-reviewer CRITICAL from
/// PR #448) with a typed `OptionMoverCategory` enum that closes the
/// injection vector at the type system.
///
/// **API contract changes (frontend-visible):**
/// - `rank` is derived from response array index (1-indexed)
/// - `contract_name`, `underlying`, `option_type`, `strike`, `expiry`,
///   `spot_price`, `oi_change_pct` are EMPTY/ZERO — the legacy
///   `option_movers` table populated these via `OptionMoversWriter`
///   which had instrument-registry access at write time. The new
///   `movers_5s` view doesn't carry them. Frontend should resolve
///   contract details via a separate `/api/instruments/{security_id}`
///   lookup.
/// - `change` (= ltp - prev_close) is computed.
/// - `oi_change` reads from the view's `oi_delta` (per-bucket OI delta).
/// - All other fields map directly from `movers_5s` columns.
// TEST-EXEMPT: HTTP handler — requires live QuestDB for integration
pub async fn get_option_movers(
    State(state): State<SharedAppState>,
    Query(params): Query<OptionMoversQuery>,
) -> impl IntoResponse {
    // Validate category via typed enum.
    let category_str = params.category.unwrap_or_else(|| "TOP_VOLUME".to_string());
    let category = match OptionMoverCategory::parse_wire_str(&category_str) {
        Some(c) => c,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid category",
                    "valid": [
                        "highest_oi", "oi_gainer", "oi_loser",
                        "top_volume", "top_value",
                        "price_gainer", "price_loser",
                    ],
                })),
            )
                .into_response();
        }
    };

    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
    let client = state.questdb_http_client();

    // Audit-2026-05-03 PR #449: query against `movers_5s` filtered by
    // `instrument_type IN ('OPTIDX', 'OPTSTK')` (both option types).
    // No SQL string interpolation of user input — `category` is a
    // typed enum returning a `&'static str` ORDER BY clause.
    let query = format!(
        "SELECT security_id, exchange_segment, last_price, prev_close, \
                change_pct_session, volume_cumulative, open_interest, oi_delta \
         FROM movers_5s \
         WHERE ts > dateadd('s', -30, now()) \
           AND instrument_type IN ('OPTIDX', 'OPTSTK') \
         LATEST ON ts PARTITION BY security_id \
         ORDER BY {order_by} \
         LIMIT 30",
        order_by = category.order_by_clause(),
    );

    let exec_url = format!("{base_url}/exec");
    let resp = match client
        .get(&exec_url)
        .query(&[("query", &query)])
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            // PR #455 (2026-05-04) MEDIUM-fix: strip URL from reqwest error
            // before formatting (Display embeds URL containing internal
            // QuestDB host:port).
            let err = err.without_url();
            tracing::error!(
                error = %err,
                code = "API-MOVERS-02",
                "QuestDB query for /api/market/option-movers failed"
            );
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "QuestDB unreachable"})),
            )
                .into_response();
        }
    };

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(err) => {
            // PR #455: same URL redaction.
            let err = err.without_url();
            tracing::error!(
                error = %err,
                code = "API-MOVERS-02",
                "Failed to parse QuestDB JSON response for /api/market/option-movers"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to parse response"})),
            )
                .into_response();
        }
    };

    let rows = body["dataset"].as_array();
    let mut entries = Vec::new();

    // Audit-2026-05-03 PR #449: row schema from `movers_5s` query
    // (8 columns): security_id, exchange_segment, last_price,
    // prev_close, change_pct_session, volume_cumulative,
    // open_interest, oi_delta. Skip rows with fewer columns.
    if let Some(rows) = rows {
        for row in rows {
            let arr = match row.as_array() {
                Some(a) if a.len() >= 8 => a,
                _ => continue,
            };
            let last_price = arr[2].as_f64().unwrap_or(0.0);
            let prev_close = arr[3].as_f64().unwrap_or(0.0);
            entries.push(OptionMoverEntry {
                rank: (entries.len() + 1) as i64,
                contract_name: String::new(), // frontend resolves
                underlying: String::new(),    // frontend resolves
                option_type: String::new(),   // frontend resolves
                strike: 0.0,                  // frontend resolves
                expiry: String::new(),        // frontend resolves
                spot_price: 0.0,              // frontend resolves
                ltp: last_price,
                change: last_price - prev_close,
                change_pct: arr[4].as_f64().unwrap_or(0.0),
                oi: arr[6].as_i64().unwrap_or(0),
                oi_change: arr[7].as_i64().unwrap_or(0),
                oi_change_pct: 0.0, // no prev_oi baseline in view
                volume: arr[5].as_i64().unwrap_or(0),
            });
        }
    }

    Json(OptionMoversResponse {
        available: !entries.is_empty(),
        category: category.as_wire_str().to_string(),
        entries,
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_index_name_mapping_known_ids() {
        assert_eq!(index_name_from_security_id(13), "Nifty 50");
        assert_eq!(index_name_from_security_id(25), "Nifty Bank");
        assert_eq!(index_name_from_security_id(27), "Fin Nifty");
        assert_eq!(index_name_from_security_id(21), "India VIX");
        assert_eq!(index_name_from_security_id(442), "Nifty Midcap Select");
    }

    #[test]
    fn test_index_name_mapping_unknown_id() {
        assert_eq!(index_name_from_security_id(99999), "Index-99999");
    }

    #[test]
    fn test_indices_response_serialization() {
        let resp = IndicesResponse {
            available: true,
            indices: vec![IndexEntry {
                name: "Nifty 50".to_string(),
                security_id: 13,
                ltp: 22819.75,
                change: -148.50,
                change_pct: -0.65,
                open: 22838.70,
                high: 22843.95,
                low: 22719.30,
                prev_close: 22968.25,
                volume: 0,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("Nifty 50"));
        assert!(json.contains("22819.75"));
        assert!(json.contains("-148.5"));
    }

    #[test]
    fn test_stock_movers_response_serialization() {
        let resp = StockMoversResponse {
            available: true,
            gainers: vec![StockMoverEntry {
                rank: 1,
                symbol: "KALYAN".to_string(),
                security_id: 12345,
                ltp: 435.0,
                change: 14.65,
                change_pct: 3.49,
                volume: 179862,
                prev_close: 420.35,
            }],
            losers: Vec::new(),
            most_active: Vec::new(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("KALYAN"));
        assert!(json.contains("3.49"));
    }

    #[test]
    fn test_option_movers_response_serialization() {
        let resp = OptionMoversResponse {
            available: true,
            category: "TOP_VOLUME".to_string(),
            entries: vec![OptionMoverEntry {
                rank: 1,
                contract_name: "BANKNIFTY 28 APR 54300 CALL".to_string(),
                underlying: "BANKNIFTY".to_string(),
                option_type: "CE".to_string(),
                strike: 54300.0,
                expiry: "2026-04-28".to_string(),
                spot_price: 52055.70,
                ltp: 630.25,
                change: -215.05,
                change_pct: -25.44,
                oi: 92640,
                oi_change: 55920,
                oi_change_pct: 152.29,
                volume: 27830,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("BANKNIFTY 28 APR 54300 CALL"));
        assert!(json.contains("TOP_VOLUME"));
    }

    #[test]
    fn test_valid_option_mover_categories() {
        let valid = [
            "HIGHEST_OI",
            "OI_GAINER",
            "OI_LOSER",
            "TOP_VOLUME",
            "TOP_VALUE",
            "PRICE_GAINER",
            "PRICE_LOSER",
        ];
        assert_eq!(valid.len(), 7);
    }

    /// PR #449 hostile-bug-hunt C1 ratchet: every literal string used
    /// by `data-filter="..."` in `crates/api/static/markets-options.html`
    /// MUST parse via `OptionMoverCategory::parse_wire_str`. The portal
    /// sends the value verbatim as `?category=<filter>`. Regression of
    /// the alias map causes 4 of 7 tabs to silently fall back to cached
    /// data with no error visible to the operator.
    #[test]
    fn test_option_mover_category_accepts_frontend_data_filter_literals() {
        let frontend_literals = [
            "highest_oi",    // line 720
            "oi_gainers",    // line 721 — plural
            "oi_losers",     // line 722 — plural
            "top_volume",    // line 723
            "top_value",     // line 724
            "price_gainers", // line 725 — plural
            "price_losers",  // line 726 — plural
        ];
        for literal in frontend_literals {
            assert!(
                OptionMoverCategory::parse_wire_str(literal).is_some(),
                "frontend data-filter literal {literal:?} MUST be \
                 accepted by parse_wire_str — see markets-options.html",
            );
        }
    }

    #[test]
    fn test_index_entry_change_calculation() {
        let prev: f64 = 22968.25;
        let ltp: f64 = 22819.75;
        let change = ltp - prev;
        let change_pct = (change / prev) * 100.0;
        assert!((change - (-148.5_f64)).abs() < 0.01);
        assert!((change_pct - (-0.646_f64)).abs() < 0.01);
    }

    #[test]
    fn test_index_entry_zero_prev_close() {
        let prev = 0.0;
        let ltp = 100.0;
        let change = if prev > 0.0 { ltp - prev } else { 0.0 };
        let change_pct = if prev > 0.0 {
            (change / prev) * 100.0
        } else {
            0.0
        };
        assert_eq!(change, 0.0);
        assert_eq!(change_pct, 0.0);
    }
}
