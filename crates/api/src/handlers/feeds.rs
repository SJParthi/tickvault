//! Feed-toggle endpoints (operator AskUserQuestion 2026-06-19: "Feed-toggle API
//! endpoint"). True two-way runtime control of the pluggable market-data feeds.
//!
//! - `GET  /api/feeds`          → current per-feed enabled state (status).
//! - `POST /api/feeds/{feed}`   → `{ "enabled": bool }` flips the feed live.
//!
//! Both live behind the existing bearer-auth `protected_routes`. The toggle flips
//! a shared [`crate::feed_state::FeedRuntimeState`] `Arc` that the Groww bridge
//! reads every loop iteration — pause/resume Groww writes mid-session, no restart.
//!
//! **Honest envelope (slice 1):** only Groww is runtime-toggleable. `POST` for
//! `dhan` returns 400 — disabling the primary trading feed mid-session is unsafe
//! (Dhan stays config+restart, per Step C). Dhan's flag is still reported by GET.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::feed_state::Feed;
use crate::state::SharedAppState;

/// `GET /api/feeds` / `POST /api/feeds/{feed}` success payload — the live state.
#[derive(Debug, Serialize)]
pub struct FeedsStatusResponse {
    pub dhan_enabled: bool,
    pub groww_enabled: bool,
    /// Whether the Groww bridge task is actually running this process. If you set
    /// `groww_enabled=true` but this is `false`, the lane was not spawned at boot
    /// (`groww_enabled` was false then) — the toggle is recorded but takes no
    /// effect until you set it in config and restart. Surfaced so the API never
    /// reports a misleading "enabled" that does nothing (3-agent honesty fix).
    pub groww_lane_running: bool,
}

/// `POST /api/feeds/{feed}` request body.
#[derive(Debug, Deserialize)]
pub struct SetFeedRequest {
    pub enabled: bool,
}

/// Error payload for the 400 cases (unknown feed / non-toggleable feed).
#[derive(Debug, Serialize)]
pub struct FeedErrorResponse {
    pub error: String,
    pub allowed: Vec<&'static str>,
}

fn current_status(state: &SharedAppState) -> FeedsStatusResponse {
    let snap = state.feed_runtime().snapshot();
    FeedsStatusResponse {
        dhan_enabled: snap.dhan_enabled,
        groww_enabled: snap.groww_enabled,
        groww_lane_running: snap.groww_lane_running,
    }
}

/// `GET /api/feeds` — report every feed's current runtime enabled state.
pub async fn get_feeds(State(state): State<SharedAppState>) -> Json<FeedsStatusResponse> {
    Json(current_status(&state))
}

