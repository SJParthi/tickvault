//! Static file handler for the TradingView frontend.

use axum::http::{StatusCode, header};
use axum::response::IntoResponse;

/// Embedded HTML content for the TradingView frontend.
/// Included at compile time from the `static/index.html` file.
const INDEX_HTML: &str = include_str!("../../static/index.html");

/// GET / — serves the TradingView frontend.
pub async fn index_html() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        INDEX_HTML,
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
}
