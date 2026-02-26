//! Static file handlers for the TradingView frontend and portal dashboard.

use axum::http::{StatusCode, header};
use axum::response::IntoResponse;

/// Embedded HTML content for the TradingView frontend.
/// Included at compile time from the `static/index.html` file.
const INDEX_HTML: &str = include_str!("../../static/index.html");

/// Embedded HTML content for the DLT Control Panel (portal).
/// Navigation hub linking to all monitoring, database, and infra services.
const PORTAL_HTML: &str = include_str!("../../static/portal.html");

/// GET / — serves the TradingView frontend.
pub async fn index_html() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        INDEX_HTML,
    )
}

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

    #[test]
    fn test_index_html_not_empty() {
        assert!(!INDEX_HTML.is_empty());
        assert!(INDEX_HTML.contains("<!DOCTYPE html>"));
    }

    #[test]
    fn test_portal_html_not_empty() {
        assert!(!PORTAL_HTML.is_empty());
        assert!(PORTAL_HTML.contains("<!DOCTYPE html>"));
        assert!(PORTAL_HTML.contains("Control Panel"));
    }
}