/// `POST /api/feeds/{feed}` — flip a feed's runtime flag (Groww only in slice 1).
///
/// Returns `200` + the new full status, or `400` for an unknown feed name or a
/// feed that is not runtime-toggleable (Dhan).
pub async fn set_feed(
    State(state): State<SharedAppState>,
    Path(feed_name): Path<String>,
    Json(req): Json<SetFeedRequest>,
) -> Result<Json<FeedsStatusResponse>, (StatusCode, Json<FeedErrorResponse>)> {
    let Some(feed) = Feed::parse(&feed_name) else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(FeedErrorResponse {
                error: format!("unknown feed: {feed_name}"),
                allowed: vec![Feed::Dhan.as_str(), Feed::Groww.as_str()],
            }),
        ));
    };
    if !feed.is_runtime_toggleable() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(FeedErrorResponse {
                error: format!(
                    "feed '{}' cannot be toggled at runtime (the primary trading feed is \
                     config+restart only); only 'groww' is runtime-toggleable",
                    feed.as_str()
                ),
                allowed: vec![Feed::Groww.as_str()],
            }),
        ));
    }

    state.feed_runtime().set_enabled(feed, req.enabled);
    let action = if req.enabled { "enable" } else { "disable" };
    info!(
        feed = feed.as_str(),
        enabled = req.enabled,
        "feed runtime toggled via API"
    );
    // Honesty (3-agent hostile review): enabling a lane that was never spawned at
    // boot is recorded but has NO effect. Tell the operator instead of silently
    // returning a misleading "enabled" — the response also carries
    // `groww_lane_running: false` so the truth is machine-readable.
    if feed == Feed::Groww && req.enabled && !state.feed_runtime().is_groww_lane_running() {
        tracing::warn!(
            "feed 'groww' enabled via API but its lane was not started at boot \
             (groww_enabled=false then) — set groww_enabled=true in config and restart \
             to start it; the API toggle only pauses/resumes a running lane"
        );
    }
    metrics::counter!(
        "tv_feed_runtime_toggle_total",
        "feed" => feed.as_str(),
        "action" => action,
    )
    .increment(1);

    Ok(Json(current_status(&state)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed_state::FeedRuntimeState;
    use crate::state::SharedAppState;
    use std::sync::Arc;
    use tickvault_common::config::{DhanConfig, FeedsConfig, InstrumentConfig, QuestDbConfig};

    fn test_qdb() -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        }
    }

    fn test_dhan() -> DhanConfig {
        DhanConfig {
            websocket_url: "wss://api-feed.dhan.co".to_string(),
            order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
            rest_api_base_url: "https://api.dhan.co/v2".to_string(),
            auth_base_url: "https://auth.dhan.co".to_string(),
            instrument_csv_url: "https://images.dhan.co/api-data/api-scrip-master-detailed.csv"
                .to_string(),
            instrument_csv_fallback_url: "https://images.dhan.co/api-data/api-scrip-master.csv"
                .to_string(),
            max_instruments_per_connection: 5000,
            max_websocket_connections: 5,
            sandbox_base_url: String::new(),
        }
    }

    fn test_instrument() -> InstrumentConfig {
        InstrumentConfig {
            daily_download_time: "08:55:00".to_string(),
            csv_cache_directory: "/tmp/tv-cache".to_string(),
            csv_cache_filename: "instruments.csv".to_string(),
            csv_download_timeout_secs: 120,
            build_window_start: "08:25:00".to_string(),
            build_window_end: "08:55:00".to_string(),
        }
    }

    fn test_state(feeds: FeedsConfig) -> SharedAppState {
        let health = Arc::new(crate::state::SystemHealthStatus::new());
        SharedAppState::new_with_feed_runtime(
            test_qdb(),
            test_dhan(),
            test_instrument(),
            health,
            Arc::new(FeedRuntimeState::from_config(&feeds)),
        )
    }

    #[tokio::test]
    async fn test_get_feeds_reports_seeded_state() {
        let state = test_state(FeedsConfig {
            dhan_enabled: true,
            groww_enabled: false,
        });
        let Json(resp) = get_feeds(State(state)).await;
        assert!(resp.dhan_enabled);
        assert!(!resp.groww_enabled);
    }

    #[tokio::test]
    async fn test_set_feed_groww_enable_flips_state() {
        let state = test_state(FeedsConfig {
            dhan_enabled: true,
            groww_enabled: false,
        });
        let res = set_feed(
            State(state.clone()),
            Path("groww".to_string()),
            Json(SetFeedRequest { enabled: true }),
        )
        .await;
        let Json(resp) = res.expect("groww enable must succeed");
        assert!(resp.groww_enabled, "groww now enabled");
        assert!(state.feed_runtime().is_enabled(Feed::Groww));
    }

    #[tokio::test]
    async fn test_set_feed_groww_disable_flips_state() {
        let state = test_state(FeedsConfig {
            dhan_enabled: true,
            groww_enabled: true,
        });
        let res = set_feed(
            State(state.clone()),
            Path("groww".to_string()),
            Json(SetFeedRequest { enabled: false }),
        )
        .await;
        let Json(resp) = res.expect("groww disable must succeed");
        assert!(!resp.groww_enabled);
        assert!(!state.feed_runtime().is_enabled(Feed::Groww));
    }

    #[tokio::test]
    async fn test_enabling_groww_reports_lane_not_running_when_unspawned() {
        // Honesty: enabling Groww via API when the bridge was not spawned at boot
        // records the flag but reports groww_lane_running=false (no misleading OK).
        let state = test_state(FeedsConfig {
            dhan_enabled: true,
            groww_enabled: false,
        });
        let Json(resp) = set_feed(
            State(state),
            Path("groww".to_string()),
            Json(SetFeedRequest { enabled: true }),
        )
        .await
        .expect("enable accepted");
        assert!(resp.groww_enabled, "flag recorded");
        assert!(
            !resp.groww_lane_running,
            "but the lane is not running (was not spawned at boot) — honest signal"
        );
    }

    #[tokio::test]
    async fn test_set_feed_dhan_is_rejected_400() {
        let state = test_state(FeedsConfig::default());
        let res = set_feed(
            State(state.clone()),
            Path("dhan".to_string()),
            Json(SetFeedRequest { enabled: false }),
        )
        .await;
        let Err((code, _body)) = res else {
            panic!("disabling dhan at runtime must be rejected");
        };
        assert_eq!(code, StatusCode::BAD_REQUEST);
        // Dhan stayed enabled — the rejected toggle did not flip it.
        assert!(state.feed_runtime().is_enabled(Feed::Dhan));
    }

    #[tokio::test]
    async fn test_set_feed_unknown_feed_is_rejected_400() {
        let state = test_state(FeedsConfig::default());
        let res = set_feed(
            State(state),
            Path("kite".to_string()),
            Json(SetFeedRequest { enabled: true }),
        )
        .await;
        let Err((code, Json(body))) = res else {
            panic!("unknown feed must be rejected");
        };
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(body.error.contains("kite"));
    }
}
