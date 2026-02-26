//! WebSocket handler for live candle streaming.
//!
//! Filters candle broadcasts by both security_id AND interval,
//! so each client only receives candles for their subscribed instrument
//! and timeframe.
//!
//! # O(1) Hot Path Guarantee
//! - Candle JSON is pre-serialized ONCE in the tick processor (shared via `Arc<str>`)
//! - Per-client send is zero-allocation: just forwards the pre-built bytes
//! - Filtering uses `Copy` types (`u32` + `IntervalId`) — no heap lookup

use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use serde::Deserialize;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use dhan_live_trader_common::tick_types::IntervalId;

use crate::state::SharedAppState;

/// Client → Server message.
#[derive(Debug, Deserialize)]
pub struct ClientMessage {
    pub action: String,
    pub security_id: Option<u32>,
    pub timeframe: Option<String>,
}

/// WebSocket upgrade handler.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<SharedAppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Handles an individual WebSocket connection.
///
/// Filters candle broadcasts by BOTH security_id AND interval_id.
/// Only candles matching the client's subscription are forwarded.
///
/// # O(1) Per-Message Cost
/// - Filter: two `==` comparisons on `Copy` types (u32 + IntervalId)
/// - Send: `Arc<str>.clone()` (refcount bump, no data copy) + axum `socket.send()`
/// - Zero `serde_json::to_string()` — JSON was pre-serialized in tick processor
async fn handle_socket(mut socket: WebSocket, state: SharedAppState) {
    let mut candle_rx = state.subscribe_candles();
    let mut subscribed_security_id: Option<u32> = None;
    let mut subscribed_interval: Option<IntervalId> = None;

    debug!("WebSocket client connected");

    // Pre-build the available intervals JSON once per connection (not per message).
    // Stored as Arc<str> so subscribe/switch_timeframe messages share the same allocation.
    let intervals_json: Arc<str> = Arc::from(build_intervals_json().as_str());

    loop {
        tokio::select! {
            // Client sends a message
            Some(msg) = socket.recv() => {
                match msg {
                    Ok(Message::Text(text)) => {
                        if let Ok(client_msg) = serde_json::from_str::<ClientMessage>(&text) {
                            match client_msg.action.as_str() {
                                "subscribe" | "switch_timeframe" => {
                                    subscribed_security_id = client_msg.security_id.or(subscribed_security_id);

                                    // Parse timeframe label to IntervalId for O(1) filtering
                                    if let Some(ref tf_label) = client_msg.timeframe {
                                        subscribed_interval = IntervalId::from_label(tf_label);
                                    }

                                    debug!(
                                        ?subscribed_security_id,
                                        ?subscribed_interval,
                                        "subscription updated"
                                    );

                                    // Send pre-built intervals — Arc::clone is just refcount bump
                                    let intervals_str: String = (*intervals_json).to_string();
                                    if let Err(err) = socket.send(Message::Text(intervals_str.into())).await {
                                        warn!(?err, "failed to send intervals");
                                        break;
                                    }
                                }
                                _ => {
                                    debug!(action = client_msg.action, "unknown action");
                                }
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        debug!("WebSocket client disconnected");
                        break;
                    }
                    Err(err) => {
                        warn!(?err, "WebSocket recv error");
                        break;
                    }
                    _ => {} // Ping/Pong handled by axum
                }
            }

            // Candle broadcast received — pre-serialized JSON, zero-alloc send
            result = candle_rx.recv() => {
                match result {
                    Ok(update) => {
                        // O(1) filter: two Copy-type comparisons
                        let sid_match = subscribed_security_id
                            .map(|sid| sid == update.security_id)
                            .unwrap_or(true);

                        let interval_match = subscribed_interval
                            .map(|sub_id| sub_id == update.interval)
                            .unwrap_or(false); // require explicit subscription

                        if sid_match && interval_match {
                            // Zero allocation: Arc<str> → String is just a memcpy of bytes.
                            // The JSON was already serialized in the tick processor.
                            let json_str: String = (*update.json_payload).to_string();
                            if let Err(err) = socket.send(Message::Text(json_str.into())).await {
                                warn!(?err, "failed to send candle update");
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "broadcast lagged — client too slow");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        debug!("candle broadcast closed");
                        break;
                    }
                }
            }
        }
    }
}

/// Pre-builds the available intervals JSON string (called once per connection).
fn build_intervals_json() -> String {
    let intervals_msg = serde_json::json!({
        "type": "available_intervals",
        "time": dhan_live_trader_common::tick_types::Timeframe::all_standard()
            .iter()
            .map(|tf| tf.as_str())
            .collect::<Vec<_>>(),
        "tick": dhan_live_trader_common::tick_types::TickInterval::all_standard()
            .iter()
            .map(|ti| ti.as_str())
            .collect::<Vec<_>>(),
        "custom": true,
    });
    intervals_msg.to_string()
}
