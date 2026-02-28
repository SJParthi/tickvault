//! Stats endpoint — proxies QuestDB queries server-side to avoid CORS.
//!
//! Returns dashboard statistics in a single JSON response:
//! table count, underlyings, derivatives, subscribed indices, ticks.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::state::SharedAppState;

/// Timeout for QuestDB stats queries (cold path, not tick processing).
const QUESTDB_STATS_TIMEOUT_SECS: u64 = 3;

/// Dashboard statistics response.
#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub questdb_reachable: bool,
    pub tables: u64,
    pub underlyings: u64,
    pub derivatives: u64,
    pub subscribed_indices: u64,
    pub ticks: u64,
}

/// `GET /api/stats` — fetch QuestDB counts in one call.
pub async fn get_stats(State(state): State<SharedAppState>) -> Json<StatsResponse> {
    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_STATS_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            return Json(StatsResponse {
                questdb_reachable: false,
                tables: 0,
                underlyings: 0,
                derivatives: 0,
                subscribed_indices: 0,
                ticks: 0,
            });
        }
    };

    let tables = query_count(&client, &base_url, "SHOW TABLES").await;
    let questdb_reachable = tables.is_some();

    Json(StatsResponse {
        questdb_reachable,
        tables: tables.unwrap_or(0),
        underlyings: query_count(&client, &base_url, "SELECT count() FROM fno_underlyings")
            .await
            .unwrap_or(0),
        derivatives: query_count(
            &client,
            &base_url,
            "SELECT count() FROM derivative_contracts",
        )
        .await
        .unwrap_or(0),
        subscribed_indices: query_count(
            &client,
            &base_url,
            "SELECT count() FROM subscribed_indices",
        )
        .await
        .unwrap_or(0),
        ticks: query_count(&client, &base_url, "SELECT count() FROM ticks")
            .await
            .unwrap_or(0),
    })
}

/// Runs a count query against QuestDB's HTTP endpoint. Returns None on failure.
async fn query_count(client: &reqwest::Client, base_url: &str, sql: &str) -> Option<u64> {
    let url = format!("{}/exec", base_url);
    let resp = client
        .get(&url)
        .query(&[("query", sql)])
        .send()
        .await
        .ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    let dataset = body.get("dataset")?.as_array()?;

    // SHOW TABLES returns rows of [table_name], count them
    if sql.starts_with("SHOW") {
        return Some(dataset.len() as u64);
    }

    // SELECT count() returns [[N]]
    dataset.first()?.as_array()?.first()?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_response_serialization() {
        let stats = StatsResponse {
            questdb_reachable: true,
            tables: 5,
            underlyings: 214,
            derivatives: 96948,
            subscribed_indices: 31,
            ticks: 0,
        };
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("\"tables\":5"));
        assert!(json.contains("\"questdb_reachable\":true"));
    }
}
