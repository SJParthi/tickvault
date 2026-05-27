//! Sub-PR #3 of 2026-05-27 daily-universe expansion — hardened HTTP
//! client for fetching the Dhan Detailed instrument-master CSV.
//!
//! **Feature-gated.** This module compiles only when the
//! `daily_universe_fetcher` cargo feature is enabled (per §21 of the
//! authoritative rule file). Activation happens in Sub-PR #10's boot
//! orchestrator. Under the current `Indices4Only` default scope the
//! feature is OFF — this code is dead until the universe-expansion
//! land sequence completes.
//!
//! ## Security hardening contract (§18 of the rule file)
//!
//! Every property below is enforced by the `CsvDownloader` builder and
//! pinned by ratchet tests in `crates/storage/tests/
//! csv_downloader_hardening_guard.rs`:
//!
//! | Property | Locked value | Threat addressed |
//! |---|---|---|
//! | Redirect policy | `reqwest::redirect::Policy::none()` | DNS-poison MITM |
//! | Body size cap | `MAX_CSV_BODY_BYTES = 50 MB` | DoS via gigabyte stream |
//! | Connect timeout | 10 s | Black-hole DNS |
//! | Read timeout | `INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS = 60 s` | Hung TCP |
//! | Content-Type | text/csv ∪ application/octet-stream ∪ text/plain | JSON error body parse |
//! | URL | hardcoded `DHAN_DETAILED_CSV_URL` constant | Open-redirect / typo |
//!
//! ## What this module DOES NOT do (deferred to later sub-PRs)
//!
//! - SHA-256 verification (Sub-PR #9 lifecycle reconciler)
//! - Row-count / mandatory-field validation (Sub-PR #4 parser)
//! - L3 RECONCILE anomaly check vs yesterday's audit row (Sub-PR #9)
//! - L7 cooldown / retry backoff orchestration (Sub-PR #10 orchestrator)
//! - Cache write to `data/instrument-cache/YYYY-MM-DD/raw.csv` with
//!   path-traversal guard (Sub-PR #10 — needs the orchestrator's
//!   trading-date context)
//! - `--operator-acknowledge-stale-csv` escape valve (Sub-PR #10)
//!
//! This module ships ONLY the HTTP layer: a hardened `reqwest::Client`
//! + a body-streaming reader bounded by `MAX_CSV_BODY_BYTES`. Composes
//! upward into the orchestrator without leaking any wire-level concerns.

#![cfg(feature = "daily_universe_fetcher")]

use std::time::Duration;

use reqwest::{
    Client, ClientBuilder, Response, StatusCode,
    header::{CONTENT_TYPE, HeaderValue},
    redirect,
};
use thiserror::Error;
use tickvault_common::constants::{
    CSV_CONNECT_TIMEOUT_SECS, DHAN_DETAILED_CSV_URL, INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS,
    MAX_CSV_BODY_BYTES,
};

/// Allowed Content-Type values. Rejecting `text/html` catches WAF block
/// pages; rejecting `application/json` catches Dhan-side error bodies
/// that would otherwise feed garbage to the CSV parser.
const ALLOWED_CONTENT_TYPES: &[&str] = &["text/csv", "application/octet-stream", "text/plain"];

/// Errors that can occur during a single CSV-fetch attempt. The
/// orchestrator (Sub-PR #10) wraps these in its infinite-retry +
/// escalation ladder per §4 of the rule file.
#[derive(Debug, Error)]
pub enum CsvDownloadError {
    /// reqwest builder / network / TLS / timeout error.
    #[error("HTTP transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// Non-2xx HTTP status. The orchestrator distinguishes 5xx
    /// (transient — retry) from 4xx (permanent — escalate).
    #[error("HTTP status not OK: {status}")]
    HttpStatus { status: StatusCode },

    /// Response Content-Type not in `ALLOWED_CONTENT_TYPES`.
    #[error("rejected Content-Type: {content_type}")]
    UnacceptableContentType { content_type: String },

