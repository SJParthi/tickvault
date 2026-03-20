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

use anyhow::{Context, Result};
use metrics_exporter_prometheus::PrometheusBuilder;
use opentelemetry::trace::TracerProvider;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::info;
use tracing_opentelemetry::OpenTelemetryLayer;

use dhan_live_trader_common::config::ObservabilityConfig;

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

    PrometheusBuilder::new()
        .with_http_listener(([0, 0, 0, 0], config.metrics_port))
        .install()
        .context("failed to install Prometheus metrics exporter")?;

    info!(
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

    let resource = Resource::builder()
        .with_service_name("dhan-live-trader")
        .build();

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer("dhan-live-trader");
    let layer = OpenTelemetryLayer::new(tracer);

    info!(
        endpoint = &config.otlp_endpoint,
        "OpenTelemetry tracing pipeline started"
    );

    Ok(Some((layer, provider)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dhan_live_trader_common::config::ObservabilityConfig;

    fn disabled_config() -> ObservabilityConfig {
        ObservabilityConfig {
            metrics_port: 0,
            otlp_endpoint: String::new(),
            metrics_enabled: false,
            tracing_enabled: false,
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
}
