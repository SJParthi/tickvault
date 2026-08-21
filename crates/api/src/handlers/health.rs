//! Health check endpoint with subsystem status.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::state::SharedAppState;

/// Health check response with subsystem status.
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
    /// Git commit SHA the running binary was built from (B9 deploy
    /// provenance) — `tickvault_common::build_info::BUILD_GIT_SHA`.
    pub git_sha: &'static str,
    pub subsystems: SubsystemStatus,
}

/// Per-subsystem health status.
///
/// `Serialize` is hand-written (2026-08-11) so the response can carry the
/// `runtime` map from `LIVE_RUNTIME_SUBSYSTEMS` WITHOUT adding a struct
/// field. A field would have forced every existing caller that builds this
/// struct literally to change; the live-runtime list is a compile-time
/// constant with no per-instance state, so a field would have carried no
/// information anyway.
pub struct SubsystemStatus {
    pub websocket: SubsystemInfo,
    pub order_update: SubsystemInfo,
    pub questdb: SubsystemInfo,
    pub token: SubsystemInfo,
    // `valkey: SubsystemInfo` field DELETED in #O4 (2026-05-24).
    pub pipeline: SubsystemInfo,
    pub tick_persistence: SubsystemInfo,
}

impl Serialize for SubsystemStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("SubsystemStatus", 7)?;
        s.serialize_field("websocket", &self.websocket)?;
        s.serialize_field("order_update", &self.order_update)?;
        s.serialize_field("questdb", &self.questdb)?;
        s.serialize_field("token", &self.token)?;
        s.serialize_field("pipeline", &self.pipeline)?;
        s.serialize_field("tick_persistence", &self.tick_persistence)?;
        // The live runtime — always present, never derived from caller state.
        //
        // Serialized COMPACTLY as {name: metric} with one shared status
        // field, not as an array of objects: `/health` is read by scripts
        // with small body limits, and repeating `"status":"unwired"` plus a
        // prose sentence on all nine rows tripled the response for zero
        // extra information. The per-row status is identical by contract
        // (the ratchet enforces it), so it is stated once.
        s.serialize_field("runtime_status", STATUS_UNWIRED)?;
        s.serialize_field("runtime", &RuntimeMetricMap)?;
        s.end()
    }
}

/// One live runtime subsystem and where its real liveness signal lives.
#[derive(Serialize)]
pub struct RuntimeSubsystem {
    /// Stable machine name, matched by the boot-path ratchet.
    pub name: &'static str,
    pub status: &'static str,
    /// The CloudWatch/Prometheus series that DOES carry this subsystem's
    /// liveness, since `/health` cannot measure it in-process.
    pub metric: &'static str,
}

/// Serializes `LIVE_RUNTIME_SUBSYSTEMS` as a compact `{name: metric}` map.
struct RuntimeMetricMap;

impl Serialize for RuntimeMetricMap {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = serializer.serialize_map(Some(LIVE_RUNTIME_SUBSYSTEMS.len()))?;
        for row in LIVE_RUNTIME_SUBSYSTEMS {
            m.serialize_entry(row.name, row.metric)?;
        }
        m.end()
    }
}

/// The subsystems that actually run on today's REST-only runtime, each
/// paired with the metric that genuinely observes it (2026-08-11,
/// observability-blind-spot audit round 2).
///
/// # Why every row is `unwired` and NOT a green boolean
///
/// `SystemHealthStatus` (the only state `/health` can read) exposes no
/// setter for any of these — they are spawned in the boot path and report
/// through `metrics::` counters, never through the API state. Rendering
/// them `up` would be a fabricated health signal for something this
/// handler cannot measure; rendering them `down` would be an equally
/// invented failure. So each row states plainly that `/health` does not
/// observe it and names the series that does.
///
/// Wiring any of these to a real signal is a state-layer change (a new
/// setter on `SystemHealthStatus` plus a producer call at the spawn
/// site); until then this list is the honest answer to "what runs?".
///
/// The boot-path ratchet
/// (`crates/api/tests/health_subsystem_boot_parity_guard.rs`) fails the
/// build in BOTH directions: a row here whose spawn site has been deleted,
/// and a live spawn site with no row here.
pub const LIVE_RUNTIME_SUBSYSTEMS: &[RuntimeSubsystem] = &[
    RuntimeSubsystem {
        name: "cadence_scheduler",
        status: STATUS_UNWIRED,
        metric: "tv_cadence_runner_respawn_total",
    },
    RuntimeSubsystem {
        name: "dhan_rest_stack",
        status: STATUS_UNWIRED,
        metric: "tv_token_remaining_seconds",
    },
    RuntimeSubsystem {
        name: "dhan_spot_1m",
        status: STATUS_UNWIRED,
        metric: "tv_spot1m_persist_errors_total",
    },
    RuntimeSubsystem {
        name: "dhan_option_chain_1m",
        status: STATUS_UNWIRED,
        metric: "tv_chain1m_persist_errors_total",
    },
    RuntimeSubsystem {
        name: "seal_writer",
        status: STATUS_UNWIRED,
        metric: "tv_seal_writer_drain_total",
    },
    RuntimeSubsystem {
        name: "market_ram_store",
        status: STATUS_UNWIRED,
        metric: "tv_ram_store_errors_total",
    },
    RuntimeSubsystem {
        name: "order_runtime",
        status: STATUS_UNWIRED,
        metric: "tv_orders_rejected_total",
    },
];

