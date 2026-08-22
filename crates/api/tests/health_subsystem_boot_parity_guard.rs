//! Boot-path parity ratchet for `/health` (2026-08-11, observability
//! blind-spot audit round 2).
//!
//! # The rot this stops
//!
//! `/health` reported six subsystems, three of them RETIRED (websocket,
//! pipeline, tick persistence), and listed NOTHING that actually runs: the
//! cadence scheduler, the four per-minute REST legs, the seal writer, the
//! RAM store, the paper order runtime. So it could answer "healthy" while
//! every live subsystem was dead — a lie by omission that no test caught,
//! because nothing tied the endpoint to the boot path.
//!
//! These tests tie them together in BOTH directions:
//!
//! * **omission** — every subsystem with a live spawn site in
//!   `crates/app/src/` MUST appear in `LIVE_RUNTIME_SUBSYSTEMS`. Add a
//!   subsystem to the boot path, forget `/health`, and the build fails.
//! * **rot** — every row in `LIVE_RUNTIME_SUBSYSTEMS` MUST still have its
//!   spawn site. Delete a subsystem (as the 2026-07 sweeps deleted the tick
//!   chain), forget `/health`, and the build fails.
//!
//! The spawn markers below are the ANCHOR. They are deliberately the
//! function-path literals the boot path calls, not loose words: a rename
//! that keeps the subsystem alive fails this test loudly and is fixed by
//! updating one row here, which is exactly the review moment we want.
//!
//! # Honest scope
//!
//! This is a SOURCE-SCAN ratchet, not a runtime probe. It proves the
//! endpoint's subsystem LIST matches the code that boots — it cannot prove
//! any subsystem is actually healthy at runtime. That is stated plainly
//! rather than implied away: per-subsystem liveness lives in the metrics
//! each row names (`RuntimeSubsystem::metric`), and wiring real signals
//! into `/health` needs a `SystemHealthStatus` setter per subsystem.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tickvault_api::handlers::health::{LIVE_RUNTIME_SUBSYSTEMS, STATUS_UNWIRED};

/// The real `/health` body, in the DEFAULT (nothing-reported) state — the
/// largest it ever gets, since every retired/unreported field renders its
/// explanatory detail.
async fn health_body_bytes() -> Vec<u8> {
    use axum::body::Body;
    use axum::http::Request;
    use tickvault_api::build_router;
    use tickvault_api::state::{SharedAppState, SystemHealthStatus};
    use tickvault_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};
    use tower::ServiceExt;

    let state = SharedAppState::new(
        QuestDbConfig {
            host: "localhost".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        },
        DhanConfig {
            websocket_url: "wss://api-feed.dhan.co".to_string(),
            order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
            rest_api_base_url: "https://api.dhan.co/v2".to_string(),
            auth_base_url: "https://auth.dhan.co".to_string(),
            instrument_csv_url: "https://images.dhan.co/x.csv".to_string(),
            instrument_csv_fallback_url: "https://images.dhan.co/y.csv".to_string(),
            max_instruments_per_connection: 5000,
            max_websocket_connections: 5,
            sandbox_base_url: String::new(),
        },
        InstrumentConfig {
            daily_download_time: "08:55:00".to_string(),
            csv_cache_directory: "/tmp/tv-test".to_string(),
            csv_cache_filename: "instruments.csv".to_string(),
            csv_download_timeout_secs: 120,
            build_window_start: "08:25:00".to_string(),
            build_window_end: "08:55:00".to_string(),
        },
        Arc::new(SystemHealthStatus::new()),
    );

    let response = build_router(state, &[], true)
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .expect("build /health request"),
        )
        .await
        .expect("call /health");

    axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read /health body")
        .to_vec()
}

/// `/health` must stay inside the 1 KiB body budget its consumers read
/// with (the api contract test caps `to_bytes` at 1024, and MCP/script
/// pollers do the same). This is a REAL constraint, not a style rule: the
/// first draft of the runtime list blew past it and made `/health`
/// unreadable for every bounded reader — an observability fix that broke
/// observability. Measured against the DEFAULT state, which is the largest
/// (every retired field renders its detail).
#[tokio::test]
async fn health_body_fits_the_one_kib_consumer_budget() {
    let body = health_body_bytes().await;
    assert!(
        body.len() < 1024,
        "/health serialized to {} bytes, over the 1024-byte budget its consumers read with. \
         Shorten the detail strings or the runtime map — do NOT raise consumer limits, since \
         they live in files this endpoint does not own.\nBody: {}",
        body.len(),
        String::from_utf8_lossy(&body)
    );
}

