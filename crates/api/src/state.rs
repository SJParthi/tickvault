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

#[cfg(test)]
mod tests {
    use super::*;
    use dhan_live_trader_common::config::QuestDbConfig;

    #[test]
    fn test_shared_app_state_new_and_questdb_config() {
        let config = QuestDbConfig {
            host: "test-host".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let state = SharedAppState::new(config);
        assert_eq!(state.questdb_config().host, "test-host");
        assert_eq!(state.questdb_config().ilp_port, 9009);
        assert_eq!(state.questdb_config().http_port, 9000);
    }

    #[test]
    fn test_shared_app_state_clone_shares_data() {
        let config = QuestDbConfig {
            host: "clone-test".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let state1 = SharedAppState::new(config);
        let state2 = state1.clone();
        assert_eq!(state2.questdb_config().host, "clone-test");
    }
}
