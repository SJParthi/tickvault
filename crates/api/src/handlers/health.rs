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

    #[test]
    fn test_health_response_serialization() {
        let resp = HealthResponse {
            status: "ok",
            version: "0.1.0",
        };
        let json = serde_json::to_string(&resp).expect("serialization should succeed");
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"version\":\"0.1.0\""));
    }
}
