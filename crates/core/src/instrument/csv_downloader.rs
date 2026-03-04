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
    let primary_error = match download_with_retry(&client, primary_url).await {
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
        Err(err) => {
            warn!(
                url = primary_url,
                %err,
                "primary CSV download failed, trying fallback"
            );
            err
        }
    };

    // Try fallback URL
    let fallback_error = match download_with_retry(&client, fallback_url).await {
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
        Err(err) => {
            warn!(
                url = fallback_url,
                %err,
                "fallback CSV download failed, trying cache"
            );
            err
        }
    };

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
                    "all sources failed — primary: {}, fallback: {}, cache: {}",
                    primary_error, fallback_error, cache_error
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

/// Load instrument CSV from local cache only (no HTTP download).
///
/// Used when the freshness marker confirms today's build already completed.
/// Avoids network overhead by reading the cached copy from the last successful download.
///
/// # Errors
/// Returns error if the cache file does not exist or is too small.
pub async fn load_cached_csv(cache_dir: &str, cache_filename: &str) -> Result<CsvDownloadResult> {
    let cache_path = PathBuf::from(cache_dir).join(cache_filename);
    let csv_text = read_cache(&cache_path)
        .await
        .with_context(|| format!("failed to load cached CSV from {}", cache_path.display()))?;
    info!(
        path = %cache_path.display(),
        bytes = csv_text.len(),
        "instrument CSV loaded from cache"
    );
    Ok(CsvDownloadResult {
        csv_text,
        source: "cache".to_owned(),
    })
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
    async fn test_load_cached_csv_success() {
        let temp_dir = env::temp_dir().join("dlt-test-load-cached");
        let cache_dir = temp_dir.to_str().unwrap();
        let cache_filename = "cached-load.csv";

        let fake_csv = "CACHED,".repeat(INSTRUMENT_CSV_MIN_BYTES);
        write_cache(cache_dir, cache_filename, &fake_csv).await;

        let result = load_cached_csv(cache_dir, cache_filename).await;
        assert!(result.is_ok());
        let dr = result.unwrap();
        assert_eq!(dr.source, "cache");
        assert_eq!(dr.csv_text.len(), fake_csv.len());

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_load_cached_csv_missing_returns_error() {
        let result = load_cached_csv("/tmp/dlt-nonexistent-cache-77777", "nonexistent.csv").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_download_falls_back_to_cache() {
        let temp_dir = env::temp_dir().join(format!(
            "dlt-test-fallback-cache-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let cache_dir = temp_dir.to_str().unwrap();
        let cache_filename = "cached-instruments.csv";

        // Ensure clean state before writing cache.
        let _ = fs::remove_dir_all(&temp_dir).await;

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

    // --- download_with_retry tests using a local TCP server ---

    /// Spawn a minimal HTTP server that responds with the given status and body.
    async fn spawn_test_http_server(status: u16, body: &str) -> String {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}", addr);

        let response_body = body.to_owned();
        tokio::spawn(async move {
            // Accept a single connection (enough for download_with_retry with retries)
            // We accept multiple connections since retry will re-connect.
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let body_clone = response_body.clone();
                tokio::spawn(async move {
                    // Read the request (discard it)
                    let mut buf = vec![0u8; 4096];
                    let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;

                    // Write a minimal HTTP response
                    let response = format!(
                        "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        status,
                        body_clone.len(),
                        body_clone
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        url
    }

    #[tokio::test]
    async fn test_download_with_retry_success_from_primary() {
        let fake_csv = "X".repeat(INSTRUMENT_CSV_MIN_BYTES + 100);
        let url = spawn_test_http_server(200, &fake_csv).await;

        let temp_dir = env::temp_dir().join("dlt-test-dl-primary-ok");
        let cache_dir = temp_dir.to_str().unwrap();

        let result = download_instrument_csv(
            &url,
            "http://127.0.0.1:1/nonexistent-fallback",
            cache_dir,
            "test.csv",
            10,
        )
        .await;

        assert!(
            result.is_ok(),
            "primary download should succeed: {:?}",
            result.err()
        );
        let download_result = result.unwrap();
        assert_eq!(download_result.source, "primary");
        assert_eq!(download_result.csv_text.len(), fake_csv.len());

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_download_with_retry_primary_fails_fallback_succeeds() {
        let fake_csv = "Y".repeat(INSTRUMENT_CSV_MIN_BYTES + 100);
        let fallback_url = spawn_test_http_server(200, &fake_csv).await;

        let temp_dir = env::temp_dir().join("dlt-test-dl-fallback-ok");
        let cache_dir = temp_dir.to_str().unwrap();

        let result = download_instrument_csv(
            "http://127.0.0.1:1/nonexistent-primary",
            &fallback_url,
            cache_dir,
            "test.csv",
            10,
        )
        .await;

        assert!(
            result.is_ok(),
            "fallback download should succeed: {:?}",
            result.err()
        );
        let download_result = result.unwrap();
        assert_eq!(download_result.source, "fallback");

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_download_with_retry_http_error_status() {
        // Server returns 500 — download_with_retry should fail after retries
        let url = spawn_test_http_server(500, "Internal Server Error").await;

        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let result = download_with_retry(&client, &url).await;
        assert!(result.is_err(), "HTTP 500 should cause retry exhaustion");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("500"),
            "error should mention HTTP 500: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_download_with_retry_body_too_small() {
        // Server returns 200 but body is too small
        let url = spawn_test_http_server(200, "tiny").await;

        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let result = download_with_retry(&client, &url).await;
        assert!(
            result.is_err(),
            "too-small body should cause retry exhaustion"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("too small"),
            "error should mention too small: {}",
            err_msg
        );
    }

    // --- write_cache error paths ---

    #[tokio::test]
    async fn test_write_cache_creates_directory_if_missing() {
        let temp_dir = env::temp_dir().join("dlt-test-write-cache-mkdir");
        let nested_dir = temp_dir.join("a").join("b").join("c");
        let cache_dir = nested_dir.to_str().unwrap();
        let fake_csv = "Z".repeat(100);

        // Should not panic — write_cache is best-effort
        write_cache(cache_dir, "test.csv", &fake_csv).await;

        // Verify the file was written
        let cache_path = nested_dir.join("test.csv");
        let content = fs::read_to_string(&cache_path).await;
        assert!(content.is_ok(), "cache file should have been created");
        assert_eq!(content.unwrap().len(), 100);

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_write_cache_overwrite_existing() {
        let temp_dir = env::temp_dir().join("dlt-test-write-cache-overwrite");
        let cache_dir = temp_dir.to_str().unwrap();

        write_cache(cache_dir, "test.csv", "first content").await;
        write_cache(cache_dir, "test.csv", "second content").await;

        let cache_path = temp_dir.join("test.csv");
        let content = fs::read_to_string(&cache_path).await.unwrap();
        assert_eq!(content, "second content");

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    // --- download_instrument_csv writes cache on success ---

    #[tokio::test]
    async fn test_download_writes_cache_on_primary_success() {
        let fake_csv = "C".repeat(INSTRUMENT_CSV_MIN_BYTES + 50);
        let url = spawn_test_http_server(200, &fake_csv).await;

        let temp_dir = env::temp_dir().join("dlt-test-dl-cache-write");
        let cache_dir = temp_dir.to_str().unwrap();
        let cache_filename = "cached-after-dl.csv";

        let result = download_instrument_csv(
            &url,
            "http://127.0.0.1:1/nonexistent",
            cache_dir,
            cache_filename,
            10,
        )
        .await;

        assert!(result.is_ok());

        // Verify cache was written
        let cache_path = temp_dir.join(cache_filename);
        let cached = fs::read_to_string(&cache_path).await;
        assert!(
            cached.is_ok(),
            "cache file should exist after successful download"
        );
        assert_eq!(cached.unwrap().len(), fake_csv.len());

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    // --- Additional coverage tests ---

    #[tokio::test]
    async fn test_download_with_retry_success_returns_body() {
        let fake_csv = "D".repeat(INSTRUMENT_CSV_MIN_BYTES + 200);
        let url = spawn_test_http_server(200, &fake_csv).await;

        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let result = download_with_retry(&client, &url).await;
        assert!(result.is_ok(), "200 with valid body should succeed");
        assert_eq!(result.unwrap().len(), fake_csv.len());
    }

    #[tokio::test]
    async fn test_download_with_retry_404_status_fails() {
        let url = spawn_test_http_server(404, "Not Found").await;

        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let result = download_with_retry(&client, &url).await;
        assert!(result.is_err(), "HTTP 404 should cause failure");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("404"),
            "error should mention HTTP 404: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_download_with_retry_403_status_fails() {
        let url = spawn_test_http_server(403, "Forbidden").await;

        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let result = download_with_retry(&client, &url).await;
        assert!(result.is_err(), "HTTP 403 should cause failure");
    }

    #[tokio::test]
    async fn test_download_primary_success_writes_cache() {
        let fake_csv = "E".repeat(INSTRUMENT_CSV_MIN_BYTES + 50);
        let url = spawn_test_http_server(200, &fake_csv).await;

        let temp_dir = env::temp_dir().join("dlt-test-dl-primary-cache-verify");
        let cache_dir = temp_dir.to_str().unwrap();
        let cache_filename = "verify-cache.csv";

        let result = download_instrument_csv(
            &url,
            "http://127.0.0.1:1/nonexistent",
            cache_dir,
            cache_filename,
            10,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap().source, "primary");

        // Verify the cache file was created
        let cache_path = temp_dir.join(cache_filename);
        let cached = fs::read_to_string(&cache_path).await;
        assert!(
            cached.is_ok(),
            "cache file should exist after primary download"
        );

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_download_fallback_success_writes_cache() {
        let fake_csv = "F".repeat(INSTRUMENT_CSV_MIN_BYTES + 50);
        let fallback_url = spawn_test_http_server(200, &fake_csv).await;

        let temp_dir = env::temp_dir().join("dlt-test-dl-fallback-cache-verify");
        let cache_dir = temp_dir.to_str().unwrap();
        let cache_filename = "verify-fallback-cache.csv";

        let result = download_instrument_csv(
            "http://127.0.0.1:1/nonexistent-primary",
            &fallback_url,
            cache_dir,
            cache_filename,
            10,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap().source, "fallback");

        // Verify the cache file was created
        let cache_path = temp_dir.join(cache_filename);
        let cached = fs::read_to_string(&cache_path).await;
        assert!(
            cached.is_ok(),
            "cache file should exist after fallback download"
        );

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_csv_download_result_debug_impl() {
        let result = CsvDownloadResult {
            csv_text: "test".to_owned(),
            source: "primary".to_owned(),
        };
        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("primary"));
    }

    #[tokio::test]
    async fn test_read_cache_exactly_min_bytes_succeeds() {
        let temp_dir = env::temp_dir().join("dlt-test-csv-exact-min");
        let _ = fs::create_dir_all(&temp_dir).await;
        let cache_path = temp_dir.join("exact-min.csv");

        let exact_content = "G".repeat(INSTRUMENT_CSV_MIN_BYTES);
        fs::write(&cache_path, &exact_content).await.unwrap();

        let result = read_cache(&cache_path).await;
        assert!(
            result.is_ok(),
            "exactly INSTRUMENT_CSV_MIN_BYTES should succeed"
        );
        assert_eq!(result.unwrap().len(), INSTRUMENT_CSV_MIN_BYTES);

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_read_cache_one_below_min_bytes_fails() {
        let temp_dir = env::temp_dir().join("dlt-test-csv-below-min");
        let _ = fs::create_dir_all(&temp_dir).await;
        let cache_path = temp_dir.join("below-min.csv");

        let content = "H".repeat(INSTRUMENT_CSV_MIN_BYTES - 1);
        fs::write(&cache_path, &content).await.unwrap();

        let result = read_cache(&cache_path).await;
        assert!(result.is_err(), "one below min should fail");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("too small"),
            "error must mention too small: {}",
            err_msg
        );

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_download_with_retry_connection_refused() {
        // Use a port that's definitely not listening
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();

        let result = download_with_retry(&client, "http://127.0.0.1:1/nothing").await;
        assert!(result.is_err(), "connection refused should fail");
    }

    #[tokio::test]
    async fn test_write_cache_best_effort_no_panic_on_readonly_path() {
        // write_cache should not panic even with invalid paths
        // /dev/null/subdir is not writable — this should silently fail
        write_cache("/proc/1/nonexistent", "test.csv", "content").await;
        // No assertion needed — just verify no panic
    }

    #[tokio::test]
    async fn test_write_cache_file_write_failure() {
        // Create a directory where the "file" name is actually a directory,
        // causing fs::write to fail (covers lines 201-202).
        let temp_dir = env::temp_dir().join("dlt-test-write-cache-file-fail");
        let cache_dir = temp_dir.to_str().unwrap();
        let _ = fs::create_dir_all(&temp_dir).await;

        // Create a directory with the same name as the target file
        let file_as_dir = temp_dir.join("test.csv");
        let _ = fs::create_dir_all(&file_as_dir).await;

        // write_cache should not panic — it logs warn and continues
        write_cache(cache_dir, "test.csv", "content").await;

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_download_with_retry_triggers_retry_notify_callback() {
        // Server returns 500 on every request. The retry logic should invoke
        // the notify callback (line 175) for each retry before giving up.
        let url = spawn_test_http_server(500, "Internal Server Error").await;

        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        // download_with_retry retries with exponential backoff.
        // Each retry invokes the notify callback.
        let result = download_with_retry(&client, &url).await;
        assert!(result.is_err(), "all retries should fail with 500");
    }

    #[tokio::test]
    async fn test_download_primary_success_logs_byte_count() {
        // Exercises line 63: `bytes = csv_text.len()` in the primary success path
        let fake_csv = "P".repeat(INSTRUMENT_CSV_MIN_BYTES + 10);
        let url = spawn_test_http_server(200, &fake_csv).await;

        let temp_dir = env::temp_dir().join("dlt-test-dl-primary-bytes-log");
        let cache_dir = temp_dir.to_str().unwrap();

        let result = download_instrument_csv(
            &url,
            "http://127.0.0.1:1/nonexistent-fallback",
            cache_dir,
            "test.csv",
            10,
        )
        .await;

        assert!(result.is_ok());
        let download_result = result.unwrap();
        assert_eq!(download_result.source, "primary");
        assert_eq!(download_result.csv_text.len(), fake_csv.len());

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_download_fallback_success_logs_byte_count() {
        // Exercises line 86: `bytes = csv_text.len()` in the fallback success path
        let fake_csv = "Q".repeat(INSTRUMENT_CSV_MIN_BYTES + 10);
        let fallback_url = spawn_test_http_server(200, &fake_csv).await;

        let temp_dir = env::temp_dir().join("dlt-test-dl-fallback-bytes-log");
        let cache_dir = temp_dir.to_str().unwrap();

        let result = download_instrument_csv(
            "http://127.0.0.1:1/nonexistent-primary",
            &fallback_url,
            cache_dir,
            "test.csv",
            10,
        )
        .await;

        assert!(result.is_ok());
        let download_result = result.unwrap();
        assert_eq!(download_result.source, "fallback");

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_download_cache_success_logs_byte_count() {
        // Exercises lines 109-110: cache success with byte count in warn log
        let temp_dir = env::temp_dir().join("dlt-test-dl-cache-bytes-log");
        let cache_dir = temp_dir.to_str().unwrap();
        let cache_filename = "cached-bytes-log.csv";

        let fake_csv = "R".repeat(INSTRUMENT_CSV_MIN_BYTES + 10);
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

    // -----------------------------------------------------------------------
    // Boundary: body exactly INSTRUMENT_CSV_MIN_BYTES
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_download_with_retry_body_exactly_min_bytes_succeeds() {
        // Boundary: body.len() == INSTRUMENT_CSV_MIN_BYTES → should succeed
        // (check is `<`, not `<=`)
        let body = "X".repeat(INSTRUMENT_CSV_MIN_BYTES);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let body_clone = body.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body_clone.len(),
                body_clone
            );
            tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes())
                .await
                .unwrap();
        });

        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://{}/test", addr);
        let result = download_with_retry(&client, &url).await;

        assert!(
            result.is_ok(),
            "body exactly MIN_BYTES should succeed: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().len(), INSTRUMENT_CSV_MIN_BYTES);
    }

    #[tokio::test]
    async fn test_download_with_retry_body_one_below_min_bytes_fails() {
        // Boundary: body.len() == INSTRUMENT_CSV_MIN_BYTES - 1 → should fail
        let body = "X".repeat(INSTRUMENT_CSV_MIN_BYTES - 1);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let body_clone = body.clone();
        tokio::spawn(async move {
            // Accept multiple connections (retries)
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                    body_clone.len(),
                    body_clone
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
            }
        });

        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://{}/test", addr);
        let result = download_with_retry(&client, &url).await;

        assert!(result.is_err(), "body one below MIN_BYTES should fail");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("too small"),
            "error must mention too small: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // read_cache: invalid UTF-8
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_read_cache_invalid_utf8_returns_error() {
        let temp_dir = env::temp_dir().join("dlt-test-csv-invalid-utf8");
        let _ = fs::create_dir_all(&temp_dir).await;
        let cache_path = temp_dir.join("bad-utf8.csv");

        // Write invalid UTF-8 bytes (0xFF repeated) that exceed MIN_BYTES
        let mut bad_bytes = vec![0xFF_u8; INSTRUMENT_CSV_MIN_BYTES + 100];
        // Ensure it's actually invalid UTF-8
        bad_bytes[0] = 0xC0;
        bad_bytes[1] = 0x01; // Invalid continuation byte
        fs::write(&cache_path, &bad_bytes).await.unwrap();

        let result = read_cache(&cache_path).await;
        assert!(
            result.is_err(),
            "invalid UTF-8 should produce an error from fs::read_to_string"
        );

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    // -----------------------------------------------------------------------
    // download_with_retry: HTTP 502/503 variants
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_download_with_retry_http_502_bad_gateway_fails() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let response = "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n";
                let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
            }
        });

        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://{}/test", addr);
        let result = download_with_retry(&client, &url).await;

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("502"),
            "error must contain status code: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // load_cached_csv: source field is "cache"
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_load_cached_csv_source_field_is_cache() {
        let temp_dir = env::temp_dir().join("dlt-test-load-cached-source");
        let cache_dir = temp_dir.to_str().unwrap();
        let cache_filename = "source-check.csv";

        let fake_csv = "S".repeat(INSTRUMENT_CSV_MIN_BYTES + 1);
        write_cache(cache_dir, cache_filename, &fake_csv).await;

        let result = load_cached_csv(cache_dir, cache_filename).await.unwrap();
        assert_eq!(result.source, "cache");
        assert_eq!(result.csv_text.len(), fake_csv.len());

        let _ = fs::remove_dir_all(&temp_dir).await;
    }
}
