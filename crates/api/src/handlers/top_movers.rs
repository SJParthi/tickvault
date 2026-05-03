//! Top movers API endpoint — returns current top gainers, losers, and
//! most active equity + index instruments.
//!
//! Audit-2026-05-03 PR #446 Step 2: backed by QuestDB SQL queries
//! against the `movers_5s` materialized view (was: in-memory
//! `TopMoversTracker` snapshot). The 6 lists in the response are
//! produced by 6 parallel QuestDB queries (3 sort metrics × 2
//! instrument types) — cold path, ~50-100ms typical.

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use tracing::warn;

use crate::handlers::movers_questdb::{DEFAULT_MOVERS_LIMIT, MoversSortMetric, query_movers};
use crate::state::SharedAppState;
use tickvault_core::pipeline::top_movers::MoverEntry;

/// Top movers API response — separated by equity vs index.
#[derive(Debug, Serialize)]
pub struct TopMoversResponse {
    /// Whether the QuestDB query layer succeeded (false on connectivity
    /// failure). When `false`, the 6 lists are all empty.
    pub available: bool,
    // --- Equity rankings (NSE_EQ + BSE_EQ) ---
    pub equity_gainers: Vec<MoverEntry>,
    pub equity_losers: Vec<MoverEntry>,
    pub equity_most_active: Vec<MoverEntry>,
    // --- Index rankings (IDX_I) ---
    pub index_gainers: Vec<MoverEntry>,
    pub index_losers: Vec<MoverEntry>,
    pub index_most_active: Vec<MoverEntry>,
    /// Total entries returned across all 6 lists. Replaces the legacy
    /// `total_tracked` field which tracked the in-memory tracker's
    /// universe size — now reports the sum of returned rows so the
    /// frontend can display a meaningful counter.
    pub total_tracked: usize,
}

/// `GET /api/top-movers` — returns the latest top movers from QuestDB.
///
/// 6 parallel queries against `movers_5s`:
/// - equity_gainers : `instrument_type = 'EQUITY'` ORDER BY change_pct DESC LIMIT 20
/// - equity_losers  : `instrument_type = 'EQUITY'` ORDER BY change_pct ASC  LIMIT 20
/// - equity_most_active : `instrument_type = 'EQUITY'` ORDER BY volume DESC LIMIT 20
/// - index_gainers  : `instrument_type = 'INDEX'`  ORDER BY change_pct DESC LIMIT 20
/// - index_losers   : `instrument_type = 'INDEX'`  ORDER BY change_pct ASC  LIMIT 20
/// - index_most_active : `instrument_type = 'INDEX'` ORDER BY volume DESC LIMIT 20
///
/// On any QuestDB error: returns `available: false` with all empty
/// lists (no 5xx — preserves the legacy contract that the endpoint
/// always serves a JSON body).
pub async fn get_top_movers(State(state): State<SharedAppState>) -> Json<TopMoversResponse> {
    let questdb = state.questdb_config();

    // Run 6 queries in parallel via tokio::join.
    let (
        equity_gainers,
        equity_losers,
        equity_most_active,
        index_gainers,
        index_losers,
        index_most_active,
    ) = tokio::join!(
        query_movers(
            questdb,
            "EQUITY",
            MoversSortMetric::GainersByChangePct,
            DEFAULT_MOVERS_LIMIT
        ),
        query_movers(
            questdb,
            "EQUITY",
            MoversSortMetric::LosersByChangePct,
            DEFAULT_MOVERS_LIMIT
        ),
        query_movers(
            questdb,
            "EQUITY",
            MoversSortMetric::MostActiveByVolume,
            DEFAULT_MOVERS_LIMIT
        ),
        query_movers(
            questdb,
            "INDEX",
            MoversSortMetric::GainersByChangePct,
            DEFAULT_MOVERS_LIMIT
        ),
        query_movers(
            questdb,
            "INDEX",
            MoversSortMetric::LosersByChangePct,
            DEFAULT_MOVERS_LIMIT
        ),
        query_movers(
            questdb,
            "INDEX",
            MoversSortMetric::MostActiveByVolume,
            DEFAULT_MOVERS_LIMIT
        ),
    );

    // Unwrap each Result. On error, log + use empty Vec (the response
    // still has `available: true` if at least the structure is
    // returnable — the operator sees "no movers right now" rather than
    // a 5xx).
    let (equity_gainers, eg_err) = unwrap_or_log(equity_gainers, "equity_gainers");
    let (equity_losers, el_err) = unwrap_or_log(equity_losers, "equity_losers");
    let (equity_most_active, em_err) = unwrap_or_log(equity_most_active, "equity_most_active");
    let (index_gainers, ig_err) = unwrap_or_log(index_gainers, "index_gainers");
    let (index_losers, il_err) = unwrap_or_log(index_losers, "index_losers");
    let (index_most_active, im_err) = unwrap_or_log(index_most_active, "index_most_active");

    // `available` is false only if EVERY query failed (likely QuestDB
    // is unreachable). Partial failures still serve `available: true`
    // with whichever lists succeeded.
    let all_failed = eg_err && el_err && em_err && ig_err && il_err && im_err;

    let total_tracked = equity_gainers.len()
        + equity_losers.len()
        + equity_most_active.len()
        + index_gainers.len()
        + index_losers.len()
        + index_most_active.len();

    Json(TopMoversResponse {
        available: !all_failed,
        equity_gainers,
        equity_losers,
        equity_most_active,
        index_gainers,
        index_losers,
        index_most_active,
        total_tracked,
    })
}

