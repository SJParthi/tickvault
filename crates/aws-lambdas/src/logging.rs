//! Structured logging for the Lambda bins.
//!
//! Python parity: the killswitch handler read `LOG_LEVEL` (default `INFO`)
//! into the stdlib logger; the heredoc lambdas used bare `print`. Here every
//! bin initializes ONE JSON tracing subscriber whose level comes from
//! `LOG_LEVEL` (same env var, same default) — CloudWatch Logs ingests the
//! JSON lines exactly as it ingested the Python logger/print output.

use tracing_subscriber::EnvFilter;

/// Map the Python-style `LOG_LEVEL` value (`INFO` / `DEBUG` / `WARNING`)
/// onto a tracing filter directive. Unknown values fall back to `info`
/// (the Python logger would have raised on setLevel of garbage only for
/// non-string types; string garbage fell back to WARNING-ish behavior —
/// deliberate deviation: we fail-soft to `info`, documented in the PR).
pub fn level_directive(raw: Option<&str>) -> &'static str {
    match raw.map(str::trim).map(str::to_ascii_uppercase).as_deref() {
        Some("DEBUG") => "debug",
        Some("WARNING") | Some("WARN") => "warn",
        Some("ERROR") => "error",
        _ => "info",
    }
}

/// Install the process-global JSON subscriber. Safe to call more than once
/// (warm Lambda re-invokes, tests) — a second call is a no-op.
pub fn init_lambda_tracing() {
    let level = std::env::var("LOG_LEVEL").ok();
    let filter = EnvFilter::new(level_directive(level.as_deref()));
    if tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_ansi(false)
        .try_init()
        .is_err()
    {
        // Already initialized (warm invoke / test harness) — benign.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_directive_maps_python_levels() {
        assert_eq!(level_directive(Some("INFO")), "info");
        assert_eq!(level_directive(Some("DEBUG")), "debug");
        assert_eq!(level_directive(Some("WARNING")), "warn");
        assert_eq!(level_directive(Some("warn")), "warn");
        assert_eq!(level_directive(Some("ERROR")), "error");
    }

    #[test]
    fn level_directive_defaults_to_info() {
        assert_eq!(level_directive(None), "info");
        assert_eq!(level_directive(Some("")), "info");
        assert_eq!(level_directive(Some("garbage")), "info");
    }

    #[test]
    fn init_is_idempotent() {
        init_lambda_tracing();
        init_lambda_tracing(); // second call must not panic
    }
}
