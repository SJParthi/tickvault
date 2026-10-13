//! Background disk-health watcher for the spill directory.
//!
//! Closes the single highest-risk gap in the zero-loss chain identified in
//! the 2026-04-28 audit: "disk full + QuestDB down simultaneously". Without
//! this watcher operators only learn about a full spill disk when ticks
//! actually start landing in the DLQ — at which point real loss is already
//! happening if the DLQ also fills up.
//!
//! The watcher probes the filesystem every 60s and exposes:
//!   * `tv_spill_dir_free_bytes` — gauge, current free bytes
//!   * `tv_spill_dir_total_bytes` — gauge, current total bytes
//!   * `tv_spill_dir_health_check_failed_total` — counter
//!   * `tv_disk_hours_left_at_fill_rate` — gauge, hours until full at the
//!     last hour's fill rate, -1 when not filling (DA-12, local `/metrics` only)
//!
//! Prometheus alert rule (added separately): fires CRITICAL Telegram when
//! `tv_spill_dir_free_bytes < SPILL_DISK_FREE_BYTES_CRITICAL_THRESHOLD`.
//! Operator gets ~hours of warning instead of seconds.
//!
//! # How free space is read (deep audit DA-12, 2026-10-12)
//!
//! One `statvfs(2)` system call per probe, through `libc`. Until 2026-10-12
//! this forked the external `df` tool (GNU form first, then the BSD form) and
//! parsed its text. That probe is also called by the tick and depth rescue
//! paths, including the frame drain's own inline fallback, so a process start
//! sat on the path that runs exactly when the disk is slow; a missing `df`, a
//! process-table or file-handle shortage turned a recoverable spill into a
//! refused one; and the two output formats behaved differently on macOS and
//! Linux. `statvfs` is one syscall with the same struct on both, no parsing
//! and no child process. It can still block on a wedged volume (as any call
//! touching that filesystem can), which is why the periodic callers keep
//! running it on the blocking pool.
//!
//! `libc` was already a pinned workspace dependency (used by `tickvault-core`
//! for the reader-thread affinity); this crate now names it directly. No new
//! package enters `Cargo.lock`.

use std::path::PathBuf;
use std::time::Duration;

use tracing::{debug, error, info, warn};

/// Cadence of the disk-health probe. 60s gives ~hours of warning before a
/// fast-filling spill dir actually runs out of space, while burning
/// negligible CPU.
pub const SPILL_DISK_HEALTH_POLL_INTERVAL_SECS: u64 = 60;

/// Threshold below which the spill dir is considered critically low. The
/// matching alert routes to Telegram CRITICAL when the gauge dips below this.
///
/// 2026-08-10: raised 1 GiB → 10 GiB. The 1 GiB was justified as "~100 minutes
/// of operator warning" at a spill rate of ~10 MB/min — a rate measured when
/// the only writer was a QuestDB outage backing up a 4-index REST runtime.
/// That reasoning does not survive the authorized target: at the r8g.xlarge
/// scale (operator Quote 13, 2026-08-08 — 13 timeframes at ~25,000
/// instruments, tick retention on a 100 GB volume) the §7 Rule 3 estimate is
/// 44–141 GB of ticks per MONTH, i.e. ~1–3 GB/day steady-state and far higher
/// during a spill episode. At those rates 1 GiB of free space is **under an
/// hour of runway**, and a threshold that fires with under an hour left is a
/// notification, not a warning.
///
/// 10 GiB restores a genuinely actionable margin (~10% of the 100 GB volume)
/// without being so large it fires during normal operation. Deliberately a
/// FIXED byte count rather than a percentage: the spill directory and the data
/// volume can be sized independently, and a percentage silently re-scales the
/// alarm every time the disk changes — which is the drift class this whole
/// sweep exists to eliminate.
pub const SPILL_DISK_FREE_BYTES_CRITICAL_THRESHOLD: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB

/// Fraction of the measured filesystem the warning aims to reserve, expressed
/// as a divisor: `total / 10` = 10%.
///
/// 10% is not a new number — it is the number the constant above was CHOSEN
/// for, in its own words: *"10 GiB restores a genuinely actionable margin
/// (~10% of the 100 GB volume)"*. What was missing is that the volume moved
/// and the byte count did not.
pub const SPILL_DISK_FREE_TARGET_DIVISOR: u64 = 10;

/// The free-space threshold to warn at, for a filesystem of `total_bytes`.
///
/// # Why this is derived and not a constant any more
///
/// The fixed 10 GiB above was justified as "~10% of the 100 GB volume". The
/// live root volume is **300 GB** since 2026-08-25 (Quote 19), so the same
/// number is 3.3% — and at the fill rate measured during that day's disk-full
/// outage it buys well under an hour, for a problem whose remediation takes
/// longer than that. The intent was a proportion; only the arithmetic was
/// frozen.
///
/// The old doc argued AGAINST a percentage on the grounds that "the spill
/// directory and the data volume can be sized independently". That objection
/// does not apply to what is actually measured here: `probe_disk_free_bytes`
/// reports `total_bytes` for the filesystem the spill directory ITSELF lives
/// on, so a fraction of that total is the same volume, not a proxy for it.
///
/// # The floor is what makes this safe in both directions
///
/// `max(FLOOR, total/10)` can only ever move the threshold UP relative to
/// today. A small dev disk keeps the 10 GiB it has now rather than collapsing
/// to a threshold that would fire after the disk is already unusable; the
/// 300 GB production volume gets 30 GiB, which is the margin the constant
/// always claimed to provide.
///
/// Pure and O(1). Pinned by `derived_threshold_scales_with_the_volume`.
#[must_use]
pub const fn spill_disk_free_critical_threshold(total_bytes: u64) -> u64 {
    let proportional = total_bytes / SPILL_DISK_FREE_TARGET_DIVISOR;
    if proportional > SPILL_DISK_FREE_BYTES_CRITICAL_THRESHOLD {
        proportional
    } else {
        SPILL_DISK_FREE_BYTES_CRITICAL_THRESHOLD
    }
}

