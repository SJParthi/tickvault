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
//! # Core pinning (2026-10-02, owner-approved `libc` dependency)
//!
//! Each reader thread pins itself to ONE core with `sched_setaffinity`, from
//! the runtime builder's `on_thread_start`. The core is
//! [`WS_READER_CORE_ENV`] (a core id, or `off`); absent, it is
//! [`DEFAULT_WS_READER_CORE`] when the process's allowed set contains it, and
//! no pin otherwise. Core 0 is NEVER used: it services network interrupts,
//! and the reader depends on that softirq work. A refused or failed pin runs
//! the reader unpinned and is logged once at boot (`HOT-PATH-03`); boot never
//! fails over it. `tv_ws_reader_pinned_core` carries the pinned core, or -1.
//! Linux only; every other target runs unpinned.
//!
//! # What this does NOT do
//!
//! * It does not change thread priority.
//! * It does not keep other threads OFF the pinned core: the process may
//!   still run any thread on it. The pin removes the reader's migrations,
//!   not its neighbours.
//!
//! # Complexity
//!
//! [`spawn_on_reader_runtime`] is O(1): one `OnceLock` load and one spawn,
//! once per connection at dial time — never per frame. The pin is one
//! syscall per reader thread, once, at thread start.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU64, Ordering};

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

/// Environment variable naming the core the reader threads pin to: a core
/// id, or `off`. Absent means [`DEFAULT_WS_READER_CORE`] when allowed.
pub const WS_READER_CORE_ENV: &str = "TICKVAULT_WS_READER_CORE";

/// Default reader core. The deployed unit confines the process to cores 1-2
/// (`AllowedCPUs=1-2`), so core 1 is the first core the process owns that
/// is not core 0.
pub const DEFAULT_WS_READER_CORE: usize = 1;

/// The core that is never pinned to: it services network interrupts, so a
/// reader there would contend with the softirq work it depends on.
pub const NEVER_PIN_CORE: usize = 0;

/// Largest core id the affinity mask can express (`CPU_SETSIZE` - 1 on glibc).
pub const MAX_WS_READER_CORE: usize = 1023;

/// Core the reader threads are pinned to, or -1. Written by
/// `on_thread_start`, read once at boot for the log and the gauge.
static PINNED_CORE: AtomicI64 = AtomicI64::new(-1);
/// Reader threads whose pin syscall failed.
static PIN_FAILURES: AtomicU64 = AtomicU64::new(0);
/// The last failed pin's OS error number (0 = none).
static PIN_LAST_ERRNO: AtomicI32 = AtomicI32::new(0);

/// What the reader threads will do about core pinning, decided once in
/// `main` from [`WS_READER_CORE_ENV`] and the process's allowed core set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderCorePlan {
    /// Pin every reader thread to this core.
    Pin(usize),
    /// The operator set `off`.
    Off,
    /// No variable, and [`DEFAULT_WS_READER_CORE`] is not in the allowed set
    /// (a one-core container, or a host confined elsewhere).
    DefaultNotAllowed,
    /// The operator named a core the process may not run on.
    NotAllowed(usize),
    /// The operator named core 0.
    RefusedCoreZero,
    /// The value was not `off` and not a core id up to [`MAX_WS_READER_CORE`].
    Invalid,
    /// Not Linux: there is no affinity call here.
    Unsupported,
}

impl ReaderCorePlan {
    /// The core to pin to, if any.
    #[must_use]
    pub const fn core(self) -> Option<usize> {
        match self {
            Self::Pin(core) => Some(core),
            _ => None,
        }
    }

    /// True when the operator asked for something the process could not do,
    /// which is worth a coded warning. `Off`, `Unsupported` and a default that
    /// does not fit the host are deliberate or expected, not faults.
    #[must_use]
    pub const fn is_refusal(self) -> bool {
        matches!(
            self,
            Self::NotAllowed(_) | Self::RefusedCoreZero | Self::Invalid
        )
    }
}