/// Individual subsystem status info.
#[derive(Serialize)]
pub struct SubsystemInfo {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Status rendered for a subsystem that no longer exists in the running
/// binary (2026-08-09). Distinct from a failure status on purpose: a retired
/// subsystem cannot be "down", and reporting it red trains operators to
/// ignore `/health` entirely.
const STATUS_RETIRED: &str = "retired";

/// Status rendered for a subsystem that DOES exist but has no producer
/// pushing its state into `/health` yet (2026-08-09). An honest "we don't
/// know" — never a fabricated "disconnected".
const STATUS_UNREPORTED: &str = "unreported";

/// Status for a subsystem that PROVABLY runs (it has a boot-path spawn
/// site) but pushes no liveness signal into `/health` (2026-08-11). The
/// honest "we don't measure this here" — never a fabricated `up`, never an
/// invented `down`. Each row names the metric that DOES observe it.
pub const STATUS_UNWIRED: &str = "unwired";

/// GET /health — returns 200 OK with subsystem status.
///
/// # Honest reporting contract (2026-08-09, observability-blind-spot audit)
///
/// Four subsystem fields here had NO production writer, so they rendered
/// their `new()` defaults as hard failures — `/health` claimed the websocket
/// was "disconnected" and tick persistence "unavailable" on a perfectly
/// healthy box, and `overall_status()` returned "degraded" on every request
/// forever. Each field is now rendered from "has a producer ever reported
/// this?" rather than from an unwritten default:
///
/// | field              | today            | why                                          |
/// |--------------------|------------------|----------------------------------------------|
/// | `websocket`        | `retired`        | live feeds retired 2026-07-13 / 2026-07-15   |
/// | `pipeline`         | `retired`        | `spawn_trading_pipeline` has no call site    |
/// | `tick_persistence` | `retired`        | `tick_persistence.rs` deleted 2026-07-17     |
/// | `order_update`     | live             | wired 2026-08-10 — see below                 |
///
/// The `order_update` row WAS the one genuine wiring gap: `dhan_rest_stack`
/// spawned the paper-mode receive-only order-update connection when
/// `[dhan_order_push] enabled = true` but never called
/// `set_order_update_connected`, so the only live Dhan socket rendered as
/// `unreported` — neither up nor down. **Wired 2026-08-10**: the connection's
/// ws lifecycle audit stream (which carries real Connected / Disconnected /
/// Sleep transitions) is teed into this setter, so the row now tracks the
/// socket for real. It still reads `unreported` until the first event, which
/// is correct — with `[dhan_order_push] enabled = false` (the default) no
/// producer exists, and inventing a status for an unspawned subsystem is the
/// false-OK this whole table was written to avoid.
///
/// All four are ARM-ON-ARRIVAL: the first setter call from any producer
/// flips the field back to live reporting with no change needed here.
/// They are KEPT (not deleted) precisely because of that: the Dhan live
/// main-feed WS revival is operator-authorized (2026-08-09), and deleting
/// the field would silently drop its re-arm path.
///
/// # The omission this round fixes (2026-08-11)
///
/// The six fields above are the ONLY things `/health` reported — and three
/// of them are retired. Nothing that actually runs today appeared at all:
/// the cadence scheduler, the four per-minute REST legs, the seal writer,
/// the RAM store and the paper order runtime. `/health` could therefore
/// answer "healthy" while every live subsystem was dead — a lie by
/// omission, and the exact "answer 'is everything working?' with no human
/// digging" failure the operator named.
///
/// The `runtime` array now lists all nine, each as `unwired` with the
/// metric that genuinely observes it. `unwired` is deliberate and is NOT
/// an improvement waiting to be "finished into green": this handler reads
/// `SystemHealthStatus`, which has no setter for any of them, so any
/// boolean here would be invented. Naming the real series is the honest,
/// actionable answer; a `true` would be a fabrication.
pub async fn health_check(State(state): State<SharedAppState>) -> Json<HealthResponse> {
    let health = state.health_status();

    let ws_count = health.websocket_connections();
    let websocket = if health.websocket_reported() {
        SubsystemInfo {
            status: if ws_count > 0 {
                "connected"
            } else {
                "disconnected"
            },
            detail: Some(format!("{ws_count} connections")),
        }
    } else {
        SubsystemInfo {
            status: STATUS_RETIRED,
            // Details are deliberately terse: `/health` is polled by
            // scripts that read a bounded body (the api contract test caps
            // at 1 KiB), and the full explanation lives in this handler's
            // doc comment where it costs no bytes on the wire.
            detail: Some("live feeds retired 2026-07-13/15".to_string()),
        }
    };

    let order_update = if health.order_update_reported() {
        SubsystemInfo {
            status: if health.order_update_connected() {
                "connected"
            } else {
                "disconnected"
            },
            detail: None,
        }
    } else {
        SubsystemInfo {
            status: STATUS_UNREPORTED,
            detail: Some("paper channel exists; no reporter".to_string()),
        }
    };

    let questdb = SubsystemInfo {
        status: if health.questdb_reachable() {
            "reachable"
        } else {
            "unreachable"
        },
        detail: None,
    };

    let remaining = health.token_remaining_secs();
    let token = SubsystemInfo {
        status: if health.token_valid() {
            "valid"
        } else {
            "invalid"
        },
        detail: if remaining > 0 {
            Some(format!("expires in {remaining}s"))
        } else {
            None
        },
    };

    let pipeline = if health.pipeline_reported() {
        SubsystemInfo {
            status: if health.pipeline_active() {
                "active"
            } else {
                "inactive"
            },
            detail: None,
        }
    } else {
        SubsystemInfo {
            status: STATUS_RETIRED,
            detail: Some("no call site since feeds retired".to_string()),
        }
    };

    let tick_buf = health.tick_buffer_size();
    let tick_spill = health.ticks_spilled();
    // A buffering/spilling signal still counts as a real report even if the
    // connected flag was never set — keep the original precedence in that case.
    let tick_persistence = if health.tick_persistence_reported() || tick_buf > 0 || tick_spill > 0 {
        SubsystemInfo {
            status: if health.tick_persistence_connected() {
                "connected"
            } else if tick_buf > 0 || tick_spill > 0 {
                "buffering"
            } else {
                "unavailable"
            },
            detail: if tick_buf > 0 || tick_spill > 0 {
                Some(format!("buffer: {tick_buf}, spilled: {tick_spill}"))
            } else {
                None
            },
        }
    } else {
        SubsystemInfo {
            status: STATUS_RETIRED,
            detail: Some("tick writer deleted 2026-07-17".to_string()),
        }
    };

    let overall = health.overall_status();

    Json(HealthResponse {
        status: overall,
        version: env!("CARGO_PKG_VERSION"),
        git_sha: tickvault_common::build_info::BUILD_GIT_SHA,
        subsystems: SubsystemStatus {
            websocket,
            order_update,
            questdb,
            token,
            pipeline,
            tick_persistence,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{SharedHealthStatus, SystemHealthStatus};
    use std::sync::Arc;

    use tickvault_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

    fn make_test_state(health: SharedHealthStatus) -> SharedAppState {
        SharedAppState::new(
            QuestDbConfig {
                host: "test".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            DhanConfig {
                websocket_url: "wss://test".to_string(),
                order_update_websocket_url: "wss://test".to_string(),
                rest_api_base_url: "https://test".to_string(),
                auth_base_url: "https://test".to_string(),
                instrument_csv_url: "https://test".to_string(),
                instrument_csv_fallback_url: "https://test".to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
                sandbox_base_url: String::new(),
            },
            InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp".to_string(),
                csv_cache_filename: "test.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            health,
        )
    }

    #[tokio::test]
    async fn test_health_check_returns_subsystem_status() {
        let health = Arc::new(SystemHealthStatus::new());
        health.set_websocket_connections(3);
        health.set_questdb_reachable(true);
        health.set_token_valid(true);
        health.set_pipeline_active(true);
        health.set_tick_persistence_connected(true);

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.status, "healthy");
        assert!(!response.version.is_empty());
        assert_eq!(response.subsystems.websocket.status, "connected");
        assert_eq!(response.subsystems.questdb.status, "reachable");
        assert_eq!(response.subsystems.token.status, "valid");
        assert_eq!(response.subsystems.pipeline.status, "active");
        assert_eq!(response.subsystems.tick_persistence.status, "connected");
    }

    #[tokio::test]
    async fn test_health_check_degraded_when_ws_disconnected() {
        let health = Arc::new(SystemHealthStatus::new());
        health.set_questdb_reachable(true);
        health.set_token_valid(true);
        // 2026-08-09: a producer REPORTING zero connections is the real
        // "websocket is down" case, and must still degrade. (Before this
        // change the test relied on the unwritten default, which is now
        // rendered `retired` instead — see the handler doc comment.)
        health.set_websocket_connections(0);

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.status, "degraded");
        assert_eq!(response.subsystems.websocket.status, "disconnected");
    }

    #[tokio::test]
    async fn test_health_check_healthy() {
        let health = Arc::new(SystemHealthStatus::new());
        health.set_websocket_connections(5);
        health.set_questdb_reachable(true);
        health.set_token_valid(true);
        health.set_pipeline_active(true);

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.status, "healthy");
    }

    #[tokio::test]
    async fn test_health_check_degraded() {
        let health = Arc::new(SystemHealthStatus::new());
        // Nothing reported at all (the `new()` defaults).

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        // token_valid has a REAL production writer (dhan_rest_stack.rs), so
        // an unset token is a genuine degrade — unchanged.
        assert_eq!(response.status, "degraded");
        assert_eq!(response.subsystems.questdb.status, "unreachable");
        assert_eq!(response.subsystems.token.status, "invalid");
        // 2026-08-09: the three writer-less subsystems no longer masquerade
        // as failures. `disconnected` / `inactive` / `unavailable` would each
        // assert a fact nobody measured.
        assert_eq!(response.subsystems.websocket.status, STATUS_RETIRED);
        assert_eq!(response.subsystems.pipeline.status, STATUS_RETIRED);
        assert_eq!(response.subsystems.tick_persistence.status, STATUS_RETIRED);
        // The order-update channel DOES exist (paper mode) — honest "unknown".
        assert_eq!(response.subsystems.order_update.status, STATUS_UNREPORTED);
    }

    #[test]
    fn test_health_response_serialization() {
        let resp = HealthResponse {
            status: "healthy",
            version: "0.1.0",
            git_sha: "0123456789abcdef0123456789abcdef01234567",
            subsystems: SubsystemStatus {
                websocket: SubsystemInfo {
                    status: "connected",
                    detail: Some("3 connections".to_string()),
                },
                order_update: SubsystemInfo {
                    status: "connected",
                    detail: None,
                },
                questdb: SubsystemInfo {
                    status: "reachable",
                    detail: None,
                },
                token: SubsystemInfo {
                    status: "valid",
                    detail: None,
                },
                pipeline: SubsystemInfo {
                    status: "active",
                    detail: None,
                },
                tick_persistence: SubsystemInfo {
                    status: "connected",
                    detail: None,
                },
            },
        };
        let json = serde_json::to_string(&resp).expect("serialization should succeed");
        assert!(json.contains("\"status\":\"healthy\""));
        assert!(json.contains("\"version\":\"0.1.0\""));
        assert!(json.contains("\"git_sha\":\"0123456789abcdef0123456789abcdef01234567\""));
        assert!(json.contains("\"websocket\""));
        assert!(json.contains("\"questdb\""));
        assert!(json.contains("\"tick_persistence\""));
    }

    // -------------------------------------------------------------------
    // SubsystemInfo: skip_serializing_if for detail: None
    // -------------------------------------------------------------------

    #[test]
    fn test_subsystem_info_detail_none_omitted_from_json() {
        let info = SubsystemInfo {
            status: "reachable",
            detail: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(
            !json.contains("detail"),
            "detail: None should be omitted via skip_serializing_if"
        );
    }

    #[test]
    fn test_subsystem_info_detail_some_included_in_json() {
        let info = SubsystemInfo {
            status: "connected",
            detail: Some("5 connections".to_string()),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"detail\":\"5 connections\""));
    }

    // -------------------------------------------------------------------
    // HealthResponse: version comes from CARGO_PKG_VERSION
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_health_check_version_matches_cargo_pkg_version() {
        let health = Arc::new(SystemHealthStatus::new());
        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(
            response.version,
            env!("CARGO_PKG_VERSION"),
            "version must match CARGO_PKG_VERSION"
        );
    }

    // -------------------------------------------------------------------
    // HealthResponse: git_sha comes from build_info (B9 deploy provenance)
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_health_check_git_sha_matches_build_info() {
        let health = Arc::new(SystemHealthStatus::new());
        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(
            response.git_sha,
            tickvault_common::build_info::BUILD_GIT_SHA,
            "git_sha must match the compile-time embedded BUILD_GIT_SHA"
        );
        assert!(!response.git_sha.is_empty(), "git_sha must never be empty");

        // The serialized JSON must carry the field under the exact name
        // the operator/tooling greps for.
        let json = serde_json::to_string(&response).expect("serialize HealthResponse");
        assert!(
            json.contains("\"git_sha\""),
            "serialized /health must contain the git_sha field: {json}"
        );
    }

    // -------------------------------------------------------------------
    // HealthResponse: all subsystems down
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_health_check_all_down_subsystem_details() {
        let health = Arc::new(SystemHealthStatus::new());
        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        // 2026-08-09: an unreported websocket explains WHY there is no count
        // instead of printing a made-up "0 connections".
        let ws_detail = response
            .subsystems
            .websocket
            .detail
            .expect("retired websocket must explain itself");
        assert!(
            ws_detail.contains("retired"),
            "websocket detail must name the retirement, got: {ws_detail}"
        );
        // Subsystems with real writers still carry no detail when idle.
        assert!(response.subsystems.questdb.detail.is_none());
        assert!(response.subsystems.token.detail.is_none());
        // The writer-less ones now carry an explanation rather than silence.
        assert!(response.subsystems.pipeline.detail.is_some());
        assert!(response.subsystems.tick_persistence.detail.is_some());
    }

    // -------------------------------------------------------------------
    // 2026-08-09 — observability-blind-spot audit: /health must not invent
    // failure states for subsystems nobody reports, and must not pin the
    // overall verdict to a permanent "degraded".
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_health_overall_is_healthy_on_rest_only_runtime() {
        // THE HEADLINE REGRESSION. Before the fix this returned "degraded"
        // forever, because `overall_status()` degraded on a websocket count
        // that no production code has written since 2026-07-13.
        let health = Arc::new(SystemHealthStatus::new());
        health.set_token_valid(true);
        health.set_questdb_reachable(true);
        // No websocket producer — exactly the live REST-only runtime.

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(
            response.status, "healthy",
            "a REST-only runtime with a valid token and a reachable QuestDB is \
             HEALTHY; reporting it degraded forever destroys the endpoint's signal"
        );
    }

    #[tokio::test]
    async fn test_health_retired_subsystems_are_not_reported_as_failures() {
        let health = Arc::new(SystemHealthStatus::new());
        health.set_token_valid(true);
        health.set_questdb_reachable(true);

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        for (name, status) in [
            ("websocket", response.subsystems.websocket.status),
            ("pipeline", response.subsystems.pipeline.status),
            (
                "tick_persistence",
                response.subsystems.tick_persistence.status,
            ),
        ] {
            assert_eq!(
                status, STATUS_RETIRED,
                "{name} has no production writer and must render `retired`"
            );
            for failure_word in ["disconnected", "inactive", "unavailable", "unreachable"] {
                assert_ne!(
                    status, failure_word,
                    "{name} must never claim `{failure_word}` — nothing measured it"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_health_order_update_is_unreported_not_disconnected() {
        // The order-update channel is LIVE (paper mode) but unwired, so the
        // honest answer is "we don't know", NOT "disconnected".
        let health = Arc::new(SystemHealthStatus::new());
        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.subsystems.order_update.status, STATUS_UNREPORTED);
        assert_ne!(response.subsystems.order_update.status, "disconnected");
        assert!(
            response.subsystems.order_update.detail.is_some(),
            "an unreported live subsystem must say why it is unreported"
        );
    }

    #[tokio::test]
    async fn test_health_websocket_arms_on_arrival_when_a_producer_reports() {
        // ARM-ON-ARRIVAL: when the authorized Dhan live main-feed WS revival
        // lands and its watchdog pushes a count, both the rendering and the
        // overall verdict must re-arm with no further edit to this handler.
        let health = Arc::new(SystemHealthStatus::new());
        health.set_token_valid(true);
        health.set_questdb_reachable(true);
        health.set_websocket_connections(1);

        let state = make_test_state(health.clone());
        let Json(response) = health_check(State(state)).await;
        assert_eq!(response.subsystems.websocket.status, "connected");
        assert_eq!(
            response.subsystems.websocket.detail,
            Some("1 connections".to_string())
        );
        assert_eq!(response.status, "healthy");

        // ...and a subsequent drop to zero degrades exactly as before.
        health.set_websocket_connections(0);
        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;
        assert_eq!(response.subsystems.websocket.status, "disconnected");
        assert_eq!(
            response.status, "degraded",
            "once a producer reports the websocket, a drop to 0 must degrade"
        );
    }

    #[tokio::test]
    async fn test_health_tick_persistence_buffering_still_wins_over_retired() {
        // A non-empty ring/spill is itself evidence of a live writer, so the
        // `buffering` state must survive the retired-rendering change.
        let health = Arc::new(SystemHealthStatus::new());
        health.set_tick_buffer_size(3);
        health.set_ticks_spilled(4);

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.subsystems.tick_persistence.status, "buffering");
        assert_eq!(
            response.subsystems.tick_persistence.detail,
            Some("buffer: 3, spilled: 4".to_string())
        );
    }

    // -------------------------------------------------------------------
    // HealthResponse: websocket connection count in detail field
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_health_check_websocket_detail_shows_count() {
        let health = Arc::new(SystemHealthStatus::new());
        health.set_websocket_connections(5);
        health.set_token_valid(true);
        health.set_questdb_reachable(true);
        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(
            response.subsystems.websocket.detail,
            Some("5 connections".to_string())
        );
    }

    // -------------------------------------------------------------------
    // HealthResponse: tick_persistence status
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_health_check_tick_persistence_connected() {
        let health = Arc::new(SystemHealthStatus::new());
        health.set_tick_persistence_connected(true);

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.subsystems.tick_persistence.status, "connected");
    }

    #[tokio::test]
    async fn test_health_check_order_update_connected() {
        let health = Arc::new(SystemHealthStatus::new());
        health.set_order_update_connected(true);
        health.set_token_valid(true);
        health.set_websocket_connections(5);
        health.set_questdb_reachable(true);
        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.subsystems.order_update.status, "connected");
    }

    // 2026-08-09 (observability-blind-spot audit): these two previously
    // asserted that an UNWRITTEN default renders as a failure state
    // ("disconnected" / "unavailable"). That is precisely the lie the audit
    // removed — nothing had measured either subsystem. They now assert the
    // honest rendering, and the "real producer reported a failure" cases are
    // covered by `test_health_check_order_update_connected` /
    // `debug_spill_and_health_detail::health_reports_token_expiry_detail_and_buffering_state`
    // plus the explicit reported-false test below.
    #[tokio::test]
    async fn test_health_check_order_update_unreported_by_default() {
        let health = Arc::new(SystemHealthStatus::new());
        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.subsystems.order_update.status, STATUS_UNREPORTED);
    }

    #[tokio::test]
    async fn test_health_check_order_update_disconnected_when_producer_reports_false() {
        // A REAL producer reporting "not connected" must still render
        // "disconnected" — the retired/unreported rendering must never mask
        // a genuine down signal.
        let health = Arc::new(SystemHealthStatus::new());
        health.set_order_update_connected(false);
        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.subsystems.order_update.status, "disconnected");
    }

    #[tokio::test]
    async fn test_health_check_tick_persistence_retired_by_default() {
        let health = Arc::new(SystemHealthStatus::new());

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.subsystems.tick_persistence.status, STATUS_RETIRED);
    }

    #[tokio::test]
    async fn test_health_check_tick_persistence_unavailable_when_producer_reports_false() {
        // Producer present, connection down, ring/spill empty → "unavailable"
        // remains the correct answer.
        let health = Arc::new(SystemHealthStatus::new());
        health.set_tick_persistence_connected(false);

        let state = make_test_state(health);
        let Json(response) = health_check(State(state)).await;

        assert_eq!(response.subsystems.tick_persistence.status, "unavailable");
    }
}
