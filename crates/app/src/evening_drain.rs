//! The evening WAL drain (operator Quote 31, 2026-10-21): "every waiting WAL
//! segment is replayed" the same evening.
//!
//! A WAL segment the replay still has to read (one the held unapplied ranges
//! pin: frames from an earlier process that never reached the database, or
//! this session's frames acked while QuestDB was untrusted) is replayed only
//! by a boot's catch-up drain, which runs before the sockets dial. So after
//! the close, once the day's own work is done (and the day's 1-minute
//! cross-verification has run, or 16:25 IST has passed; 17:25 until 2026-10-22), the drain-hold
//! supervisor asks this process for a planned restart. systemd's
//! `Restart=always` starts it again three seconds later and its catch-up
//! replays the backlog.
//!
//! # Bounds
//!
//! - At most [`EVENING_DRAIN_RESTARTS_MAX`] restarts an evening,
//!   [`EVENING_DRAIN_SPACING_SECS`] (40 minutes) apart, so at most two fall in
//!   any hour and, with a deploy restart, the unit keeps headroom under its
//!   `StartLimitBurst=5` per hour.
//! - Each restart's own catch-up is held for the whole spacing, whatever the
//!   cap or the clock says; only then is progress judged. A restart after
//!   which the backlog did not shrink ends the evening's restarts, and that
//!   verdict is written down so it cannot flip back.
//! - None after [`EVENING_DRAIN_LAST_RESTART_SECS_IST`] (05:00 IST), so the
//!   last one is judged well before the 07:30 hold ceiling.
//! - The restarted boot keeps the round cap, the memory stop and the disk
//!   floor; its clock is [`EVENING_DRAIN_CATCHUP_BUDGET_SECS`] instead of five
//!   minutes, so lag pauses do not end it early, and (PR RD-2, 2026-11-08,
//!   owner Quote 41) its replay frame cap is
//!   `EVENING_DRAIN_REPLAY_FRAME_CAP`, 5 x `WAL_REPLAY_MAX_FRAMES_PER_BOOT`
//!   (15,000,000), raised once before the first catch-up round. Before, it
//!   kept the 3,000,000 cap, which the boot pass alone (about 3.5 million
//!   frames) already passed, so the longer clock bought nothing.
//!
//! # State
//!
//! One small file, [`EVENING_DRAIN_STATE_PATH`], survives the restart: the
//! evening it belongs to, restarts so far, the last restart instant, the
//! backlog then, whether the next boot is the evening drain boot, and whether
//! the evening has given up. A missing file reads as "no restart yet"; a
//! damaged or unreadable one stops the restarts (it could not be counted).
//! New records carry a CRC. Unchecksummed v1 records are never trusted for
//! decisions: they block their current evening, then the supervisor can
//! quarantine them using only a strictly older filesystem modification time.
//! A current/future-dated damaged file stays closed. Quarantine preserves the
//! original bytes; it does not infer a day from an untrusted record.
//! Cold: read once a minute after the close and once at boot. The restarted
//! app sends its usual boot message; the shutdown message is not sent.

use std::fs::{File, Metadata};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use tickvault_common::error_code::ErrorCode;
use tracing::{error, info};

/// The state file, relative to the app's working directory.
pub const EVENING_DRAIN_STATE_PATH: &str = "data/state/evening-drain.state";
// All fixed-width integer fields plus framing fit comfortably inside this
// bound. Applied BEFORE reading or parsing, including corrupt input.
const STATE_MAX_BYTES: usize = 256;
const STATE_CRC_HEX_DIGITS: usize = 8;
const CRC32_IEEE_POLYNOMIAL: u32 = 0xedb8_8320;
const BITS_PER_BYTE: usize = 8;
const SECONDS_PER_DAY: i64 = 86_400;
const IST_OFFSET_SECS: i64 = 19_800;
// Boot's pending clear and the supervisor share this lock. The state directory
// has one owning process; independent external writers are not coordinated.
static STATE_IO: Mutex<()> = Mutex::new(());
/// Planned restarts allowed in one evening.
pub const EVENING_DRAIN_RESTARTS_MAX: u32 = 3;
/// Least time between two restarts, and from a restart to its progress
/// check: the old process's shutdown, the boot (up to five minutes waiting
/// for the token), the 15-minute catch-up, and a few minutes for the
/// replayed segments to settle (hostile review, 2026-10-21: 20 minutes was
/// shorter than that).
pub const EVENING_DRAIN_SPACING_SECS: i64 = 2_400;
/// 15:50 IST: no restart before this (the capture ends at 15:40).
pub const EVENING_DRAIN_FIRST_SECS_IST: u64 = 15 * 3_600 + 50 * 60;
/// 05:00 IST: no restart after this.
pub const EVENING_DRAIN_LAST_RESTART_SECS_IST: u64 = 5 * 3_600;
/// 12:00 IST: from here to [`EVENING_DRAIN_FIRST_SECS_IST`] the evening's
/// restarts are still ahead (wait); from 05:00 to here they are over.
pub const EVENING_DRAIN_AFTERNOON_SECS_IST: u64 = 12 * 3_600;
const _: () = assert!(
    EVENING_DRAIN_LAST_RESTART_SECS_IST < EVENING_DRAIN_AFTERNOON_SECS_IST
        && EVENING_DRAIN_AFTERNOON_SECS_IST < EVENING_DRAIN_FIRST_SECS_IST
);
/// 16:25 IST: past the day's last 1-minute cross-verification attempt
/// (it ends by 16:23), so a restart can no longer cut one short. Derived from
/// the scheduled-stop window, which the cross-verification end bound is
/// derived from too (2026-10-22, plan item 64, Quote 37 and §12.15.12: was
/// 17:25 with the 17:30 stop).
pub const EVENING_DRAIN_XVERIFY_CLEAR_SECS_IST: u64 =
    crate::shutdown_class::SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST as u64;

