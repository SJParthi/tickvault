//! A dedicated tokio runtime for the Dhan socket READ tasks (2026-10-02).
//!
//! # The problem this removes
//!
//! Until this module, the sixteen socket read tasks ran on the process's ONE
//! tokio runtime — two worker threads on the deployed box
//! (`TICKVAULT_TOKIO_WORKER_THREADS=2`) — together with the frame drain (the
//! per-frame decode + candle fold, the busiest task in the process) and every
//! background task: QuestDB DDL, partition archive, the depth steering loop,
//! contract attach, token renewal, the API server, health pollers.
//!
//! tokio does not preempt. A task that runs for 20 ms without reaching an
//! `.await` holds its worker for 20 ms. With two workers, the drain on one
//! and any synchronous stretch of background work on the other (a `std::fs`
//! call, a long sort, the seal escalation's inline disk write when its queue
//! is full) left NO worker to poll the sockets. A socket that is not polled
//! does not read, and does not answer Dhan's ping — "more than 40 seconds"
//! of that and Dhan closes it. The reader's own code was already
//! non-blocking ([`super::pool_supervisor::WalRingSink`] is one WAL
//! `try_send` and one ring `try_send`); what it could not control was whether
//! it got a thread at all.
//!
//! # The fix
//!
//! The read tasks run on a runtime of their own, on OS threads named
//! `tv-ws-reader`, that nothing else is spawned onto. The operating system
//! DOES preempt, so a busy drain or a stuck background task now costs the
//! readers a scheduler time slice, never an unbounded wait. Everything the
//! reader touches across the boundary is runtime-agnostic: tokio `mpsc`
//! channels, atomics, `ArcSwap`. Each socket's wire writer is spawned from
//! inside its read task, so it lands on this runtime too.
//!
//! # Install once, from `main`, and only from `main`
//!
//! [`install_reader_runtime`] is called once at boot. Until it is called —
//! which is every unit and integration test — [`spawn_on_reader_runtime`]
//! falls back to `tokio::spawn` on the caller's runtime, which is exactly the
//! previous behaviour. That keeps paused-clock tests deterministic: a task on
//! a second runtime would not see a test's paused clock.
//!
//! The runtime is held in a `static` and therefore never dropped. Dropping a
//! runtime from inside async code panics, and the read tasks must outlive
//! every other task anyway: shutdown closes the sockets first (Z11d).
//!
//! # What this does NOT do
//!
//! * It does not pin threads to cores. Pinning needs `sched_setaffinity`,
//!   which needs the `libc` crate declared (it is already in the lock file
//!   as a transitive dependency) — a new direct dependency, which needs the
//!   owner's approval. The process-wide core set stays the systemd unit's
//!   `AllowedCPUs=1-2`.
//! * It does not change thread priority, for the same reason.
//!
//! # Complexity
//!
//! [`spawn_on_reader_runtime`] is O(1): one `OnceLock` load and one spawn,
//! once per connection at dial time — never per frame.

use std::sync::OnceLock;

/// Environment variable holding the reader runtime's thread count.
///
/// `0` turns the dedicated runtime OFF: the read tasks share the main runtime
/// as they did before 2026-10-02. That is the rollback, and it needs no
/// rebuild — set it in the systemd unit and restart.
pub const WS_READER_THREADS_ENV: &str = "TICKVAULT_WS_READER_THREADS";

/// Thread count when the variable is absent or unusable.
///
/// One thread, because the per-frame work on this runtime is a TLS decrypt,
/// a WebSocket frame decode and two `try_send`s — microseconds — across at
/// most sixteen sockets. A second thread would buy parallel decrypt that the
/// measured frame rate does not need, and costs one more runnable thread on a
/// process confined to two cores.
pub const DEFAULT_WS_READER_THREADS: usize = 1;

/// Largest accepted thread count. Beyond the process's two-core confinement
/// any further thread is a context switch, not throughput; the ceiling only
/// stops a typo from spawning hundreds.
pub const MAX_WS_READER_THREADS: usize = 4;

/// Thread name, so `top -H` / `perf` on the box can attribute CPU to the
/// readers separately from `tv-worker`.
pub const WS_READER_THREAD_NAME: &str = "tv-ws-reader";

static READER_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Resolves the reader thread count from the raw environment value.
///
/// Fail-SAFE: absent, empty, non-numeric or above [`MAX_WS_READER_THREADS`]
/// all give [`DEFAULT_WS_READER_THREADS`]. `0` is honoured: it is the
/// documented off switch.
#[must_use]
pub fn resolve_ws_reader_threads(raw: Option<&str>) -> usize {
    match raw.map(str::trim) {
        Some(v) if !v.is_empty() => match v.parse::<usize>() {
            Ok(n) if n <= MAX_WS_READER_THREADS => n,
            _ => DEFAULT_WS_READER_THREADS,
        },
        _ => DEFAULT_WS_READER_THREADS,
    }
}

