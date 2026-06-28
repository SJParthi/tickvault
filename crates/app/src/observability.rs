//! Observability initialization — Prometheus metrics + OpenTelemetry tracing.
//!
//! Initializes two subsystems at boot:
//! 1. **Prometheus metrics** — HTTP endpoint on `:metrics_port` for Prometheus to scrape.
//!    Uses the `metrics` facade crate, so all crates emit metrics via `metrics::counter!()` etc.
//! 2. **OpenTelemetry tracing** — OTLP gRPC export for distributed traces.
//!    Returns a tracing layer that bridges `tracing` spans → OpenTelemetry → OTLP collector.
//!
//! # Hot Path Compliance
//! - Prometheus exporter: `metrics::counter!()` is O(1) after first registration (cached key).
//! - OpenTelemetry layer: only exports spans on drop, no allocation per-event on hot path.

use std::path::PathBuf;

use anyhow::{Context, Result};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use opentelemetry::trace::TracerProvider;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_opentelemetry::OpenTelemetryLayer;

use tickvault_common::config::ObservabilityConfig;

/// Directory that holds the structured `errors.jsonl` stream.
///
/// Relative to the process working directory. In Docker + AWS the compose
/// stack mounts `./data/logs` so the file survives restarts and can be
/// tailed by Alloy/Loki or by the Claude triage daemon.
pub const ERRORS_JSONL_DIR: &str = "data/logs";

/// File-name prefix for the rolling FULL-app JSON log appender.
///
/// Mirror of `ERRORS_JSONL_PREFIX` for the all-levels app log. `RollingFileAppender`
/// with `Rotation::HOURLY` produces files named `app.{YYYY-MM-DD-HH}`:
///   data/logs/app.2026-05-02-04
///   data/logs/app.2026-05-02-05
///   ...
///
/// Hourly chunks bound any single file's size for industry-standard
/// log analysis tooling (a daily file would routinely exceed 100 MB
/// during market hours, breaking IDE / `less` / `grep` ergonomics).
pub const APP_LOG_PREFIX: &str = "app";

/// 2026-05-02 — per-category log file separation.
///
/// Operator-requested separation of logs into domain-specific
/// directories so candles / live ticks / historical / option chain
/// logs can be tailed independently without grep'ing the giant
/// `app.*` stream.
///
/// Each category gets its own subdirectory under `data/logs/` with
/// hourly-rotated files named `{prefix}.{YYYY-MM-DD-HH}`. The targets
/// filter for each category is built by [`build_category_targets`].
///
/// 2026-05-09 — `LogCategory::Movers` retired (operator directive
/// "Also retire LogCategory::Movers" after the QuestDB movers
/// infrastructure was dropped in PR #539). The 4 RAM-tracker tracing
/// targets (`mover_classifier`, `movers_window`, `option_movers`,
/// `top_movers`) are now folded into `LogCategory::LiveTicks`. The
/// existing on-disk `data/logs/movers/` directory is orphaned;
/// operator deletes manually.
pub const CATEGORY_CANDLES_DIR: &str = "data/logs/candles";
pub const CATEGORY_CANDLES_PREFIX: &str = "candles";
pub const CATEGORY_LIVE_TICKS_DIR: &str = "data/logs/live_ticks";
pub const CATEGORY_LIVE_TICKS_PREFIX: &str = "live_ticks";
// PR-D (2026-05-26): CATEGORY_HISTORICAL_* constants retired alongside
// the deleted Dhan historical fetch chain.
// 2026-06-28: CATEGORY_OPTION_CHAIN_* + LogCategory::OptionChain retired
// with the entire option_chain REST subsystem (operator directive
// 2026-06-28 — disabled since 2026-06-02 with no live consumer).

/// Stable identifier for the log categories.
///
/// Used by [`build_category_targets`] + the boot-time appender wiring.
/// The `&'static str` form is what `RollingFileAppender::new` expects
/// for the filename prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogCategory {
    Candles,
    LiveTicks,
}

impl LogCategory {
    #[must_use]
    pub fn dir(self) -> &'static str {
        match self {
            Self::Candles => CATEGORY_CANDLES_DIR,
            Self::LiveTicks => CATEGORY_LIVE_TICKS_DIR,
        }
    }

    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Candles => CATEGORY_CANDLES_PREFIX,
            Self::LiveTicks => CATEGORY_LIVE_TICKS_PREFIX,
        }
    }

    /// Every variant. Used by retention sweepers + tests.
    #[must_use]
    pub fn all() -> [Self; 2] {
        [Self::Candles, Self::LiveTicks]
    }
}

/// Returns the list of `tracing` `target` prefixes routed to the given
/// category. Each entry is a Rust module path verified against the live
/// `crates/{core,storage,trading}/src/` tree on 2026-05-02.
///
/// `tracing_subscriber::filter::Targets` does prefix matching on `target`
/// by default, so an entry like `tickvault_core::pipeline::candle_aggregator`
/// matches that exact module's logs.
#[must_use]
pub fn build_category_targets(cat: LogCategory) -> &'static [&'static str] {
    match cat {
        LogCategory::Candles => &[
            "tickvault_core::pipeline::candle_aggregator",
            // PR-E (2026-05-26): tickvault_storage::candle_persistence retired.
            "tickvault_storage::materialized_views",
        ],
        // 2026-05-09: the 4 RAM-tracker targets formerly under
        // `LogCategory::Movers` (mover_classifier, movers_window,
        // option_movers, top_movers) are folded into LiveTicks since
        // they run inline with the tick processor and the QuestDB
        // movers infrastructure was retired in PR #539.
        LogCategory::LiveTicks => &[
            "tickvault_core::websocket",
            "tickvault_core::pipeline::tick_processor",
            "tickvault_core::pipeline::tick_gap_detector",
            "tickvault_core::pipeline::no_tick_watchdog",
            "tickvault_core::pipeline::depth_sequence_tracker",
            "tickvault_core::pipeline::volume_monotonicity_guard",
            "tickvault_core::pipeline::mover_classifier",
            "tickvault_core::pipeline::movers_window",
            "tickvault_core::pipeline::option_movers",
            "tickvault_core::pipeline::top_movers",
            "tickvault_storage::tick_persistence",
            "tickvault_storage::tick_spill_drain",
        ],
        // PR-D (2026-05-26): LogCategory::Historical variant retired
        // alongside the deleted Dhan historical fetch chain.
        // 2026-06-28: LogCategory::OptionChain variant retired with the
        // deleted option_chain REST subsystem.
    }
}

