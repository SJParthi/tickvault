//! Where the two hot threads run, and at what priority (2026-10-30, plan
//! ITEM DT, owner order of 30 Oct 2026).
//!
//! # The problem
//!
//! The deployed box is an r8g.large: two cores. About 50 ticks a session
//! took 2 to 16 ms from socket to fold while our own code measured 0.03 ms
//! at p99 (29 Oct): the socket reader and the frame drain were waiting for a
//! CPU that QuestDB, the WAL writer or a background task held. The kernel
//! receive queue never passed 1.5 KB, so nothing was lost; the time went to
//! scheduling.
//!
//! # The three switches, all off by default
//!
//! | Switch | Env | Effect when on |
//! |---|---|---|
//! | Drain thread | `TICKVAULT_DRAIN_THREAD` | the frame drain runs on its own OS thread, pinned next to the readers (app crate, `drain_thread`) |
//! | Real-time priority | [`HOT_RT_PRIORITY_ENV`] | readers take `SCHED_FIFO` priority + 1, the drain takes priority; refused unless the kernel's RT throttle is active |
//! | Background core | [`BACKGROUND_CORE_ENV`] | the main thread is pinned to that core before the main runtime is built, so every tokio worker and every thread they spawn (WAL writer, seal writer, telemetry) inherits it |
//!
//! The QuestDB container's half is `TV_QDB_CORE_ISOLATION=1` in the compose
//! `.env`, applied by `tickvault host-tuning apply`.
//!
//! # Why the throttle gate
//!
//! A `SCHED_FIFO` thread that spins never yields to a normal thread on its
//! core. `kernel.sched_rt_runtime_us` (default 950,000 of a 1,000,000 µs
//! period) caps real-time threads at 95% of each period, so a stuck reader
//! or drain still leaves 50 ms a second for everything else, and the box can
//! be reached. With the throttle off (`-1`), or equal to the period, nothing
//! caps it: we refuse and log a coded line instead.
//!
//! # Why the reader is one above the drain
//!
//! Two `SCHED_FIFO` threads of the same priority on one core do not preempt
//! each other: the one running keeps the core until it blocks. A drain
//! working through a backlog would then hold the reader off the socket, the
//! exact wait this removes. One step higher lets a socket read preempt the
//! drain; the reader's work per frame is microseconds and it blocks again.
//!
//! # Complexity
//!
//! Every function here is O(1), runs once at boot, and allocates nothing.

/// Environment variable: `off` (default) or a `SCHED_FIFO` priority in
/// `1..=`[`MAX_HOT_RT_PRIORITY`] for the drain; readers take one more.
pub const HOT_RT_PRIORITY_ENV: &str = "TICKVAULT_HOT_RT_PRIORITY";

/// Highest accepted drain priority. Kept low on purpose: the readers take
/// one more, and the kernel's own threads (migration, watchdog) sit at 99.
pub const MAX_HOT_RT_PRIORITY: u8 = 49;

/// Environment variable: `off` (default) or the core id every non-hot
/// thread is pinned to.
pub const BACKGROUND_CORE_ENV: &str = "TICKVAULT_BACKGROUND_CORE";

/// `kernel.sched_rt_runtime_us`.
pub const RT_RUNTIME_PATH: &str = "/proc/sys/kernel/sched_rt_runtime_us";
/// `kernel.sched_rt_period_us`.
pub const RT_PERIOD_PATH: &str = "/proc/sys/kernel/sched_rt_period_us";

/// What the operator asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotRtPlan {
    /// No real-time priority (the default).
    Off,
    /// The drain at this `SCHED_FIFO` priority, the readers one higher.
    Fifo(u8),
    /// Not `off` and not a priority in range.
    Invalid,
}

/// Resolves [`HOT_RT_PRIORITY_ENV`]. Absent, empty or `off`: [`HotRtPlan::Off`].
#[must_use]
pub fn resolve_hot_rt_priority(raw: Option<&str>) -> HotRtPlan {
    let Some(value) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return HotRtPlan::Off;
    };
    if value.eq_ignore_ascii_case("off") {
        return HotRtPlan::Off;
    }
    match value.parse::<u8>() {
        Ok(p @ 1..=MAX_HOT_RT_PRIORITY) => HotRtPlan::Fifo(p),
        _ => HotRtPlan::Invalid,
    }
}