const _: () = assert!(
    crate::dhan_live_crossverify_boot::XVERIFY_LAST_END_SECS_OF_DAY_IST
        < EVENING_DRAIN_XVERIFY_CLEAR_SECS_IST,
    "the evening drain may restart only after the last cross-verification attempt ends"
);
/// The restarted boot's catch-up clock, in seconds.
pub const EVENING_DRAIN_CATCHUP_BUDGET_SECS: u64 = 900;
/// A pending flag older than this is ignored (a restart that never came).
pub const EVENING_DRAIN_PENDING_MAX_AGE_SECS: i64 = 900;

const _: () = assert!(EVENING_DRAIN_CATCHUP_BUDGET_SECS < EVENING_DRAIN_SPACING_SECS as u64);
const _: () = assert!(
    EVENING_DRAIN_LAST_RESTART_SECS_IST + (EVENING_DRAIN_SPACING_SECS as u64)
        < crate::drain_hold::DRAIN_HOLD_CEILING_SECS_IST
);
const _: () = assert!(EVENING_DRAIN_FIRST_SECS_IST > crate::drain_hold::DRAIN_HOLD_START_SECS_IST);
// At most two restarts start in any hour, leaving systemd (5 an hour) room
// for a deploy and a crash.
const _: () = assert!(3_600 / EVENING_DRAIN_SPACING_SECS < 2);

static SELF_RESTART: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Asks `main` for a planned restart. One stored permit, so a request made
/// while no shutdown wait is running is taken by the next one. O(1).
pub fn request_self_restart() {
    SELF_RESTART.notify_one();
}

/// Resolves when [`request_self_restart`] was called. Cancel-safe.
pub async fn self_restart_requested() {
    SELF_RESTART.notified().await;
}

/// What survives a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EveningState {
    /// The evening (its trading day, days from the common era); 0 = none.
    pub day: i32,
    /// Restarts made that evening.
    pub restarts: u32,
    /// When the last one was requested (unix seconds).
    pub last_unix: i64,
    /// The backlog (segments) when it was requested.
    pub backlog_before: u64,
    /// When the next boot was marked as the evening drain boot; 0 = not.
    pub pending_unix: i64,
    /// The evening gave up (cap, window or no progress): no more restarts.
    pub gave_up: bool,
}

impl EveningState {
    /// `v2 day restarts last backlog pending gave_up crc32`. Pure, bounded.
    /// CRC detects accidental damage; it does not authenticate deliberate edits.
    #[must_use]
    pub fn to_line(&self) -> String {
        let payload = format!(
            "v2 {} {} {} {} {} {}",
            self.day,
            self.restarts,
            self.last_unix,
            self.backlog_before,
            self.pending_unix,
            u8::from(self.gave_up)
        );
        format!("{payload} {:08x}\n", state_crc32(payload.as_bytes()))
    }

    /// The inverse of [`Self::to_line`]; anything else is `None`. Pure.
    #[must_use]
    pub fn from_line(line: &str) -> Option<Self> {
        if line.len() > STATE_MAX_BYTES {
            return None;
        }
        let line = line.strip_suffix('\n').unwrap_or(line);
        let (payload, checksum) = line.rsplit_once(' ')?;
        if checksum.len() != STATE_CRC_HEX_DIGITS
            || !checksum
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || u32::from_str_radix(checksum, 16).ok()? != state_crc32(payload.as_bytes())
        {
            return None;
        }
        let mut it = payload.split_ascii_whitespace();
        // v1 cannot detect valid-digit corruption and must not drive a cap,
        // give-up latch or pending flag during migration.
        if it.next()? != "v2" {
            return None;
        }
        let state = Self {
            day: it.next()?.parse().ok()?,
            restarts: it.next()?.parse().ok()?,
            last_unix: it.next()?.parse().ok()?,
            backlog_before: it.next()?.parse().ok()?,
            pending_unix: it.next()?.parse().ok()?,
            gave_up: match it.next()? {
                "0" => false,
                "1" => true,
                _ => return None,
            },
        };
        it.next().is_none().then_some(state)
    }
}