/// File-name prefix for the rolling ERROR-only JSONL appender.
///
/// `RollingFileAppender` with `Rotation::HOURLY` produces files named
/// `{prefix}.{YYYY-MM-DD-HH}`, so the set on disk looks like:
///   data/logs/errors.jsonl.2026-04-18-09
///   data/logs/errors.jsonl.2026-04-18-10
///   ...
///
/// The bare filename `errors.jsonl` is kept as a compatibility symlink
/// (future enhancement) so human operators can `tail -F data/logs/errors.jsonl`.
pub const ERRORS_JSONL_PREFIX: &str = "errors.jsonl";

/// Bucket boundaries (upper bounds) for nanosecond-scale duration histograms.
///
/// Without these, `metrics-exporter-prometheus` renders histograms as
/// Prometheus **summaries** — which lack the `_bucket` series that
/// `histogram_quantile(...)` Grafana queries require, producing "No data".
///
/// Range: 100 ns → 10 s, logarithmic spacing. Covers:
/// - Tick parse (<1 µs)
/// - Full tick processing (~10 µs target, alert at >100 µs)
/// - Pipeline stalls (>10 ms indicates backpressure)
/// - Wire-to-done (>1 s indicates a broken socket)
const TICK_NS_HISTOGRAM_BUCKETS: &[f64] = &[
    100.0,            // 100 ns
    500.0,            // 500 ns
    1_000.0,          // 1 µs
    5_000.0,          // 5 µs
    10_000.0,         // 10 µs — zero-allocation hot-path budget
    50_000.0,         // 50 µs
    100_000.0,        // 100 µs
    500_000.0,        // 500 µs
    1_000_000.0,      // 1 ms
    10_000_000.0,     // 10 ms
    100_000_000.0,    // 100 ms
    1_000_000_000.0,  // 1 s
    10_000_000_000.0, // 10 s
];

/// Bucket boundaries (upper bounds in milliseconds) for REST + DB latency
/// histograms ending in `_ms` (e.g., `tv_api_request_duration_ms`,
/// `tv_questdb_liveness_latency_ms`, `tv_phase2_run_ms`). Same rationale
/// as the `_ns` buckets: without these the exporter emits summaries and
/// Grafana `histogram_quantile` queries show "No data".
///
/// Range: 1 ms → 60 s, roughly log-spaced.
const API_MS_HISTOGRAM_BUCKETS: &[f64] = &[
    1.0,      // 1 ms
    5.0,      // 5 ms
    10.0,     // 10 ms
    25.0,     // 25 ms
    50.0,     // 50 ms
    100.0,    // 100 ms
    250.0,    // 250 ms
    500.0,    // 500 ms
    1_000.0,  // 1 s — Dhan rate-limit window
    2_500.0,  // 2.5 s
    5_000.0,  // 5 s
    10_000.0, // 10 s
    30_000.0, // 30 s
    60_000.0, // 60 s
];

/// Initializes the Prometheus metrics exporter.
///
/// Starts an HTTP server on `0.0.0.0:{metrics_port}` that serves `/metrics`
/// in Prometheus exposition format. Installs itself as the global `metrics` recorder.
///
/// After this call, any crate can emit metrics via `metrics::counter!()`,
/// `metrics::gauge!()`, `metrics::histogram!()`.
pub fn init_metrics(config: &ObservabilityConfig) -> Result<()> {
    if !config.metrics_enabled {
        info!("Prometheus metrics disabled by config");
        return Ok(());
    }

    // L123 (Wave-5 plan §AA, SEC-H1): bind to the configured address
    // (default `127.0.0.1` per `ObservabilityConfig::default_metrics_bind_addr`).
    // Previously hard-coded to `0.0.0.0` which exposed
    // `tv_subsystem_memory_estimated_bytes` (and every other gauge) to
    // any peer in the VPC.
    PrometheusBuilder::new()
        .with_http_listener(std::net::SocketAddr::new(
            config.metrics_bind_addr,
            config.metrics_port,
        ))
        // Force every `*_duration_ns` histogram to render as a Prometheus
        // histogram (with `_bucket` series) instead of the exporter's
        // default summary. Grafana's `histogram_quantile(rate(*_bucket))`
        // queries need `_bucket` to exist — without this, the latency
        // panels show "No data" forever.
        .set_buckets_for_metric(
            Matcher::Suffix("_duration_ns".to_string()),
            TICK_NS_HISTOGRAM_BUCKETS,
        )
        .context("failed to set histogram buckets for _duration_ns metrics")?
        .set_buckets_for_metric(Matcher::Suffix("_ms".to_string()), API_MS_HISTOGRAM_BUCKETS)
        .context("failed to set histogram buckets for _ms metrics")?
        .install()
        .context("failed to install Prometheus metrics exporter")?;

    info!(
        bind_addr = %config.metrics_bind_addr,
        port = config.metrics_port,
        "Prometheus metrics exporter started"
    );
    Ok(())
}