    /// Response body exceeded `MAX_CSV_BODY_BYTES` (50 MB). Defends
    /// against malicious / malfunctioning servers streaming gigabytes.
    #[error("response body exceeded cap of {} bytes", MAX_CSV_BODY_BYTES)]
    BodyTooLarge,
}

/// Hardened HTTP client for the instrument-master CSV.
///
/// Constructed once per orchestrator boot (cold path). Each
/// `fetch_csv()` call performs one bounded GET attempt; retries +
/// escalation live in the orchestrator above this layer.
pub struct CsvDownloader {
    client: Client,
}

impl CsvDownloader {
    /// Build a `CsvDownloader` with the §18 hardening locked in.
    ///
    /// # Errors
    ///
    /// Returns `Err` only if `reqwest::ClientBuilder::build` fails,
    /// which in practice means TLS root certificates could not be
    /// loaded. Other failures (DNS, network) surface later in
    /// `fetch_csv`.
    pub fn new() -> Result<Self, CsvDownloadError> {
        let client = ClientBuilder::new()
            // §18 — refuse to follow ANY redirect.
            .redirect(redirect::Policy::none())
            // §18 — bound TCP handshake.
            .connect_timeout(Duration::from_secs(CSV_CONNECT_TIMEOUT_SECS))
            // §18 — bound whole response.
            .timeout(Duration::from_secs(
                INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS,
            ))
            // Defensive — strict HTTPS, no HTTP/0.9 fallback.
            .https_only(true)
            .build()?;
        Ok(Self { client })
    }

    /// Perform ONE bounded GET against `DHAN_DETAILED_CSV_URL`.
    ///
    /// On success returns the CSV body as bytes. The orchestrator above
    /// then SHA-256s + row-validates + persists the bytes (Sub-PR #9
    /// + #10 work).
    ///
    /// # Errors
    ///
    /// See [`CsvDownloadError`] variants.
    pub async fn fetch_csv(&self) -> Result<Vec<u8>, CsvDownloadError> {
        let response = self.client.get(DHAN_DETAILED_CSV_URL).send().await?;

        if !response.status().is_success() {
            return Err(CsvDownloadError::HttpStatus {
                status: response.status(),
            });
        }

        validate_content_type(response.headers().get(CONTENT_TYPE))?;

        read_body_with_cap(response).await
    }

    /// Borrow the inner `reqwest::Client` for advanced uses (test fixtures,
    /// metric scraping). The client carries the locked-in hardening.
    #[must_use]
    pub fn client(&self) -> &Client {
        &self.client
    }
}

/// Validate the Content-Type header against `ALLOWED_CONTENT_TYPES`.
/// Missing header is allowed (some static-CDN setups don't send one).
fn validate_content_type(header: Option<&HeaderValue>) -> Result<(), CsvDownloadError> {
    let Some(value) = header else {
        // Missing Content-Type — accept (defensive — Dhan's static CDN
        // historically omits it; sufficient validation happens via row-
        // count / SHA-256 in upper layers).
        return Ok(());
    };
    let raw = value
        .to_str()
        .map_err(|_| CsvDownloadError::UnacceptableContentType {
            content_type: "<invalid utf-8>".to_string(),
        })?;
    // Strip charset suffix (e.g. "text/csv; charset=utf-8") for the
    // allowlist check.
    let primary = raw.split(';').next().unwrap_or("").trim().to_lowercase();
    if ALLOWED_CONTENT_TYPES
        .iter()
        .any(|&allowed| allowed == primary)
    {
        Ok(())
    } else {
        Err(CsvDownloadError::UnacceptableContentType {
            content_type: raw.to_string(),
        })
    }
}