/// The kernel's real-time throttle, read from `/proc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtThrottle {
    /// Real-time threads may use at most `runtime_us` of each `period_us`.
    Active {
        /// `sched_rt_runtime_us`.
        runtime_us: u64,
        /// `sched_rt_period_us`.
        period_us: u64,
    },
    /// `-1`, or a runtime at or above the period: nothing caps them.
    Unlimited,
    /// A file was missing or did not parse.
    Unreadable,
}

/// Classifies the two `/proc` values. Pure.
#[must_use]
pub fn classify_rt_throttle(runtime_raw: Option<&str>, period_raw: Option<&str>) -> RtThrottle {
    let (Some(runtime), Some(period)) = (runtime_raw.map(str::trim), period_raw.map(str::trim))
    else {
        return RtThrottle::Unreadable;
    };
    let Ok(period_us) = period.parse::<u64>() else {
        return RtThrottle::Unreadable;
    };
    match runtime.parse::<i64>() {
        Ok(-1) => RtThrottle::Unlimited,
        Ok(r) if r >= 0 => {
            let runtime_us = r.unsigned_abs();
            if period_us == 0 || runtime_us >= period_us {
                RtThrottle::Unlimited
            } else {
                RtThrottle::Active {
                    runtime_us,
                    period_us,
                }
            }
        }
        _ => RtThrottle::Unreadable,
    }
}

/// Reads the throttle from this host's `/proc` (once, at boot).
#[must_use]
pub fn read_rt_throttle() -> RtThrottle {
    // O(1) EXEMPT: begin — two small /proc reads, once in `main` before any runtime exists
    let runtime = std::fs::read_to_string(RT_RUNTIME_PATH).ok();
    let period = std::fs::read_to_string(RT_PERIOD_PATH).ok();
    // O(1) EXEMPT: end
    classify_rt_throttle(runtime.as_deref(), period.as_deref())
}

/// What boot does about real-time priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotRtDecision {
    /// Not asked for.
    Off,
    /// Apply: readers at `reader`, the drain at `drain` (`reader = drain + 1`).
    Apply {
        /// Reader worker priority.
        reader: u8,
        /// Drain thread priority.
        drain: u8,
    },
    /// Asked for, refused: the kernel throttle is off, so a stuck thread
    /// could hold its core forever.
    RefusedThrottleUnlimited,
    /// Asked for, refused: the throttle could not be read.
    RefusedThrottleUnreadable,
    /// The value was not `off` or a priority in range.
    RefusedInvalid,
}

impl HotRtDecision {
    /// Reader priority, if any.
    #[must_use]
    pub const fn reader(self) -> Option<u8> {
        match self {
            Self::Apply { reader, .. } => Some(reader),
            _ => None,
        }
    }

    /// Drain priority, if any.
    #[must_use]
    pub const fn drain(self) -> Option<u8> {
        match self {
            Self::Apply { drain, .. } => Some(drain),
            _ => None,
        }
    }

    /// True when the operator asked and boot said no (a coded line).
    #[must_use]
    pub const fn is_refusal(self) -> bool {
        matches!(
            self,
            Self::RefusedThrottleUnlimited | Self::RefusedThrottleUnreadable | Self::RefusedInvalid
        )
    }
}

/// Decides from the plan and the throttle. Pure. The throttle is consulted
/// only when the plan asks for priority.
#[must_use]
pub fn decide_hot_rt(plan: HotRtPlan, throttle: impl FnOnce() -> RtThrottle) -> HotRtDecision {
    match plan {
        HotRtPlan::Off => HotRtDecision::Off,
        HotRtPlan::Invalid => HotRtDecision::RefusedInvalid,
        HotRtPlan::Fifo(drain) => match throttle() {
            RtThrottle::Active { .. } => HotRtDecision::Apply {
                reader: drain.saturating_add(1),
                drain,
            },
            RtThrottle::Unlimited => HotRtDecision::RefusedThrottleUnlimited,
            RtThrottle::Unreadable => HotRtDecision::RefusedThrottleUnreadable,
        },
    }
}