/// Initializes the OpenTelemetry tracing pipeline.
///
/// Returns a tuple of (OpenTelemetryLayer, SdkTracerProvider). The layer is composed
/// with the tracing subscriber. The provider must be stored and shut down on exit.
///
/// The layer is generic over `S` so it can be composed with any subscriber stack.
///
/// Returns `None` if tracing is disabled via config.
pub fn init_tracing<S>(
    config: &ObservabilityConfig,
) -> Result<
    Option<(
        OpenTelemetryLayer<S, opentelemetry_sdk::trace::SdkTracer>,
        SdkTracerProvider,
    )>,
>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    if !config.tracing_enabled {
        info!("OpenTelemetry tracing disabled by config");
        return Ok(None);
    }

    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&config.otlp_endpoint)
        .build()
        .context("failed to build OTLP span exporter")?;

    let resource = Resource::builder().with_service_name("tickvault").build();

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer("tickvault");
    let layer = OpenTelemetryLayer::new(tracer);

    info!(
        endpoint = &config.otlp_endpoint,
        "OpenTelemetry tracing pipeline started"
    );

    Ok(Some((layer, provider)))
}

/// Initializes the ERROR-only JSON-per-line file appender.
///
/// Returns a `Layer` that can be composed into the tracing subscriber stack
/// plus a `WorkerGuard` that MUST be kept alive for the duration of the
/// process — dropping it stops the background flush thread and can lose
/// buffered events.
///
/// File on disk: `{dir}/{prefix}.{YYYY-MM-DD-HH}` rotated hourly.
///
/// The caller is responsible for the 48-hour retention sweep (typically a
/// tokio task that runs every hour and deletes files with mtime older than
/// 48h). Keeping retention outside this function lets tests call
/// `init_errors_jsonl_appender` without filesystem side effects.
pub fn init_errors_jsonl_appender(
    dir: impl Into<PathBuf>,
) -> Result<(tracing_appender::non_blocking::NonBlocking, WorkerGuard)> {
    let dir: PathBuf = dir.into();
    std::fs::create_dir_all(&dir).with_context(|| {
        format!(
            "failed to create errors.jsonl directory at {}",
            dir.display()
        )
    })?;

    let appender = RollingFileAppender::new(Rotation::HOURLY, &dir, ERRORS_JSONL_PREFIX);
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    info!(
        dir = %dir.display(),
        prefix = ERRORS_JSONL_PREFIX,
        rotation = "hourly",
        "errors.jsonl appender initialized"
    );

    Ok((non_blocking, guard))
}

/// Initializes the rolling all-levels app log appender (hourly).
///
/// Mirror of [`init_errors_jsonl_appender`] for the full app log. Returns
/// `(NonBlocking, WorkerGuard)` for composition into the tracing
/// subscriber. The `WorkerGuard` MUST be kept alive for the duration of
/// the process — dropping it stops the background flush thread and can
/// lose buffered events.
///
/// File on disk: `{dir}/app.{YYYY-MM-DD-HH}` rotated hourly. Replaces the
/// previous daily-but-actually-static `app.YYYY-MM-DD.log` writer that
/// kept the same file open for a full trading day, producing 50–100 MB
/// single-file logs that broke standard ops tooling.
///
/// The caller is responsible for the retention sweep (typically a
/// tokio task that runs every hour and deletes files older than N hours).
/// See [`sweep_app_log_retention`].
pub fn init_app_log_appender(
    dir: impl Into<PathBuf>,
) -> Result<(tracing_appender::non_blocking::NonBlocking, WorkerGuard)> {
    let dir: PathBuf = dir.into();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create app log directory at {}", dir.display()))?;

    let appender = RollingFileAppender::new(Rotation::HOURLY, &dir, APP_LOG_PREFIX);
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    info!(
        dir = %dir.display(),
        prefix = APP_LOG_PREFIX,
        rotation = "hourly",
        "app log appender initialized"
    );

    Ok((non_blocking, guard))
}

/// 2026-05-02 — generic per-category log appender. Mirrors
/// [`init_app_log_appender`] but parameterized over (`dir`, `prefix`)
/// so the 5 [`LogCategory`] variants can each get their own
/// hourly-rotated stream without code duplication.
///
/// File on disk: `{dir}/{prefix}.{YYYY-MM-DD-HH}` rotated hourly.
///
/// The caller MUST keep the `WorkerGuard` alive for the process
/// lifetime — dropping it stops the background flush thread (same
/// constraint as `init_app_log_appender` + `init_errors_jsonl_appender`).
pub fn init_category_log_appender(
    dir: impl Into<PathBuf>,
    prefix: &'static str,
) -> Result<(tracing_appender::non_blocking::NonBlocking, WorkerGuard)> {
    let dir: PathBuf = dir.into();
    std::fs::create_dir_all(&dir).with_context(|| {
        format!(
            "failed to create category log directory at {}",
            dir.display()
        )
    })?;

    let appender = RollingFileAppender::new(Rotation::HOURLY, &dir, prefix);
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    info!(
        dir = %dir.display(),
        prefix,
        rotation = "hourly",
        "category log appender initialized"
    );

    Ok((non_blocking, guard))
}

/// Sweeps a category log directory using the same age-based mtime
/// criterion as [`sweep_app_log_retention`]. Generic over `prefix` so
/// the 5 categories share one implementation.
pub fn sweep_category_log_retention(
    dir: &std::path::Path,
    prefix: &str,
    retention_hours: u64,
) -> std::io::Result<usize> {
    let now = std::time::SystemTime::now();
    let cutoff = std::time::Duration::from_secs(retention_hours.saturating_mul(3600));
    let mut deleted = 0usize;

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // Match `{prefix}` exactly OR `{prefix}.{anything}` produced by
        // the rolling appender. Anything else in the dir is left alone.
        if name != prefix && !name.starts_with(&format!("{prefix}.")) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = match metadata.modified() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let age = now.duration_since(modified).unwrap_or_default();
        if age > cutoff && std::fs::remove_file(&path).is_ok() {
            deleted = deleted.saturating_add(1);
        }
    }

    Ok(deleted)
}

