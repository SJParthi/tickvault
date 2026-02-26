//! WebSocket handler for live candle streaming.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, warn};

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
async fn handle_socket(mut socket: WebSocket, state: SharedAppState) {
    let mut candle_rx = state.subscribe_candles();
    let mut subscribed_security_id: Option<u32> = None;
    let mut subscribed_timeframe: Option<String> = None;

    debug!("WebSocket client connected");

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
                                    subscribed_timeframe = client_msg.timeframe.or(subscribed_timeframe.take());
                                    debug!(
                                        ?subscribed_security_id,
                                        ?subscribed_timeframe,
                                        "subscription updated"
                                    );

                                    // Send available intervals
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
                                    if let Err(err) = socket.send(Message::Text(intervals_msg.to_string().into())).await {
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
                        // Filter by subscribed security_id
                        let sid_match = subscribed_security_id
                            .map(|sid| sid == msg.candle.security_id)
                            .unwrap_or(true); // if no filter, send all

                        if sid_match {
                            let update = ServerCandleUpdate {
                                msg_type: "candle_update",
                                security_id: msg.candle.security_id,
                                timeframe: subscribed_timeframe.clone().unwrap_or_default(),
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
