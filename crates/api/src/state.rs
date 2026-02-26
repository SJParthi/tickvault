//! Shared application state for the API server.

use std::sync::Arc;

use tokio::sync::broadcast;

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::tick_types::CandleBroadcastMessage;

/// Shared state available to all API handlers via axum's `State` extractor.
#[derive(Clone)]
pub struct SharedAppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    /// Broadcast receiver source for live candle updates.
    candle_broadcast: broadcast::Sender<CandleBroadcastMessage>,
    /// QuestDB config for historical candle queries.
    questdb_config: QuestDbConfig,
}

impl SharedAppState {
    /// Creates new shared state.
    pub fn new(
        candle_broadcast: broadcast::Sender<CandleBroadcastMessage>,
        questdb_config: QuestDbConfig,
    ) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                candle_broadcast,
                questdb_config,
            }),
        }
    }

    /// Returns a new broadcast receiver for candle updates.
    pub fn subscribe_candles(&self) -> broadcast::Receiver<CandleBroadcastMessage> {
        self.inner.candle_broadcast.subscribe()
    }

    /// Returns the QuestDB config for making SQL queries.
    pub fn questdb_config(&self) -> &QuestDbConfig {
        &self.inner.questdb_config
    }
}
