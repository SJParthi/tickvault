//! Shared application state for the API server.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use dhan_live_trader_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

/// Shared state available to all API handlers via axum's `State` extractor.
#[derive(Clone)]
pub struct SharedAppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    /// QuestDB config for SQL queries (stats endpoint) and instrument persistence.
    questdb_config: QuestDbConfig,
    /// Dhan API config (CSV URLs for manual instrument rebuild).
    dhan_config: DhanConfig,
    /// Instrument config (cache dir, timeout, build window).
    instrument_config: InstrumentConfig,
    /// Concurrency guard: prevents concurrent instrument rebuilds.
    rebuild_in_progress: AtomicBool,
}

impl SharedAppState {
    /// Creates new shared state.
    pub fn new(
        questdb_config: QuestDbConfig,
        dhan_config: DhanConfig,
        instrument_config: InstrumentConfig,
    ) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                questdb_config,
                dhan_config,
                instrument_config,
                rebuild_in_progress: AtomicBool::new(false),
            }),
        }
    }

    /// Returns the QuestDB config for making SQL queries.
    pub fn questdb_config(&self) -> &QuestDbConfig {
        &self.inner.questdb_config
    }

    /// Returns the Dhan API config (CSV URLs).
    pub fn dhan_config(&self) -> &DhanConfig {
        &self.inner.dhan_config
    }

    /// Returns the instrument config (cache dir, timeout, build window).
    pub fn instrument_config(&self) -> &InstrumentConfig {
        &self.inner.instrument_config
    }

    /// Returns the rebuild-in-progress atomic flag.
    pub fn rebuild_in_progress(&self) -> &AtomicBool {
        &self.inner.rebuild_in_progress
    }
}
