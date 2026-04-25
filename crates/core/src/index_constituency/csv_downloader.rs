//! Concurrent CSV downloader for niftyindices.com index constituents.
//!
//! Downloads multiple index CSVs in parallel using a semaphore to limit
//! concurrency. Individual failures are logged and skipped — partial
//! results are always returned.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use backon::{ExponentialBuilder, Retryable};
use reqwest::Client;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use tickvault_common::config::IndexConstituencyConfig;
use tickvault_common::constants::{
    INDEX_CONSTITUENCY_BASE_URL, INDEX_CONSTITUENCY_KNOWN_STALE_SLUGS,
    INDEX_CONSTITUENCY_MIN_INDICES, INDEX_CONSTITUENCY_RETRY_MAX_DELAY_SECS,
    INDEX_CONSTITUENCY_RETRY_MAX_TIMES, INDEX_CONSTITUENCY_RETRY_MIN_DELAY_SECS,
    INDEX_CONSTITUENCY_USER_AGENT,
};

/// Download index constituency CSVs concurrently.
///
/// Returns a `Vec` of successfully downloaded `(index_name, csv_text)` pairs.
/// Individual CSV failures are logged and skipped — never fails the batch.
pub async fn download_constituency_csvs(
    config: &IndexConstituencyConfig,
    slugs: &[(&str, &str)],
) -> Vec<(String, String)> {
    if slugs.is_empty() {
        return Vec::new();
    }

    // Q8 (2026-04-24): filter out known-stale slugs before dispatching
    // downloads. Skipping at the source means no HTTP request, no 404
    // retry, no "received HTML instead of CSV" log line, no aggregated
    // "14 failed" WARN. See `INDEX_CONSTITUENCY_KNOWN_STALE_SLUGS` for
    // the operator runbook to remove a slug from the stale set after
    // verifying the correct URL at niftyindices.com in a browser.
    //
    // 2026-04-25 (PR #357 audit follow-up): the original log was at INFO
    // level which never reached `data/logs/errors.log`. Operator asked
    // "why these 14 of them are missing dude why didnt you fix that bro
    // why why why we need all the indices right dude" — the answer is the
    // queue is still pending operator action. Surfacing this at WARN with
    // the full slug list + index names makes the queue grep-able from
    // `errors.log` and visible on the `make doctor` summary so it's not
    // forgotten. Stays WARN (NOT ERROR) — no Telegram page; this is a
    // queue reminder, not an incident.
    let stale_skipped: Vec<&str> = slugs
        .iter()
        .filter_map(|(name, s)| {
            INDEX_CONSTITUENCY_KNOWN_STALE_SLUGS
                .contains(s)
                .then_some(*name)
        })
        .collect();
    let stale_skipped_count = stale_skipped.len();
    if stale_skipped_count > 0 {
        // O(1) EXEMPT: cold path — runs once per daily boot, not per tick.
        let pending_index_names = stale_skipped.join(", ");
        warn!(
            skipped = stale_skipped_count,
            pending_indices = %pending_index_names,
            "constituency downloader: {stale_skipped_count} indices still queued for \
             operator URL re-discovery (niftyindices.com renamed these slugs; see \
             INDEX_CONSTITUENCY_KNOWN_STALE_SLUGS in crates/common/src/constants.rs \
             for the re-enable runbook). Pending: {pending_index_names}"
        );
    }
    let active_slugs: Vec<(&str, &str)> = slugs
        .iter()
        .copied()
        .filter(|(_, s)| !INDEX_CONSTITUENCY_KNOWN_STALE_SLUGS.contains(s))
        .collect();

    let client = match Client::builder()
        .timeout(Duration::from_secs(config.download_timeout_secs))
        .user_agent(INDEX_CONSTITUENCY_USER_AGENT)
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(error = %err, "failed to build HTTP client for constituency download");
            return Vec::new();
        }
    };

    let semaphore = Arc::new(Semaphore::new(config.max_concurrent_downloads));
    let mut handles = Vec::with_capacity(active_slugs.len());

    for (name, slug) in &active_slugs {
        let client = client.clone();
        let semaphore = Arc::clone(&semaphore);
        let name = name.to_string();
        let url = build_download_url(slug);
        let delay_ms = config.inter_batch_delay_ms;

        handles.push(tokio::spawn(async move {
            let _permit = semaphore.acquire().await;
            let result = download_single_csv(&client, &name, &url).await;
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            (name, result)
        }));
    }

    let mut results = Vec::with_capacity(active_slugs.len());
    let mut failed_count = 0usize;

    for handle in handles {
        match handle.await {
            Ok((name, Ok(csv_text))) => {
                results.push((name, csv_text));
            }
            Ok((name, Err(err))) => {
                warn!(index = %name, error = %err, "failed to download constituency CSV");
                failed_count = failed_count.saturating_add(1);
            }
            Err(err) => {
                warn!(error = %err, "constituency download task panicked");
                failed_count = failed_count.saturating_add(1);
            }
        }
    }

    if results.len() < INDEX_CONSTITUENCY_MIN_INDICES {
        warn!(
            downloaded = results.len(),
            failed = failed_count,
            minimum = INDEX_CONSTITUENCY_MIN_INDICES,
            "fewer than minimum indices downloaded — constituency map may be incomplete"
        );
    } else {
        info!(
            downloaded = results.len(),
            failed = failed_count,
            "constituency CSVs downloaded"
        );
    }

    results
}