/// Outcome of one health check, exposed for unit testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskHealthOutcome {
    /// `statvfs` succeeded; we have a free-bytes number.
    Ok { free_bytes: u64, total_bytes: u64 },
    /// `statvfs` failed (path missing or unreadable, a path with an interior
    /// NUL byte, or a non-Unix OS). Gauge is left at its previous value;
    /// operator has no signal.
    ProbeFailed { reason: &'static str },
}

/// The `reason` of a failed probe. One string for every failure so the
/// callers' blind-write counters read one series.
pub const DISK_PROBE_FAILED_REASON: &str = "statvfs_failed";

/// One `statvfs(2)` reading of the filesystem holding a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FsStats {
    /// Bytes an unprivileged writer may still use (`f_bavail * f_frsize`),
    /// the same "Avail" figure `df` printed.
    free_bytes: u64,
    /// Size of the filesystem (`f_blocks * f_frsize`).
    total_bytes: u64,
    /// Inodes an unprivileged writer may still use (`f_favail`).
    free_inodes: u64,
    /// Inodes of the filesystem (`f_files`).
    total_inodes: u64,
}

/// The one `statvfs` call, and nothing else that needs `unsafe`.
///
/// The crate root denies `unsafe_code` outside tests (audit CR-5,
/// 2026-10-11); this module is one of the two production opt-outs, both
/// listed in `crates/core/tests/unsafe_code_lint_guard.rs`. The block carries
/// a SAFETY note.
#[cfg(unix)]
#[allow(unsafe_code)] // APPROVED: one statvfs(2) call on an owned NUL-terminated path, see SAFETY
mod statvfs_probe {
    use std::os::unix::ffi::OsStrExt;

    /// `statvfs` on `path`; `None` when the call fails or the path holds an
    /// interior NUL byte. O(1): one syscall, plus one `CString` allocation
    /// for the path (the kernel needs a NUL-terminated copy).
    pub(super) fn stat(path: &std::path::Path) -> Option<super::FsStats> {
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut st = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
        // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call, and `st` points to writable memory the size of `libc::statvfs`.
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), st.as_mut_ptr()) }; // SAFETY: see above
        if rc != 0 {
            return None;
        }
        // SAFETY: `statvfs` returned 0, so the kernel filled every field; the struct was also zeroed first, so no field is uninitialised either way.
        let st = unsafe { st.assume_init() }; // SAFETY: see above
        Some(super::fs_stats_from_fields(
            widen(st.f_bavail),
            widen(st.f_blocks),
            widen(st.f_frsize),
            widen(st.f_favail),
            widen(st.f_files),
        ))
    }

    /// The `statvfs` field types differ by platform (`u64` on 64-bit Linux,
    /// `u32` on macOS); every one of them widens losslessly to `u64`.
    fn widen<T: Into<u64>>(v: T) -> u64 {
        v.into()
    }
}

/// Bytes and inodes from the raw `statvfs` fields. Saturating, so a
/// fabricated or corrupt block size can never wrap into a small number that
/// reads as "nearly full" or "plenty free". Pure, O(1).
const fn fs_stats_from_fields(
    avail_blocks: u64,
    total_blocks: u64,
    fragment_size: u64,
    free_inodes: u64,
    total_inodes: u64,
) -> FsStats {
    FsStats {
        free_bytes: avail_blocks.saturating_mul(fragment_size),
        total_bytes: total_blocks.saturating_mul(fragment_size),
        free_inodes,
        total_inodes,
    }
}

