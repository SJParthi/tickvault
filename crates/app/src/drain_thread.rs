//! Where the frame drain runs (2026-10-30, plan ITEM DT, owner order of
//! 30 Oct 2026: "always less than one microsecond").
//!
//! # Default: unchanged
//!
//! With [`DRAIN_THREAD_ENV`] absent, empty or `off`, [`spawn_frame_drain`]
//! is `tokio::spawn`, exactly as before: the drain shares the main runtime's
//! workers with every background task.
//!
//! # `on`: its own OS thread
//!
//! The drain gets an OS thread named [`DRAIN_THREAD_NAME`] that nothing
//! else runs on, with a current-thread tokio runtime of its own, pinned to
//! the readers' core (or the core named in the variable). The drain's work
//! is unchanged: the same `run_frame_drain` future, polled by a different
//! executor. Everything it awaits (the frame ring's `Notify`, tokio timers,
//! the shutdown `Notify`, the seed channel, `consume_budget`) works across
//! runtimes, and the drain spawns no task of its own. Its blocking steps
//! (`block_in_place` in `blocking_flush` / `off_worker`) see a current-thread
//! runtime and run inline, which blocks only the drain thread: nothing else
//! is on it to wait.
//!
//! With a drain priority recorded by [`set_drain_placement`] (the real-time
//! switch, refused unless the kernel's real-time throttle is active) the thread also
//! takes `SCHED_FIFO` at that priority.
//!
//! # Failure handling
//!
//! The thread and its runtime are built BEFORE the future is handed over,
//! so a refused thread or runtime falls back to `tokio::spawn` with the
//! future intact, logged once with a coded line. A refused pin or priority
//! keeps the thread, unpinned or at normal priority, also logged.
//!
//! # Complexity
//!
//! [`spawn_frame_drain`] runs once per lane start: one thread, one runtime,
//! two syscalls. Nothing here runs per frame.

use std::sync::OnceLock;

use tickvault_common::error_code::ErrorCode;
use tickvault_core::websocket::reader_runtime;
use tracing::{info, warn};

/// Environment variable: `off` (default), `on` (pin to the readers' core),
/// or a core id other than 0.
pub const DRAIN_THREAD_ENV: &str = "TICKVAULT_DRAIN_THREAD";

/// The drain thread's name, so `top -H` / `perf` attribute it.
pub const DRAIN_THREAD_NAME: &str = "tv-feed-drain";

/// Stack for the drain thread. The drain's future is large (one select loop
/// over many arms); 8 MiB is the main thread's default, well above it.
const DRAIN_THREAD_STACK_BYTES: usize = 8 * 1024 * 1024;

/// What the operator asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainThreadPlan {
    /// The default: `tokio::spawn` on the main runtime.
    Shared,
    /// Own thread. `None`: the readers' pinned core, or unpinned when they
    /// are not pinned. `Some(core)`: that core.
    Own(Option<usize>),
    /// Core 0 (it services network interrupts) or not `off`/`on`/a core id.
    /// Boot runs the drain shared and logs a coded line.
    Invalid,
}

/// Resolves [`DRAIN_THREAD_ENV`]. Pure.
#[must_use]
pub fn resolve_drain_thread(raw: Option<&str>) -> DrainThreadPlan {
    let Some(value) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return DrainThreadPlan::Shared;
    };
    if value.eq_ignore_ascii_case("off") {
        return DrainThreadPlan::Shared;
    }
    if value.eq_ignore_ascii_case("on") {
        return DrainThreadPlan::Own(None);
    }
    match value.parse::<usize>() {
        Ok(reader_runtime::NEVER_PIN_CORE) => DrainThreadPlan::Invalid,
        Ok(core) if core <= reader_runtime::MAX_WS_READER_CORE => DrainThreadPlan::Own(Some(core)),
        _ => DrainThreadPlan::Invalid,
    }
}

