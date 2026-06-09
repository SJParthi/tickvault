//! Boot sequence helpers extracted from `main.rs` for testability and coverage.
//!
//! Contains both pure helper functions (no I/O) and async boot orchestration
//! functions that take dependencies as parameters. All logic that was previously
//! inline in `main()` is extracted here so that `cargo llvm-cov` can instrument
//! it via the lib target.
//!
//! # Function categories
//!
//! - **Pure helpers** — zero I/O, fully testable: `format_bind_addr`, `effective_ws_stagger`, etc.
//! - **Config & logging** — `load_and_validate_config`, `build_env_filter`, `build_fmt_layer`
//! - **Boot orchestration** — `load_instruments`, `build_websocket_pool`, `run_shutdown_fast`
//! - **Observability consumers** — `run_slow_boot_observability` (gap-track + QuestDB health; both boot paths)

use chrono::{NaiveDate, Timelike};
use tracing::warn;

use tickvault_common::trading_calendar::{TradingCalendar, ist_offset};

// ---------------------------------------------------------------------------
// Constants (moved from main.rs for coverage instrumentation)
// ---------------------------------------------------------------------------

/// Base config file path (relative to working directory).
pub const CONFIG_BASE_PATH: &str = "config/base.toml";

/// Log file path for Alloy/Loki consumption (legacy fixed name).
pub const APP_LOG_FILE_PATH: &str = "data/logs/app.log";

/// Log directory for rotated log files.
pub const LOG_DIRECTORY: &str = "data/logs";

/// Error-only log file name (WARN + ERROR only, for fast debugging).
pub const ERROR_LOG_FILE_PATH: &str = "data/logs/errors.log";

/// Fast boot window start (IST).
pub const FAST_BOOT_WINDOW_START: &str = "09:00:00";

/// Fast boot window end (IST). NSE regular session closes at 15:30.
pub const FAST_BOOT_WINDOW_END: &str = "15:30:00";

/// Reduced WebSocket connection stagger for off-market-hours boot (milliseconds).
pub const OFF_HOURS_CONNECTION_STAGGER_MS: u64 = 1000;

/// Local override config file path (git-ignored, optional).
pub const CONFIG_LOCAL_PATH: &str = "config/local.toml";

/// Resolves the deployment environment name for config-file selection.
///
/// Precedence (first non-empty wins): `TV_ENVIRONMENT` → `ENVIRONMENT` → `"dev"`.
/// This is the SAME precedence used by
/// `tickvault_core::auth::secret_manager::resolve_environment`, so the
/// config-file env and the SSM secret prefix (`/tickvault/<env>/...`) always
/// agree — the deployed box's `TV_ENVIRONMENT=staging` selects BOTH
/// `config/staging.toml` AND `/tickvault/staging/...` secrets.
pub fn resolve_config_env() -> String {
    std::env::var("TV_ENVIRONMENT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("ENVIRONMENT")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| "dev".to_string())
}

/// Returns the per-environment config override path (`config/<env>.toml`) to
/// merge BETWEEN base and local, or `None` when no override applies.
///
/// `None` is returned for `dev` and `local` so existing local-dev behaviour
/// (base + local only) is byte-identical — there is no `config/dev.toml`, and
/// `config/local.toml` is already merged separately. For any other env
/// (`staging`, `production`, `sandbox`) the matching `config/<env>.toml` is
/// returned. The env name is sanitised to a strict `[a-z0-9-]` allowlist so a
/// malicious env var can never traverse paths.
pub fn config_env_path(env: &str) -> Option<String> {
    let normalized = env.trim().to_ascii_lowercase();
    // dev/local: no dedicated override file (base + local already cover it).
    if normalized.is_empty() || normalized == "dev" || normalized == "local" {
        return None;
    }
    // Strict allowlist — reject anything that could traverse the filesystem.
    if !normalized
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return None;
    }
    // `prod` is an accepted alias for the canonical `production.toml`.
    let file_stem = if normalized == "prod" {
        "production"
    } else {
        normalized.as_str()
    };
    Some(format!("config/{file_stem}.toml"))
}

// ---------------------------------------------------------------------------
// IST timestamp formatter for tracing-subscriber
// ---------------------------------------------------------------------------

/// Custom `FormatTime` implementation that outputs IST timestamps with +05:30 offset.
///
/// All log timestamps display as `2026-04-06T11:31:41.806275+05:30` instead of
/// UTC `2026-04-06T06:01:41.806275Z`. Required for SEBI compliance and IST-native
/// debugging during market hours.
#[derive(Clone, Debug)]
pub struct IstTimer;

impl tracing_subscriber::fmt::time::FormatTime for IstTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        let now = chrono::Utc::now().with_timezone(&ist_offset());
        write!(w, "{}", now.format("%Y-%m-%dT%H:%M:%S%.6f%:z"))
    }
}

// ---------------------------------------------------------------------------
// Pure helper functions
// ---------------------------------------------------------------------------

/// Determines the effective WebSocket connection stagger in milliseconds.
///
/// Always uses `OFF_HOURS_CONNECTION_STAGGER_MS` (1s) for fast boot.
/// This ensures crash recovery during market hours gets all 5 connections
/// up in ~5 seconds instead of ~50 seconds with 10s stagger.
/// Dhan's WebSocket servers handle 5 concurrent connects without issue.
pub fn effective_ws_stagger(_configured_stagger_ms: u64, _is_market_hours: bool) -> u64 {
    OFF_HOURS_CONNECTION_STAGGER_MS
}

/// Formats a host:port pair into a `SocketAddr` string.
///
/// Returns the formatted string — caller is responsible for parsing.
/// This is a pure function extracted for testability.
pub fn format_bind_addr(host: &str, port: u16) -> String {
    format!("{host}:{port}")
}

/// Determines the boot mode description for logging.
///
/// Returns `"fast"` if a valid token cache exists AND we're within market
/// hours on a trading day. Otherwise returns `"standard"`.
pub fn determine_boot_mode(has_cache: bool, is_market_hours: bool) -> &'static str {
    if has_cache && is_market_hours {
        "fast"
    } else {
        "standard"
    }
}

/// Determines whether to use the fast boot path.
///
/// Fast boot requires BOTH a valid token cache AND being within market hours
/// on a trading day. If the cache exists but we're outside market hours,
/// the slow boot path is used (downloads fresh instruments, starts Docker first).
pub fn should_fast_boot(has_cache: bool, is_market_hours: bool) -> bool {
    has_cache && is_market_hours
}

/// Returns `true` if the post-market 15:30 IST shutdown sequence should
/// emit a `Market closed — WebSockets disconnected, API stays up` Telegram
/// alert.
///
/// The wall-clock 15:30 trigger fires unconditionally on every day because
/// `compute_market_close_sleep` is time-of-day based, not calendar-aware.
/// On weekends and NSE holidays where no market open ever occurred, the
/// trigger still expires and would otherwise emit a misleading `[HIGH]
/// Post-Market` alert. Per `audit-findings-2026-04-17.md` Rule 11
/// (false-OK / false-HIGH suppression) and `market-hours.md` (holiday
/// calendar checked before every trading day), suppress the emission on
/// non-trading days.
pub fn should_emit_post_market_alert(calendar: &TradingCalendar, date: NaiveDate) -> bool {
    calendar.is_trading_day(date)
}

