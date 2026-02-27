//! Shared application state for the API server.

use std::sync::Arc;

use dhan_live_trader_common::config::QuestDbConfig;

/// Shared state available to all API handlers via axum's `State` extractor.
#[derive(Clone)]
pub struct SharedAppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    /// QuestDB config for SQL queries (stats endpoint).
    questdb_config: QuestDbConfig,
}

impl SharedAppState {
    /// Creates new shared state.
    pub fn new(questdb_config: QuestDbConfig) -> Self {
        Self {
            inner: Arc::new(AppStateInner { questdb_config }),
        }
    }

    /// Returns the QuestDB config for making SQL queries.
    pub fn questdb_config(&self) -> &QuestDbConfig {
        &self.inner.questdb_config
    }
}