/// `statvfs` on `path`, or `None` on a non-Unix OS.
fn stat_fs(path: &std::path::Path) -> Option<FsStats> {
    #[cfg(unix)]
    {
        statvfs_probe::stat(path)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// One-shot health check: one `statvfs(2)` call on the filesystem holding
/// `path`. Returns the outcome enum so the caller can decide what to do with
/// it (the production background task just updates the gauge; tests can
/// assert on the numbers).
///
/// # Performance
/// O(1): one syscall and one small `CString` for the path, no child process
/// and no parsing. It still touches the filesystem, so on a wedged volume it
/// can block like any other call there; the periodic callers run it on the
/// blocking pool.
// TEST-EXEMPT: covered by `test_probe_against_real_path_returns_ok_or_probe_failed` and `test_probe_disk_free_bytes_statvfs_on_tmp_reports_nonzero_total` (different name pattern).
pub fn probe_disk_free_bytes(path: &std::path::Path) -> DiskHealthOutcome {
    match stat_fs(path) {
        Some(stats) => DiskHealthOutcome::Ok {
            free_bytes: stats.free_bytes,
            total_bytes: stats.total_bytes,
        },
        None => DiskHealthOutcome::ProbeFailed {
            reason: DISK_PROBE_FAILED_REASON,
        },
    }
}

/// Free and total INODES of the filesystem holding `path`, from the same
/// `statvfs(2)` call as the byte probe. `None` when the call fails — the byte
/// probe above is the one that decides health; this is the second number the
/// operator has never had.
///
/// Infra sweep row 1 (2026-09-08): the spill and WAL tiers write MANY small
/// files (one NDJSON per rescue, one segment per rotation), and a volume can
/// run out of inodes with hundreds of gigabytes free — every create then
/// fails with ENOSPC while the bytes gauge reads healthy and the free-space
/// alarm stays green. Published as `tv_spill_dir_free_inodes` (local
/// `/metrics` only; no EMF name, no alarm — budget rule) so the number exists
/// before anyone needs it.
///
/// A filesystem that does not track inodes reports a total of 0; that is
/// returned as `None`, never as "0 free".
#[must_use]
pub fn probe_disk_free_inodes(path: &std::path::Path) -> Option<(u64, u64)> {
    let stats = stat_fs(path)?;
    (stats.total_inodes > 0).then_some((stats.free_inodes, stats.total_inodes))
}

// ---------------------------------------------------------------------------
// Hours left at the current fill rate (deep audit DA-12, S3-1, 2026-10-12)
// ---------------------------------------------------------------------------
//
// The free-space threshold above says "low now". It cannot say "full by the
// end of the day while the cold upload is behind", which is the case that
// matters when frame logs are kept until a verified S3 copy exists. So the
// watcher keeps its last hour of free-space readings in a fixed ring and
// projects how long the disk lasts at the rate it has been filling.
//
// Published as `tv_disk_hours_left_at_fill_rate` on local `/metrics` only (no
// EMF name, no alarm, no page: budget rule), and one edge-triggered coded
// `warn!` (`RESOURCE-03`, log-sink-only, `source = "disk_hours_left_low"`)
// when, at that rate, the disk is full before the session ends at
// `TICK_PERSIST_END_SECS_OF_DAY_IST` (15:40 IST), after which nothing more is
// captured and the evening archive frees space. A projection longer than the
// rest of the session is the normal shape of a trading day (the volume is
// sized for one session), so it never logs. The ring is a fixed array written
// once a minute: O(1) per probe, no allocation after the watcher starts.
//
// Honest limits: the rate is the slope between the oldest and newest reading
// of the window, so a cleanup or volume growth inside the hour reads "not
// filling" until it leaves the window; the opening burst can over-project
// for the first hour; and the projection does not know whether the upload is
// failing. A second, upload-aware signal is a separate item.

/// Readings kept for the fill-rate projection: one hour at the 60 s cadence.
const DISK_FILL_RATE_WINDOW_SAMPLES: usize = 60;

/// The window must span at least this long before a rate is projected, so a
/// few noisy readings right after start never produce a figure.
const DISK_FILL_RATE_MIN_SPAN_SECS: u64 = 600;

/// The warning re-arms only once the projection is this many hours longer
/// than the rest of the session, so a projection hovering near the close logs
/// once. A projection that disappears (the disk stopped filling) keeps the
/// latch; the latch clears at the IST day change.
const DISK_HOURS_LEFT_REARM_MARGIN_HOURS: f64 = 2.0;

/// Gauge value when there is no projection (not filling, or too few readings).
const DISK_HOURS_LEFT_NONE_GAUGE: f64 = -1.0;

/// One free-space reading: seconds since the watcher started (monotonic) and
/// the free bytes at that instant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FreeSample {
    at_secs: u64,
    free_bytes: u64,
}

/// A fixed ring of the most recent readings. O(1) push, oldest and newest.
#[derive(Debug, Clone)]
struct DiskFillWindow {
    samples: [FreeSample; DISK_FILL_RATE_WINDOW_SAMPLES],
    len: usize,
    next: usize,
}

impl DiskFillWindow {
    const fn new() -> Self {
        Self {
            samples: [FreeSample {
                at_secs: 0,
                free_bytes: 0,
            }; DISK_FILL_RATE_WINDOW_SAMPLES],
            len: 0,
            next: 0,
        }
    }

    /// Records a reading, overwriting the oldest once the ring is full.
    fn push(&mut self, sample: FreeSample) {
        self.samples[self.next] = sample;
        self.next = (self.next + 1) % DISK_FILL_RATE_WINDOW_SAMPLES;
        if self.len < DISK_FILL_RATE_WINDOW_SAMPLES {
            self.len += 1;
        }
    }

    fn newest(&self) -> Option<FreeSample> {
        (self.len > 0).then(|| {
            self.samples
                [(self.next + DISK_FILL_RATE_WINDOW_SAMPLES - 1) % DISK_FILL_RATE_WINDOW_SAMPLES]
        })
    }

    fn oldest(&self) -> Option<FreeSample> {
        (self.len > 0).then(|| {
            if self.len < DISK_FILL_RATE_WINDOW_SAMPLES {
                self.samples[0]
            } else {
                self.samples[self.next]
            }
        })
    }
}

/// Hours until the disk is full at the rate it filled across the window, or
/// `None` when it is not filling, the window spans under
/// [`DISK_FILL_RATE_MIN_SPAN_SECS`], or the clock did not move forward.
/// Pure and O(1).
fn hours_left(window: &DiskFillWindow) -> Option<f64> {
    let oldest = window.oldest()?;
    let newest = window.newest()?;
    let span_secs = newest.at_secs.checked_sub(oldest.at_secs)?;
    if span_secs < DISK_FILL_RATE_MIN_SPAN_SECS {
        return None;
    }
    let consumed = oldest.free_bytes.checked_sub(newest.free_bytes)?;
    if consumed == 0 {
        return None;
    }
    let bytes_per_sec = consumed as f64 / span_secs as f64;
    Some(newest.free_bytes as f64 / bytes_per_sec / 3_600.0)
}

/// Hours from `unix_secs` until the capture window ends today
/// (`TICK_PERSIST_END_SECS_OF_DAY_IST`, IST), 0 once it has passed. Pure and
/// O(1).
fn hours_until_session_end(unix_secs: u64) -> f64 {
    let ist_secs_of_day = (unix_secs
        + u64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS.unsigned_abs()))
        % u64::from(tickvault_common::constants::SECONDS_PER_DAY);
    let end = u64::from(tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST);
    end.saturating_sub(ist_secs_of_day) as f64 / 3_600.0
}

