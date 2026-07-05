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
//! - rust shadow: `data/groww/rust-live-ticks.ndjson` size + age + a capped
//!   line count (PR-R1 native-shadow capture file).
//! - connections: `data/groww/shards/c*/groww-status.json` (scale runs) or
//!   the single `data/groww/groww-status.json` (normal runs).
//! - race: the newest `data/groww/parity-<date>.tsv` (PR-R2 comparer output).
//! - db: today's ticks + sealed 1m candles via QuestDB HTTP `/exec` with a
//!   bounded timeout (reuses `super::stats::query_count`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::state::SharedAppState;

/// Default directory holding the Groww sidecar/shadow artifacts. Overridable
/// via [`GROWW_DIR_ENV`] (tests / non-standard layouts).
const GROWW_DIR_DEFAULT: &str = "data/groww";
/// Env override for [`GROWW_DIR_DEFAULT`].
const GROWW_DIR_ENV: &str = "TV_GROWW_DIR";
/// The native Rust shadow client's NDJSON capture file (PR-R1).
const RUST_SHADOW_FILENAME: &str = "rust-live-ticks.ndjson";
/// Per-connection shard directories live under `data/groww/shards/c<NN>/`.
const SHARDS_SUBDIR: &str = "shards";
/// The sidecar connect+subscribe PROOF status file name (both layouts).
const STATUS_FILENAME: &str = "groww-status.json";
/// PR-R2 parity comparer summary: `parity-<YYYY-MM-DD>.tsv`.
const PARITY_PREFIX: &str = "parity-";
const PARITY_SUFFIX: &str = ".tsv";

/// Line-count read cap for the shadow NDJSON: files larger than this report
/// `lines: null` instead of paying an unbounded read every 3s poll.
const LINE_COUNT_CAP_BYTES: u64 = 8 * 1024 * 1024;
/// A status JSON larger than this is malformed — skip it.
const MAX_STATUS_FILE_BYTES: u64 = 64 * 1024;
/// A parity TSV larger than this is not the expected summary — skip it.
const MAX_PARITY_FILE_BYTES: u64 = 256 * 1024;
/// Bound the parity metric map (defensive cap on a public payload).
const MAX_PARITY_ROWS: usize = 128;
/// Bound the connections array (defensive cap on a public payload).
const MAX_CONNECTIONS_REPORTED: usize = 128;
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

/// Native Rust shadow capture file status (PR-R1 `rust-live-ticks.ndjson`).
#[derive(Debug, Serialize)]
pub struct RustShadowStatus {
    pub file_bytes: u64,
    /// Seconds since the file was last written; `null` if mtime unreadable.
    pub modified_age_secs: Option<u64>,
    /// Captured tick lines; `null` when the file exceeds the 8 MiB count cap
    /// (honest skip, never an estimate).
    pub lines: Option<u64>,
}

/// One Groww connection row (from a sidecar `groww-status.json`).
#[derive(Debug, Serialize)]
pub struct BoardConnRow {
    pub conn_id: u32,
    /// Sanitized short event tag (`subscribed` / `streaming` / ...).
    pub event: String,
    pub subscribed_total: u64,
    pub emitted: u64,
    pub dropped: u64,
    /// Seconds since the status file's own timestamp; `null` if absent.
    pub age_secs: Option<u64>,
}

/// Newest parity-comparer summary (PR-R2 `parity-<date>.tsv`), parsed as a
/// generic metric→integer map so future comparer columns flow through.
#[derive(Debug, Serialize)]
pub struct ParityRace {
    /// The IST date embedded in the filename (validated `YYYY-MM-DD`).
    pub date: String,
    pub metrics: BTreeMap<String, i64>,
}

/// QuestDB persistence counters for the "vault" panel.
#[derive(Debug, Serialize)]
pub struct BoardDb {
    /// True when QuestDB answered the probe this poll.
    pub reachable: bool,
    /// Ticks persisted since IST midnight; `null` on query failure.
    pub ticks_today: Option<u64>,
    /// 1m candles sealed since IST midnight; `null` on query failure.
    pub candles_1m_today: Option<u64>,
}