/// What boot does about the background core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundCorePlan {
    /// Not asked for (the default): nothing is pinned.
    Off,
    /// Pin the main thread (and so every thread it or the main runtime
    /// spawns) to this core.
    Pin(usize),
    /// Asked for a core this process may not run on.
    NotAllowed(usize),
    /// Asked for the readers' own core, which would put the background work
    /// back beside them.
    SameAsReaders(usize),
    /// Not `off` and not a core id.
    Invalid,
}

impl BackgroundCorePlan {
    /// True when the operator asked and boot said no.
    #[must_use]
    pub const fn is_refusal(self) -> bool {
        matches!(
            self,
            Self::NotAllowed(_) | Self::SameAsReaders(_) | Self::Invalid
        )
    }
}

/// Resolves [`BACKGROUND_CORE_ENV`]. `reader_core` is the core the readers
/// are pinned to, if any. Pure.
#[must_use]
pub fn resolve_background_core(
    raw: Option<&str>,
    reader_core: Option<usize>,
    core_allowed: impl Fn(usize) -> bool,
) -> BackgroundCorePlan {
    let Some(value) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return BackgroundCorePlan::Off;
    };
    if value.eq_ignore_ascii_case("off") {
        return BackgroundCorePlan::Off;
    }
    match value.parse::<usize>() {
        Ok(core) if core <= super::reader_runtime::MAX_WS_READER_CORE => {
            if reader_core == Some(core) {
                BackgroundCorePlan::SameAsReaders(core)
            } else if core_allowed(core) {
                BackgroundCorePlan::Pin(core)
            } else {
                BackgroundCorePlan::NotAllowed(core)
            }
        }
        _ => BackgroundCorePlan::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTIVE: RtThrottle = RtThrottle::Active {
        runtime_us: 950_000,
        period_us: 1_000_000,
    };

    #[test]
    fn test_resolve_hot_rt_priority_is_off_unless_asked() {
        for raw in [None, Some(""), Some("  "), Some("off"), Some("OFF")] {
            assert_eq!(resolve_hot_rt_priority(raw), HotRtPlan::Off, "{raw:?}");
        }
    }

    #[test]
    fn test_resolve_hot_rt_priority_accepts_only_the_low_range() {
        assert_eq!(resolve_hot_rt_priority(Some("1")), HotRtPlan::Fifo(1));
        assert_eq!(resolve_hot_rt_priority(Some(" 49 ")), HotRtPlan::Fifo(49));
        for bad in ["0", "50", "99", "-1", "abc", "1.5", "300"] {
            assert_eq!(
                resolve_hot_rt_priority(Some(bad)),
                HotRtPlan::Invalid,
                "{bad}"
            );
        }
    }

    #[test]
    fn test_classify_rt_throttle_default_kernel_values_are_active() {
        assert_eq!(
            classify_rt_throttle(Some("950000\n"), Some("1000000\n")),
            ACTIVE
        );
    }

    #[test]
    fn test_classify_rt_throttle_minus_one_or_full_period_is_unlimited() {
        assert_eq!(
            classify_rt_throttle(Some("-1"), Some("1000000")),
            RtThrottle::Unlimited
        );
        assert_eq!(
            classify_rt_throttle(Some("1000000"), Some("1000000")),
            RtThrottle::Unlimited
        );
        assert_eq!(
            classify_rt_throttle(Some("5"), Some("0")),
            RtThrottle::Unlimited
        );
    }

    #[test]
    fn test_classify_rt_throttle_zero_runtime_is_active_and_blocks_rt_entirely() {
        // 0 µs per period: real-time threads cannot run at all; the kernel
        // refuses SCHED_FIFO (EPERM/EINVAL), which the apply path reports.
        assert_eq!(
            classify_rt_throttle(Some("0"), Some("1000000")),
            RtThrottle::Active {
                runtime_us: 0,
                period_us: 1_000_000
            }
        );
    }

    #[test]
    fn test_classify_rt_throttle_missing_or_garbage_is_unreadable() {
        assert_eq!(
            classify_rt_throttle(None, Some("1")),
            RtThrottle::Unreadable
        );
        assert_eq!(
            classify_rt_throttle(Some("1"), None),
            RtThrottle::Unreadable
        );
        assert_eq!(
            classify_rt_throttle(Some("x"), Some("1000000")),
            RtThrottle::Unreadable
        );
        assert_eq!(
            classify_rt_throttle(Some("-2"), Some("1000000")),
            RtThrottle::Unreadable
        );
        assert_eq!(
            classify_rt_throttle(Some("1"), Some("y")),
            RtThrottle::Unreadable
        );
    }

    #[test]
    fn test_decide_hot_rt_applies_reader_one_above_drain_when_throttle_active() {
        let d = decide_hot_rt(HotRtPlan::Fifo(1), || ACTIVE);
        assert_eq!(
            d,
            HotRtDecision::Apply {
                reader: 2,
                drain: 1
            }
        );
        assert_eq!(d.reader(), Some(2));
        assert_eq!(d.drain(), Some(1));
        assert!(!d.is_refusal());
    }

    #[test]
    fn test_decide_hot_rt_refuses_when_throttle_off_or_unreadable() {
        let d = decide_hot_rt(HotRtPlan::Fifo(1), || RtThrottle::Unlimited);
        assert_eq!(d, HotRtDecision::RefusedThrottleUnlimited);
        assert!(d.is_refusal());
        assert_eq!(d.reader(), None);
        assert_eq!(d.drain(), None);
        let d = decide_hot_rt(HotRtPlan::Fifo(1), || RtThrottle::Unreadable);
        assert_eq!(d, HotRtDecision::RefusedThrottleUnreadable);
        assert!(d.is_refusal());
    }

    #[test]
    fn test_decide_hot_rt_off_never_reads_the_throttle() {
        let d = decide_hot_rt(HotRtPlan::Off, || panic!("must not read /proc when off"));
        assert_eq!(d, HotRtDecision::Off);
        assert!(!d.is_refusal());
        let d = decide_hot_rt(HotRtPlan::Invalid, || panic!("must not read /proc"));
        assert_eq!(d, HotRtDecision::RefusedInvalid);
        assert!(d.is_refusal());
    }

    #[test]
    fn test_decide_hot_rt_max_priority_does_not_overflow() {
        let d = decide_hot_rt(HotRtPlan::Fifo(MAX_HOT_RT_PRIORITY), || ACTIVE);
        assert_eq!(
            d,
            HotRtDecision::Apply {
                reader: MAX_HOT_RT_PRIORITY + 1,
                drain: MAX_HOT_RT_PRIORITY
            }
        );
    }

    #[test]
    fn test_read_rt_throttle_never_panics_on_this_host() {
        let _ = read_rt_throttle();
    }

    #[test]
    fn test_resolve_background_core_off_by_default() {
        for raw in [None, Some(""), Some("off"), Some("Off")] {
            assert_eq!(
                resolve_background_core(raw, Some(1), |_| true),
                BackgroundCorePlan::Off
            );
        }
    }

    #[test]
    fn test_resolve_background_core_pins_an_allowed_core_other_than_the_readers() {
        let p = resolve_background_core(Some("0"), Some(1), |c| c < 2);
        assert_eq!(p, BackgroundCorePlan::Pin(0));
        assert!(!p.is_refusal());
    }

    #[test]
    fn test_resolve_background_core_refuses_the_readers_core_a_disallowed_core_and_garbage() {
        assert_eq!(
            resolve_background_core(Some("1"), Some(1), |_| true),
            BackgroundCorePlan::SameAsReaders(1)
        );
        assert_eq!(
            resolve_background_core(Some("7"), Some(1), |c| c < 2),
            BackgroundCorePlan::NotAllowed(7)
        );
        for bad in ["x", "-1", "99999", "0.5"] {
            let p = resolve_background_core(Some(bad), Some(1), |_| true);
            assert_eq!(p, BackgroundCorePlan::Invalid, "{bad}");
            assert!(p.is_refusal());
        }
    }

    #[test]
    fn test_resolve_background_core_without_pinned_readers_pins_any_allowed_core() {
        assert_eq!(
            resolve_background_core(Some("1"), None, |_| true),
            BackgroundCorePlan::Pin(1)
        );
    }
}
