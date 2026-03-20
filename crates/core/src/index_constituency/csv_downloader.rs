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

use dhan_live_trader_common::config::IndexConstituencyConfig;
use dhan_live_trader_common::constants::{
    INDEX_CONSTITUENCY_BASE_URL, INDEX_CONSTITUENCY_MIN_INDICES,
    INDEX_CONSTITUENCY_RETRY_MAX_DELAY_SECS, INDEX_CONSTITUENCY_RETRY_MAX_TIMES,
    INDEX_CONSTITUENCY_RETRY_MIN_DELAY_SECS, INDEX_CONSTITUENCY_USER_AGENT,
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
    let mut handles = Vec::with_capacity(slugs.len());

    for (name, slug) in slugs {
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

    let mut results = Vec::with_capacity(slugs.len());
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
            let response = client.get(&url).send().await?;

            let status = response.status();
            if !status.is_success() {
                anyhow::bail!("HTTP {status} for {url}");
            }

            let text = response.text().await?;
            if text.is_empty() {
                anyhow::bail!("empty response body for {url}");
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
        warn!(
            index = %name_owned,
            error = %err,
            retry_in = ?dur,
            "retrying constituency CSV download"
        );
    })
    .await
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