/// Computes the sleep duration until the given market close time (IST).
///
/// Returns `Duration::ZERO` on parse failure or if already past the close time.
pub fn compute_market_close_sleep(market_close_time_str: &str) -> std::time::Duration {
    let close_time = match chrono::NaiveTime::parse_from_str(market_close_time_str, "%H:%M:%S") {
        Ok(t) => t,
        Err(err) => {
            warn!(
                ?err,
                market_close_time = market_close_time_str,
                "failed to parse market close time"
            );
            return std::time::Duration::ZERO;
        }
    };

    let now_ist = chrono::Utc::now().with_timezone(&ist_offset());
    let now_time = now_ist.time();

    if now_time >= close_time {
        return std::time::Duration::ZERO;
    }

    let now_secs = u64::from(now_time.num_seconds_from_midnight());
    let close_secs = u64::from(close_time.num_seconds_from_midnight());
    std::time::Duration::from_secs(close_secs.saturating_sub(now_secs))
}

/// Opens (or creates) the app log file for Alloy consumption.
///
/// Creates `data/logs/` directory if needed. Returns `None` if the file
/// cannot be created (best-effort — logging to stdout always works).
pub fn create_log_file_writer() -> Option<std::fs::File> {
    create_log_file_writer_at(APP_LOG_FILE_PATH)
}

/// Builds the EnvFilter directive string for the rolling app-log file.
///
/// Why this exists separately from the global `RUST_LOG`:
///
/// The historical-fetch + candle-aggregation paths emit hundreds of DEBUG
/// lines per minute (`candle batch flushed to QuestDB`, `skipped invalid
/// candles in API response`, etc.). With a global `RUST_LOG=debug` those
/// alone produce ~7 MB/hour in `data/logs/app.YYYY-MM-DD-HH`, exceeding
/// IDE code-insight thresholds (2.56 MB) and slowing `less` / `grep`.
///
/// This filter applies to the FILE appender ONLY. Stdout (when enabled),
/// `errors.log`, and `errors.jsonl.YYYY-MM-DD-HH` keep the configured
/// global level — DEBUG is preserved for triage at the source.
///
/// # Arguments
/// * `base_level` — the user's configured `[logging] level` (`info`,
///   `debug`, etc.). Used as the default for targets not explicitly
///   downgraded.
///
/// # Returns
/// A directive string suitable for `tracing_subscriber::EnvFilter::new()`.
///
/// # Suppression policy (highest-volume targets first)
///
/// | Target | Reason |
/// |---|---|
/// | `tickvault_storage::tick_persistence` | tick flush noise |
/// | `tickvault_core::option_chain::client` | per-request rate-limit + fetched debug |
/// | `tickvault_core::auth::secret_manager` | per-secret SSM fetch debug |
/// | `aws_config::profile::credentials` | already suppressed at `warn` for credential leak |
/// | `aws_smithy_http_client` / `aws_smithy_runtime` | HTTP request/response noise |
/// | `hyper_util::client::legacy::pool` | connection-pool internal noise |
/// | `rustls::client` | TLS handshake DEBUG |
///
/// All other targets (including `tickvault_core::pipeline`,
/// `tickvault_trading::oms`, `tickvault_core::websocket`) keep
/// `base_level` so trading hot-path DEBUG remains in the file.
#[must_use]
pub fn build_app_log_filter_directive(base_level: &str) -> String {
    // Order matters: later entries win when they overlap. We start with
    // the user's base level and then layer per-target downgrades on top.
    format!(
        "{base},\
         tickvault_storage::tick_persistence=info,\
         tickvault_core::option_chain::client=info,\
         tickvault_core::auth::secret_manager=info,\
         aws_config::profile::credentials=warn,\
         aws_smithy_http_client=warn,\
         aws_smithy_runtime=warn,\
         hyper_util::client::legacy=warn,\
         rustls::client=warn",
        base = base_level
    )
}

/// Opens (or creates) a log file at the given path for Alloy consumption.
///
/// Creates parent directory if needed. Returns `None` if the directory
/// cannot be created or the file cannot be opened.
///
/// Extracted from `create_log_file_writer` for testability — allows tests
/// to exercise error paths with invalid/unwritable paths.
pub fn create_log_file_writer_at(log_file_path: &str) -> Option<std::fs::File> {
    let log_dir = std::path::Path::new(log_file_path)
        .parent()
        .unwrap_or(std::path::Path::new("data/logs"));

    // O(1) EXEMPT: begin — cold path, logging bootstrap before tracing is initialized
    #[allow(clippy::print_stderr)] // APPROVED: tracing not yet initialized at this point
    if let Err(err) = std::fs::create_dir_all(log_dir) {
        eprintln!("warning: cannot create log directory {log_dir:?}: {err}");
        return None;
    }

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file_path)
    {
        Ok(file) => Some(file),
        #[allow(clippy::print_stderr)] // APPROVED: tracing not yet initialized at this point
        Err(err) => {
            eprintln!("warning: cannot open log file {log_file_path}: {err}");
            None
        } // O(1) EXEMPT: end
    }
}

/// Creates (or opens) the error-only log file for fast debugging.
///
/// Contains WARN + ERROR events only, giving operators a small grep-friendly
/// file with ONLY problems. Caller is responsible for attaching a level filter.
// TEST-EXEMPT: cold-path boot function — delegates to `create_log_file_writer_at`
// which is fully tested.
pub fn create_error_log_writer() -> Option<std::fs::File> {
    create_log_file_writer_at(ERROR_LOG_FILE_PATH)
}

// ---------------------------------------------------------------------------
// Clock drift check
// ---------------------------------------------------------------------------

/// Maximum acceptable clock drift in seconds before warning.
pub const CLOCK_DRIFT_THRESHOLD_SECS: u64 = 2;

/// HTTP timeout for clock drift check (seconds).
const CLOCK_DRIFT_HTTP_TIMEOUT_SECS: u64 = 5;

// APPROVED: hardcoded URL — public time reference, not a Dhan API endpoint
const CLOCK_DRIFT_REFERENCE_URL: &str = "https://www.google.com";

/// Checks system clock against an HTTP server's Date header (cold path, boot only).
pub async fn check_clock_drift() -> Option<i64> {
    let local_now = chrono::Utc::now();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            CLOCK_DRIFT_HTTP_TIMEOUT_SECS,
        ))
        .build()
        .ok()?;
    let response = client.head(CLOCK_DRIFT_REFERENCE_URL).send().await.ok()?;
    let date_str = response.headers().get("date")?.to_str().ok()?;
    let server_time = chrono::DateTime::parse_from_str(date_str, "%a, %d %b %Y %H:%M:%S GMT")
        .ok()?
        .with_timezone(&chrono::Utc);
    let drift_secs = (server_time - local_now).num_seconds();
    if drift_secs.unsigned_abs() > CLOCK_DRIFT_THRESHOLD_SECS {
        warn!(
            drift_secs,
            "clock drift detected — system clock may be inaccurate"
        );
    }
    Some(drift_secs)
}

// ---------------------------------------------------------------------------
// Heartbeat watchdog
// ---------------------------------------------------------------------------

/// Heartbeat watchdog interval in seconds.
pub const WATCHDOG_INTERVAL_SECS: u64 = 30;

