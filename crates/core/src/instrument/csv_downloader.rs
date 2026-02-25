//! Instrument CSV download with retry, fallback, and local cache.
//!
//! Downloads Dhan's instrument master CSV using this strategy:
//! 1. Try primary URL with exponential backoff retry.
//! 2. If primary fails → try fallback URL with same retry.
//! 3. If both fail → read cached file from previous successful download.
//! 4. If no cache → error, system cannot start.
//! 5. On success → write to cache directory for next-day fallback.
//! 6. Sanity: response body must exceed minimum expected size.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use backon::{ExponentialBuilder, Retryable};
use reqwest::Client;
use tokio::fs;
use tracing::{info, warn};

use dhan_live_trader_common::constants::*;
use dhan_live_trader_common::error::ApplicationError;

/// Result of a successful CSV download.
#[derive(Debug)]
pub struct CsvDownloadResult {
    /// The full CSV text.
    pub csv_text: String,
    /// Which source provided the CSV: "primary", "fallback", or "cache".
    pub source: String,
}

/// Download the instrument CSV with retry, fallback, and cache.
///
/// # Arguments
/// * `primary_url` — Primary (detailed) CSV URL.
/// * `fallback_url` — Fallback (compact) CSV URL.
/// * `cache_dir` — Directory for caching the last successful download.
/// * `cache_filename` — Filename within `cache_dir`.
/// * `download_timeout_secs` — Per-request timeout.
///
/// # Strategy
/// 1. Try `primary_url` with retries.
/// 2. If all retries fail, try `fallback_url` with retries.
/// 3. If both fail, read from cache file.
/// 4. On success from network, write to cache.
pub async fn download_instrument_csv(
    primary_url: &str,
    fallback_url: &str,
    cache_dir: &str,
    cache_filename: &str,
    download_timeout_secs: u64,
) -> Result<CsvDownloadResult> {
    let client = Client::builder()
        .timeout(Duration::from_secs(download_timeout_secs))
        .build()
        .context("failed to build HTTP client for CSV download")?;

    // Try primary URL
    match download_with_retry(&client, primary_url).await {
        Ok(csv_text) => {
            info!(
                url = primary_url,
                bytes = csv_text.len(),
                "instrument CSV downloaded from primary URL"
            );
            write_cache(cache_dir, cache_filename, &csv_text).await;
            return Ok(CsvDownloadResult {
                csv_text,
                source: "primary".to_owned(),
            });
        }
        Err(primary_error) => {
            warn!(
                url = primary_url,
                %primary_error,
                "primary CSV download failed, trying fallback"
            );
        }
    }

    // Try fallback URL
    match download_with_retry(&client, fallback_url).await {
        Ok(csv_text) => {
            info!(
                url = fallback_url,
                bytes = csv_text.len(),
                "instrument CSV downloaded from fallback URL"
            );
            write_cache(cache_dir, cache_filename, &csv_text).await;
            return Ok(CsvDownloadResult {
                csv_text,
                source: "fallback".to_owned(),
            });
        }
        Err(fallback_error) => {
            warn!(
                url = fallback_url,
                %fallback_error,
                "fallback CSV download failed, trying cache"
            );
        }
    }

    // Try cached file
    let cache_path = PathBuf::from(cache_dir).join(cache_filename);
    match read_cache(&cache_path).await {
        Ok(csv_text) => {
            warn!(
                path = %cache_path.display(),
                bytes = csv_text.len(),
                "using cached instrument CSV (both downloads failed)"
            );
            Ok(CsvDownloadResult {
                csv_text,
                source: "cache".to_owned(),
            })
        }
        Err(cache_error) => {
            bail!(ApplicationError::InstrumentDownloadFailed {
                reason: format!(
                    "all sources failed — primary, fallback, and cache ({})",
                    cache_error
                ),
            });
        }
    }
}

