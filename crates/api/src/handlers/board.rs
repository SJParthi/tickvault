//! Live Board JSON snapshot — `GET /api/board/data`.
//!
//! The single aggregation endpoint the `/board` operator webpage polls every
//! ~3 seconds. Cold-path only, strictly read-only, best-effort with HONEST
//! nulls: any source that cannot be read (QuestDB down, no groww status
//! files, non-Linux host) degrades to `null` / an empty list — a number is
//! NEVER fabricated.
//!
//! Public-route posture mirrors the existing `GET /api/feeds/health` read
//! (operator 2026-06-23 "public read, authed toggle"): the payload carries
//! only numbers, fixed labels, and short sanitized ascii tags. The raw error
//! log surface is deliberately NOT duplicated here — the `/board` page
//! client-fetches the EXISTING bearer-gated `/api/debug/logs/jsonl/latest`
//! (2026-07-04 security trim respected).
//!
//! Sources aggregated:
//! - status: boot epoch (`SystemHealthStatus::boot_epoch_secs`), build sha
//!   (`tickvault_common::build_info`), trading-session flag, `/proc` RSS.
//! - feeds: the existing [`super::feeds::get_feeds_health`] response, reused
//!   verbatim (no forked verdict logic).
//! - db: sealed 1m candles via QuestDB HTTP `/exec` with a
//!   bounded timeout (reuses `super::stats::query_count`).
//!
//! (The `connections` + `race` sections were removed 2026-07-17 — dashboard
//! tidy: their producers, the Groww sidecar `groww-status.json` files and
//! the shadow-parity `parity-<date>.tsv`, were deleted with the live feeds
//! (Groww 2026-07-15; the §35 shadow client with it), so both fields could
//! only ever serve permanent empty/null values.)

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::state::SharedAppState;

/// QuestDB count queries are cold-path; keep each request bounded.
const QUESTDB_BOARD_TIMEOUT_SECS: u64 = 3;

/// "System status" strip values.
#[derive(Debug, Serialize)]
pub struct BoardStatus {
    /// Seconds since boot; `null` until the boot epoch is stamped.
    pub uptime_secs: Option<u64>,
    /// Short (7-char) build git sha from `build_info`.
    pub build_sha_short: &'static str,
    /// Trading session open right now (weekend + NSE-holiday aware).
    pub market_open: bool,
    /// Process resident memory in bytes from `/proc/self/status`; `null` on
    /// hosts without procfs (macOS dev). CPU% is deliberately OMITTED — no
    /// cheap dependency-free source exists, and we never fake a number.
    pub mem_rss_bytes: Option<u64>,
}

/// QuestDB persistence counters for the "vault" panel.
#[derive(Debug, Serialize)]
pub struct BoardDb {
    /// True when QuestDB answered the probe this poll.
    pub reachable: bool,
    /// 1m candles sealed since IST midnight; `null` on query failure.
    pub candles_1m_today: Option<u64>,
    // `ticks_today` tile RETIRED 2026-07-19 (BATCH-5): the `ticks` writer was
    // deleted 2026-07-17 (dead live-WS sweep), so `ticks` receives no new
    // rows — the count would be a permanently-stale/zero "live ticks captured
    // today" number, i.e. a false-OK. Retired rather than repointed at another
    // table (repointing a "ticks" label at candles would be a different lie).
}

/// The full `GET /api/board/data` payload.
#[derive(Debug, Serialize)]
pub struct BoardDataResponse {
    pub status: BoardStatus,
    /// The existing `/api/feeds/health` payload, embedded verbatim.
    pub feeds: super::feeds::FeedsHealthResponse,
    pub db: BoardDb,
}

