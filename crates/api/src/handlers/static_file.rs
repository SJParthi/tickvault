//! Static file handler for the DLT Control Panel (portal).

use axum::http::{StatusCode, header};
use axum::response::IntoResponse;

/// Embedded HTML content for the DLT Control Panel (portal).
/// Navigation hub linking to all monitoring, database, and infra services.
const PORTAL_HTML: &str = include_str!("../../static/portal.html");

/// GET /portal — serves the DLT Control Panel with links to all services.
pub async fn portal() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        PORTAL_HTML,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn test_portal_html_not_empty() {
        assert!(!PORTAL_HTML.is_empty());
        assert!(PORTAL_HTML.contains("<!DOCTYPE html>"));
        assert!(PORTAL_HTML.contains("Control Panel"));
    }

    #[tokio::test]
    async fn test_portal_handler_returns_ok_with_html() {
        let response = portal().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_portal_handler_content_type_is_html() {
        let response = portal().await.into_response();
        let ct = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "text/html; charset=utf-8");
    }

    #[test]
    fn test_portal_html_contains_doctype_and_closing_tags() {
        assert!(PORTAL_HTML.contains("</html>"));
        assert!(PORTAL_HTML.contains("<head>"));
        assert!(PORTAL_HTML.contains("</body>"));
    }
}
