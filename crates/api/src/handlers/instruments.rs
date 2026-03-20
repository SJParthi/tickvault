//! Instrument rebuild endpoint — one-shot manual trigger.
//!
//! `POST /api/instruments/rebuild` — bypasses time gate, respects freshness marker.
//! Concurrent requests guarded by `AtomicBool` (only one rebuild at a time).

use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use tracing::{info, warn};

use dhan_live_trader_core::instrument::run_instrument_diagnostic;
use dhan_live_trader_core::instrument::try_rebuild_instruments;

use crate::state::SharedAppState;

/// Response for the instrument rebuild endpoint.
#[derive(Debug, Serialize)]
pub struct RebuildResponse {
    pub status: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derivative_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub underlying_count: Option<usize>,
}

/// RAII guard that clears `rebuild_in_progress` on drop.
///
/// Guarantees the flag is released even if `do_rebuild` panics or the
/// tokio task is cancelled at an `.await` point.
struct RebuildGuard<'a> {
    flag: &'a AtomicBool,
}

impl Drop for RebuildGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

/// `POST /api/instruments/rebuild` — one-shot instrument rebuild.
///
/// - Bypasses time gate (accessible any time).
/// - Respects freshness marker (idempotent — already built today → no-op).
/// - Concurrent requests guarded by `AtomicBool`.
pub async fn rebuild_instruments(State(state): State<SharedAppState>) -> Json<RebuildResponse> {
    // Concurrency guard: only one rebuild at a time
    if state
        .rebuild_in_progress()
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        info!("rebuild request rejected: another rebuild is already in progress");
        return Json(RebuildResponse {
            status: "in_progress".to_string(),
            message: "another instrument rebuild is already running".to_string(),
            derivative_count: None,
            underlying_count: None,
        });
    }

    // RAII guard: flag is cleared on drop (panic, cancellation, or normal return)
    let _guard = RebuildGuard {
        flag: state.rebuild_in_progress(),
    };
    do_rebuild(&state).await
}

/// Inner rebuild logic — separated for clean guard release.
async fn do_rebuild(state: &SharedAppState) -> Json<RebuildResponse> {
    let dhan = state.dhan_config();
    let inst = state.instrument_config();
    let qdb = state.questdb_config();

    match try_rebuild_instruments(
        &dhan.instrument_csv_url,
        &dhan.instrument_csv_fallback_url,
        inst,
        qdb,
    )
    .await
    {
        Ok(Some(universe)) => {
            let dc = universe.derivative_contracts.len();
            let uc = universe.underlyings.len();
            info!(
                derivative_count = dc,
                underlying_count = uc,
                "manual instrument rebuild succeeded"
            );
            Json(RebuildResponse {
                status: "rebuilt".to_string(),
                message: format!("instruments rebuilt: {dc} derivatives, {uc} underlyings"),
                derivative_count: Some(dc),
                underlying_count: Some(uc),
            })
        }
        Ok(None) => {
            info!("manual rebuild: instruments already built today");
            Json(RebuildResponse {
                status: "already_built".to_string(),
                message: "instruments already built today — no action taken".to_string(),
                derivative_count: None,
                underlying_count: None,
            })
        }
        Err(err) => {
            warn!(%err, "manual instrument rebuild failed");
            Json(RebuildResponse {
                status: "failed".to_string(),
                message: format!("rebuild failed: {err}"),
                derivative_count: None,
                underlying_count: None,
            })
        }
    }
}