/// The full `GET /api/board/data` payload.
#[derive(Debug, Serialize)]
pub struct BoardDataResponse {
    pub status: BoardStatus,
    /// The existing `/api/feeds/health` payload, embedded verbatim.
    pub feeds: super::feeds::FeedsHealthResponse,
    /// `null` when the shadow capture file does not exist.
    pub rust_shadow: Option<RustShadowStatus>,
    /// Empty when no groww status file exists (feed off / not started).
    pub connections: Vec<BoardConnRow>,
    /// `null` until a parity TSV exists → the page shows "race pending".
    pub race: Option<ParityRace>,
    /// Static caveat flag: Dhan tick timestamps carry whole-second precision
    /// only (`live-market-feed.md` — LTT is epoch SECONDS), so a per-tick
    /// capture-lag number for Dhan would be dishonest sub-second.
    pub dhan_clock_whole_seconds: bool,
    pub db: BoardDb,
}

/// `GET /api/board/data` — the Live Board snapshot. Best-effort everywhere:
/// every failing source degrades to `null`/empty, the endpoint always
/// answers 200 with whatever is truthfully known.
// TEST-EXEMPT: covered by #[tokio::test] test_board_data_all_sources_down_returns_nulls_not_panics (the pub-fn-test-guard greps #[test] files only, not #[tokio::test]).
pub async fn board_data(State(state): State<SharedAppState>) -> Json<BoardDataResponse> {
    // Feeds: reuse the existing health aggregation verbatim.
    let feeds = super::feeds::get_feeds_health(State(state.clone())).await.0;
    let market_open = feeds.market_open;

    // Uptime from the boot-epoch stamp (0 = not stamped yet → null).
    let boot = state.health_status().boot_epoch_secs();
    let now_secs = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
    let uptime_secs = (boot > 0 && now_secs >= boot).then(|| now_secs - boot);

    // File-derived sections (groww status files, shadow NDJSON, parity TSV)
    // bundled into one spawn_blocking so file I/O never parks the runtime.
    // A join failure degrades to the all-empty view — never a panic.
    let dir = groww_dir();
    let now_ist = super::feeds::now_ist_nanos();
    let file_view = tokio::task::spawn_blocking(move || collect_file_view(&dir, now_ist))
        .await
        .unwrap_or_default();

    // QuestDB counts — three bounded queries, concurrently (max one timeout).
    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
    let client = super::stats::build_stats_client(QUESTDB_BOARD_TIMEOUT_SECS);
    let midnight = ist_today_midnight_literal();
    let ticks_sql = format!("SELECT count() FROM ticks WHERE ts >= '{midnight}'");
    let candles_sql = format!("SELECT count() FROM candles_1m WHERE ts >= '{midnight}'");
    let (probe, ticks_today, candles_1m_today) = tokio::join!(
        super::stats::query_count(&client, &base_url, "SHOW TABLES"),
        super::stats::query_count(&client, &base_url, &ticks_sql),
        super::stats::query_count(&client, &base_url, &candles_sql),
    );

    Json(BoardDataResponse {
        status: BoardStatus {
            uptime_secs,
            build_sha_short: tickvault_common::build_info::build_git_sha_short(),
            market_open,
            mem_rss_bytes: read_proc_rss_bytes(),
        },
        feeds,
        rust_shadow: file_view.rust_shadow,
        connections: file_view.connections,
        race: file_view.race,
        dhan_clock_whole_seconds: true,
        db: BoardDb {
            reachable: probe.is_some(),
            ticks_today,
            candles_1m_today,
        },
    })
}

/// The groww artifacts directory (env-overridable for tests).
fn groww_dir() -> PathBuf {
    if let Ok(custom) = std::env::var(GROWW_DIR_ENV)
        && !custom.trim().is_empty()
    {
        return PathBuf::from(custom);
    }
    PathBuf::from(GROWW_DIR_DEFAULT)
}

/// Everything read from disk for one poll (all best-effort).
#[derive(Debug, Default)]
struct FileView {
    rust_shadow: Option<RustShadowStatus>,
    connections: Vec<BoardConnRow>,
    race: Option<ParityRace>,
}