// IEEE CRC-32 over at most STATE_MAX_BYTES, once per cold state read/write.
// Kept local rather than widening a storage-internal WAL checksum API.
fn state_crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..BITS_PER_BYTE {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (CRC32_IEEE_POLYNOMIAL & mask);
        }
    }
    !crc
}

enum StateRead {
    Missing,
    Valid(EveningState),
    Invalid(Metadata),
}

fn regular_metadata(path: &Path) -> std::io::Result<Metadata> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::other("evening state is not a regular file"));
    }
    Ok(metadata)
}

fn unchanged_file(before: &Metadata, after: &Metadata) -> bool {
    if !after.file_type().is_file()
        || before.len() != after.len()
        || before
            .modified()
            .ok()
            .zip(after.modified().ok())
            .is_none_or(|(a, b)| a != b)
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        before.dev() == after.dev() && before.ino() == after.ino()
    }
    #[cfg(not(unix))]
    {
        // Automatic archival requires file identity, implemented for both
        // supported runtimes (macOS and Linux). Other platforms fail closed.
        false
    }
}

fn read_state(path: &Path) -> std::io::Result<StateRead> {
    let before = match regular_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(StateRead::Missing),
        Err(err) => return Err(err),
    };
    let mut file = File::open(path)?;
    let opened = file.metadata()?;
    if !unchanged_file(&before, &opened) {
        return Err(std::io::Error::other("evening state changed while opening"));
    }
    let mut bytes = Vec::with_capacity(STATE_MAX_BYTES + 1);
    (&mut file)
        .take((STATE_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if !unchanged_file(&opened, &after) {
        return Err(std::io::Error::other("evening state changed while reading"));
    }
    let state = std::str::from_utf8(&bytes)
        .ok()
        .and_then(EveningState::from_line);
    Ok(match state {
        Some(state) => StateRead::Valid(state),
        None => StateRead::Invalid(after),
    })
}

/// Reads the state: the default when the file does not exist, `None` when it
/// cannot be read or parsed (the caller then makes no restart: a count it
/// cannot read is a cap it cannot keep).
#[must_use]
pub fn load_state(path: &Path) -> Option<EveningState> {
    let _guard = STATE_IO.lock().ok()?;
    match read_state(path).ok()? {
        StateRead::Missing => Some(EveningState::default()),
        StateRead::Valid(state) => Some(state),
        StateRead::Invalid(_) => None,
    }
}

fn evening_start_unix(now_unix: i64) -> Option<i64> {
    if now_unix < 0 {
        return None;
    }
    let local = now_unix.checked_add(IST_OFFSET_SECS)?;
    let day = local
        .div_euclid(SECONDS_PER_DAY)
        .checked_mul(SECONDS_PER_DAY)?;
    let start = day
        .checked_add(i64::try_from(crate::drain_hold::DRAIN_HOLD_START_SECS_IST).ok()?)?
        .checked_sub(IST_OFFSET_SECS)?;
    let start = if start > now_unix {
        start.checked_sub(SECONDS_PER_DAY)?
    } else {
        start
    };
    (start >= 0).then_some(start)
}

fn quarantine_directory(path: &Path, start_unix: i64) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".bad-{start_unix}"));
    PathBuf::from(name)
}

fn archive_untrusted_state(
    path: &Path,
    expected: &Metadata,
    start_unix: i64,
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    if !unchanged_file(expected, &regular_metadata(path)?) {
        return Err(std::io::Error::other(
            "evening state changed before quarantine",
        ));
    }
    let quarantine = quarantine_directory(path, start_unix);
    // create_dir reserves the destination without overwriting evidence.
    std::fs::create_dir(&quarantine)?;
    rename(path, &quarantine.join("state"))?;
    Ok(quarantine)
}

/// Reads the supervisor's evening state and archives only old, untrusted
/// regular files. A damaged/legacy file modified at or after this evening's
/// 15:40 IST start stays closed. Embedded dates are never trusted.
///
/// Original bytes go to a newly reserved `.bad-<start>/state` directory;
/// a collision or any I/O failure refuses recovery. Mac/Linux file identity
/// checks and the process-local lock protect cooperating app accesses. They
/// do not make an atomic compare-and-rename against independent external writers.
#[must_use]
pub fn load_state_for_evening(path: &Path, now_unix: i64) -> Option<EveningState> {
    let _guard = STATE_IO.lock().ok()?;
    let start = evening_start_unix(now_unix)?;
    match read_state(path).ok()? {
        StateRead::Missing => Some(EveningState::default()),
        StateRead::Valid(state) => Some(state),
        StateRead::Invalid(metadata) => {
            let cutoff = SystemTime::UNIX_EPOCH
                .checked_add(Duration::from_secs(u64::try_from(start).ok()?))?;
            if metadata.modified().ok()? >= cutoff {
                return None;
            }
            let quarantine = archive_untrusted_state(path, &metadata, start, |from, to| {
                std::fs::rename(from, to)
            })
            .ok()?;
            info!(path = %path.display(), quarantine = %quarantine.display(),
                "evening drain: preserved an old untrusted state; this evening starts fresh");
            Some(EveningState::default())
        }
    }
}