/// The core an [`DrainThreadPlan::Own`] drain pins to, given where the
/// readers were pinned (`-1` = not pinned). Pure.
#[must_use]
pub fn drain_core(plan: DrainThreadPlan, reader_pinned_core: i64) -> Option<usize> {
    match plan {
        DrainThreadPlan::Own(Some(core)) => Some(core),
        DrainThreadPlan::Own(None) => usize::try_from(reader_pinned_core).ok(),
        DrainThreadPlan::Shared | DrainThreadPlan::Invalid => None,
    }
}

/// Placement decided in `main`: the plan and the drain's `SCHED_FIFO`
/// priority (`None` = normal). Unset (every test) = [`DrainThreadPlan::Shared`].
static PLACEMENT: OnceLock<(DrainThreadPlan, Option<u8>)> = OnceLock::new();

/// Records the drain placement. Called once from `main`, before any lane.
// TEST-EXEMPT: one OnceLock set at boot; a test setting it would move every other drain test off the shared runtime. The value it stores is resolve_drain_thread, tested below
pub fn set_drain_placement(plan: DrainThreadPlan, rt_priority: Option<u8>) {
    let _first = PLACEMENT.set((plan, rt_priority));
}

/// The recorded placement, or the default.
#[must_use]
pub fn drain_placement() -> (DrainThreadPlan, Option<u8>) {
    PLACEMENT
        .get()
        .copied()
        .unwrap_or((DrainThreadPlan::Shared, None))
}

/// Why the drain's join returned no value.
#[derive(Debug)]
pub enum DrainJoinError {
    /// The shared-runtime task was cancelled or panicked.
    Task(tokio::task::JoinError),
    /// The drain thread ended without sending a result (it panicked; in a
    /// release build `panic = "abort"` ends the process instead).
    ThreadEnded,
}

impl std::fmt::Display for DrainJoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Task(err) => write!(f, "{err}"),
            Self::ThreadEnded => f.write_str("the drain thread ended without a result"),
        }
    }
}

/// The running drain, wherever it runs.
pub enum DrainJoin<T> {
    /// On the main runtime.
    Shared(tokio::task::JoinHandle<T>),
    /// On its own thread; the result arrives on this channel.
    Own(tokio::sync::oneshot::Receiver<T>),
}

impl<T> DrainJoin<T> {
    /// Waits for the drain to end.
    ///
    /// # Errors
    /// [`DrainJoinError`] when it ended without a value.
    pub async fn join(self) -> Result<T, DrainJoinError> {
        match self {
            Self::Shared(handle) => handle.await.map_err(DrainJoinError::Task),
            Self::Own(rx) => rx.await.map_err(|_closed| DrainJoinError::ThreadEnded),
        }
    }

    /// True when the drain runs on its own thread.
    #[must_use]
    pub const fn is_own_thread(&self) -> bool {
        matches!(self, Self::Own(_))
    }
}

/// Spawns the frame drain at the placement `main` recorded.
pub fn spawn_frame_drain<F>(future: F) -> DrainJoin<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (plan, rt_priority) = drain_placement();
    if plan == DrainThreadPlan::Invalid {
        warn!(
            code = ErrorCode::HotPath03ReaderPinNotApplied.code_str(),
            source = "drain_thread_invalid",
            "the frame drain runs on the shared runtime — TICKVAULT_DRAIN_THREAD is not \
             off, on, or a core id other than 0"
        );
    }
    spawn_frame_drain_with(
        future,
        plan,
        drain_core(plan, reader_runtime::reader_pinned_core()),
        rt_priority,
    )
}