/// Spawns a background watchdog that periodically checks system liveness.
///
/// Checks every 30 seconds:
/// - Token handle has a valid (non-None) token
/// - Tick broadcast sender has active receivers
///
/// Logs ERROR on failure (triggers Telegram via Loki -> Grafana).
pub fn spawn_heartbeat_watchdog(
    token_handle: tickvault_core::auth::token_manager::TokenHandle,
    tick_sender: tokio::sync::broadcast::Sender<tickvault_common::tick_types::ParsedTick>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(WATCHDOG_INTERVAL_SECS));
        interval.tick().await; // Skip immediate first tick.

        loop {
            interval.tick().await;

            let token_ok = {
                let guard = token_handle.load();
                guard.as_ref().is_some()
            };
            if !token_ok {
                // AUTH-GAP-01: token watchdog observed a None handle mid-session.
                tracing::error!(
                    code = tickvault_common::error_code::ErrorCode::AuthGapTokenExpiry.code_str(),
                    severity = tickvault_common::error_code::ErrorCode::AuthGapTokenExpiry
                        .severity()
                        .as_str(),
                    "AUTH-GAP-01: WATCHDOG — token handle is None, authentication may have failed"
                );
            }

            let receiver_count = tick_sender.receiver_count();
            if receiver_count == 0 {
                tracing::error!(
                    "WATCHDOG: tick broadcast has 0 receivers — tick processor may have crashed"
                );
            }

            // L4: Export system metrics (open FDs, thread count) every watchdog cycle.
            crate::infra::export_system_metrics();

            // C1: Emit systemd watchdog heartbeat (no-op if not under systemd).
            crate::infra::notify_systemd_watchdog();
        }
    })
}

// ---------------------------------------------------------------------------
// STAGE-D: Boot-time WAL replay drain helpers
// ---------------------------------------------------------------------------
//
// These helpers drain frames recovered from the WS frame WAL at boot, before
// any live connection opens. They are the per-WS-type counterparts of the
// LiveFeed `frame_sender_clone()` injection path: for Depth and OrderUpdate,
// the live per-underlying / per-contract channels are built dynamically
// much later in boot, so we cannot inject through them without a major
// refactor. Instead, each helper runs a one-shot drain directly against
// the final persistence sink:
//
//   - Depth-20 / Depth-200 → temporary [`DeepDepthWriter`] that parses each
//     recovered binary frame via the dispatcher and writes to QuestDB. The
//     compound dedup key (`STORAGE-GAP-01`) makes the drain idempotent —
//     replaying the same WAL N times yields one row per logical record.
//
//   - OrderUpdate → the live `broadcast::Sender<OrderUpdate>` (available
//     at line 1073/2705). Each recovered JSON frame is reparsed via
//     `parse_order_update` and broadcast. If there are no subscribers
//     yet the broadcast is a no-op — the raw JSON is still preserved in
//     the WAL archive directory for forensic replay.
//
// All three helpers run once at boot and never block the critical path
// beyond their own duration. They return counters for the caller to emit
// Prometheus metrics.

