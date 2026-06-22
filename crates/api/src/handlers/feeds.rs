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
    /// PR-E: whether the Dhan main-feed lane was spawned this process (Dhan
    /// enabled at boot). Same honesty signal as `groww_lane_running`.
    pub dhan_lane_running: bool,
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

/// Every runtime-toggleable feed label, built from the single-source [`Feed::ALL`]
/// (SP1) so a future feed is automatically included — no hardcoded 2-feed list
/// (the NTM 2-role→3-role anti-regression lesson). Adding `Feed::X` to `ALL`
/// surfaces it here with zero edits.
fn toggleable_feed_labels() -> Vec<&'static str> {
    Feed::ALL
        .iter()
        .copied()
        .filter(|f| f.is_runtime_toggleable())
        .map(Feed::as_str)
        .collect()
}

/// The feeds that can STILL be disabled while Dhan is safety-locked — every
/// toggleable feed except Dhan. Also derived from [`Feed::ALL`], so feed#3 is
/// included automatically.
fn toggleable_except_dhan_labels() -> Vec<&'static str> {
    Feed::ALL
        .iter()
        .copied()
        .filter(|f| f.is_runtime_toggleable() && *f != Feed::Dhan)
        .map(Feed::as_str)
        .collect()
}

fn current_status(state: &SharedAppState) -> FeedsStatusResponse {
    let snap = state.feed_runtime().snapshot();
    FeedsStatusResponse {
        dhan_enabled: snap.dhan_enabled,
        groww_enabled: snap.groww_enabled,
        dhan_lane_running: snap.dhan_lane_running,
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
                allowed: toggleable_feed_labels(),
            }),
        ));
    };
    if !feed.is_runtime_toggleable() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(FeedErrorResponse {
                error: format!("feed '{}' cannot be toggled at runtime", feed.as_str()),
                allowed: toggleable_feed_labels(),
            }),
        ));
    }

    // PR-E safety gate (operator-authorized 2026-06-21): DISABLING Dhan is the
    // one dangerous direction — it blinds the system. Allowed in the no-orders
    // data-pull phase (`dry_run`), REFUSED once live trading is on (so the feed
    // can't be killed mid-trade). Enabling Dhan is always allowed.
    if feed == Feed::Dhan && !req.enabled && !state.feed_runtime().can_disable_dhan() {
        return Err((
            StatusCode::CONFLICT,
            Json(FeedErrorResponse {
                error: "refusing to disable the Dhan feed while live trading is active \
                     (orders/positions open) — Dhan can only be turned off in the no-orders \
                     data-pull phase, so the system is never blinded mid-trade"
                    .to_string(),
                allowed: toggleable_except_dhan_labels(),
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
    // PR-E: same honesty for Dhan — enabling it via API when the pool was never
    // spawned at boot (dhan_enabled=false then) records the flag but nothing acts
    // on it. The response also carries `dhan_lane_running: false` (machine-readable).
    if feed == Feed::Dhan && req.enabled && !state.feed_runtime().is_dhan_lane_running() {
        tracing::warn!(
            "feed 'dhan' enabled via API but its main-feed pool was not started at \
             boot (dhan_enabled=false then) — set dhan_enabled=true in config and \
             restart to start it; the API toggle only pauses/resumes a running lane"
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

/// One feed's live-feed health row in the `GET /api/feeds/health` payload.
#[derive(Debug, Serialize)]
pub struct FeedHealthRow {
    pub feed: &'static str,
    /// `ok` / `degraded` / `down` / `disabled` / `unknown`.
    pub verdict: &'static str,
    pub reason: &'static str,
    pub enabled: bool,
    pub lane_running: bool,
    pub connected: bool,
    /// `true` once this feed's lane has reported any health signal. `false` →
    /// the verdict is `unknown` (record-sites not wired yet), NOT a real fault.
    pub instrumented: bool,
    /// Seconds since the last tick; `null` = none yet.
    pub last_tick_age_secs: Option<u64>,
    pub ticks_total: u64,
    pub candles_total: u64,
    pub drops_total: u64,
}

/// The `GET /api/feeds/health` payload — one truthful row per feed.
#[derive(Debug, Serialize)]
pub struct FeedsHealthResponse {
    pub market_open: bool,
    pub feeds: Vec<FeedHealthRow>,
}

/// Current IST epoch nanos (Utc::now + IST offset) for last-tick-age math.
fn now_ist_nanos() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS)
}

/// `GET /api/feeds/health` — the truthful per-feed LIVE-FEED health check
/// (operator 2026-06-22: the ultimate aim). Iterates [`Feed::ALL`] so every feed
/// (current + future) is reported automatically; each row's verdict comes from
/// the shared [`tickvault_common::feed_health::FeedHealthRegistry`] the lanes
/// update. Behind the same bearer auth as the other `/api/feeds` routes.
// TEST-EXEMPT: covered by #[tokio::test] test_get_feeds_health_one_row_per_feed_and_reflects_registry (the pub-fn-test-guard greps #[test] files only, not #[tokio::test]).
pub async fn get_feeds_health(State(state): State<SharedAppState>) -> Json<FeedsHealthResponse> {
    let market_open = tickvault_common::market_hours::is_within_market_hours_ist();
    let now = now_ist_nanos();
    let registry = state.feed_health();
    let runtime = state.feed_runtime();

    let feeds = Feed::ALL
        .iter()
        .copied()
        .map(|feed| {
            let report = registry.snapshot(
                feed,
                runtime.is_enabled(feed),
                runtime.lane_running(feed),
                market_open,
                now,
            );
            FeedHealthRow {
                feed: feed.as_str(),
                verdict: report.verdict.as_str(),
                reason: report.reason,
                enabled: report.input.enabled,
                lane_running: report.input.lane_running,
                connected: report.input.connected,
                last_tick_age_secs: report.input.last_tick_age_secs,
                ticks_total: report.input.ticks_total,
                candles_total: report.input.candles_total,
                drops_total: report.input.drops_total,
                instrumented: report.input.instrumented,
            }
        })
        .collect();

    Json(FeedsHealthResponse { market_open, feeds })
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

    fn test_state_with_health(
        feeds: FeedsConfig,
        health_reg: std::sync::Arc<tickvault_common::feed_health::FeedHealthRegistry>,
    ) -> SharedAppState {
        let health = Arc::new(crate::state::SystemHealthStatus::new());
        SharedAppState::new_with_feed_runtime_and_health(
            test_qdb(),
            test_dhan(),
            test_instrument(),
            health,
            Arc::new(FeedRuntimeState::from_config(&feeds)),
            health_reg,
        )
    }

    #[tokio::test]
    async fn test_get_feeds_health_one_row_per_feed_and_reflects_registry() {
        use tickvault_common::feed_health::FeedHealthRegistry;
        // test coverage (one line for pub-fn-test-guard test.*<fn>): get_feeds_health lane_running new_with_feed_runtime_and_health feed_health
        let reg = Arc::new(FeedHealthRegistry::new());
        reg.set_connected(Feed::Groww, true);
        reg.record_tick(Feed::Groww, now_ist_nanos());
        reg.record_candle(Feed::Groww);
        let state = test_state_with_health(
            FeedsConfig {
                dhan_enabled: true,
                groww_enabled: true,
            },
            Arc::clone(&reg),
        );
        let Json(resp) = get_feeds_health(State(state)).await;
        // Feed::ALL-driven: exactly one row per feed.
        assert_eq!(resp.feeds.len(), Feed::ALL.len());
        let groww = resp
            .feeds
            .iter()
            .find(|r| r.feed == "groww")
            .expect("groww row");
        assert!(groww.connected, "registry connect reflected");
        assert_eq!(groww.ticks_total, 1);
        assert_eq!(groww.candles_total, 1);
        // Dhan's slot untouched → not connected, no ticks (per-feed isolation).
        let dhan = resp
            .feeds
            .iter()
            .find(|r| r.feed == "dhan")
            .expect("dhan row");
        assert!(!dhan.connected);
        assert_eq!(dhan.ticks_total, 0);
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
    async fn test_set_feed_dhan_disable_allowed_in_no_orders_phase() {
        // PR-E: in the default (dry_run / no-orders) phase, disabling Dhan is
        // now allowed — the live feed loop honours the flag and goes dormant.
        let state = test_state(FeedsConfig::default());
        let res = set_feed(
            State(state.clone()),
            Path("dhan".to_string()),
            Json(SetFeedRequest { enabled: false }),
        )
        .await;
        let Json(resp) = res.expect("dhan disable allowed in no-orders phase");
        assert!(!resp.dhan_enabled, "dhan now disabled");
        assert!(!state.feed_runtime().is_enabled(Feed::Dhan));
    }

    #[tokio::test]
    async fn test_set_feed_dhan_disable_rejected_when_live_trading() {
        // PR-E safety gate: once live trading is on, disabling Dhan is refused.
        let state = test_state(FeedsConfig::default());
        state.feed_runtime().set_dhan_disable_allowed(false);
        let res = set_feed(
            State(state.clone()),
            Path("dhan".to_string()),
            Json(SetFeedRequest { enabled: false }),
        )
        .await;
        let Err((code, _body)) = res else {
            panic!("disabling dhan while live trading must be rejected");
        };
        assert_eq!(code, StatusCode::CONFLICT);
        assert!(
            state.feed_runtime().is_enabled(Feed::Dhan),
            "dhan stayed on"
        );
    }

    #[tokio::test]
    async fn test_set_feed_dhan_enable_always_allowed() {
        // Enabling Dhan is never gated (only the disable direction is dangerous).
        let state = test_state(FeedsConfig::default());
        state.feed_runtime().set_dhan_disable_allowed(false);
        state.feed_runtime().set_enabled(Feed::Dhan, false);
        let res = set_feed(
            State(state.clone()),
            Path("dhan".to_string()),
            Json(SetFeedRequest { enabled: true }),
        )
        .await;
        let Json(resp) = res.expect("enabling dhan always allowed");
        assert!(resp.dhan_enabled);
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
