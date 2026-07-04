//! Runtime source badge for Telegram messages — 💻 LOCAL vs ☁️ AWS.
//!
//! Operator directive 2026-07-04 (verbatim): "in telegram when it displays
//! message then it should clearly tell whether it is from local or aws".
//!
//! During the local-window runs (operator's Mac carrying the live feeds while
//! the AWS box is paused) BOTH runtimes can plausibly send Telegram messages
//! to the SAME chat. Without a source tag the operator cannot tell which
//! machine an alert came from. Every outgoing Telegram message therefore
//! carries a source badge immediately after the severity tag in the first
//! line: `💻 LOCAL` or `☁️ AWS`.
//!
//! # Why the signal is systemd-management (NOT `TV_ENVIRONMENT`)
//!
//! `TV_ENVIRONMENT` cannot distinguish the two runtimes: it DEFAULTS to
//! `prod` when unset (`boot_helpers::resolve_config_env` /
//! `secret_manager::resolve_environment` — single real env, operator
//! 2026-06-30), so a Mac `make run` and the AWS box both resolve to `prod`.
//! The one existing signal that DOES distinguish them is systemd management:
//! the ONLY systemd deployment of this binary is the prod box unit
//! (`deploy/systemd/tickvault.service`, `Type=notify`), and systemd exports
//! `NOTIFY_SOCKET` into the service process (the same signal
//! `infra::notify_systemd_ready` already keys on — "No-op when NOTIFY_SOCKET
//! is unset (e.g. local `cargo run`)"). No new env var is invented.
//!
//! # Honest envelope
//!
//! The badge reports HOW the process is being run, which today maps 1:1 to
//! WHERE it runs: systemd-managed = the AWS prod box; anything else (Mac
//! `make run`, `cargo run`, tests) = LOCAL. If the operator ever runs the
//! binary under systemd on a non-AWS host, that run would badge as `☁️ AWS`
//! — acceptable inside the current deployment envelope (exactly one systemd
//! unit exists, on the AWS box). The badge leaks nothing beyond the single
//! word LOCAL/AWS — no hostname, no IP, no env-var values.
//!
//! # Performance
//!
//! Cold path only (notification dispatch). The env probe runs ONCE per
//! process via [`OnceLock`]; every later call is an O(1) atomic load.

use std::sync::OnceLock;

/// Where this process is running, as seen by the Telegram badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSource {
    /// Not systemd-managed — operator's Mac (`make run`), `cargo run`, tests.
    Local,
    /// Systemd-managed — the AWS prod box (`deploy/systemd/tickvault.service`
    /// is the only systemd deployment of this binary).
    Aws,
}

impl RuntimeSource {
    /// The badge string placed immediately after the severity tag on the
    /// first line of every Telegram message.
    ///
    /// Plain-English per the 10 Telegram commandments: one emoji + one word,
    /// no file paths, no library names, no env-var jargon.
    pub const fn badge(&self) -> &'static str {
        match self {
            Self::Local => "\u{1f4bb} LOCAL",    // 💻
            Self::Aws => "\u{2601}\u{fe0f} AWS", // ☁️
        }
    }
}

/// Pure classifier: systemd-managed process → AWS, otherwise LOCAL.
///
/// Kept pure (bool in, enum out) so unit tests cover both arms without
/// mutating process-global environment variables (the `NOTIFY_SOCKET` env
/// var is also mutated under a lock by `crates/app` systemd tests — core
/// tests must never race it).
pub const fn classify_runtime_source(systemd_managed: bool) -> RuntimeSource {
    if systemd_managed {
        RuntimeSource::Aws
    } else {
        RuntimeSource::Local
    }
}

/// Returns this process's runtime source, probing the environment ONCE.
///
/// `NOTIFY_SOCKET` is exported by systemd into `Type=notify` services (the
/// prod unit) and is absent under `make run` / `cargo run` on the Mac. The
/// result is cached for the process lifetime — the badge cannot flip
/// mid-session, so every message of a run carries a consistent source.
pub fn runtime_source() -> RuntimeSource {
    static SOURCE: OnceLock<RuntimeSource> = OnceLock::new();
    *SOURCE.get_or_init(|| classify_runtime_source(std::env::var_os("NOTIFY_SOCKET").is_some()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_runtime_source_systemd_managed_is_aws() {
        assert_eq!(classify_runtime_source(true), RuntimeSource::Aws);
    }

    #[test]
    fn test_classify_runtime_source_not_systemd_managed_is_local() {
        assert_eq!(classify_runtime_source(false), RuntimeSource::Local);
    }

    #[test]
    fn test_badge_local_is_laptop_emoji_plus_word() {
        assert_eq!(RuntimeSource::Local.badge(), "💻 LOCAL");
    }

    #[test]
    fn test_badge_aws_is_cloud_emoji_plus_word() {
        assert_eq!(RuntimeSource::Aws.badge(), "☁️ AWS");
    }

    #[test]
    fn test_badges_are_distinct_and_short() {
        // One glance on a phone: the two badges must never be confusable
        // and must stay a single short token (commandment 8: one decision).
        let local = RuntimeSource::Local.badge();
        let aws = RuntimeSource::Aws.badge();
        assert_ne!(local, aws);
        assert!(local.contains("LOCAL") && !local.contains("AWS"));
        assert!(aws.contains("AWS") && !aws.contains("LOCAL"));
        assert!(local.chars().count() <= 10);
        assert!(aws.chars().count() <= 10);
    }

    #[test]
    fn test_badge_never_leaks_env_details() {
        // Security envelope: badge is exactly LOCAL/AWS — no hostname, no
        // socket path, no env-var name may ever appear in operator text.
        for src in [RuntimeSource::Local, RuntimeSource::Aws] {
            let b = src.badge();
            assert!(!b.contains("NOTIFY_SOCKET"));
            assert!(!b.contains('/'));
            assert!(!b.contains("systemd"));
        }
    }

    #[test]
    fn test_runtime_source_is_stable_across_calls() {
        // Whatever the ambient environment, the cached value never flips
        // mid-process — every Telegram of a run carries one consistent badge.
        let first = runtime_source();
        let second = runtime_source();
        assert_eq!(first, second);
        assert!(matches!(first, RuntimeSource::Local | RuntimeSource::Aws));
    }
}
