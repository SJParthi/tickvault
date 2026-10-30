//! Boot wiring for the hot-path latency proof (2026-10-30, plan item DT).
//!
//! Two things `main.rs` calls once each, kept here so `main.rs` does not grow
//! (it is on the source-size ratchet):
//!
//! - [`build_main_runtime`]: the main tokio runtime, whose workers register
//!   their thread ids for the per-second scheduling-wait series.
//! - [`start_hot_path_proof`]: the per-minute proof tables' DDL task and
//!   writer thread, started before the telemetry publisher so the first
//!   closed minute has somewhere to go.
//!
//! Both are cold (boot only). Nothing here runs per tick.

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_storage::hot_path_latency_minute::{
    ENSURE_ATTEMPTS, ENSURE_RETRY_INTERVAL, ensure_hot_path_latency_minute_table,
    spawn_hot_path_latency_minute_writer,
};
use tracing::error;

/// The main tokio runtime (the one the frame drain runs on).
///
/// `enable_all` matches what `#[tokio::main]` installed (IO + time drivers).
/// Named threads so `top -H` / `perf` on the box can tell a runtime worker
/// apart from a blocking-pool thread while attributing a stall.
///
/// DT0 review (2026-10-29): every thread that acts as a worker registers its
/// thread id from `on_thread_park` (tokio runs that hook only on worker
/// threads; after the first park it is one thread-local read), and gives its
/// slot back from `on_thread_stop`, so the telemetry thread can read the
/// scheduling wait of every worker, whichever one hosts the drain.
///
/// # Errors
/// The runtime builder's error (thread spawn failure).
pub fn build_main_runtime(worker_threads: usize) -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .thread_name("tv-worker")
        .enable_all()
        .on_thread_park(|| {
            let _slot = tickvault_storage::hot_thread_sched::register_main_worker_thread();
        })
        .on_thread_stop(tickvault_storage::hot_thread_sched::release_main_worker_thread)
        .build()
}

/// Starts the per-minute proof tables: a task that creates them (retried
/// quietly while QuestDB starts; only the last attempt logs) and the writer
/// thread with its queue. Call once, inside the runtime, before the telemetry
/// publisher. Returns whether the writer started; a writer that cannot start
/// logs one coded `error!` and every minute is then counted as dropped.
pub fn start_hot_path_proof(questdb: &QuestDbConfig) -> bool {
    let ddl_config = questdb.clone();
    tokio::spawn(async move {
        for attempt in 1..=ENSURE_ATTEMPTS {
            if ensure_hot_path_latency_minute_table(&ddl_config, attempt == ENSURE_ATTEMPTS).await {
                break;
            }
            if attempt < ENSURE_ATTEMPTS {
                tokio::time::sleep(ENSURE_RETRY_INTERVAL).await;
            }
        }
    });
    match spawn_hot_path_latency_minute_writer(questdb) {
        Ok(_writer) => true,
        Err(err) => {
            error!(
                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                error = %err,
                "the hot-path latency minute writer FAILED TO START — the per-minute \
                 latency table will not be written (minutes counted as dropped)"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The main runtime's workers register for the scheduling-wait series
    /// when they park and give the slot back when the runtime stops.
    #[test]
    fn test_build_main_runtime_registers_its_workers_for_the_sched_series() {
        use tickvault_storage::hot_thread_sched::main_workers_registered;
        let rt = build_main_runtime(2).expect("runtime");
        let name = rt.block_on(async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            tokio::spawn(async {
                std::thread::current()
                    .name()
                    .map(str::to_owned)
                    .unwrap_or_default()
            })
            .await
            .expect("task ran")
        });
        assert_eq!(name, "tv-worker");
        if std::path::Path::new("/proc/thread-self").exists() {
            assert!(
                main_workers_registered() >= 1,
                "an idle worker parked and registered"
            );
        }
        drop(rt);
    }

    /// The first start succeeds (the writer thread does not connect until a
    /// row arrives); the queue is process-wide, so a second start is refused
    /// and logged rather than starting a second writer.
    #[test]
    fn test_start_hot_path_proof_starts_once_and_refuses_a_second_start() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_owned(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let (first, second) =
            rt.block_on(async { (start_hot_path_proof(&cfg), start_hot_path_proof(&cfg)) });
        assert!(first, "the first start installs the writer");
        assert!(!second, "a second start is refused");
    }
}
