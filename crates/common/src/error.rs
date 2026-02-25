//! Common error types used across the dhan-live-trader workspace.
//!
//! Each crate may define its own specific error types using `thiserror`,
//! but cross-cutting errors live here.

/// Top-level application error.
#[derive(Debug, thiserror::Error)]
pub enum ApplicationError {
    /// Configuration loading or parsing failed.
    #[error("configuration error: {0}")]
    Configuration(String),

    /// Secret retrieval from SSM Parameter Store failed.
    #[error("secret retrieval failed for path '{path}': {source}")]
    SecretRetrieval { path: String, source: anyhow::Error },

    /// Market hour validation failed (attempting to trade outside hours).
    #[error("market hour violation: {0}")]
    MarketHourViolation(String),

    /// Infrastructure service unavailable.
    #[error("infrastructure unavailable: {service} at {endpoint}")]
    InfrastructureUnavailable { service: String, endpoint: String },

    /// Instrument CSV download failed after all retries and fallbacks.
    #[error("instrument CSV download failed: {reason}")]
    InstrumentDownloadFailed { reason: String },

    /// Instrument CSV parsing failed.
    #[error("instrument CSV parse error at row {row}: {reason}")]
    InstrumentParseFailed { row: usize, reason: String },

    /// F&O universe build validation failed.
    #[error("F&O universe validation failed: {check}")]
    UniverseValidationFailed { check: String },

    /// Required CSV column not found in header.
    #[error("required CSV column '{column}' not found in header")]
    CsvColumnMissing { column: String },
}