/// [`spawn_frame_drain`] with the placement given, for tests.
pub fn spawn_frame_drain_with<F>(
    future: F,
    plan: DrainThreadPlan,
    core: Option<usize>,
    rt_priority: Option<u8>,
) -> DrainJoin<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    if !matches!(plan, DrainThreadPlan::Own(_)) {
        return DrainJoin::Shared(tokio::spawn(future));
    }
    // Built here, before the thread exists, so a failure keeps the future.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            warn!(
                code = ErrorCode::HotPath03ReaderPinNotApplied.code_str(),
                source = "drain_thread_runtime_refused",
                %err,
                "the frame drain runs on the shared runtime — its own runtime could not be built"
            );
            return DrainJoin::Shared(tokio::spawn(future));
        }
    };
    let (future_tx, future_rx) = std::sync::mpsc::sync_channel::<F>(1);
    let (result_tx, result_rx) = tokio::sync::oneshot::channel::<F::Output>();
    let spawned = std::thread::Builder::new()
        // APPROVED: one name String per lane start (cold), never per frame
        .name(String::from(DRAIN_THREAD_NAME))
        .stack_size(DRAIN_THREAD_STACK_BYTES)
        .spawn(move || {
            let Ok(future) = future_rx.recv() else {
                return;
            };
            place_this_thread(core, rt_priority);
            let output = runtime.block_on(future);
            let _receiver_gone_is_fine = result_tx.send(output);
            // The runtime is leaked like the main one (see main.rs,
            // RUNTIME-NOT-DROPPED-AT-EXIT): dropping it can wait on a timer
            // a blocking step still holds.
            std::mem::forget(runtime);
        });
    match spawned {
        Ok(_detached) => match future_tx.send(future) {
            Ok(()) => DrainJoin::Own(result_rx),
            // The thread is gone already (unreachable: it waits on recv).
            Err(std::sync::mpsc::SendError(future)) => DrainJoin::Shared(tokio::spawn(future)),
        },
        Err(err) => {
            warn!(
                code = ErrorCode::HotPath03ReaderPinNotApplied.code_str(),
                source = "drain_thread_spawn_refused",
                %err,
                "the frame drain runs on the shared runtime — its own thread could not be created"
            );
            // The closure held only the receiving end; the future never left.
            DrainJoin::Shared(tokio::spawn(future))
        }
    }
}