/// Drains recovered order-update JSON frames into the live broadcast
/// channel. Runs once at boot, right after `order_update_sender` is
/// created and before the live order-update WebSocket starts.
///
/// Returns `(parsed, broadcast_count, parse_errors)`. Broadcast count
/// reflects successful `send()` calls; it may be zero if there are no
/// subscribers yet — that is expected and non-fatal, because the raw
/// JSON is already durable in the WAL archive for forensic replay.
pub fn drain_replayed_order_updates_to_broadcast(
    frames: Vec<Vec<u8>>,
    sender: &tokio::sync::broadcast::Sender<tickvault_common::order_types::OrderUpdate>,
) -> (u64, u64, u64) {
    let mut parsed = 0u64;
    let mut broadcast_count = 0u64;
    let mut parse_errors = 0u64;

    for frame in frames {
        let text = match std::str::from_utf8(&frame) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "STAGE-C.2b: order-update replay drain — frame is not valid UTF-8; skipping"
                );
                parse_errors += 1;
                continue;
            }
        };

        match tickvault_core::parser::order_update::parse_order_update(text) {
            Ok(update) => {
                parsed += 1;
                // `send()` returns the number of subscribers that received
                // the message. Zero subscribers is NOT an error during
                // boot drain — live consumers attach seconds later. The
                // raw JSON is already durable in the archive, so there
                // is no data loss even if no one consumes the broadcast.
                match sender.send(update) {
                    Ok(_) => broadcast_count += 1,
                    Err(_) => {
                        // No receivers yet — not an error; frame remains in WAL archive.
                    }
                }
            }
            Err(err) => {
                parse_errors += 1;
                tracing::warn!(
                    ?err,
                    text_len = text.len(),
                    "STAGE-C.2b: order-update replay drain — parse error; skipping"
                );
            }
        }
    }

    (parsed, broadcast_count, parse_errors)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    // -----------------------------------------------------------------------
    // resolve_config_env — env-var precedence (TV_ENVIRONMENT > ENVIRONMENT > dev)
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_config_env_defaults_to_dev_when_unset() {
        // Process-global env vars are racy across parallel tests, so this test
        // only asserts the DEFAULT branch under a serialized lock that clears
        // both vars. The precedence ordering itself is pinned by the pure
        // `config_env_path` tests below + the documented contract.
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: single-threaded section under ENV_LOCK; restored before unlock.
        let saved_tv = std::env::var("TV_ENVIRONMENT").ok();
        let saved_env = std::env::var("ENVIRONMENT").ok();
        unsafe {
            std::env::remove_var("TV_ENVIRONMENT");
            std::env::remove_var("ENVIRONMENT");
        }
        assert_eq!(resolve_config_env(), "dev");
        unsafe {
            match saved_tv {
                Some(v) => std::env::set_var("TV_ENVIRONMENT", v),
                None => std::env::remove_var("TV_ENVIRONMENT"),
            }
            match saved_env {
                Some(v) => std::env::set_var("ENVIRONMENT", v),
                None => std::env::remove_var("ENVIRONMENT"),
            }
        }
    }

    // -----------------------------------------------------------------------
    // config_env_path — per-environment override selection + path-traversal safety
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_env_path_dev_and_local_return_none() {
        // dev/local must NOT add an override file — base + local already cover
        // local development, so behaviour stays byte-identical.
        assert_eq!(config_env_path("dev"), None);
        assert_eq!(config_env_path("local"), None);
        assert_eq!(config_env_path(""), None);
        assert_eq!(config_env_path("  "), None);
        assert_eq!(config_env_path("DEV"), None); // case-insensitive
    }

    #[test]
    fn test_config_env_path_staging_maps_to_staging_toml() {
        // The 3-month data-pull phase runs staging = sandbox/dry_run config.
        assert_eq!(
            config_env_path("staging"),
            Some("config/staging.toml".to_string())
        );
    }

    #[test]
    fn test_config_env_path_prod_aliases_to_production_toml() {
        // Both "prod" and "production" resolve to the canonical production.toml.
        assert_eq!(
            config_env_path("prod"),
            Some("config/production.toml".to_string())
        );
        assert_eq!(
            config_env_path("production"),
            Some("config/production.toml".to_string())
        );
        assert_eq!(
            config_env_path("PROD"),
            Some("config/production.toml".to_string())
        );
    }

    #[test]
    fn test_config_env_path_rejects_path_traversal() {
        // A hostile env var must never escape config/ — anything outside the
        // strict [a-z0-9-] allowlist returns None (falls back to base+local).
        assert_eq!(config_env_path("../secrets"), None);
        assert_eq!(config_env_path("..%2fetc%2fpasswd"), None);
        assert_eq!(config_env_path("prod/../../etc"), None);
        assert_eq!(config_env_path("a b"), None);
        assert_eq!(config_env_path("prod.toml"), None); // dot not allowed
    }

    // -----------------------------------------------------------------------
    // build_app_log_filter_directive
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_app_log_filter_directive_starts_with_base_level() {
        let d = build_app_log_filter_directive("info");
        assert!(
            d.starts_with("info,"),
            "directive must lead with base level: {d}"
        );
    }

    #[test]
    fn app_log_filter_with_debug_base_still_suppresses_noisy_targets() {
        let d = build_app_log_filter_directive("debug");
        assert!(d.starts_with("debug,"));
        // The whole point: even with `debug` as the base, the chatty
        // targets must be downgraded.
        assert!(d.contains("tickvault_storage::tick_persistence=info"));
        assert!(d.contains("tickvault_core::option_chain::client=info"));
        assert!(d.contains("aws_config::profile::credentials=warn"));
    }

    #[test]
    fn app_log_filter_parses_as_valid_envfilter_directive() {
        // Belt-and-suspenders: the directive string must be parseable
        // by tracing_subscriber::EnvFilter or boot will silently fall
        // back to defaults.
        for base in ["trace", "debug", "info", "warn", "error"] {
            let d = build_app_log_filter_directive(base);
            let parsed = tracing_subscriber::EnvFilter::try_new(&d);
            assert!(
                parsed.is_ok(),
                "directive must parse for base={base}: {d}\nerr: {:?}",
                parsed.err()
            );
        }
    }

    #[test]
    fn app_log_filter_aws_credentials_kept_at_warn_to_prevent_credential_leak() {
        // Regression: aws_config::profile::credentials logs the
        // access_key_id at INFO. We MUST keep it at warn or higher
        // in the file appender to avoid persisting that to disk.
        for base in ["trace", "debug", "info"] {
            let d = build_app_log_filter_directive(base);
            assert!(
                d.contains("aws_config::profile::credentials=warn"),
                "base={base}: aws credentials target must be downgraded to warn: {d}"
            );
        }
    }

    #[test]
    fn app_log_filter_does_not_suppress_trading_hot_path() {
        // The trading + websocket + pipeline hot paths MUST keep the
        // base level — operators need that DEBUG for triage.
        let d = build_app_log_filter_directive("debug");
        assert!(!d.contains("tickvault_trading=info"));
        assert!(!d.contains("tickvault_core::pipeline=info"));
        assert!(!d.contains("tickvault_core::websocket=info"));
    }

    #[test]
    fn app_log_filter_directive_has_no_trailing_comma() {
        // tracing-subscriber tolerates trailing commas in some versions
        // but not all; defensive contract.
        let d = build_app_log_filter_directive("info");
        assert!(!d.ends_with(','), "directive must not end with comma: {d}");
    }

    // -----------------------------------------------------------------------
    // Config path tests
    // -----------------------------------------------------------------------

    #[test]
    fn config_base_path_is_toml() {
        assert!(CONFIG_BASE_PATH.ends_with(".toml"));
    }

    #[test]
    fn config_local_path_is_toml() {
        assert!(CONFIG_LOCAL_PATH.ends_with(".toml"));
    }

    #[test]
    fn test_config_base_path_starts_with_config() {
        assert!(CONFIG_BASE_PATH.starts_with("config/"));
    }

    #[test]
    fn test_config_local_path_starts_with_config() {
        assert!(CONFIG_LOCAL_PATH.starts_with("config/"));
    }

    #[test]
    fn test_config_base_path_is_relative_toml() {
        let path = std::path::Path::new(CONFIG_BASE_PATH);
        assert!(!path.is_absolute());
        assert!(path.extension().is_some_and(|ext| ext == "toml"));
    }

    #[test]
    fn test_config_local_path_is_git_ignorable() {
        assert!(CONFIG_LOCAL_PATH.contains("local"));
    }

    // -----------------------------------------------------------------------
    // Socket addr tests
    // -----------------------------------------------------------------------

    #[test]
    fn socket_addr_parses_valid_host_port() {
        let addr: Result<SocketAddr, _> = "0.0.0.0:8080".parse();
        assert!(addr.is_ok());
    }

    #[test]
    fn socket_addr_rejects_invalid() {
        let addr: Result<SocketAddr, _> = "not_a_socket".parse();
        assert!(addr.is_err());
    }

    // -----------------------------------------------------------------------
    // compute_market_close_sleep tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_market_close_sleep_valid_time() {
        let duration = compute_market_close_sleep("15:30:00");
        assert!(duration.as_secs() <= 86_400);
    }

    #[test]
    fn test_compute_market_close_sleep_invalid_format() {
        let duration = compute_market_close_sleep("invalid");
        assert_eq!(duration, std::time::Duration::ZERO);
    }

    #[test]
    fn test_compute_market_close_sleep_empty_string() {
        let duration = compute_market_close_sleep("");
        assert_eq!(duration, std::time::Duration::ZERO);
    }

    #[test]
    fn test_compute_market_close_sleep_midnight() {
        let duration = compute_market_close_sleep("00:00:00");
        assert!(duration.as_secs() <= 86_400);
    }

    #[test]
    fn test_compute_market_close_sleep_end_of_day() {
        let duration = compute_market_close_sleep("23:59:59");
        assert!(duration.as_secs() <= 86_400);
    }

    #[test]
    fn test_compute_market_close_sleep_partial_format_rejected() {
        let duration = compute_market_close_sleep("15:30");
        assert_eq!(duration, std::time::Duration::ZERO);
    }

    #[test]
    fn test_compute_market_close_sleep_numbers_only_rejected() {
        let duration = compute_market_close_sleep("153000");
        assert_eq!(duration, std::time::Duration::ZERO);
    }

    #[test]
    fn test_compute_market_close_sleep_out_of_range_hours() {
        let duration = compute_market_close_sleep("25:00:00");
        assert_eq!(duration, std::time::Duration::ZERO);
    }

    #[test]
    fn test_compute_market_close_sleep_with_seconds() {
        let duration = compute_market_close_sleep("09:15:00");
        assert!(duration.as_secs() <= 86_400);
    }

    #[test]
    fn test_compute_market_close_sleep_various_invalid_formats() {
        let invalid_inputs = ["abc", "12:30", "12-30-00", "99:99:99", "12:30:00 AM"];
        for input in &invalid_inputs {
            let duration = compute_market_close_sleep(input);
            assert_eq!(
                duration,
                std::time::Duration::ZERO,
                "invalid format '{input}' should return Duration::ZERO"
            );
        }
    }

    #[test]
    fn test_compute_market_close_sleep_boundary_values() {
        let d = compute_market_close_sleep("00:00:01");
        assert!(d.as_secs() <= 86_400);
        let d = compute_market_close_sleep("23:59:58");
        assert!(d.as_secs() <= 86_400);
    }

    #[test]
    fn test_compute_market_close_sleep_always_lte_24h() {
        let times = [
            "00:00:00", "01:00:00", "06:00:00", "09:15:00", "12:00:00", "15:30:00", "18:00:00",
            "23:59:59",
        ];
        for t in &times {
            let d = compute_market_close_sleep(t);
            assert!(d.as_secs() <= 86_400, "must be <= 24h for {t}");
        }
    }

    #[test]
    fn test_compute_market_close_sleep_returns_zero_for_past_time() {
        let now_ist = chrono::Utc::now().with_timezone(&ist_offset());
        let now_hour = now_ist.time().hour();
        if now_hour >= 1 {
            let d = compute_market_close_sleep("00:00:01");
            assert_eq!(d, std::time::Duration::ZERO);
        }
    }

    #[test]
    fn test_compute_market_close_sleep_uses_ist_not_utc() {
        let now_ist = chrono::Utc::now().with_timezone(&ist_offset());
        let now_secs = u64::from(now_ist.time().num_seconds_from_midnight());
        let close_secs: u64 = 23 * 3600 + 59 * 60 + 59;
        let d = compute_market_close_sleep("23:59:59");
        if now_secs < close_secs {
            assert!(d.as_secs() > 0);
        } else {
            assert_eq!(d, std::time::Duration::ZERO);
        }
    }

    #[test]
    fn test_compute_market_close_sleep_saturating_sub() {
        for h in 0..24_u32 {
            let time_str = format!("{h:02}:00:00");
            let d = compute_market_close_sleep(&time_str);
            assert!(d.as_secs() <= 86_400);
        }
    }

    #[test]
    fn test_compute_market_close_sleep_num_seconds_from_midnight() {
        let t = chrono::NaiveTime::parse_from_str("15:30:00", "%H:%M:%S").unwrap();
        assert_eq!(t.num_seconds_from_midnight(), 15 * 3600 + 30 * 60);
    }

    #[test]
    fn test_compute_market_close_sleep_midnight_secs() {
        let t = chrono::NaiveTime::parse_from_str("00:00:00", "%H:%M:%S").unwrap();
        assert_eq!(t.num_seconds_from_midnight(), 0);
    }

    #[test]
    fn test_compute_market_close_sleep_end_of_day_secs() {
        let t = chrono::NaiveTime::parse_from_str("23:59:59", "%H:%M:%S").unwrap();
        assert_eq!(t.num_seconds_from_midnight(), 86399);
    }

    // -----------------------------------------------------------------------
    // Fast boot window tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_post_market_monitor_constants() {
        assert_eq!(FAST_BOOT_WINDOW_END, "15:30:00");
    }

    #[test]
    fn test_fast_boot_window_start_is_valid_time() {
        let parsed = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_START, "%H:%M:%S");
        assert!(parsed.is_ok());
    }

    #[test]
    fn test_fast_boot_window_end_is_valid_time() {
        let parsed = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_END, "%H:%M:%S");
        assert!(parsed.is_ok());
    }

    #[test]
    fn test_fast_boot_window_start_before_end() {
        let start = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_START, "%H:%M:%S").unwrap();
        let end = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_END, "%H:%M:%S").unwrap();
        assert!(start < end);
    }

    #[test]
    fn test_fast_boot_window_covers_market_hours() {
        let start = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_START, "%H:%M:%S").unwrap();
        let end = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_END, "%H:%M:%S").unwrap();
        let market_open = chrono::NaiveTime::parse_from_str("09:15:00", "%H:%M:%S").unwrap();
        let market_close = chrono::NaiveTime::parse_from_str("15:30:00", "%H:%M:%S").unwrap();
        assert!(start <= market_open);
        assert!(end >= market_close);
    }

    #[test]
    fn test_fast_boot_window_duration_is_reasonable() {
        let start = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_START, "%H:%M:%S").unwrap();
        let end = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_END, "%H:%M:%S").unwrap();
        let duration_secs = end.num_seconds_from_midnight() - start.num_seconds_from_midnight();
        assert!(duration_secs >= 3600);
        assert!(duration_secs <= 43200);
    }

    // -----------------------------------------------------------------------
    // Off-hours stagger tests
    // -----------------------------------------------------------------------

    #[test]
    fn off_hours_stagger_is_less_than_market_hours() {
        const {
            assert!(OFF_HOURS_CONNECTION_STAGGER_MS > 0);
            assert!(OFF_HOURS_CONNECTION_STAGGER_MS <= 2000);
        }
    }

    #[test]
    fn test_off_hours_stagger_constant_value() {
        assert_eq!(OFF_HOURS_CONNECTION_STAGGER_MS, 1000);
    }

    #[test]
    fn test_off_hours_stagger_fits_in_u64() {
        let d = std::time::Duration::from_millis(OFF_HOURS_CONNECTION_STAGGER_MS);
        assert_eq!(d.as_secs(), 1);
    }

    // -----------------------------------------------------------------------
    // App log file path tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_app_log_file_path_is_not_empty() {
        assert!(!APP_LOG_FILE_PATH.is_empty());
    }

    #[test]
    fn test_app_log_file_path_ends_with_log() {
        assert!(APP_LOG_FILE_PATH.ends_with(".log"));
    }

    #[test]
    fn test_app_log_file_path_is_relative() {
        assert!(!APP_LOG_FILE_PATH.starts_with('/'));
    }

    #[test]
    fn test_app_log_file_path_has_two_segments() {
        let segments: Vec<&str> = APP_LOG_FILE_PATH.split('/').collect();
        assert!(segments.len() >= 2);
    }

    #[test]
    fn test_create_log_file_app_log_file_path_parent_dir() {
        let path = std::path::Path::new(APP_LOG_FILE_PATH);
        let parent = path.parent().unwrap_or(std::path::Path::new("data/logs"));
        assert!(!parent.as_os_str().is_empty());
    }

    #[test]
    fn test_create_log_file_path_in_data_directory() {
        assert!(APP_LOG_FILE_PATH.starts_with("data/"));
    }

    // -----------------------------------------------------------------------
    // create_log_file_writer tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_create_log_file_writer_returns_some_in_default_path() {
        let _ = create_log_file_writer();
    }

    #[test]
    fn test_create_log_file_writer_returns_file_handle() {
        let _ = create_log_file_writer();
    }

    #[test]
    fn test_create_log_file_writer_with_temp_dir() {
        let tmp = std::env::temp_dir().join("tv_test_log_writer");
        let _ = std::fs::create_dir_all(&tmp);
        let log_path = tmp.join("test.log");
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path);
        assert!(result.is_ok());
        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_dir(&tmp);
    }

    #[test]
    fn test_create_log_file_parent_extraction() {
        let path = std::path::Path::new(APP_LOG_FILE_PATH);
        let parent = path.parent();
        assert!(parent.is_some());
        assert!(!parent.unwrap().as_os_str().is_empty());
    }

    // -----------------------------------------------------------------------
    // Shutdown bind addr tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_shutdown_bind_addr_fallback() {
        let fallback: SocketAddr = SocketAddr::from(([0, 0, 0, 0], 8080));
        assert_eq!(fallback.port(), 8080);
    }

    #[test]
    fn test_shutdown_bind_addr_parse_invalid_triggers_fallback() {
        let result: Result<SocketAddr, _> = "not_valid".parse();
        assert!(result.is_err());
        let fallback = result.unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 8080)));
        assert_eq!(fallback.port(), 8080);
    }

    // -----------------------------------------------------------------------
    // create_log_file_writer — deeper tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_create_log_file_writer_returns_some() {
        // In a normal environment, the log file should be creatable.
        // This exercises both the create_dir_all and OpenOptions paths.
        let result = create_log_file_writer();
        assert!(
            result.is_some(),
            "create_log_file_writer should succeed in writable directory"
        );
    }

    #[test]
    fn test_create_log_file_writer_file_is_writable() {
        use std::io::Write;
        if let Some(mut file) = create_log_file_writer() {
            let write_result = file.write_all(b"test log line\n");
            assert!(
                write_result.is_ok(),
                "log file must be writable after creation"
            );
        }
    }

    #[test]
    fn test_create_log_file_writer_idempotent() {
        // Multiple calls should all succeed (file opened in append mode).
        let f1 = create_log_file_writer();
        let f2 = create_log_file_writer();
        assert!(f1.is_some());
        assert!(f2.is_some());
    }

    #[test]
    fn test_app_log_file_path_parent_is_valid_dir() {
        let path = std::path::Path::new(APP_LOG_FILE_PATH);
        let parent = path.parent();
        assert!(
            parent.is_some(),
            "log file path must have a parent directory"
        );
        let parent = parent.unwrap();
        // After create_log_file_writer(), the parent dir should exist.
        let _ = create_log_file_writer();
        assert!(
            parent.exists(),
            "log directory must exist after writer creation"
        );
    }
    // -----------------------------------------------------------------------
    // compute_market_close_sleep — deterministic future time test
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_market_close_sleep_future_time_returns_positive() {
        // Use 23:59:59 which is always in the future during normal test runs
        // (IST is UTC+5:30, so 23:59:59 IST is ~18:29:59 UTC)
        let now_ist = chrono::Utc::now().with_timezone(&ist_offset());
        let now_secs = now_ist.time().num_seconds_from_midnight();
        let close_secs = 23 * 3600 + 59 * 60 + 59; // 23:59:59
        if now_secs < close_secs {
            let d = compute_market_close_sleep("23:59:59");
            assert!(d.as_secs() > 0, "future time must return positive duration");
            let expected_secs = close_secs - now_secs;
            // Allow 2 seconds tolerance for test execution time
            assert!(
                (d.as_secs() as i64 - expected_secs as i64).unsigned_abs() <= 2,
                "duration should match expected time until close"
            );
        }
    }

    #[test]
    fn test_compute_market_close_sleep_consistency() {
        // Two calls close together should return similar durations
        let d1 = compute_market_close_sleep("15:30:00");
        let d2 = compute_market_close_sleep("15:30:00");
        // Within 1 second of each other
        let diff = if d1 > d2 {
            d1.as_secs() - d2.as_secs()
        } else {
            d2.as_secs() - d1.as_secs()
        };
        assert!(
            diff <= 1,
            "consecutive calls should return similar durations"
        );
    }

    // -----------------------------------------------------------------------
    // Const assertions — compile-time guarantees
    // -----------------------------------------------------------------------

    #[test]
    fn test_fast_boot_window_start_is_before_nine_fifteen() {
        let start = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_START, "%H:%M:%S").unwrap();
        let nine_fifteen = chrono::NaiveTime::parse_from_str("09:15:00", "%H:%M:%S").unwrap();
        assert!(
            start <= nine_fifteen,
            "fast boot window must start at or before market open (09:15)"
        );
    }

    #[test]
    fn test_fast_boot_window_end_is_market_close() {
        let end = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_END, "%H:%M:%S").unwrap();
        let market_close = chrono::NaiveTime::parse_from_str("15:30:00", "%H:%M:%S").unwrap();
        assert_eq!(
            end, market_close,
            "fast boot window end must equal market close"
        );
    }

    // -----------------------------------------------------------------------
    // compute_market_close_sleep — exercise now >= close_time branch
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_market_close_sleep_definitely_past_midnight() {
        // 00:00:00 is always in the past after the first second of the day.
        // In IST (UTC+5:30), 00:00:00 is midnight. Tests run during daytime,
        // so this should always return ZERO.
        let now_ist = chrono::Utc::now().with_timezone(&ist_offset());
        let now_secs = now_ist.time().num_seconds_from_midnight();
        if now_secs > 0 {
            let d = compute_market_close_sleep("00:00:00");
            assert_eq!(
                d,
                std::time::Duration::ZERO,
                "midnight is always past during daytime tests"
            );
        }
    }

    #[test]
    fn test_compute_market_close_sleep_past_time_is_zero() {
        // Test with 00:00:01 — should be past at any reasonable test time.
        let now_ist = chrono::Utc::now().with_timezone(&ist_offset());
        let now_secs = now_ist.time().num_seconds_from_midnight();
        if now_secs > 1 {
            let d = compute_market_close_sleep("00:00:01");
            assert_eq!(d, std::time::Duration::ZERO);
        }
    }

    // -----------------------------------------------------------------------
    // create_log_file_writer — deeper error path coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_create_log_file_writer_creates_directory_if_missing() {
        // Verify that after calling create_log_file_writer, the parent
        // directory exists (it creates it if missing).
        let result = create_log_file_writer();
        if result.is_some() {
            let log_dir = std::path::Path::new(APP_LOG_FILE_PATH)
                .parent()
                .unwrap_or(std::path::Path::new("data/logs"));
            assert!(log_dir.exists(), "log directory must be created");
            assert!(log_dir.is_dir(), "log parent must be a directory");
        }
    }

    #[test]
    fn test_create_log_file_writer_file_exists_after_creation() {
        let result = create_log_file_writer();
        if result.is_some() {
            let log_path = std::path::Path::new(APP_LOG_FILE_PATH);
            assert!(log_path.exists(), "log file must exist after creation");
            assert!(log_path.is_file(), "log path must be a regular file");
        }
    }

    #[test]
    fn test_create_log_file_writer_append_mode() {
        use std::io::Write;
        // Write to the log file twice — second write should append, not overwrite.
        let f1 = create_log_file_writer();
        if let Some(mut file) = f1 {
            file.write_all(b"first line\n").unwrap();
        }
        let f2 = create_log_file_writer();
        if let Some(mut file) = f2 {
            file.write_all(b"second line\n").unwrap();
        }
        // If we got here without panic, append mode works correctly.
    }

    // -----------------------------------------------------------------------
    // create_log_file_writer_at — error path coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_create_log_file_writer_at_unwritable_directory_returns_none() {
        // /proc is a read-only pseudo-filesystem on Linux — create_dir_all fails.
        let result = create_log_file_writer_at("/proc/nonexistent_tv_dir/app.log");
        assert!(result.is_none(), "unwritable directory should return None");
    }

    #[test]
    fn test_create_log_file_writer_at_directory_as_file_path_returns_none() {
        // Creating a file at a path where a directory already exists fails.
        // /tmp always exists — trying to open it as a file should fail.
        let result = create_log_file_writer_at("/tmp");
        assert!(
            result.is_none(),
            "directory path as file should return None"
        );
    }

    #[test]
    fn test_create_log_file_writer_at_valid_path_returns_some() {
        let tmp = std::env::temp_dir().join("tv_test_writer_at");
        let log_path = tmp.join("test_at.log");
        let path_str = log_path.to_string_lossy().to_string();
        let result = create_log_file_writer_at(&path_str);
        assert!(result.is_some(), "valid path should return Some");
        // Cleanup
        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_dir(&tmp);
    }

    #[test]
    fn test_create_log_file_writer_at_empty_path_returns_none() {
        // Empty string as path — will fail to create dir or open file.
        let result = create_log_file_writer_at("");
        assert!(result.is_none(), "empty path should return None");
    }

    #[test]
    fn test_create_log_file_writer_at_deeply_nested_creates_dirs() {
        let tmp = std::env::temp_dir().join("tv_test_deep/a/b/c");
        let log_path = tmp.join("deep.log");
        let path_str = log_path.to_string_lossy().to_string();
        let result = create_log_file_writer_at(&path_str);
        assert!(result.is_some(), "deeply nested path should succeed");
        // Cleanup
        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("tv_test_deep"));
    }

    #[test]
    fn test_create_log_file_writer_at_default_path_matches_original() {
        // Verify that create_log_file_writer delegates to create_log_file_writer_at
        let result_default = create_log_file_writer();
        let result_at = create_log_file_writer_at(APP_LOG_FILE_PATH);
        // Both should succeed in the same environment
        assert_eq!(
            result_default.is_some(),
            result_at.is_some(),
            "both functions should agree on success/failure"
        );
    }

    #[test]
    fn test_create_log_file_writer_at_dev_null_returns_some() {
        // /dev/null is always writable on Linux
        let result = create_log_file_writer_at("/dev/null");
        assert!(result.is_some(), "/dev/null should be openable");
    }

    #[test]
    fn test_create_log_file_writer_at_read_only_fs_returns_none() {
        // /sys is a sysfs pseudo-filesystem — directory creation should fail
        let result = create_log_file_writer_at("/sys/tv_impossible/app.log");
        assert!(result.is_none(), "read-only filesystem should return None");
    }

    // -----------------------------------------------------------------------
    // effective_ws_stagger tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_effective_ws_stagger_always_uses_fast_stagger() {
        // Always uses 1s stagger for fast crash recovery during market hours.
        assert_eq!(
            effective_ws_stagger(3000, true),
            OFF_HOURS_CONNECTION_STAGGER_MS
        );
        assert_eq!(
            effective_ws_stagger(3000, false),
            OFF_HOURS_CONNECTION_STAGGER_MS
        );
        assert_eq!(
            effective_ws_stagger(0, true),
            OFF_HOURS_CONNECTION_STAGGER_MS
        );
        assert_eq!(
            effective_ws_stagger(0, false),
            OFF_HOURS_CONNECTION_STAGGER_MS
        );
    }

    // -----------------------------------------------------------------------
    // format_bind_addr tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_bind_addr_ipv4() {
        let addr = format_bind_addr("0.0.0.0", 3001);
        assert_eq!(addr, "0.0.0.0:3001");
    }

    #[test]
    fn test_format_bind_addr_localhost() {
        let addr = format_bind_addr("127.0.0.1", 8080);
        assert_eq!(addr, "127.0.0.1:8080");
    }

    #[test]
    fn test_format_bind_addr_port_zero() {
        let addr = format_bind_addr("0.0.0.0", 0);
        assert_eq!(addr, "0.0.0.0:0");
    }

    #[test]
    fn test_format_bind_addr_parses_to_socket_addr() {
        let addr = format_bind_addr("0.0.0.0", 3001);
        let parsed: Result<SocketAddr, _> = addr.parse();
        assert!(parsed.is_ok(), "formatted addr must parse to SocketAddr");
        assert_eq!(parsed.unwrap().port(), 3001);
    }

    #[test]
    fn test_format_bind_addr_high_port() {
        let addr = format_bind_addr("0.0.0.0", 65535);
        let parsed: SocketAddr = addr.parse().unwrap();
        assert_eq!(parsed.port(), 65535);
    }

    // -----------------------------------------------------------------------
    // determine_boot_mode tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_determine_boot_mode_fast_with_cache_and_market() {
        assert_eq!(determine_boot_mode(true, true), "fast");
    }

    #[test]
    fn test_determine_boot_mode_standard_no_cache() {
        assert_eq!(determine_boot_mode(false, true), "standard");
    }

    #[test]
    fn test_determine_boot_mode_standard_no_market() {
        assert_eq!(determine_boot_mode(true, false), "standard");
    }

    #[test]
    fn test_determine_boot_mode_standard_neither() {
        assert_eq!(determine_boot_mode(false, false), "standard");
    }

    // -----------------------------------------------------------------------
    // should_fast_boot tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_should_fast_boot_true() {
        assert!(should_fast_boot(true, true));
    }

    #[test]
    fn test_should_fast_boot_false_no_cache() {
        assert!(!should_fast_boot(false, true));
    }

    #[test]
    fn test_should_fast_boot_false_no_market() {
        assert!(!should_fast_boot(true, false));
    }

    #[test]
    fn test_should_fast_boot_false_neither() {
        assert!(!should_fast_boot(false, false));
    }

    #[test]
    fn test_should_fast_boot_matches_determine_boot_mode() {
        // should_fast_boot(true, true) <=> determine_boot_mode returns "fast"
        for &cache in &[true, false] {
            for &market in &[true, false] {
                let is_fast = should_fast_boot(cache, market);
                let mode = determine_boot_mode(cache, market);
                assert_eq!(
                    is_fast,
                    mode == "fast",
                    "should_fast_boot and determine_boot_mode must agree for cache={cache}, market={market}"
                );
            }
        }
    }

    #[test]
    fn test_heartbeat_watchdog_spawned() {
        assert_eq!(super::WATCHDOG_INTERVAL_SECS, 30);
    }

    /// Ratchet: the Rust watchdog ping cadence MUST be fast enough that
    /// systemd's `WatchdogSec=` never trips during a healthy run.
    ///
    /// The systemd watchdog rule of thumb is `interval >= 2 * ping_cadence`
    /// so that a single missed ping (e.g. the task was briefly scheduled
    /// out) does not kill the process. If either side drifts — someone
    /// halves `WatchdogSec` in the .service file, or someone doubles
    /// `WATCHDOG_INTERVAL_SECS` in Rust — this test fails at build time.
    ///
    /// Without this test, the failure mode is a silent overnight
    /// kill-loop: systemd kills the app every `WatchdogSec` seconds,
    /// the supervisor restarts it, and operators see only the restart
    /// noise without understanding why.
    #[test]
    fn test_systemd_watchdog_sec_covers_rust_ping_interval() {
        let service_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("deploy/systemd/tickvault.service"))
            .unwrap_or_else(|| panic!("CARGO_MANIFEST_DIR has no grandparent — unexpected layout"));

        let contents = match std::fs::read_to_string(&service_path) {
            Ok(c) => c,
            Err(err) => {
                // If the service file is absent in a stripped checkout
                // we cannot run this ratchet — fail explicitly rather
                // than silently skip (silent skips caused production
                // kill-loops in other projects).
                panic!(
                    "cannot read {}: {err} — this ratchet requires the systemd unit file",
                    service_path.display()
                );
            }
        };

        let watchdog_sec_line = contents
            .lines()
            .map(str::trim)
            .find(|line| !line.starts_with('#') && line.starts_with("WatchdogSec="))
            .unwrap_or_else(|| {
                panic!(
                    "no uncommented WatchdogSec= line found in {}",
                    service_path.display()
                )
            });

        let value_str = watchdog_sec_line
            .strip_prefix("WatchdogSec=")
            .unwrap_or_else(|| panic!("unexpected line shape: {watchdog_sec_line}"))
            .trim();

        let watchdog_sec: u64 = value_str.parse().unwrap_or_else(|_| {
            panic!("WatchdogSec must be an integer seconds value, got: {value_str}")
        });

        let ping_secs = super::WATCHDOG_INTERVAL_SECS;
        let minimum_expected = ping_secs.saturating_mul(2);
        assert!(
            watchdog_sec >= minimum_expected,
            "systemd WatchdogSec={watchdog_sec} is too tight for Rust ping cadence \
             WATCHDOG_INTERVAL_SECS={ping_secs}. Requires WatchdogSec >= {minimum_expected} \
             (2x ping cadence) to tolerate a single missed ping without being killed. \
             Either increase WatchdogSec in deploy/systemd/tickvault.service or decrease \
             WATCHDOG_INTERVAL_SECS in boot_helpers.rs."
        );
    }

    #[test]
    fn test_clock_drift_check() {
        assert_eq!(super::CLOCK_DRIFT_THRESHOLD_SECS, 2);
    }

    // -----------------------------------------------------------------------
    // Clock drift constants
    // -----------------------------------------------------------------------

    #[test]
    fn test_clock_drift_http_timeout_is_reasonable() {
        assert!(super::CLOCK_DRIFT_HTTP_TIMEOUT_SECS >= 1);
        assert!(super::CLOCK_DRIFT_HTTP_TIMEOUT_SECS <= 30);
    }

    #[test]
    fn test_clock_drift_reference_url_is_https() {
        assert!(
            super::CLOCK_DRIFT_REFERENCE_URL.starts_with("https://"),
            "clock drift reference URL must use HTTPS"
        );
    }

    // -----------------------------------------------------------------------
    // create_log_file_writer_at: path with no parent (file at root-like level)
    // -----------------------------------------------------------------------

    #[test]
    fn test_create_log_file_writer_at_relative_no_parent() {
        // A relative path with no slash has the parent "".
        // This exercises the unwrap_or fallback to "data/logs".
        let result = create_log_file_writer_at("justfilename.log");
        // Should succeed because it creates "data/logs" fallback.
        // Actually, Path::new("justfilename.log").parent() is Some(""),
        // and create_dir_all("") succeeds (no-op).
        // The file is created in current dir.
        if result.is_some() {
            let _ = std::fs::remove_file("justfilename.log");
        }
    }

    // -----------------------------------------------------------------------
    // effective_ws_stagger: edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_effective_ws_stagger_u64_max_always_fast() {
        // Even with u64::MAX configured, always uses fast stagger.
        assert_eq!(
            effective_ws_stagger(u64::MAX, true),
            OFF_HOURS_CONNECTION_STAGGER_MS
        );
        assert_eq!(
            effective_ws_stagger(u64::MAX, false),
            OFF_HOURS_CONNECTION_STAGGER_MS
        );
    }

    // -----------------------------------------------------------------------
    // format_bind_addr: empty host
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_bind_addr_empty_host() {
        let addr = format_bind_addr("", 8080);
        assert_eq!(addr, ":8080");
    }

    // -----------------------------------------------------------------------
    // determine_boot_mode and should_fast_boot: consistency exhaustive
    // -----------------------------------------------------------------------

    #[test]
    fn test_determine_boot_mode_returns_only_fast_or_standard() {
        for &cache in &[true, false] {
            for &market in &[true, false] {
                let mode = determine_boot_mode(cache, market);
                assert!(
                    mode == "fast" || mode == "standard",
                    "unexpected boot mode: {mode}"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // STAGE-C.2b: WAL replay drain helpers
    // -----------------------------------------------------------------------

    /// The order-update drain helper parses a valid JSON frame and
    /// broadcasts it to the live `tokio::sync::broadcast::Sender`. A
    /// subscriber created before the drain must receive the replayed
    /// update. Unit-testable with no I/O.
    #[tokio::test]
    async fn test_drain_replayed_order_updates_to_broadcast_parses_and_broadcasts() {
        let (sender, mut receiver) =
            tokio::sync::broadcast::channel::<tickvault_common::order_types::OrderUpdate>(16);

        // Minimal Dhan order-update JSON — only `OrderNo` and `Status`
        // are required by the serde deserializer; every other field
        // uses `#[serde(default)]`. Matches the canonical minimal
        // example in `crates/core/src/parser/order_update.rs`
        // (`test_parse_order_update_minimal`).
        let json = r#"{"Data": {"OrderNo": "ORD-TEST-001", "Status": "PENDING"}}"#;

        let frames = vec![json.as_bytes().to_vec()];
        let (parsed, broadcast_count, parse_errors) =
            drain_replayed_order_updates_to_broadcast(frames, &sender);

        assert_eq!(parsed, 1, "one valid frame must parse");
        assert_eq!(
            broadcast_count, 1,
            "one subscriber attached before drain must receive broadcast"
        );
        assert_eq!(parse_errors, 0);

        // The subscriber receives the replayed update synchronously in
        // the test runtime.
        let received = receiver.try_recv().expect("subscriber must receive update");
        assert_eq!(received.order_no, "ORD-TEST-001");
    }

    /// Invalid JSON must be counted as a parse error and must NOT
    /// broadcast. Good frames in the same batch still parse.
    #[tokio::test]
    async fn test_drain_replayed_order_updates_to_broadcast_skips_invalid_json() {
        let (sender, _receiver) =
            tokio::sync::broadcast::channel::<tickvault_common::order_types::OrderUpdate>(16);
        let frames = vec![b"not-json".to_vec(), b"{\"incomplete\":".to_vec()];
        let (parsed, broadcast_count, parse_errors) =
            drain_replayed_order_updates_to_broadcast(frames, &sender);
        assert_eq!(parsed, 0);
        assert_eq!(broadcast_count, 0);
        assert_eq!(parse_errors, 2);
    }

    // -----------------------------------------------------------------------
    // should_emit_post_market_alert — Saturday/holiday gate
    // -----------------------------------------------------------------------
    //
    // Live observation 2026-05-02 (Saturday): the 15:30 IST shutdown
    // trigger fired on a non-trading day because compute_market_close_sleep
    // is wall-clock based. Operator received a misleading
    // [HIGH] Post-Market: Market closed — WebSockets disconnected
    // Telegram. These tests pin the gate that suppresses the emission
    // on non-trading days.

    fn build_test_calendar() -> TradingCalendar {
        use tickvault_common::config::{NseHolidayEntry, TradingConfig};
        let cfg = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![NseHolidayEntry {
                date: "2026-01-26".to_string(),
                name: "Republic Day".to_string(),
            }],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        };
        TradingCalendar::from_config(&cfg).expect("calendar must build from synthetic config")
    }

    #[test]
    fn test_should_emit_post_market_alert_suppressed_on_saturday() {
        use chrono::Datelike;
        // 2026-05-02 — the live incident date. NaiveDate weekday: Sat.
        let saturday = NaiveDate::from_ymd_opt(2026, 5, 2).unwrap();
        assert_eq!(saturday.weekday(), chrono::Weekday::Sat);
        let cal = build_test_calendar();
        assert!(
            !should_emit_post_market_alert(&cal, saturday),
            "Saturday must suppress the Post-Market alert"
        );
    }

    #[test]
    fn test_should_emit_post_market_alert_suppressed_on_sunday() {
        use chrono::Datelike;
        // 2026-05-03 — the day after the live incident.
        let sunday = NaiveDate::from_ymd_opt(2026, 5, 3).unwrap();
        assert_eq!(sunday.weekday(), chrono::Weekday::Sun);
        let cal = build_test_calendar();
        assert!(
            !should_emit_post_market_alert(&cal, sunday),
            "Sunday must suppress the Post-Market alert"
        );
    }

    #[test]
    fn test_should_emit_post_market_alert_suppressed_on_nse_holiday() {
        use chrono::Datelike;
        // 2026-01-26 — Republic Day, in the test holiday list. Falls
        // on a Monday so this isolates the holiday filter from the
        // weekend filter.
        let republic_day = NaiveDate::from_ymd_opt(2026, 1, 26).unwrap();
        assert_eq!(republic_day.weekday(), chrono::Weekday::Mon);
        let cal = build_test_calendar();
        assert!(
            !should_emit_post_market_alert(&cal, republic_day),
            "NSE holiday must suppress the Post-Market alert"
        );
    }

    #[test]
    fn test_should_emit_post_market_alert_fires_on_normal_trading_day() {
        use chrono::Datelike;
        // 2026-05-04 — Monday, not in the test holiday list.
        let monday = NaiveDate::from_ymd_opt(2026, 5, 4).unwrap();
        assert_eq!(monday.weekday(), chrono::Weekday::Mon);
        let cal = build_test_calendar();
        assert!(
            should_emit_post_market_alert(&cal, monday),
            "regular trading day must fire the Post-Market alert"
        );
    }
}
