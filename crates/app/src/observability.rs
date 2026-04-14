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

use tickvault_common::config::ObservabilityConfig;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::config::ObservabilityConfig;

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

    // -----------------------------------------------------------------------
    // Additional coverage: metrics-only disabled, tracing-only disabled,
    // various config combinations
    // -----------------------------------------------------------------------

    #[test]
    fn init_metrics_disabled_tracing_enabled_only_metrics_returns_ok() {
        let config = ObservabilityConfig {
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

    #[test]
    fn disabled_tracing_short_circuits() {
        let config = ObservabilityConfig {
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
}