/// Writes and syncs a v2 temp file, renames it, then syncs its parent.
/// This propagates durability errors; it is not a power-loss guarantee for
/// every filesystem, storage device or external writer.
///
/// # Errors
/// The write or the rename failed.
pub fn store_state(path: &Path, state: &EveningState) -> std::io::Result<()> {
    let _guard = STATE_IO
        .lock()
        .map_err(|_| std::io::Error::other("evening state lock poisoned"))?;
    store_state_locked(path, state)
}

fn store_state_locked(path: &Path, state: &EveningState) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    match std::fs::symlink_metadata(&tmp) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(std::io::Error::other("evening temp is not a regular file"));
        }
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err),
        _ => {}
    }
    let mut file = File::create(&tmp)?;
    file.write_all(state.to_line().as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(parent)?.sync_all()
}

/// Whether a restart may be requested at `secs_ist` (15:50 to 05:00, wrapping
/// midnight). O(1).
#[must_use]
pub const fn in_restart_window(secs_ist: u64) -> bool {
    secs_ist >= EVENING_DRAIN_FIRST_SECS_IST || secs_ist < EVENING_DRAIN_LAST_RESTART_SECS_IST
}

/// One evening decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EveningStep {
    /// No backlog, or it could not be read: nothing to do, nothing held.
    Idle,
    /// A backlog, and a restart will come (the day's own work, or the spacing,
    /// is still in the way): it holds the box.
    Wait,
    /// Restart now: it holds the box.
    Restart,
    /// No more restarts tonight (cap, window, or no progress): it no longer
    /// holds the box; the next boot replays it as before.
    GaveUp,
}

impl EveningStep {
    /// Whether the backlog keeps the drain hold. O(1).
    #[must_use]
    pub const fn holds(self) -> bool {
        matches!(self, Self::Wait | Self::Restart)
    }
}

/// Whether the day's 1-minute cross-verification can no longer be cut short
/// by a restart: its day marker exists, or it is past 16:25 IST (its last
/// attempt ends by 16:23), including the hours after midnight. O(1).
#[must_use]
pub const fn crossverify_settled(marker_written: bool, secs_ist: u64) -> bool {
    marker_written
        || secs_ist >= EVENING_DRAIN_XVERIFY_CLEAR_SECS_IST
        || secs_ist < crate::drain_hold::DRAIN_HOLD_CEILING_SECS_IST
}

/// Decides the evening's next step. `other_done` is the day's own work done
/// (everything but the backlog); `xverify_settled` is
/// [`crossverify_settled`]; `backlog` is the pinned segment count, `None`
/// when unreadable. Pure, O(1).
///
/// Order matters (hostile review, 2026-10-21): a restart's own catch-up is
/// held for the whole spacing before the cap or the clock can end the
/// evening, and progress is judged only after it.
#[must_use]
pub fn evening_step(
    state: &EveningState,
    day: i32,
    other_done: bool,
    xverify_settled: bool,
    backlog: Option<u64>,
    now_unix: i64,
    secs_ist: u64,
) -> EveningStep {
    let tonight = state.day == day;
    if tonight && state.gave_up {
        return EveningStep::GaveUp;
    }
    let restarts = if tonight { state.restarts } else { 0 };
    let since_last = now_unix.saturating_sub(state.last_unix);
    let catching_up = restarts > 0 && since_last < EVENING_DRAIN_SPACING_SECS;
    let backlog = match backlog {
        // A scan that raced the catch-up's segment moves keeps the hold.
        None if catching_up => return EveningStep::Wait,
        None | Some(0) => return EveningStep::Idle,
        Some(n) => n,
    };
    if catching_up {
        return EveningStep::Wait;
    }
    if restarts > 0 && backlog >= state.backlog_before {
        return EveningStep::GaveUp;
    }
    if restarts >= EVENING_DRAIN_RESTARTS_MAX {
        return EveningStep::GaveUp;
    }
    if !in_restart_window(secs_ist) {
        // M2 (2026-10-26): the supervisor starts at 15:40, before the 15:50
        // window. Giving up there latched the verdict for the whole evening,
        // so 26 Oct made no restart at all (15:40:57, restarts=0). Before the
        // window on the same afternoon the restart is still to come.
        if secs_ist >= EVENING_DRAIN_AFTERNOON_SECS_IST {
            return EveningStep::Wait;
        }
        return EveningStep::GaveUp;
    }
    if !other_done || !xverify_settled {
        return EveningStep::Wait;
    }
    EveningStep::Restart
}