/// Build the full download URL for a given slug.
pub fn build_download_url(slug: &str) -> String {
    format!("{INDEX_CONSTITUENCY_BASE_URL}{slug}.csv")
}

/// Download a single CSV with retry (2 attempts, exponential backoff).
async fn download_single_csv(client: &Client, name: &str, url: &str) -> Result<String> {
    let name_owned = name.to_string();
    let url_owned = url.to_string();
    let client = client.clone();

    (|| {
        let client = client.clone();
        let url = url_owned.clone();
        async move {
            let response = client
                .get(&url)
                .header("Accept", "text/csv, application/csv, text/plain, */*")
                // niftyindices.com gates CSV downloads behind a same-origin
                // Referer; without it, Cloudflare returns an HTML challenge
                // page (HTTP 200 + HTML body), which the `<`/`<!DOCTYPE`
                // check below rejects as "received HTML instead of CSV".
                .header("Referer", INDEX_CONSTITUENCY_BASE_URL)
                .send()
                .await?;

            let status = response.status();
            if !status.is_success() {
                anyhow::bail!("HTTP {status} for {url}");
            }

            let text = response.text().await?;
            if text.is_empty() {
                anyhow::bail!("empty response body for {url}");
            }

            // Reject HTML responses — niftyindices.com returns 200 + HTML
            // for non-existent slugs AND for Cloudflare-throttled requests.
            // Diagnostic enhancement (2026-04-20): embed a short signature
            // of the response body so the operator can tell WHICH kind of
            // HTML came back — a Cloudflare "checking your browser" page
            // needs a retry with longer backoff; a generic 404-style HTML
            // page needs a slug correction. Without the snippet we spend
            // 15+ minutes re-running in prod to figure out which it is.
            let trimmed = text.trim_start();
            if trimmed.starts_with('<') || trimmed.starts_with("<!DOCTYPE") {
                let signature = classify_html_body(trimmed);
                let snippet: String = trimmed.chars().take(160).collect();
                anyhow::bail!(
                    "received HTML instead of CSV for {url} — kind={signature} — snippet={snippet:?}"
                );
            }

            Ok(text)
        }
    })
    .retry(
        ExponentialBuilder::default()
            .with_min_delay(Duration::from_secs(INDEX_CONSTITUENCY_RETRY_MIN_DELAY_SECS))
            .with_max_delay(Duration::from_secs(INDEX_CONSTITUENCY_RETRY_MAX_DELAY_SECS))
            .with_max_times(INDEX_CONSTITUENCY_RETRY_MAX_TIMES),
    )
    .notify(move |err, dur| {
        // Per-retry events are noisy (see observability-architecture.md —
        // the caller emits a single aggregated WARN after all downloads
        // complete, so the retry-in-progress detail belongs at DEBUG).
        // Use `RUST_LOG=tickvault_core::index_constituency=debug` to
        // re-enable in a triage session.
        tracing::debug!(
            index = %name_owned,
            error = %err,
            retry_in = ?dur,
            "retrying constituency CSV download"
        );
    })
    .await
}