/// Resolves the pin plan from the raw environment value and a predicate
/// answering "may this process run on core N?". Pure, so every branch is
/// testable without a machine.
#[must_use]
pub fn resolve_ws_reader_core(
    raw: Option<&str>,
    core_allowed: impl Fn(usize) -> bool,
) -> ReaderCorePlan {
    let value = raw.map(str::trim).filter(|v| !v.is_empty());
    let Some(value) = value else {
        return if core_allowed(DEFAULT_WS_READER_CORE) {
            ReaderCorePlan::Pin(DEFAULT_WS_READER_CORE)
        } else {
            ReaderCorePlan::DefaultNotAllowed
        };
    };
    if value.eq_ignore_ascii_case("off") {
        return ReaderCorePlan::Off;
    }
    match value.parse::<usize>() {
        Ok(NEVER_PIN_CORE) => ReaderCorePlan::RefusedCoreZero,
        Ok(core) if core <= MAX_WS_READER_CORE => {
            if core_allowed(core) {
                ReaderCorePlan::Pin(core)
            } else {
                ReaderCorePlan::NotAllowed(core)
            }
        }
        _ => ReaderCorePlan::Invalid,
    }
}

/// Resolves the pin plan for THIS process: reads the allowed core set once.
/// Not Linux: always [`ReaderCorePlan::Unsupported`].
#[must_use]
pub fn resolve_ws_reader_core_for_process(raw: Option<&str>) -> ReaderCorePlan {
    #[cfg(target_os = "linux")]
    {
        match affinity::allowed_cpu_set() {
            Some(set) => resolve_ws_reader_core(raw, |core| affinity::cpu_in_set(&set, core)),
            // The kernel would not say: treat every core as not allowed, so
            // nothing is pinned blind.
            None => resolve_ws_reader_core(raw, |_| false),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = raw;
        ReaderCorePlan::Unsupported
    }
}

/// The core the reader threads are pinned to, or -1 when none is.
#[must_use]
pub fn reader_pinned_core() -> i64 {
    PINNED_CORE.load(Ordering::Relaxed)
}

/// How many reader threads failed to pin, and the last OS error number.
#[must_use]
pub fn reader_pin_failures() -> (u64, i32) {
    (
        PIN_FAILURES.load(Ordering::Relaxed),
        PIN_LAST_ERRNO.load(Ordering::Relaxed),
    )
}

/// Pins the calling thread to `core`, recording the outcome. Runs in
/// `on_thread_start`, so it must not log (`main` builds the runtime before a
/// subscriber exists) and must not panic.
fn pin_this_reader_thread(core: usize) {
    #[cfg(target_os = "linux")]
    {
        match affinity::pin_current_thread(core) {
            Ok(()) => PINNED_CORE.store(i64::try_from(core).unwrap_or(-1), Ordering::Relaxed),
            Err(errno) => {
                PIN_FAILURES.fetch_add(1, Ordering::Relaxed);
                PIN_LAST_ERRNO.store(errno, Ordering::Relaxed);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = core;
    }
}

/// The two affinity syscalls, and nothing else that needs `unsafe`.
#[cfg(target_os = "linux")]
mod affinity {
    /// The calling thread's allowed core set (in `main`, before any runtime,
    /// that is the process's — the systemd `AllowedCPUs` cpuset shows here).
    pub(super) fn allowed_cpu_set() -> Option<libc::cpu_set_t> {
        let mut set = empty_set();
        // SAFETY: `set` is a valid, writable `cpu_set_t` and the size passed is its exact size.
        let rc =
            unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) }; // SAFETY: see above
        (rc == 0).then_some(set)
    }

    /// True when `core` is in `set`. Bounds-checked: `CPU_ISSET` indexes the
    /// mask without a check of its own.
    pub(super) fn cpu_in_set(set: &libc::cpu_set_t, core: usize) -> bool {
        if core > super::MAX_WS_READER_CORE {
            return false;
        }
        // SAFETY: `core` is below CPU_SETSIZE (checked above), so the bit index is inside the mask.
        unsafe { libc::CPU_ISSET(core, set) } // SAFETY: see above
    }

    /// Pins the calling thread to `core`. `Err` carries the OS error number.
    pub(super) fn pin_current_thread(core: usize) -> Result<(), i32> {
        if core > super::MAX_WS_READER_CORE {
            return Err(libc::EINVAL);
        }
        let mut set = empty_set();
        // SAFETY: `core` is below CPU_SETSIZE (checked above); `set` is a valid, owned mask.
        unsafe { libc::CPU_SET(core, &mut set) }; // SAFETY: see above
        // SAFETY: `set` is a valid `cpu_set_t` and the size passed is its exact size; pid 0 is this thread.
        let rc =
            unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) }; // SAFETY: see above
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EINVAL))
        }
    }

    fn empty_set() -> libc::cpu_set_t {
        // SAFETY: `cpu_set_t` is a plain array of integers; all-zero is the valid empty set.
        unsafe { std::mem::zeroed() } // SAFETY: see above
    }
}

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

