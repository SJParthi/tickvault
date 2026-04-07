//! Static file handler for the DLT Control Panel (portal).

use axum::http::{StatusCode, header};
use axum::response::IntoResponse;

/// Embedded HTML content for the DLT Control Panel (portal).
/// Navigation hub linking to all monitoring, database, and infra services.
const PORTAL_HTML: &str = include_str!("../../static/portal.html");

/// Embedded HTML content for the Options Chain page.
/// Live option chain with Greeks, IV, OI — auto-refreshes from QuestDB.
const OPTIONS_CHAIN_HTML: &str = include_str!("../../static/options-chain.html");

/// GET /portal — serves the DLT Control Panel with links to all services.
pub async fn portal() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        PORTAL_HTML,
    )
}

/// GET /portal/options-chain — serves the live Options Chain dashboard.
// TEST-EXEMPT: static HTML serving — tested via portal pattern tests below
pub async fn options_chain() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        OPTIONS_CHAIN_HTML,
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

    #[test]
    fn test_options_chain_html_not_empty() {
        assert!(!OPTIONS_CHAIN_HTML.is_empty());
        assert!(OPTIONS_CHAIN_HTML.contains("<!DOCTYPE html>"));
        assert!(OPTIONS_CHAIN_HTML.contains("Options Chain"));
    }

    #[tokio::test]
    async fn test_options_chain_handler_returns_ok() {
        let response = options_chain().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn test_portal_html_contains_websocket_status() {
        assert!(PORTAL_HTML.contains("WebSocket Connections"));
        assert!(PORTAL_HTML.contains("wsLiveCount"));
        assert!(PORTAL_HTML.contains("wsD20Count"));
        assert!(PORTAL_HTML.contains("wsD200Count"));
        assert!(PORTAL_HTML.contains("wsOUStatus"));
        assert!(PORTAL_HTML.contains("loadWsStatus"));
    }

    #[test]
    fn test_options_chain_html_contains_api_endpoint() {
        assert!(OPTIONS_CHAIN_HTML.contains("/api/option-chain"));
    }

    #[test]
    fn test_options_chain_html_has_closing_tags() {
        assert!(OPTIONS_CHAIN_HTML.contains("</html>"));
        assert!(OPTIONS_CHAIN_HTML.contains("</body>"));
        assert!(OPTIONS_CHAIN_HTML.contains("</script>"));
    }
}