/// The state to write for a restart requested now. Pure.
#[must_use]
pub fn state_for_restart(
    state: &EveningState,
    day: i32,
    backlog: u64,
    now_unix: i64,
) -> EveningState {
    let restarts = if state.day == day { state.restarts } else { 0 };
    EveningState {
        day,
        restarts: restarts.saturating_add(1),
        last_unix: now_unix,
        backlog_before: backlog,
        pending_unix: now_unix,
        gave_up: false,
    }
}

/// The state to write when the evening gives up, so the verdict cannot flip
/// back while the backlog count moves. Pure.
#[must_use]
pub fn state_for_give_up(state: &EveningState, day: i32) -> EveningState {
    if state.day == day {
        EveningState {
            gave_up: true,
            ..*state
        }
    } else {
        EveningState {
            day,
            gave_up: true,
            ..EveningState::default()
        }
    }
}

/// What this boot's WAL catch-up may spend: its clock and its replay frame
/// cap. Decided once per boot by [`catchup_allowance_for`] (PR RD-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatchupAllowance {
    /// The catch-up clock, in seconds.
    pub budget_secs: u64,
    /// Frames this boot may replay across STAGE-C and every catch-up round.
    pub frame_cap: u64,
}

/// The catch-up allowance for this boot. On the evening drain boot (a fresh
/// pending flag, inside the restart window or the hour after it): the clock
/// is at least [`EVENING_DRAIN_CATCHUP_BUDGET_SECS`] and the frame cap is
/// `EVENING_DRAIN_REPLAY_FRAME_CAP` (5 x `WAL_REPLAY_MAX_FRAMES_PER_BOOT`,
/// 15,000,000, owner Quote 41). Any other boot: `standard` seconds and the
/// standard 3,000,000 frames. Pure.
#[must_use]
pub fn catchup_allowance_for(
    state: &EveningState,
    standard: u64,
    now_unix: i64,
    secs_ist: u64,
) -> CatchupAllowance {
    let age = now_unix.saturating_sub(state.pending_unix);
    let fresh = state.pending_unix != 0 && (0..=EVENING_DRAIN_PENDING_MAX_AGE_SECS).contains(&age);
    let evening = in_restart_window(secs_ist)
        || secs_ist
            < EVENING_DRAIN_LAST_RESTART_SECS_IST + EVENING_DRAIN_PENDING_MAX_AGE_SECS as u64;
    if fresh && evening {
        CatchupAllowance {
            budget_secs: standard.max(EVENING_DRAIN_CATCHUP_BUDGET_SECS),
            frame_cap: tickvault_storage::wal_replay_frame_cap::EVENING_DRAIN_REPLAY_FRAME_CAP,
        }
    } else {
        standard_allowance(standard)
    }
}

/// `standard` seconds and the standard per-boot frame cap. Pure.
const fn standard_allowance(standard: u64) -> CatchupAllowance {
    CatchupAllowance {
        budget_secs: standard,
        frame_cap: tickvault_storage::ws_frame_spill::WAL_REPLAY_MAX_FRAMES_PER_BOOT,
    }
}