/// Stream the response body into a `Vec<u8>` bounded by
/// `MAX_CSV_BODY_BYTES`. Aborts mid-stream if the cap is exceeded so a
/// 1 GB malicious payload cannot exhaust RAM.
async fn read_body_with_cap(mut response: Response) -> Result<Vec<u8>, CsvDownloadError> {
    // Pre-size from Content-Length if present (and within the cap),
    // otherwise start small and grow. Cap-bounded by construction.
    let initial_capacity = response
        .content_length()
        .map(|len| len.min(MAX_CSV_BODY_BYTES as u64) as usize)
        .unwrap_or(0);
    let mut body = Vec::with_capacity(initial_capacity);

    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_CSV_BODY_BYTES {
            return Err(CsvDownloadError::BodyTooLarge);
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn detailed_csv_url_is_dhan_images_host() {
        assert_eq!(
            DHAN_DETAILED_CSV_URL, "https://images.dhan.co/api-data/api-scrip-master-detailed.csv",
            "Detailed CSV URL is operator-locked per rule file §3"
        );
    }

    #[test]
    fn csv_connect_timeout_is_bounded() {
        assert_eq!(CSV_CONNECT_TIMEOUT_SECS, 10);
        assert!(CSV_CONNECT_TIMEOUT_SECS < INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS);
    }

    #[test]
    fn allowed_content_types_match_rule_file_contract() {
        // Per §18 — exactly these three; any change requires rule-file
        // edit.
        assert_eq!(ALLOWED_CONTENT_TYPES.len(), 3);
        assert!(ALLOWED_CONTENT_TYPES.contains(&"text/csv"));
        assert!(ALLOWED_CONTENT_TYPES.contains(&"application/octet-stream"));
        assert!(ALLOWED_CONTENT_TYPES.contains(&"text/plain"));
    }

    #[test]
    fn validate_content_type_accepts_missing_header() {
        assert!(validate_content_type(None).is_ok());
    }

    #[test]
    fn validate_content_type_accepts_text_csv() {
        let h = HeaderValue::from_static("text/csv");
        assert!(validate_content_type(Some(&h)).is_ok());
    }

    #[test]
    fn validate_content_type_accepts_text_csv_with_charset() {
        let h = HeaderValue::from_static("text/csv; charset=utf-8");
        assert!(validate_content_type(Some(&h)).is_ok());
    }

    #[test]
    fn validate_content_type_accepts_octet_stream() {
        let h = HeaderValue::from_static("application/octet-stream");
        assert!(validate_content_type(Some(&h)).is_ok());
    }

    #[test]
    fn validate_content_type_rejects_text_html_waf_page() {
        let h = HeaderValue::from_static("text/html");
        let result = validate_content_type(Some(&h));
        assert!(matches!(
            result,
            Err(CsvDownloadError::UnacceptableContentType { .. })
        ));
    }

    #[test]
    fn validate_content_type_rejects_application_json() {
        let h = HeaderValue::from_static("application/json");
        let result = validate_content_type(Some(&h));
        assert!(matches!(
            result,
            Err(CsvDownloadError::UnacceptableContentType { .. })
        ));
    }

    #[test]
    fn validate_content_type_is_case_insensitive() {
        let h = HeaderValue::from_static("TEXT/CSV");
        assert!(validate_content_type(Some(&h)).is_ok());
    }

    #[test]
    fn validate_content_type_handles_unusual_whitespace() {
        let h = HeaderValue::from_static("  text/csv  ;  charset=utf-8  ");
        assert!(validate_content_type(Some(&h)).is_ok());
    }

    #[test]
    fn new_returns_a_built_client_under_normal_conditions() {
        let downloader = CsvDownloader::new().expect("client should build");
        // Tautology: just verify the type compiles + the inner
        // reference returns. Full HTTPS verification requires a live
        // network test which lives in the orchestrator integration
        // tests in Sub-PR #10.
        let _client_ref = downloader.client();
    }

    #[test]
    fn csv_download_error_display_is_informative() {
        let e = CsvDownloadError::HttpStatus {
            status: StatusCode::INTERNAL_SERVER_ERROR,
        };
        let msg = format!("{e}");
        assert!(
            msg.contains("500"),
            "error message should include status code"
        );
    }

    #[test]
    fn body_too_large_message_includes_cap() {
        let msg = format!("{}", CsvDownloadError::BodyTooLarge);
        // The cap is large (50 MB = 52428800) — make sure the message
        // includes it so the operator can diagnose without digging.
        let cap_str = MAX_CSV_BODY_BYTES.to_string();
        assert!(
            msg.contains(&cap_str),
            "body-too-large error should mention the byte cap"
        );
    }
}