/// The runtime map must survive on the wire through the REAL endpoint, not
/// just in a hand-built struct — otherwise `/health` is as blind as before.
#[tokio::test]
async fn health_endpoint_publishes_every_runtime_subsystem() {
    let body = health_body_bytes().await;
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse /health json");
    let runtime = json
        .pointer("/subsystems/runtime")
        .and_then(serde_json::Value::as_object)
        .expect("/health must expose subsystems.runtime as an object");

    for row in LIVE_RUNTIME_SUBSYSTEMS {
        let metric = runtime
            .get(row.name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("runtime subsystem `{}` missing from /health", row.name));
        assert_eq!(
            metric, row.metric,
            "runtime subsystem `{}` must publish the metric that observes it",
            row.name
        );
    }

    assert_eq!(
        json.pointer("/subsystems/runtime_status")
            .and_then(serde_json::Value::as_str),
        Some(STATUS_UNWIRED),
        "the shared runtime status must reach the wire"
    );
}

/// `(health subsystem name, spawn-site marker in crates/app/src/)`.
///
/// The marker is a literal that appears at the subsystem's boot-path call
/// site. Verified present 2026-08-11.
const BOOT_SPAWN_MARKERS: &[(&str, &str)] = &[
    ("cadence_scheduler", "cadence_boot::spawn_cadence_scheduler"),
    ("dhan_rest_stack", "dhan_rest_stack::spawn_dhan_rest_stack"),
    ("dhan_spot_1m", "tv_spot1m_persist_errors_total"),
    ("dhan_option_chain_1m", "tv_chain1m_persist_errors_total"),
    ("seal_writer", "spawn_seal_writer_loop"),
    (
        "market_ram_store",
        "market_ram_store_boot::install_market_ram_stores",
    ),
    ("order_runtime", "order_runtime::"),
];

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/api
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/api must sit two levels under the repo root")
        .to_path_buf()
}

/// Concatenated source of every `.rs` file under `crates/app/src/`.
fn app_src_concat() -> String {
    let root = repo_root().join("crates/app/src");
    let mut out = String::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read_dir {} failed: {e}", dir.display()));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push_str(
                    &std::fs::read_to_string(&path)
                        .unwrap_or_else(|e| panic!("read {} failed: {e}", path.display())),
                );
                out.push('\n');
            }
        }
    }
    assert!(
        !out.is_empty(),
        "crates/app/src must contain Rust sources — scan found none at {}",
        root.display()
    );
    out
}

/// DIRECTION 1 (rot): every `/health` row must still have a boot-path
/// spawn site. A deleted subsystem must not keep being advertised.
#[test]
fn every_reported_runtime_subsystem_still_boots() {
    let src = app_src_concat();

    for row in LIVE_RUNTIME_SUBSYSTEMS {
        let (_, marker) = BOOT_SPAWN_MARKERS
            .iter()
            .find(|(name, _)| *name == row.name)
            .unwrap_or_else(|| {
                panic!(
                    "/health reports runtime subsystem `{}` but this guard has no boot marker \
                     for it. Add the (name, spawn-site marker) pair to BOOT_SPAWN_MARKERS, or \
                     remove the row from LIVE_RUNTIME_SUBSYSTEMS if the subsystem is gone.",
                    row.name
                )
            });

        assert!(
            src.contains(marker),
            "/health advertises runtime subsystem `{}`, but its boot-path marker `{marker}` no \
             longer appears anywhere in crates/app/src. Either the subsystem was deleted (remove \
             the row from LIVE_RUNTIME_SUBSYSTEMS so /health stops reporting a dead subsystem — \
             the exact rot that left `websocket` and `tick_persistence` in the response for \
             months) or the call site was renamed (update the marker).",
            row.name
        );
    }
}

/// DIRECTION 2 (omission): every live spawn site must be reported. This is
/// the half that catches "/health says healthy while the real runtime is
/// missing from the response entirely".
#[test]
fn every_booting_subsystem_is_reported_by_health() {
    let src = app_src_concat();

    for (name, marker) in BOOT_SPAWN_MARKERS {
        if !src.contains(marker) {
            // Not booting today — direction 1 owns that case.
            continue;
        }
        assert!(
            LIVE_RUNTIME_SUBSYSTEMS.iter().any(|r| r.name == *name),
            "`{name}` boots (marker `{marker}` is live in crates/app/src) but /health does not \
             report it. Add a RuntimeSubsystem row naming the metric that observes it — an \
             endpoint that omits a live subsystem can report `healthy` while that subsystem is \
             dead."
        );
    }
}