/// [`catchup_allowance_for`] on the state file, clearing its pending flag so
/// it applies to one boot only. Called once per lane start, cold.
#[must_use]
// TEST-EXEMPT: file I/O around catchup_allowance_for, which is unit-tested (test_catchup_allowance_for_*) and on store_state/load_state; covered by allowance_or_applies_to_one_boot_and_fails_closed_on_a_bad_file.
pub fn allowance_or(standard: u64) -> CatchupAllowance {
    let path = Path::new(EVENING_DRAIN_STATE_PATH);
    // Keep the read/clear transaction inside one lock: an old boot snapshot
    // must not overwrite a newer supervisor restart count when clearing pending.
    let Ok(_guard) = STATE_IO.lock() else {
        return standard_allowance(standard);
    };
    let Ok(StateRead::Valid(state)) = read_state(path) else {
        return standard_allowance(standard);
    };
    if state.pending_unix == 0 {
        return standard_allowance(standard);
    }
    let allowance = catchup_allowance_for(
        &state,
        standard,
        chrono::Utc::now().timestamp(),
        crate::dhan_feed_stack::now_ist_secs_of_day(),
    );
    let cleared = EveningState {
        pending_unix: 0,
        ..state
    };
    if let Err(err) = store_state_locked(path, &cleared) {
        error!(
            code = ErrorCode::DrainHold01NotWritten.code_str(),
            source = "evening_drain_state",
            error = %err,
            "evening drain: the pending flag could not be cleared; it ages out in 15 minutes"
        );
    }
    if allowance != standard_allowance(standard) {
        info!(
            budget_secs = allowance.budget_secs,
            frame_cap = allowance.frame_cap,
            restart = state.restarts,
            backlog_before = state.backlog_before,
            "evening drain boot: the WAL catch-up takes the longer evening clock and the \
             evening frame cap"
        );
    }
    allowance
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i32 = 739_910;
    const NOW: i64 = 1_792_000_000;
    /// 18:00 IST: after the close and past the cross-verification.
    const EVENING: u64 = 18 * 3_600;
    const SPACING: i64 = EVENING_DRAIN_SPACING_SECS;

    fn tonight(restarts: u32, last: i64, backlog_before: u64) -> EveningState {
        EveningState {
            day: DAY,
            restarts,
            last_unix: last,
            backlog_before,
            ..EveningState::default()
        }
    }

    /// `evening_step` with the day's own work done and the cross-check clear.
    fn step(s: &EveningState, backlog: Option<u64>, now: i64, secs: u64) -> EveningStep {
        evening_step(s, DAY, true, true, backlog, now, secs)
    }

    #[test]
    fn test_regression_state_crc32_matches_the_ieee_reference_vector() {
        assert_eq!(state_crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(state_crc32(b""), 0);
        assert_eq!(
            EveningState::default().to_line(),
            "v2 0 0 0 0 0 0 3598aea2\n"
        );
    }

    #[test]
    fn test_regression_quarantine_rename_failure_preserves_original_and_refuses_reset() {
        let dir =
            std::env::temp_dir().join(format!("tv-evening-rename-failure-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("directory");
        let path = dir.join("state");
        std::fs::write(&path, b"old torn bytes").expect("fixture");
        let metadata = regular_metadata(&path).expect("metadata");
        let result = archive_untrusted_state(&path, &metadata, NOW, |_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected rename failure",
            ))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).expect("original"), b"old torn bytes");
        assert!(!quarantine_directory(&path, NOW).join("state").exists());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn test_regression_replacement_after_read_is_not_archived() {
        let dir =
            std::env::temp_dir().join(format!("tv-evening-replacement-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("directory");
        let path = dir.join("state");
        std::fs::write(&path, b"old").expect("old");
        let metadata = regular_metadata(&path).expect("metadata");
        let replacement = dir.join("replacement");
        std::fs::write(&replacement, b"new").expect("replacement");
        File::open(&replacement)
            .expect("open")
            .set_modified(metadata.modified().expect("mtime"))
            .expect("same mtime");
        std::fs::rename(&replacement, &path).expect("replace");
        assert!(
            archive_untrusted_state(&path, &metadata, NOW, |from, to| std::fs::rename(from, to))
                .is_err()
        );
        assert_eq!(std::fs::read(&path).expect("new remains"), b"new");
        assert!(!quarantine_directory(&path, NOW).exists());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn test_store_state_load_state_to_line_from_line_round_trip() {
        let dir = std::env::temp_dir().join(format!("tv-evening-drain-{}", std::process::id()));
        let path = dir.join("evening-drain.state");
        let state = EveningState {
            day: DAY,
            restarts: 2,
            last_unix: NOW,
            backlog_before: 400,
            pending_unix: NOW,
            gave_up: true,
        };
        store_state(&path, &state).expect("write");
        assert_eq!(load_state(&path), Some(state));
        assert_eq!(EveningState::from_line(&state.to_line()), Some(state));
        // Missing reads as no restart yet; damaged reads as unknown.
        assert_eq!(
            load_state(&dir.join("absent")),
            Some(EveningState::default())
        );
        std::fs::write(&path, "v1 1 2 3").expect("write");
        assert_eq!(load_state(&path), None);
        assert_eq!(EveningState::from_line("v2 1 2 3 4 5 0"), None);
        assert_eq!(EveningState::from_line("v1 1 2 3 4 5 2"), None);
        assert_eq!(EveningState::from_line("v1 1 2 3 4 5 0 7"), None);
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_evening_step_idles_and_holds_nothing_without_a_backlog() {
        let s = EveningState::default();
        assert_eq!(step(&s, None, NOW, EVENING), EveningStep::Idle);
        assert_eq!(step(&s, Some(0), NOW, EVENING), EveningStep::Idle);
        assert!(!EveningStep::Idle.holds());
    }

    #[test]
    fn evening_step_waits_for_the_days_work_and_the_cross_check_then_restarts() {
        let s = EveningState::default();
        let not_done = evening_step(&s, DAY, false, true, Some(9), NOW, EVENING);
        assert_eq!(not_done, EveningStep::Wait);
        let xverify_running = evening_step(&s, DAY, true, false, Some(9), NOW, EVENING);
        assert_eq!(xverify_running, EveningStep::Wait);
        assert_eq!(step(&s, Some(9), NOW, EVENING), EveningStep::Restart);
        assert!(EveningStep::Wait.holds() && EveningStep::Restart.holds());
    }

    #[test]
    fn evening_step_holds_each_restarts_catch_up_then_judges_progress() {
        let s = tonight(1, NOW, 100);
        // Inside the spacing: held, even when the scan raced and read nothing.
        assert_eq!(step(&s, Some(100), NOW + 60, EVENING), EveningStep::Wait);
        assert_eq!(step(&s, None, NOW + 60, EVENING), EveningStep::Wait);
        let later = NOW + SPACING;
        assert_eq!(step(&s, Some(100), later, EVENING), EveningStep::GaveUp);
        assert_eq!(step(&s, Some(60), later, EVENING), EveningStep::Restart);
        assert!(!EveningStep::GaveUp.holds());
    }

    #[test]
    fn evening_step_holds_the_last_restart_and_one_past_the_window() {
        // The third restart's own catch-up is held although the cap is hit.
        let capped = tonight(EVENING_DRAIN_RESTARTS_MAX, NOW, 100);
        assert_eq!(
            step(&capped, Some(50), NOW + 60, EVENING),
            EveningStep::Wait
        );
        assert_eq!(
            step(&capped, Some(50), NOW + SPACING, EVENING),
            EveningStep::GaveUp
        );
        // A restart at 04:55 is held after 05:00, then the window ends it.
        let late = tonight(1, NOW, 100);
        assert_eq!(
            step(&late, Some(50), NOW + 600, 5 * 3_600 + 300),
            EveningStep::Wait
        );
        let after = step(&late, Some(50), NOW + SPACING, 5 * 3_600 + 2_100);
        assert_eq!(after, EveningStep::GaveUp);
    }

    #[test]
    fn evening_step_stops_outside_the_window_and_after_giving_up() {
        let fresh = EveningState::default();
        assert_eq!(step(&fresh, Some(5), NOW, 6 * 3_600), EveningStep::GaveUp);
        // 15:00 is before tonight's window: wait, never give up (M2).
        assert_eq!(step(&fresh, Some(5), NOW, 15 * 3_600), EveningStep::Wait);
        assert_eq!(step(&fresh, Some(5), NOW, 2 * 3_600), EveningStep::Restart);
        // A latched give-up does not flip back when the count moves.
        let gave_up = state_for_give_up(&tonight(1, NOW, 100), DAY);
        assert_eq!(
            step(&gave_up, Some(10), NOW + 2 * SPACING, EVENING),
            EveningStep::GaveUp
        );
        // Last evening's count and verdict do not carry over.
        let yesterday = EveningState {
            day: DAY - 1,
            ..gave_up
        };
        assert_eq!(
            step(&yesterday, Some(5), NOW, EVENING),
            EveningStep::Restart
        );
    }

    /// M2 (2026-10-26, live): the drain-hold supervisor's first look is at
    /// 15:40, ten minutes before the window. It returned GaveUp there, the
    /// caller latched it for the day, and the evening made no restart
    /// ("no more restarts tonight", restarts=0, 15:40:57 IST).
    #[test]
    fn test_regression_evening_step_waits_before_the_window_instead_of_latching_give_up() {
        let fresh = EveningState::default();
        let at_1540 = 15 * 3_600 + 40 * 60;
        assert_eq!(step(&fresh, Some(1_696), NOW, at_1540), EveningStep::Wait);
        assert!(step(&fresh, Some(1_696), NOW, at_1540).holds());
        // Ten minutes later the same evening restarts.
        let at_1550 = EVENING_DRAIN_FIRST_SECS_IST;
        assert_eq!(
            step(&fresh, Some(1_696), NOW + 600, at_1550),
            EveningStep::Restart
        );
        // After 05:00 the night is over: give up, as before.
        assert_eq!(
            step(
                &fresh,
                Some(1_696),
                NOW,
                EVENING_DRAIN_LAST_RESTART_SECS_IST
            ),
            EveningStep::GaveUp
        );
        // A real cap still gives up before the window too.
        let capped = tonight(EVENING_DRAIN_RESTARTS_MAX, NOW - 2 * SPACING, 100);
        assert_eq!(step(&capped, Some(50), NOW, at_1540), EveningStep::GaveUp);
    }

    #[test]
    fn state_for_restart_counts_per_evening() {
        let s = state_for_restart(&EveningState::default(), DAY, 70, NOW);
        let want = EveningState {
            day: DAY,
            restarts: 1,
            last_unix: NOW,
            backlog_before: 70,
            pending_unix: NOW,
            gave_up: false,
        };
        assert_eq!(s, want);
        let s2 = state_for_restart(&s, DAY, 40, NOW + SPACING);
        assert_eq!(s2.restarts, 2);
        let next_evening = state_for_restart(&s2, DAY + 1, 40, NOW + 86_400);
        assert_eq!(next_evening.restarts, 1);
    }

    #[test]
    fn state_for_give_up_keeps_tonights_count_and_resets_a_new_evening() {
        let s = tonight(2, NOW, 30);
        let g = state_for_give_up(&s, DAY);
        assert!(g.gave_up);
        assert_eq!((g.restarts, g.last_unix), (2, NOW));
        let fresh = state_for_give_up(&s, DAY + 1);
        assert_eq!(
            (fresh.day, fresh.restarts, fresh.gave_up),
            (DAY + 1, 0, true)
        );
    }

    #[test]
    fn crossverify_settled_by_marker_or_clock() {
        assert!(!crossverify_settled(false, 16 * 3_600));
        assert!(crossverify_settled(true, 16 * 3_600));
        // 2026-10-22 (plan item 64): settled from 16:25, after the 16:23 end bound.
        assert_eq!(EVENING_DRAIN_XVERIFY_CLEAR_SECS_IST, 16 * 3_600 + 25 * 60);
        assert!(!crossverify_settled(false, 16 * 3_600 + 25 * 60 - 1));
        assert!(crossverify_settled(
            false,
            EVENING_DRAIN_XVERIFY_CLEAR_SECS_IST
        ));
        assert!(crossverify_settled(false, 2 * 3_600), "after midnight");
    }

    #[test]
    fn test_catchup_allowance_for_only_a_fresh_evening_pending_flag() {
        use tickvault_storage::wal_replay_frame_cap::EVENING_DRAIN_REPLAY_FRAME_CAP;
        use tickvault_storage::ws_frame_spill::WAL_REPLAY_MAX_FRAMES_PER_BOOT;
        let pending = EveningState {
            pending_unix: NOW,
            ..EveningState::default()
        };
        let evening = CatchupAllowance {
            budget_secs: EVENING_DRAIN_CATCHUP_BUDGET_SECS,
            frame_cap: EVENING_DRAIN_REPLAY_FRAME_CAP,
        };
        // Quote 41: the evening drain boot replays up to 5 x 3,000,000.
        assert_eq!(EVENING_DRAIN_REPLAY_FRAME_CAP, 15_000_000);
        assert_eq!(
            catchup_allowance_for(&pending, 300, NOW + 30, EVENING),
            evening
        );
        let standard = |secs| CatchupAllowance {
            budget_secs: secs,
            frame_cap: WAL_REPLAY_MAX_FRAMES_PER_BOOT,
        };
        // Stale flag, no flag, or a morning boot: the standard allowance.
        let stale = NOW + EVENING_DRAIN_PENDING_MAX_AGE_SECS + 1;
        assert_eq!(
            catchup_allowance_for(&pending, 300, stale, EVENING),
            standard(300)
        );
        assert_eq!(
            catchup_allowance_for(&EveningState::default(), 300, NOW, EVENING),
            standard(300)
        );
        assert_eq!(
            catchup_allowance_for(&pending, 20, NOW + 30, 10 * 3_600),
            standard(20)
        );
        // A flag stamped in the future is not fresh.
        assert_eq!(
            catchup_allowance_for(&pending, 300, NOW - 5, EVENING),
            standard(300)
        );
        // The standard clock never shrinks on the evening boot.
        assert_eq!(
            catchup_allowance_for(&pending, 5_000, NOW + 30, EVENING).budget_secs,
            5_000
        );
    }

    #[test]
    fn in_restart_window_wraps_midnight() {
        assert!(in_restart_window(EVENING_DRAIN_FIRST_SECS_IST));
        assert!(in_restart_window(23 * 3_600));
        assert!(in_restart_window(0));
        assert!(!in_restart_window(EVENING_DRAIN_LAST_RESTART_SECS_IST));
        assert!(!in_restart_window(EVENING_DRAIN_FIRST_SECS_IST - 1));
    }

    #[tokio::test]
    async fn test_request_self_restart_resolves_a_later_self_restart_requested() {
        request_self_restart();
        tokio::time::timeout(std::time::Duration::from_secs(1), self_restart_requested())
            .await
            .expect("the stored permit resolves the wait");
    }

    #[test]
    fn the_lane_and_main_are_wired_to_the_evening_drain() {
        let lane = include_str!("dhan_feed_stack.rs");
        assert!(lane.contains(
            "crate::evening_drain::allowance_or(wal_catchup_budget_secs(now_ist_secs_of_day()))"
        ));
        let main = include_str!("main.rs");
        assert!(main.contains("tickvault_app::evening_drain::self_restart_requested()"));
        let hold = include_str!("drain_hold.rs");
        assert!(hold.contains("crate::evening_drain::request_self_restart()"));
        assert!(hold.contains("crate::evening_drain::state_for_give_up("));
        assert!(hold.contains("crate::evening_drain::crossverify_settled("));
    }
}
