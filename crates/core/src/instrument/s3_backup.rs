//! S3 backup and restore for instrument cache files (I-P0-05).
//!
//! Provides disaster recovery for instrument data by backing up
//! rkyv binary cache and CSV files to S3. Enables cross-AZ,
//! cross-instance recovery when local EBS volumes are lost.
//!
//! # Usage
//! - Backup: Called after successful instrument build (outside market hours)
//! - Restore: Called when local cache is missing during market hours
//!
//! # S3 Key Layout
//! ```text
//! s3://{bucket}/instruments/{date}/universe.rkyv
//! s3://{bucket}/instruments/{date}/instruments.csv
//! s3://{bucket}/instruments/latest/universe.rkyv
//! s3://{bucket}/instruments/latest/instruments.csv
//! ```

use std::path::Path;

use chrono::NaiveDate;
use tracing::{info, warn};

// I-P0-05: S3 key path components
const S3_KEY_DATE_FORMAT: &str = "%Y-%m-%d";
const S3_KEY_LATEST: &str = "latest";
const S3_RKYV_FILENAME: &str = "universe.rkyv";
const S3_CSV_FILENAME: &str = "instruments.csv";

/// Configuration for S3 instrument cache backup.
///
/// Loaded from the application config. When `bucket` is empty,
/// S3 backup is disabled and all operations return [`S3BackupError::NotConfigured`].
#[derive(Debug, Clone)]
pub struct S3BackupConfig {
    /// S3 bucket name (e.g., `"dlt-instrument-backup"`).
    /// Empty string means S3 backup is disabled.
    pub bucket: String,
    /// Key prefix inside the bucket (e.g., `"instruments"`).
    /// Defaults to `"instruments"` if empty.
    pub prefix: String,
    /// AWS region for the S3 bucket (e.g., `"ap-south-1"`).
    pub region: String,
}

impl Default for S3BackupConfig {
    fn default() -> Self {
        Self {
            bucket: String::new(),
            prefix: "instruments".to_owned(),
            region: "ap-south-1".to_owned(),
        }
    }
}

/// Errors from S3 backup/restore operations.
// I-P0-05: error types for S3 backup module
#[derive(Debug, thiserror::Error)]
pub enum S3BackupError {
    /// S3 backup is not configured (bucket is empty or invalid).
    #[error("S3 backup not configured: bucket name is empty")]
    NotConfigured,

    /// Upload to S3 failed.
    #[error("S3 upload failed for key '{key}': {reason}")]
    UploadFailed {
        /// The S3 key that failed to upload.
        key: String,
        /// Human-readable reason for the failure.
        reason: String,
    },

    /// Download from S3 failed.
    #[error("S3 download failed for key '{key}': {reason}")]
    DownloadFailed {
        /// The S3 key that failed to download.
        key: String,
        /// Human-readable reason for the failure.
        reason: String,
    },

    /// Local filesystem I/O error (reading cache files for upload, or writing after download).
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
}

/// Check whether S3 backup is configured with a valid bucket name.
///
/// Returns `true` when `config.bucket` is non-empty and `config.region` is non-empty.
// I-P0-05: configuration gate — all operations check this first
pub fn is_s3_backup_configured(config: &S3BackupConfig) -> bool {
    !config.bucket.trim().is_empty() && !config.region.trim().is_empty()
}

/// Build the S3 key prefix, normalizing empty prefix to `"instruments"`.
fn effective_prefix(config: &S3BackupConfig) -> &str {
    let prefix = config.prefix.trim();
    if prefix.is_empty() {
        "instruments"
    } else {
        prefix
    }
}

/// Build an S3 key for a dated backup.
///
/// Format: `{prefix}/{date}/filename`
// I-P0-05: key layout — dated path
pub fn s3_key_for_date(config: &S3BackupConfig, date: NaiveDate, filename: &str) -> String {
    let prefix = effective_prefix(config);
    let date_str = date.format(S3_KEY_DATE_FORMAT);
    format!("{prefix}/{date_str}/{filename}")
}

