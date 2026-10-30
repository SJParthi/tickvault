//! A memory watchdog that ACTS (plan item 4, scenario #39), and the
//! `tv-task-watch` thread that also publishes the background-task
//! heartbeats ([`crate::task_heartbeat`]).
//!
//! # What existed
//!
//! `resource_monitor` pages RESOURCE-02 when resident memory reaches 80% of
//! the ceiling, the WAL catch-up drain stands down at 60%, and `oom_monitor`
//! reports a kernel OOM kill about a minute after it happens. None of them
//! does anything to bring memory down.
//!
//! # The action ladder
//!
//! Every [`MEMORY_POLL_EVERY_SECS`] seconds this thread reads `VmRSS` and
//! places it against the same ceiling RESOURCE-02 uses, as a level:
//!
//! | Level | Entry (default) | Action (`mode = "act"`) |
//! |---|---|---|
//! | `normal` | below `trim_pct` | nothing |
//! | `trim` | `trim_pct` (70%) | return freed heap pages to the OS (`malloc_trim`), at most once a minute, and only OUTSIDE 09:00–15:40 IST |
//! | `shed` | `shed_pct` (80%) | the trim, in session too, plus a shed signal: caches that opt in ([`shed_generation`]) empty themselves on their next use; repeated at most once a minute |
//! | `critical` | `critical_pct` (90%) | the same, and one log line per episode |
//!
//! A level is left only when usage falls `hysteresis_pct` below its entry,
//! so usage hovering on a line cannot flap.
//!
//! **What the ladder never does:** it never drops a received frame, never
//! touches the WAL, the frame ring, the candle fold or the writers, and never
//! restarts the process. Both actions release only memory nothing is using
//! (free heap pages, and read caches that are rebuilt on demand). It CAN
//! delay: a heap trim makes any thread that allocates during the walk wait
//! for it, socket readers included (see the honest limits).
//!
//! `mode = "off"` (the default) runs no memory poll at all. `"shadow"`
//! classifies and counts what it WOULD do on
//! `tv_memory_watchdog_would_act_total{action}`, and acts on nothing.
//!
//! # Honest limits
//!
//! * **The production build has no trim.** It is a static musl binary
//!   (`aarch64-unknown-linux-musl`, deploy-aws.yml), and `malloc_trim` exists
//!   only in glibc. There every trim is counted on
//!   `tv_memory_watchdog_trims_total{outcome="not_available"}`, one warning is
//!   logged when an `act` watchdog starts, and only the shed signal acts. A
//!   working trim needs a glibc build or an allocator with a purge call, an
//!   owner decision; not taken here.
//! * Where it exists (glibc), `malloc_trim` walks the heap holding each
//!   arena's lock in turn. A thread that allocates during the walk waits for
//!   it. On a multi-GiB heap the walk can take milliseconds (UNMEASURED; the
//!   last walk's duration is published as `tv_memory_watchdog_trim_last_us`).
//!   That is why the `trim` rung stays out of the market session and only
//!   `shed` (where the alternative is the OOM killer) trims in session.
//! * The one cache that opts into the shed signal today is the API quote
//!   cache (at most 2,048 entries, 1 s TTL), so shedding frees little. The
//!   big resident structures (the candle fold,
//!   the seal ring, the frame ring, the WAL buffers) are pre-sized for the
//!   session and are deliberately not shed: shedding them would lose data.
//! * With no readable ceiling (non-Linux dev box) the ladder never leaves
//!   `normal`; counted on `tv_memory_watchdog_probe_failed_total`, logged at
//!   the 1st, 2nd, 4th... unreadable poll in a row, and `usage_pct` reads
//!   [`USAGE_PCT_UNKNOWN`] (-1) instead of the last good value.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tickvault_common::config::{MemoryWatchdogConfig, MemoryWatchdogMode};
use tracing::{info, warn};

use crate::resource_monitor::{
    DEFAULT_PROC_MEMINFO_PATH, DEFAULT_PROC_SELF_STATUS_PATH, parse_vmrss_bytes,
    resolve_cgroup_memory_max_path, resolve_memory_ceiling,
};
use crate::task_heartbeat::{HeartbeatPublisher, publish_once};

/// The watch thread's tick: heartbeats publish every tick.
pub const WATCH_TICK_SECS: u64 = 1;

/// [`WATCH_TICK_SECS`] as a `Duration`.
pub const WATCH_TICK: Duration = Duration::from_secs(WATCH_TICK_SECS);

