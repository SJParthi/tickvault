//! Integration tests for the MCP read-only debug endpoints
//! (`/api/debug/spill/status`, `/api/debug/logs/jsonl/latest`) and the
//! `/health` per-subsystem detail strings.
//!
//! Coverage top-up (2026-07-02, coverage-gate fix PR): these exercise the
//! spill-directory scanners (per-category + legacy top-level, newest-file
//! tracking), the logs-jsonl 404/200 paths, and the health endpoint's
//! "expires in Ns" / "buffering" detail branches that were previously only
//! reachable with live subsystem state.
//!
//! Env-var discipline: `TV_SPILL_DIR` is touched ONLY by the spill test and
//! `TV_LOGS_DIR` ONLY by the logs test, each pointing at a unique temp dir —
//! so parallel tests inside this binary never race on the same variable.

use axum::extract::State;
use axum::response::IntoResponse;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tickvault_api::handlers::debug::{logs_jsonl_latest, spill_status};
use tickvault_api::handlers::health::health_check;
use tickvault_api::state::{SharedAppState, SystemHealthStatus};
use tickvault_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn unique_temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "tv-debug-test-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir {dir:?}: {e}"));
    dir
}

fn write_file(dir: &Path, name: &str, contents: &[u8]) {
    std::fs::write(dir.join(name), contents).unwrap_or_else(|e| panic!("write {name}: {e}"));
}

async fn body_string(resp: axum::response::Response) -> (axum::http::StatusCode, String) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    (status, String::from_utf8_lossy(&bytes).to_string())
}

fn test_state() -> SharedAppState {
    SharedAppState::new(
        QuestDbConfig {
            host: "localhost".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        },
        DhanConfig {
            websocket_url: "wss://test".to_string(),
            order_update_websocket_url: "wss://test".to_string(),
            rest_api_base_url: "https://test".to_string(),
            auth_base_url: "https://test".to_string(),
            instrument_csv_url: "https://test/csv".to_string(),
            instrument_csv_fallback_url: "https://test/csv2".to_string(),
            max_instruments_per_connection: 100,
            max_websocket_connections: 5,
            sandbox_base_url: String::new(),
        },
        InstrumentConfig {
            daily_download_time: "06:30:00".to_string(),
            csv_cache_directory: "/tmp".to_string(),
            csv_cache_filename: "test.csv".to_string(),
            csv_download_timeout_secs: 30,
            build_window_start: "06:00:00".to_string(),
            build_window_end: "09:15:00".to_string(),
        },
        Arc::new(SystemHealthStatus::new()),
    )
}

