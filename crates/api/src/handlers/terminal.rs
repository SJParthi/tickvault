//! WebSocket tick broadcast and terminal frontend handlers.
//!
//! - `GET /ws/ticks` — upgrades to WebSocket, streams live ticks as JSON to browser.
//! - `GET /` — serves the DLT Trading Terminal frontend.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde::Serialize;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use dhan_live_trader_common::tick_types::ParsedTick;

use crate::state::SharedAppState;

// ---------------------------------------------------------------------------
// Browser tick format — matches frontend/src/ws-client.ts RawTick interface
// ---------------------------------------------------------------------------

/// Lightweight tick struct serialized as JSON for browser WebSocket clients.
///
/// Conversion from `ParsedTick` is O(1) — field copies only.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserTick {
    security_id: u32,
    ltp: f32,
    volume: u32,
    timestamp: u32,
    open: f32,
    high: f32,
    low: f32,
    close: f32,
}

impl From<ParsedTick> for BrowserTick {
    #[inline]
    fn from(tick: ParsedTick) -> Self {
        Self {
            security_id: tick.security_id,
            ltp: tick.last_traded_price,
            volume: tick.volume,
            timestamp: tick.exchange_timestamp,
            open: tick.day_open,
            high: tick.day_high,
            low: tick.day_low,
            close: tick.day_close,
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket handler — /ws/ticks
// ---------------------------------------------------------------------------

/// Upgrades HTTP to WebSocket and streams live ticks to the browser.
pub async fn websocket_ticks(
    ws: WebSocketUpgrade,
    State(state): State<SharedAppState>,
) -> impl IntoResponse {
    let receiver = state.subscribe_ticks();
    ws.on_upgrade(move |socket| handle_tick_stream(socket, receiver))
}

/// Streams ticks from broadcast channel to a single browser WebSocket client.
///
/// Exits when the client disconnects or the broadcast channel closes.
async fn handle_tick_stream(mut socket: WebSocket, mut receiver: broadcast::Receiver<ParsedTick>) {
    info!("browser WebSocket client connected to /ws/ticks");

    loop {
        match receiver.recv().await {
            Ok(tick) => {
                let browser_tick = BrowserTick::from(tick);
                // serde_json::to_string on a flat struct with primitives is O(1).
                match serde_json::to_string(&browser_tick) {
                    Ok(json) => {
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            debug!("browser WebSocket client disconnected");
                            break;
                        }
                    }
                    Err(err) => {
                        warn!(?err, "failed to serialize tick for browser");
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(count)) => {
                // Client fell behind — skip stale ticks, continue with latest.
                // This is expected under heavy load; chart only needs current price.
                debug!(
                    skipped = count,
                    "browser client lagged — skipping stale ticks"
                );
            }
            Err(broadcast::error::RecvError::Closed) => {
                info!("tick broadcast channel closed — stopping browser stream");
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal HTML handler — GET /
// ---------------------------------------------------------------------------

/// Embedded HTML content for the DLT Trading Terminal.
const TERMINAL_HTML: &str = include_str!("../../static/terminal.html");

/// Embedded JS bundle (esbuild output, ~170KB).
const TERMINAL_JS: &str = include_str!("../../static/terminal.js");

/// Embedded CSS stylesheet (~5KB).
const TERMINAL_CSS: &str = include_str!("../../static/terminal.css");

/// GET / — serves the DLT Trading Terminal frontend.
pub async fn terminal_page() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        TERMINAL_HTML,
    )
}

/// GET /static/terminal.js — serves the bundled JS.
pub async fn terminal_js() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        TERMINAL_JS,
    )
}

/// GET /static/terminal.css — serves the terminal stylesheet.
pub async fn terminal_css() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        TERMINAL_CSS,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_browser_tick_from_parsed_tick() {
        let tick = ParsedTick {
            security_id: 13,
            last_traded_price: 24_500.5,
            volume: 1_000_000,
            exchange_timestamp: 1_772_073_900,
            day_open: 24_400.0,
            day_high: 24_600.0,
            day_low: 24_350.0,
            day_close: 24_480.0,
            ..Default::default()
        };
        let browser = BrowserTick::from(tick);
        assert_eq!(browser.security_id, 13);
        assert_eq!(browser.ltp, 24_500.5);
        assert_eq!(browser.volume, 1_000_000);
        assert_eq!(browser.timestamp, 1_772_073_900);
        assert_eq!(browser.open, 24_400.0);
        assert_eq!(browser.high, 24_600.0);
        assert_eq!(browser.low, 24_350.0);
        assert_eq!(browser.close, 24_480.0);
    }

    #[test]
    fn test_browser_tick_serializes_camel_case() {
        let tick = BrowserTick {
            security_id: 42,
            ltp: 100.5,
            volume: 500,
            timestamp: 1000000,
            open: 99.0,
            high: 101.0,
            low: 98.5,
            close: 100.0,
        };
        let json = serde_json::to_string(&tick).expect("serialize");
        assert!(json.contains("\"securityId\":42"));
        assert!(json.contains("\"ltp\":"));
        assert!(json.contains("\"volume\":500"));
        assert!(json.contains("\"timestamp\":1000000"));
    }

    #[test]
    fn test_terminal_html_not_empty() {
        assert!(!TERMINAL_HTML.is_empty());
        assert!(TERMINAL_HTML.contains("<!DOCTYPE html>"));
    }
}
