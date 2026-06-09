//! NTM Sub-PR #3 of 2026-05-27 daily-universe expansion — hardened HTTP
//! client for fetching NSE index-constituent CSVs from niftyindices.com.
//!
//! **Feature-gated.** Compiles only when the `daily_universe_fetcher`
//! cargo feature is enabled (per §21 of the rule file). Activation +
//! boot wiring happen in Sub-PR #10's orchestrator; under the default
//! scope the feature is OFF and this code is dead — same lifecycle as
//! its sibling `csv_downloader.rs`.
//!
//! ## Security hardening contract (mirrors §18 of the rule file)
//!
//! Pinned by `crates/storage/tests/constituency_downloader_hardening_guard.rs`:
//!
//! | Property | Locked value | Threat addressed |
//! |---|---|---|
//! | Redirect policy | `reqwest::redirect::Policy::none()` | DNS-poison MITM / open-redirect |
//! | Body size cap | `MAX_CSV_BODY_BYTES` (50 MB) | DoS via gigabyte stream |
//! | Connect timeout | `CSV_CONNECT_TIMEOUT_SECS` (10 s) | Black-hole DNS |
//! | Read timeout | `INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS` (60 s) | Hung TCP |
//! | HTTPS only | `https_only(true)` | downgrade attack |
//! | Content-Type | text/csv ∪ octet-stream ∪ text/plain (reject text/html WAF/404 page) | JSON/HTML error body fed to parser |
//! | URL | `INDEX_CONSTITUENCY_BASE_URL` + slug + `.csv` (slug from the locked allowlist) | open-redirect / typo |
//! | User-Agent | `INDEX_CONSTITUENCY_USER_AGENT` (niftyindices 403s without a browser UA) | bot-wall 403 |
//!
//! ## What this module DOES NOT do (deferred — same split as csv_downloader)
//!
//! - Retry/backoff orchestration (the orchestrator drives it via the
//!   `INDEX_CONSTITUENCY_RETRY_*` constants).
//! - Symbol→Dhan `security_id` resolution (Sub-PR #4 — the ISIN join).
//! - Boot wiring + Telegram/metrics on failure (Sub-PR #6/#10).
//!
//! Ships ONLY the HTTP layer: a hardened `reqwest::Client` + a
//! body-streaming reader bounded by `MAX_CSV_BODY_BYTES`.

#![cfg(feature = "daily_universe_fetcher")]

use std::time::Duration;

use reqwest::{
    Client, ClientBuilder, Response, StatusCode,
    header::{CONTENT_TYPE, HeaderValue, USER_AGENT},
    redirect,
};
use thiserror::Error;
use tickvault_common::constants::{
    CSV_CONNECT_TIMEOUT_SECS, INDEX_CONSTITUENCY_BASE_URL, INDEX_CONSTITUENCY_USER_AGENT,
    INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS, MAX_CSV_BODY_BYTES,
};

/// Allowed Content-Type values. Rejecting `text/html` catches the
/// niftyindices 404 page + Cloudflare bot-wall block page; rejecting
/// `application/json` catches error bodies that would feed garbage to
/// the constituent-CSV parser.
const ALLOWED_CONTENT_TYPES: &[&str] = &["text/csv", "application/octet-stream", "text/plain"];

/// Errors from a single constituent-CSV fetch attempt. The orchestrator
/// (Sub-PR #10) wraps these in the `INDEX_CONSTITUENCY_RETRY_*` ladder.
#[derive(Debug, Error)]
pub enum ConstituencyDownloadError {
    /// reqwest builder / network / TLS / timeout error.
    #[error("HTTP transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// Non-2xx HTTP status (404 = stale slug; 403 = bot-wall; 5xx = transient).
    #[error("HTTP status not OK for slug {slug}: {status}")]
    HttpStatus { slug: String, status: StatusCode },

    /// Response Content-Type not in `ALLOWED_CONTENT_TYPES` (e.g. an
    /// HTML 404 / Cloudflare page).
    #[error("rejected Content-Type for slug {slug}: {content_type}")]
    UnacceptableContentType { slug: String, content_type: String },

    /// Response body exceeded `MAX_CSV_BODY_BYTES`.
    #[error(
        "response body for slug {slug} exceeded cap of {} bytes",
        MAX_CSV_BODY_BYTES
    )]
    BodyTooLarge { slug: String },

    /// Slug contained a character outside the safe `[A-Za-z0-9_-]` set.
    /// All current callers pass compile-time slugs from the locked
    /// `INDEX_CONSTITUENCY_SLUGS`; this is defense-in-depth against a
    /// future untrusted caller (no URL injection into the GET path).
    #[error("invalid slug (illegal characters): {slug}")]
    InvalidSlug { slug: String },
}

/// A slug is safe iff it is non-empty and every byte is ASCII
/// alphanumeric, `_`, or `-` (covers every entry in the locked
/// `INDEX_CONSTITUENCY_SLUGS`, including the hyphenated
/// `ind_niftyfinancialservices25-50list`).
#[must_use]
pub fn is_safe_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Hardened HTTP client for niftyindices.com constituent CSVs.
///
/// Constructed once per orchestrator boot (cold path). Each
/// `fetch_slug()` performs one bounded GET; retries live above.
pub struct ConstituencyDownloader {
    client: Client,
}

