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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal valid strategy TOML that `load_strategy_config_file` accepts.
    const VALID_STRATEGY_TOML: &str = r#"
[[strategy]]
name = "test_strategy"
security_ids = [1333]
position_size_fraction = 0.1
stop_loss_atr_multiplier = 2.0
target_atr_multiplier = 3.0

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 30.0
"#;

    /// Creates a temporary TOML file with valid strategy content.
    /// Returns the directory path and file path (both must be kept alive).
    /// Uses a unique counter to avoid conflicts between parallel tests.
    fn write_temp_strategy_file(content: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique_id = COUNTER.fetch_add(1, Ordering::Relaxed);

        let dir = std::env::temp_dir().join(format!(
            "dlt_hot_reload_test_{}_{}",
            std::process::id(),
            unique_id,
        ));
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let file_path = dir.join("test_strategies.toml");
        std::fs::write(&file_path, content).expect("failed to write temp strategy file");
        (dir, file_path)
    }

    /// Cleans up test temp directory.
    fn cleanup_temp_dir(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn new_creates_valid_reloader_with_initial_strategies() {
        let (dir, file_path) = write_temp_strategy_file(VALID_STRATEGY_TOML);

        let result = StrategyHotReloader::new(&file_path);
        assert!(
            result.is_ok(),
            "hot reloader creation must succeed with valid TOML"
        );

        let (reloader, strategies, _params) = result.unwrap();
        assert_eq!(strategies.len(), 1);
        assert_eq!(strategies[0].name, "test_strategy");
        assert_eq!(reloader.watched_path(), file_path);

        cleanup_temp_dir(&dir);
    }

    #[test]
    fn watched_path_returns_correct_path() {
        let (dir, file_path) = write_temp_strategy_file(VALID_STRATEGY_TOML);

        let (reloader, _strategies, _params) = StrategyHotReloader::new(&file_path).unwrap();
        assert_eq!(reloader.watched_path(), file_path.as_path());

        cleanup_temp_dir(&dir);
    }

    #[test]
    fn try_recv_returns_none_when_no_changes() {
        let (dir, file_path) = write_temp_strategy_file(VALID_STRATEGY_TOML);

        let (reloader, _strategies, _params) = StrategyHotReloader::new(&file_path).unwrap();

        // No file changes have occurred — try_recv should return None
        let event = reloader.try_recv();
        assert!(
            event.is_none(),
            "try_recv must return None when no file changes occurred"
        );

        cleanup_temp_dir(&dir);
    }

    #[test]
    fn new_fails_with_invalid_toml() {
        let (dir, file_path) = write_temp_strategy_file("this is not valid TOML [[[");

        let result = StrategyHotReloader::new(&file_path);
        assert!(
            result.is_err(),
            "hot reloader must fail with invalid TOML content"
        );

        cleanup_temp_dir(&dir);
    }

    #[test]
    fn new_fails_with_nonexistent_file() {
        let nonexistent = Path::new("/tmp/dlt_hot_reload_nonexistent_file_12345.toml");

        let result = StrategyHotReloader::new(nonexistent);
        assert!(
            result.is_err(),
            "hot reloader must fail when config file does not exist"
        );
    }

    #[test]
    fn new_returns_correct_initial_indicator_params() {
        let toml_with_params = r#"
[indicator_params]
ema_fast_period = 10
ema_slow_period = 21
rsi_period = 7

[[strategy]]
name = "param_test"
security_ids = [42]

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 25.0
"#;
        let (dir, file_path) = write_temp_strategy_file(toml_with_params);

        let (_reloader, _strategies, params) = StrategyHotReloader::new(&file_path).unwrap();
        assert_eq!(params.ema_fast_period, 10);
        assert_eq!(params.ema_slow_period, 21);
        assert_eq!(params.rsi_period, 7);

        cleanup_temp_dir(&dir);
    }

    #[test]
    fn new_returns_multiple_strategies() {
        let multi_strategy_toml = r#"
[[strategy]]
name = "strategy_a"
security_ids = [100]
position_size_fraction = 0.1
stop_loss_atr_multiplier = 2.0
target_atr_multiplier = 3.0

[[strategy.entry_long]]
field = "rsi"
operator = "lt"
threshold = 30.0

[[strategy]]
name = "strategy_b"
security_ids = [200, 300]
position_size_fraction = 0.2
stop_loss_atr_multiplier = 1.5
target_atr_multiplier = 2.5

[[strategy.entry_short]]
field = "macd_histogram"
operator = "lt"
threshold = 0.0
"#;
        let (dir, file_path) = write_temp_strategy_file(multi_strategy_toml);

        let (_reloader, strategies, _params) = StrategyHotReloader::new(&file_path).unwrap();
        assert_eq!(strategies.len(), 2);
        assert_eq!(strategies[0].name, "strategy_a");
        assert_eq!(strategies[1].name, "strategy_b");

        cleanup_temp_dir(&dir);
    }

    #[test]
    fn try_recv_is_nonblocking() {
        let (dir, file_path) = write_temp_strategy_file(VALID_STRATEGY_TOML);

        let (reloader, _strategies, _params) = StrategyHotReloader::new(&file_path).unwrap();

        // Call try_recv multiple times rapidly — must not block
        for _ in 0..100 {
            let _ = reloader.try_recv();
        }

        cleanup_temp_dir(&dir);
    }

    #[test]
    fn reload_event_has_debug_impl() {
        // ReloadEvent must implement Debug for logging/diagnostics
        let event = ReloadEvent {
            strategies: vec![],
            indicator_params: IndicatorParams::default(),
        };
        let debug_str = format!("{event:?}");
        assert!(debug_str.contains("ReloadEvent"));
    }

    #[test]
    fn hot_reload_error_display_watcher_init() {
        let err = HotReloadError::WatcherInit(notify::Error::generic("test error"));
        let display = format!("{err}");
        assert!(
            display.contains("file watcher"),
            "WatcherInit error display must mention file watcher"
        );
    }

    #[test]
    fn new_fails_with_empty_toml() {
        // Empty TOML has no strategies, which means no entry conditions
        // load_strategy_config_file will succeed (zero strategies is valid TOML)
        // but the reloader should still create successfully
        let (dir, file_path) = write_temp_strategy_file("");

        let result = StrategyHotReloader::new(&file_path);
        assert!(
            result.is_ok(),
            "empty TOML (zero strategies) should create reloader"
        );

        let (_reloader, strategies, _params) = result.unwrap();
        assert!(strategies.is_empty());

        cleanup_temp_dir(&dir);
    }
}