/// Build an S3 key for the `latest` alias.
///
/// Format: `{prefix}/latest/filename`
// I-P0-05: key layout — latest alias
pub fn s3_key_for_latest(config: &S3BackupConfig, filename: &str) -> String {
    let prefix = effective_prefix(config);
    format!("{prefix}/{S3_KEY_LATEST}/{filename}")
}

/// Back up instrument cache files (rkyv + CSV) to S3.
///
/// Called after a successful instrument build, outside market hours.
/// Uploads to both a dated key and the `latest` alias.
///
/// # Errors
/// - [`S3BackupError::NotConfigured`] if S3 bucket is not set.
/// - [`S3BackupError::IoError`] if local cache files cannot be read.
/// - [`S3BackupError::UploadFailed`] when the S3 SDK call fails (once wired).
// I-P0-05: backup entry point
pub async fn backup_instrument_cache(
    cache_dir: &str,
    csv_filename: &str,
    date: NaiveDate,
    config: &S3BackupConfig,
) -> Result<(), S3BackupError> {
    // Gate: S3 must be configured
    if !is_s3_backup_configured(config) {
        warn!(
            // I-P0-05: backup skipped — not configured
            "S3 backup skipped: not configured (bucket empty)"
        );
        return Err(S3BackupError::NotConfigured);
    }

    let cache_path = Path::new(cache_dir);

    // Verify local files exist before attempting upload
    let rkyv_path = cache_path.join(dhan_live_trader_common::constants::BINARY_CACHE_FILENAME);
    let csv_path = cache_path.join(csv_filename);

    if !rkyv_path.exists() {
        return Err(S3BackupError::IoError(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("rkyv cache not found: {}", rkyv_path.display()),
        )));
    }
    if !csv_path.exists() {
        return Err(S3BackupError::IoError(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("CSV cache not found: {}", csv_path.display()),
        )));
    }

    // Build S3 keys for the dated and latest copies
    let rkyv_dated_key = s3_key_for_date(config, date, S3_RKYV_FILENAME);
    let csv_dated_key = s3_key_for_date(config, date, S3_CSV_FILENAME);
    let rkyv_latest_key = s3_key_for_latest(config, S3_RKYV_FILENAME);
    let csv_latest_key = s3_key_for_latest(config, S3_CSV_FILENAME);

    info!(
        // I-P0-05: backup initiated
        bucket = %config.bucket,
        rkyv_key = %rkyv_dated_key,
        csv_key = %csv_dated_key,
        "S3 instrument backup: would upload 4 objects (stub — aws-sdk-s3 not yet wired)"
    );

    // TODO(I-P0-05): Wire aws-sdk-s3 PutObject calls here.
    // For each file (rkyv, csv) upload to both dated and latest keys.
    // Use multipart upload for files > 8 MiB.
    // The keys are ready:
    let _keys = [
        &rkyv_dated_key,
        &csv_dated_key,
        &rkyv_latest_key,
        &csv_latest_key,
    ];

    // Stub: return NotConfigured until aws-sdk-s3 is added as a dependency.
    Err(S3BackupError::UploadFailed {
        key: rkyv_dated_key,
        reason: "aws-sdk-s3 dependency not yet added — stub implementation".to_owned(),
    })
}