/// Deletes `app.YYYY-MM-DD-HH` files under `dir` whose mtime is older
/// than `retention_hours`. Mirror of [`sweep_errors_jsonl_retention`].
///
/// Typical usage: tokio task that calls this every 3600s with
/// `retention_hours = 168` (= 7 days, matching the prior daily-file
/// retention semantic of "keep 7 daily files"). Returns the number of
/// files deleted. Best-effort — never blocks the app.
pub fn sweep_app_log_retention(
    dir: &std::path::Path,
    retention_hours: u64,
) -> std::io::Result<usize> {
    let now = std::time::SystemTime::now();
    let cutoff = std::time::Duration::from_secs(retention_hours.saturating_mul(3600));
    let mut deleted = 0usize;

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // Match `app` OR `app.YYYY-MM-DD-HH` produced by the rolling
        // appender. Skip `app.log` (legacy fixed name kept for the Alloy
        // file mount) — owned by `infra.rs::ensure_app_log_exists`.
        if name == "app.log" {
            continue;
        }
        if !name.starts_with(APP_LOG_PREFIX) {
            continue;
        }
        // Don't accidentally sweep `app_<other-prefix>` files; require
        // the rolling-suffix dot or exact prefix match.
        if name.len() > APP_LOG_PREFIX.len() && !name[APP_LOG_PREFIX.len()..].starts_with('.') {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(mtime) else {
            continue;
        };
        if age > cutoff {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    deleted = deleted.saturating_add(1);
                }
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        path = %path.display(),
                        "app log retention sweep: remove_file failed"
                    );
                }
            }
        }
    }
    Ok(deleted)
}