/// `GET /api/board/data` — the Live Board snapshot. Best-effort everywhere:
/// every failing source degrades to `null`/empty, the endpoint always
/// answers 200 with whatever is truthfully known.
///
/// 2026-07-09 follow-up (closes the #1458 HIGH residual): the computed 200
/// body is TTL-cached (2s, single slot in [`SharedAppState`]) and the route
/// rides the shared `crate::public_guard` rate limiter (route_layer, before
/// this handler). A cache HIT performs zero QuestDB round-trips and zero
/// file I/O; the always-200 honest-nulls contract is unchanged (a DB-down
/// null body is deliberately cached too). The /board page polls every
/// ~3s > the 2s TTL, so a single legit tab still sees fresh data on every
/// poll.
///
/// HONEST BOUND (adversarial review 2026-07-09): there is no single-flight
/// on a miss, and with QuestDB BLACK-HOLED a compute can take ~3s (> the 2s
/// TTL) — so in that regime concurrent misses each run their own 3-probe
/// pass. The true attack bound is the LIMITER: ≤5 admitted requests/s →
/// ≤5 concurrent computes/s (unbounded before this change). With QuestDB
/// UP a compute is ~ms, so the "one pass per TTL window" behavior holds in
/// the healthy regime only. Miss single-flighting = same flagged follow-up
/// as the stats handler.
// TEST-EXEMPT: covered by #[tokio::test] test_board_data_cache_hit_skips_recompute (the pub-fn-test-guard greps #[test] files only, not #[tokio::test]).
pub async fn board_data(State(state): State<SharedAppState>) -> axum::response::Response {
    use axum::response::IntoResponse;

    if let Some(body) = state.board_cache().get() {
        metrics::counter!("tv_api_cache_hits_total", "endpoint" => "board").increment(1);
        return crate::response_cache::cached_json_response(body, "hit");
    }

    let snapshot = compute_board_data(&state).await;
    match serde_json::to_string(&snapshot) {
        Ok(body) => {
            state.board_cache().put(body.clone());
            crate::response_cache::cached_json_response(body, "miss")
        }
        // Practically unreachable (plain structs of numbers/strings) — fall
        // through to the uncached Json path rather than 500 on a cache
        // problem; never cache a body we could not serialize.
        Err(_) => Json(snapshot).into_response(),
    }
}

/// Runs the full board aggregation (feeds + files + 3 QuestDB queries).
/// Extracted from the handler so the cache wrapper stays thin and the
/// existing null-contract test exercises the computation directly.
// TEST-EXEMPT: covered by #[tokio::test] test_compute_board_data_all_sources_down_returns_nulls_not_panics (the pub-fn-test-guard greps #[test] files only, not #[tokio::test]).
pub(crate) async fn compute_board_data(state: &SharedAppState) -> BoardDataResponse {
    let state = state.clone();
    // Feeds: reuse the existing health aggregation verbatim.
    let feeds = super::feeds::get_feeds_health(State(state.clone())).await.0;
    let market_open = feeds.market_open;

    // Uptime from the boot-epoch stamp (0 = not stamped yet → null).
    let boot = state.health_status().boot_epoch_secs();
    let now_secs = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
    let uptime_secs = (boot > 0 && now_secs >= boot).then(|| now_secs - boot);

    // QuestDB counts — three bounded queries, concurrently (max one timeout).
    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
    let client = super::stats::build_stats_client(QUESTDB_BOARD_TIMEOUT_SECS);
    let midnight = ist_today_midnight_literal();
    // `ticks_today` tile retired 2026-07-19 (BATCH-5) — the ticks writer is
    // gone, so no ticks query is issued anymore.
    let candles_sql = format!("SELECT count() FROM candles_1m WHERE ts >= '{midnight}'");
    let (probe, candles_1m_today) = tokio::join!(
        super::stats::query_count(&client, &base_url, "SHOW TABLES"),
        super::stats::query_count(&client, &base_url, &candles_sql),
    );

    BoardDataResponse {
        status: BoardStatus {
            uptime_secs,
            build_sha_short: tickvault_common::build_info::build_git_sha_short(),
            market_open,
            mem_rss_bytes: read_proc_rss_bytes(),
        },
        feeds,
        db: BoardDb {
            reachable: probe.is_some(),
            candles_1m_today,
        },
    }
}