/// Seconds between two memory polls. Five seconds: memory climbs over
/// minutes, and one `/proc/self/status` read every five seconds costs
/// nothing measurable.
pub const MEMORY_POLL_EVERY_SECS: u64 = 5;

/// The least time between two `malloc_trim` walks.
pub const TRIM_MIN_INTERVAL_SECS: u64 = 60;

/// The least time between two shed signals while the level stays at
/// `shed` or above.
pub const SHED_MIN_INTERVAL_SECS: u64 = 60;

/// Bytes read from `/proc/self/status`. The file is about 1.5 KiB; 8 KiB
/// leaves room and lives on the stack (no allocation per poll).
const STATUS_READ_BYTES: usize = 8 * 1024;

/// `tv_memory_watchdog_usage_pct` while no reading is available: a value no
/// real usage can take, so a held level never passes for a fresh reading.
pub const USAGE_PCT_UNKNOWN: f64 = -1.0;

/// Basis points per percent.
const BP_PER_PCT: u64 = 100;

/// One rung of the ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PressureLevel {
    /// Under `trim_pct`.
    Normal = 0,
    /// At or over `trim_pct`.
    Trim = 1,
    /// At or over `shed_pct`.
    Shed = 2,
    /// At or over `critical_pct`.
    Critical = 3,
}

impl PressureLevel {
    /// The level as written in logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Trim => "trim",
            Self::Shed => "shed",
            Self::Critical => "critical",
        }
    }

    /// The value of the `tv_memory_watchdog_level` gauge (0..=3).
    #[must_use]
    pub const fn gauge_value(self) -> f64 {
        match self {
            Self::Normal => 0.0,
            Self::Trim => 1.0,
            Self::Shed => 2.0,
            Self::Critical => 3.0,
        }
    }
}

/// Entry line of `level` in basis points (0 for `normal`).
const fn entry_bp(level: PressureLevel, cfg: &MemoryWatchdogConfig) -> u64 {
    let pct = match level {
        PressureLevel::Normal => 0,
        PressureLevel::Trim => cfg.trim_pct,
        PressureLevel::Shed => cfg.shed_pct,
        PressureLevel::Critical => cfg.critical_pct,
    };
    (pct as u64) * BP_PER_PCT
}

const LEVELS_DESC: [PressureLevel; 4] = [
    PressureLevel::Critical,
    PressureLevel::Shed,
    PressureLevel::Trim,
    PressureLevel::Normal,
];

/// PURE: resident memory as basis points of the ceiling (8,000 = 80%).
/// `None` when either reading is missing or the ceiling is 0 (no
/// denominator: nothing can be concluded). `u128` so `rss * 10_000` cannot
/// overflow. O(1).
#[must_use]
pub const fn usage_bp(rss_bytes: Option<u64>, ceiling_bytes: Option<u64>) -> Option<u64> {
    let (Some(rss), Some(ceiling)) = (rss_bytes, ceiling_bytes) else {
        return None;
    };
    if ceiling == 0 {
        return None;
    }
    let bp = (rss as u128) * 10_000 / (ceiling as u128);
    Some(if bp > u64::MAX as u128 {
        u64::MAX
    } else {
        bp as u64
    })
}

/// PURE: the next level from `prev` at `bp` basis points.
///
/// Rising: the highest level whose entry line `bp` has reached, at once.
/// Falling: a level is kept until `bp` is `hysteresis_pct` below its entry;
/// the result is the highest level at or below `prev` that `bp` still holds
/// with the hysteresis allowance. O(4).
#[must_use]
pub fn next_level(prev: PressureLevel, bp: u64, cfg: &MemoryWatchdogConfig) -> PressureLevel {
    let rising = LEVELS_DESC
        .iter()
        .copied()
        .find(|level| bp >= entry_bp(*level, cfg))
        .unwrap_or(PressureLevel::Normal);
    if rising >= prev {
        return rising;
    }
    let hysteresis_bp = u64::from(cfg.hysteresis_pct) * BP_PER_PCT;
    LEVELS_DESC
        .iter()
        .copied()
        .filter(|level| *level <= prev)
        .find(|level| bp.saturating_add(hysteresis_bp) >= entry_bp(*level, cfg))
        .unwrap_or(PressureLevel::Normal)
}

/// What one poll decided to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// The level after this poll.
    pub level: PressureLevel,
    /// The level before it.
    pub prev: PressureLevel,
    /// Walk the heap and return free pages to the OS.
    pub trim: bool,
    /// Signal opt-in caches to empty themselves.
    pub shed: bool,
}

