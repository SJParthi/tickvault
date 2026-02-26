//! Shared application state for the API server.

use std::sync::Arc;

use tokio::sync::broadcast;

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::tick_types::PreSerializedCandleUpdate;
use dhan_live_trader_storage::candle_query::QuestDbCandleQuerier;

/// Shared state available to all API handlers via axum's `State` extractor.
#[derive(Clone)]
pub struct SharedAppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    /// Broadcast receiver source for live candle updates.
    candle_broadcast: broadcast::Sender<PreSerializedCandleUpdate>,
    /// QuestDB config for historical candle queries.
    questdb_config: QuestDbConfig,
    /// QuestDB HTTP candle querier (None if QuestDB unavailable).
    candle_querier: Option<QuestDbCandleQuerier>,
}

impl SharedAppState {
    /// Creates new shared state.
    pub fn new(
        candle_broadcast: broadcast::Sender<PreSerializedCandleUpdate>,
        questdb_config: QuestDbConfig,
    ) -> Self {
        let candle_querier = QuestDbCandleQuerier::new(&questdb_config)
            .map_err(|err| {
                tracing::warn!(
                    ?err,
                    "failed to create QuestDB candle querier — historical queries unavailable"
                );
                err
            })
            .ok();

        Self {
            inner: Arc::new(AppStateInner {
                candle_broadcast,
                questdb_config,
                candle_querier,
            }),
        }
    }

    /// Returns a new broadcast receiver for candle updates.
    pub fn subscribe_candles(&self) -> broadcast::Receiver<PreSerializedCandleUpdate> {
        self.inner.candle_broadcast.subscribe()
    }

    /// Returns the QuestDB config for making SQL queries.
    pub fn questdb_config(&self) -> &QuestDbConfig {
        &self.inner.questdb_config
    }

    /// Returns the candle querier (None if QuestDB is unavailable).
    pub fn candle_querier(&self) -> Option<&QuestDbCandleQuerier> {
        self.inner.candle_querier.as_ref()
    }
}