/// Restore instrument cache files from S3 to local disk.
///
/// Called when the local cache is missing during market hours.
/// Tries `latest` alias first, falls back to a specific date if provided.
///
/// # Errors
/// - [`S3BackupError::NotConfigured`] if S3 bucket is not set.
/// - [`S3BackupError::DownloadFailed`] when the S3 SDK call fails (once wired).
/// - [`S3BackupError::IoError`] if local cache directory cannot be written to.
// I-P0-05: restore entry point
pub async fn restore_instrument_cache(
    cache_dir: &str,
    config: &S3BackupConfig,
) -> Result<(), S3BackupError> {
    // Gate: S3 must be configured
    if !is_s3_backup_configured(config) {
        warn!(
            // I-P0-05: restore skipped — not configured
            "S3 restore skipped: not configured (bucket empty)"
        );
        return Err(S3BackupError::NotConfigured);
    }

    // Verify cache directory is writable
    let cache_path = Path::new(cache_dir);
    if !cache_path.exists() {
        std::fs::create_dir_all(cache_path)?;
    }

    let rkyv_latest_key = s3_key_for_latest(config, S3_RKYV_FILENAME);
    let csv_latest_key = s3_key_for_latest(config, S3_CSV_FILENAME);

    info!(
        // I-P0-05: restore initiated
        bucket = %config.bucket,
        rkyv_key = %rkyv_latest_key,
        csv_key = %csv_latest_key,
        "S3 instrument restore: would download 2 objects (stub — aws-sdk-s3 not yet wired)"
    );

    // TODO(I-P0-05): Wire aws-sdk-s3 GetObject calls here.
    // Download latest/universe.rkyv → cache_dir/BINARY_CACHE_FILENAME
    // Download latest/instruments.csv → cache_dir/csv_cache_filename

    // Stub: return NotConfigured until aws-sdk-s3 is added as a dependency.
    Err(S3BackupError::DownloadFailed {
        key: rkyv_latest_key,
        reason: "aws-sdk-s3 dependency not yet added — stub implementation".to_owned(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    // -- Config tests -------------------------------------------------------

    #[test]
    fn test_s3_backup_config_default_not_configured() {
        let config = S3BackupConfig::default();
        assert!(
            !is_s3_backup_configured(&config),
            "default config should not be configured (empty bucket)"
        );
        assert!(config.bucket.is_empty());
        assert_eq!(config.prefix, "instruments");
        assert_eq!(config.region, "ap-south-1");
    }

    #[test]
    fn test_s3_backup_config_with_bucket_is_configured() {
        let config = S3BackupConfig {
            bucket: "dlt-instrument-backup".to_owned(),
            prefix: "instruments".to_owned(),
            region: "ap-south-1".to_owned(),
        };
        assert!(
            is_s3_backup_configured(&config),
            "config with valid bucket should be configured"
        );
    }

    #[test]
    fn test_s3_backup_config_whitespace_bucket_not_configured() {
        let config = S3BackupConfig {
            bucket: "   ".to_owned(),
            prefix: "instruments".to_owned(),
            region: "ap-south-1".to_owned(),
        };
        assert!(
            !is_s3_backup_configured(&config),
            "whitespace-only bucket should not be configured"
        );
    }

    #[test]
    fn test_s3_backup_config_empty_region_not_configured() {
        let config = S3BackupConfig {
            bucket: "my-bucket".to_owned(),
            prefix: "instruments".to_owned(),
            region: String::new(),
        };
        assert!(
            !is_s3_backup_configured(&config),
            "empty region should not be configured"
        );
    }

    // -- Backup/restore not-configured tests --------------------------------

    #[tokio::test]
    async fn test_backup_not_configured_returns_error() {
        let config = S3BackupConfig::default();
        let date = NaiveDate::from_ymd_opt(2025, 6, 15).expect("valid date"); // APPROVED: test constant
        let result = backup_instrument_cache("/tmp/nonexistent", "test.csv", date, &config).await;
        assert!(result.is_err());
        match result {
            Err(S3BackupError::NotConfigured) => {} // expected
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_restore_not_configured_returns_error() {
        let config = S3BackupConfig::default();
        let result = restore_instrument_cache("/tmp/nonexistent", &config).await;
        assert!(result.is_err());
        match result {
            Err(S3BackupError::NotConfigured) => {} // expected
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    // -- S3 key layout tests ------------------------------------------------

    #[test]
    fn test_s3_key_layout_date_format() {
        let config = S3BackupConfig {
            bucket: "my-bucket".to_owned(),
            prefix: "instruments".to_owned(),
            region: "ap-south-1".to_owned(),
        };
        let date = NaiveDate::from_ymd_opt(2025, 6, 15).expect("valid date"); // APPROVED: test constant

        let rkyv_key = s3_key_for_date(&config, date, S3_RKYV_FILENAME);
        assert_eq!(rkyv_key, "instruments/2025-06-15/universe.rkyv");

        let csv_key = s3_key_for_date(&config, date, S3_CSV_FILENAME);
        assert_eq!(csv_key, "instruments/2025-06-15/instruments.csv");
    }

    #[test]
    fn test_s3_key_layout_latest_prefix() {
        let config = S3BackupConfig {
            bucket: "my-bucket".to_owned(),
            prefix: "instruments".to_owned(),
            region: "ap-south-1".to_owned(),
        };

        let rkyv_key = s3_key_for_latest(&config, S3_RKYV_FILENAME);
        assert_eq!(rkyv_key, "instruments/latest/universe.rkyv");

        let csv_key = s3_key_for_latest(&config, S3_CSV_FILENAME);
        assert_eq!(csv_key, "instruments/latest/instruments.csv");
    }

    #[test]
    fn test_s3_key_layout_custom_prefix() {
        let config = S3BackupConfig {
            bucket: "my-bucket".to_owned(),
            prefix: "prod/cache".to_owned(),
            region: "ap-south-1".to_owned(),
        };
        let date = NaiveDate::from_ymd_opt(2025, 1, 1).expect("valid date"); // APPROVED: test constant

        assert_eq!(
            s3_key_for_date(&config, date, S3_RKYV_FILENAME),
            "prod/cache/2025-01-01/universe.rkyv"
        );
        assert_eq!(
            s3_key_for_latest(&config, S3_CSV_FILENAME),
            "prod/cache/latest/instruments.csv"
        );
    }

    #[test]
    fn test_s3_key_layout_empty_prefix_defaults_to_instruments() {
        let config = S3BackupConfig {
            bucket: "my-bucket".to_owned(),
            prefix: String::new(),
            region: "ap-south-1".to_owned(),
        };
        let date = NaiveDate::from_ymd_opt(2025, 3, 10).expect("valid date"); // APPROVED: test constant

        assert_eq!(
            s3_key_for_date(&config, date, S3_RKYV_FILENAME),
            "instruments/2025-03-10/universe.rkyv"
        );
        assert_eq!(
            s3_key_for_latest(&config, S3_RKYV_FILENAME),
            "instruments/latest/universe.rkyv"
        );
    }

    // -- I/O error tests ----------------------------------------------------

    #[tokio::test]
    async fn test_backup_missing_cache_dir_returns_io_error() {
        let config = S3BackupConfig {
            bucket: "my-bucket".to_owned(),
            prefix: "instruments".to_owned(),
            region: "ap-south-1".to_owned(),
        };
        let date = NaiveDate::from_ymd_opt(2025, 6, 15).expect("valid date"); // APPROVED: test constant

        // Use a path that definitely does not exist
        let result = backup_instrument_cache(
            "/tmp/dlt-s3-test-nonexistent-12345",
            "test.csv",
            date,
            &config,
        )
        .await;

        assert!(result.is_err());
        match result {
            Err(S3BackupError::IoError(ref err)) => {
                assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected IoError(NotFound), got {other:?}"),
        }
    }

    // -- Display tests ------------------------------------------------------

    #[test]
    fn test_s3_backup_error_display() {
        let not_configured = S3BackupError::NotConfigured;
        assert_eq!(
            not_configured.to_string(),
            "S3 backup not configured: bucket name is empty"
        );

        let upload_failed = S3BackupError::UploadFailed {
            key: "instruments/2025-06-15/universe.rkyv".to_owned(),
            reason: "access denied".to_owned(),
        };
        assert_eq!(
            upload_failed.to_string(),
            "S3 upload failed for key 'instruments/2025-06-15/universe.rkyv': access denied"
        );

        let download_failed = S3BackupError::DownloadFailed {
            key: "instruments/latest/universe.rkyv".to_owned(),
            reason: "not found".to_owned(),
        };
        assert_eq!(
            download_failed.to_string(),
            "S3 download failed for key 'instruments/latest/universe.rkyv': not found"
        );

        let io_error = S3BackupError::IoError(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "permission denied",
        ));
        assert_eq!(io_error.to_string(), "I/O error: permission denied");
    }
}