impl ConstituencyDownloader {
    /// Build a `ConstituencyDownloader` with the hardening locked in.
    ///
    /// # Errors
    ///
    /// Returns `Err` only if `reqwest::ClientBuilder::build` fails (TLS
    /// root certs unavailable). Network/DNS failures surface in `fetch_slug`.
    pub fn new() -> Result<Self, ConstituencyDownloadError> {
        let client = ClientBuilder::new()
            .redirect(redirect::Policy::none())
            .connect_timeout(Duration::from_secs(CSV_CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(
                INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS,
            ))
            .https_only(true)
            .build()?;
        Ok(Self { client })
    }

    /// The full URL for a slug: `{BASE}{slug}.csv`.
    #[must_use]
    pub fn url_for_slug(slug: &str) -> String {
        format!("{INDEX_CONSTITUENCY_BASE_URL}{slug}.csv")
    }

    /// Perform ONE bounded GET for a single index slug. Returns the raw
    /// CSV bytes on success. The browser-like User-Agent is mandatory —
    /// niftyindices.com returns 403 without it.
    ///
    /// # Errors
    ///
    /// See [`ConstituencyDownloadError`].
    pub async fn fetch_slug(&self, slug: &str) -> Result<Vec<u8>, ConstituencyDownloadError> {
        if !is_safe_slug(slug) {
            return Err(ConstituencyDownloadError::InvalidSlug {
                slug: slug.to_string(),
            });
        }
        let url = Self::url_for_slug(slug);
        let response = self
            .client
            .get(&url)
            .header(USER_AGENT, INDEX_CONSTITUENCY_USER_AGENT)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(ConstituencyDownloadError::HttpStatus {
                slug: slug.to_string(),
                status: response.status(),
            });
        }

        validate_content_type(slug, response.headers().get(CONTENT_TYPE))?;
        read_body_with_cap(slug, response).await
    }

    /// Borrow the inner client (carries the locked-in hardening).
    #[must_use]
    pub fn client(&self) -> &Client {
        &self.client
    }
}

/// Validate the Content-Type against `ALLOWED_CONTENT_TYPES`. A missing
/// header is accepted (some CDNs omit it); an HTML body is rejected so a
/// 404/bot-wall page never reaches the CSV parser.
fn validate_content_type(
    slug: &str,
    header: Option<&HeaderValue>,
) -> Result<(), ConstituencyDownloadError> {
    let Some(value) = header else {
        return Ok(());
    };
    let raw = value
        .to_str()
        .map_err(|_| ConstituencyDownloadError::UnacceptableContentType {
            slug: slug.to_string(),
            content_type: "<invalid utf-8>".to_string(),
        })?;
    let primary = raw.split(';').next().unwrap_or("").trim().to_lowercase();
    if ALLOWED_CONTENT_TYPES.iter().any(|&a| a == primary) {
        Ok(())
    } else {
        Err(ConstituencyDownloadError::UnacceptableContentType {
            slug: slug.to_string(),
            content_type: raw.to_string(),
        })
    }
}

/// Stream the body into a `Vec<u8>` bounded by `MAX_CSV_BODY_BYTES`,
/// aborting mid-stream if the cap is exceeded.
async fn read_body_with_cap(
    slug: &str,
    mut response: Response,
) -> Result<Vec<u8>, ConstituencyDownloadError> {
    let initial_capacity = response
        .content_length()
        .map(|len| len.min(MAX_CSV_BODY_BYTES as u64) as usize)
        .unwrap_or(0);
    let mut body = Vec::with_capacity(initial_capacity);

    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_CSV_BODY_BYTES {
            return Err(ConstituencyDownloadError::BodyTooLarge {
                slug: slug.to_string(),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_for_slug_composes_base_and_csv_suffix() {
        assert_eq!(
            ConstituencyDownloader::url_for_slug("ind_niftytotalmarket_list"),
            "https://www.niftyindices.com/IndexConstituent/ind_niftytotalmarket_list.csv"
        );
    }

    #[test]
    fn base_url_is_niftyindices_constituent_host() {
        assert_eq!(
            INDEX_CONSTITUENCY_BASE_URL,
            "https://www.niftyindices.com/IndexConstituent/"
        );
    }

    #[test]
    fn connect_timeout_is_bounded_and_below_read_timeout() {
        assert_eq!(CSV_CONNECT_TIMEOUT_SECS, 10);
        assert!(CSV_CONNECT_TIMEOUT_SECS < INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS);
    }

    #[test]
    fn all_locked_slugs_are_safe() {
        use tickvault_common::constants::INDEX_CONSTITUENCY_SLUGS;
        for (name, slug) in INDEX_CONSTITUENCY_SLUGS {
            assert!(is_safe_slug(slug), "{name} slug `{slug}` must be safe");
        }
    }

    #[test]
    fn rejects_unsafe_slug() {
        for bad in ["../etc/passwd", "a/b", "a.b", "", "a b", "a;b"] {
            assert!(!is_safe_slug(bad), "`{bad}` must be rejected");
        }
        // The one hyphenated real slug must still pass.
        assert!(is_safe_slug("ind_niftyfinancialservices25-50list"));
    }

    #[test]
    fn rejects_html_content_type() {
        let html = HeaderValue::from_static("text/html; charset=utf-8");
        let err = validate_content_type("ind_x", Some(&html)).unwrap_err();
        assert!(matches!(
            err,
            ConstituencyDownloadError::UnacceptableContentType { .. }
        ));
    }

    #[test]
    fn accepts_text_csv_and_octet_stream_and_missing() {
        for ok in [
            "text/csv",
            "application/octet-stream",
            "text/plain; charset=utf-8",
        ] {
            let hv = HeaderValue::from_str(ok).unwrap();
            assert!(validate_content_type("ind_x", Some(&hv)).is_ok(), "{ok}");
        }
        assert!(validate_content_type("ind_x", None).is_ok());
    }

    #[test]
    fn downloader_constructs_with_hardening() {
        assert!(ConstituencyDownloader::new().is_ok());
    }
}