/// Download CSV text from a URL with exponential backoff retry.
async fn download_with_retry(client: &Client, url: &str) -> Result<String> {
    let url_owned = url.to_owned();

    let csv_text = (|| {
        let client = client.clone();
        let url = url_owned.clone();
        async move {
            let response = client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("HTTP request failed for {}", url))?;

            let status = response.status();
            if !status.is_success() {
                bail!("HTTP {} from {}", status, url);
            }

            let body = response
                .text()
                .await
                .with_context(|| format!("failed to read response body from {}", url))?;

            // Sanity check: body should not be suspiciously small
            if body.len() < INSTRUMENT_CSV_MIN_BYTES {
                bail!(
                    "CSV body too small ({} bytes) from {}, expected at least {} bytes",
                    body.len(),
                    url,
                    INSTRUMENT_CSV_MIN_BYTES
                );
            }

            Ok(body)
        }
    })
    .retry(
        ExponentialBuilder::default()
            .with_min_delay(Duration::from_millis(INSTRUMENT_CSV_RETRY_INITIAL_DELAY_MS))
            .with_max_delay(Duration::from_millis(INSTRUMENT_CSV_RETRY_MAX_DELAY_MS))
            .with_max_times(INSTRUMENT_CSV_MAX_DOWNLOAD_RETRIES),
    )
    .notify(|err, dur| {
        warn!(
            %err,
            retry_after_ms = dur.as_millis(),
            "retrying instrument CSV download"
        );
    })
    .await?;

    Ok(csv_text)
}

/// Write CSV text to the cache directory.
///
/// Failures are logged but do not propagate — cache is best-effort.
async fn write_cache(cache_dir: &str, cache_filename: &str, csv_text: &str) {
    let dir = Path::new(cache_dir);

    if let Err(error) = fs::create_dir_all(dir).await {
        warn!(
            path = %dir.display(),
            %error,
            "failed to create CSV cache directory"
        );
        return;
    }

    let cache_path = dir.join(cache_filename);
    if let Err(error) = fs::write(&cache_path, csv_text).await {
        warn!(
            path = %cache_path.display(),
            %error,
            "failed to write CSV cache file"
        );
    } else {
        info!(
            path = %cache_path.display(),
            bytes = csv_text.len(),
            "instrument CSV cached successfully"
        );
    }
}

/// Read CSV text from a cached file.
async fn read_cache(cache_path: &Path) -> Result<String> {
    let csv_text = fs::read_to_string(cache_path)
        .await
        .with_context(|| format!("failed to read cache file {}", cache_path.display()))?;

    if csv_text.len() < INSTRUMENT_CSV_MIN_BYTES {
        bail!(
            "cached CSV too small ({} bytes), expected at least {} bytes",
            csv_text.len(),
            INSTRUMENT_CSV_MIN_BYTES
        );
    }

    Ok(csv_text)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[tokio::test]
    async fn test_write_and_read_cache() {
        let temp_dir = env::temp_dir().join("dlt-test-csv-cache");
        let cache_dir = temp_dir.to_str().unwrap();
        let cache_filename = "test-instruments.csv";

        // Create a fake CSV large enough to pass the size check
        let fake_csv = "H".repeat(INSTRUMENT_CSV_MIN_BYTES + 1);

        write_cache(cache_dir, cache_filename, &fake_csv).await;

        let cache_path = temp_dir.join(cache_filename);
        let result = read_cache(&cache_path).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), fake_csv.len());

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_read_cache_missing_file_returns_error() {
        let path = PathBuf::from("/tmp/dlt-nonexistent-cache-12345/missing.csv");
        let result = read_cache(&path).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_cache_too_small_returns_error() {
        let temp_dir = env::temp_dir().join("dlt-test-csv-small");
        let _ = fs::create_dir_all(&temp_dir).await;
        let cache_path = temp_dir.join("small.csv");
        fs::write(&cache_path, "tiny").await.unwrap();

        let result = read_cache(&cache_path).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("too small"),
            "error must mention too small: {}",
            err_msg
        );

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_download_all_sources_fail_returns_error() {
        let result = download_instrument_csv(
            "http://127.0.0.1:1/nonexistent-primary",
            "http://127.0.0.1:1/nonexistent-fallback",
            "/tmp/dlt-nonexistent-cache-99999",
            "missing.csv",
            2,
        )
        .await;

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("all sources failed"),
            "error must mention all sources: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_download_falls_back_to_cache() {
        let temp_dir = env::temp_dir().join("dlt-test-fallback-cache");
        let cache_dir = temp_dir.to_str().unwrap();
        let cache_filename = "cached-instruments.csv";

        let fake_csv = "CACHED_DATA,".repeat(INSTRUMENT_CSV_MIN_BYTES);
        write_cache(cache_dir, cache_filename, &fake_csv).await;

        let result = download_instrument_csv(
            "http://127.0.0.1:1/nonexistent-primary",
            "http://127.0.0.1:1/nonexistent-fallback",
            cache_dir,
            cache_filename,
            2,
        )
        .await;

        assert!(result.is_ok());
        let download_result = result.unwrap();
        assert_eq!(download_result.source, "cache");
        assert_eq!(download_result.csv_text.len(), fake_csv.len());

        let _ = fs::remove_dir_all(&temp_dir).await;
    }
}