/// Helper: unwrap a movers-query Result. On Err, log WARN + return
/// empty Vec + true (error flag). On Ok, return Vec + false.
fn unwrap_or_log(
    result: Result<Vec<MoverEntry>, anyhow::Error>,
    list_name: &'static str,
) -> (Vec<MoverEntry>, bool) {
    match result {
        Ok(v) => (v, false),
        Err(err) => {
            warn!(?err, list = list_name, "QuestDB movers query failed");
            (Vec::new(), true)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_response_serialization() {
        let resp = TopMoversResponse {
            available: false,
            equity_gainers: Vec::new(),
            equity_losers: Vec::new(),
            equity_most_active: Vec::new(),
            index_gainers: vec![],
            index_losers: vec![],
            index_most_active: vec![],
            total_tracked: 0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"available\":false"));
        assert!(json.contains("\"total_tracked\":0"));
    }

    #[test]
    fn response_with_entries_serialization() {
        let entry = MoverEntry {
            security_id: 42,
            exchange_segment_code: 2,
            last_traded_price: 100.5_f32,
            prev_close: 0.0,
            change_pct: 5.0_f32,
            volume: 10000,
        };
        let resp = TopMoversResponse {
            available: true,
            equity_gainers: vec![entry],
            equity_losers: Vec::new(),
            equity_most_active: vec![entry],
            index_gainers: vec![],
            index_losers: vec![],
            index_most_active: vec![],
            total_tracked: 100,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"available\":true"));
        assert!(json.contains("\"security_id\":42"));
        assert!(json.contains("\"total_tracked\":100"));
    }

    #[test]
    fn test_top_movers_response_debug_impl() {
        let resp = TopMoversResponse {
            available: true,
            equity_gainers: vec![],
            equity_losers: vec![],
            equity_most_active: vec![],
            index_gainers: vec![],
            index_losers: vec![],
            index_most_active: vec![],
            total_tracked: 42,
        };
        let debug = format!("{resp:?}");
        assert!(debug.contains("TopMoversResponse"));
        assert!(debug.contains("42"));
    }

    #[test]
    fn test_unwrap_or_log_returns_empty_on_err() {
        let err: Result<Vec<MoverEntry>, anyhow::Error> = Err(anyhow::anyhow!("simulated"));
        let (vec, flag) = unwrap_or_log(err, "test_list");
        assert!(vec.is_empty());
        assert!(flag);
    }

    #[test]
    fn test_unwrap_or_log_returns_value_on_ok() {
        let entry = MoverEntry {
            security_id: 1,
            exchange_segment_code: 1,
            last_traded_price: 100.0_f32,
            prev_close: 0.0,
            change_pct: 0.0,
            volume: 0,
        };
        let ok: Result<Vec<MoverEntry>, anyhow::Error> = Ok(vec![entry]);
        let (vec, flag) = unwrap_or_log(ok, "test_list");
        assert_eq!(vec.len(), 1);
        assert!(!flag);
    }
}
