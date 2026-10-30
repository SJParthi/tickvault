//! Boot wiring for plan item 4 (2026-10-30): the background-task heartbeats
//! and the memory action ladder.
//!
//! Kept out of `main.rs`, which is at its size pin
//! (`source_size_ratchet_guard`). Called once from the boot sequence, AFTER
//! the metrics recorder is installed (a handle resolved before it is a no-op
//! forever) and BEFORE any watched supervisor is spawned (so each reads the
//! respawn switch as configured).

use tickvault_common::config::ApplicationConfig;
use tracing::warn;

/// Sets the missed-heartbeat respawn switch from `[task_heartbeat]` and
/// starts the `tv-task-watch` thread with `[memory_watchdog]`.
///
/// Returns `true` when the thread started. A refused thread is logged as an
/// uncoded, log-only `warn!` and boot continues: nothing on the capture path
/// depends on this thread. Deliberately NOT `BOOT-02` (hostile review
/// 2026-10-30): that code pages "boot is blocked, the app is not running",
/// which would be false here, and this change adds no page.
pub fn start_task_watch(config: &ApplicationConfig) -> bool {
    tickvault_storage::task_heartbeat::set_respawn_on_missed_beats(
        config.task_heartbeat.respawn_on_missed_beats,
    );
    match tickvault_storage::memory_watchdog::spawn_watch_thread(config.memory_watchdog) {
        Ok(_detached) => true,
        Err(err) => {
            warn!(
                error = %err,
                "the task-watch thread FAILED TO SPAWN — background-task heartbeats and the \
                 memory watchdog will not run this session (host thread or memory limit); \
                 capture is unaffected"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_from(toml: &str) -> ApplicationConfig {
        use figment::Figment;
        use figment::providers::{Format, Toml};
        let base = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/base.toml"
        ))
        .expect("config/base.toml");
        Figment::new()
            .merge(Toml::string(&base))
            .merge(Toml::string(toml))
            .extract()
            .expect("config parses")
    }

    #[test]
    fn test_start_task_watch_reads_the_switch_and_starts_the_thread() {
        // base.toml ships both features off.
        let cfg = config_from("");
        assert!(!cfg.task_heartbeat.respawn_on_missed_beats);
        assert_eq!(
            cfg.memory_watchdog.mode,
            tickvault_common::config::MemoryWatchdogMode::Off
        );
        assert!(start_task_watch(&cfg), "the watch thread must start");
        assert!(!tickvault_storage::task_heartbeat::respawn_on_missed_beats());

        // The switch follows config. Reset afterwards: it is process-global.
        let on = config_from("[task_heartbeat]\nrespawn_on_missed_beats = true\n");
        assert!(start_task_watch(&on));
        assert!(tickvault_storage::task_heartbeat::respawn_on_missed_beats());
        tickvault_storage::task_heartbeat::set_respawn_on_missed_beats(false);
    }
}
