//! Hot-thread placement at boot (2026-10-30, plan ITEM DT). Called from
//! `main` before any runtime is built; every switch is off by default.
//!
//! Order matters and is the caller's: [`decide_rt_from_env`] runs before the
//! reader runtime is installed (its readers take the priority at start);
//! [`place_drain_and_background`] runs after it and BEFORE the main runtime
//! is built, because pinning the main thread is what puts the main runtime's
//! workers, and every thread they spawn, on the background core. The pin
//! plan is resolved against the process's allowed set before the main
//! thread is pinned (pinning first would hide core 1 from the check).
//!
//! [`report_hot_thread_placement`] logs the outcome once `async_main` has a
//! subscriber. A requested placement that was not applied is one coded
//! `warn!` (HOT-PATH-03, log-sink-only, runbook
//! `docs/error-runbooks/wave-1-error-codes.md`); boot never fails over it.
//!
//! Cold path: each function runs once per process.

use std::sync::OnceLock;

use tickvault_common::error_code::ErrorCode;
use tickvault_core::websocket::hot_thread_placement::{self, BackgroundCorePlan, HotRtDecision};
use tickvault_core::websocket::reader_runtime;
use tracing::{info, warn};

use crate::drain_thread;

/// The real-time priority decision, for the boot log.
static HOT_RT_DECISION: OnceLock<HotRtDecision> = OnceLock::new();
/// The background-core plan and the main thread's pin result.
static BACKGROUND_CORE_OUTCOME: OnceLock<(BackgroundCorePlan, Option<Result<(), i32>>)> =
    OnceLock::new();

/// Decides real-time priority from [`hot_thread_placement::HOT_RT_PRIORITY_ENV`]
/// and the kernel's RT throttle, and records it for the boot log.
// TEST-EXEMPT: one env read and one OnceLock set at boot; the decision is decide_hot_rt, tested in tickvault-core
pub fn decide_rt_from_env() -> HotRtDecision {
    let raw = std::env::var(hot_thread_placement::HOT_RT_PRIORITY_ENV).ok();
    let decision = hot_thread_placement::decide_hot_rt(
        hot_thread_placement::resolve_hot_rt_priority(raw.as_deref()),
        hot_thread_placement::read_rt_throttle,
    );
    let _first = HOT_RT_DECISION.set(decision);
    decision
}

/// Records the frame drain's placement and, when
/// [`hot_thread_placement::BACKGROUND_CORE_ENV`] asks, pins the CALLING
/// thread (main) to the background core. Call after the reader runtime is
/// installed and before the main runtime is built.
// TEST-EXEMPT: env reads and one syscall on the calling thread; the decisions are resolve_drain_thread / resolve_background_core, tested in their modules
pub fn place_drain_and_background(rt: HotRtDecision) {
    let drain_raw = std::env::var(drain_thread::DRAIN_THREAD_ENV).ok();
    drain_thread::set_drain_placement(
        drain_thread::resolve_drain_thread(drain_raw.as_deref()),
        rt.drain(),
    );
    let background_raw = std::env::var(hot_thread_placement::BACKGROUND_CORE_ENV).ok();
    let plan = hot_thread_placement::resolve_background_core(
        background_raw.as_deref(),
        usize::try_from(reader_runtime::reader_pinned_core()).ok(),
        reader_runtime::current_thread_allows_core,
    );
    let pin = match plan {
        BackgroundCorePlan::Pin(core) => Some(reader_runtime::pin_current_thread_to_core(core)),
        _ => None,
    };
    let _first = BACKGROUND_CORE_OUTCOME.set((plan, pin));
}

/// Logs the hot-thread placement once: real-time priority, background core,
/// drain placement.
// TEST-EXEMPT: logging only; every input it reads is decided and tested elsewhere
pub fn report_hot_thread_placement() {
    let code = ErrorCode::HotPath03ReaderPinNotApplied.code_str();
    let decision = HOT_RT_DECISION.get().copied().unwrap_or(HotRtDecision::Off);
    let (rt_applied, rt_failed, rt_errno) = reader_runtime::reader_rt_outcome();
    if decision.is_refusal() {
        warn!(
            code,
            source = "hot_rt_refused",
            decision = ?decision,
            "socket readers and the frame drain run at NORMAL priority — real-time priority \
             was asked for and refused (the kernel's real-time throttle is off or unreadable, \
             or the value is not 1 to 49)"
        );
    } else if decision.reader().is_some() && rt_failed > 0 {
        warn!(
            code,
            source = "hot_rt_reader_failed",
            applied_threads = rt_applied,
            failed_threads = rt_failed,
            os_errno = rt_errno,
            "socket reader threads run at NORMAL priority — the kernel refused real-time \
             priority (os_errno 1 = EPERM: the unit needs LimitRTPRIO; 38 = ENOSYS: the kernel \
             lacks the call)"
        );
    } else if let HotRtDecision::Apply { reader, drain } = decision {
        info!(
            reader,
            drain,
            applied_threads = rt_applied,
            "socket readers and the frame drain take real-time priority"
        );
    }
    match BACKGROUND_CORE_OUTCOME.get().copied() {
        Some((BackgroundCorePlan::Pin(core), Some(Ok(())))) => {
            info!(
                core,
                "background threads (runtime workers, writers) pinned to one core"
            );
        }
        Some((plan @ BackgroundCorePlan::Pin(_), Some(Err(errno)))) => warn!(
            code,
            source = "background_core_pin_failed",
            plan = ?plan,
            os_errno = errno,
            "background threads run UNPINNED — the kernel refused the core"
        ),
        Some((plan, _)) if plan.is_refusal() => warn!(
            code,
            source = "background_core_refused",
            plan = ?plan,
            "background threads run UNPINNED — TICKVAULT_BACKGROUND_CORE named the readers' \
             core, a core this process may not use, or is not a core id; or the readers are not \
             pinned to a core of their own, so pinning would move them onto the background core"
        ),
        _ => {}
    }
    let (drain_plan, drain_rt) = drain_thread::drain_placement();
    info!(plan = ?drain_plan, rt_priority = ?drain_rt, "frame drain placement");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_switch_set() -> bool {
        [
            hot_thread_placement::HOT_RT_PRIORITY_ENV,
            hot_thread_placement::BACKGROUND_CORE_ENV,
            drain_thread::DRAIN_THREAD_ENV,
        ]
        .iter()
        .all(|k| std::env::var_os(k).is_none())
    }

    #[test]
    fn test_decide_rt_from_env_place_drain_and_background_report_hot_thread_placement_default_off()
    {
        if !no_switch_set() {
            return; // a developer shell set a switch; the default is what is pinned here
        }
        // A thread of its own, so a pin (none expected) could never touch
        // the test harness's thread.
        let (decision, placement) = std::thread::spawn(|| {
            let decision = decide_rt_from_env();
            place_drain_and_background(decision);
            report_hot_thread_placement();
            (decision, drain_thread::drain_placement())
        })
        .join()
        .expect("boot placement thread");
        assert_eq!(
            decision,
            HotRtDecision::Off,
            "no switch: no real-time priority"
        );
        assert_eq!(
            placement,
            (drain_thread::DrainThreadPlan::Shared, None),
            "no switch: the drain stays on the shared runtime"
        );
        assert!(matches!(
            BACKGROUND_CORE_OUTCOME.get(),
            Some((BackgroundCorePlan::Off, None))
        ));
    }
}
