//! HTTP-CLIENT-01 — panic-free reqwest client construction (C2 fix, 2026-07-03).
//!
//! Root cause this module removes: 8 sites in `crates/storage/src/` used the
//! fallback `Client::builder().build().unwrap_or_else(|_| Client::new())`.
//! `reqwest::Client::new()` PANICS when the builder fails (TLS backend init,
//! DNS resolver init, fd exhaustion) — the exact conditions under which the
//! builder's `Err` arm is reachable in the first place. A panic inside a
//! tokio task is a SILENT task death (no `error!`, no respawn signal) — the
//! suspected mechanism behind the 2026-07-03 10:35 IST SLO-publisher death
//! during a 1.13M-frame storm (`SLO-03` incident): `wait_for_questdb_ready`
//! builds a client per invocation on the every-10s SLO tick and the every-5s
//! pool-watchdog tick.
//!
//! This module provides:
//! - [`client_from_build_result`] — the pure, testable core mapping a builder
//!   `Err` to a typed [`HttpClientBuildError`] instead of a panic.
//! - [`build_probe_client`] — build a timeout-bounded client, no panic path.
//! - [`shared_probe_client`] — a process-wide `OnceLock` client for the
//!   repeating QuestDB readiness probes: built ONCE, reused forever
//!   (O(1) after first call — a single atomic load).
//!
//! Failure emit sites log `error!` with
//! `code = ErrorCode::HttpClient01BuildFailed.code_str()` and increment
//! `tv_http_client_build_failed_total{site}` (STATIC labels only). Runbook:
//! `.claude/rules/project/http-client-error-codes.md`.

use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::Client;

/// Request timeout for the shared QuestDB readiness-probe client.
///
/// Mirrors `boot_probe::PROBE_REQUEST_TIMEOUT_SECS` (2s) — a single probe
/// must never stretch the 5s pool-watchdog / 10s SLO scheduler ticks.
pub const PROBE_REQUEST_TIMEOUT_SECS: u64 = 2;

/// Typed, panic-free error for a failed `reqwest::ClientBuilder::build()`.
///
/// The message is the builder error's `Display` string with URL userinfo
/// redacted ([`redact_userinfo`] strips `scheme://user:pass@` credentials —
/// reqwest builder errors can embed proxy URLs from `HTTPS_PROXY` that carry
/// Basic-Auth userinfo); it never carries request bodies or our own tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpClientBuildError {
    message: String,
}

impl HttpClientBuildError {
    /// The captured builder-failure description.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for HttpClientBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "HTTP-CLIENT-01 reqwest client build failed: {}",
            self.message
        )
    }
}

impl std::error::Error for HttpClientBuildError {}