/// Builds (but does not install) a reader runtime with `threads` workers.
///
/// Separate from [`install_reader_runtime`] so a test can build one without
/// touching the process-global slot.
///
/// # Errors
/// The OS refused to create a thread.
pub fn build_reader_runtime(threads: usize) -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads.max(1))
        .thread_name(WS_READER_THREAD_NAME)
        .enable_all()
        .build()
}

/// What [`install_reader_runtime`] did, for the boot log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderRuntimeInstall {
    /// A dedicated runtime with this many threads now carries the read tasks.
    Installed(usize),
    /// `threads == 0`: the operator turned it off; read tasks share the main
    /// runtime.
    Disabled,
    /// A runtime was already installed; nothing changed.
    AlreadyInstalled,
}

/// Installs the dedicated reader runtime. Called once, from `main`, before
/// any socket is dialled.
///
/// # Errors
/// The OS refused to create a thread. The caller logs it and boots on the
/// shared runtime — a slower reader is better than no feed.
pub fn install_reader_runtime(threads: usize) -> std::io::Result<ReaderRuntimeInstall> {
    if threads == 0 {
        return Ok(ReaderRuntimeInstall::Disabled);
    }
    if READER_RUNTIME.get().is_some() {
        return Ok(ReaderRuntimeInstall::AlreadyInstalled);
    }
    let runtime = build_reader_runtime(threads)?;
    match READER_RUNTIME.set(runtime) {
        Ok(()) => Ok(ReaderRuntimeInstall::Installed(threads)),
        // A racing installer won; ours is dropped here, outside any async
        // context (this is called from `main` before `block_on`).
        Err(_ours) => Ok(ReaderRuntimeInstall::AlreadyInstalled),
    }
}

/// The installed reader runtime's handle, if any.
#[must_use]
pub fn reader_runtime_handle() -> Option<tokio::runtime::Handle> {
    // APPROVED: Handle is an Arc; cloned once per socket spawn, never per frame
    READER_RUNTIME.get().map(|rt| rt.handle().clone())
}

/// Spawns a socket read task: on the dedicated reader runtime when one is
/// installed, otherwise on the caller's runtime (`tokio::spawn`).
///
/// # Panics
/// Like `tokio::spawn`, when no runtime is installed and the caller is not
/// inside one.
pub fn spawn_on_reader_runtime<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    match READER_RUNTIME.get() {
        Some(rt) => rt.spawn(future),
        None => tokio::spawn(future),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_ws_reader_threads_defaults_on_absent_or_garbage() {
        assert_eq!(resolve_ws_reader_threads(None), DEFAULT_WS_READER_THREADS);
        for bad in ["", "  ", "abc", "-1", "1.5", "99", "18446744073709551616"] {
            assert_eq!(
                resolve_ws_reader_threads(Some(bad)),
                DEFAULT_WS_READER_THREADS,
                "input {bad:?} must fall back to the default"
            );
        }
    }

    #[test]
    fn resolve_ws_reader_threads_honours_zero_as_off_and_valid_counts() {
        assert_eq!(resolve_ws_reader_threads(Some("0")), 0);
        assert_eq!(resolve_ws_reader_threads(Some(" 2 ")), 2);
        assert_eq!(
            resolve_ws_reader_threads(Some(&MAX_WS_READER_THREADS.to_string())),
            MAX_WS_READER_THREADS
        );
    }

    #[test]
    fn install_reader_runtime_with_zero_threads_is_disabled_and_installs_nothing() {
        assert_eq!(
            install_reader_runtime(0).expect("zero never touches the OS"),
            ReaderRuntimeInstall::Disabled
        );
    }

    #[test]
    fn build_reader_runtime_names_its_threads_and_runs_tasks() {
        let rt = build_reader_runtime(1).expect("one thread");
        let name = rt.block_on(async {
            tokio::spawn(async {
                std::thread::current()
                    .name()
                    .map(str::to_owned)
                    .unwrap_or_default()
            })
            .await
            .expect("task ran")
        });
        assert_eq!(name, WS_READER_THREAD_NAME);
    }

    /// Without an installed runtime the spawn lands on the caller's runtime,
    /// which is what every test in the workspace relies on. This test never
    /// installs, so it cannot leak a runtime into its siblings.
    #[tokio::test]
    async fn test_spawn_on_reader_runtime_and_reader_runtime_handle_fall_back_to_the_callers_runtime()
     {
        if reader_runtime_handle().is_some() {
            return;
        }
        // A current-thread test runtime runs its tasks on the test thread.
        let caller = std::thread::current().id();
        let ran_on = spawn_on_reader_runtime(async { std::thread::current().id() })
            .await
            .expect("task ran");
        assert_eq!(ran_on, caller);
    }
}