/// The IST calendar day number of `unix_secs` (days since 1970-01-01 IST).
/// Pure and O(1).
fn ist_day_number(unix_secs: u64) -> u64 {
    (unix_secs + u64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS.unsigned_abs()))
        / u64::from(tickvault_common::constants::SECONDS_PER_DAY)
}

/// The edge-triggered decision for the hours-left warning: `(fire, latched)`.
/// Fires once when the projection is shorter than `session_hours_left` (the
/// disk fills before the capture window ends). Re-arms only once the
/// projection is [`DISK_HOURS_LEFT_REARM_MARGIN_HOURS`] longer than that; no
/// projection keeps the latch as it is. Pure and O(1).
fn hours_left_warn_edge(
    hours: Option<f64>,
    session_hours_left: f64,
    latched: bool,
) -> (bool, bool) {
    match hours {
        Some(h) if h < session_hours_left => (!latched, true),
        Some(h) if h < session_hours_left + DISK_HOURS_LEFT_REARM_MARGIN_HOURS => (false, latched),
        Some(_) => (false, false),
        None => (false, latched),
    }
}

/// Spawn the background watcher task. Idempotent — call once at boot. The
/// returned `JoinHandle` can be aborted on shutdown.
///
/// On non-POSIX systems the watcher logs a warning and the task exits
/// immediately (the gauges remain at zero, which is honest about the lack
/// of a signal).
// TEST-EXEMPT: tokio task spawn — exercised in production by `crates/app/src/main.rs`. The pure functions above (`probe_disk_free_bytes`, `fs_stats_from_fields`, `hours_left`, `hours_left_warn_edge`) are unit-tested, including real `statvfs` probes of the temp dir; this wrapper is a one-line spawn that needs an integration harness to test usefully.
pub fn spawn_spill_disk_health_watcher(spill_dir: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let m_free = metrics::gauge!("tv_spill_dir_free_bytes");
        let m_total = metrics::gauge!("tv_spill_dir_total_bytes");
        let m_free_inodes = metrics::gauge!("tv_spill_dir_free_inodes");
        let m_total_inodes = metrics::gauge!("tv_spill_dir_total_inodes");
        let m_failed = metrics::counter!("tv_spill_dir_health_check_failed_total");
        // DA-12 (S3-1): local `/metrics` only — no EMF name, no alarm.
        let m_hours_left = metrics::gauge!("tv_disk_hours_left_at_fill_rate");
        m_hours_left.set(DISK_HOURS_LEFT_NONE_GAUGE);
        let started = std::time::Instant::now();
        let mut fill_window = DiskFillWindow::new();
        let mut hours_left_latched = false;
        let mut hours_left_day = 0_u64;

        info!(
            path = %spill_dir.display(),
            interval_secs = SPILL_DISK_HEALTH_POLL_INTERVAL_SECS,
            critical_threshold_floor_bytes = SPILL_DISK_FREE_BYTES_CRITICAL_THRESHOLD,
            critical_threshold_divisor = SPILL_DISK_FREE_TARGET_DIVISOR,
            "spill disk-health watcher started — the effective threshold is \
             max(floor, total/divisor), resolved against the volume on every probe"
        );

        // Ensure the dir exists so `statvfs` doesn't fail the probe.
        if let Err(err) = std::fs::create_dir_all(&spill_dir) {
            error!(
                code = tickvault_common::error_code::ErrorCode::Resource03SpillFreeLow.code_str(),
                source = "spill_dir_create",
                ?err,
                path = %spill_dir.display(),
                "could not create spill dir for health watcher"
            );
        }

        let mut ticker =
            tokio::time::interval(Duration::from_secs(SPILL_DISK_HEALTH_POLL_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            // `statvfs` on a stalled or full volume (the moment this probe
            // matters) can hang. So both probes run on the blocking pool,
            // never on a shared tokio worker (O(1) sweep, 2026-10-03). One
            // PathBuf clone a minute.
            let probe_dir = spill_dir.clone();
            let (outcome, inodes) = tokio::task::spawn_blocking(move || {
                (
                    probe_disk_free_bytes(&probe_dir),
                    probe_disk_free_inodes(&probe_dir),
                )
            })
            .await
            .unwrap_or((
                DiskHealthOutcome::ProbeFailed {
                    reason: "probe_task_failed",
                },
                None,
            ));
            match outcome {
                DiskHealthOutcome::Ok {
                    free_bytes,
                    total_bytes,
                } => {
                    m_free.set(free_bytes as f64);
                    m_total.set(total_bytes as f64);
                    if let Some((free_inodes, total_inodes)) = inodes {
                        m_free_inodes.set(free_inodes as f64);
                        m_total_inodes.set(total_inodes as f64);
                    }
                    fill_window.push(FreeSample {
                        at_secs: started.elapsed().as_secs(),
                        free_bytes,
                    });
                    let projected = hours_left(&fill_window);
                    m_hours_left.set(projected.unwrap_or(DISK_HOURS_LEFT_NONE_GAUGE));
                    let now_unix = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs());
                    let today = ist_day_number(now_unix);
                    if today != hours_left_day {
                        hours_left_day = today;
                        hours_left_latched = false;
                    }
                    let session_hours_left = hours_until_session_end(now_unix);
                    let (fire, latched) =
                        hours_left_warn_edge(projected, session_hours_left, hours_left_latched);
                    hours_left_latched = latched;
                    if fire {
                        warn!(
                            code = tickvault_common::error_code::ErrorCode::Resource03SpillFreeLow.code_str(),
                            source = "disk_hours_left_low",
                            path = %spill_dir.display(),
                            hours_left = projected.unwrap_or(DISK_HOURS_LEFT_NONE_GAUGE),
                            free_bytes,
                            total_bytes,
                            session_hours_left,
                            "disk fills before the capture window ends at the rate it filled \
                             over the last hour — check that the cold backup is keeping up"
                        );
                    }
                    // Derived per-probe, never cached: the volume can grow
                    // online (gp3 `modify-volume` is a one-command operation
                    // and was used on 2026-08-25), and a threshold resolved
                    // once at startup would keep warning against the size the
                    // box booted with.
                    let threshold_bytes = spill_disk_free_critical_threshold(total_bytes);
                    if free_bytes < threshold_bytes {
                        error!(
                            code = tickvault_common::error_code::ErrorCode::Resource03SpillFreeLow.code_str(),
                            source = "spill_free_below_threshold",
                            path = %spill_dir.display(),
                            free_bytes,
                            total_bytes,
                            threshold = threshold_bytes,
                            "CRITICAL: spill dir free space below threshold — \
                             tick spill is at risk if QuestDB stays down"
                        );
                    } else {
                        debug!(
                            path = %spill_dir.display(),
                            free_bytes,
                            total_bytes,
                            "spill dir health probe ok"
                        );
                    }
                }
                DiskHealthOutcome::ProbeFailed { reason } => {
                    m_failed.increment(1);
                    error!(
                        code = tickvault_common::error_code::ErrorCode::Resource03SpillFreeLow.code_str(),
                        source = "probe_failed",
                        path = %spill_dir.display(),
                        reason,
                        "spill disk health probe failed — operator has no free-space signal"
                    );
                }
            }
        }
    })
}