/// The ladder's memory between polls, in whole seconds of the watch
/// thread's own monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderState {
    /// The current level.
    pub level: PressureLevel,
    /// When the last trim ran (or would have, in shadow).
    pub last_trim_secs: Option<u64>,
    /// When the last shed signal went out (or would have).
    pub last_shed_secs: Option<u64>,
}

impl LadderState {
    /// A fresh ladder at `normal`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            level: PressureLevel::Normal,
            last_trim_secs: None,
            last_shed_secs: None,
        }
    }
}

impl Default for LadderState {
    fn default() -> Self {
        Self::new()
    }
}

const fn due(last: Option<u64>, now_secs: u64, min_interval: u64) -> bool {
    match last {
        None => true,
        Some(at) => now_secs.saturating_sub(at) >= min_interval,
    }
}

/// PURE: one poll of the ladder. `bp = None` (unreadable) keeps the level
/// and decides nothing. Updates `state` and returns the decision. O(1).
pub fn decide(
    state: &mut LadderState,
    bp: Option<u64>,
    now_secs: u64,
    in_session: bool,
    cfg: &MemoryWatchdogConfig,
) -> Decision {
    let prev = state.level;
    let Some(bp) = bp else {
        return Decision {
            level: prev,
            prev,
            trim: false,
            shed: false,
        };
    };
    let level = next_level(prev, bp, cfg);
    state.level = level;
    let trim_rung = match level {
        PressureLevel::Normal => false,
        PressureLevel::Trim => !in_session,
        PressureLevel::Shed | PressureLevel::Critical => true,
    };
    let trim = trim_rung && due(state.last_trim_secs, now_secs, TRIM_MIN_INTERVAL_SECS);
    let shed_rung = level >= PressureLevel::Shed;
    // Rate-limited on every entry too (hostile review 2026-10-30): usage
    // crossing the shed line and back every poll must not shed every poll.
    let shed = shed_rung && due(state.last_shed_secs, now_secs, SHED_MIN_INTERVAL_SECS);
    if trim {
        state.last_trim_secs = Some(now_secs);
    }
    if shed {
        state.last_shed_secs = Some(now_secs);
    }
    Decision {
        level,
        prev,
        trim,
        shed,
    }
}

static SHED_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The shed signal: a counter bumped each time the ladder sheds. A cache
/// that opts in remembers the value it last saw and empties itself when the
/// value moves. O(1), one relaxed load, no allocation.
#[must_use]
#[inline]
pub fn shed_generation() -> u64 {
    SHED_GENERATION.load(Ordering::Relaxed)
}

fn bump_shed_generation() {
    SHED_GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// The one `malloc_trim` call, and nothing else that needs `unsafe`.
///
/// The crate root denies `unsafe_code` outside tests; this module is one of
/// the production opt-outs listed in `crates/core/tests/unsafe_code_lint_guard.rs`.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[allow(unsafe_code)] // APPROVED: one malloc_trim(3) call with a constant argument, see SAFETY
mod trim_call {
    /// Returns freed heap pages to the OS. `true` when glibc released any.
    pub(super) fn malloc_trim() -> bool {
        // SAFETY: `malloc_trim` takes a plain integer, touches only glibc's own allocator state under glibc's own locks, and is documented as safe to call from any thread at any time.
        let released = unsafe { libc::malloc_trim(0) }; // SAFETY: see above
        released != 0
    }
}

/// Outcome of one trim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimOutcome {
    /// glibc returned memory to the OS.
    Released,
    /// glibc had nothing to return.
    NothingToRelease,
    /// No `malloc_trim` on this platform.
    NotAvailable,
}

/// Whether this build can trim the heap at all (glibc Linux only).
#[must_use]
pub const fn trim_available() -> bool {
    cfg!(all(target_os = "linux", target_env = "gnu"))
}

/// Runs one heap trim. See the module's honest limits for its cost.
#[must_use]
pub fn trim_heap() -> TrimOutcome {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        if trim_call::malloc_trim() {
            TrimOutcome::Released
        } else {
            TrimOutcome::NothingToRelease
        }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        TrimOutcome::NotAvailable
    }
}