fn collect_file_view(dir: &Path, now_ist_nanos: i64) -> FileView {
    FileView {
        rust_shadow: rust_shadow_status(&dir.join(RUST_SHADOW_FILENAME)),
        connections: scan_connections(dir, now_ist_nanos),
        race: newest_parity_race(dir),
    }
}

/// Status of the native-shadow NDJSON capture file; `None` when absent.
fn rust_shadow_status(path: &Path) -> Option<RustShadowStatus> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let file_bytes = meta.len();
    let modified_age_secs = meta
        .modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .map(|d| d.as_secs());
    let lines = if file_bytes <= LINE_COUNT_CAP_BYTES {
        count_newlines_capped(path)
    } else {
        None
    };
    Some(RustShadowStatus {
        file_bytes,
        modified_age_secs,
        lines,
    })
}

/// Counts `\n` bytes in a file, aborting (→ `None`) past the read cap so a
/// file that grew between the metadata check and the read stays bounded.
fn count_newlines_capped(path: &Path) -> Option<u64> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 64 * 1024];
    let mut count: u64 = 0;
    let mut total: u64 = 0;
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n as u64);
        if total > LINE_COUNT_CAP_BYTES {
            return None;
        }
        count += buf[..n].iter().filter(|&&b| b == b'\n').count() as u64;
    }
    Some(count)
}

/// One row per Groww connection: shard layout (`shards/c<NN>/groww-status.json`)
/// when present, otherwise the single-sidecar `groww-status.json` as conn 0.
fn scan_connections(dir: &Path, now_ist_nanos: i64) -> Vec<BoardConnRow> {
    let mut rows: Vec<BoardConnRow> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir.join(SHARDS_SUBDIR)) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(id) = name.strip_prefix('c').and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            if let Some(row) =
                read_status_file(&entry.path().join(STATUS_FILENAME), id, now_ist_nanos)
            {
                rows.push(row);
            }
        }
    }
    if rows.is_empty()
        && let Some(row) = read_status_file(&dir.join(STATUS_FILENAME), 0, now_ist_nanos)
    {
        rows.push(row);
    }
    rows.sort_by_key(|r| r.conn_id);
    rows.truncate(MAX_CONNECTIONS_REPORTED);
    rows
}

/// Reads + parses one sidecar status file; `None` on any problem (a torn or
/// oversized file is skipped, never an error).
fn read_status_file(path: &Path, conn_id: u32, now_ist_nanos: i64) -> Option<BoardConnRow> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_STATUS_FILE_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    parse_status_json(&text, conn_id, now_ist_nanos)
}

/// Parses the sidecar `write_status` record. Unknown fields are ignored;
/// missing counters default to 0 (the tag itself is what proves liveness).
fn parse_status_json(text: &str, conn_id: u32, now_ist_nanos: i64) -> Option<BoardConnRow> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let event = sanitize_tag(value.get("event").and_then(|v| v.as_str()).unwrap_or(""));
    if event.is_empty() {
        return None;
    }
    let age_secs = value
        .get("ts_ist_nanos")
        .and_then(serde_json::Value::as_i64)
        .map(|ts| {
            let diff = now_ist_nanos.saturating_sub(ts).max(0);
            u64::try_from(diff / 1_000_000_000).unwrap_or(u64::MAX)
        });
    Some(BoardConnRow {
        conn_id,
        event,
        subscribed_total: value
            .get("total")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        emitted: value
            .get("emitted")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        dropped: value
            .get("dropped")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        age_secs,
    })
}

/// Public-payload hygiene: keep only ascii alnum/underscore, max 32 chars.
fn sanitize_tag(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(32)
        .collect()
}

/// Finds + parses the newest `parity-<YYYY-MM-DD>.tsv`; `None` when no valid
/// summary exists yet ("race pending").
fn newest_parity_race(dir: &Path) -> Option<ParityRace> {
    let mut best: Option<(String, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(date) = name
            .strip_prefix(PARITY_PREFIX)
            .and_then(|s| s.strip_suffix(PARITY_SUFFIX))
        else {
            continue;
        };
        if !is_valid_iso_date(date) {
            continue;
        }
        if best.as_ref().is_none_or(|(d, _)| date > d.as_str()) {
            best = Some((date.to_string(), entry.path()));
        }
    }
    let (date, path) = best?;
    let meta = std::fs::metadata(&path).ok()?;
    if meta.len() > MAX_PARITY_FILE_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let metrics = parse_parity_tsv(&text);
    if metrics.is_empty() {
        return None;
    }
    Some(ParityRace { date, metrics })
}