/// Resident memory of this process in bytes; `None` on hosts without procfs.
fn read_proc_rss_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    parse_vmrss_kib(&text).map(|kib| kib.saturating_mul(1024))
}

/// Extracts the `VmRSS:  NNNN kB` value (kibibytes) from `/proc/self/status`.
fn parse_vmrss_kib(status_text: &str) -> Option<u64> {
    status_text.lines().find_map(|line| {
        let rest = line.strip_prefix("VmRSS:")?;
        rest.split_whitespace().next()?.parse::<u64>().ok()
    })
}

/// Today's IST midnight as a QuestDB timestamp literal. The `ts` columns
/// store IST wall-clock instants, so comparing against the IST calendar-day
/// midnight literal gives exact "today (IST)" counts.
fn ist_today_midnight_literal() -> String {
    let ist_now = chrono::Utc::now()
        + chrono::Duration::nanoseconds(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
    format!("{}T00:00:00.000000Z", ist_now.format("%Y-%m-%d"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed_state::FeedRuntimeState;
    use std::sync::Arc;
    use tickvault_common::config::{DhanConfig, FeedsConfig, InstrumentConfig, QuestDbConfig};

    fn test_state() -> SharedAppState {
        let health = Arc::new(crate::state::SystemHealthStatus::new());
        SharedAppState::new_with_feed_runtime(
            QuestDbConfig {
                host: "127.0.0.1".to_string(),
                // A port nothing listens on → connection refused fast, so the
                // "QuestDB down" path is exercised without a real server.
                http_port: 1,
                pg_port: 8812,
                ilp_port: 9009,
            },
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
            },
            InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/tv-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            health,
            Arc::new(FeedRuntimeState::from_config(&FeedsConfig::default())),
        )
    }

    /// Like [`test_state`] but pointing QuestDB at a specific mock port.
    fn test_state_with_port(http_port: u16) -> SharedAppState {
        let health = Arc::new(crate::state::SystemHealthStatus::new());
        SharedAppState::new_with_feed_runtime(
            QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port,
                pg_port: 8812,
                ilp_port: 9009,
            },
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
            },
            InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/tv-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            health,
            Arc::new(FeedRuntimeState::from_config(&FeedsConfig::default())),
        )
    }

    /// Multi-request mock QuestDB serving one canned response per connection,
    /// then EXHAUSTING (further connections are refused) — the same
    /// mock-exhaustion proof pattern as the stats/quote cache tests (#1458).
    async fn start_multi_mock_server(responses: Vec<&'static str>) -> u16 {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let port = listener.local_addr().expect("local_addr").port();

        tokio::spawn(async move {
            for body in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        port
    }

    /// 2026-07-09 follow-up: a second `board_data` call inside the 2s TTL
    /// must be a cache HIT — byte-identical body with ZERO QuestDB
    /// round-trips and zero recompute. The mock serves EXACTLY the 3
    /// responses of the first pass; an uncached second pass would hit the
    /// exhausted server and produce the differing reachable=false/null body.
    #[tokio::test]
    async fn test_board_data_cache_hit_skips_recompute() {
        let port = start_multi_mock_server(vec![
            r#"{"dataset":[["ticks"],["candles_1m"]]}"#,
            r#"{"dataset":[[123456]]}"#,
            r#"{"dataset":[[7890]]}"#,
        ])
        .await;
        let state = test_state_with_port(port);

        let first = board_data(State(state.clone())).await;
        assert_eq!(first.status(), axum::http::StatusCode::OK);
        assert_eq!(
            first
                .headers()
                .get(crate::response_cache::CACHE_MARKER_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("miss"),
            "first call must be a cache miss"
        );
        let first_body = axum::body::to_bytes(first.into_body(), 256 * 1024)
            .await
            .expect("first body readable");
        assert!(
            String::from_utf8_lossy(&first_body).contains("\"reachable\":true"),
            "first (computed) body must reflect the mocked reachable QuestDB"
        );

        // Mock exhausted — only the cache can reproduce this body.
        let second = board_data(State(state)).await;
        assert_eq!(second.status(), axum::http::StatusCode::OK);
        assert_eq!(
            second
                .headers()
                .get(crate::response_cache::CACHE_MARKER_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("hit"),
            "second call inside the TTL must be a cache hit"
        );
        let second_body = axum::body::to_bytes(second.into_body(), 256 * 1024)
            .await
            .expect("second body readable");
        assert_eq!(
            first_body, second_body,
            "cache hit must return the byte-identical body without recompute"
        );
    }

    #[tokio::test]
    async fn test_compute_board_data_all_sources_down_returns_nulls_not_panics() {
        // Everything down/absent: QuestDB unreachable, no groww files (test
        // CWD has no data/groww), boot epoch never stamped, market state
        // whatever the calendar says. The endpoint must answer with honest
        // nulls — never panic, never invent a number.
        let resp = compute_board_data(&test_state()).await;
        assert!(resp.status.uptime_secs.is_none(), "boot epoch 0 → null");
        assert!(!resp.status.build_sha_short.is_empty());
        assert!(!resp.db.reachable, "QuestDB down → reachable=false");
        assert!(resp.db.candles_1m_today.is_none());
        // Feeds section still present (one row per feed, from the registry).
        assert_eq!(
            resp.feeds.feeds.len(),
            crate::feed_state::Feed::ALL.len(),
            "feeds section embedded even when everything else is down"
        );
        // JSON shape: nulls serialize as null, not as a panic or a fake 0.
        // (connections/race fields removed 2026-07-17 — dashboard tidy.)
        let json = serde_json::to_value(&resp).expect("serializes");
        assert!(
            json["db"].get("ticks_today").is_none(),
            "retired ticks_today tile absent (BATCH-5 2026-07-19)"
        );
        assert!(json.get("race").is_none(), "retired race field absent");
        assert!(
            json.get("connections").is_none(),
            "retired connections field absent"
        );
    }

    // (parity/scan_connections/parse_status_json test suites deleted
    // 2026-07-17 with their subject fns — dashboard tidy.)

    #[test]
    fn test_parse_vmrss_kib_extracts_value() {
        let text = "Name:\ttickvault\nVmPeak:\t  999 kB\nVmRSS:\t  123456 kB\n";
        assert_eq!(parse_vmrss_kib(text), Some(123_456));
        assert!(parse_vmrss_kib("Name:\tx\n").is_none());
    }

    #[test]
    fn test_ist_today_midnight_literal_shape() {
        let lit = ist_today_midnight_literal();
        assert_eq!(lit.len(), "2026-07-05T00:00:00.000000Z".len());
        assert!(lit.ends_with("T00:00:00.000000Z"));
        // Date prefix stays strict YYYY-MM-DD (is_valid_iso_date deleted
        // 2026-07-17 with the parity reader — inline shape check kept).
        let bytes = &lit.as_bytes()[..10];
        assert!(bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        }));
    }

    #[test]
    fn test_now_ist_nanos_is_ahead_of_utc() {
        // The pub(crate) IST-nanos helper (reused from feeds) must sit the
        // IST offset ahead of raw UTC nanos.
        let utc = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let ist = crate::handlers::feeds::now_ist_nanos();
        let delta = ist - utc;
        let offset = tickvault_common::constants::IST_UTC_OFFSET_NANOS;
        assert!(
            (delta - offset).abs() < 2_000_000_000,
            "IST nanos ~offset ahead of UTC (delta={delta})"
        );
    }
}