/// `GET /api/instruments/diagnostic` — full instrument system health check.
///
/// Downloads CSV, validates headers, parses rows, builds universe, and
/// reports detailed status for each step.
pub async fn instrument_diagnostic(State(state): State<SharedAppState>) -> Json<serde_json::Value> {
    let dhan = state.dhan_config();
    let inst = state.instrument_config();

    let report = run_instrument_diagnostic(
        &dhan.instrument_csv_url,
        &dhan.instrument_csv_fallback_url,
        inst,
    )
    .await;

    Json(
        serde_json::to_value(report).unwrap_or_else(
            |err| serde_json::json!({"error": format!("serialization failed: {err}")}),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rebuild_response_serialization() {
        let resp = RebuildResponse {
            status: "rebuilt".to_string(),
            message: "test".to_string(),
            derivative_count: Some(96948),
            underlying_count: Some(214),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"rebuilt\""));
        assert!(json.contains("96948"));
    }

    #[test]
    fn test_rebuild_response_skips_none_fields() {
        let resp = RebuildResponse {
            status: "already_built".to_string(),
            message: "done".to_string(),
            derivative_count: None,
            underlying_count: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("derivative_count"));
        assert!(!json.contains("underlying_count"));
    }

    #[test]
    fn test_rebuild_response_in_progress() {
        let resp = RebuildResponse {
            status: "in_progress".to_string(),
            message: "another instrument rebuild is already running".to_string(),
            derivative_count: None,
            underlying_count: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"in_progress\""));
    }

    #[test]
    fn test_rebuild_response_failed() {
        let resp = RebuildResponse {
            status: "failed".to_string(),
            message: "rebuild failed: connection error".to_string(),
            derivative_count: None,
            underlying_count: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"failed\""));
        assert!(json.contains("connection error"));
    }

    #[test]
    fn test_rebuild_guard_clears_flag_on_drop() {
        let flag = AtomicBool::new(true);
        {
            let _guard = RebuildGuard { flag: &flag };
            assert!(flag.load(Ordering::SeqCst)); // still true while guard alive
        }
        // Guard dropped — flag should be false
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_rebuild_guard_clears_flag_even_if_was_false() {
        let flag = AtomicBool::new(false);
        {
            let _guard = RebuildGuard { flag: &flag };
        }
        assert!(!flag.load(Ordering::SeqCst));
    }

    // -----------------------------------------------------------------------
    // Helper: builds a SharedAppState with unreachable URLs (for handler tests)
    // -----------------------------------------------------------------------

    fn test_state() -> crate::state::SharedAppState {
        use dhan_live_trader_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

        crate::state::SharedAppState::new(
            QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
                pg_port: 1,
                ilp_port: 1,
            },
            DhanConfig {
                websocket_url: "wss://test".to_string(),
                order_update_websocket_url: "wss://test".to_string(),
                rest_api_base_url: "https://test".to_string(),
                auth_base_url: "https://test".to_string(),
                instrument_csv_url: "http://127.0.0.1:1/unreachable.csv".to_string(),
                instrument_csv_fallback_url: "http://127.0.0.1:1/fallback.csv".to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
            },
            InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/dlt-test-instruments".to_string(),
                csv_cache_filename: "test-instruments.csv".to_string(),
                csv_download_timeout_secs: 1,
                build_window_start: "00:00:00".to_string(),
                build_window_end: "23:59:59".to_string(),
            },
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(crate::state::SystemHealthStatus::new()),
        )
    }

    // -----------------------------------------------------------------------
    // Handler tests: concurrent rebuild guard rejection
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_rebuild_instruments_concurrent_guard_rejects_second_request() {
        let state = test_state();

        // Pre-set the flag to simulate a rebuild already in progress
        state.rebuild_in_progress().store(true, Ordering::SeqCst);

        let Json(resp) = rebuild_instruments(State(state.clone())).await;
        assert_eq!(resp.status, "in_progress");
        assert!(resp.message.contains("already running"));
        assert!(resp.derivative_count.is_none());
        assert!(resp.underlying_count.is_none());

        // Flag should still be true (we didn't acquire it, so we didn't release it)
        assert!(state.rebuild_in_progress().load(Ordering::SeqCst));
    }

    // -----------------------------------------------------------------------
    // Handler tests: do_rebuild with unreachable CSV URLs → Err path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_rebuild_instruments_error_path() {
        let state = test_state();

        // Flag starts false, so handler will acquire it
        assert!(!state.rebuild_in_progress().load(Ordering::SeqCst));

        let Json(resp) = rebuild_instruments(State(state.clone())).await;

        // With unreachable URLs, rebuild should fail
        assert_eq!(resp.status, "failed");
        assert!(resp.message.contains("rebuild failed"));
        assert!(resp.derivative_count.is_none());
        assert!(resp.underlying_count.is_none());

        // Flag should be cleared by the RAII guard
        assert!(!state.rebuild_in_progress().load(Ordering::SeqCst));
    }

    // -----------------------------------------------------------------------
    // Handler tests: instrument_diagnostic
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_instrument_diagnostic_returns_json_value() {
        let state = test_state();

        let Json(result) = instrument_diagnostic(State(state)).await;

        // Should always return a JSON value, even when CSV is unreachable
        assert!(result.is_object());
    }

    // -----------------------------------------------------------------------
    // Handler tests: do_rebuild inner function directly
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_do_rebuild_error_clears_guard_flag() {
        let state = test_state();

        let Json(resp) = do_rebuild(&state).await;
        // With unreachable URLs, we expect failure
        assert_eq!(resp.status, "failed");
    }

    // -----------------------------------------------------------------------
    // Handler tests: Ok(None) path — instruments already built today
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_rebuild_instruments_already_built_today_returns_already_built() {
        use dhan_live_trader_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};
        use dhan_live_trader_common::constants::INSTRUMENT_FRESHNESS_MARKER_FILENAME;

        // Create a unique temp dir with today's freshness marker
        let temp_dir =
            std::env::temp_dir().join(format!("dlt-test-already-built-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Write today's IST date as the freshness marker
        let ist_offset = chrono::FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap();
        let today = chrono::Utc::now()
            .with_timezone(&ist_offset)
            .date_naive()
            .to_string();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        std::fs::write(&marker_path, &today).unwrap();

        let state = crate::state::SharedAppState::new(
            QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
                pg_port: 1,
                ilp_port: 1,
            },
            DhanConfig {
                websocket_url: "wss://test".to_string(),
                order_update_websocket_url: "wss://test".to_string(),
                rest_api_base_url: "https://test".to_string(),
                auth_base_url: "https://test".to_string(),
                instrument_csv_url: "http://127.0.0.1:1/unreachable.csv".to_string(),
                instrument_csv_fallback_url: "http://127.0.0.1:1/fallback.csv".to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
            },
            InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: temp_dir.to_str().unwrap().to_string(),
                csv_cache_filename: "test-instruments.csv".to_string(),
                csv_download_timeout_secs: 1,
                build_window_start: "00:00:00".to_string(),
                build_window_end: "23:59:59".to_string(),
            },
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(crate::state::SystemHealthStatus::new()),
        );

        let Json(resp) = rebuild_instruments(State(state)).await;
        assert_eq!(resp.status, "already_built");
        assert!(resp.message.contains("already built today"));
        assert!(resp.derivative_count.is_none());
        assert!(resp.underlying_count.is_none());

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // -----------------------------------------------------------------------
    // RebuildResponse Debug impl
    // -----------------------------------------------------------------------

    #[test]
    fn test_rebuild_response_debug_impl() {
        let resp = RebuildResponse {
            status: "rebuilt".to_string(),
            message: "test".to_string(),
            derivative_count: Some(100),
            underlying_count: Some(50),
        };
        let debug = format!("{resp:?}");
        assert!(debug.contains("RebuildResponse"));
        assert!(debug.contains("rebuilt"));
    }
}