/// Redact URL userinfo (`scheme://user:pass@host` → `scheme://***@host`)
/// from an error message before it is logged / rendered.
///
/// reqwest builder errors can embed proxy URLs sourced from the
/// `HTTPS_PROXY` env var, which may carry Basic-Auth credentials in the
/// userinfo portion. Regex-free scan: for each `://` occurrence, if an `@`
/// appears before the next `/`, whitespace, or end-of-string, the
/// characters between `://` and the final `@` of that authority are
/// replaced with `***`.
fn redact_userinfo(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    while let Some(idx) = rest.find("://") {
        let (head, tail) = rest.split_at(idx + "://".len());
        out.push_str(head);
        // Authority ends at the next '/', whitespace, or end of string.
        let boundary = tail
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(tail.len());
        let authority = &tail[..boundary];
        // rfind: a raw password may itself contain '@' — redact up to the
        // LAST '@' of the authority so no credential fragment survives.
        if let Some(at) = authority.rfind('@') {
            out.push_str("***");
            rest = &tail[at..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// Pure core: map a `ClientBuilder::build()` result into a typed error.
///
/// NEVER panics — the `Err` arm captures the error's `Display` string
/// (userinfo-redacted via [`redact_userinfo`]) into
/// [`HttpClientBuildError`]. Extracted as a pure function so the
/// builder-failure path is deterministically unit-testable (a real
/// `reqwest` builder failure needs fd/TLS exhaustion, which tests cannot
/// safely induce).
///
/// Promoted `pub(crate)` → `pub` 2026-07-17 (post-#1562 audit): the app
/// crate's `oms_wiring::build_oms_http_client` reuses this SAME redaction +
/// typed-error core instead of duplicating it (one HTTP-CLIENT-01
/// implementation, one redaction path).
///
/// # Errors
///
/// Returns [`HttpClientBuildError`] carrying the redacted `Display` string
/// whenever `res` is `Err` — never panics, never falls back to the
/// panic-class `Client::new()`.
pub fn client_from_build_result<E: fmt::Display>(
    res: Result<Client, E>,
) -> Result<Client, HttpClientBuildError> {
    res.map_err(|e| HttpClientBuildError {
        message: redact_userinfo(&e.to_string()),
    })
}

/// Build a timeout-bounded reqwest client with NO panic path.
///
/// # Errors
///
/// Returns [`HttpClientBuildError`] when the underlying
/// `ClientBuilder::build()` fails (TLS backend init, resolver init, fd
/// exhaustion). Callers degrade gracefully — they never panic.
pub fn build_probe_client(timeout_secs: u64) -> Result<Client, HttpClientBuildError> {
    client_from_build_result(
        Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build(),
    )
}

/// Process-wide shared QuestDB readiness-probe client.
static PROBE_CLIENT: OnceLock<Client> = OnceLock::new();

/// Returns the process-wide shared probe client, building it on first call.
///
/// Built ONCE per process and reused across every probe invocation — the
/// boot probe, the every-5s pool-watchdog QuestDB probe, and the every-10s
/// SLO scheduler probe all share this instance, so TLS/resolver init happens
/// exactly once instead of ~8,640 times per trading session. O(1) after the
/// first call (a single atomic load).
///
/// The client is built BEFORE `get_or_init` so a build failure propagates as
/// a typed error instead of panicking inside the init closure. A racing
/// double-build is harmless — `OnceLock` keeps exactly one instance and the
/// loser's client is dropped.
///
/// # Errors
///
/// Returns [`HttpClientBuildError`] when the client cannot be built (and no
/// prior call succeeded). The NEXT call retries the build, so a transient
/// fd-exhaustion window does not poison the process forever.
pub fn shared_probe_client() -> Result<&'static Client, HttpClientBuildError> {
    if let Some(client) = PROBE_CLIENT.get() {
        return Ok(client);
    }
    let client = build_probe_client(PROBE_REQUEST_TIMEOUT_SECS)?;
    Ok(PROBE_CLIENT.get_or_init(move || client))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_from_build_result_maps_error_without_panic() {
        let res: Result<Client, &str> = Err("boom");
        let err = client_from_build_result(res).expect_err("Err must map to typed error");
        assert!(
            err.message().contains("boom"),
            "captured message must carry the source Display: {}",
            err.message()
        );
    }

    #[test]
    fn test_client_from_build_result_ok_passthrough() {
        let client = build_probe_client(1).expect("test client builds");
        let res: Result<Client, &str> = Ok(client);
        assert!(client_from_build_result(res).is_ok());
    }

    #[test]
    fn test_shared_probe_client_reuses_same_instance() {
        let a = shared_probe_client().expect("first call builds");
        let b = shared_probe_client().expect("second call reuses");
        assert!(
            std::ptr::eq(a, b),
            "shared_probe_client must return the SAME static instance"
        );
    }

    #[test]
    fn test_build_probe_client_succeeds_with_timeout() {
        let client = build_probe_client(PROBE_REQUEST_TIMEOUT_SECS);
        assert!(client.is_ok(), "normal-env build must succeed: {client:?}");
    }

    #[test]
    fn test_error_display_carries_code_and_message_no_secret_echo() {
        let res: Result<Client, &str> = Err("resolver init failed");
        let err = client_from_build_result(res).expect_err("typed error");
        let rendered = format!("{err}");
        assert!(rendered.contains("HTTP-CLIENT-01"), "{rendered}");
        assert!(rendered.contains("resolver init failed"), "{rendered}");
        // The error type has no fields beyond the builder message — assert
        // the rendered form is exactly the code + captured message (no URL,
        // no token, nothing else to leak).
        assert_eq!(
            rendered,
            "HTTP-CLIENT-01 reqwest client build failed: resolver init failed"
        );
        // std::error::Error trait object path.
        let as_err: &dyn std::error::Error = &err;
        assert_eq!(as_err.to_string(), rendered);
    }

    #[test]
    fn test_redact_userinfo_strips_basic_auth() {
        let input = "error trying to connect via proxy \
                     https://user:s3cret@proxy.example:8080 (tunnel failed)";
        let out = redact_userinfo(input);
        assert!(out.contains("***@proxy.example"), "{out}");
        assert!(!out.contains("s3cret"), "credential leaked: {out}");
        assert!(!out.contains("user:"), "username leaked: {out}");
        // Non-authority content is preserved verbatim.
        assert!(out.contains("(tunnel failed)"), "{out}");
        assert!(
            out.starts_with("error trying to connect via proxy "),
            "{out}"
        );
    }

    #[test]
    fn test_redact_userinfo_passthrough_no_userinfo() {
        for input in [
            "resolver init failed",
            "connect error for http://questdb:9000/exec timed out",
            "plain text with no url at all",
            "",
        ] {
            assert_eq!(
                redact_userinfo(input),
                input,
                "no-userinfo input must pass through"
            );
        }
    }

    #[test]
    fn test_redact_userinfo_password_containing_at_sign() {
        // A raw password containing '@' — rfind redacts up to the LAST '@'.
        let input = "proxy http://user:p@ss@proxy.host:3128/ refused";
        let out = redact_userinfo(input);
        assert!(out.contains("***@proxy.host"), "{out}");
        assert!(!out.contains("p@ss"), "credential leaked: {out}");
    }

    #[test]
    fn test_error_message_is_redacted() {
        let res: Result<Client, String> = Err(
            "builder error: proxy scheme https://admin:hunter2@corp-proxy.internal:8080 rejected"
                .to_string(),
        );
        let err = client_from_build_result(res).expect_err("typed error");
        let rendered = format!("{err}");
        assert!(
            !rendered.contains("hunter2"),
            "secret leaked in Display: {rendered}"
        );
        assert!(
            !rendered.contains("admin:"),
            "username leaked in Display: {rendered}"
        );
        assert!(rendered.contains("***@corp-proxy.internal"), "{rendered}");
        assert!(rendered.contains("HTTP-CLIENT-01"), "{rendered}");
        // message() accessor is redacted too (same underlying field).
        assert!(!err.message().contains("hunter2"), "{}", err.message());
    }
}