// ---------------------------------------------------------------------------
// /api/debug/spill/status — populated spill dir (category + legacy scanners)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spill_status_reports_categories_legacy_and_newest_file() {
    let dir = unique_temp_dir("spill");

    // Category subdir with TWO .bin files (the second written later must win
    // the newest-file race) + one non-.bin file that must be IGNORED.
    let ticks = dir.join("ticks");
    std::fs::create_dir_all(&ticks).expect("mkdir ticks");
    write_file(&ticks, "seg-0001.bin", b"12345"); // 5 bytes
    write_file(&ticks, "ignored.txt", b"not a spill file");
    // Ensure a strictly later mtime for the second .bin so the
    // newest-file tracking branch (`mtime > existing`) is exercised.
    std::thread::sleep(std::time::Duration::from_millis(50));
    write_file(&ticks, "seg-0002.bin", b"1234567890"); // 10 bytes

    // Legacy TOP-LEVEL .bin (pre-per-category layout) + an ignored non-.bin.
    write_file(&dir, "ticks-20260701.bin", b"abc"); // 3 bytes
    write_file(&dir, "README.md", b"ignored");

    // SAFETY: unsafe block required by Rust 2024 edition for std::env::set_var.
    // Only this test touches TV_SPILL_DIR in this test binary.
    unsafe {
        std::env::set_var("TV_SPILL_DIR", &dir);
    }
    let (status, body) = body_string(spill_status().await.into_response()).await;

    assert_eq!(status, axum::http::StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).expect("valid JSON body");
    assert_eq!(json["exists"], serde_json::Value::Bool(true));

    // ticks category: exactly the two .bin files, 15 bytes total, newest =
    // the later-written file. The .txt file must not be counted.
    assert_eq!(json["categories"]["ticks"]["file_count"], 2);
    assert_eq!(json["categories"]["ticks"]["total_bytes"], 15);
    assert_eq!(json["categories"]["ticks"]["newest_file"], "seg-0002.bin");

    // Untouched categories report empty with a null newest_file.
    assert_eq!(json["categories"]["candles"]["file_count"], 0);
    assert_eq!(
        json["categories"]["depth"]["newest_file"],
        serde_json::Value::Null
    );

    // Legacy top-level scanner: one .bin (the README.md is ignored). NOTE the
    // category SUBDIRS are not files, so they never count here either.
    assert_eq!(json["legacy_top_level"]["file_count"], 1);
    assert_eq!(json["legacy_top_level"]["total_bytes"], 3);
    assert_eq!(
        json["legacy_top_level"]["newest_file"],
        "ticks-20260701.bin"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// /api/debug/logs/jsonl/latest — 404 when absent, newest-by-lexical when present
// ---------------------------------------------------------------------------

#[tokio::test]
async fn logs_jsonl_latest_404_then_serves_lexically_newest_file() {
    let dir = unique_temp_dir("logs");
    // SAFETY: unsafe block required by Rust 2024 edition for std::env::set_var.
    // Only this test touches TV_LOGS_DIR in this test binary.
    unsafe {
        std::env::set_var("TV_LOGS_DIR", &dir);
    }

    // Empty dir → 404 with the documented JSON error body.
    let (status, body) = body_string(logs_jsonl_latest().await.into_response()).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert!(
        body.contains("no errors.jsonl file present"),
        "404 body must carry the documented error message; got: {body}"
    );

    // Two hourly-rotated files → the LEXICALLY newest (ISO-8601 suffix) wins.
    write_file(&dir, "errors.jsonl.2026-07-01-09", b"{\"old\":true}\n");
    write_file(&dir, "errors.jsonl.2026-07-02-10", b"{\"new\":true}\n");
    let (status, body) = body_string(logs_jsonl_latest().await.into_response()).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(
        body.contains("\"new\":true"),
        "must serve the newest hourly file; got: {body}"
    );
    assert!(
        !body.contains("\"old\":true"),
        "must NOT serve the older hourly file"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// /health — token "expires in Ns" detail + tick-persistence "buffering" state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_reports_token_expiry_detail_and_buffering_state() {
    let state = test_state();
    let health = state.health_status();

    // Token valid with headroom → status "valid" + "expires in 120s" detail.
    health.set_token_valid(true);
    health.set_token_remaining_secs(120);
    // Persistence DISCONNECTED but ring/spill carrying data → the honest
    // "buffering" state (not "unavailable"), with the buffer/spill counts.
    health.set_tick_persistence_connected(false);
    health.set_tick_buffer_size(5);
    health.set_ticks_spilled(7);

    let axum::Json(resp) = health_check(State(state)).await;

    assert_eq!(resp.subsystems.token.status, "valid");
    assert_eq!(
        resp.subsystems.token.detail.as_deref(),
        Some("expires in 120s"),
        "token detail must carry the remaining seconds"
    );
    assert_eq!(
        resp.subsystems.tick_persistence.status, "buffering",
        "disconnected persistence with a non-empty ring/spill is BUFFERING, not unavailable"
    );
    assert_eq!(
        resp.subsystems.tick_persistence.detail.as_deref(),
        Some("buffer: 5, spilled: 7"),
        "buffering detail must carry the live buffer + spilled counts"
    );
}