/// Pins the drain thread and sets its priority, logging the outcome once.
fn place_this_thread(core: Option<usize>, rt_priority: Option<u8>) {
    let pinned = core.map(reader_runtime::pin_current_thread_to_core);
    let fifo = rt_priority.map(reader_runtime::set_current_thread_fifo);
    match (pinned, fifo) {
        (Some(Err(errno)), _) | (_, Some(Err(errno))) => warn!(
            code = ErrorCode::HotPath03ReaderPinNotApplied.code_str(),
            source = "drain_thread_placement_refused",
            core = ?core,
            rt_priority = ?rt_priority,
            pin_ok = matches!(pinned, Some(Ok(()))),
            fifo_ok = matches!(fifo, Some(Ok(()))),
            os_errno = errno,
            "the frame drain runs on its own thread, but its core pin or real-time priority \
             was refused"
        ),
        _ => info!(
            core = ?core,
            rt_priority = ?rt_priority,
            "the frame drain runs on its own thread"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_drain_thread_is_shared_unless_asked() {
        for raw in [None, Some(""), Some(" "), Some("off"), Some("OFF")] {
            assert_eq!(
                resolve_drain_thread(raw),
                DrainThreadPlan::Shared,
                "{raw:?}"
            );
        }
    }

    #[test]
    fn test_resolve_drain_thread_on_or_a_core() {
        assert_eq!(resolve_drain_thread(Some("on")), DrainThreadPlan::Own(None));
        assert_eq!(resolve_drain_thread(Some("ON")), DrainThreadPlan::Own(None));
        assert_eq!(
            resolve_drain_thread(Some("1")),
            DrainThreadPlan::Own(Some(1))
        );
        for bad in ["0", "x", "-1", "1.5", "99999"] {
            assert_eq!(
                resolve_drain_thread(Some(bad)),
                DrainThreadPlan::Invalid,
                "{bad}"
            );
        }
    }

    #[test]
    fn test_drain_core_follows_the_readers_or_the_named_core() {
        assert_eq!(drain_core(DrainThreadPlan::Own(None), 1), Some(1));
        assert_eq!(drain_core(DrainThreadPlan::Own(None), -1), None);
        assert_eq!(drain_core(DrainThreadPlan::Own(Some(3)), 1), Some(3));
        assert_eq!(drain_core(DrainThreadPlan::Shared, 1), None);
        assert_eq!(drain_core(DrainThreadPlan::Invalid, 1), None);
    }

    #[test]
    fn test_drain_placement_defaults_to_shared_until_set_drain_placement_runs() {
        // Tests never call `set_drain_placement`.
        assert_eq!(drain_placement().0, DrainThreadPlan::Shared);
    }

    #[test]
    fn join_error_display_names_the_cause() {
        assert_eq!(
            DrainJoinError::ThreadEnded.to_string(),
            "the drain thread ended without a result"
        );
    }

    /// A drain-shaped future: timers, a `Notify` from another runtime, a
    /// bounded channel, and a value out. Same output on both placements.
    async fn drain_like(
        mut rx: tokio::sync::mpsc::Receiver<u64>,
        stop: std::sync::Arc<tokio::sync::Notify>,
    ) -> (u64, u64, bool) {
        let mut sum = 0_u64;
        let mut count = 0_u64;
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(1));
        loop {
            tokio::select! {
                biased;
                () = stop.notified() => return (sum, count, false),
                v = rx.recv() => match v {
                    Some(v) => { sum = sum.wrapping_add(v); count += 1; }
                    None => return (sum, count, true),
                },
                _ = tick.tick() => {}
            }
        }
    }

    async fn run_on(plan: DrainThreadPlan) -> ((u64, u64, bool), bool) {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let stop = std::sync::Arc::new(tokio::sync::Notify::new());
        let join = spawn_frame_drain_with(
            drain_like(rx, std::sync::Arc::clone(&stop)),
            plan,
            None,
            None,
        );
        let own = join.is_own_thread();
        for v in 0..10_000_u64 {
            tx.send(v * 3).await.unwrap();
        }
        drop(tx);
        (join.join().await.unwrap(), own)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_spawn_frame_drain_with_own_thread_gives_the_same_output_as_shared() {
        let (shared, shared_own) = run_on(DrainThreadPlan::Shared).await;
        let (own, own_own) = run_on(DrainThreadPlan::Own(None)).await;
        assert!(!shared_own);
        assert!(own_own);
        assert_eq!(shared, own);
        assert_eq!(own, (3 * (9_999 * 10_000 / 2), 10_000, true));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_spawn_frame_drain_with_own_thread_runs_on_a_named_thread_and_sees_a_shutdown_notify()
     {
        let stop = std::sync::Arc::new(tokio::sync::Notify::new());
        let s2 = std::sync::Arc::clone(&stop);
        let join = spawn_frame_drain_with(
            async move {
                let name = std::thread::current().name().map(str::to_owned);
                s2.notified().await;
                name
            },
            DrainThreadPlan::Own(None),
            None,
            None,
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        stop.notify_one();
        assert_eq!(
            join.join().await.unwrap().as_deref(),
            Some(DRAIN_THREAD_NAME)
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "current_thread")]
    async fn own_thread_pins_to_the_requested_core_when_allowed() {
        let core = 0_usize;
        if !reader_runtime::current_thread_allows_core(core) {
            return;
        }
        let join = spawn_frame_drain_with(
            async move { reader_runtime::current_thread_allows_core(core) },
            DrainThreadPlan::Own(Some(core)),
            Some(core),
            None,
        );
        assert!(join.join().await.unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_dropped_receiver_does_not_hang_the_thread() {
        let join = spawn_frame_drain_with(async { 7_u8 }, DrainThreadPlan::Own(None), None, None);
        drop(join);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    #[test]
    fn the_drain_spawn_site_uses_this_module() {
        let src = include_str!("dhan_feed_stack.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        assert!(
            prod.contains("crate::drain_thread::spawn_frame_drain(run_frame_drain("),
            "the live drain must be spawned through drain_thread so the switch reaches it"
        );
        assert!(prod.contains("drain.join().await"));
    }
}