/// Builds (but does not install) a reader runtime with `threads` workers,
/// each pinned to `pin_core` when it is `Some` (also any blocking-pool
/// thread this runtime starts; the read tasks start none).
///
/// Separate from [`install_reader_runtime`] so a test can build one without
/// touching the process-global slot.
///
/// # Errors
/// The OS refused to create a thread.
pub fn build_reader_runtime(
    threads: usize,
    pin_core: Option<usize>,
) -> std::io::Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .worker_threads(threads.max(1))
        .thread_name(WS_READER_THREAD_NAME)
        .enable_all();
    if let Some(core) = pin_core {
        builder.on_thread_start(move || pin_this_reader_thread(core));
    }
    builder.build()
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
/// any socket is dialled, and never from inside a runtime (it runs one
/// empty task to completion so a worker has passed its pin before the boot
/// log reads [`reader_pinned_core`]).
///
/// # Errors
/// The OS refused to create a thread. The caller logs it and boots on the
/// shared runtime — a slower reader is better than no feed.
pub fn install_reader_runtime(
    threads: usize,
    pin_core: Option<usize>,
) -> std::io::Result<ReaderRuntimeInstall> {
    if threads == 0 {
        return Ok(ReaderRuntimeInstall::Disabled);
    }
    if READER_RUNTIME.get().is_some() {
        return Ok(ReaderRuntimeInstall::AlreadyInstalled);
    }
    let runtime = build_reader_runtime(threads, pin_core)?;
    // APPROVED-BLOCKING: runs once in `main`, before any runtime or socket exists, and waits only for one empty task on a fresh worker.
    let _warmed = runtime.block_on(runtime.spawn(async {}));
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
            install_reader_runtime(0, None).expect("zero never touches the OS"),
            ReaderRuntimeInstall::Disabled
        );
    }

    #[test]
    fn build_reader_runtime_names_its_threads_and_runs_tasks() {
        let rt = build_reader_runtime(1, None).expect("one thread");
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

    #[test]
    fn test_resolve_ws_reader_core_default_off_zero_invalid_and_not_allowed() {
        let all = |_core: usize| true;
        let none = |_core: usize| false;
        assert_eq!(
            resolve_ws_reader_core(None, all),
            ReaderCorePlan::Pin(DEFAULT_WS_READER_CORE)
        );
        assert_eq!(
            resolve_ws_reader_core(Some("  "), all),
            ReaderCorePlan::Pin(DEFAULT_WS_READER_CORE)
        );
        assert_eq!(
            resolve_ws_reader_core(None, none),
            ReaderCorePlan::DefaultNotAllowed
        );
        for off in ["off", "OFF", " Off "] {
            assert_eq!(resolve_ws_reader_core(Some(off), all), ReaderCorePlan::Off);
        }
        // Core 0 is refused even when the process may run there.
        assert_eq!(
            resolve_ws_reader_core(Some("0"), all),
            ReaderCorePlan::RefusedCoreZero
        );
        assert_eq!(
            resolve_ws_reader_core(Some("2"), all),
            ReaderCorePlan::Pin(2)
        );
        assert_eq!(
            resolve_ws_reader_core(Some("2"), |c| c == 1),
            ReaderCorePlan::NotAllowed(2)
        );
        for bad in ["-1", "abc", "1.5", "1024", "99999999999999999999"] {
            assert_eq!(
                resolve_ws_reader_core(Some(bad), all),
                ReaderCorePlan::Invalid,
                "input {bad:?}"
            );
        }
        assert_eq!(
            resolve_ws_reader_core(Some("1023"), all),
            ReaderCorePlan::Pin(MAX_WS_READER_CORE)
        );
    }

    #[test]
    fn test_reader_core_plan_core_and_is_refusal() {
        assert_eq!(ReaderCorePlan::Pin(3).core(), Some(3));
        for plan in [
            ReaderCorePlan::Off,
            ReaderCorePlan::DefaultNotAllowed,
            ReaderCorePlan::NotAllowed(5),
            ReaderCorePlan::RefusedCoreZero,
            ReaderCorePlan::Invalid,
            ReaderCorePlan::Unsupported,
        ] {
            assert_eq!(plan.core(), None, "{plan:?}");
        }
        assert!(ReaderCorePlan::NotAllowed(5).is_refusal());
        assert!(ReaderCorePlan::RefusedCoreZero.is_refusal());
        assert!(ReaderCorePlan::Invalid.is_refusal());
        assert!(!ReaderCorePlan::Pin(1).is_refusal());
        assert!(!ReaderCorePlan::Off.is_refusal());
        assert!(!ReaderCorePlan::DefaultNotAllowed.is_refusal());
        assert!(!ReaderCorePlan::Unsupported.is_refusal());
    }

    #[test]
    fn test_resolve_ws_reader_core_for_process_never_pins_core_zero() {
        for raw in [None, Some("0"), Some("off"), Some("1"), Some("2")] {
            let plan = resolve_ws_reader_core_for_process(raw);
            assert_ne!(plan.core(), Some(NEVER_PIN_CORE), "input {raw:?}");
            #[cfg(not(target_os = "linux"))]
            assert_eq!(plan, ReaderCorePlan::Unsupported);
        }
        assert_eq!(resolve_ws_reader_core_for_process(Some("off")).core(), None);
    }

    /// Builds a pinned runtime on a core this test process may use (other
    /// than 0) and checks, from inside a reader thread, that the kernel now
    /// confines that thread to exactly that core.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_build_reader_runtime_pins_threads_and_reader_pinned_core_reports_it() {
        let Some(set) = affinity::allowed_cpu_set() else {
            return;
        };
        let Some(core) = (1..=MAX_WS_READER_CORE).find(|c| affinity::cpu_in_set(&set, *c)) else {
            return; // a one-core box: nothing to pin to but core 0
        };
        let rt = build_reader_runtime(1, Some(core)).expect("one thread");
        let only_that_core = rt.block_on(async move {
            tokio::spawn(async move {
                let mine = affinity::allowed_cpu_set().expect("own mask");
                (0..=MAX_WS_READER_CORE)
                    .filter(|c| affinity::cpu_in_set(&mine, *c))
                    .eq(std::iter::once(core))
            })
            .await
            .expect("task ran")
        });
        assert!(only_that_core, "reader thread must run on core {core} only");
        assert_eq!(reader_pinned_core(), i64::try_from(core).expect("small"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_reader_pin_failures_counts_a_refused_pin_without_panicking() {
        let Some(set) = affinity::allowed_cpu_set() else {
            return;
        };
        let Some(outside) = (1..=MAX_WS_READER_CORE).find(|c| !affinity::cpu_in_set(&set, *c))
        else {
            return;
        };
        let (before, _) = reader_pin_failures();
        std::thread::spawn(move || pin_this_reader_thread(outside))
            .join()
            .expect("pin never panics");
        let (after, errno) = reader_pin_failures();
        assert_eq!(after - before, 1);
        assert_eq!(errno, libc::EINVAL);
    }
}
