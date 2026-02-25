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
}
