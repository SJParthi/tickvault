//! Hot-reload watcher for strategy configuration files.
//!
//! Uses the `notify` crate to watch for file changes. When a strategy TOML
//! file is modified, it re-parses and sends the new definitions through
//! a bounded channel. The consumer (strategy engine) swaps in the new
//! definitions on the next tick — no lock contention on the hot path.
//!
//! # Architecture
//! ```text
//! [notify watcher] → file changed → parse TOML → validate
//!                  → send via crossbeam channel → strategy engine receives
//!                  → swap definitions (cold path, between ticks)
//! ```

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{error, info, warn};

use super::config::{StrategyConfigError, load_strategy_config_file};
use super::types::StrategyDefinition;
use crate::indicator::IndicatorParams;

// ---------------------------------------------------------------------------
// Hot-Reload Types
// ---------------------------------------------------------------------------

/// A reload event containing new strategy definitions and indicator params.
#[derive(Debug)]
pub struct ReloadEvent {
    /// Updated strategy definitions.
    pub strategies: Vec<StrategyDefinition>,
    /// Updated indicator parameters.
    pub indicator_params: IndicatorParams,
}

/// Error type for hot-reload operations.
#[derive(Debug, thiserror::Error)]
pub enum HotReloadError {
    /// File watcher setup failed.
    #[error("failed to create file watcher: {0}")]
    WatcherInit(#[from] notify::Error),

    /// Strategy config parse/validation failed.
    #[error("config reload failed: {0}")]
    ConfigError(#[from] StrategyConfigError),
}

// ---------------------------------------------------------------------------
// Hot-Reload Watcher
// ---------------------------------------------------------------------------

/// Watches a strategy config file for changes and sends reload events.
///
/// Runs on a dedicated thread (spawned by `notify`). The consumer should
/// check the receiver on each tick or periodically.
pub struct StrategyHotReloader {
    /// The file watcher handle (must be kept alive).
    _watcher: RecommendedWatcher,
    /// Receiver for reload events.
    reload_receiver: mpsc::Receiver<ReloadEvent>,
    /// Path being watched (for diagnostics).
    watched_path: PathBuf,
}

impl StrategyHotReloader {
    /// Creates a new hot-reloader watching the given config file.
    ///
    /// # Cold path
    /// Called once at startup. Sets up the `notify` file watcher.
    ///
    /// # Returns
    /// The hot-reloader and the initial parsed config.
    pub fn new(
        config_path: &Path,
    ) -> Result<(Self, Vec<StrategyDefinition>, IndicatorParams), HotReloadError> {
        // Parse initial config
        let (initial_strategies, initial_params) = load_strategy_config_file(config_path)?;

        info!(
            path = %config_path.display(),
            strategy_count = initial_strategies.len(),
            "loaded initial strategy config"
        );

        // Set up file watcher
        let (reload_sender, reload_receiver) = mpsc::channel::<ReloadEvent>();
        let watched_path = config_path.to_path_buf();
        let reload_path = config_path.to_path_buf();

        let mut watcher = notify::recommended_watcher(
            move |result: Result<Event, notify::Error>| match result {
                Ok(event) => {
                    if matches!(
                        event.kind,
                        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                    ) {
                        handle_file_change(&reload_path, &reload_sender);
                    }
                }
                Err(err) => {
                    error!(?err, "file watcher error");
                }
            },
        )?;

        // Watch the parent directory (some editors write to a temp file then rename)
        let watch_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
        watcher.watch(watch_dir, RecursiveMode::NonRecursive)?;

        info!(
            path = %config_path.display(),
            watch_dir = %watch_dir.display(),
            "strategy hot-reload watcher started"
        );

        Ok((
            Self {
                _watcher: watcher,
                reload_receiver,
                watched_path,
            },
            initial_strategies,
            initial_params,
        ))
    }

    /// Checks for a pending reload event (non-blocking).
    ///
    /// Call this between ticks or at the start of each tick batch.
    /// Returns `Some(ReloadEvent)` if the config file was modified.
    ///
    /// # Performance
    /// O(1) — `try_recv` is a non-blocking channel check.
    pub fn try_recv(&self) -> Option<ReloadEvent> {
        // Drain all pending events, keep only the latest
        let mut latest = None;
        while let Ok(event) = self.reload_receiver.try_recv() {
            latest = Some(event);
        }
        latest
    }

    /// Returns the path being watched.
    pub fn watched_path(&self) -> &Path {
        &self.watched_path
    }
}

/// Handles a file change event: re-parse and send reload event.
fn handle_file_change(config_path: &Path, sender: &mpsc::Sender<ReloadEvent>) {
    match load_strategy_config_file(config_path) {
        Ok((strategies, indicator_params)) => {
            info!(
                path = %config_path.display(),
                strategy_count = strategies.len(),
                "strategy config reloaded successfully"
            );

            let event = ReloadEvent {
                strategies,
                indicator_params,
            };

            if let Err(err) = sender.send(event) {
                warn!(?err, "failed to send reload event — receiver dropped");
            }
        }
        Err(err) => {
            // Log error but keep old config — never crash on bad config reload
            error!(
                ?err,
                path = %config_path.display(),
                "strategy config reload failed — keeping previous config"
            );
        }
    }
}
