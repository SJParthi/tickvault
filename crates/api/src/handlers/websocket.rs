//! WebSocket handler for live candle streaming.
//!
//! Filters candle broadcasts by both security_id AND interval,
//! so each client only receives candles for their subscribed instrument
//! and timeframe.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
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

/// Server → Client candle update.
#[derive(Debug, Serialize)]
pub struct ServerCandleUpdate {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub security_id: u32,
    pub timeframe: String,
    pub data: CandleData,
}

/// Candle data matching TradingView Lightweight Charts format.
#[derive(Debug, Serialize)]
pub struct CandleData {
    pub time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: u32,
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
async fn handle_socket(mut socket: WebSocket, state: SharedAppState) {
    let mut candle_rx = state.subscribe_candles();
    let mut subscribed_security_id: Option<u32> = None;
    let mut subscribed_interval: Option<IntervalId> = None;
    let mut subscribed_timeframe_label: String = String::new();

    debug!("WebSocket client connected");

    // Pre-serialize the available intervals message once (not per-message).
    let intervals_json = build_intervals_json();

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
                                        subscribed_timeframe_label = tf_label.clone();
                                    }

                                    debug!(
                                        ?subscribed_security_id,
                                        ?subscribed_interval,
                                        timeframe = %subscribed_timeframe_label,
                                        "subscription updated"
                                    );

                                    // Send pre-built available intervals message
                                    if let Err(err) = socket.send(Message::Text(intervals_json.clone().into())).await {
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

            // Candle broadcast received
            result = candle_rx.recv() => {
                match result {
                    Ok(msg) => {
                        // Filter by BOTH security_id AND interval_id
                        let sid_match = subscribed_security_id
                            .map(|sid| sid == msg.candle.security_id)
                            .unwrap_or(true);

                        let interval_match = subscribed_interval
                            .map(|sub_id| sub_id == msg.interval)
                            .unwrap_or(false); // require explicit subscription

                        if sid_match && interval_match {
                            let update = ServerCandleUpdate {
                                msg_type: "candle_update",
                                security_id: msg.candle.security_id,
                                timeframe: msg.interval.as_label(),
                                data: CandleData {
                                    time: msg.candle.timestamp,
                                    open: msg.candle.open,
                                    high: msg.candle.high,
                                    low: msg.candle.low,
                                    close: msg.candle.close,
                                    volume: msg.candle.volume,
                                },
                            };

                            if let Ok(json) = serde_json::to_string(&update)
                                && let Err(err) = socket.send(Message::Text(json.into())).await
                            {
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