/// Classifies an HTML response body into a short operator-friendly signature
/// so the retry log can distinguish:
/// - `cloudflare` — WAF / bot-challenge page → symptom of rate-limit or IP ban
/// - `missing` — 200 + "404-not-found" looking HTML → symptom of wrong slug
/// - `server_error` — 200 + server error page (rare but seen for NSE)
/// - `unknown` — HTML we haven't classified
///
/// This is a pure heuristic — we're optimising for fast triage, not strict
/// accuracy. Each signature maps to a known remediation action in the runbook
/// at `docs/runbooks/constituency-csv-html-rejection.md` (to be added).
#[must_use]
pub fn classify_html_body(body: &str) -> &'static str {
    let lower = body.to_ascii_lowercase();
    if lower.contains("cloudflare")
        || lower.contains("checking your browser")
        || lower.contains("attention required")
        || lower.contains("ray id")
        || lower.contains("cf-ray")
    {
        "cloudflare"
    } else if lower.contains("not found")
        || lower.contains("404")
        || lower.contains("page not found")
    {
        "missing"
    } else if lower.contains("internal server error")
        || lower.contains("503")
        || lower.contains("service unavailable")
    {
        "server_error"
    } else {
        "unknown"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_download_url_construction() {
        let url = build_download_url("ind_nifty50list");
        assert_eq!(
            url,
            "https://www.niftyindices.com/IndexConstituent/ind_nifty50list.csv"
        );
    }

    /// Q8 regression (2026-04-24): every slug in
    /// `INDEX_CONSTITUENCY_KNOWN_STALE_SLUGS` MUST also appear in
    /// `INDEX_CONSTITUENCY_SLUGS`. Otherwise the filter becomes a no-op
    /// guard that masks a typo, and the operator's "remove from stale
    /// after verifying" workflow silently breaks.
    #[test]
    fn test_known_stale_slugs_all_exist_in_main_list() {
        use tickvault_common::constants::{
            INDEX_CONSTITUENCY_KNOWN_STALE_SLUGS, INDEX_CONSTITUENCY_SLUGS,
        };
        let active_slugs: std::collections::HashSet<&str> =
            INDEX_CONSTITUENCY_SLUGS.iter().map(|(_, s)| *s).collect();
        for stale in INDEX_CONSTITUENCY_KNOWN_STALE_SLUGS {
            assert!(
                active_slugs.contains(stale),
                "stale slug `{stale}` not found in INDEX_CONSTITUENCY_SLUGS \
                 — this means the skip-filter is silently doing nothing \
                 for that entry. Either add it to the main list or \
                 remove it from the stale list."
            );
        }
    }

    /// Q8 regression: the stale list is a set (no duplicates). Duplicate
    /// entries hide bookkeeping errors during manual slug re-audits.
    #[test]
    fn test_known_stale_slugs_are_unique() {
        use tickvault_common::constants::INDEX_CONSTITUENCY_KNOWN_STALE_SLUGS;
        let mut seen = std::collections::HashSet::new();
        for s in INDEX_CONSTITUENCY_KNOWN_STALE_SLUGS {
            assert!(
                seen.insert(*s),
                "duplicate stale slug `{s}` — remove the dup"
            );
        }
    }

    /// Q8 regression: downloader source must reference
    /// `INDEX_CONSTITUENCY_KNOWN_STALE_SLUGS.contains(...)` so the
    /// skip-filter stays wired. Catches accidental removal during
    /// refactors.
    #[test]
    fn test_downloader_filters_known_stale_slugs() {
        let src = include_str!("csv_downloader.rs");
        assert!(
            src.contains("INDEX_CONSTITUENCY_KNOWN_STALE_SLUGS.contains"),
            "downloader MUST filter against INDEX_CONSTITUENCY_KNOWN_STALE_SLUGS. \
             If this regresses, every boot fires 14 redundant WARNs for URLs \
             already known to 404."
        );
    }

    /// 2026-04-25 (PR #357 audit follow-up): the stale-slug skip log was
    /// upgraded from `info!` to `warn!` so the operator's "why are 14
    /// missing" question is grep-able from `data/logs/errors.log` and
    /// visible to `make doctor`. This source-scan ratchet blocks any
    /// regression back to INFO level, which would silently bury the
    /// queue under INFO-level boot noise.
    #[test]
    fn test_stale_slug_skip_log_is_warn_not_info() {
        let src = include_str!("csv_downloader.rs");
        // The skip block must use warn! and include both the skipped count
        // and the pending_indices field so the operator sees the queue
        // names directly in the log line.
        assert!(
            src.contains("warn!(\n            skipped = stale_skipped_count,\n            pending_indices = %pending_index_names,"),
            "stale-slug skip log MUST be warn! with skipped + pending_indices \
             fields so the operator queue is grep-able from errors.log. \
             Reverting to info!() hides the queue."
        );
    }

    /// Companion ratchet: the WARN message must include the literal
    /// "queued for operator URL re-discovery" so monitoring + doctor
    /// scripts can grep for the queue without false positives from
    /// other index_constituency log lines.
    #[test]
    fn test_stale_slug_skip_log_contains_queue_marker_phrase() {
        let src = include_str!("csv_downloader.rs");
        assert!(
            src.contains("queued for \\\n             operator URL re-discovery"),
            "stale-slug skip log MUST contain 'queued for operator URL \
             re-discovery' as a stable grep marker for `make doctor` and \
             the operator runbook."
        );
    }

    /// Ratchet: niftyindices.com serves CSVs only when the request carries
    /// a same-origin Referer. Without it Cloudflare returns an HTML WAF
    /// challenge page and every constituency download fails with
    /// "received HTML instead of CSV". This source-scan test blocks
    /// regressions by asserting the `.header("Referer", ...)` call is
    /// still present in the single request builder.
    #[test]
    fn test_single_csv_request_includes_referer_header() {
        let source = include_str!("csv_downloader.rs");
        assert!(
            source.contains(".header(\"Referer\", INDEX_CONSTITUENCY_BASE_URL)"),
            "Referer header must be set on the single CSV request — \
             niftyindices.com returns HTML without it"
        );
    }

    // -----------------------------------------------------------------------
    // classify_html_body — operator triage tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_html_body_cloudflare_challenge_is_detected() {
        let body = r#"<!DOCTYPE html><html><head><title>Just a moment...</title></head><body>Checking your browser before accessing niftyindices.com. This process is automatic. Cloudflare Ray ID: 8a1b2c3d4e5f6a7b</body></html>"#;
        assert_eq!(classify_html_body(body), "cloudflare");
    }

    #[test]
    fn test_classify_html_body_attention_required_is_cloudflare() {
        let body = r#"<html><title>Attention Required!</title><body>Please solve the captcha.</body></html>"#;
        assert_eq!(classify_html_body(body), "cloudflare");
    }

    #[test]
    fn test_classify_html_body_cf_ray_alone_is_cloudflare() {
        let body = r#"<html><head><meta name="cf-ray" content="xxx"></head></html>"#;
        assert_eq!(classify_html_body(body), "cloudflare");
    }

    #[test]
    fn test_classify_html_body_404_is_missing() {
        let body = r#"<!DOCTYPE html><html><head><title>404 Not Found</title></head><body>Page not found</body></html>"#;
        assert_eq!(classify_html_body(body), "missing");
    }

    #[test]
    fn test_classify_html_body_page_not_found_is_missing() {
        let body = r#"<html><body>Sorry, this page not found</body></html>"#;
        assert_eq!(classify_html_body(body), "missing");
    }

    #[test]
    fn test_classify_html_body_503_is_server_error() {
        let body = r#"<html><title>503 Service Unavailable</title></html>"#;
        assert_eq!(classify_html_body(body), "server_error");
    }

    #[test]
    fn test_classify_html_body_internal_error_is_server_error() {
        let body = r#"<html><body>Internal Server Error</body></html>"#;
        assert_eq!(classify_html_body(body), "server_error");
    }

    #[test]
    fn test_classify_html_body_generic_html_is_unknown() {
        let body = r#"<html><body>Some random HTML page content</body></html>"#;
        assert_eq!(classify_html_body(body), "unknown");
    }

    #[test]
    fn test_classify_html_body_mixed_case_detected() {
        // Real-world Cloudflare pages mix case freely; the classifier uses
        // `to_ascii_lowercase` so nothing slips through.
        let body = r#"<!DOCTYPE HTML><HTML><TITLE>CLOUDFLARE challenge</TITLE></HTML>"#;
        assert_eq!(classify_html_body(body), "cloudflare");
    }

    #[test]
    fn test_classify_html_body_empty_is_unknown() {
        assert_eq!(classify_html_body(""), "unknown");
    }

    #[test]
    fn test_classify_html_body_cloudflare_wins_over_404_when_both_present() {
        // A Cloudflare challenge page that happens to render a 404-looking
        // title should still classify as cloudflare — the operator needs
        // to act on the rate-limit remediation, not change the slug.
        let body = r#"<html><title>404</title><body>Cloudflare Ray ID: xxx</body></html>"#;
        assert_eq!(classify_html_body(body), "cloudflare");
    }

    #[tokio::test]
    async fn test_download_empty_slug_list_returns_empty() {
        let config = IndexConstituencyConfig::default();
        let result = download_constituency_csvs(&config, &[]).await;
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // Additional coverage: build_download_url
    // -----------------------------------------------------------------------

    #[test]
    fn test_download_url_includes_csv_extension() {
        let url = build_download_url("test_slug");
        assert!(url.ends_with(".csv"), "URL must end with .csv");
    }

    #[test]
    fn test_download_url_empty_slug() {
        let url = build_download_url("");
        assert_eq!(url, "https://www.niftyindices.com/IndexConstituent/.csv");
    }

    #[test]
    fn test_download_url_special_characters_in_slug() {
        let url = build_download_url("ind_nifty%20next50list");
        assert!(url.contains("ind_nifty%20next50list.csv"));
    }

    #[test]
    fn test_download_url_uses_correct_base_url() {
        let url = build_download_url("anything");
        assert!(
            url.starts_with(INDEX_CONSTITUENCY_BASE_URL),
            "URL must start with base URL constant"
        );
    }

    #[test]
    fn test_download_url_various_slugs() {
        let slugs = [
            "ind_nifty50list",
            "ind_niftybanklist",
            "ind_niftyfinancelist",
            "ind_niftyITlist",
        ];
        for slug in slugs {
            let url = build_download_url(slug);
            assert!(url.contains(slug));
            assert!(url.ends_with(".csv"));
        }
    }

    // -----------------------------------------------------------------------
    // Additional coverage: download_constituency_csvs with unreachable URLs
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_download_unreachable_urls_returns_empty() {
        let config = IndexConstituencyConfig {
            enabled: true,
            download_timeout_secs: 1,
            max_concurrent_downloads: 2,
            inter_batch_delay_ms: 0,
        };
        // Use URLs that will fail immediately (unreachable port)
        let slugs: &[(&str, &str)] = &[
            ("Test1", "http://127.0.0.1:1/fake1"),
            ("Test2", "http://127.0.0.1:1/fake2"),
        ];
        // Note: build_download_url won't be called here as slugs are full URLs
        // but the download will attempt them anyway. Actually, the function builds
        // URLs from slugs, so the real URL would be
        // INDEX_CONSTITUENCY_BASE_URL + slug + ".csv" which won't resolve.
        let result = download_constituency_csvs(&config, slugs).await;
        assert!(
            result.is_empty(),
            "unreachable URLs should return empty results"
        );
    }

    #[tokio::test]
    async fn test_download_single_slug_unreachable() {
        let config = IndexConstituencyConfig {
            enabled: true,
            download_timeout_secs: 1,
            max_concurrent_downloads: 1,
            inter_batch_delay_ms: 0,
        };
        let slugs: &[(&str, &str)] = &[("Nifty 50", "nonexistent_slug_xyz")];
        let result = download_constituency_csvs(&config, slugs).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_download_with_inter_batch_delay() {
        let config = IndexConstituencyConfig {
            enabled: true,
            download_timeout_secs: 1,
            max_concurrent_downloads: 1,
            inter_batch_delay_ms: 50, // Non-zero delay
        };
        let slugs: &[(&str, &str)] = &[("Test", "nonexistent_slug_delay")];
        let start = std::time::Instant::now();
        let result = download_constituency_csvs(&config, slugs).await;
        // Should still complete despite delay (download fails fast)
        assert!(result.is_empty());
        // The delay is applied after the download, so total time should be reasonable
        assert!(
            start.elapsed().as_secs() < 30,
            "should complete within 30 seconds"
        );
    }

    #[tokio::test]
    async fn test_download_with_zero_delay() {
        let config = IndexConstituencyConfig {
            enabled: true,
            download_timeout_secs: 1,
            max_concurrent_downloads: 2,
            inter_batch_delay_ms: 0, // Zero delay
        };
        let slugs: &[(&str, &str)] = &[("A", "slug_a"), ("B", "slug_b")];
        let result = download_constituency_csvs(&config, slugs).await;
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // Additional coverage: URL construction determinism
    // -----------------------------------------------------------------------

    #[test]
    fn test_download_url_deterministic() {
        let url1 = build_download_url("test");
        let url2 = build_download_url("test");
        assert_eq!(url1, url2, "URL construction must be deterministic");
    }

    #[test]
    fn test_download_url_different_slugs_different_urls() {
        let url1 = build_download_url("slug_a");
        let url2 = build_download_url("slug_b");
        assert_ne!(url1, url2, "different slugs must produce different URLs");
    }
}