/// Reads `VmRSS` from a `/proc/<pid>/status`-shaped file into a stack
/// buffer: no allocation (the path is short enough for std's stack path
/// buffer), O(file size) ≈ 1.5 KiB. `None` when unreadable.
#[must_use]
pub fn read_vmrss_bytes(status_path: &Path) -> Option<u64> {
    let mut file = std::fs::File::open(status_path).ok()?;
    let mut buf = [0u8; STATUS_READ_BYTES];
    let mut filled = 0usize;
    while filled < buf.len() {
        let rest = buf.get_mut(filled..)?;
        match file.read(rest) {
            Ok(0) => break,
            Ok(n) => filled = filled.saturating_add(n),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    let text = std::str::from_utf8(buf.get(..filled)?).ok()?;
    parse_vmrss_bytes(text)
}

/// Pre-resolved Prometheus handles: the poll allocates nothing after
/// [`MemoryWatchdog::new`].
struct Handles {
    level: metrics::Gauge,
    usage_pct: metrics::Gauge,
    trims_released: metrics::Counter,
    trims_nothing: metrics::Counter,
    trims_not_available: metrics::Counter,
    trim_last_us: metrics::Gauge,
    sheds: metrics::Counter,
    would_trim: metrics::Counter,
    would_shed: metrics::Counter,
}

impl Handles {
    fn resolve() -> Self {
        let h = Self {
            level: metrics::gauge!("tv_memory_watchdog_level"),
            usage_pct: metrics::gauge!("tv_memory_watchdog_usage_pct"),
            trims_released: metrics::counter!("tv_memory_watchdog_trims_total", "outcome" => "released"),
            trims_nothing: metrics::counter!("tv_memory_watchdog_trims_total", "outcome" => "nothing_to_release"),
            trims_not_available: metrics::counter!("tv_memory_watchdog_trims_total", "outcome" => "not_available"),
            trim_last_us: metrics::gauge!("tv_memory_watchdog_trim_last_us"),
            sheds: metrics::counter!("tv_memory_watchdog_sheds_total"),
            would_trim: metrics::counter!("tv_memory_watchdog_would_act_total", "action" => "trim"),
            would_shed: metrics::counter!("tv_memory_watchdog_would_act_total", "action" => "shed"),
        };
        // Registered at 0 so a reader sees "watching, never acted".
        h.trims_released.increment(0);
        h.trims_nothing.increment(0);
        h.trims_not_available.increment(0);
        h.sheds.increment(0);
        h.would_trim.increment(0);
        h.would_shed.increment(0);
        h
    }
}

/// The ladder plus its ceiling and metric handles.
pub struct MemoryWatchdog {
    cfg: MemoryWatchdogConfig,
    ceiling_bytes: Option<u64>,
    state: LadderState,
    /// Polls in a row with no usable reading (throttles the warning).
    unreadable_streak: u64,
    /// `tv_memory_watchdog_probe_failed_total`, resolved on the first
    /// unreadable poll and reused after (no lookup per poll).
    probe_failed: Option<metrics::Counter>,
    /// Shed signals this watchdog sent (act mode). Per instance, so tests
    /// never read the process-global generation other tests also move.
    sheds_sent: u64,
    /// Trims this watchdog attempted (act mode), whatever the outcome.
    trims_run: u64,
    handles: Handles,
}

impl MemoryWatchdog {
    /// A watchdog measuring against `ceiling_bytes` (the RESOURCE-02
    /// ceiling; `None` when it could not be read).
    #[must_use]
    pub fn new(cfg: MemoryWatchdogConfig, ceiling_bytes: Option<u64>) -> Self {
        if cfg.mode == MemoryWatchdogMode::Act && !trim_available() {
            warn!(
                "memory watchdog: this build has no malloc_trim (musl), so the trim rung \
                 does nothing here and is counted as outcome=not_available; only the shed \
                 signal acts. A working trim needs a glibc build or another allocator, \
                 which is the owner's call"
            );
        }
        Self {
            cfg,
            ceiling_bytes,
            state: LadderState::new(),
            unreadable_streak: 0,
            probe_failed: None,
            sheds_sent: 0,
            trims_run: 0,
            handles: Handles::resolve(),
        }
    }

    /// The current level.
    #[must_use]
    pub const fn level(&self) -> PressureLevel {
        self.state.level
    }

    /// A poll with no usable reading (resident memory or the ceiling could
    /// not be read): counted, and logged at the 1st, 2nd, 4th, 8th... in a
    /// row, so a host that can never be read (a dev box) does not log every
    /// five seconds. The ladder holds its level and acts on nothing.
    fn note_unreadable(&mut self) {
        self.unreadable_streak = self.unreadable_streak.saturating_add(1);
        self.probe_failed
            .get_or_insert_with(|| metrics::counter!("tv_memory_watchdog_probe_failed_total"))
            .increment(1);
        // No reading: publish the "unknown" sentinel, never the last good
        // value, which would read as current.
        self.handles.usage_pct.set(USAGE_PCT_UNKNOWN);
        if self.unreadable_streak.is_power_of_two() {
            warn!(
                polls_in_a_row = self.unreadable_streak,
                ceiling_known = self.ceiling_bytes.is_some(),
                "memory watchdog: resident memory or its ceiling could not be read; the \
                 ladder holds its level and acts on nothing until a reading succeeds"
            );
        }
    }

    /// One poll at `rss_bytes`. Decides, then acts (`act`), counts
    /// (`shadow`), or does nothing (`off`). Publishes the level and usage.
    ///
    /// O(1). No allocation except the one log line on a level change and
    /// the trim walk itself (inside glibc, which frees, not allocates).
    pub fn step(&mut self, rss_bytes: Option<u64>, now_secs: u64, in_session: bool) -> Decision {
        let bp = usage_bp(rss_bytes, self.ceiling_bytes);
        if bp.is_none() {
            self.note_unreadable();
        } else {
            self.unreadable_streak = 0;
        }
        let decision = decide(&mut self.state, bp, now_secs, in_session, &self.cfg);
        self.handles.level.set(decision.level.gauge_value());
        if let Some(bp) = bp {
            let whole = u32::try_from(bp / BP_PER_PCT).unwrap_or(u32::MAX);
            // APPROVED: a whole percentage (u32), not an f32 price
            self.handles.usage_pct.set(f64::from(whole));
        }
        if decision.level != decision.prev {
            report_level_change(&decision, bp, self.cfg.mode);
        }
        match self.cfg.mode {
            MemoryWatchdogMode::Off => {}
            MemoryWatchdogMode::Shadow => {
                if decision.trim {
                    self.handles.would_trim.increment(1);
                }
                if decision.shed {
                    self.handles.would_shed.increment(1);
                }
            }
            MemoryWatchdogMode::Act => {
                if decision.shed {
                    bump_shed_generation();
                    self.sheds_sent = self.sheds_sent.saturating_add(1);
                    self.handles.sheds.increment(1);
                }
                if decision.trim {
                    self.trims_run = self.trims_run.saturating_add(1);
                    let started = Instant::now();
                    match trim_heap() {
                        TrimOutcome::Released => self.handles.trims_released.increment(1),
                        TrimOutcome::NothingToRelease => self.handles.trims_nothing.increment(1),
                        // The production musl build has no malloc_trim: count it
                        // as not done, and leave the walk-time gauge alone.
                        TrimOutcome::NotAvailable => {
                            self.handles.trims_not_available.increment(1);
                            return decision;
                        }
                    }
                    let micros = u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);
                    // APPROVED: a duration in whole microseconds (u32), not an f32 price
                    self.handles.trim_last_us.set(f64::from(micros));
                }
            }
        }
        decision
    }
}

fn report_level_change(decision: &Decision, bp: Option<u64>, mode: MemoryWatchdogMode) {
    let pct = bp.unwrap_or(0) / BP_PER_PCT;
    if decision.level > decision.prev && decision.level >= PressureLevel::Shed {
        warn!(
            level = decision.level.as_str(),
            prev = decision.prev.as_str(),
            usage_pct = pct,
            mode = mode.as_str(),
            "memory watchdog: resident memory is high; releasing free heap pages (where this \
             build can) and opt-in caches. No received frame is dropped; a heap trim can make \
             threads that allocate wait for its walk. If memory reaches the kernel's limit the \
             process is killed: whether to restart earlier is the owner's call"
        );
    } else {
        info!(
            level = decision.level.as_str(),
            prev = decision.prev.as_str(),
            usage_pct = pct,
            mode = mode.as_str(),
            "memory watchdog: level changed"
        );
    }
}

/// Seconds of the IST day now, for the session gate.
fn ist_secs_of_day_now() -> u32 {
    let now_ist = chrono::Utc::now().timestamp().saturating_add(i64::from(
        tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
    ));
    u32::try_from(now_ist.rem_euclid(86_400)).unwrap_or(0)
}

/// Starts the `tv-task-watch` thread: every second it publishes the
/// background-task heartbeats; every [`MEMORY_POLL_EVERY_SECS`] it runs the
/// memory ladder, unless `cfg.mode` is `off`.
///
/// Its own OS thread, so a wedged tokio runtime cannot stop it (the same
/// reason `tv-telemetry` is a thread). Call once, after the metrics recorder
/// is installed.
///
/// # Errors
/// The OS refused to create the thread.
// TEST-EXEMPT: thread spawn over the unit-tested `MemoryWatchdog::step`, `publish_once` and `read_vmrss_bytes`; spawn site pinned by `test_main_spawns_the_task_watch_thread`.
pub fn spawn_watch_thread(
    cfg: MemoryWatchdogConfig,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let mut heartbeats = HeartbeatPublisher::resolve();
    let mut watchdog = (cfg.mode != MemoryWatchdogMode::Off).then(|| {
        let ceiling = resolve_memory_ceiling(
            &resolve_cgroup_memory_max_path(),
            Path::new(DEFAULT_PROC_MEMINFO_PATH),
        );
        info!(
            mode = cfg.mode.as_str(),
            ceiling_bytes = ceiling.bytes().unwrap_or(0),
            ceiling_source = ceiling.source(),
            trim_pct = cfg.trim_pct,
            shed_pct = cfg.shed_pct,
            critical_pct = cfg.critical_pct,
            "memory watchdog started"
        );
        MemoryWatchdog::new(cfg, ceiling.bytes())
    });
    std::thread::Builder::new()
        .name("tv-task-watch".to_owned())
        .spawn(move || {
            let started = Instant::now();
            let status = Path::new(DEFAULT_PROC_SELF_STATUS_PATH);
            let mut next_memory_poll = 0u64;
            loop {
                let now = Instant::now();
                // A task that stopped because the process is shutting down is
                // not stuck: publish nothing once the shutdown is classified.
                if !crate::hot_path_telemetry::is_shutting_down() {
                    let _stuck = publish_once(&mut heartbeats, now);
                }
                let now_secs = now.saturating_duration_since(started).as_secs();
                if let Some(dog) = watchdog.as_mut()
                    && now_secs >= next_memory_poll
                {
                    next_memory_poll = now_secs.saturating_add(MEMORY_POLL_EVERY_SECS);
                    let in_session =
                        crate::hot_path_telemetry::is_in_session_secs_of_day(ist_secs_of_day_now());
                    let _ = dog.step(read_vmrss_bytes(status), now_secs, in_session);
                }
                // APPROVED-BLOCKING: this is the watch thread's own dedicated OS thread; sleeping here blocks nothing else, and it is why the watch survives a wedged tokio runtime.
                std::thread::sleep(WATCH_TICK);
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: MemoryWatchdogMode) -> MemoryWatchdogConfig {
        MemoryWatchdogConfig {
            mode,
            ..MemoryWatchdogConfig::default()
        }
    }

    const GIB: u64 = 1 << 30;

    #[test]
    fn usage_bp_basic_and_unknowns() {
        assert_eq!(usage_bp(Some(8), Some(10)), Some(8_000));
        assert_eq!(usage_bp(Some(0), Some(10)), Some(0));
        assert_eq!(usage_bp(None, Some(10)), None);
        assert_eq!(usage_bp(Some(1), None), None);
        assert_eq!(
            usage_bp(Some(1), Some(0)),
            None,
            "zero ceiling is no denominator"
        );
        assert_eq!(usage_bp(Some(u64::MAX), Some(u64::MAX)), Some(10_000));
        assert_eq!(
            usage_bp(Some(u64::MAX), Some(1)),
            Some(u64::MAX),
            "saturates"
        );
    }

    #[test]
    fn rising_jumps_straight_to_the_highest_line_reached() {
        let c = cfg(MemoryWatchdogMode::Act);
        let n = PressureLevel::Normal;
        assert_eq!(next_level(n, 6_999, &c), PressureLevel::Normal);
        assert_eq!(next_level(n, 7_000, &c), PressureLevel::Trim);
        assert_eq!(next_level(n, 8_000, &c), PressureLevel::Shed);
        assert_eq!(next_level(n, 9_000, &c), PressureLevel::Critical);
        assert_eq!(next_level(n, 20_000, &c), PressureLevel::Critical);
    }

    #[test]
    fn falling_needs_the_hysteresis_margin() {
        let c = cfg(MemoryWatchdogMode::Act);
        // Shed entered at 80%; hysteresis 5 points: kept down to 75%.
        assert_eq!(
            next_level(PressureLevel::Shed, 7_999, &c),
            PressureLevel::Shed
        );
        assert_eq!(
            next_level(PressureLevel::Shed, 7_500, &c),
            PressureLevel::Shed
        );
        assert_eq!(
            next_level(PressureLevel::Shed, 7_499, &c),
            PressureLevel::Trim
        );
        assert_eq!(
            next_level(PressureLevel::Shed, 6_499, &c),
            PressureLevel::Normal
        );
        // Critical falling to 84% lands on Shed (still over its own line).
        assert_eq!(
            next_level(PressureLevel::Critical, 8_400, &c),
            PressureLevel::Shed
        );
        assert_eq!(
            next_level(PressureLevel::Critical, 8_500, &c),
            PressureLevel::Critical
        );
        assert_eq!(
            next_level(PressureLevel::Trim, 0, &c),
            PressureLevel::Normal
        );
    }

    #[test]
    fn hovering_on_a_line_never_flaps() {
        let c = cfg(MemoryWatchdogMode::Act);
        let mut level = PressureLevel::Normal;
        let mut changes = 0;
        for i in 0..1_000u64 {
            let bp = if i % 2 == 0 { 8_001 } else { 7_999 };
            let next = next_level(level, bp, &c);
            if next != level {
                changes += 1;
            }
            level = next;
        }
        assert_eq!(changes, 1, "one rise, then held by the hysteresis");
    }

    #[test]
    fn next_level_is_monotone_in_usage_for_every_start() {
        let c = cfg(MemoryWatchdogMode::Act);
        for prev in LEVELS_DESC {
            let mut last = PressureLevel::Normal;
            for bp in (0..=12_000u64).step_by(25) {
                let level = next_level(prev, bp, &c);
                assert!(level >= last, "prev {prev:?} bp {bp}: {level:?} < {last:?}");
                last = level;
            }
        }
    }

    #[test]
    fn trim_rung_waits_for_the_close_but_shed_trims_in_session() {
        let c = cfg(MemoryWatchdogMode::Act);
        let mut s = LadderState::new();
        let d = decide(&mut s, Some(7_200), 0, true, &c);
        assert_eq!(d.level, PressureLevel::Trim);
        assert!(!d.trim, "no heap walk in session at the trim rung");
        assert!(!d.shed);
        let d = decide(&mut s, Some(7_200), 5, false, &c);
        assert!(d.trim, "out of session the trim rung trims");
        let d = decide(&mut s, Some(8_100), 10, true, &c);
        assert_eq!(d.level, PressureLevel::Shed);
        assert!(d.shed, "entering shed always sheds");
        assert!(!d.trim, "trim ran 5 s ago; once a minute at most");
        let d = decide(&mut s, Some(8_100), 70, true, &c);
        assert!(d.trim, "in session at shed, after a minute");
        assert!(d.shed, "repeated once a minute while high");
        let d = decide(&mut s, Some(8_100), 75, true, &c);
        assert!(!d.trim && !d.shed, "rate-limited");
    }

    #[test]
    fn an_unreadable_poll_keeps_the_level_and_does_nothing() {
        let c = cfg(MemoryWatchdogMode::Act);
        let mut s = LadderState::new();
        let _ = decide(&mut s, Some(9_500), 0, false, &c);
        let d = decide(&mut s, None, 100, false, &c);
        assert_eq!(d.level, PressureLevel::Critical);
        assert!(!d.trim && !d.shed);
    }

    #[test]
    fn re_entering_shed_inside_the_minute_is_rate_limited() {
        // Hostile review 2026-10-30: usage crossing the shed line and back
        // every poll must not shed every poll.
        let c = cfg(MemoryWatchdogMode::Act);
        let mut s = LadderState::new();
        assert!(
            decide(&mut s, Some(8_000), 0, true, &c).shed,
            "first entry sheds"
        );
        assert_eq!(
            decide(&mut s, Some(6_000), 10, true, &c).level,
            PressureLevel::Normal
        );
        assert!(
            !decide(&mut s, Some(8_000), 20, true, &c).shed,
            "inside the minute"
        );
        assert_eq!(
            decide(&mut s, Some(6_000), 30, true, &c).level,
            PressureLevel::Normal
        );
        assert!(
            decide(&mut s, Some(8_000), 60, true, &c).shed,
            "a minute later"
        );
        let mut flapping = 0;
        let mut s2 = LadderState::new();
        for i in 0..120u64 {
            let bp = if i % 2 == 0 { 8_000 } else { 6_000 };
            if decide(&mut s2, Some(bp), i * 5, true, &c).shed {
                flapping += 1;
            }
        }
        assert_eq!(flapping, 10, "10 minutes of flapping sheds once a minute");
    }

    #[test]
    fn shadow_never_moves_the_shed_signal() {
        let mut dog = MemoryWatchdog::new(cfg(MemoryWatchdogMode::Shadow), Some(10 * GIB));
        let d = dog.step(Some(9 * GIB + GIB / 2), 0, true);
        assert_eq!(d.level, PressureLevel::Critical);
        assert!(d.shed && d.trim);
        assert_eq!(dog.sheds_sent, 0, "shadow decides, never acts");
        assert_eq!(dog.trims_run, 0);
    }

    #[test]
    fn test_shed_generation_moves_only_in_act() {
        let mut dog = MemoryWatchdog::new(cfg(MemoryWatchdogMode::Act), Some(10 * GIB));
        let before = shed_generation();
        let d = dog.step(Some(8 * GIB + GIB / 2), 0, false);
        assert!(d.shed && d.trim);
        assert_eq!(dog.sheds_sent, 1);
        assert_eq!(dog.trims_run, 1, "attempted whatever the outcome");
        // Monotonic: other tests can only move it further.
        assert!(shed_generation() > before);
        assert_eq!(dog.level(), PressureLevel::Shed);
    }

    #[test]
    fn off_mode_still_tracks_the_level_but_never_acts() {
        let mut dog = MemoryWatchdog::new(cfg(MemoryWatchdogMode::Off), Some(10 * GIB));
        let d = dog.step(Some(9 * GIB), 0, false);
        assert_eq!(d.level, PressureLevel::Critical);
        assert_eq!(dog.sheds_sent, 0);
        assert_eq!(dog.trims_run, 0);
    }

    #[test]
    fn an_unreadable_streak_counts_up_and_resets_on_a_reading() {
        let mut dog = MemoryWatchdog::new(cfg(MemoryWatchdogMode::Shadow), Some(10 * GIB));
        let _ = dog.step(None, 0, false);
        let _ = dog.step(None, 5, false);
        let _ = dog.step(None, 10, false);
        assert_eq!(dog.unreadable_streak, 3);
        let _ = dog.step(Some(GIB), 15, false);
        assert_eq!(dog.unreadable_streak, 0, "a good reading ends the streak");
    }

    #[test]
    fn no_ceiling_never_leaves_normal() {
        let mut dog = MemoryWatchdog::new(cfg(MemoryWatchdogMode::Act), None);
        let d = dog.step(Some(u64::MAX), 0, false);
        assert_eq!(d.level, PressureLevel::Normal);
        assert!(!d.trim && !d.shed);
    }

    #[test]
    fn test_trim_available_matches_what_trim_heap_reports() {
        let outcome = trim_heap();
        assert_eq!(outcome == TrimOutcome::NotAvailable, !trim_available());
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        assert_ne!(outcome, TrimOutcome::NotAvailable);
        #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
        assert_eq!(outcome, TrimOutcome::NotAvailable);
    }

    #[test]
    fn read_vmrss_from_a_fixture_and_from_a_missing_path() {
        let dir = std::env::temp_dir().join(format!(
            "tv-memwd-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("status");
        std::fs::write(
            &path,
            "Name:\ttickvault\nVmRSS:\t   2048 kB\nThreads:\t40\n",
        )
        .expect("fixture");
        assert_eq!(read_vmrss_bytes(&path), Some(2048 * 1024));
        std::fs::write(&path, "Name:\tx\n").expect("fixture");
        assert_eq!(read_vmrss_bytes(&path), None, "no VmRSS line");
        assert_eq!(read_vmrss_bytes(&dir.join("absent")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_vmrss_from_this_process_is_positive() {
        let rss = read_vmrss_bytes(Path::new(DEFAULT_PROC_SELF_STATUS_PATH));
        assert!(rss.is_some_and(|b| b > 0));
    }

    #[test]
    fn test_gauge_value_and_level_labels() {
        assert_eq!(PressureLevel::Normal.as_str(), "normal");
        assert_eq!(PressureLevel::Critical.as_str(), "critical");
        assert!((PressureLevel::Shed.gauge_value() - 2.0).abs() < f64::EPSILON);
        assert!(PressureLevel::Trim < PressureLevel::Shed);
    }

    #[test]
    fn ist_secs_of_day_is_inside_a_day() {
        assert!(ist_secs_of_day_now() < 86_400);
    }
}
