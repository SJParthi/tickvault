//! Static file handler for the DLT Control Panel (portal).

use axum::http::{StatusCode, header};
use axum::response::IntoResponse;

/// Embedded HTML content for the DLT Control Panel (portal).
/// Navigation hub linking to all monitoring, database, and infra services.
const PORTAL_HTML: &str = include_str!("../../static/portal.html");

/// Embedded HTML content for the Options Chain page.
/// Live option chain with Greeks, IV, OI — auto-refreshes from QuestDB.
const OPTIONS_CHAIN_HTML: &str = include_str!("../../static/options-chain.html");

/// Embedded HTML content for the Market Dashboard page.
/// Live market data: Index tickers, Stock Movers, Option Movers — auto-refreshes.
const MARKET_DASHBOARD_HTML: &str = include_str!("../../static/market-dashboard.html");

/// Embedded HTML content for the WebSocket Dashboard page.
/// Live WS connection pool monitoring: 4 pools, subsystem health, limits reference.
const WS_DASHBOARD_HTML: &str = include_str!("../../static/ws-dashboard.html");

/// GET /portal — serves the DLT Control Panel with links to all services.
pub async fn portal() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        PORTAL_HTML,
    )
}

/// GET /portal/market-dashboard — serves the live Market Dashboard.
// TEST-EXEMPT: static HTML serving — tested via pattern tests below
pub async fn market_dashboard() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        MARKET_DASHBOARD_HTML,
    )
}

/// GET /portal/ws-dashboard — serves the live WebSocket connection dashboard.
// TEST-EXEMPT: static HTML serving — tested via pattern tests below
pub async fn ws_dashboard() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        WS_DASHBOARD_HTML,
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

    #[test]
    fn test_market_dashboard_html_not_empty() {
        assert!(!MARKET_DASHBOARD_HTML.is_empty());
        assert!(MARKET_DASHBOARD_HTML.contains("<!DOCTYPE html>"));
        assert!(MARKET_DASHBOARD_HTML.contains("Market Dashboard"));
    }

    #[tokio::test]
    async fn test_market_dashboard_handler_returns_ok() {
        let response = market_dashboard().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn test_market_dashboard_html_contains_api_endpoints() {
        assert!(MARKET_DASHBOARD_HTML.contains("/api/market/indices"));
        assert!(MARKET_DASHBOARD_HTML.contains("/api/market/stock-movers"));
        assert!(MARKET_DASHBOARD_HTML.contains("/api/market/option-movers"));
    }

    #[test]
    fn test_market_dashboard_html_has_tabs() {
        assert!(MARKET_DASHBOARD_HTML.contains("Stocks"));
        assert!(MARKET_DASHBOARD_HTML.contains("Options"));
        assert!(MARKET_DASHBOARD_HTML.contains("Index"));
    }

    // -------------------------------------------------------------------
    // WS Dashboard tests
    // -------------------------------------------------------------------

    #[test]
    fn test_ws_dashboard_html_not_empty() {
        assert!(!WS_DASHBOARD_HTML.is_empty());
        assert!(WS_DASHBOARD_HTML.contains("<!DOCTYPE html>"));
        assert!(WS_DASHBOARD_HTML.contains("WebSocket Dashboard"));
    }

    #[tokio::test]
    async fn test_ws_dashboard_handler_returns_ok() {
        let response = ws_dashboard().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_ws_dashboard_handler_content_type_is_html() {
        let response = ws_dashboard().await.into_response();
        let ct = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "text/html; charset=utf-8");
    }

    #[test]
    fn test_ws_dashboard_html_contains_doctype_and_closing_tags() {
        assert!(WS_DASHBOARD_HTML.contains("</html>"));
        assert!(WS_DASHBOARD_HTML.contains("<head>"));
        assert!(WS_DASHBOARD_HTML.contains("</body>"));
        assert!(WS_DASHBOARD_HTML.contains("</script>"));
    }

    #[test]
    fn test_ws_dashboard_html_contains_connection_sections() {
        // All 4 pool cards present
        assert!(WS_DASHBOARD_HTML.contains("Live Market Feed"));
        assert!(WS_DASHBOARD_HTML.contains("Depth 20-Level"));
        assert!(WS_DASHBOARD_HTML.contains("Depth 200-Level"));
        assert!(WS_DASHBOARD_HTML.contains("Order Update"));
        // Subsystem health section
        assert!(WS_DASHBOARD_HTML.contains("Subsystem Health"));
        assert!(WS_DASHBOARD_HTML.contains("QuestDB"));
        assert!(WS_DASHBOARD_HTML.contains("Auth Token"));
        assert!(WS_DASHBOARD_HTML.contains("Tick Pipeline"));
        assert!(WS_DASHBOARD_HTML.contains("Tick Persistence"));
    }

    #[test]
    fn test_ws_dashboard_html_contains_health_endpoint() {
        // Must fetch from /health for live data
        assert!(WS_DASHBOARD_HTML.contains("/health"));
        assert!(WS_DASHBOARD_HTML.contains("fetchHealth"));
    }

    #[test]
    fn test_ws_dashboard_html_contains_connection_limits() {
        // Reference table with confirmed limits
        assert!(WS_DASHBOARD_HTML.contains("Connection Limits Reference"));
        assert!(WS_DASHBOARD_HTML.contains("wss://api-feed.dhan.co"));
        assert!(WS_DASHBOARD_HTML.contains("wss://depth-api-feed.dhan.co"));
        assert!(WS_DASHBOARD_HTML.contains("wss://full-depth-api.dhan.co"));
        assert!(WS_DASHBOARD_HTML.contains("wss://api-order-update.dhan.co"));
    }

    #[test]
    fn test_ws_dashboard_html_max_5_connections_per_type() {
        // Verify "of 5 connections" appears 3 times (Feed + D20 + D200)
        let count = WS_DASHBOARD_HTML.matches("of 5 connections").count();
        assert_eq!(count, 3, "should show 'of 5 connections' for Feed, D20, D200");
        // Order Update shows "of 1 connection"
        assert!(WS_DASHBOARD_HTML.contains("of 1 connection"));
    }

    #[test]
    fn test_portal_html_contains_ws_dashboard_link() {
        assert!(PORTAL_HTML.contains("/portal/ws-dashboard"));
        assert!(PORTAL_HTML.contains("Full Dashboard"));
    }

    #[test]
    fn test_portal_html_depth_labels_show_max_5() {
        // Both depth cards must say "max 5 connections" (not "max 4")
        let count = PORTAL_HTML.matches("max 5 connections").count();
        assert!(count >= 3, "Live Feed + D20 + D200 should all say 'max 5 connections', found {count}");
        assert!(!PORTAL_HTML.contains("max 4 connections"), "portal must not contain 'max 4 connections'");
    }
}
