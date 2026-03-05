//! Instrument rebuild endpoint — one-shot manual trigger.
//!
//! `POST /api/instruments/rebuild` — bypasses time gate, respects freshness marker.
//! Concurrent requests guarded by `AtomicBool` (only one rebuild at a time).

use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use tracing::{info, warn};

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
    fn test_rebuild_guard_clears_flag_on_drop() {
        let flag = AtomicBool::new(true);
        {
            let _guard = RebuildGuard { flag: &flag };
            assert!(flag.load(Ordering::SeqCst));
        }
        // After guard is dropped, flag must be false
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_rebuild_guard_clears_flag_when_already_false() {
        let flag = AtomicBool::new(false);
        {
            let _guard = RebuildGuard { flag: &flag };
        }
        assert!(!flag.load(Ordering::SeqCst));
    }
}
