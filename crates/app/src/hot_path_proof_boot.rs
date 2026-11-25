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

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_storage::hot_path_latency_minute::{
    ENSURE_ATTEMPTS, ENSURE_RETRY_INTERVAL, ProofTableReadiness,
    ensure_hot_path_latency_minute_table, spawn_hot_path_latency_minute_writer,
};
use tracing::{error, info};

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

/// Starts the gated writer before its ensure task and before the telemetry
/// publisher. The initial quiet retries become bounded-backoff background
/// retries until real metadata confirms every key. Returns thread-start
/// success, not database readiness. Shutdown ownership remains separate.
pub fn start_hot_path_proof(questdb: &QuestDbConfig) -> bool {
    start_with(
        questdb,
        spawn_hot_path_latency_minute_writer,
        |ddl_config, readiness| {
            // Detached like the existing boot tasks; runtime shutdown cancels
            // ensure, while the writer stays fail-closed if it was never keyed.
            drop(tokio::spawn(async move {
                retry_until_keyed(move |report| {
                    let config = ddl_config.clone();
                    let readiness = Arc::clone(&readiness);
                    async move {
                        ensure_hot_path_latency_minute_table(&config, report, &readiness).await
                    }
                })
                .await;
            }));
        },
    )
}

fn start_with(
    questdb: &QuestDbConfig,
    spawn_writer: impl FnOnce(
        &QuestDbConfig,
        Arc<ProofTableReadiness>,
    ) -> std::io::Result<std::thread::JoinHandle<()>>,
    spawn_ensure: impl FnOnce(QuestDbConfig, Arc<ProofTableReadiness>),
) -> bool {
    let readiness = Arc::new(ProofTableReadiness::default());
    match spawn_writer(questdb, Arc::clone(&readiness)) {
        Ok(_writer) => {
            // A rejected second start or OS spawn failure starts no orphan
            // ensure task, and cannot open another writer's per-start gate.
            spawn_ensure(questdb.clone(), readiness);
            true
        }
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

async fn retry_until_keyed<F, Fut>(mut ensure: F) -> u32
where
    F: FnMut(bool) -> Fut,
    Fut: Future<Output = bool>,
{
    let mut attempt = 0_u32;
    let mut delay = ENSURE_RETRY_INTERVAL;
    let max_delay = Duration::from_secs(crate::candle_ddl_boot::DDL_BACKGROUND_RETRY_MAX_SECS);
    loop {
        attempt = attempt.saturating_add(1);
        if ensure(attempt == ENSURE_ATTEMPTS).await {
            info!(
                attempt,
                "hot-path proof tables verified keyed — buffered writes may resume"
            );
            return attempt;
        }
        tokio::time::sleep(delay).await;
        if attempt >= ENSURE_ATTEMPTS {
            delay = delay.saturating_mul(2).min(max_delay);
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

    #[test]
    fn test_rejected_writer_start_never_spawns_an_ensure_task() {
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_owned(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        for reason in ["already installed", "injected OS spawn failure"] {
            assert!(!start_with(
                &cfg,
                |_, _| Err(std::io::Error::other(reason)),
                |_, _| panic!("a failed writer must not start an ensure task"),
            ));
        }
    }

    #[test]
    fn test_successful_start_passes_the_same_gate_to_the_ensure_task() {
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_owned(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let shared = std::cell::RefCell::new(None);
        assert!(start_with(
            &cfg,
            |_, readiness| {
                *shared.borrow_mut() = Some(readiness);
                std::thread::Builder::new().spawn(|| {})
            },
            |_, readiness| assert!(Arc::ptr_eq(
                shared.borrow().as_ref().expect("writer gate"),
                &readiness
            )),
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn test_ensure_retries_past_ten_with_a_bounded_backoff() {
        let mut times = Vec::new();
        let mut reports = Vec::new();
        let attempts = retry_until_keyed(|report| {
            times.push(tokio::time::Instant::now());
            reports.push(report);
            std::future::ready(times.len() == 18)
        })
        .await;
        assert_eq!(attempts, 18);
        assert_eq!(reports.iter().filter(|&&r| r).count(), 1);
        assert!(reports[usize::try_from(ENSURE_ATTEMPTS - 1).expect("index")]);
        let delays: Vec<_> = times.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(delays[..10].iter().all(|&d| d == ENSURE_RETRY_INTERVAL));
        assert_eq!(delays[10], ENSURE_RETRY_INTERVAL * 2);
        assert_eq!(delays[11], ENSURE_RETRY_INTERVAL * 4);
        assert!(
            delays[14..].iter().all(|&d| d
                == Duration::from_secs(crate::candle_ddl_boot::DDL_BACKGROUND_RETRY_MAX_SECS))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_cancelling_a_retry_does_not_schedule_another_attempt() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let task = tokio::spawn(retry_until_keyed(move |_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::future::ready(false)
        }));
        tokio::task::yield_now().await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        task.abort();
        assert!(task.await.expect_err("cancelled").is_cancelled());
        tokio::time::advance(ENSURE_RETRY_INTERVAL * 100).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