/// Strict `YYYY-MM-DD` shape check (filename → payload, so validate).
fn is_valid_iso_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        })
}

/// Parses the comparer's `metric\tvalue` TSV into a bounded metric map.
/// Non-numeric rows (incl. the header) are skipped.
fn parse_parity_tsv(text: &str) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if out.len() >= MAX_PARITY_ROWS {
            break;
        }
        let mut parts = line.splitn(2, '\t');
        let (Some(key), Some(val)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Ok(num) = val.trim().parse::<i64>() else {
            continue;
        };
        let key: String = key
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(64)
            .collect();
        if key.is_empty() {
            continue;
        }
        out.insert(key, num);
    }
    out
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

    /// Unique per-test scratch dir (no tempfile dep; cleaned best-effort).
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-board-test-{tag}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[tokio::test]
    async fn test_board_data_all_sources_down_returns_nulls_not_panics() {
        // Everything down/absent: QuestDB unreachable, no groww files (test
        // CWD has no data/groww), boot epoch never stamped, market state
        // whatever the calendar says. The endpoint must answer with honest
        // nulls — never panic, never invent a number.
        let Json(resp) = board_data(State(test_state())).await;
        assert!(resp.status.uptime_secs.is_none(), "boot epoch 0 → null");
        assert!(!resp.status.build_sha_short.is_empty());
        assert!(!resp.db.reachable, "QuestDB down → reachable=false");
        assert!(resp.db.ticks_today.is_none());
        assert!(resp.db.candles_1m_today.is_none());
        assert!(resp.race.is_none(), "no parity file → race pending");
        // Feeds section still present (one row per feed, from the registry).
        assert_eq!(
            resp.feeds.feeds.len(),
            crate::feed_state::Feed::ALL.len(),
            "feeds section embedded even when everything else is down"
        );
        assert!(resp.dhan_clock_whole_seconds);
        // JSON shape: nulls serialize as null, not as a panic or a fake 0.
        let json = serde_json::to_value(&resp).expect("serializes");
        assert!(json["db"]["ticks_today"].is_null());
        assert!(json["race"].is_null());
    }

    #[test]
    fn test_parse_parity_tsv_extracts_numeric_metrics() {
        let text = "metric\tvalue\nmatched\t981\nmissing_in_rust\t10\n\
                    latency_all_p50_ns\t2275532\nnot a row\ngarbage\tNaN\n";
        let map = parse_parity_tsv(text);
        assert_eq!(map.get("matched"), Some(&981));
        assert_eq!(map.get("missing_in_rust"), Some(&10));
        assert_eq!(map.get("latency_all_p50_ns"), Some(&2_275_532));
        assert!(!map.contains_key("metric"), "header row skipped");
        assert!(!map.contains_key("garbage"), "non-numeric skipped");
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn test_parse_parity_tsv_caps_rows() {
        let mut text = String::new();
        for i in 0..(MAX_PARITY_ROWS + 50) {
            text.push_str(&format!("k{i}\t{i}\n"));
        }
        assert_eq!(parse_parity_tsv(&text).len(), MAX_PARITY_ROWS);
    }

    #[test]
    fn test_newest_parity_race_picks_latest_valid_date() {
        let dir = scratch_dir("parity");
        std::fs::write(dir.join("parity-2026-07-04.tsv"), "matched\t5\n").expect("write");
        std::fs::write(dir.join("parity-2026-07-05.tsv"), "matched\t9\n").expect("write");
        std::fs::write(dir.join("parity-notadate.tsv"), "matched\t1\n").expect("write");
        let race = newest_parity_race(&dir).expect("race present");
        assert_eq!(race.date, "2026-07-05");
        assert_eq!(race.metrics.get("matched"), Some(&9));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_newest_parity_race_none_when_dir_missing() {
        let dir = std::env::temp_dir().join("tv-board-test-definitely-absent");
        assert!(newest_parity_race(&dir.join("nope")).is_none());
    }

    #[test]
    fn test_scan_connections_prefers_shards_dir() {
        let dir = scratch_dir("shards");
        // Shard layout: c00 + c02 present, c01 missing its status file.
        for (shard, total) in [("c00", 1000u64), ("c02", 998u64)] {
            let sd = dir.join(SHARDS_SUBDIR).join(shard);
            std::fs::create_dir_all(&sd).expect("mkdir");
            std::fs::write(
                sd.join(STATUS_FILENAME),
                format!(
                    "{{\"event\":\"streaming\",\"total\":{total},\"emitted\":7,\
                     \"dropped\":1,\"ts_ist_nanos\":100}}"
                ),
            )
            .expect("write");
        }
        std::fs::create_dir_all(dir.join(SHARDS_SUBDIR).join("c01")).expect("mkdir");
        // A single-file status also present — must be IGNORED when shards exist.
        std::fs::write(
            dir.join(STATUS_FILENAME),
            "{\"event\":\"subscribed\",\"total\":5}",
        )
        .expect("write");
        let rows = scan_connections(&dir, 5_000_000_100);
        assert_eq!(rows.len(), 2, "one row per shard with a status file");
        assert_eq!(rows[0].conn_id, 0);
        assert_eq!(rows[1].conn_id, 2);
        assert_eq!(rows[0].event, "streaming");
        assert_eq!(rows[0].subscribed_total, 1000);
        assert_eq!(rows[0].age_secs, Some(5), "age from ts_ist_nanos delta");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_connections_falls_back_to_single_status_file() {
        let dir = scratch_dir("single");
        std::fs::write(
            dir.join(STATUS_FILENAME),
            "{\"event\":\"subscribed\",\"total\":767,\"emitted\":0,\"dropped\":0}",
        )
        .expect("write");
        let rows = scan_connections(&dir, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].conn_id, 0);
        assert_eq!(rows[0].subscribed_total, 767);
        assert!(rows[0].age_secs.is_none(), "no ts field → null age");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_connections_empty_when_nothing_exists() {
        let dir = scratch_dir("empty");
        assert!(scan_connections(&dir, 0).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_status_json_sanitizes_event_and_rejects_garbage() {
        let row = parse_status_json(
            "{\"event\":\"str<script>eaming!\",\"total\":3}",
            7,
            1_000_000_000,
        )
        .expect("parses");
        assert_eq!(row.event, "strscripteaming", "tag sanitized to alnum/_");
        assert!(parse_status_json("not json at all", 0, 0).is_none());
        assert!(
            parse_status_json("{\"total\":3}", 0, 0).is_none(),
            "no event tag → skipped"
        );
    }

    #[test]
    fn test_count_lines_capped_counts_and_respects_cap() {
        let dir = scratch_dir("lines");
        let path = dir.join(RUST_SHADOW_FILENAME);
        std::fs::write(&path, "a\nb\nc\n").expect("write");
        assert_eq!(count_newlines_capped(&path), Some(3));
        let shadow = rust_shadow_status(&path).expect("present");
        assert_eq!(shadow.file_bytes, 6);
        assert_eq!(shadow.lines, Some(3));
        assert!(shadow.modified_age_secs.is_some());
        assert!(
            rust_shadow_status(&dir.join("absent.ndjson")).is_none(),
            "absent file → null section"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        assert!(is_valid_iso_date(&lit[..10]));
    }

    #[test]
    fn test_is_valid_iso_date() {
        assert!(is_valid_iso_date("2026-07-05"));
        assert!(!is_valid_iso_date("2026-7-5"));
        assert!(!is_valid_iso_date("20260705xx"));
        assert!(!is_valid_iso_date("2026-07-05T"));
    }

    #[test]
    fn test_sanitize_tag_truncates() {
        let long = "a".repeat(100);
        assert_eq!(sanitize_tag(&long).len(), 32);
        assert_eq!(sanitize_tag("stream-ing 42"), "streaming42");
    }
}