/// No fabricated health. Every row must be `unwired` for as long as
/// `SystemHealthStatus` has no setter feeding it — a hardcoded `up`/`true`
/// would be exactly the invented signal this audit removed.
#[test]
fn runtime_rows_never_fabricate_a_health_verdict() {
    for row in LIVE_RUNTIME_SUBSYSTEMS {
        assert_eq!(
            row.status, STATUS_UNWIRED,
            "runtime subsystem `{}` renders `{}` — but /health reads SystemHealthStatus, which \
             has no producer for it, so any verdict other than `{STATUS_UNWIRED}` is fabricated. \
             Wiring a REAL signal means adding a setter + a producer call site first, then \
             rendering from that setter (never a literal).",
            row.name, row.status
        );
        for fabricated in ["up", "healthy", "ok", "active", "connected", "true"] {
            assert_ne!(
                row.status, fabricated,
                "`{}` must never claim `{fabricated}` — nothing measures it",
                row.name
            );
        }
    }
}

/// Each row must name a metric that a producer actually emits — otherwise
/// `/health` sends the operator to a dead series, which is worse than
/// silence (they conclude "no data = fine").
#[test]
fn every_runtime_metric_has_a_live_producer() {
    let root = repo_root();
    let mut sources = String::new();
    for crate_name in ["app", "core", "storage", "trading", "common", "api"] {
        let dir = root.join("crates").join(crate_name).join("src");
        if !dir.exists() {
            continue;
        }
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && let Ok(text) = std::fs::read_to_string(&path)
                {
                    sources.push_str(&text);
                    sources.push('\n');
                }
            }
        }
    }

    for row in LIVE_RUNTIME_SUBSYSTEMS {
        assert!(
            sources.contains(row.metric),
            "/health points the operator at `{}` for subsystem `{}`, but no crate emits that \
             metric — a dead series reads as `no data`, which an operator interprets as healthy.",
            row.metric,
            row.name
        );
    }
}

/// The retired fields stay retired-labelled, never silently deleted: the
/// Dhan live-WS revival is operator-authorized (2026-08-09) and re-arms
/// through them.
#[test]
fn retired_subsystems_are_labelled_not_deleted() {
    let health_rs = repo_root().join("crates/api/src/handlers/health.rs");
    let src = std::fs::read_to_string(&health_rs).expect("read health.rs");

    for marker in ["STATUS_RETIRED", "STATUS_UNREPORTED", "STATUS_UNWIRED"] {
        assert!(
            src.contains(marker),
            "`{marker}` must survive in health.rs — it is how /health distinguishes \
             `does not exist`, `exists but nothing reports it`, and `runs but is not observed \
             here` from a real failure."
        );
    }
}

/// The `runtime` array must actually reach the wire. A list that exists in
/// Rust but never serializes would leave `/health` exactly as blind as
/// before, while looking fixed in review.
#[test]
fn runtime_array_is_present_in_serialized_health_json() {
    use tickvault_api::handlers::health::{SubsystemInfo, SubsystemStatus};

    let info = || SubsystemInfo {
        status: "reachable",
        detail: None,
    };
    let subsystems = SubsystemStatus {
        websocket: info(),
        order_update: info(),
        questdb: info(),
        token: info(),
        pipeline: info(),
        tick_persistence: info(),
    };

    let json = serde_json::to_string(&subsystems).expect("serialize SubsystemStatus");

    assert!(
        json.contains("\"runtime\""),
        "serialized /health must carry the runtime map: {json}"
    );
    assert!(
        json.contains("\"runtime_status\":\"unwired\""),
        "the shared runtime status must be stated on the wire: {json}"
    );
    // `/health` is polled by scripts with small body limits (the api
    // contract test reads at most 1 KiB). Keeping the response inside that
    // budget is part of the contract, not an accident — a payload that
    // silently outgrows it breaks every such reader.
    assert!(
        json.len() < 900,
        "SubsystemStatus serialized to {} bytes; /health must stay inside the 1 KiB read \
         budget its consumers use. Shrink the runtime map (it is name->metric on purpose) \
         rather than raising this.",
        json.len()
    );
    for row in LIVE_RUNTIME_SUBSYSTEMS {
        assert!(
            json.contains(row.name),
            "runtime subsystem `{}` missing from serialized /health: {json}",
            row.name
        );
        assert!(
            json.contains(row.metric),
            "runtime subsystem `{}` must publish the metric that observes it: {json}",
            row.name
        );
    }
}