/// Backoff between a watcher death and its respawn. Small so disk-free
/// monitoring resumes quickly, but non-zero so a watcher that panics
/// instantly on every start cannot busy-spin the CPU — it respawns at
/// most once per this interval, and the `tv_disk_watcher_respawn_total`
/// counter rate surfaces the flap to the operator via CloudWatch.
pub const DISK_WATCHER_RESPAWN_BACKOFF_SECS: u64 = 5;

/// Classify why a supervised task's `JoinHandle` resolved, into a stable
/// metric label. Pure function so the supervisor's branch logic is unit
/// testable without constructing a real `JoinError` (which has no public
/// constructor).
#[must_use]
pub fn classify_join_exit(join_result: &Result<(), tokio::task::JoinError>) -> &'static str {
    match join_result {
        Ok(()) => "clean_exit",
        Err(e) if e.is_panic() => "panic",
        Err(e) if e.is_cancelled() => "cancelled",
        Err(_) => "unknown",
    }
}

/// G3 (zero-tick-loss audit) — supervise the spill disk-health watcher.
///
/// [`spawn_spill_disk_health_watcher`] runs an infinite probe loop, so its
/// `JoinHandle` resolves ONLY on a fatal event (panic or external cancel).
/// Before this supervisor the handle was bound to `_` in `main.rs`, so a
/// panic made disk-free monitoring vanish silently — and that monitoring is
/// the early-warning for the single highest-risk gap in the zero-loss chain
/// ("disk full + QuestDB down"). This supervisor mirrors the WS-GAP-05 pool
/// supervisor: on every watcher death it logs `error!` (code
/// `DISK-WATCHER-01`) + increments `tv_disk_watcher_respawn_total{reason}`,
/// then respawns after [`DISK_WATCHER_RESPAWN_BACKOFF_SECS`] so monitoring
/// continues. The counter feeds a CloudWatch alarm so the operator is paged
/// on a flapping watcher.
///
/// The returned `JoinHandle` is itself an infinite loop (it never resolves
/// in normal operation); callers bind it to a `_`-prefixed name. The
/// supervisor body has no panic path of its own (no `unwrap`/`expect`,
/// pure-function classification), so it does not need a supervisor-of-the-
/// supervisor.
// O(1) EXEMPT: cold-path supervisor — one task per session, fires only on watcher death.
pub fn spawn_supervised_spill_disk_health_watcher(
    spill_dir: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle = spawn_spill_disk_health_watcher(spill_dir.clone());
            let join_result = handle.await;
            let reason = classify_join_exit(&join_result);
            error!(
                reason,
                code = tickvault_common::error_code::ErrorCode::DiskWatcher01Respawned.code_str(),
                backoff_secs = DISK_WATCHER_RESPAWN_BACKOFF_SECS,
                path = %spill_dir.display(),
                "DISK-WATCHER-01: spill disk-health watcher exited — respawning so \
                 free-space monitoring continues (disk-full + QuestDB-down early warning)"
            );
            metrics::counter!("tv_disk_watcher_respawn_total", "reason" => reason).increment(1);
            tokio::time::sleep(Duration::from_secs(DISK_WATCHER_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_threshold_constant_is_at_least_one_gib() {
        // 1 GiB minimum so the alert has meaningful runway. If a future
        // edit lowers this to a few MB, alerts fire too late.
        assert!(SPILL_DISK_FREE_BYTES_CRITICAL_THRESHOLD >= 1024 * 1024 * 1024);
    }

    /// The 2026-08-25 finding: the fixed 10 GiB was sized as "~10% of the
    /// 100 GB volume" and the volume became 300 GB, so the alert had shrunk
    /// to 3.3% without anyone changing it.
    ///
    /// Bite-proof in BOTH directions — a derived threshold that silently
    /// went DOWN on a small disk would be worse than the constant it
    /// replaced, so the floor is asserted as hard as the scaling is.
    #[test]
    fn derived_threshold_scales_with_the_volume() {
        const GIB: u64 = 1024 * 1024 * 1024;
        const GB: u64 = 1_000_000_000;

        // The live production volume (300 GB gp3, verified 2026-08-25).
        // 10% of it is 30 GB, comfortably above the 10 GiB floor, so the
        // derived value must win.
        assert_eq!(spill_disk_free_critical_threshold(300 * GB), 30 * GB);

        // The 100 GB volume the constant was originally justified against:
        // 10 GB proportional vs a 10 GiB floor — the FLOOR wins, because a
        // GiB is larger than a GB. That is the intended direction (never
        // weaker than today), and it is the case a naive `total/10` would
        // have quietly loosened.
        assert_eq!(
            spill_disk_free_critical_threshold(100 * GB),
            SPILL_DISK_FREE_BYTES_CRITICAL_THRESHOLD
        );

        // A small dev disk keeps the floor rather than collapsing to a
        // threshold that fires after the disk is already unusable.
        assert_eq!(
            spill_disk_free_critical_threshold(20 * GIB),
            SPILL_DISK_FREE_BYTES_CRITICAL_THRESHOLD
        );

        // Degenerate inputs cannot produce a threshold of zero, which would
        // mean "warn when the disk is completely full" — i.e. never warn in
        // time. `probe_disk_free_bytes` should never report 0, but a parse
        // that returned it must not disarm the alarm.
        assert_eq!(
            spill_disk_free_critical_threshold(0),
            SPILL_DISK_FREE_BYTES_CRITICAL_THRESHOLD
        );

        // Monotonic: a bigger volume never earns a smaller warning margin.
        let mut prev = 0_u64;
        for gb in [50_u64, 100, 200, 300, 500, 1000] {
            let t = spill_disk_free_critical_threshold(gb * GB);
            assert!(
                t >= prev,
                "threshold must never shrink as the volume grows ({gb} GB gave {t}, previous {prev})"
            );
            prev = t;
        }
    }

    #[test]
    fn test_poll_interval_is_reasonable() {
        // Too short = wasted CPU; too long = operator surprised by full disk.
        assert!(SPILL_DISK_HEALTH_POLL_INTERVAL_SECS >= 30);
        assert!(SPILL_DISK_HEALTH_POLL_INTERVAL_SECS <= 300);
    }

    const GIB_TEST: u64 = 1024 * 1024 * 1024;

    fn window_of(points: &[(u64, u64)]) -> DiskFillWindow {
        let mut w = DiskFillWindow::new();
        for &(at_secs, free_bytes) in points {
            w.push(FreeSample {
                at_secs,
                free_bytes,
            });
        }
        w
    }

    /// DA-12 (S3-1): a disk losing 10 GiB an hour with 50 GiB left reads
    /// five hours; the ring keeps only the last hour, so an old slow stretch
    /// pushed out of the window no longer dilutes the rate.
    #[test]
    fn hours_left_projection_from_fill_rate() {
        // 50 GiB free after losing 10 GiB in 3,600 s → 5.0 hours.
        let w = window_of(&[(0, 60 * GIB_TEST), (3_600, 50 * GIB_TEST)]);
        let h = hours_left(&w).expect("a filling disk projects a figure");
        assert!((h - 5.0).abs() < 1e-9, "expected 5.0 h, got {h}");

        // A full ring: 120 readings a minute apart, 1 GiB lost per minute.
        // Only the last 60 remain (59 minutes of span, 59 GiB consumed).
        let mut ring = DiskFillWindow::new();
        for i in 0..120u64 {
            ring.push(FreeSample {
                at_secs: i * 60,
                free_bytes: (500 - i) * GIB_TEST,
            });
        }
        assert_eq!(ring.len, DISK_FILL_RATE_WINDOW_SAMPLES);
        assert_eq!(ring.oldest().map(|s| s.at_secs), Some(60 * 60));
        assert_eq!(ring.newest().map(|s| s.at_secs), Some(119 * 60));
        let h = hours_left(&ring).expect("filling");
        // 381 GiB left at 1 GiB/min = 381 min = 6.35 h.
        assert!((h - 381.0 / 60.0).abs() < 1e-9, "got {h}");
    }

    /// DA-12 (S3-1): no projection when the disk is flat or gaining space,
    /// when the window is shorter than the minimum span, when empty, or when
    /// the clock did not move forward.
    #[test]
    fn hours_left_none_when_disk_not_filling() {
        assert_eq!(hours_left(&DiskFillWindow::new()), None);
        assert_eq!(hours_left(&window_of(&[(0, GIB_TEST)])), None);
        // Flat.
        assert_eq!(
            hours_left(&window_of(&[(0, 9 * GIB_TEST), (3_600, 9 * GIB_TEST)])),
            None
        );
        // Gaining (cleanup or volume growth).
        assert_eq!(
            hours_left(&window_of(&[(0, 9 * GIB_TEST), (3_600, 20 * GIB_TEST)])),
            None
        );
        // Filling, but the span is under the minimum.
        assert_eq!(
            hours_left(&window_of(&[
                (0, 9 * GIB_TEST),
                (DISK_FILL_RATE_MIN_SPAN_SECS - 1, 8 * GIB_TEST)
            ])),
            None
        );
        // Exactly the minimum span projects.
        assert!(
            hours_left(&window_of(&[
                (0, 9 * GIB_TEST),
                (DISK_FILL_RATE_MIN_SPAN_SECS, 8 * GIB_TEST)
            ]))
            .is_some()
        );
        // Zero free left reads zero hours, never a division by zero.
        assert_eq!(
            hours_left(&window_of(&[(0, GIB_TEST), (3_600, 0)])),
            Some(0.0)
        );
    }

    /// DA-12 (S3-1): the warning fires once on the way under a day, stays
    /// quiet while latched (including the band between 24 h and 30 h), and
    /// re-arms only above 30 h or when there is no projection.
    #[test]
    fn hours_left_warn_is_edge_triggered_with_hysteresis() {
        // Six session hours left (09:40 IST).
        assert_eq!(hours_left_warn_edge(None, 6.0, false), (false, false));
        assert_eq!(hours_left_warn_edge(Some(48.0), 6.0, false), (false, false));
        assert_eq!(hours_left_warn_edge(Some(5.0), 6.0, false), (true, true));
        assert_eq!(hours_left_warn_edge(Some(1.0), 6.0, true), (false, true));
        assert_eq!(hours_left_warn_edge(Some(7.0), 6.0, true), (false, true));
        assert_eq!(hours_left_warn_edge(Some(7.0), 6.0, false), (false, false));
        assert_eq!(hours_left_warn_edge(Some(8.5), 6.0, true), (false, false));
        // No projection (the disk stopped filling) keeps the latch either way.
        assert_eq!(hours_left_warn_edge(None, 6.0, true), (false, true));
        assert_eq!(hours_left_warn_edge(Some(0.0), 6.0, false), (true, true));
        // After the close only a disk full within the margin keeps it latched.
        assert_eq!(hours_left_warn_edge(Some(0.5), 0.0, true), (false, true));
        assert_eq!(hours_left_warn_edge(Some(3.0), 0.0, true), (false, false));
    }

    #[test]
    fn hours_left_warn_stays_quiet_on_a_normal_session_fill() {
        // The volume is sized for one session: a 6.5-hour projection at 09:30
        // IST (6h10m of capture left) is a normal day and must not log, while
        // the same rate one hour later with less disk left (5h projected,
        // 5h10m of capture left) must.
        assert_eq!(
            hours_left_warn_edge(Some(6.5), 6.0 + 10.0 / 60.0, false),
            (false, false)
        );
        assert_eq!(
            hours_left_warn_edge(Some(5.0), 5.0 + 10.0 / 60.0, false),
            (true, true)
        );
    }

    #[test]
    fn hours_until_session_end_tracks_the_ist_clock() {
        // 2026-10-12 04:10 UTC = 09:40 IST, six hours before 15:40.
        let at_0940_ist = 1_791_778_200_u64;
        assert!((hours_until_session_end(at_0940_ist) - 6.0).abs() < 1e-9);
        // 15:40 IST and later read 0.
        assert_eq!(hours_until_session_end(at_0940_ist + 6 * 3_600), 0.0);
        assert_eq!(hours_until_session_end(at_0940_ist + 8 * 3_600), 0.0);
        // The IST day changes at 18:30 UTC, not at UTC midnight.
        assert_eq!(
            ist_day_number(at_0940_ist),
            ist_day_number(at_0940_ist + 14 * 3_600 - 1)
        );
        assert_eq!(
            ist_day_number(at_0940_ist) + 1,
            ist_day_number(at_0940_ist + 14 * 3_600 + 1_200)
        );
    }

    /// The byte arithmetic is a multiply of two kernel fields; a fabricated
    /// block size must saturate, never wrap into a small number.
    #[test]
    fn test_fs_stats_from_fields_multiplies_and_saturates() {
        let s = fs_stats_from_fields(800, 1_000, 4_096, 7, 9);
        assert_eq!(s.free_bytes, 800 * 4_096);
        assert_eq!(s.total_bytes, 1_000 * 4_096);
        assert_eq!((s.free_inodes, s.total_inodes), (7, 9));
        let huge = fs_stats_from_fields(u64::MAX, u64::MAX, 4_096, 0, 0);
        assert_eq!(huge.free_bytes, u64::MAX);
        assert_eq!(huge.total_bytes, u64::MAX);
        assert_eq!(fs_stats_from_fields(5, 5, 0, 0, 0).total_bytes, 0);
    }

    /// DA-12 (2026-10-12): on a real Unix filesystem the probe answers from
    /// `statvfs` with a non-zero total and free never above it.
    #[cfg(unix)]
    #[test]
    fn test_probe_disk_free_bytes_statvfs_on_tmp_reports_nonzero_total() {
        let tmp = std::env::temp_dir();
        match probe_disk_free_bytes(&tmp) {
            DiskHealthOutcome::Ok {
                free_bytes,
                total_bytes,
            } => {
                assert!(total_bytes > 0, "a mounted filesystem has a size");
                assert!(
                    free_bytes <= total_bytes,
                    "free {free_bytes} > total {total_bytes}"
                );
            }
            DiskHealthOutcome::ProbeFailed { reason } => {
                panic!("statvfs on the temp dir must succeed on Unix, got {reason}") // APPROVED: test
            }
        }
    }

    /// A path that does not exist is a failed probe with the one reason
    /// string, never a fabricated zero (which would read as "disk full").
    #[test]
    fn test_probe_disk_free_bytes_missing_path_is_probe_failed() {
        assert_eq!(
            probe_disk_free_bytes(std::path::Path::new("/definitely/not/a/mount/point")),
            DiskHealthOutcome::ProbeFailed {
                reason: DISK_PROBE_FAILED_REASON
            }
        );
    }

    /// A path with an interior NUL byte cannot be handed to the kernel; it is
    /// a failed probe, never a panic.
    #[cfg(unix)]
    #[test]
    fn test_probe_disk_free_bytes_interior_nul_is_probe_failed() {
        use std::os::unix::ffi::OsStrExt;
        let p = std::path::Path::new(std::ffi::OsStr::from_bytes(b"/tmp/a\0b"));
        assert!(matches!(
            probe_disk_free_bytes(p),
            DiskHealthOutcome::ProbeFailed { .. }
        ));
        assert!(probe_disk_free_inodes(p).is_none());
    }

    /// DA-12: the inode probe answers from the same `statvfs` call.
    #[cfg(unix)]
    #[test]
    fn test_probe_disk_free_inodes_statvfs_on_tmp() {
        // tmpfs and most real filesystems track inodes; one that does not
        // reports a total of 0, which the probe turns into None.
        if let Some((free, total)) = probe_disk_free_inodes(&std::env::temp_dir()) {
            assert!(total > 0);
            assert!(free <= total, "free {free} > total {total}");
        }
    }

    /// DA-12 ratchet: the probe never starts a child process again. The
    /// production half of this file (above the test module) must not name
    /// `Command` at all.
    #[test]
    fn test_disk_probe_never_forks_df() {
        let src = include_str!("disk_health_watcher.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src); // APPROVED: test
        assert!(
            !production.contains("Command::new"),
            "disk_health_watcher must read free space with statvfs, not by starting a process"
        );
        assert!(
            production.contains("libc::statvfs("),
            "the statvfs call must stay"
        );
    }

    #[test]
    fn probe_disk_free_inodes_never_fabricates_a_reading() {
        // A path that does not exist yields nothing; a real mount yields a
        // free count bounded by its total.
        assert!(
            probe_disk_free_inodes(std::path::Path::new("/definitely/not/a/mount/point")).is_none()
        );
        if let Some((free, total)) = probe_disk_free_inodes(std::path::Path::new("/tmp")) {
            assert!(total > 0);
            assert!(free <= total, "free {free} > total {total}");
        }
    }

    #[test]
    fn test_probe_against_real_path_returns_ok_or_probe_failed() {
        // We can't assert specific numbers (CI machines vary), but on a
        // Unix runner `statvfs` succeeds against `/tmp`. On a hypothetical
        // non-Unix runner this returns ProbeFailed; either outcome is valid —
        // the test just exercises the codepath. On a real filesystem the free
        // inode count can never exceed the total.
        if let Some((free_inodes, total_inodes)) =
            probe_disk_free_inodes(std::path::Path::new("/tmp"))
        {
            assert!(
                total_inodes > 0,
                "a mounted filesystem reports its inode total"
            );
            assert!(
                free_inodes <= total_inodes,
                "free {free_inodes} > total {total_inodes}"
            );
        }
        assert!(
            probe_disk_free_inodes(std::path::Path::new("/definitely/not/a/mount/point")).is_none(),
            "a path that does not exist yields no inode reading, never a fabricated one"
        );
        let outcome = probe_disk_free_bytes(std::path::Path::new("/tmp"));
        match outcome {
            DiskHealthOutcome::Ok {
                free_bytes,
                total_bytes,
            } => {
                assert!(total_bytes > 0, "real /tmp must report non-zero total");
                assert!(
                    free_bytes <= total_bytes,
                    "free must not exceed total on a sane FS"
                );
            }
            DiskHealthOutcome::ProbeFailed { reason } => {
                assert!(!reason.is_empty(), "failure must carry a reason string");
            }
        }
    }

    // -- G3 supervisor (spawn_supervised_spill_disk_health_watcher) --

    #[test]
    fn test_respawn_backoff_is_small_but_nonzero() {
        // Non-zero so a tight panic loop can't busy-spin the CPU; small so
        // disk-free monitoring resumes within seconds of a watcher death.
        assert!(DISK_WATCHER_RESPAWN_BACKOFF_SECS >= 1);
        assert!(DISK_WATCHER_RESPAWN_BACKOFF_SECS <= 30);
    }

    #[tokio::test]
    async fn test_classify_join_exit_clean() {
        let h = tokio::spawn(async {});
        let r = h.await;
        assert_eq!(classify_join_exit(&r), "clean_exit");
    }

    #[tokio::test]
    async fn test_classify_join_exit_panic() {
        // A panicking task yields a JoinError where is_panic() == true.
        let h = tokio::spawn(async {
            panic!("intentional test panic"); // APPROVED: test — exercises the panic branch
        });
        let r = h.await;
        assert_eq!(classify_join_exit(&r), "panic");
    }

    #[tokio::test]
    async fn test_classify_join_exit_cancelled() {
        // An aborted task yields a JoinError where is_cancelled() == true.
        let h = tokio::spawn(async {
            // Sleep long enough that abort lands before completion.
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        h.abort();
        let r = h.await;
        assert_eq!(classify_join_exit(&r), "cancelled");
    }

    #[tokio::test]
    async fn test_spawn_supervised_spill_disk_health_watcher_keeps_running() {
        // The supervisor is an infinite loop — its JoinHandle must NOT
        // resolve in normal operation. The inner watcher it spawns also
        // loops forever (60s probe interval), so the supervisor parks on
        // `handle.await` and never completes. (If a future edit makes the
        // supervisor return after one watcher death instead of respawning,
        // this guard fails.)
        let handle = spawn_supervised_spill_disk_health_watcher(std::path::PathBuf::from(
            "data/spill-supervisor-test",
        ));
        // Let the spawned task make progress.
        tokio::task::yield_now().await;
        assert!(
            !handle.is_finished(),
            "supervisor must keep running, not exit after spawning the watcher"
        );
        handle.abort();
    }
}
