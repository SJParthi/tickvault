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

    /// QuestDB write operation failed.
    #[error("QuestDB write failed for table '{table}': {source}")]
    QuestDbWriteFailed {
        table: String,
        source: anyhow::Error,
    },

    /// TOTP code generation failed.
    #[error("TOTP generation failed: {reason}")]
    TotpGenerationFailed { reason: String },

    /// Dhan authentication API call failed.
    #[error("Dhan authentication failed: {reason}")]
    AuthenticationFailed { reason: String },

    /// Token renewal failed after all retries.
    #[error("token renewal failed after {attempts} attempts: {reason}")]
    TokenRenewalFailed { attempts: u32, reason: String },

    /// Authentication circuit breaker tripped — too many consecutive failures.
    #[error("auth circuit breaker tripped after {failures} consecutive failures")]
    AuthCircuitBreakerTripped { failures: u32 },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_configuration_error_display() {
        let err = ApplicationError::Configuration("missing field".to_string());
        assert_eq!(err.to_string(), "configuration error: missing field");
    }

    #[test]
    fn test_secret_retrieval_error_display() {
        let err = ApplicationError::SecretRetrieval {
            path: "/dlt/dev/dhan/client-id".to_string(),
            source: anyhow::anyhow!("not found"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/dlt/dev/dhan/client-id"));
        assert!(msg.contains("not found"));
    }

    #[test]
    fn test_market_hour_violation_display() {
        let err = ApplicationError::MarketHourViolation("order after 15:30".to_string());
        assert!(err.to_string().contains("15:30"));
    }

    #[test]
    fn test_instrument_download_failed_display() {
        let err = ApplicationError::InstrumentDownloadFailed {
            reason: "all sources failed".to_string(),
        };
        assert!(err.to_string().contains("all sources failed"));
    }

    #[test]
    fn test_instrument_parse_failed_display() {
        let err = ApplicationError::InstrumentParseFailed {
            row: 42,
            reason: "missing column".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("42"));
        assert!(msg.contains("missing column"));
    }

    #[test]
    fn test_universe_validation_failed_display() {
        let err = ApplicationError::UniverseValidationFailed {
            check: "NIFTY missing".to_string(),
        };
        assert!(err.to_string().contains("NIFTY missing"));
    }

    #[test]
    fn test_csv_column_missing_display() {
        let err = ApplicationError::CsvColumnMissing {
            column: "SECURITY_ID".to_string(),
        };
        assert!(err.to_string().contains("SECURITY_ID"));
    }

    #[test]
    fn test_questdb_write_failed_display() {
        let err = ApplicationError::QuestDbWriteFailed {
            table: "fno_underlyings".to_string(),
            source: anyhow::anyhow!("connection refused"),
        };
        let msg = err.to_string();
        assert!(msg.contains("fno_underlyings"));
        assert!(msg.contains("connection refused"));
    }

    #[test]
    fn test_totp_generation_failed_display() {
        let err = ApplicationError::TotpGenerationFailed {
            reason: "invalid base32".to_string(),
        };
        assert!(err.to_string().contains("invalid base32"));
    }

    #[test]
    fn test_authentication_failed_display() {
        let err = ApplicationError::AuthenticationFailed {
            reason: "HTTP 401".to_string(),
        };
        assert!(err.to_string().contains("HTTP 401"));
    }

    #[test]
    fn test_token_renewal_failed_display() {
        let err = ApplicationError::TokenRenewalFailed {
            attempts: 3,
            reason: "timeout".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("3"));
        assert!(msg.contains("timeout"));
    }

    #[test]
    fn test_auth_circuit_breaker_display() {
        let err = ApplicationError::AuthCircuitBreakerTripped { failures: 5 };
        assert!(err.to_string().contains("5"));
    }

    #[test]
    fn test_application_error_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        // This will fail at compile time if ApplicationError is not Send + Sync
        assert_send_sync::<ApplicationError>();
    }
}
