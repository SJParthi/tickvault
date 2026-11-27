//! Structured logging for the Lambda bins.
//!
//! Legacy parity: the killswitch handler read `LOG_LEVEL` (default `INFO`)
//! into the stdlib logger; the heredoc lambdas used bare `print`. Here every
//! bin initializes ONE JSON tracing subscriber whose level comes from
//! `LOG_LEVEL` (same env var, same default) — CloudWatch Logs ingests the
//! JSON lines exactly as it ingested the legacy logger/print output.

use tracing_subscriber::EnvFilter;

/// Map the legacy-style `LOG_LEVEL` value (`INFO` / `DEBUG` / `WARNING`)
/// onto a tracing filter directive. Unknown values fall back to `info`
/// (the legacy logger would have raised on setLevel of garbage only for
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

/// Only canonical SNS UUIDs may enter receipts. Never log unexpected provider text.
pub(crate) fn sns_message_id(raw: Option<&str>) -> Option<&str> {
    raw.filter(|id| {
        id.len() == 36
            && id.bytes().enumerate().all(|(pos, byte)| match pos {
                8 | 13 | 18 | 23 => byte == b'-',
                _ => byte.is_ascii_hexdigit(),
            })
    })
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
    fn level_directive_maps_legacy_levels() {
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
    fn sns_message_id_preserves_only_the_bounded_uuid_shape() {
        let id = "B5B1CC77-914A-57C5-A0BA-2515C6A30E93";
        assert_eq!(sns_message_id(Some(id)), Some(id));
        for invalid in [
            None,
            Some(""),
            Some("B5B1CC77914A57C5A0BA2515C6A30E93"),
            Some("B5B1CC77_914A-57C5-A0BA-2515C6A30E93"),
            Some("B5B1CC77-914A-57C5-A0BA-2515C6A30E9Z"),
            Some("B5B1CC77-914A-57C5-A0BA-2515C6A30E93\n"),
        ] {
            assert_eq!(sns_message_id(invalid), None);
        }
    }

    #[test]
    fn init_is_idempotent() {
        init_lambda_tracing();
        init_lambda_tracing(); // second call must not panic
    }
}