/// Deletes `errors.jsonl.*` files under `dir` whose mtime is older than
/// `retention_hours`.
///
/// Typical usage: tokio task that calls this every 3600s with
/// `retention_hours = 48`. Returns the number of files deleted.
/// Swallows individual `remove_file` errors (logs them at WARN) and
/// continues — best-effort cleanup, never blocks the app.
pub fn sweep_errors_jsonl_retention(
    dir: &std::path::Path,
    retention_hours: u64,
) -> std::io::Result<usize> {
    let now = std::time::SystemTime::now();
    let cutoff = std::time::Duration::from_secs(retention_hours.saturating_mul(3600));
    let mut deleted = 0usize;

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // Only touch files matching the rolling appender's naming convention:
        // `errors.jsonl` OR `errors.jsonl.YYYY-MM-DD-HH`.
        if !name.starts_with(ERRORS_JSONL_PREFIX) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(mtime) else {
            continue;
        };
        if age > cutoff {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    deleted = deleted.saturating_add(1);
                }
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        path = %path.display(),
                        "errors.jsonl retention sweep: remove_file failed"
                    );
                }
            }
        }
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::config::ObservabilityConfig;

    fn disabled_config() -> ObservabilityConfig {
        ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 0,
            otlp_endpoint: String::new(),
            metrics_enabled: false,
            tracing_enabled: false,
        }
    }

    /// Regression: both bucket-boundary arrays must be non-empty and
    /// monotonically increasing. `set_buckets_for_metric` returns
    /// `BuildError::EmptyBucketsOrQuantiles` on empty arrays, which
    /// would silently kill `init_metrics` at boot.
    #[test]
    fn test_histogram_buckets_are_non_empty_and_monotonic() {
        for (name, buckets) in [
            ("TICK_NS_HISTOGRAM_BUCKETS", TICK_NS_HISTOGRAM_BUCKETS),
            ("API_MS_HISTOGRAM_BUCKETS", API_MS_HISTOGRAM_BUCKETS),
        ] {
            assert!(!buckets.is_empty(), "{name} must not be empty");
            for window in buckets.windows(2) {
                assert!(
                    window[0] < window[1],
                    "{name} must be strictly increasing: {} not < {}",
                    window[0],
                    window[1]
                );
            }
        }
    }

    #[test]
    fn init_metrics_disabled_returns_ok() {
        let config = disabled_config();
        let result = init_metrics(&config);
        assert!(result.is_ok(), "disabled metrics should return Ok");
    }

    #[test]
    fn init_tracing_disabled_returns_none() {
        let config = disabled_config();
        let result = init_tracing::<tracing_subscriber::Registry>(&config);
        assert!(result.is_ok(), "disabled tracing should return Ok");
        assert!(
            result.unwrap().is_none(),
            "disabled tracing should return None"
        );
    }

    // -----------------------------------------------------------------------
    // Config field validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn disabled_config_has_zeroed_port() {
        let config = disabled_config();
        assert_eq!(config.metrics_port, 0, "disabled config should have port 0");
    }

    #[test]
    fn disabled_config_has_empty_endpoint() {
        let config = disabled_config();
        assert!(
            config.otlp_endpoint.is_empty(),
            "disabled config should have empty OTLP endpoint"
        );
    }

    #[test]
    fn disabled_config_flags_are_both_false() {
        let config = disabled_config();
        assert!(!config.metrics_enabled, "metrics must be disabled");
        assert!(!config.tracing_enabled, "tracing must be disabled");
    }

    // -----------------------------------------------------------------------
    // Default ObservabilityConfig tests
    // -----------------------------------------------------------------------

    #[test]
    fn default_observability_config_has_metrics_enabled() {
        let config = ObservabilityConfig::default();
        assert!(
            config.metrics_enabled,
            "default config should have metrics enabled"
        );
    }

    #[test]
    fn default_observability_config_has_tracing_enabled() {
        let config = ObservabilityConfig::default();
        assert!(
            config.tracing_enabled,
            "default config should have tracing enabled"
        );
    }

    #[test]
    fn default_observability_config_binds_to_loopback_per_l123() {
        // L123 / SEC-H1: the new per-component memory gauge must NOT
        // be reachable from any peer in the VPC by default. Anyone
        // changing this default must update the alert / dashboard /
        // disaster-recovery docs in the same PR.
        let config = ObservabilityConfig::default();
        assert_eq!(
            config.metrics_bind_addr,
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            "L123: default metrics bind must be 127.0.0.1 (loopback)"
        );
    }

    #[test]
    fn default_metrics_bind_addr_helper_returns_loopback() {
        // L123 ratchet: the helper used by `#[serde(default)]` must
        // return loopback so config files that omit `metrics_bind_addr`
        // inherit the safe default.
        assert!(
            ObservabilityConfig::default_metrics_bind_addr().is_loopback(),
            "default_metrics_bind_addr() must return a loopback address"
        );
    }

    #[test]
    fn default_observability_config_has_valid_port() {
        let config = ObservabilityConfig::default();
        assert!(
            config.metrics_port > 0,
            "default metrics port must be positive"
        );
    }

    #[test]
    fn default_observability_config_has_nonempty_endpoint() {
        let config = ObservabilityConfig::default();
        assert!(
            !config.otlp_endpoint.is_empty(),
            "default OTLP endpoint must not be empty"
        );
    }

    // -----------------------------------------------------------------------
    // Enabled config validation (doesn't actually start — just verify config shape)
    // -----------------------------------------------------------------------

    #[test]
    fn enabled_config_can_be_constructed() {
        let config = ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 9091,
            otlp_endpoint: "http://localhost:4317".to_string(),
            metrics_enabled: true,
            tracing_enabled: true,
        };
        assert!(config.metrics_enabled);
        assert!(config.tracing_enabled);
        assert_eq!(config.metrics_port, 9091);
        assert!(!config.otlp_endpoint.is_empty());
    }

    // -----------------------------------------------------------------------
    // Additional coverage: metrics-only disabled, tracing-only disabled,
    // various config combinations
    // -----------------------------------------------------------------------

    #[test]
    fn init_metrics_disabled_tracing_enabled_only_metrics_returns_ok() {
        let config = ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 0,
            otlp_endpoint: "http://localhost:4317".to_string(),
            metrics_enabled: false,
            tracing_enabled: true,
        };
        // init_metrics should return Ok immediately when disabled
        let result = init_metrics(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn init_tracing_disabled_metrics_enabled_returns_none() {
        let config = ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 9091,
            otlp_endpoint: "http://localhost:4317".to_string(),
            metrics_enabled: true,
            tracing_enabled: false,
        };
        let result = init_tracing::<tracing_subscriber::Registry>(&config);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn init_tracing_enabled_with_unreachable_endpoint_builds_ok() {
        // Even with an unreachable endpoint, build should succeed
        // (tonic/OTLP connects lazily, not at build time).
        // Requires tokio runtime because tonic::Channel::new needs it.
        let config = ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 0,
            otlp_endpoint: "http://nonexistent-host:99999".to_string(),
            metrics_enabled: false,
            tracing_enabled: true,
        };
        let result = init_tracing::<tracing_subscriber::Registry>(&config);
        // Should succeed (lazy connection)
        assert!(result.is_ok());
        if let Ok(Some((_layer, provider))) = result {
            // Clean up to avoid background exporter leaks
            drop(provider);
        }
    }

    #[test]
    fn observability_config_clone() {
        let config = ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 9091,
            otlp_endpoint: "http://test:4317".to_string(),
            metrics_enabled: true,
            tracing_enabled: false,
        };
        let cloned = config.clone();
        assert_eq!(cloned.metrics_port, 9091);
        assert_eq!(cloned.otlp_endpoint, "http://test:4317");
        assert!(cloned.metrics_enabled);
        assert!(!cloned.tracing_enabled);
    }

    #[test]
    fn observability_config_debug() {
        let config = ObservabilityConfig::default();
        let debug = format!("{config:?}");
        assert!(debug.contains("ObservabilityConfig"));
        assert!(debug.contains("metrics_port"));
    }

    #[test]
    fn init_metrics_disabled_is_idempotent() {
        let config = disabled_config();
        // Multiple calls should all succeed
        assert!(init_metrics(&config).is_ok());
        assert!(init_metrics(&config).is_ok());
    }

    #[test]
    fn init_tracing_disabled_is_idempotent() {
        let config = disabled_config();
        let r1 = init_tracing::<tracing_subscriber::Registry>(&config);
        let r2 = init_tracing::<tracing_subscriber::Registry>(&config);
        assert!(r1.is_ok());
        assert!(r2.is_ok());
        assert!(r1.unwrap().is_none());
        assert!(r2.unwrap().is_none());
    }

    // -----------------------------------------------------------------------
    // Config combination tests
    // -----------------------------------------------------------------------

    #[test]
    fn metrics_disabled_tracing_disabled_both_return_ok_none() {
        let config = ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 0,
            otlp_endpoint: String::new(),
            metrics_enabled: false,
            tracing_enabled: false,
        };
        assert!(init_metrics(&config).is_ok());
        let tracing_result = init_tracing::<tracing_subscriber::Registry>(&config);
        assert!(tracing_result.is_ok());
        assert!(tracing_result.unwrap().is_none());
    }

    #[test]
    fn config_with_custom_port_preserves_value() {
        let config = ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 9999,
            otlp_endpoint: "http://custom:4317".to_string(),
            metrics_enabled: false,
            tracing_enabled: false,
        };
        assert_eq!(config.metrics_port, 9999);
        assert_eq!(config.otlp_endpoint, "http://custom:4317");
    }

    #[test]
    fn config_with_port_zero_is_valid_when_disabled() {
        let config = ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 0,
            otlp_endpoint: String::new(),
            metrics_enabled: false,
            tracing_enabled: false,
        };
        // Port 0 is fine when metrics are disabled
        assert!(init_metrics(&config).is_ok());
    }

    // -----------------------------------------------------------------------
    // init_tracing enabled path — builds OTLP exporter lazily
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn init_tracing_enabled_with_localhost_builds_ok() {
        // OTLP exporter connects lazily — build should succeed even with
        // unreachable endpoints. This exercises the full pipeline build.
        let config = ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 0,
            otlp_endpoint: "http://127.0.0.1:4317".to_string(),
            metrics_enabled: false,
            tracing_enabled: true,
        };
        let result = init_tracing::<tracing_subscriber::Registry>(&config);
        assert!(
            result.is_ok(),
            "tracing build should succeed with lazy connection"
        );
        if let Ok(Some((_layer, provider))) = result {
            drop(provider);
        }
    }

    #[tokio::test]
    async fn init_tracing_enabled_returns_some() {
        let config = ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 0,
            otlp_endpoint: "http://localhost:4317".to_string(),
            metrics_enabled: false,
            tracing_enabled: true,
        };
        let result = init_tracing::<tracing_subscriber::Registry>(&config);
        assert!(result.is_ok());
        let inner = result.unwrap();
        assert!(
            inner.is_some(),
            "enabled tracing must return Some((layer, provider))"
        );
        if let Some((_layer, provider)) = inner {
            drop(provider);
        }
    }

    // -----------------------------------------------------------------------
    // Default config assertions — deeper validation
    // -----------------------------------------------------------------------

    #[test]
    fn default_config_port_is_above_reserved_range() {
        let config = ObservabilityConfig::default();
        // Common Prometheus exporter ports: 9090-9099, 9100+
        assert!(
            config.metrics_port >= 1024,
            "default port must be above reserved range"
        );
    }

    #[test]
    fn default_config_endpoint_starts_with_http() {
        let config = ObservabilityConfig::default();
        assert!(
            config.otlp_endpoint.starts_with("http://")
                || config.otlp_endpoint.starts_with("https://"),
            "OTLP endpoint must use HTTP/HTTPS protocol"
        );
    }

    #[test]
    fn default_config_endpoint_contains_port() {
        let config = ObservabilityConfig::default();
        // OTLP endpoint should contain a port (e.g., :4317)
        assert!(
            config.otlp_endpoint.contains(':'),
            "OTLP endpoint should contain port separator"
        );
    }

    // -----------------------------------------------------------------------
    // Config serialization/deserialization consistency
    // -----------------------------------------------------------------------

    #[test]
    fn config_partial_eq_via_field_comparison() {
        let c1 = ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 9091,
            otlp_endpoint: "http://test:4317".to_string(),
            metrics_enabled: true,
            tracing_enabled: true,
        };
        let c2 = c1.clone();
        assert_eq!(c1.metrics_port, c2.metrics_port);
        assert_eq!(c1.otlp_endpoint, c2.otlp_endpoint);
        assert_eq!(c1.metrics_enabled, c2.metrics_enabled);
        assert_eq!(c1.tracing_enabled, c2.tracing_enabled);
    }

    // -----------------------------------------------------------------------
    // init_metrics enabled path — attempts real Prometheus install
    // -----------------------------------------------------------------------

    #[test]
    fn init_metrics_enabled_installs_exporter() {
        // Use a random high port to avoid conflicts with other tests.
        // Port 0 means the OS picks an available port, but PrometheusBuilder
        // requires an explicit port. Use a high ephemeral port.
        let config = ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 19_091,
            otlp_endpoint: String::new(),
            metrics_enabled: true,
            tracing_enabled: false,
        };
        let result = init_metrics(&config);
        // First call should succeed — installs the global recorder.
        // Subsequent calls may fail because the global recorder is already set.
        // Either outcome is acceptable.
        let _ = result;
    }

    #[test]
    fn init_metrics_enabled_second_call_handles_already_installed() {
        // The global metrics recorder can only be installed once.
        // After the first successful install (in any test), subsequent
        // installs should return an error (not panic).
        let config = ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 19_092,
            otlp_endpoint: String::new(),
            metrics_enabled: true,
            tracing_enabled: false,
        };
        let result = init_metrics(&config);
        // May succeed or fail — just verify no panic.
        let _ = result;
    }

    #[test]
    fn disabled_config_metrics_short_circuits() {
        // Verifying the early return: disabled metrics should NOT attempt
        // to bind a port. The test would fail if it tried to bind port 0
        // and left a dangling listener.
        let config = ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 0,
            otlp_endpoint: String::new(),
            metrics_enabled: false,
            tracing_enabled: false,
        };
        // Should succeed instantly without side effects
        let start = std::time::Instant::now();
        let result = init_metrics(&config);
        let elapsed = start.elapsed();
        assert!(result.is_ok());
        assert!(
            elapsed.as_millis() < 100,
            "disabled metrics should return immediately"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 2: errors.jsonl appender tests
    // -----------------------------------------------------------------------

    #[test]
    fn errors_jsonl_prefix_and_dir_constants_are_stable() {
        assert_eq!(ERRORS_JSONL_PREFIX, "errors.jsonl");
        assert_eq!(ERRORS_JSONL_DIR, "data/logs");
    }

    #[test]
    fn init_errors_jsonl_appender_creates_directory() {
        let tmp = std::env::temp_dir().join(format!("tv-errors-jsonl-test-{}", std::process::id()));
        // Clean slate
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(!tmp.exists());

        let result = init_errors_jsonl_appender(&tmp);
        assert!(result.is_ok(), "appender init should succeed");
        assert!(tmp.exists(), "init must create the directory");
        assert!(tmp.is_dir());

        // Drop guard explicitly before cleanup so the background thread exits.
        drop(result);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sweep_errors_jsonl_retention_preserves_fresh_files() {
        let tmp =
            std::env::temp_dir().join(format!("tv-errors-sweep-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap_or_else(|e| panic!("create_dir_all: {e}"));

        // Freshly-created file: mtime = now, far below any reasonable
        // retention threshold. Sweep MUST leave it alone.
        let fresh = tmp.join("errors.jsonl.2099-01-01-00");
        std::fs::write(&fresh, b"fresh").unwrap_or_else(|e| panic!("write fresh: {e}"));

        let deleted =
            sweep_errors_jsonl_retention(&tmp, 48).unwrap_or_else(|e| panic!("sweep failed: {e}"));
        assert_eq!(deleted, 0, "fresh file must not be deleted");
        assert!(fresh.exists(), "fresh file must survive the sweep");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sweep_errors_jsonl_retention_handles_missing_dir() {
        let tmp =
            std::env::temp_dir().join(format!("tv-errors-sweep-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(!tmp.exists());

        let result = sweep_errors_jsonl_retention(&tmp, 48);
        assert!(
            matches!(result, Ok(0)),
            "missing directory must return Ok(0), got {result:?}"
        );
    }

    #[test]
    fn sweep_errors_jsonl_retention_ignores_unrelated_files() {
        let tmp =
            std::env::temp_dir().join(format!("tv-errors-sweep-unrelated-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap_or_else(|e| panic!("create_dir_all: {e}"));

        let unrelated = tmp.join("somebody-elses-file.log");
        std::fs::write(&unrelated, b"not mine").unwrap_or_else(|e| panic!("write: {e}"));

        let deleted =
            sweep_errors_jsonl_retention(&tmp, 0).unwrap_or_else(|e| panic!("sweep failed: {e}"));
        assert_eq!(deleted, 0, "unrelated files must never be touched");
        assert!(unrelated.exists(), "unrelated file must remain untouched");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn init_app_log_appender_creates_directory() {
        let tmp = std::env::temp_dir().join(format!("tv-app-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(!tmp.exists());

        let result = init_app_log_appender(&tmp);
        assert!(result.is_ok(), "app log appender init should succeed");
        assert!(tmp.exists(), "init must create the directory");
        assert!(tmp.is_dir());

        drop(result);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sweep_app_log_retention_preserves_fresh_files() {
        let tmp = std::env::temp_dir().join(format!("tv-app-log-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap_or_else(|e| panic!("create_dir_all: {e}"));

        let fresh = tmp.join("app.2099-01-01-00");
        std::fs::write(&fresh, b"fresh").unwrap_or_else(|e| panic!("write fresh: {e}"));

        let deleted =
            sweep_app_log_retention(&tmp, 168).unwrap_or_else(|e| panic!("sweep failed: {e}"));
        assert_eq!(deleted, 0, "fresh file must not be deleted");
        assert!(fresh.exists(), "fresh file must survive the sweep");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sweep_app_log_retention_ignores_legacy_app_log_alloy_mount() {
        let tmp = std::env::temp_dir().join(format!("tv-app-log-alloy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap_or_else(|e| panic!("create_dir_all: {e}"));

        // The Alloy file mount target `app.log` is an `infra.rs`-owned file;
        // the rolling sweeper MUST NOT touch it even with retention=0.
        let alloy = tmp.join("app.log");
        std::fs::write(&alloy, b"alloy mount").unwrap_or_else(|e| panic!("write: {e}"));

        let deleted =
            sweep_app_log_retention(&tmp, 0).unwrap_or_else(|e| panic!("sweep failed: {e}"));
        assert_eq!(deleted, 0, "app.log must never be swept");
        assert!(alloy.exists(), "app.log must survive the sweep");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sweep_app_log_retention_handles_missing_dir() {
        let tmp = std::env::temp_dir().join(format!("tv-app-log-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let result = sweep_app_log_retention(&tmp, 168);
        assert!(matches!(result, Ok(0)));
    }

    #[test]
    fn sweep_app_log_retention_ignores_unrelated_files() {
        let tmp = std::env::temp_dir().join(format!("tv-app-log-unrelated-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap_or_else(|e| panic!("create_dir_all: {e}"));

        // `application.log` starts with "app" but the next char is not '.',
        // so the rolling-suffix guard must skip it.
        let unrelated_a = tmp.join("application.log");
        let unrelated_b = tmp.join("errors.jsonl.2000-01-01-00");
        std::fs::write(&unrelated_a, b"a").unwrap_or_else(|e| panic!("write: {e}"));
        std::fs::write(&unrelated_b, b"b").unwrap_or_else(|e| panic!("write: {e}"));

        let deleted =
            sweep_app_log_retention(&tmp, 0).unwrap_or_else(|e| panic!("sweep failed: {e}"));
        assert_eq!(
            deleted, 0,
            "unrelated prefixes (application, errors.jsonl) must not be swept"
        );
        assert!(unrelated_a.exists());
        assert!(unrelated_b.exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn app_log_prefix_matches_rolling_appender_naming() {
        // Files produced by `RollingFileAppender::new(Rotation::HOURLY, dir, "app")`
        // are `app.YYYY-MM-DD-HH`. The sweeper's filter relies on this
        // exact prefix; if the constant ever drifts the sweeper silently
        // stops working.
        assert_eq!(APP_LOG_PREFIX, "app");
    }

    #[test]
    fn sweep_errors_jsonl_retention_with_zero_hours_sweeps_everything_matching_prefix() {
        let tmp = std::env::temp_dir().join(format!("tv-errors-sweep-zero-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap_or_else(|e| panic!("create_dir_all: {e}"));

        let a = tmp.join("errors.jsonl.2000-01-01-00");
        let b = tmp.join("errors.jsonl.2001-01-01-00");
        std::fs::write(&a, b"a").unwrap_or_else(|e| panic!("write: {e}"));
        std::fs::write(&b, b"b").unwrap_or_else(|e| panic!("write: {e}"));

        // With retention=0 every file older than 0s is deleted. Since we just
        // wrote the files this second, duration_since may return ~0, so they
        // may or may not be removed. Either outcome is fine; what MUST hold
        // is: no panic, count <= 2.
        let deleted =
            sweep_errors_jsonl_retention(&tmp, 0).unwrap_or_else(|e| panic!("sweep failed: {e}"));
        assert!(deleted <= 2);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn disabled_tracing_short_circuits() {
        let config = ObservabilityConfig {
            metrics_bind_addr:
                tickvault_common::config::ObservabilityConfig::default_metrics_bind_addr(),
            metrics_port: 0,
            otlp_endpoint: String::new(),
            metrics_enabled: false,
            tracing_enabled: false,
        };
        let start = std::time::Instant::now();
        let result = init_tracing::<tracing_subscriber::Registry>(&config);
        let elapsed = start.elapsed();
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
        assert!(
            elapsed.as_millis() < 100,
            "disabled tracing should return immediately"
        );
    }

    // -----------------------------------------------------------------------
    // 2026-05-02 — Per-category log file separation ratchets
    // -----------------------------------------------------------------------

    #[test]
    fn test_log_category_all_returns_exactly_two_variants() {
        // 2026-06-28: 2 categories — candles, live ticks. `LogCategory::OptionChain`
        // retired with the deleted option_chain REST subsystem.
        // `LogCategory::Historical` retired earlier alongside the deleted Dhan
        // historical fetch chain; `LogCategory::Movers` (PR #539) folded into
        // LiveTicks.
        let all = LogCategory::all();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_log_category_dir_and_prefix_stable_for_each_variant() {
        // Pin the on-disk paths so an accidental rename doesn't break
        // operator runbooks or external Loki/Alloy scrape configs.
        assert_eq!(LogCategory::Candles.dir(), "data/logs/candles");
        assert_eq!(LogCategory::Candles.prefix(), "candles");
        assert_eq!(LogCategory::LiveTicks.dir(), "data/logs/live_ticks");
        assert_eq!(LogCategory::LiveTicks.prefix(), "live_ticks");
    }

    /// 2026-05-09 ratchet: the 4 RAM-tracker tracing targets that
    /// formerly routed to `LogCategory::Movers` MUST now route to
    /// `LogCategory::LiveTicks`. Operator directive after the PR #539
    /// QuestDB movers infrastructure retirement: "Also retire
    /// LogCategory::Movers". Blocks regression to a separate movers
    /// category.
    #[test]
    fn test_build_category_targets_live_ticks_includes_movers_ram_tracker_targets() {
        let targets = build_category_targets(LogCategory::LiveTicks);
        let expected = [
            "tickvault_core::pipeline::mover_classifier",
            "tickvault_core::pipeline::movers_window",
            "tickvault_core::pipeline::option_movers",
            "tickvault_core::pipeline::top_movers",
        ];
        for e in expected {
            assert!(
                targets.contains(&e),
                "post-2026-05-09 LiveTicks targets MUST include {e} \
                 (formerly under retired LogCategory::Movers)"
            );
        }
    }

    // PR-D (2026-05-26): historical category routing test retired
    // alongside the deleted LogCategory::Historical variant.

    #[test]
    fn test_build_category_targets_live_ticks_covers_websocket_and_pipeline() {
        let targets = build_category_targets(LogCategory::LiveTicks);
        assert!(targets.contains(&"tickvault_core::websocket"));
        assert!(targets.contains(&"tickvault_core::pipeline::tick_processor"));
        assert!(targets.contains(&"tickvault_storage::tick_persistence"));
    }

    #[test]
    fn test_build_category_targets_each_variant_is_non_empty() {
        for cat in LogCategory::all() {
            let targets = build_category_targets(cat);
            assert!(
                !targets.is_empty(),
                "category {cat:?} must route at least one module"
            );
        }
    }

    #[test]
    fn test_init_category_log_appender_creates_directory_idempotent() {
        let temp = std::env::temp_dir().join(format!("tv_cat_log_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp); // best-effort cleanup
        let result = init_category_log_appender(&temp, "test_category");
        assert!(result.is_ok(), "first init must succeed");
        assert!(temp.is_dir(), "appender must have created the directory");
        // Idempotent — second call on same dir must also succeed.
        let _drop_first = result;
        let result2 = init_category_log_appender(&temp, "test_category");
        assert!(result2.is_ok(), "second init on same dir must succeed");
        // Best-effort cleanup
        drop(result2);
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_sweep_category_log_retention_returns_zero_on_missing_dir() {
        let nonexistent = std::env::temp_dir().join("definitely_not_a_real_dir_xyz");
        let _ = std::fs::remove_dir_all(&nonexistent);
        let deleted = sweep_category_log_retention(&nonexistent, "movers", 24).unwrap();
        assert_eq!(deleted, 0);
    }
}
