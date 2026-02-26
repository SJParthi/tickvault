//! Health check endpoint.

use axum::Json;
use serde::Serialize;

/// Health check response.
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
}

/// GET /health — returns 200 OK with status.
pub async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_check_returns_ok() {
        let Json(response) = health_check().await;
        assert_eq!(response.status, "ok");
        assert!(!response.version.is_empty());
    }
}
